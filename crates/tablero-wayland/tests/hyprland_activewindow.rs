//! Integration coverage for the Hyprland active-window path, no live compositor.
//!
//! A fake producer parses realistic Hyprland JSON with the same
//! [`parse_active_window`] the live producer uses, then emits the resulting
//! [`Msg::ActiveWindow`] across the producer bridge into a [`TitleWidget`] —
//! exactly the path the bar drives. We assert the rendered title, the
//! truncation-aware redraw policy, and the absent-window handling.
//!
//! Per-monitor routing is covered by the widget's own unit tests in
//! `crates/tablero-core/src/widget/title.rs::tests`; this file exercises
//! the parser → bridge → widget seam.

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero_core::render::Bounds;
use tablero_core::widget::{ActiveWindow, Msg, TitleWidget, Widget};
use tablero_wayland::hyprland::parse_active_window;
use tablero_wayland::producer::{ProducerBridge, from_fn};

// A realistically-shaped `j/activewindow` reply: the parser must ignore
// everything but `class` and `title`.
const ACTIVEWINDOW_JSON: &str = r#"{
    "address": "0x55",
    "class": "firefox",
    "title": "GitHub",
    "workspace": {"id": 2}
}"#;

const EMPTY_ACTIVEWINDOW_JSON: &str = "{}";

/// One bar's worth of harness: a TitleWidget pre-bound to one monitor name,
/// plus what the render loop saw (cumulative counters).
struct Harness {
    widget: TitleWidget,
    processed: usize,
    redraws: usize,
}

impl Harness {
    fn new(monitor: &str) -> Self {
        Self {
            widget: TitleWidget::new(Bounds::new(0, 0, 320, 32)).with_monitor(monitor),
            processed: 0,
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

#[test]
fn activewindow_json_produces_the_expected_renderable_state() {
    let mut harness = Harness::new("DP-1");
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                if h.widget.update(&msg) {
                    h.redraws += 1;
                }
                h.processed += 1;
            }
        })
        .expect("channel registers");

    bridge.spawn(from_fn("hyprland-fake", move |tx| async move {
        let window = parse_active_window(ACTIVEWINDOW_JSON)?;
        tx.send(Msg::ActiveWindow {
            monitor: "DP-1".to_string(),
            window: Some(window),
        })?;
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| h.processed >= 1);

    assert_eq!(
        harness.widget.title(),
        "GitHub",
        "the widget extracted the title from the JSON"
    );
    assert_eq!(harness.redraws, 1, "the first snapshot drove one redraw");

    drop(bridge);
}

#[test]
fn an_unchanged_activewindow_does_not_redraw() {
    let mut harness = Harness::new("DP-1");
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                if h.widget.update(&msg) {
                    h.redraws += 1;
                }
                h.processed += 1;
            }
        })
        .expect("channel registers");

    // Same active window state twice (an irrelevant event re-queried and
    // got identical data): only the first should redraw.
    bridge.spawn(from_fn("hyprland-fake", move |tx| async move {
        let window = parse_active_window(ACTIVEWINDOW_JSON)?;
        tx.send(Msg::ActiveWindow {
            monitor: "DP-1".to_string(),
            window: Some(window.clone()),
        })?;
        tx.send(Msg::ActiveWindow {
            monitor: "DP-1".to_string(),
            window: Some(window),
        })?;
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| h.processed >= 2);

    assert_eq!(harness.processed, 2, "both messages crossed the bridge");
    assert_eq!(
        harness.redraws, 1,
        "only the first, state-changing snapshot redrew"
    );

    drop(bridge);
}

#[test]
fn empty_activewindow_object_collapses_to_an_empty_slot() {
    let mut harness = Harness::new("DP-1");
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                if h.widget.update(&msg) {
                    h.redraws += 1;
                }
                h.processed += 1;
            }
        })
        .expect("channel registers");

    bridge.spawn(from_fn("hyprland-fake", move |tx| async move {
        // Real window first (one redraw), then the documented "no focused
        // window" `{}` reply (a second redraw: the title disappears).
        tx.send(Msg::ActiveWindow {
            monitor: "DP-1".to_string(),
            window: Some(ActiveWindow::new("firefox", "GitHub")),
        })?;
        let empty = parse_active_window(EMPTY_ACTIVEWINDOW_JSON)?;
        tx.send(Msg::ActiveWindow {
            monitor: "DP-1".to_string(),
            window: Some(empty),
        })?;
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| h.processed >= 2);

    // The widget now reserves no slot for a trimmed-empty snapshot.
    assert_eq!(harness.widget.title(), "");
    assert_eq!(harness.redraws, 2, "real→empty is a visible change");

    drop(bridge);
}

#[test]
fn an_activewindow_window_none_collapses_to_an_empty_slot() {
    let mut harness = Harness::new("DP-1");
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                if h.widget.update(&msg) {
                    h.redraws += 1;
                }
                h.processed += 1;
            }
        })
        .expect("channel registers");

    // Some, then None: first redraw happened, second resets the widget.
    bridge.spawn(from_fn("hyprland-fake", |tx| async move {
        tx.send(Msg::ActiveWindow {
            monitor: "DP-1".to_string(),
            window: Some(ActiveWindow::new("firefox", "GitHub")),
        })?;
        tx.send(Msg::ActiveWindow {
            monitor: "DP-1".to_string(),
            window: None,
        })?;
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| h.processed >= 2);

    assert_eq!(harness.widget.title(), "");
    assert_eq!(
        harness.redraws, 2,
        "None after Some is also a visible change"
    );

    drop(bridge);
}
