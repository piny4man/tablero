//! NetworkManager connectivity source.
//!
//! Reads the primary connection from NetworkManager over the system DBus and
//! emits typed [`Msg::Network`] snapshots through the [producer
//! bridge](crate::producer), so connectivity reaches the render loop the same way
//! every other message does — the rendering code never talks to DBus directly.
//!
//! NetworkManager exposes an overall `State` plus a `PrimaryConnection` object
//! path on `org.freedesktop.NetworkManager`. The producer reads those, follows
//! the primary active connection to its `Type` (and, on Wi-Fi, traverses
//! device → access point for the `Ssid`), normalizes
//! the readings into a [`Network`], then re-reads and re-emits whenever the state
//! or primary connection changes. Normalization lives in [`network_from_nm`], a
//! pure function the tests drive directly — the full DBus value → message →
//! widget path is covered without a live system bus.

use futures_util::stream::{StreamExt, select_all};
use log::warn;
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, proxy};

use crate::widget::{Msg, Network, NetworkState};

use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// Normalize NetworkManager's overall state and the primary connection's type
/// into a [`Network`] snapshot.
///
/// `nm_state` follows NetworkManager's `NMState` enum: `10` asleep, `20`
/// disconnected, `30` disconnecting all read as offline; `40` connecting and
/// `50`/`60`/`70` connected (local/site/global) are classified by `conn_type`;
/// `0` unknown — or anything a future NetworkManager might add — is
/// [`Unknown`](NetworkState::Unknown). `conn_type` is the active connection's
/// `Type` (`"802-3-ethernet"` → wired, `"802-11-wireless"` → wireless, anything
/// else unknown). The `ssid` is passed straight to [`Network::new`], which keeps
/// it only on a wireless link. Pure over its inputs: the integration tests drive
/// the full reading → message → widget path through this without a live bus.
pub fn network_from_nm(nm_state: u32, conn_type: Option<&str>, ssid: Option<&str>) -> Network {
    let state = match nm_state {
        // Asleep, disconnected, or tearing the connection down: offline.
        10 | 20 | 30 => NetworkState::Disconnected,
        // Connecting or connected (local/site/global): classify by link type.
        40 | 50 | 60 | 70 => connection_state(conn_type),
        // Unknown (0) or any value a future NetworkManager might add.
        _ => NetworkState::Unknown,
    };
    Network::new(state, ssid)
}

/// Map an active connection's `Type` string to a connected [`NetworkState`].
///
/// Only the two link kinds the bar distinguishes are recognized; an absent or
/// unrecognized type degrades to [`Unknown`](NetworkState::Unknown) rather than
/// guessing.
fn connection_state(conn_type: Option<&str>) -> NetworkState {
    match conn_type {
        Some("802-3-ethernet") => NetworkState::Wired,
        Some("802-11-wireless") => NetworkState::Wireless,
        _ => NetworkState::Unknown,
    }
}

/// The subset of `org.freedesktop.NetworkManager` the bar reads. zbus generates
/// `NetworkManagerProxy` from this.
#[proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    /// Overall connectivity state; see [`network_from_nm`] for the value mapping.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    /// Object path of the primary active connection, or `"/"` when there is none.
    #[zbus(property)]
    fn primary_connection(&self) -> zbus::Result<OwnedObjectPath>;
}

/// The subset of `org.freedesktop.NetworkManager.Connection.Active` the bar
/// reads on the primary connection. The path is dynamic, so this proxy is built
/// per-connection.
#[proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait ActiveConnection {
    /// The connection type, e.g. `"802-3-ethernet"` or `"802-11-wireless"`.
    #[zbus(property, name = "Type")]
    fn type_(&self) -> zbus::Result<String>;

    /// Object paths of the devices carrying this connection.
    #[zbus(property)]
    fn devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
}

