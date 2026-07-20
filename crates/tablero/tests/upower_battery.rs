//! Integration coverage for the UPower battery path, without a live system bus.
//!
//! Drives a real `calloop` event loop wired exactly as the bar wires it: a
//! [`ProducerBridge`] runs a fake producer that feeds raw UPower readings through
//! the real [`battery_from_upower`] boundary, and the resulting [`Msg::Battery`]s
//! cross a calloop channel into the same app-state update path the render loop
//! uses ([`Dashboard::update`]). This exercises every layer except the DBus I/O:
//! normalization, message passing, and the widget's redraw decision, across the
//! common power states and a no-battery environment.

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero::producer::{ProducerBridge, from_fn};
use tablero::render::Bounds;
use tablero::upower::battery_from_upower;
use tablero::widget::{BatteryWidget, Dashboard, Msg};

/// One raw UPower display-device reading: `(State, Percentage, IsPresent)`.
type Reading = (u32, f64, bool);

/// Stands in for the bar: owns a battery widget and records what it rendered.
struct Harness {
    dashboard: Dashboard,
    labels: Vec<String>,
    redraws: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            dashboard: Dashboard::new(vec![Box::new(BatteryWidget::new(Bounds::new(
                0, 0, 320, 32,
            )))]),
            labels: Vec::new(),
            redraws: 0,
        }
    }
}

/// Pump the loop until `done(&harness)` holds or the timeout elapses.
fn pump_until(
    event_loop: &mut EventLoop<Harness>,
    harness: &mut Harness,
    done: impl Fn(&Harness) -> bool,
) {
    let mut waited = Duration::ZERO;
    let step = Duration::from_millis(20);
    while !done(harness) && waited < Duration::from_secs(5) {
        event_loop
            .dispatch(step, harness)
            .expect("loop dispatch succeeds");
        waited += step;
    }
}

/// Run a sequence of raw UPower readings through the bridge and return the
/// labels the battery widget rendered plus how many readings forced a redraw.
fn drive(readings: Vec<Reading>) -> (Vec<String>, usize) {
    let mut harness = Harness::new();
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    let expected = readings.len();
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                if h.dashboard.update(&msg) {
                    h.redraws += 1;
                }
                // Re-read the label off the widget's message to record exactly
                // what the bar now shows for this reading.
                if let Msg::Battery(snapshot) = &msg {
                    h.labels
                        .push(snapshot.map(|b| b.label()).unwrap_or_default());
                }
            }
        })
        .expect("channel registers");

    // A fake "DBus" producer: emits each raw reading normalized through the real
    // UPower boundary, exactly as the live producer would after reading the bus.
    bridge.spawn(from_fn("fake-upower", move |tx| async move {
        for (state, percentage, is_present) in readings {
            tx.send(Msg::Battery(battery_from_upower(
                state, percentage, is_present,
            )))?;
        }
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| {
        h.labels.len() >= expected
    });

    drop(bridge);
    (harness.labels, harness.redraws)
}

#[test]
fn common_power_states_render_expected_labels() {
    // 2 Discharging, 1 Charging, 4 Fully charged, 0 Unknown — all present.
    let (labels, redraws) = drive(vec![
        (2, 84.6, true),
        (1, 85.0, true),
        (4, 100.0, true),
        (0, 100.0, true),
    ]);

    assert_eq!(
        labels,
        vec![
            "85% discharging".to_string(),
            "85% charging".to_string(),
            "100% full".to_string(),
            "100% unknown".to_string(),
        ]
    );
    // Every reading here changes the visible label, so each drives a redraw.
    assert_eq!(redraws, 4, "each distinct reading repaints once");
}

#[test]
fn no_battery_environment_renders_blank() {
    // A desktop / removed pack: IsPresent is false, so nothing is shown even
    // though the daemon still reports a percentage and state.
    let (labels, redraws) = drive(vec![(2, 0.0, false)]);

    assert_eq!(labels, vec![String::new()], "absent battery shows nothing");
    assert_eq!(
        redraws, 0,
        "an absent battery matches the empty initial state: no repaint"
    );
}

#[test]
fn unchanged_whole_percent_does_not_repaint() {
    // Two readings a sub-percent apart normalize to the same 85% discharging:
    // the first paints, the second is filtered out by the redraw policy.
    let (labels, redraws) = drive(vec![(2, 85.0, true), (2, 85.4, true)]);

    assert_eq!(
        labels,
        vec!["85% discharging".to_string(), "85% discharging".to_string()]
    );
    assert_eq!(redraws, 1, "sub-percent jitter must not repaint");
}
