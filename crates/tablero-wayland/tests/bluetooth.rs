//! Integration coverage for the Bluetooth path, without a live system bus.
//!
//! Drives a real `calloop` event loop wired exactly as the bar wires it: a
//! [`ProducerBridge`] runs a fake producer that feeds a sequence of pre-built
//! [`Msg::Bluetooth`]s through the same app-state update path the render
//! loop uses ([`Dashboard::update`]). This exercises every layer except the
//! DBus I/O: message passing, normalization at the widget boundary, and the
//! redraw decision, across the common adapter states (on, off,
//! unavailable, with and without connected devices).

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero_core::render::Bounds;
use tablero_core::widget::{Bluetooth, BluetoothState, BluetoothWidget, Dashboard, Msg, Widget};
use tablero_wayland::bluetooth::bluetooth_from_bluez;
use tablero_wayland::producer::{ProducerBridge, from_fn};

/// Stands in for the bar: owns a bluetooth widget and records what it
/// rendered.
struct Harness {
    dashboard: Dashboard,
    labels: Vec<String>,
    redraws: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            dashboard: Dashboard::new(vec![Box::new(BluetoothWidget::new(Bounds::new(
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

/// Drive a sequence of raw BlueZ readings through the bridge and return
/// the labels the bluetooth widget rendered plus how many readings forced a
/// redraw.
///
/// Each `(adapter_count, powered, connected)` triple is normalized through
/// the real [`bluetooth_from_bluez`] boundary and shipped as a
/// [`Msg::Bluetooth`], exactly as the live producer would after reading
/// the bus.
fn drive(readings: Vec<(usize, Option<bool>, u32)>) -> (Vec<String>, usize) {
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
                // Re-read the label off the widget's message to record
                // exactly what the bar now shows for this reading.
                if let Msg::Bluetooth(snapshot) = &msg {
                    h.labels.push(snapshot.label());
                }
            }
        })
        .expect("channel registers");

    // A fake "BlueZ" producer: emits each raw reading normalized through the
    // real bluetooth boundary, exactly as the live producer would after
    // reading the bus.
    bridge.spawn(from_fn("fake-bluez", move |tx| async move {
        for (adapter_count, powered, connected) in readings {
            tx.send(Msg::Bluetooth(bluetooth_from_bluez(
                adapter_count,
                powered,
                connected,
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
fn common_adapter_states_render_expected_labels() {
    // No adapter, adapter on with 2 devices, adapter off, adapter on with 0
    // devices.
    let (labels, redraws) = drive(vec![
        (0, None, 0),
        (1, Some(true), 2),
        (1, Some(false), 99),
        (1, Some(true), 0),
    ]);

    assert_eq!(
        labels,
        vec![
            "unavailable".to_string(),
            "2 connected".to_string(),
            "off".to_string(),
            "on".to_string(),
        ]
    );
    // The widget starts in `Unavailable`, so the first `Unavailable`
    // reading is a no-op; every other reading changes the visible label
    // and repaints.
    assert_eq!(redraws, 3, "3 distinct state changes repaint once each");
}

#[test]
fn an_unpowered_adapter_with_a_stale_connected_count_renders_off() {
    // The connected count is forced to zero when the adapter is off, so the
    // widget never shows `99 connected off` or any other mixed label.
    let (labels, _) = drive(vec![(1, Some(false), 99)]);
    assert_eq!(labels, vec!["off".to_string()]);
}

#[test]
fn missing_powered_with_a_present_adapter_renders_unavailable() {
    // The adapter exists but didn't tell us whether it's on: show
    // unavailable rather than guessing.
    let (labels, _) = drive(vec![(1, None, 0)]);
    assert_eq!(labels, vec!["unavailable".to_string()]);
}

#[test]
fn an_unavailable_reading_after_unavailable_is_not_a_redraw() {
    // A second unavailable reading matches the initial state of the widget
    // (which starts in Unavailable) and is filtered out by the redraw
    // policy.
    let (labels, redraws) = drive(vec![(0, None, 0), (0, None, 0)]);
    assert_eq!(
        labels,
        vec!["unavailable".to_string(), "unavailable".to_string()]
    );
    assert_eq!(redraws, 0, "no-op state changes must not repaint");
}

#[test]
fn an_on_to_off_transition_drives_a_redraw() {
    // The widget starts in Unavailable, so the first On reading is itself
    // a state change. The subsequent Off reading is the second redraw.
    let (labels, redraws) = drive(vec![(1, Some(true), 2), (1, Some(false), 0)]);
    assert_eq!(labels, vec!["2 connected".to_string(), "off".to_string()]);
    assert_eq!(redraws, 2, "two distinct state changes, two redraws");
}

#[test]
fn the_widget_reserves_a_slot_even_before_any_reading() {
    // The bluetooth widget always reserves a slot (showing `unavailable`),
    // unlike battery/network which measure zero before their first reading.
    // We drive the widget directly: its initial state is `unavailable` and
    // its measure is always non-zero, so the dashboard would lay it out
    // even on a fresh bar.
    let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
    assert_eq!(widget.label(), "unavailable");
    let mut ctx = tablero_core::render::RenderContext::new(320, 32);
    assert!(
        widget.measure(&mut ctx, 32) > 0,
        "bluetooth widget always measures a non-zero slot width"
    );
    // A fresh Unavailable reading is a no-op against the initial state.
    assert!(!widget.update(&Msg::Bluetooth(Bluetooth::new(
        BluetoothState::Unavailable,
        0
    ))));
}

#[test]
fn a_configured_on_click_emits_a_run_program_command() {
    // End-to-end click path: a bluetooth widget built with an on-click
    // path returns `Some(Command::RunProgram(path))` for clicks inside its
    // bounds, and `None` for clicks outside or when no path is configured.
    use std::path::PathBuf;
    use tablero_core::widget::Command;

    let no_click = BluetoothWidget::new(Bounds::new(0, 0, 200, 32));
    assert_eq!(no_click.on_click(10, 10), None);

    let clicked = BluetoothWidget::new(Bounds::new(0, 0, 200, 32))
        .with_on_click(Some(PathBuf::from("/usr/bin/blueman-manager")));
    assert_eq!(
        clicked.on_click(10, 10),
        Some(Command::RunProgram(PathBuf::from(
            "/usr/bin/blueman-manager"
        )))
    );
    // Off-bounds clicks never reach the executor: (0, 0) is at the
    // top-left corner of a (50, 10, 200, 32) widget's padding region, and
    // (260, 10) is past the right edge.
    let far_clicked = BluetoothWidget::new(Bounds::new(50, 10, 200, 32))
        .with_on_click(Some(PathBuf::from("/usr/bin/blueman-manager")));
    assert_eq!(far_clicked.on_click(0, 0), None);
    assert_eq!(far_clicked.on_click(260, 10), None);
}

#[test]
fn a_run_program_command_actually_spawns_the_program() {
    // End-to-end through the bridge: a bluetooth widget built with an
    // on-click path emits `RunProgram(<script>)`, the bar fans that command
    // out to the run-commands executor over its command channel, and the
    // executor spawns the script directly (no shell). We verify the spawn
    // landed by waiting for the script's marker file to appear.
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use tablero_core::widget::Command;
    use tablero_wayland::command::{command_channel, run_commands};

    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("clicked.marker");
    let script = dir.path().join("on-click.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display()))
        .expect("write script");
    let mut perms = std::fs::metadata(&script).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).expect("chmod");

    let widget =
        BluetoothWidget::new(Bounds::new(0, 0, 200, 32)).with_on_click(Some(script.clone()));
    let Some(Command::RunProgram(path)) = widget.on_click(10, 10) else {
        panic!("widget should emit a RunProgram command");
    };
    assert_eq!(path, PathBuf::from(&script));

    // Drive the command through a real executor task, exactly as the bar
    // wires it.
    let (bridge, _channel) = ProducerBridge::new().expect("runtime starts");
    let (cmd_tx, cmd_rx) = command_channel();
    bridge.spawn_task("run-commands", run_commands(cmd_rx));
    cmd_tx.send(Command::RunProgram(script.clone())).unwrap();
    drop(cmd_tx);

    // Wait briefly for the spawned child to land.
    let mut waited = Duration::ZERO;
    let step = Duration::from_millis(20);
    while !marker.exists() && waited < Duration::from_secs(2) {
        std::thread::sleep(step);
        waited += step;
    }
    assert!(
        marker.exists(),
        "executor spawned the script and the marker file appeared"
    );
}