/// The subset of `org.freedesktop.NetworkManager.Device.Wireless` used to reach
/// the active access point of a wireless device.
#[proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
trait WirelessDevice {
    /// Object path of the access point currently in use, or `"/"` when none.
    #[zbus(property)]
    fn active_access_point(&self) -> zbus::Result<OwnedObjectPath>;
}

/// The subset of `org.freedesktop.NetworkManager.AccessPoint` carrying the SSID.
#[proxy(
    interface = "org.freedesktop.NetworkManager.AccessPoint",
    default_service = "org.freedesktop.NetworkManager"
)]
trait AccessPoint {
    /// The network name as raw bytes (not necessarily valid UTF-8).
    #[zbus(property)]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;
}

/// Read NetworkManager's current connectivity and normalize it into a snapshot.
///
/// A failed read degrades to `None` (network shown unavailable) and a logged
/// warning rather than ending the stream — a transient DBus hiccup, or
/// NetworkManager not running at all, never takes the source down; the widget
/// simply stays blank.
async fn read_snapshot(conn: &Connection, nm: &NetworkManagerProxy<'_>) -> Option<Network> {
    match try_read(conn, nm).await {
        Ok(network) => Some(network),
        Err(e) => {
            warn!("networkmanager: reading network state failed: {e}");
            None
        }
    }
}

/// Read the overall state and primary connection, following it to a type and
/// (on Wi-Fi) an SSID, then fold them into a normalized snapshot.
async fn try_read(conn: &Connection, nm: &NetworkManagerProxy<'_>) -> zbus::Result<Network> {
    let state = nm.state().await?;
    let primary = nm.primary_connection().await?;
    // "/" is NetworkManager's "no primary connection": nothing to classify.
    if primary.as_str() == "/" {
        return Ok(network_from_nm(state, None, None));
    }

    let active = ActiveConnectionProxy::builder(conn)
        .path(primary)?
        .build()
        .await?;
    let conn_type = active.type_().await?;
    // The SSID lives several hops away and only matters on Wi-Fi; a missing one
    // degrades to no name rather than failing the whole read.
    let ssid = if conn_type == "802-11-wireless" {
        read_ssid(conn, &active).await
    } else {
        None
    };

    Ok(network_from_nm(state, Some(&conn_type), ssid.as_deref()))
}

/// Best-effort SSID lookup for a wireless connection: connection → device →
/// active access point → `Ssid`.
///
/// Every hop can be absent (no device, no associated AP) or fail transiently;
/// any of those simply yields `None`, so the widget falls back to the bare
/// `"wifi"` label rather than erroring. The raw bytes are decoded lossily — an
/// SSID is not guaranteed to be valid UTF-8.
async fn read_ssid(conn: &Connection, active: &ActiveConnectionProxy<'_>) -> Option<String> {
    let device_path = active.devices().await.ok()?.into_iter().next()?;
    let wireless = WirelessDeviceProxy::builder(conn)
        .path(device_path)
        .ok()?
        .build()
        .await
        .ok()?;
    let ap_path = wireless.active_access_point().await.ok()?;
    if ap_path.as_str() == "/" {
        return None;
    }
    let access_point = AccessPointProxy::builder(conn)
        .path(ap_path)
        .ok()?
        .build()
        .await
        .ok()?;
    let bytes = access_point.ssid().await.ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// A [`Producer`] that streams NetworkManager connectivity changes into the
/// render loop.
///
/// Construct with [`new`](NetworkProducer::new) and hand it to the producer
/// bridge; it reads an initial snapshot, then re-reads and emits on every change
/// to the overall state or the primary connection until the system bus closes or
/// the render loop shuts down.
pub struct NetworkProducer;

impl NetworkProducer {
    /// Create a NetworkManager connectivity producer.
    pub fn new() -> Self {
        Self
    }
}

impl Default for NetworkProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for NetworkProducer {
    fn name(&self) -> String {
        "networkmanager".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx))
    }
}

