//! Integration coverage for the NetworkManager connectivity path, without a live
//! system bus.
//!
//! Drives a real `calloop` event loop wired exactly as the bar wires it: a
//! [`ProducerBridge`] runs a fake producer that feeds raw NetworkManager readings
//! through the real [`network_from_nm`] boundary, and the resulting
//! [`Msg::Network`]s cross a calloop channel into the same app-state update path
//! the render loop uses ([`Dashboard::update`]). This exercises every layer except
//! the DBus I/O: normalization, message passing, and the widget's redraw decision,
//! across the common connectivity states and a NetworkManager-unavailable
//! environment.

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero_core::render::Bounds;
use tablero_core::widget::{Dashboard, Msg, NetworkWidget};
use tablero_wayland::networkmanager::network_from_nm;
use tablero_wayland::producer::{ProducerBridge, from_fn};

/// One raw NetworkManager reading: `(State, Type, Ssid)`, where `Type` and `Ssid`
/// are absent when NetworkManager would not report them.
type Reading = (u32, Option<&'static str>, Option<&'static str>);

/// Stands in for the bar: owns a network widget and records what it rendered.
struct Harness {
    dashboard: Dashboard,
    labels: Vec<String>,
    redraws: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            dashboard: Dashboard::new(vec![Box::new(NetworkWidget::new(Bounds::new(
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

/// Run a sequence of `Msg::Network`s through the bridge and return the labels the
/// network widget rendered plus how many of them forced a redraw.
fn run_messages(messages: Vec<Msg>) -> (Vec<String>, usize) {
    let mut harness = Harness::new();
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    let expected = messages.len();
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                if h.dashboard.update(&msg) {
                    h.redraws += 1;
                }
                // Re-read the label off the widget's message to record exactly
                // what the bar now shows for this reading.
                if let Msg::Network(snapshot) = &msg {
                    h.labels
                        .push(snapshot.as_ref().map(|n| n.label()).unwrap_or_default());
                }
            }
        })
        .expect("channel registers");

    // A fake "DBus" producer: emits each prepared message, exactly as the live
    // producer would after reading the bus.
    bridge.spawn(from_fn("fake-networkmanager", move |tx| async move {
        for msg in messages {
            tx.send(msg)?;
        }
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| {
        h.labels.len() >= expected
    });

    drop(bridge);
    (harness.labels, harness.redraws)
}

/// Run raw NetworkManager readings through the real boundary, exactly as the live
/// producer normalizes them before sending.
fn drive(readings: Vec<Reading>) -> (Vec<String>, usize) {
    let messages = readings
        .into_iter()
        .map(|(state, conn_type, ssid)| Msg::Network(Some(network_from_nm(state, conn_type, ssid))))
        .collect();
    run_messages(messages)
}

#[test]
fn common_network_states_render_expected_labels() {
    // 20 disconnected, 70 connected ethernet, 70 connected wifi (named), 0
    // unknown — the four states the widget must show clearly.
    let (labels, redraws) = drive(vec![
        (20, None, None),
        (70, Some("802-3-ethernet"), None),
        (70, Some("802-11-wireless"), Some("home-net")),
        (0, None, None),
    ]);

    assert_eq!(
        labels,
        vec![
            "disconnected".to_string(),
            "wired".to_string(),
            "wifi home-net".to_string(),
            "unknown".to_string(),
        ]
    );
    // Every reading here changes the visible label, so each drives a redraw.
    assert_eq!(redraws, 4, "each distinct reading repaints once");
}

#[test]
fn wireless_without_ssid_renders_bare_wifi() {
    // Connected to Wi-Fi but the SSID could not be read (no associated AP yet):
    // the widget shows the bare "wifi" label, never an empty "wifi ".
    let (labels, redraws) = drive(vec![(70, Some("802-11-wireless"), None)]);

    assert_eq!(labels, vec!["wifi".to_string()]);
    assert_eq!(redraws, 1);
}

#[test]
fn unchanged_connectivity_does_not_repaint() {
    // The same wireless network reported twice (the second with an untrimmed
    // SSID that normalizes equal): the first paints, the second is filtered out.
    let (labels, redraws) = drive(vec![
        (70, Some("802-11-wireless"), Some("home-net")),
        (70, Some("802-11-wireless"), Some("  home-net  ")),
    ]);

    assert_eq!(
        labels,
        vec!["wifi home-net".to_string(), "wifi home-net".to_string()]
    );
    assert_eq!(redraws, 1, "an identical snapshot must not repaint");
}

#[test]
fn networkmanager_unavailable_renders_blank() {
    // NetworkManager is not running / the read failed: the live producer degrades
    // to `Msg::Network(None)`, which the widget renders as nothing.
    let (labels, redraws) = run_messages(vec![Msg::Network(None)]);

    assert_eq!(
        labels,
        vec![String::new()],
        "unavailable network shows nothing"
    );
    assert_eq!(
        redraws, 0,
        "an absent snapshot matches the empty initial state: no repaint"
    );
}
