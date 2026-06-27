//! Integration coverage for the async producer bridge.
//!
//! Drives a real `calloop` event loop (no Wayland connection) wired exactly as
//! the bar wires it: a [`ProducerBridge`] runs producers on its Tokio runtime,
//! and their messages cross a calloop channel into the same app-state update
//! path the render loop uses ([`Dashboard::update`]).

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero_core::render::Bounds;
use tablero_core::widget::{ClockWidget, Dashboard, Msg};
use tablero_wayland::producer::{ProducerBridge, from_fn};

/// Stands in for the bar: owns app state and records what the loop observed.
struct Harness {
    dashboard: Dashboard,
    received: Vec<Msg>,
    redraws: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            dashboard: Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 320, 32)))]),
            received: Vec::new(),
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
fn producer_message_reaches_app_state_update_path() {
    let mut harness = Harness::new();
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                if h.dashboard.update(&msg) {
                    h.redraws += 1;
                }
                h.received.push(msg);
            }
        })
        .expect("channel registers");

    bridge.spawn(from_fn("emit-one", |tx| async move {
        tx.send(Msg::tick_now())?;
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| !h.received.is_empty());

    assert_eq!(harness.received.len(), 1, "exactly one message crossed");
    assert!(matches!(harness.received[0], Msg::Tick(_)));
    assert_eq!(
        harness.redraws, 1,
        "the producer message drove one visible state change"
    );

    // Keep the bridge alive until the loop work is done.
    drop(bridge);
}

#[test]
fn one_producer_error_does_not_stop_another_producer() {
    let mut harness = Harness::new();
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                h.received.push(msg);
            }
        })
        .expect("channel registers");

    // A failing producer's error is logged by the bridge, never propagated...
    bridge.spawn(from_fn("boom", |_tx| async move { Err("kaboom".into()) }));
    // ...so a healthy producer still delivers its message and the loop survives.
    bridge.spawn(from_fn("emit-one", |tx| async move {
        tx.send(Msg::tick_now())?;
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| !h.received.is_empty());

    assert_eq!(
        harness.received.len(),
        1,
        "healthy producer still delivered"
    );
    assert!(matches!(harness.received[0], Msg::Tick(_)));

    drop(bridge);
}
