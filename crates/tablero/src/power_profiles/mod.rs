//! power-profiles-daemon source and profile-switching command executor.
//!
//! On machines that also expose a known platform TDP/fan driver (today:
//! `qc71_laptop`), the same producer overlays that hardware state and the
//! command executor applies both PPD and the privileged platform helper.
//! Detection is silent: Framework laptops, desktops, and anything without
//! that sysfs stay a pure D-Bus client.

mod platform;

use std::collections::HashMap;
use std::pin::pin;
use std::time::Duration;

use futures_util::future::{self, Either};
use futures_util::stream::{StreamExt, select, select_all};
use log::{info, warn};
use tokio::sync::Notify;
use zbus::Connection;
use zbus::fdo::PropertiesProxy;
use zbus::names::InterfaceName;
use zbus::zvariant::{OwnedValue, Value};

use crate::command::CommandReceiver;
use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};
use crate::widget::{Command, Msg, PowerProfile, PowerProfilesState};

pub use platform::{PlatformKind, PowerProfilesSettings, detect_platform, overlay_snapshot};

use platform::{apply_platform_profile, read_platform_profile, resolve_hardware_helper};

const CURRENT_NAME: &str = "org.freedesktop.UPower.PowerProfiles";
const CURRENT_PATH: &str = "/org/freedesktop/UPower/PowerProfiles";
const LEGACY_NAME: &str = "net.hadess.PowerProfiles";
const LEGACY_PATH: &str = "/net/hadess/PowerProfiles";
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// How often the platform driver's sysfs state is re-read. The driver offers no
/// change notification, so this bounds how long a mode set by another tool (or
/// the laptop's own key) goes unnoticed; our own changes are re-read at once.
const PLATFORM_POLL: Duration = Duration::from_secs(5);

/// Raised by the command executor once it has applied a platform profile, so
/// the producer shows a click's result without waiting out [`PLATFORM_POLL`].
static PLATFORM_APPLIED: Notify = Notify::const_new();

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

fn compose(
    ppd: Option<PowerProfilesState>,
    platform: Option<PlatformKind>,
    sys_root: &std::path::Path,
) -> Option<PowerProfilesState> {
    let hardware_profile = platform.and_then(|kind| read_platform_profile(sys_root, kind));
    overlay_snapshot(ppd, platform.map(PlatformKind::name), hardware_profile)
}

/// Event-driven power-profiles-daemon producer, with optional platform overlay.
pub struct PowerProfilesProducer {
    settings: PowerProfilesSettings,
}

impl PowerProfilesProducer {
    /// Create a producer that auto-detects platform backends under `/sys`.
    pub fn new() -> Self {
        Self {
            settings: PowerProfilesSettings::default(),
        }
    }

    /// Override detection settings (sysfs root, helper path, enable flag).
    pub fn with_settings(mut self, settings: PowerProfilesSettings) -> Self {
        self.settings = settings;
        self
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
        Box::pin(run(tx, self.settings))
    }
}

