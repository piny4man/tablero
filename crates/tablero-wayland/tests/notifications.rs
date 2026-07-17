//! Integration coverage for the notifications path, without a live swaync.
//!
//! Drives a real `calloop` event loop wired exactly as the bar wires it: a
//! [`ProducerBridge`] runs a fake producer that feeds a sequence of pre-built
//! [`Msg::Notifications`]s through the same app-state update path the render
//! loop uses ([`Dashboard::update`]). This exercises every layer except the
//! D-Bus I/O: message passing, normalization at the widget boundary via
//! [`notifications_from_swaync`], the redraw decision, and the two-button
//! click path, across the common states (quiet, pending, DND, daemon absent).

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero_core::render::Bounds;
use tablero_core::widget::{
    ClickButton, Command, Dashboard, Msg, Notifications, NotificationsWidget, Widget,
};
use tablero_wayland::notifications::notifications_from_swaync;
use tablero_wayland::producer::{ProducerBridge, from_fn};

/// Stands in for the bar: owns a notifications widget and records redraws.
struct Harness {
    dashboard: Dashboard,
    seen: usize,
    redraws: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            dashboard: Dashboard::new(vec![Box::new(NotificationsWidget::new(Bounds::new(
                0, 0, 320, 32,
            )))]),
            seen: 0,
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

/// Drive a sequence of pre-built [`Msg::Notifications`]s through the bridge
/// and return how many readings forced a redraw.
fn drive(readings: Vec<Msg>) -> usize {
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
                if matches!(msg, Msg::Notifications(_)) {
                    h.seen += 1;
                }
            }
        })
        .expect("channel registers");

    bridge.spawn(from_fn("fake-notifications", move |tx| async move {
        for msg in readings {
            tx.send(msg)?;
        }
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| h.seen >= expected);

    drop(bridge);
    harness.redraws
}

/// A reading exactly as the producer would normalize it from swaync.
fn reading(count: u32, dnd: bool) -> Msg {
    Msg::Notifications(Some(notifications_from_swaync(count, dnd)))
}

#[test]
fn common_notification_states_each_force_a_redraw() {
    // Quiet, first pending notification (dot appears), DND on (bell slashes),
    // daemon gone (widget hides), daemon back.
    let redraws = drive(vec![
        reading(0, false),
        reading(1, false),
        reading(1, true),
        Msg::Notifications(None),
        reading(0, false),
    ]);
    assert_eq!(redraws, 5, "every reading changes the visible state");
}

#[test]
fn identical_reading_is_not_a_redraw() {
    let redraws = drive(vec![reading(2, false), reading(2, false)]);
    assert_eq!(redraws, 1, "no-op state changes must not repaint");
}

#[test]
fn readings_differing_only_in_dropped_daemon_fields_are_no_ops() {
    // The daemon fields the bar does not render (panel open, inhibitors)
    // never reach the snapshot, so two readings with the same count and DND
    // normalize identically whatever else changed daemon-side.
    let a = Msg::Notifications(Some(notifications_from_swaync(3, false)));
    let b = Msg::Notifications(Some(Notifications::new(3, false)));
    let redraws = drive(vec![a, b]);
    assert_eq!(redraws, 1);
}

#[test]
fn absent_reading_before_any_notification_is_not_a_redraw() {
    // "swaync not running" matches the empty initial state.
    let redraws = drive(vec![Msg::Notifications(None)]);
    assert_eq!(redraws, 0);
}

#[test]
fn clicks_route_by_button_through_the_dashboard() {
    let mut dashboard = Dashboard::new(vec![Box::new(NotificationsWidget::new(Bounds::new(
        0, 0, 320, 32,
    )))]);
    dashboard.update(&reading(1, false));

    assert_eq!(
        dashboard.on_click(10, 10, ClickButton::Left),
        Some(Command::ToggleNotificationPanel)
    );
    assert_eq!(
        dashboard.on_click(10, 10, ClickButton::Right),
        Some(Command::ToggleNotificationsDnd)
    );
}

#[test]
fn clicks_are_ignored_while_the_daemon_is_absent() {
    // Before a first reading (or after swaync leaves the bus) there is no
    // panel to toggle, so neither button produces a command.
    let dashboard = Dashboard::new(vec![Box::new(NotificationsWidget::new(Bounds::new(
        0, 0, 320, 32,
    )))]);
    assert_eq!(dashboard.on_click(10, 10, ClickButton::Left), None);
    assert_eq!(dashboard.on_click(10, 10, ClickButton::Right), None);
}

#[test]
fn the_widget_reserves_no_slot_before_any_reading() {
    // The notifications widget measures zero before the first reading, so a
    // fresh bar with no swaync running does not reserve a slot.
    let widget = NotificationsWidget::new(Bounds::new(0, 0, 320, 32));
    let mut ctx = tablero_core::render::RenderContext::new(320, 32);
    assert_eq!(widget.measure(&mut ctx, 32), 0);
}
