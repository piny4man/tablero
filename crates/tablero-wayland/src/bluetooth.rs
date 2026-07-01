//! BlueZ bluetooth source.
//!
//! Reads the local Bluetooth adapter state over the system DBus and emits
//! typed [`Msg::Bluetooth`] snapshots through the [producer
//! bridge](crate::producer), so the adapter reaches the render loop the same
//! way every other message does — the rendering code never talks to DBus
//! directly.
//!
//! BlueZ exposes every adapter and paired device through
//! `org.freedesktop.DBus.ObjectManager` at the well-known name `org.bluez`:
//! `GetManagedObjects` returns the full tree of `org.bluez.Adapter1` and
//! `org.bluez.Device1` instances as a `{path: {iface: props}}` map. The
//! producer polls that map on a fixed cadence, normalizes the readings into
//! a [`Bluetooth`] snapshot, and re-emits on every change.
//!
//! Normalization lives in [`bluetooth_from_bluez`], a pure function the
//! tests drive directly — the full DBus value → message → widget path is
//! covered without a live system bus. Polling (rather than per-property
//! signal subscriptions) is what keeps the implementation tractable: BlueZ
//! adapters and devices appear and disappear dynamically, so a static
//! `select_all` over per-proxy property streams would miss additions after
//! startup. The widget's `update` reports `false` on an unchanged
//! snapshot, so a steady-state adapter costs one DBus call per tick and no
//! repaints.

use std::collections::HashMap;
use std::time::Duration;

use log::warn;
use tokio::time::interval;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, proxy};

use tablero_core::widget::{Bluetooth, BluetoothState, Msg};

use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// How often the producer polls BlueZ for adapter and device state.
///
/// Two seconds is frequent enough to track power toggles and device
/// connections as they happen, and far too coarse to keep the loop busy:
/// between ticks the producer is parked on a timer and the render loop is
/// idle, waking only when a sample changes a visible label.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(2);

/// The `org.bluez.Adapter1` interface name — the marker the ObjectManager
/// map uses to distinguish adapters from devices.
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";

/// The `org.bluez.Device1` interface name — the marker the ObjectManager
/// map uses to distinguish devices from adapters.
const DEVICE_IFACE: &str = "org.bluez.Device1";

/// Normalize the BlueZ readings into a [`Bluetooth`] snapshot.
///
/// The producer may discover any combination of (no adapter / one adapter /
/// many adapters); this function folds all of those onto the three states
/// the bar actually displays:
///
/// * `adapter_count == 0` → [`Unavailable`](BluetoothState::Unavailable),
///   regardless of `powered` (a stale `powered` from before an adapter was
///   removed can never leak into a "powered off" reading on hardware with
///   no adapter);
/// * `adapter_count >= 1`, `powered == Some(true)` →
///   [`On`](BluetoothState::On) with `connected`;
/// * `adapter_count >= 1`, `powered == Some(false)` →
///   [`Off`](BluetoothState::Off) (the connected count is irrelevant when
///   the adapter is off and is forced to zero inside [`Bluetooth::new`]);
/// * `adapter_count >= 1`, `powered == None` →
///   [`Unavailable`](BluetoothState::Unavailable) — BlueZ didn't tell us.
///
/// Pure over its inputs: the integration tests drive the full BlueZ value →
/// message → widget path through this without a live bus.
pub fn bluetooth_from_bluez(
    adapter_count: usize,
    powered: Option<bool>,
    connected: u32,
) -> Bluetooth {
    if adapter_count == 0 {
        return Bluetooth::new(BluetoothState::Unavailable, 0);
    }
    match powered {
        Some(true) => Bluetooth::new(BluetoothState::On, connected),
        Some(false) => Bluetooth::new(BluetoothState::Off, 0),
        None => Bluetooth::new(BluetoothState::Unavailable, 0),
    }
}

