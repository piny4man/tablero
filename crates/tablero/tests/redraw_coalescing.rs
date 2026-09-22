//! Redraw coalescing over a real calloop loop: messages that arrive together
//! share one painted frame, and the damage reported for that frame covers every
//! widget they changed.
//!
//! The harness mirrors the host loop's shape — the channel callback applies each
//! message and only *requests* a repaint, and the flush runs once per dispatch.

use std::time::{Duration, Instant};

use calloop::EventLoop;
use calloop::channel::{Event as ChannelEvent, channel};
use calloop::timer::{TimeoutAction, Timer};

use tablero::redraw::{FRAME_CALLBACK_TIMEOUT, RedrawScheduler};
use tablero::render::{Bounds, RenderContext};
use tablero::widget::{ClockWidget, Damage, Dashboard, Msg, WorkspaceWidget, Workspaces};

const WIDTH: u32 = 400;
const HEIGHT: u32 = 32;

struct Harness {
    dashboard: Dashboard,
    ctx: RenderContext,
    scheduler: RedrawScheduler,
    applied: usize,
    frames: Vec<(&'static str, Damage)>,
}

impl Harness {
    fn new() -> Self {
        let placeholder = Bounds::new(0, 0, 1, 1);
        let mut harness = Self {
            dashboard: Dashboard::with_zones(
                vec![Box::new(WorkspaceWidget::new(placeholder))],
                vec![],
                vec![Box::new(ClockWidget::new(placeholder))],
            ),
            ctx: RenderContext::new(WIDTH, HEIGHT),
            scheduler: RedrawScheduler::new(),
            applied: 0,
            frames: Vec::new(),
        };
        harness.apply(&workspaces(1), "workspaces");
        harness.apply(&Msg::tick_now(), "tick");
        harness.flush();
        harness.frames.clear();
        harness
    }

    fn apply(&mut self, msg: &Msg, kind: &'static str) {
        self.applied += 1;
        if self.dashboard.update(msg) {
            self.scheduler.request(kind);
        }
    }

    /// The host loop's post-dispatch step.
    fn flush(&mut self) {
        if let Some(cause) = self.scheduler.take_due(Instant::now()) {
            self.dashboard.layout(&mut self.ctx, WIDTH, HEIGHT);
            let damage = self.dashboard.take_damage();
            self.dashboard.draw(&mut self.ctx);
            self.frames.push((cause, damage));
        }
    }
}

fn workspaces(active: i32) -> Msg {
    Msg::Workspaces(Workspaces::new([1, 2, 3], active))
}

/// Dispatch and flush until `expected` messages have been applied.
fn pump(event_loop: &mut EventLoop<Harness>, harness: &mut Harness, expected: usize) {
    let target = harness.applied + expected;
    let deadline = Instant::now() + Duration::from_secs(5);
    while harness.applied < target && Instant::now() < deadline {
        event_loop
            .dispatch(Duration::from_millis(20), harness)
            .expect("loop dispatch succeeds");
        harness.flush();
    }
    assert_eq!(harness.applied, target, "every message was applied");
}

fn harness_loop() -> (EventLoop<'static, Harness>, calloop::channel::Sender<Msg>) {
    let event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let (tx, rx) = channel();
    event_loop
        .handle()
        .insert_source(rx, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                h.apply(&msg, "workspaces");
            }
        })
        .expect("channel registers");
    (event_loop, tx)
}

#[test]
fn messages_pending_in_one_dispatch_paint_one_frame() {
    let mut harness = Harness::new();
    let (mut event_loop, tx) = harness_loop();

    for active in [2, 3, 1, 2] {
        tx.send(workspaces(active)).expect("loop is alive");
    }
    pump(&mut event_loop, &mut harness, 4);

    assert_eq!(harness.frames.len(), 1, "four changes, one repaint");
    let (cause, damage) = harness.frames[0];
    assert_eq!(cause, "workspaces");
    assert!(
        matches!(damage, Damage::Region(region) if region.width < WIDTH),
        "only the workspace strip is damaged, got {damage:?}"
    );
}

#[test]
fn messages_that_change_nothing_paint_no_frame() {
    let mut harness = Harness::new();
    let (mut event_loop, tx) = harness_loop();

    tx.send(workspaces(1)).expect("loop is alive");
    tx.send(workspaces(1)).expect("loop is alive");
    pump(&mut event_loop, &mut harness, 2);

    assert!(harness.frames.is_empty());
}

#[test]
fn a_repaint_waits_for_the_previous_frame_to_be_shown() {
    let mut harness = Harness::new();
    let (mut event_loop, tx) = harness_loop();
    harness.scheduler.committed(Instant::now());

    tx.send(workspaces(2)).expect("loop is alive");
    pump(&mut event_loop, &mut harness, 1);
    assert!(
        harness.frames.is_empty(),
        "the compositor still owes a frame"
    );

    harness.scheduler.frame_done();
    harness.flush();
    assert_eq!(harness.frames.len(), 1);
}

#[test]
fn a_deferred_repaint_paints_when_the_frame_callback_times_out_without_another_message() {
    let mut harness = Harness::new();
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    harness.scheduler.committed(Instant::now());
    harness.apply(&workspaces(2), "workspaces");
    harness.flush();
    assert!(
        harness.frames.is_empty(),
        "the compositor still owes a frame"
    );

    // The host arms this deadline; nothing else is allowed to wake the loop.
    let deadline = harness
        .scheduler
        .wake_deadline()
        .expect("a blocked repaint must schedule a wake");
    event_loop
        .handle()
        .insert_source(Timer::from_deadline(deadline), |_, _, h: &mut Harness| {
            h.flush();
            TimeoutAction::Drop
        })
        .expect("timer registers");

    event_loop
        .dispatch(
            FRAME_CALLBACK_TIMEOUT + Duration::from_millis(200),
            &mut harness,
        )
        .expect("dispatch");
    assert_eq!(
        harness.frames.len(),
        1,
        "the timeout woke the workspace repaint with no further message"
    );
    assert_eq!(harness.frames[0].0, "workspaces");
}