/// Drive the connectivity stream: initial snapshot, then re-read on each change.
///
/// Returns `Ok(())` once the render loop has gone away (a [`send`] reports the
/// channel closed) or the property streams end. A failed system-bus connection
/// propagates as an error the bridge logs and isolates — the bar keeps running,
/// the network widget simply stays blank.
///
/// [`send`]: MsgSender::send
async fn run(tx: MsgSender) -> ProducerResult {
    let conn = Connection::system().await?;
    let nm = NetworkManagerProxy::new(&conn).await?;

    // Seed the bar with the current connectivity before the first change signal.
    if tx
        .send(Msg::Network(read_snapshot(&conn, &nm).await))
        .is_err()
    {
        return Ok(());
    }

    // Either the overall state or the primary connection changing warrants a
    // re-read; merge the two property streams and re-emit a fresh snapshot on
    // each tick. An unchanged snapshot is harmless — the widget reports no
    // visible change.
    let mut changes = select_all([
        nm.receive_state_changed().await.map(|_| ()).boxed(),
        nm.receive_primary_connection_changed()
            .await
            .map(|_| ())
            .boxed(),
    ]);
    while changes.next().await.is_some() {
        if tx
            .send(Msg::Network(read_snapshot(&conn, &nm).await))
            .is_err()
        {
            return Ok(());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_states_normalize_to_disconnected() {
        // 10 asleep, 20 disconnected, 30 disconnecting — all offline regardless
        // of any stale connection type.
        for state in [10, 20, 30] {
            let network = network_from_nm(state, Some("802-3-ethernet"), None);
            assert_eq!(network.state(), NetworkState::Disconnected);
        }
    }

    #[test]
    fn connected_ethernet_normalizes_to_wired() {
        // 50/60/70 are connected (local/site/global); 40 is connecting.
        for state in [40, 50, 60, 70] {
            let network = network_from_nm(state, Some("802-3-ethernet"), None);
            assert_eq!(network.state(), NetworkState::Wired);
            assert_eq!(network.label(), "wired");
        }
    }

    #[test]
    fn connected_wireless_keeps_its_ssid() {
        let network = network_from_nm(70, Some("802-11-wireless"), Some("home-net"));
        assert_eq!(network.state(), NetworkState::Wireless);
        assert_eq!(network.ssid(), Some("home-net"));
        assert_eq!(network.label(), "wifi home-net");
    }

    #[test]
    fn connected_wireless_without_ssid_shows_bare_wifi() {
        let network = network_from_nm(70, Some("802-11-wireless"), None);
        assert_eq!(network.state(), NetworkState::Wireless);
        assert_eq!(network.ssid(), None);
        assert_eq!(network.label(), "wifi");
    }

    #[test]
    fn connected_with_unknown_type_normalizes_to_unknown() {
        // Connected, but the link is neither plain ethernet nor wifi (a VPN,
        // a bridge, a missing type): shown as unknown rather than mislabeled.
        assert_eq!(
            network_from_nm(70, Some("vpn"), None).state(),
            NetworkState::Unknown
        );
        assert_eq!(
            network_from_nm(70, None, None).state(),
            NetworkState::Unknown
        );
    }

    #[test]
    fn unknown_nm_state_normalizes_to_unknown() {
        assert_eq!(
            network_from_nm(0, Some("802-3-ethernet"), None).state(),
            NetworkState::Unknown
        );
        assert_eq!(
            network_from_nm(99, Some("802-11-wireless"), Some("home-net")).state(),
            NetworkState::Unknown
        );
    }

    #[test]
    fn ssid_is_dropped_when_the_link_is_not_wireless() {
        // Even if NetworkManager hands an SSID alongside a wired link, the
        // normalized snapshot never carries it.
        let network = network_from_nm(70, Some("802-3-ethernet"), Some("home-net"));
        assert_eq!(network.ssid(), None);
        assert_eq!(network.label(), "wired");
    }
}