/// Read a `bool` property out of an interface's property map.
///
/// BlueZ exposes its properties as a `HashMap<String, OwnedValue>` per
/// interface in `GetManagedObjects`. A missing property, a non-`bool`
/// payload, or an out-of-range value all degrade to `None` rather than
/// failing the whole read, so a transient type drift never takes the source
/// down.
fn read_bool(props: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    props.get(key).and_then(|v| bool::try_from(v).ok())
}

/// Walk an `ObjectManager` snapshot and extract the readings needed for the
/// bluetooth widget.
///
/// BlueZ may expose multiple adapters on machines with a wired and a
/// wireless adapter (USB dongles, laptops with both an internal adapter and
/// an external one). The bar shows a single power-state word, so the
/// readings are aggregated across **every** adapter rather than picking one:
///
/// * the power state is "on" if **any** adapter is powered, "off" if no
///   adapter is on and at least one explicitly reports `Powered = false`,
///   and "unknown" only when every adapter failed to report power at all;
/// * the connected-device count is the sum across every adapter's devices,
///   so a user with two adapters sees the right total.
///
/// Picking a single adapter and counting devices against it would let the
/// reported power state and connected count disagree — e.g. the smallest
/// adapter off, a second adapter on with two devices, the bar would say
/// "off" but the count would already be 2. Aggregating keeps the two
/// readings tied to the same scope.
fn summarize(
    objects: &HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>,
) -> (usize, Option<bool>, u32) {
    let mut adapter_count = 0usize;
    let mut any_on = false;
    let mut any_off = false;

    for interfaces in objects.values() {
        let Some(adapter_props) = interfaces.get(ADAPTER_IFACE) else {
            continue;
        };
        adapter_count += 1;
        match read_bool(adapter_props, "Powered") {
            Some(true) => any_on = true,
            Some(false) => any_off = true,
            None => {}
        }
    }

    // Power-state rule: any adapter on → on; otherwise, any adapter
    // explicitly off → off; otherwise unknown. A transient BlueZ hiccup
    // on every adapter is the only path to `None` — the rule does not
    // require *every* adapter to be off, only that at least one is and
    // none is on. The count below is the total over all adapters
    // (meaningful only when at least one is on, but the
    // bluetooth_from_bluez normalization zeroes the count for any other
    // state so this stays consistent).
    let powered = if any_on {
        Some(true)
    } else if any_off {
        Some(false)
    } else {
        None
    };

    let mut connected: u32 = 0;
    for interfaces in objects.values() {
        if let Some(device_props) = interfaces.get(DEVICE_IFACE) {
            // The Connected property is what tells the bar a device is
            // actively paired and bonded for the current session; `Paired`
            // alone means "was paired once" and `Trusted` is just user
            // intent.
            if read_bool(device_props, "Connected") == Some(true) {
                connected = connected.saturating_add(1);
            }
        }
    }

    (adapter_count, powered, connected)
}

/// The subset of `org.freedesktop.DBus.ObjectManager` BlueZ implements, used
/// to enumerate adapters and devices in one round trip. zbus generates
/// `ObjectManagerProxy` from this.
#[proxy(
    interface = "org.freedesktop.DBus.ObjectManager",
    default_service = "org.bluez",
    default_path = "/"
)]
trait ObjectManager {
    /// Return every managed object under `default_path`, as
    /// `{object_path: {interface_name: properties}}`. Filtering on the
    /// `org.bluez.Adapter1` and `org.bluez.Device1` interface names picks
    /// out the adapters and devices the bar cares about.
    fn get_managed_objects(
        &self,
    ) -> zbus::Result<HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>>;
}

/// Read BlueZ's current adapter state and normalize it into a snapshot.
///
/// A failed read degrades to a fresh snapshot with `adapter_count = 0`
/// (i.e. [`Unavailable`](BluetoothState::Unavailable)) and a logged
/// warning rather than ending the stream — a transient DBus hiccup never
/// takes the source down.
async fn read_snapshot(om: &ObjectManagerProxy<'_>) -> Bluetooth {
    match om.get_managed_objects().await {
        Ok(objects) => {
            let (adapter_count, powered, connected) = summarize(&objects);
            bluetooth_from_bluez(adapter_count, powered, connected)
        }
        Err(e) => {
            warn!("bluetooth: reading BlueZ state failed: {e}");
            Bluetooth::new(BluetoothState::Unavailable, 0)
        }
    }
}

