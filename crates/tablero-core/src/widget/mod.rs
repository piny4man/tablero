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

pub mod battery;
pub mod clock;
pub mod network;
pub mod system;
pub mod workspaces;

pub use battery::{Battery, BatteryState, BatteryWidget};
pub use clock::ClockWidget;
pub use network::{Network, NetworkState, NetworkWidget};
pub use system::{SystemStats, SystemWidget};
pub use workspaces::{WorkspaceWidget, Workspaces};

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
    /// The Hyprland workspace set or active workspace changed.
    Workspaces(Workspaces),
    /// The battery reading changed; `None` means no battery is present (or the
    /// power daemon is unavailable), so the widget shows nothing.
    Battery(Option<Battery>),
    /// A fresh system-pressure sample: current CPU and memory load.
    System(SystemStats),
    /// The network connectivity changed; `None` means no network is available
    /// (or the network daemon is unreachable), so the widget shows nothing.
    Network(Option<Network>),
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

/// An outbound action a widget requests in response to input.
///
/// Where [`Msg`] flows *in* (data sources update widgets), a `Command` flows
/// *out*: a widget turns a click into an intent, and the host loop hands it to
/// whatever can execute it (for workspace switches, the Hyprland command
/// socket). Keeping the intent typed and execution-free here means click
/// handling stays a pure, testable decision. Marked `#[non_exhaustive]` for the
/// same forward-compatibility reason as [`Msg`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Switch the compositor to the workspace with this id.
    SwitchWorkspace(i32),
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

    /// Handle a click at surface pixel `(px, py)`.
    ///
    /// Returns a [`Command`] when the widget owns an interactive region at that
    /// pixel, or `None` when it does not. The default is non-interactive, so a
    /// widget opts into input only by overriding this — clicks on display-only
    /// widgets (and on empty space) are ignored safely.
    fn on_click(&self, _px: u32, _py: u32) -> Option<Command> {
        None
    }
}

/// The set of widgets composing the bar, plus the redraw policy over them.
///
/// [`update`](Dashboard::update) broadcasts a message to every widget and
/// reports whether *any* of them changed — that boolean is the host loop's cue
/// to repaint. [`draw`](Dashboard::draw) clears the background and paints each
/// widget. Layout is intentionally minimal for now (see [`layout`](Dashboard::layout)).
pub struct Dashboard {
    widgets: Vec<Box<dyn Widget>>,
    /// Horizontal gap between adjacent widget columns, in pixels.
    spacing: u32,
    /// Inner padding inset on each widget column, in pixels.
    padding: u32,
}

impl Dashboard {
    /// Build a dashboard from its widgets, front-to-back in draw order, with no
    /// spacing or padding between/within columns.
    pub fn new(widgets: Vec<Box<dyn Widget>>) -> Self {
        Self {
            widgets,
            spacing: 0,
            padding: 0,
        }
    }

