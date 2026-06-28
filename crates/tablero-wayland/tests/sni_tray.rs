//! Integration coverage for the system-tray path, without a live session bus.
//!
//! Drives a real `calloop` event loop wired exactly as the bar wires it: a
//! [`ProducerBridge`] runs a fake producer that feeds tray snapshots — built
//! through the real [`tray_item_from_props`] normalization boundary — and the
//! resulting [`Msg::Tray`]s cross a calloop channel into the same app-state
//! update path the render loop uses ([`Dashboard::update`]). This exercises every
//! layer except the DBus I/O: item normalization, the lifecycle of items
//! appearing/updating/disappearing, the widget's redraw decision, and click
//! activation routing back through a [`Command`].

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero_core::render::Bounds;
use tablero_core::widget::{Command, Dashboard, Msg, TrayItem, TrayState, TrayWidget};
use tablero_wayland::producer::{ProducerBridge, from_fn};
use tablero_wayland::sni::tray_item_from_props;

/// One raw tray item reading: `(address, id, title, status)`. Icons are resolved
/// to `None` here — icon decoding is covered by the unit tests; this path focuses
/// on lifecycle and normalization.
type Reading = (&'static str, &'static str, &'static str, &'static str);

/// Stands in for the bar: owns a tray widget and records what it rendered.
struct Harness {
    dashboard: Dashboard,
    /// The tray widget's item count after each applied message.
    counts: Vec<usize>,
    redraws: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            dashboard: Dashboard::new(vec![Box::new(TrayWidget::new(Bounds::new(0, 0, 320, 32)))]),
            counts: Vec::new(),
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

/// Build a tray snapshot from raw readings through the real normalization
/// boundary, exactly as the live producer does before sending.
fn state_from(readings: &[Reading]) -> TrayState {
    let items: Vec<TrayItem> = readings
        .iter()
        .map(|(addr, id, title, status)| tray_item_from_props(addr, id, title, status, None))
        .collect();
    TrayState::new(items)
}

/// Run a sequence of tray snapshots through the bridge and return the item count
/// the widget showed after each, plus how many of them forced a redraw.
fn run_states(states: Vec<TrayState>) -> (Vec<usize>, usize) {
    let mut harness = Harness::new();
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    let expected = states.len();
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                if h.dashboard.update(&msg) {
                    h.redraws += 1;
                }
                if let Msg::Tray(state) = &msg {
                    h.counts.push(state.len());
                }
            }
        })
        .expect("channel registers");

    // A fake "DBus" producer: emits each prepared snapshot, exactly as the live
    // host would after reading the watcher and items.
    let messages: Vec<Msg> = states.into_iter().map(Msg::Tray).collect();
    bridge.spawn(from_fn("fake-sni", move |tx| async move {
        for msg in messages {
            tx.send(msg)?;
        }
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| {
        h.counts.len() >= expected
    });

    drop(bridge);
    (harness.counts, harness.redraws)
}

#[test]
fn tray_item_lifecycle_adds_updates_and_removes() {
    // An item appears, a second joins, the first updates its title, then both
    // leave — the count tracks the live set at each step.
    let (counts, redraws) = run_states(vec![
        state_from(&[(":1.1", "discord", "Discord", "Active")]),
        state_from(&[
            (":1.1", "discord", "Discord", "Active"),
            (":1.2", "telegram", "Telegram", "Active"),
        ]),
        state_from(&[
            (":1.1", "discord", "Discord — 2 unread", "NeedsAttention"),
            (":1.2", "telegram", "Telegram", "Active"),
        ]),
        state_from(&[]),
    ]);

    assert_eq!(counts, vec![1, 2, 2, 0]);
    // Every step changes the rendered tray, so each repaints once.
    assert_eq!(redraws, 4, "each distinct snapshot repaints once");
}

#[test]
fn re_reporting_the_same_items_does_not_repaint() {
    // The same two items reported twice — the second with reversed order and an
    // untrimmed title that normalizes equal: the first paints, the second is
    // filtered out by the widget's change detection.
    let (counts, redraws) = run_states(vec![
        state_from(&[
            (":1.1", "discord", "Discord", "Active"),
            (":1.2", "telegram", "Telegram", "Active"),
        ]),
        state_from(&[
            (":1.2", "telegram", "  Telegram  ", "Active"),
            (":1.1", "discord", "Discord", "Active"),
        ]),
    ]);

    assert_eq!(counts, vec![2, 2]);
    assert_eq!(redraws, 1, "an identical snapshot must not repaint");
}

#[test]
fn malformed_item_data_still_renders_without_crashing() {
    // An item with a blank title and an unrecognized status must not crash the
    // bar: it normalizes (title falls back to id, status to Passive) and still
    // occupies a clickable cell.
    let (counts, redraws) = run_states(vec![state_from(&[(":1.9", "weird-app", "", "???")])]);

    assert_eq!(counts, vec![1], "a malformed item still appears");
    assert_eq!(redraws, 1);
}

#[test]
fn empty_tray_renders_nothing_and_does_not_repaint() {
    // No items registered: the widget matches its empty initial state, so the
    // snapshot applies but forces no repaint.
    let (counts, redraws) = run_states(vec![state_from(&[])]);

    assert_eq!(counts, vec![0]);
    assert_eq!(redraws, 0, "an empty tray matches the initial state");
}

#[test]
fn clicking_a_tray_item_activates_it_by_address() {
    // After the tray is populated, a click in an item's cell yields the
    // activation command carrying that item's watcher address — the payload the
    // SNI executor turns into an Activate call.
    let mut dashboard = Dashboard::new(vec![Box::new(TrayWidget::new(Bounds::new(0, 0, 320, 32)))]);
    dashboard.layout(320, 32);
    dashboard.update(&Msg::Tray(state_from(&[
        (":1.1", "discord", "Discord", "Active"),
        (":1.2", "telegram", "Telegram", "Active"),
    ])));

    // Square cells of side 32 packed from the origin: address-sorted, :1.1 owns
    // [0,32) and :1.2 owns [32,64).
    assert_eq!(
        dashboard.on_click(16, 16),
        Some(Command::ActivateTrayItem(":1.1".to_string()))
    );
    assert_eq!(
        dashboard.on_click(48, 16),
        Some(Command::ActivateTrayItem(":1.2".to_string()))
    );
    // Empty space past the items activates nothing.
    assert_eq!(dashboard.on_click(200, 16), None);
}