/// A [`Producer`] that polls BlueZ for the local adapter state and emits
/// [`Msg::Bluetooth`] snapshots on every change.
///
/// Construct with [`new`](BluetoothProducer::new) and hand it to the
/// producer bridge; it reads an initial snapshot, then re-reads and emits on
/// every tick until the system bus closes or the render loop shuts down.
pub struct BluetoothProducer {
    interval: Duration,
}

impl BluetoothProducer {
    /// Create a bluetooth producer sampling at the default cadence.
    pub fn new() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
        }
    }

    /// Create a producer sampling at a custom `interval` (used by tests).
    pub fn with_interval(interval: Duration) -> Self {
        Self { interval }
    }
}

impl Default for BluetoothProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for BluetoothProducer {
    fn name(&self) -> String {
        "bluetooth".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx, self.interval))
    }
}

/// Drive the polling loop: connect to the system bus, seed the bar with the
/// initial snapshot, then on every tick re-read and re-emit.
///
/// A failed system-bus connection propagates as an error the bridge logs
/// and isolates — the bar keeps running, the bluetooth widget simply stays
/// `unavailable`. A failed per-tick read is logged and degrades to
/// [`Unavailable`](BluetoothState::Unavailable); the next tick retries the
/// live read.
///
/// [`Unavailable`]: tablero_core::widget::BluetoothState::Unavailable
async fn run(tx: MsgSender, period: Duration) -> ProducerResult {
    let conn = Connection::system().await?;
    let om = ObjectManagerProxy::new(&conn).await?;

    // Seed the bar with the current adapter before the first tick fires, so
    // the widget never shows the empty initial state for the full interval.
    if tx.send(Msg::Bluetooth(read_snapshot(&om).await)).is_err() {
        return Ok(());
    }

    let mut ticker = interval(period);
    loop {
        ticker.tick().await;
        if tx.send(Msg::Bluetooth(read_snapshot(&om).await)).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_objects() -> HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> {
        HashMap::new()
    }

    fn adapter_with(powered: Option<bool>) -> HashMap<String, OwnedValue> {
        let mut props = HashMap::new();
        if let Some(p) = powered {
            props.insert("Powered".to_string(), OwnedValue::from(p));
        }
        props
    }

    fn device_with(connected: Option<bool>) -> HashMap<String, OwnedValue> {
        let mut props = HashMap::new();
        if let Some(c) = connected {
            props.insert("Connected".to_string(), OwnedValue::from(c));
        }
        props
    }

    fn insert_adapter(
        objects: &mut HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>,
        path: &str,
        props: HashMap<String, OwnedValue>,
    ) {
        let mut ifaces = HashMap::new();
        ifaces.insert(ADAPTER_IFACE.to_string(), props);
        objects.insert(OwnedObjectPath::try_from(path).unwrap(), ifaces);
    }

    fn insert_device(
        objects: &mut HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>,
        path: &str,
        props: HashMap<String, OwnedValue>,
    ) {
        let mut ifaces = HashMap::new();
        ifaces.insert(DEVICE_IFACE.to_string(), props);
        objects.insert(OwnedObjectPath::try_from(path).unwrap(), ifaces);
    }

    #[test]
    fn no_adapter_normalizes_to_unavailable_regardless_of_powered() {
        // A stale `powered` from before the adapter was unplugged must not
        // leak back into the bar as `off` — the snapshot says "no adapter".
        for powered in [None, Some(true), Some(false)] {
            assert_eq!(
                bluetooth_from_bluez(0, powered, 0).state(),
                BluetoothState::Unavailable
            );
        }
    }

    #[test]
    fn powered_adapter_normalizes_to_on_with_the_connected_count() {
        assert_eq!(
            bluetooth_from_bluez(1, Some(true), 0),
            Bluetooth::new(BluetoothState::On, 0)
        );
        assert_eq!(
            bluetooth_from_bluez(1, Some(true), 2),
            Bluetooth::new(BluetoothState::On, 2)
        );
    }

    #[test]
    fn unpowered_adapter_normalizes_to_off_with_zero_connected() {
        // Even if BlueZ hands a non-zero connected count alongside a powered
        // off adapter, the normalized snapshot zeroes it.
        assert_eq!(
            bluetooth_from_bluez(1, Some(false), 5),
            Bluetooth::new(BluetoothState::Off, 0)
        );
    }

    #[test]
    fn missing_powered_with_an_adapter_normalizes_to_unavailable() {
        // The adapter exists but didn't tell us whether it's on — show
        // `unavailable` rather than guess.
        assert_eq!(
            bluetooth_from_bluez(1, None, 0).state(),
            BluetoothState::Unavailable
        );
    }

    #[test]
    fn summarize_with_no_objects_reports_zero_adapters_and_unknown_power() {
        let (count, powered, connected) = summarize(&empty_objects());
        assert_eq!(count, 0);
        assert_eq!(powered, None);
        assert_eq!(connected, 0);
    }

    #[test]
    fn summarize_aggregates_power_across_all_adapters() {
        // Two adapters: hci0 powered, hci1 unpowered. Any-on wins: the bar
        // sees "on" and counts devices across both adapters.
        let mut objects = empty_objects();
        insert_adapter(&mut objects, "/org/bluez/hci0", adapter_with(Some(true)));
        insert_adapter(&mut objects, "/org/bluez/hci1", adapter_with(Some(false)));
        let (count, powered, connected) = summarize(&objects);
        assert_eq!(count, 2);
        assert_eq!(powered, Some(true));
        assert_eq!(connected, 0);
    }

    #[test]
    fn summarize_power_is_off_when_no_adapter_is_on_and_at_least_one_is() {
        // hci0 reports no Powered at all (transient) but hci1 says off:
        // no adapter is on, and at least one explicitly says off, so the
        // bar settles on `off` — the rule does not require *every*
        // adapter to be off, only that at least one is and none is on.
        let mut objects = empty_objects();
        insert_adapter(&mut objects, "/org/bluez/hci0", adapter_with(None));
        insert_adapter(&mut objects, "/org/bluez/hci1", adapter_with(Some(false)));
        let (_, powered, _) = summarize(&objects);
        assert_eq!(powered, Some(false));
    }

    #[test]
    fn summarize_power_is_unknown_when_every_adapter_omits_powered() {
        // No adapter told us anything about power: degrade to "unknown"
        // rather than guessing.
        let mut objects = empty_objects();
        insert_adapter(&mut objects, "/org/bluez/hci0", adapter_with(None));
        insert_adapter(&mut objects, "/org/bluez/hci1", adapter_with(None));
        let (_, powered, _) = summarize(&objects);
        assert_eq!(powered, None);
    }

    #[test]
    fn summarize_aggregation_is_independent_of_path_iteration_order() {
        // The same set of adapters inserted in the opposite order must
        // resolve to the same `powered` reading. `summarize` no longer sorts
        // adapter paths, so the result has to be independent of HashMap
        // iteration order — the aggregation iterates every adapter and
        // combines, which is by definition order-independent.
        let mut objects_a = empty_objects();
        insert_adapter(&mut objects_a, "/org/bluez/hci0", adapter_with(Some(true)));
        insert_adapter(&mut objects_a, "/org/bluez/hci1", adapter_with(Some(false)));
        let mut objects_b = empty_objects();
        insert_adapter(&mut objects_b, "/org/bluez/hci1", adapter_with(Some(false)));
        insert_adapter(&mut objects_b, "/org/bluez/hci0", adapter_with(Some(true)));
        let (count_a, powered_a, connected_a) = summarize(&objects_a);
        let (count_b, powered_b, connected_b) = summarize(&objects_b);
        assert_eq!((count_a, powered_a, connected_a), (count_b, powered_b, connected_b));
    }

    #[test]
    fn summarize_power_state_and_connected_count_share_the_same_scope() {
        // The bug this regression test pins down: with hci0 off and hci1
        // on with two devices, the bar must say "on" with 2 connected —
        // never "off" with 2 connected, which would have been possible if
        // summarize had picked hci0 for power but summed devices across
        // both adapters.
        let mut objects = empty_objects();
        insert_adapter(&mut objects, "/org/bluez/hci0", adapter_with(Some(false)));
        insert_adapter(&mut objects, "/org/bluez/hci1", adapter_with(Some(true)));
        insert_device(
            &mut objects,
            "/org/bluez/hci1/dev_AA",
            device_with(Some(true)),
        );
        insert_device(
            &mut objects,
            "/org/bluez/hci1/dev_BB",
            device_with(Some(true)),
        );
        let (count, powered, connected) = summarize(&objects);
        assert_eq!(count, 2);
        assert_eq!(powered, Some(true));
        assert_eq!(connected, 2);
        // And the normalization must surface that as "on" with 2 connected.
        assert_eq!(
            bluetooth_from_bluez(count, powered, connected),
            Bluetooth::new(BluetoothState::On, 2)
        );
    }

    #[test]
    fn summarize_counts_connected_devices_across_adapters() {
        let mut objects = empty_objects();
        insert_adapter(&mut objects, "/org/bluez/hci0", adapter_with(Some(true)));
        // One connected device, one disconnected — only the connected one counts.
        insert_device(
            &mut objects,
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
            device_with(Some(true)),
        );
        insert_device(
            &mut objects,
            "/org/bluez/hci0/dev_11_22_33_44_55_66",
            device_with(Some(false)),
        );
        insert_device(
            &mut objects,
            "/org/bluez/hci0/dev_DE_AD_BE_EF",
            device_with(None),
        );
        // A second adapter with one of its devices connected.
        insert_adapter(&mut objects, "/org/bluez/hci1", adapter_with(Some(true)));
        insert_device(
            &mut objects,
            "/org/bluez/hci1/dev_AB_CD_EF_00_11_22",
            device_with(Some(true)),
        );
        let (count, powered, connected) = summarize(&objects);
        assert_eq!(count, 2);
        assert_eq!(powered, Some(true));
        assert_eq!(connected, 2);
    }

    #[test]
    fn summarize_ignores_devices_with_unknown_connected() {
        // A device that hasn't reported Connected yet counts as zero — we
        // only count affirmative `Connected = true` readings.
        let mut objects = empty_objects();
        insert_adapter(&mut objects, "/org/bluez/hci0", adapter_with(Some(true)));
        insert_device(&mut objects, "/org/bluez/hci0/dev_AA", device_with(None));
        let (_, _, connected) = summarize(&objects);
        assert_eq!(connected, 0);
    }

    #[test]
    fn bluetooth_bluez_round_trip_drives_visible_labels() {
        // The full BlueZ value → Bluetooth snapshot → label pipeline, end to
        // end, without a live system bus.
        let powered_on = {
            let mut objects = empty_objects();
            insert_adapter(&mut objects, "/org/bluez/hci0", adapter_with(Some(true)));
            insert_device(
                &mut objects,
                "/org/bluez/hci0/dev_AA",
                device_with(Some(true)),
            );
            insert_device(
                &mut objects,
                "/org/bluez/hci0/dev_BB",
                device_with(Some(true)),
            );
            let (count, powered, connected) = summarize(&objects);
            bluetooth_from_bluez(count, powered, connected)
        };
        assert_eq!(powered_on.state(), BluetoothState::On);
        assert_eq!(powered_on.connected(), 2);
        assert_eq!(powered_on.label(), "2 connected");

        let powered_off = bluetooth_from_bluez(1, Some(false), 0);
        assert_eq!(powered_off.state(), BluetoothState::Off);
        assert_eq!(powered_off.label(), "off");

        let no_adapter = bluetooth_from_bluez(0, None, 0);
        assert_eq!(no_adapter.state(), BluetoothState::Unavailable);
        assert_eq!(no_adapter.label(), "unavailable");
    }
}