    /// Set the column `spacing` (gap between widgets) and `padding` (inset within
    /// each widget's slot) used by [`layout`](Dashboard::layout), in pixels.
    pub fn with_layout(mut self, spacing: u32, padding: u32) -> Self {
        self.spacing = spacing;
        self.padding = padding;
        self
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
    /// Widgets are placed left-to-right in equal-width columns, in construction
    /// order, so multiple widgets never overlap. A single widget therefore still
    /// gets the full surface. The configured [`spacing`](Dashboard::with_layout)
    /// reserves a horizontal gap between adjacent columns, and `padding` insets
    /// each column on all four sides; both default to zero. This is the seam a
    /// content-aware layout engine will replace; the widget contract
    /// ([`Widget::set_bounds`]) already supports arbitrary slots.
    pub fn layout(&mut self, width: u32, height: u32) {
        let count = self.widgets.len() as u32;
        if count == 0 {
            return;
        }
        // The inter-column gaps come out of the usable width first; the remainder
        // is split into equal columns.
        let gaps = self.spacing.saturating_mul(count - 1);
        let usable = width.saturating_sub(gaps);
        let column = usable / count;
        let pad = self.padding;
        for (i, widget) in self.widgets.iter_mut().enumerate() {
            let slot_x = (column + self.spacing).saturating_mul(i as u32);
            // The last column absorbs any rounding remainder so the row spans the
            // full width; earlier columns are exactly one column wide.
            let slot_w = if i as u32 == count - 1 {
                width.saturating_sub(slot_x)
            } else {
                column
            };
            // Inset the slot by the padding on every side, clamping so a slot
            // narrower than the padding collapses to zero rather than underflowing.
            let inner = Bounds::new(
                slot_x + pad,
                pad,
                slot_w.saturating_sub(pad.saturating_mul(2)),
                height.saturating_sub(pad.saturating_mul(2)),
            );
            widget.set_bounds(inner);
        }
    }

    /// Clear the background and paint every widget in order.
    pub fn draw(&self, ctx: &mut RenderContext) {
        ctx.fill_background();
        for widget in &self.widgets {
            widget.draw(ctx);
        }
    }

    /// Route a click at surface pixel `(px, py)` to the widgets.
    ///
    /// Returns the [`Command`] of the first widget (in draw order) that claims
    /// the pixel, or `None` if none do — clicks on empty space or display-only
    /// widgets produce nothing.
    pub fn on_click(&self, px: u32, py: u32) -> Option<Command> {
        self.widgets
            .iter()
            .find_map(|widget| widget.on_click(px, py))
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
    fn layout_splits_the_surface_into_equal_columns() {
        let mut dash = Dashboard::new(vec![
            Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1))),
            Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1))),
        ]);
        dash.layout(101, 32);
        // First column is floor(101/2); the last absorbs the rounding remainder
        // so the two columns tile the full width without overlap or gaps.
        assert_eq!(dash.widgets[0].bounds(), Bounds::new(0, 0, 50, 32));
        assert_eq!(dash.widgets[1].bounds(), Bounds::new(50, 0, 51, 32));
    }

    #[test]
    fn layout_reserves_spacing_between_columns() {
        let mut dash = Dashboard::new(vec![
            Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1))),
            Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1))),
        ])
        .with_layout(10, 0);
        dash.layout(110, 32);
        // One 10px gap is reserved first; the remaining 100 splits into two 50px
        // columns. The gap sits between them, so column 1 starts at 60.
        assert_eq!(dash.widgets[0].bounds(), Bounds::new(0, 0, 50, 32));
        assert_eq!(dash.widgets[1].bounds(), Bounds::new(60, 0, 50, 32));
    }

    #[test]
    fn layout_insets_each_column_by_padding() {
        let mut dash = Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1)))])
            .with_layout(0, 4);
        dash.layout(100, 32);
        // The single column is the full surface, inset by 4px on every side.
        assert_eq!(dash.widgets[0].bounds(), Bounds::new(4, 4, 92, 24));
    }

    #[test]
    fn on_click_routes_to_the_interactive_widget() {
        // A clock (display-only) beside a workspace widget: a click in the
        // workspace column yields its command, a click in the clock column none.
        let mut workspaces = WorkspaceWidget::new(Bounds::new(0, 0, 1, 1));
        workspaces.update(&Msg::Workspaces(Workspaces::new([1, 2], 1)));
        let mut dash = Dashboard::new(vec![
            Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1))),
            Box::new(workspaces),
        ]);
        dash.layout(200, 32);
        // Clock owns the left column [0,100); workspaces own [100,200).
        assert_eq!(dash.on_click(10, 16), None);
        assert_eq!(dash.on_click(110, 16), Some(Command::SwitchWorkspace(1)));
    }

    #[test]
    fn on_click_on_empty_dashboard_is_none() {
        let dash = Dashboard::new(vec![]);
        assert_eq!(dash.on_click(0, 0), None);
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
