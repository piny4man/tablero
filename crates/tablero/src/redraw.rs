//! When a bar repaints: at most once per loop dispatch, and at most once per
//! compositor frame.
//!
//! ```text
//!   Msg, Msg, Msg ──request──▶ dirty ──take_due──▶ paint + commit ──committed──▶ awaiting frame
//!                                 ▲                                                   │
//!                                 └──────────────── frame_done ◀──────────────────────┘
//! ```
//!
//! A change only *requests* a repaint. The host loop asks [`take_due`] once per
//! dispatch, after every pending message has been applied, so a burst of
//! messages paints one frame instead of one each. A commit then holds further
//! repaints until the compositor's frame callback, which caps the rate at the
//! output's refresh and stops a hidden bar from painting frames nobody sees.
//!
//! A frame callback is requested only alongside a commit that carries damage,
//! so an idle bar never runs a frame loop.
//!
//! [`take_due`]: RedrawScheduler::take_due

use std::time::{Duration, Instant};

/// How long a requested repaint waits for a frame callback before painting
/// anyway. A compositor sends none while a surface is hidden, so this is what
/// keeps a bar from going stale forever if a callback is never delivered.
pub const FRAME_CALLBACK_TIMEOUT: Duration = Duration::from_secs(1);

/// One surface's pending-repaint state.
#[derive(Debug, Default)]
pub struct RedrawScheduler {
    /// Why the pending repaint was first requested; `None` when clean.
    cause: Option<&'static str>,
    /// When the last commit asked for a frame callback that has yet to arrive.
    awaiting_frame: Option<Instant>,
}

impl RedrawScheduler {
    /// A clean scheduler with no commit outstanding.
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask for a repaint. Requests made before the next [`take_due`] collapse
    /// into one frame, labelled with the first `cause` for diagnostics.
    ///
    /// [`take_due`]: Self::take_due
    pub fn request(&mut self, cause: &'static str) {
        self.cause.get_or_insert(cause);
    }

    /// Whether a repaint has been requested and not yet taken.
    pub fn is_dirty(&self) -> bool {
        self.cause.is_some()
    }

    /// The cause to paint for, if a repaint is requested and the compositor is
    /// ready for another frame. Taking it leaves the scheduler clean.
    ///
    /// While a frame callback is outstanding this returns `None` and the request
    /// stays pending, until [`FRAME_CALLBACK_TIMEOUT`] has passed since the
    /// commit that asked for it.
    pub fn take_due(&mut self, now: Instant) -> Option<&'static str> {
        if self
            .awaiting_frame
            .is_some_and(|since| now.saturating_duration_since(since) < FRAME_CALLBACK_TIMEOUT)
        {
            return None;
        }
        self.cause.take()
    }

    /// A frame was committed at `now` together with a frame-callback request.
    pub fn committed(&mut self, now: Instant) {
        self.awaiting_frame = Some(now);
    }

    /// The compositor's frame callback arrived: the next repaint may go ahead.
    pub fn frame_done(&mut self) {
        self.awaiting_frame = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_scheduler_has_nothing_to_paint() {
        let mut scheduler = RedrawScheduler::new();
        assert!(!scheduler.is_dirty());
        assert_eq!(scheduler.take_due(Instant::now()), None);
    }

    #[test]
    fn requests_before_a_flush_collapse_into_one_frame_with_the_first_cause() {
        let mut scheduler = RedrawScheduler::new();
        scheduler.request("workspaces");
        scheduler.request("active-window");
        scheduler.request("tick");

        let now = Instant::now();
        assert_eq!(scheduler.take_due(now), Some("workspaces"));
        assert_eq!(scheduler.take_due(now), None);
        assert!(!scheduler.is_dirty());
    }

    #[test]
    fn a_request_waits_for_the_frame_callback_of_the_previous_commit() {
        let mut scheduler = RedrawScheduler::new();
        let committed = Instant::now();
        scheduler.committed(committed);
        scheduler.request("tick");

        assert_eq!(scheduler.take_due(committed), None);
        assert!(scheduler.is_dirty(), "the request is kept, not dropped");

        scheduler.frame_done();
        assert_eq!(scheduler.take_due(committed), Some("tick"));
    }

    #[test]
    fn a_frame_callback_that_never_arrives_stops_blocking_after_the_timeout() {
        let mut scheduler = RedrawScheduler::new();
        let committed = Instant::now();
        scheduler.committed(committed);
        scheduler.request("tick");

        let just_before = committed + FRAME_CALLBACK_TIMEOUT - Duration::from_millis(1);
        assert_eq!(scheduler.take_due(just_before), None);
        assert_eq!(
            scheduler.take_due(committed + FRAME_CALLBACK_TIMEOUT),
            Some("tick")
        );
    }

    #[test]
    fn a_frame_callback_with_nothing_requested_paints_nothing() {
        let mut scheduler = RedrawScheduler::new();
        scheduler.committed(Instant::now());
        scheduler.frame_done();
        assert_eq!(scheduler.take_due(Instant::now()), None);
    }
}
