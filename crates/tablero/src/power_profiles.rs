//! power-profiles-daemon source and profile-switching command executor.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::stream::{StreamExt, select, select_all};
use log::warn;
use zbus::Connection;
use zbus::fdo::PropertiesProxy;
use zbus::names::InterfaceName;
use zbus::zvariant::{OwnedValue, Value};

use crate::widget::{Command, Msg, PowerProfile, PowerProfilesState};

use crate::command::CommandReceiver;
use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

const CURRENT_NAME: &str = "org.freedesktop.UPower.PowerProfiles";
const CURRENT_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
const LEGACY_NAME: &str = "net.hadess.PowerProfiles";
const LEGACY_PATH: &str = "/net/hadess/PowerProfiles";
const RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
enum Endpoint {
    Current,
    Legacy,
}

impl Endpoint {
    fn service(self) -> &'static str {
        match self {
            Self::Current => CURRENT_NAME,
            Self::Legacy => LEGACY_NAME,
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Current => CURRENT_PATH,
            Self::Legacy => LEGACY_PATH,
        }
    }

    fn interface(self) -> &'static str {
        self.service()
    }
}

/// Normalize one raw D-Bus profile dictionary using Waybar's driver fallbacks.
pub fn profile_from_properties(mut values: HashMap<String, OwnedValue>) -> Option<PowerProfile> {
    let take = |values: &mut HashMap<String, OwnedValue>, key: &str| {
        values
            .remove(key)
            .and_then(|value| String::try_from(value).ok())
    };
    let name = take(&mut values, "Profile")?;
    if name.is_empty() {
        return None;
    }

    let mut driver = take(&mut values, "Driver").unwrap_or_default();
    let mut cpu_driver = take(&mut values, "CpuDriver").unwrap_or_default();
    let mut platform_driver = take(&mut values, "PlatformDriver").unwrap_or_default();
    if cpu_driver.is_empty() {
        cpu_driver.clone_from(&driver);
    }
    if platform_driver.is_empty() {
        platform_driver.clone_from(&driver);
    }
    if driver.is_empty() {
        driver.clone_from(&cpu_driver);
    }
    if driver.is_empty() {
        driver = "Unavailable".to_string();
        cpu_driver = "Unavailable".to_string();
        platform_driver = "Unavailable".to_string();
    }

    Some(PowerProfile::new(name, driver, cpu_driver, platform_driver))
}

async fn properties_proxy(
    conn: &Connection,
    endpoint: Endpoint,
) -> zbus::Result<PropertiesProxy<'_>> {
    PropertiesProxy::builder(conn)
        .destination(endpoint.service())?
        .path(endpoint.path())?
        .build()
        .await
}

async fn read_snapshot(conn: &Connection, endpoint: Endpoint) -> zbus::Result<PowerProfilesState> {
    let proxy = properties_proxy(conn, endpoint).await?;
    let interface = InterfaceName::try_from(endpoint.interface())?;
    let mut values = proxy.get_all(interface).await?;
    let active = values
        .remove("ActiveProfile")
        .ok_or_else(|| zbus::Error::Failure("ActiveProfile is missing".to_string()))?;
    let active = String::try_from(active)
        .map_err(|error| zbus::Error::Failure(format!("invalid ActiveProfile: {error}")))?;
    let profiles = values
        .remove("Profiles")
        .ok_or_else(|| zbus::Error::Failure("Profiles is missing".to_string()))?;
    let profiles = Vec::<HashMap<String, OwnedValue>>::try_from(profiles)
        .map_err(|error| zbus::Error::Failure(format!("invalid Profiles: {error}")))?
        .into_iter()
        .filter_map(profile_from_properties)
        .collect();
    Ok(PowerProfilesState::new(active, profiles))
}

async fn find_endpoint(conn: &Connection) -> Option<(Endpoint, PowerProfilesState)> {
    for endpoint in [Endpoint::Current, Endpoint::Legacy] {
        if let Ok(snapshot) = read_snapshot(conn, endpoint).await {
            return Some((endpoint, snapshot));
        }
    }
    None
}

/// Event-driven power-profiles-daemon producer.
pub struct PowerProfilesProducer;

impl PowerProfilesProducer {
    /// Create a power-profiles-daemon producer.
    pub fn new() -> Self {
        Self
    }
}

