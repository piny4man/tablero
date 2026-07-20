//! Integration coverage for the Hyprland workspace path, no live compositor.
//!
//! A fake producer parses realistic Hyprland JSON with the same
//! [`snapshot_from_json`] the live producer uses, then emits the resulting
//! [`Msg::Workspaces`] across the producer bridge into a [`WorkspaceWidget`] —
//! exactly the path the bar drives. We assert the renderable state (the widget
//! label) and the redraw-only-on-change policy.

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero::hyprland::snapshot_from_json;
use tablero::producer::{ProducerBridge, from_fn};
use tablero::render::Bounds;
use tablero::widget::{Msg, Widget, WorkspaceWidget};

// Ids out of order with extra fields, active in the middle: the producer must
// normalize this to the sorted `1 [2] 3`.
const WORKSPACES_JSON: &str =
    r#"[{"id":2,"name":"2","windows":1},{"id":1,"name":"1","windows":4},{"id":3,"name":"3"}]"#;
const ACTIVE_JSON: &str = r#"{"id":2,"name":"2","monitor":"DP-1"}"#;

/// Stands in for the bar's workspace path: the widget plus what the loop saw.
struct Harness {
    widget: WorkspaceWidget,
    processed: usize,
    redraws: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            widget: WorkspaceWidget::new(Bounds::new(0, 0, 320, 32)),
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
fn workspace_json_produces_the_expected_renderable_state() {
    let mut harness = Harness::new();
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

    bridge.spawn(from_fn("hyprland-fake", |tx| async move {
        let snapshot = snapshot_from_json(WORKSPACES_JSON, ACTIVE_JSON)?;
        tx.send(Msg::Workspaces(snapshot))?;
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| h.processed >= 1);

    assert_eq!(
        harness.widget.label(),
        "1 [2] 3",
        "normalized workspaces render with the active one bracketed"
    );
    assert_eq!(harness.redraws, 1, "the first snapshot drove one redraw");

    drop(bridge);
}

#[test]
fn an_unchanged_snapshot_does_not_redraw() {
    let mut harness = Harness::new();
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

    // The same workspace state arrives twice (e.g. an irrelevant Hyprland event
    // triggered a re-query that produced an identical snapshot).
    bridge.spawn(from_fn("hyprland-fake", |tx| async move {
        let snapshot = snapshot_from_json(WORKSPACES_JSON, ACTIVE_JSON)?;
        tx.send(Msg::Workspaces(snapshot.clone()))?;
        tx.send(Msg::Workspaces(snapshot))?;
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
