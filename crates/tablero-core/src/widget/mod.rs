//! The widget/message architecture: typed messages drive widget state, widgets
//! report whether their visible state changed, and the [`Dashboard`] turns those
//! reports into a dirty-flag redraw decision.
//!
//! ```text
//!   data source ──Msg──▶ Dashboard::update ──bool──▶ redraw? ──▶ Dashboard::draw
//! ```
//!
//! A widget never renders itself unprompted: the host loop builds a [`Msg`],
//! applies it, and only repaints when [`Dashboard::update`] reports a change (or
//! the Wayland lifecycle forces a frame). See [`Widget`] for the per-widget
//! contract and [`clock::ClockWidget`] for the reference implementation.

use chrono::{DateTime, Local};

use crate::render::{Bounds, RenderContext};

pub mod clock;

pub use clock::ClockWidget;

/// Every state update a widget can react to.
///
/// One typed model carries all inputs that can change what is on screen, so new
/// widgets and data sources plug in by adding variants rather than by rewriting
/// the host loop. Marked `#[non_exhaustive]`: downstream code must keep a
/// catch-all arm so new variants are non-breaking.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Msg {
    /// The wall clock advanced; carries the current local time.
    Tick(DateTime<Local>),
}

impl Msg {
    /// Build a [`Msg::Tick`] sampling the current local time.
    ///
    /// Keeps the wall-clock read on the message-producer side so widgets stay
    /// pure functions of the messages they receive.
    pub fn tick_now() -> Self {
        Msg::Tick(Local::now())
    }
}

/// A drawable, message-driven component of the bar.
///
/// The contract is deliberately small but complete for an event loop that only
/// repaints on change:
///
/// * [`update`](Widget::update) folds a [`Msg`] into the widget's state and
///   returns whether its *visible* state changed — the redraw signal.
/// * [`draw`](Widget::draw) paints the current state into the shared context;
///   it takes `&self` so drawing never mutates state.
/// * [`bounds`](Widget::bounds) / [`set_bounds`](Widget::set_bounds) expose the
///   layout slot so the host can place the widget and future code can hit-test
///   it.
pub trait Widget {
    /// Apply `msg`; return `true` iff the widget's visible state changed and it
    /// therefore needs to be redrawn.
    fn update(&mut self, msg: &Msg) -> bool;

    /// Draw the current state into `ctx` at the widget's [`bounds`](Widget::bounds).
    fn draw(&self, ctx: &mut RenderContext);

    /// The widget's current layout slot.
    fn bounds(&self) -> Bounds;

    /// Reposition the widget into a new layout slot.
    fn set_bounds(&mut self, bounds: Bounds);
}

/// The set of widgets composing the bar, plus the redraw policy over them.
///
/// [`update`](Dashboard::update) broadcasts a message to every widget and
/// reports whether *any* of them changed — that boolean is the host loop's cue
/// to repaint. [`draw`](Dashboard::draw) clears the background and paints each
/// widget. Layout is intentionally minimal for now (see [`layout`](Dashboard::layout)).
pub struct Dashboard {
    widgets: Vec<Box<dyn Widget>>,
}

impl Dashboard {
    /// Build a dashboard from its widgets, front-to-back in draw order.
    pub fn new(widgets: Vec<Box<dyn Widget>>) -> Self {
        Self { widgets }
    }

    /// Broadcast `msg` to every widget.
    ///
    /// Returns `true` if at least one widget reported a visible change, i.e. the
    /// frame is now dirty and should be redrawn. Every widget is updated even
    /// once a change is seen, so each keeps its state current.
    pub fn update(&mut self, msg: &Msg) -> bool {
        let mut dirty = false;
        for widget in &mut self.widgets {
            dirty |= widget.update(msg);
        }
        dirty
    }

    /// Assign layout slots for a `width * height` surface.
    ///
    /// The v1 bar holds a single widget, so every widget is given the full
    /// surface. This is the seam a real layout engine will replace; the widget
    /// contract ([`Widget::set_bounds`]) already supports it.
    pub fn layout(&mut self, width: u32, height: u32) {
        let full = Bounds::new(0, 0, width, height);
        for widget in &mut self.widgets {
            widget.set_bounds(full);
        }
    }

    /// Clear the background and paint every widget in order.
    pub fn draw(&self, ctx: &mut RenderContext) {
        ctx.fill_background();
        for widget in &self.widgets {
            widget.draw(ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> Msg {
        Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, h, m, s).unwrap())
    }

    #[test]
    fn update_is_dirty_only_when_a_widget_changes() {
        let mut dash = Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 320, 32)))]);

        // First message paints from the empty initial state: dirty.
        assert!(dash.update(&at(12, 0, 0)));
        // Same wall-clock second again: no visible change, not dirty.
        assert!(!dash.update(&at(12, 0, 0)));
        // A new second flips the displayed text: dirty again.
        assert!(dash.update(&at(12, 0, 1)));
    }

    #[test]
    fn empty_dashboard_never_reports_dirty() {
        let mut dash = Dashboard::new(vec![]);
        assert!(!dash.update(&at(12, 0, 0)));
    }

    #[test]
    fn layout_assigns_the_full_surface_to_each_widget() {
        let mut dash = Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1)))]);
        dash.layout(1920, 32);
        // Drawing after layout fills a full-surface context without panicking.
        let mut ctx = RenderContext::new(1920, 32);
        dash.update(&at(12, 0, 0));
        dash.draw(&mut ctx);
        assert_eq!(ctx.pixels().len(), 1920 * 32 * 4);
    }

    #[test]
    fn draw_clears_background_each_frame() {
        let mut dash = Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 320, 32)))]);
        dash.update(&at(12, 0, 0));
        let mut ctx = RenderContext::new(320, 32);
        dash.draw(&mut ctx);
        // Far corner is clear of the left-aligned clock: opaque dark background.
        let px = ctx.pixels();
        let last = &px[px.len() - 4..];
        assert!(last[0] < 0x30 && last[1] < 0x30 && last[2] < 0x30);
        assert_eq!(last[3], 0xFF);
    }
}
