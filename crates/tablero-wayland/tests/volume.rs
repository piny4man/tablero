//! Integration coverage for the volume path, without a live PipeWire server.
//!
//! Drives a real `calloop` event loop wired exactly as the bar wires it: a
//! [`ProducerBridge`] runs a fake producer that feeds a sequence of pre-built
//! [`Msg::Volume`]s through the same app-state update path the render loop
//! uses ([`Dashboard::update`]). This exercises every layer except the
//! PipeWire I/O: message passing, normalization at the widget boundary, the
//! redraw decision, and the on-click path, across the common widget states
//! (present, absent, muted, device change, identical re-readings).

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero_core::render::Bounds;
use tablero_core::widget::{Dashboard, DeviceKind, Msg, Volume, VolumeWidget, Widget};
use tablero_wayland::producer::{ProducerBridge, from_fn};

/// Stands in for the bar: owns a volume widget and records what it rendered.
struct Harness {
    dashboard: Dashboard,
    labels: Vec<String>,
    redraws: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            dashboard: Dashboard::new(vec![Box::new(VolumeWidget::new(Bounds::new(
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

/// Drive a sequence of pre-built [`Msg::Volume`]s through the bridge and
/// return the labels the volume widget rendered plus how many readings
/// forced a redraw.
fn drive(readings: Vec<Msg>) -> (Vec<String>, usize) {
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
                if let Msg::Volume(snapshot) = &msg {
                    let label = snapshot.as_ref().map(|v| v.label()).unwrap_or_default();
                    h.labels.push(label);
                }
            }
        })
        .expect("channel registers");

    bridge.spawn(from_fn("fake-volume", move |tx| async move {
        for msg in readings {
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

fn vol(level: f32, muted: bool, device: DeviceKind) -> Msg {
    Msg::Volume(Some(Volume::new(level, muted, device)))
}

#[test]
fn common_volume_states_render_expected_labels() {
    // Unmuted, muted, absent, and back to unmuted.
    let (labels, redraws) = drive(vec![
        vol(0.5, false, DeviceKind::Speakers),
        vol(0.5, true, DeviceKind::Speakers),
        Msg::Volume(None),
        vol(0.25, false, DeviceKind::Headphones),
    ]);

    assert_eq!(
        labels,
        vec![
            "Vol 50%".to_string(),
            "Mute".to_string(),
            String::new(),
            "Vol 25%".to_string(),
        ]
    );
    // First reading is dirty (empty → "Vol 50%"); mute flips the label;
    // None empties the label; the fourth reading re-paints. Every message
    // causes a visible change, so four redraws.
    assert_eq!(redraws, 4, "every reading changes the visible state");
}

#[test]
fn identical_reading_is_not_a_redraw() {
    // The same normalized snapshot: no repaint. Sub-percent jitter (0.42 vs
    // 0.424) normalizes to the same whole percent, so the second reading
    // is a no-op.
    let (labels, redraws) = drive(vec![
        vol(0.42, false, DeviceKind::Speakers),
        vol(0.424, false, DeviceKind::Speakers),
    ]);
    assert_eq!(labels, vec!["Vol 42%".to_string(), "Vol 42%".to_string()]);
    assert_eq!(redraws, 1, "no-op state changes must not repaint");
}

#[test]
fn a_new_level_is_a_visible_change() {
    let (labels, redraws) = drive(vec![
        vol(0.42, false, DeviceKind::Speakers),
        vol(0.65, false, DeviceKind::Speakers),
    ]);
    assert_eq!(labels, vec!["Vol 42%".to_string(), "Vol 65%".to_string()]);
    assert_eq!(redraws, 2);
}

#[test]
fn a_toggle_to_mute_is_a_visible_change() {
    let (labels, redraws) = drive(vec![
        vol(0.42, false, DeviceKind::Speakers),
        vol(0.42, true, DeviceKind::Speakers),
    ]);
    assert_eq!(labels, vec!["Vol 42%".to_string(), "Mute".to_string()]);
    assert_eq!(redraws, 2);
}

#[test]
fn a_toggle_back_from_mute_is_a_visible_change() {
    let (labels, redraws) = drive(vec![
        vol(0.42, true, DeviceKind::Speakers),
        vol(0.42, false, DeviceKind::Speakers),
    ]);
    assert_eq!(labels, vec!["Mute".to_string(), "Vol 42%".to_string()]);
    assert_eq!(redraws, 2);
}

#[test]
fn a_new_device_kind_is_a_visible_change() {
    // Even at the same level and mute state, switching from speakers to
    // headphones changes the rendered glyph — the user needs to see the
    // difference, so the snapshot carries the device and equality says
    // they are not the same.
    let (_, redraws) = drive(vec![
        vol(0.5, false, DeviceKind::Speakers),
        vol(0.5, false, DeviceKind::Headphones),
    ]);
    assert_eq!(redraws, 2);
}

#[test]
fn a_volume_going_absent_is_a_visible_change_then_blank() {
    let (_, redraws) = drive(vec![
        vol(0.42, false, DeviceKind::Speakers),
        Msg::Volume(None),
    ]);
    assert_eq!(redraws, 2);
}

#[test]
fn absent_reading_before_any_volume_is_not_a_redraw() {
    // "Unavailable" matches the empty initial state, so the first reading
    // is a no-op against the empty state.
    let (labels, redraws) = drive(vec![Msg::Volume(None)]);
    assert_eq!(labels, vec![String::new()]);
    assert_eq!(redraws, 0);
}

#[test]
fn the_widget_reserves_no_slot_before_any_reading() {
    // The volume widget measures zero before the first reading, so a fresh
    // bar with no PipeWire yet does not reserve a slot — matching how the
    // network / system widgets treat an absent source.
    let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
    let mut ctx = tablero_core::render::RenderContext::new(320, 32);
    assert_eq!(widget.measure(&mut ctx, 32), 0);
    widget.update(&vol(0.5, false, DeviceKind::Speakers));
    assert!(widget.measure(&mut ctx, 32) > 0);
}

#[test]
fn a_run_program_command_actually_spawns_the_program() {
    // End-to-end click path: a volume widget built with an on-click path
    // emits `RunProgram(<script>)`, the bar fans that command out to the
    // run-commands executor over its command channel, and the executor
    // spawns the script directly (no shell). Verify the spawn landed by
    // waiting for the script's marker file to appear.
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

    let widget = VolumeWidget::new(Bounds::new(0, 0, 200, 32)).with_on_click(Some(script.clone()));
    let Some(Command::RunProgram(path)) = widget.on_click(10, 10) else {
        panic!("widget should emit a RunProgram command");
    };
    assert_eq!(path, PathBuf::from(&script));

    // Drive the command through a real executor task, exactly as the bar
    // would. The executor spawns the script as a child process and the
    // script writes the marker file.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (tx, rx) = command_channel();
    tx.send(Command::RunProgram(script.clone())).unwrap();
    drop(tx);
    rt.block_on(run_commands(rx)).unwrap();

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