async fn run(tx: MsgSender, settings: PowerProfilesSettings) -> ProducerResult {
    let conn = Connection::system().await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let owner_streams = [CURRENT_NAME, LEGACY_NAME].map(|name| async {
        dbus.receive_name_owner_changed_with_args(&[(0, name)])
            .await
            .map(|stream| stream.map(|_| ()))
    });
    let mut owners = select_all(futures_util::future::try_join_all(owner_streams).await?);

    let platform = settings.detect_platform();
    if let Some(kind) = platform {
        info!("power-profiles: {} hardware backend detected", kind.name());
    }

    let mut previous = None;
    loop {
        let Some((endpoint, _)) = find_endpoint(&conn).await else {
            let composed = compose(None, platform, settings.sys_root());
            if send_if_changed(&tx, &mut previous, composed).is_err() {
                return Ok(());
            }
            let delay = if platform.is_some() {
                PLATFORM_POLL
            } else {
                RETRY_DELAY
            };
            tokio::time::sleep(delay).await;
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
        let owner_changes = (&mut owners).map(|_| ());
        let events = select(changes, owner_changes);
        futures_util::pin_mut!(events);
        // The daemon pushes its changes, so between those only the platform
        // file is re-read, against the daemon state already in hand.
        loop {
            let composed = compose(Some(snapshot.clone()), platform, settings.sys_root());
            if send_if_changed(&tx, &mut previous, composed).is_err() {
                return Ok(());
            }
            if platform.is_none() {
                match events.next().await {
                    Some(()) => break,
                    None => return Ok(()),
                }
            }
            let applied = pin!(PLATFORM_APPLIED.notified());
            let wake = future::select(events.next(), applied);
            match tokio::time::timeout(PLATFORM_POLL, wake).await {
                Ok(Either::Left((Some(()), _))) => break,
                Ok(Either::Left((None, _))) => return Ok(()),
                Ok(Either::Right(_)) | Err(_) => {}
            }
        }
        // Either state changed or a compatibility name changed owner.
        // Re-seeding also refreshes the Profiles list if hardware
        // capabilities changed.
    }
}

fn send_if_changed(
    tx: &MsgSender,
    previous: &mut Option<Option<PowerProfilesState>>,
    next: Option<PowerProfilesState>,
) -> Result<(), ()> {
    if previous.as_ref() == Some(&next) {
        return Ok(());
    }
    *previous = Some(next.clone());
    tx.send(Msg::PowerProfiles(next)).map_err(|_| ())
}

/// Execute profile rotations requested by the widget.
pub async fn run_commands(
    mut commands: CommandReceiver,
    settings: PowerProfilesSettings,
) -> ProducerResult {
    let conn = Connection::system().await?;
    let platform = settings.detect_platform();
    let helper =
        platform.and_then(|kind| resolve_hardware_helper(settings.hardware_helper(), kind));
    if let Some(kind) = platform
        && helper.is_none()
    {
        warn!(
            "power-profiles: {} detected but no helper on PATH; clicks update power-profiles-daemon only",
            kind.name()
        );
    }

    while let Some(command) = commands.recv().await {
        let Command::SetPowerProfile(profile) = command else {
            continue;
        };
        match find_endpoint(&conn).await {
            Some((endpoint, _)) => match properties_proxy(&conn, endpoint).await {
                Ok(proxy) => {
                    let interface = InterfaceName::try_from(endpoint.interface())?;
                    if let Err(error) = proxy
                        .set(interface, "ActiveProfile", Value::new(profile.as_str()))
                        .await
                    {
                        warn!("power-profiles-daemon: setting {profile:?} failed: {error}");
                    }
                }
                Err(error) => {
                    warn!("power-profiles-daemon: setting {profile:?} failed: {error}");
                }
            },
            None if platform.is_none() => {
                warn!("power-profiles-daemon: cannot set {profile:?}; daemon unavailable");
                continue;
            }
            None => {}
        }
        if let (Some(kind), Some(helper)) = (platform, helper.as_ref()) {
            if let Err(error) = apply_platform_profile(&profile, kind, helper).await {
                warn!("power-profiles: {error}");
            }
            PLATFORM_APPLIED.notify_one();
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

    /// The platform poll works from the daemon state already in hand: it takes
    /// no bus connection, so it cannot issue a D-Bus call.
    #[test]
    fn a_platform_poll_reports_only_a_changed_hardware_mode() {
        let sys = tempfile::tempdir().unwrap();
        let qc71 = sys.path().join("devices/platform/qc71_laptop");
        std::fs::create_dir_all(&qc71).unwrap();
        let mode = qc71.join("performance_mode");
        std::fs::write(&mode, "2\n").unwrap();
        let platform = detect_platform(sys.path());
        assert!(platform.is_some());
        let daemon = PowerProfilesState::new(
            "balanced",
            vec![
                PowerProfile::new("balanced", "multiple", "amd_pstate", "placeholder"),
                PowerProfile::new("performance", "amd_pstate", "amd_pstate", "amd_pstate"),
            ],
        );
        let (bridge, channel) = crate::producer::ProducerBridge::new().unwrap();
        let tx = bridge.sender();
        let mut previous = None;
        let mut poll = || {
            let composed = compose(Some(daemon.clone()), platform, sys.path());
            send_if_changed(&tx, &mut previous, composed).unwrap();
            std::iter::from_fn(|| channel.try_recv().ok())
                .map(|msg| match msg {
                    Msg::PowerProfiles(Some(state)) => state,
                    other => panic!("unexpected message: {other:?}"),
                })
                .collect::<Vec<_>>()
        };

        let seeded = poll();
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].active_name(), "balanced");
        assert!(poll().is_empty(), "steady hardware sends nothing");

        // Another tool switches the hardware mode behind the daemon's back.
        std::fs::write(&mode, "3\n").unwrap();
        let changed = poll();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].active_name(), "performance");
        assert_eq!(changed[0].profiles(), daemon.profiles());
        assert!(poll().is_empty());
    }
}
