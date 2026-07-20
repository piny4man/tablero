//! UPower battery source.
//!
//! Reads the battery from UPower's display device over the system DBus and emits
//! typed [`Msg::Battery`] snapshots through the [producer bridge](crate::producer),
//! so battery state reaches the render loop the same way every other message does
//! — the rendering code never talks to DBus directly.
//!
//! UPower aggregates every power supply into a synthetic *display device* at
//! `/org/freedesktop/UPower/devices/DisplayDevice`, implementing
//! `org.freedesktop.UPower.Device`. The producer reads its `Percentage`, `State`,
//! and `IsPresent` properties, normalizes them into a [`Battery`] (or `None` when
//! no battery is present), then re-reads and re-emits whenever any of those
//! properties changes. Normalization lives in [`battery_from_upower`], a pure
//! function the tests drive directly — the full DBus value → message → widget
//! path is covered without a live system bus.

use futures_util::stream::{StreamExt, select_all};
use log::warn;
use zbus::{Connection, proxy};

use crate::widget::{Battery, BatteryState, Msg};

use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// Map a raw UPower `State` enum value to a normalized [`BatteryState`].
///
/// UPower distinguishes more states than the bar needs to show; they collapse
/// onto the four the user actually reads. The values follow UPower's `Device`
/// state enum: `1` Charging, `2` Discharging, `3` Empty, `4` Fully charged,
/// `5` Pending charge, `6` Pending discharge, `0`/anything else Unknown.
pub fn upower_state(raw: u32) -> BatteryState {
    match raw {
        // Charging, or queued to start charging.
        1 | 5 => BatteryState::Charging,
        // On battery: actively discharging, drained, or queued to discharge.
        2 | 3 | 6 => BatteryState::Discharging,
        // Topped off.
        4 => BatteryState::Full,
        // Unknown (0) or any value a future UPower might add.
        _ => BatteryState::Unknown,
    }
}

/// Normalize the three raw UPower device readings into a battery snapshot.
///
/// Returns `None` when no battery is present — a desktop, or a laptop with the
/// pack removed — so the widget shows nothing rather than a meaningless `0%`.
/// Otherwise the raw `State` is mapped with [`upower_state`] and the percentage
/// is clamped and rounded by [`Battery::new`]. Pure over its inputs: the
/// integration tests drive the full reading → message → widget path through this
/// without a live system bus.
pub fn battery_from_upower(raw_state: u32, percentage: f64, is_present: bool) -> Option<Battery> {
    if !is_present {
        return None;
    }
    Some(Battery::new(upower_state(raw_state), percentage))
}

/// The subset of `org.freedesktop.UPower.Device` the bar reads, on the display
/// device by default. zbus generates `UPowerDeviceProxy` from this.
#[proxy(
    interface = "org.freedesktop.UPower.Device",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower/devices/DisplayDevice"
)]
trait UPowerDevice {
    /// Charge level, 0.0–100.0.
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<f64>;

    /// Raw charge state; see [`upower_state`] for the value mapping.
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;

    /// Whether a battery is physically present.
    #[zbus(property)]
    fn is_present(&self) -> zbus::Result<bool>;
}

/// Read the device's current properties and normalize them into a snapshot.
///
/// A failed read degrades to `None` (battery shown absent) and a logged warning
/// rather than ending the stream — a transient DBus hiccup never takes the source
/// down.
async fn read_snapshot(device: &UPowerDeviceProxy<'_>) -> Option<Battery> {
    match try_read(device).await {
        Ok(snapshot) => snapshot,
        Err(e) => {
            warn!("upower: reading battery state failed: {e}");
            None
        }
    }
}

/// Read the three properties and fold them into a normalized snapshot.
async fn try_read(device: &UPowerDeviceProxy<'_>) -> zbus::Result<Option<Battery>> {
    let is_present = device.is_present().await?;
    let percentage = device.percentage().await?;
    let state = device.state().await?;
    Ok(battery_from_upower(state, percentage, is_present))
}

/// A [`Producer`] that streams UPower battery changes into the render loop.
///
/// Construct with [`new`](UPowerProducer::new) and hand it to the producer
/// bridge; it reads an initial snapshot, then re-reads and emits on every change
/// to the display device's percentage, state, or presence until the system bus
/// closes or the render loop shuts down.
pub struct UPowerProducer;

impl UPowerProducer {
    /// Create a UPower battery producer.
    pub fn new() -> Self {
        Self
    }
}

impl Default for UPowerProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for UPowerProducer {
    fn name(&self) -> String {
        "upower-battery".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx))
    }
}

/// Drive the battery stream: initial snapshot, then re-read on each change.
///
/// Returns `Ok(())` once the render loop has gone away (a [`send`] reports the
/// channel closed) or the property streams end. A failed system-bus connection
/// propagates as an error the bridge logs and isolates — the bar keeps running,
/// the battery widget simply stays blank.
///
/// [`send`]: MsgSender::send
async fn run(tx: MsgSender) -> ProducerResult {
    let conn = Connection::system().await?;
    let device = UPowerDeviceProxy::new(&conn).await?;

    // Seed the bar with the current battery before the first change signal.
    if tx.send(Msg::Battery(read_snapshot(&device).await)).is_err() {
        return Ok(());
    }

    // Any of percentage, state, or presence changing warrants a re-read; merge
    // the three property streams and re-emit a fresh snapshot on each tick. An
    // unchanged snapshot is harmless — the widget reports no visible change.
    let mut changes = select_all([
        device
            .receive_percentage_changed()
            .await
            .map(|_| ())
            .boxed(),
        device.receive_state_changed().await.map(|_| ()).boxed(),
        device
            .receive_is_present_changed()
            .await
            .map(|_| ())
            .boxed(),
    ]);
    while changes.next().await.is_some() {
        if tx.send(Msg::Battery(read_snapshot(&device).await)).is_err() {
            return Ok(());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charging_states_normalize_to_charging() {
        // 1 Charging, 5 Pending charge.
        assert_eq!(upower_state(1), BatteryState::Charging);
        assert_eq!(upower_state(5), BatteryState::Charging);
    }

    #[test]
    fn on_battery_states_normalize_to_discharging() {
        // 2 Discharging, 3 Empty, 6 Pending discharge.
        assert_eq!(upower_state(2), BatteryState::Discharging);
        assert_eq!(upower_state(3), BatteryState::Discharging);
        assert_eq!(upower_state(6), BatteryState::Discharging);
    }

    #[test]
    fn fully_charged_normalizes_to_full() {
        assert_eq!(upower_state(4), BatteryState::Full);
    }

    #[test]
    fn unknown_and_unexpected_states_normalize_to_unknown() {
        assert_eq!(upower_state(0), BatteryState::Unknown);
        assert_eq!(upower_state(99), BatteryState::Unknown);
    }

    #[test]
    fn present_battery_normalizes_into_a_snapshot() {
        let battery = battery_from_upower(2, 84.6, true).expect("present");
        assert_eq!(battery.state(), BatteryState::Discharging);
        assert_eq!(battery.percent(), 85);
        assert_eq!(battery.label(), "85% discharging");
    }

    #[test]
    fn absent_battery_normalizes_to_none() {
        // No battery present: percentage/state are irrelevant, the snapshot is
        // absent so the widget shows nothing.
        assert_eq!(battery_from_upower(2, 0.0, false), None);
    }
}