impl Default for PowerProfilesProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for PowerProfilesProducer {
    fn name(&self) -> String {
        "power-profiles-daemon".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx))
    }
}

async fn run(tx: MsgSender) -> ProducerResult {
    let conn = Connection::system().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let owner_streams = [CURRENT_NAME, LEGACY_NAME].map(|name| async {
        dbus.receive_name_owner_changed_with_args(&[(0, name)])
            .await
            .map(|stream| stream.map(|_| ()))
    });
    let mut owners = select_all(futures_util::future::try_join_all(owner_streams).await?);

    loop {
        let Some((endpoint, _)) = find_endpoint(&conn).await else {
            if tx.send(Msg::PowerProfiles(None)).is_err() {
                return Ok(());
            }
            tokio::time::sleep(RETRY_DELAY).await;
            continue;
        };
        let proxy = match properties_proxy(&conn, endpoint).await {
            Ok(proxy) => proxy,
            Err(error) => {
                warn!("power-profiles-daemon: reconnecting after proxy error: {error}");
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        let interface = endpoint.interface().to_string();
        let changes = match proxy.receive_properties_changed().await {
            Ok(changes) => changes.filter_map(|signal| {
                let interface = interface.clone();
                async move {
                    signal
                        .args()
                        .ok()
                        .filter(|args| args.interface_name().as_str() == interface)
                        .map(|_| ())
                }
            }),
            Err(error) => {
                warn!("power-profiles-daemon: reconnecting after signal error: {error}");
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        // Subscribe before the seed read so a change racing with GetAll remains
        // queued and causes another refresh instead of being lost.
        let snapshot = match read_snapshot(&conn, endpoint).await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warn!("power-profiles-daemon: reconnecting after snapshot error: {error}");
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        if tx.send(Msg::PowerProfiles(Some(snapshot))).is_err() {
            return Ok(());
        }
        let owner_changes = (&mut owners).map(|_| ());
        let events = select(changes, owner_changes);
        futures_util::pin_mut!(events);
        if events.next().await.is_none() {
            return Ok(());
        }
        // Either state changed or one of the compatibility names changed owner.
        // Re-seeding also refreshes the Profiles list if hardware capabilities changed.
    }
}

/// Execute profile rotations requested by the widget.
pub async fn run_commands(mut commands: CommandReceiver) -> ProducerResult {
    let conn = Connection::system().await?;
    while let Some(command) = commands.recv().await {
        let Command::SetPowerProfile(profile) = command else {
            continue;
        };
        let endpoint = match find_endpoint(&conn).await {
            Some((endpoint, _)) => endpoint,
            None => {
                warn!("power-profiles-daemon: cannot set {profile:?}; daemon unavailable");
                continue;
            }
        };
        let proxy = match properties_proxy(&conn, endpoint).await {
            Ok(proxy) => proxy,
            Err(error) => {
                warn!("power-profiles-daemon: setting {profile:?} failed: {error}");
                continue;
            }
        };
        let interface = InterfaceName::try_from(endpoint.interface())?;
        if let Err(error) = proxy
            .set(interface, "ActiveProfile", Value::new(profile.as_str()))
            .await
        {
            warn!("power-profiles-daemon: setting {profile:?} failed: {error}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(value: &str) -> OwnedValue {
        OwnedValue::try_from(Value::new(value)).expect("string value")
    }

    #[test]
    fn profile_normalization_preserves_and_falls_back_drivers() {
        let current = profile_from_properties(HashMap::from([
            ("Profile".into(), value("balanced")),
            ("Driver".into(), value("multiple")),
            ("CpuDriver".into(), value("amd_pstate")),
            ("PlatformDriver".into(), value("placeholder")),
        ]))
        .expect("profile");
        assert_eq!(current.name(), "balanced");
        assert_eq!(current.driver(), "multiple");
        assert_eq!(current.cpu_driver(), "amd_pstate");
        assert_eq!(current.platform_driver(), "placeholder");

        let split = profile_from_properties(HashMap::from([
            ("Profile".into(), value("performance")),
            ("CpuDriver".into(), value("amd_pstate")),
        ]))
        .expect("profile");
        assert_eq!(split.driver(), "amd_pstate");
        assert_eq!(split.cpu_driver(), "amd_pstate");
    }

    #[test]
    fn empty_profile_name_is_dropped() {
        assert!(profile_from_properties(HashMap::from([("Profile".into(), value(""))])).is_none());
    }
}
