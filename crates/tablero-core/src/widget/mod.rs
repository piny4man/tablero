//! The widget/message architecture: typed messages drive widget state, widgets
//! report whether their visible state changed, and the [`Dashboard`] turns those
//! reports into a dirty-flag redraw decision and a content-aware zone layout.
//!
//! ```text
//!   data source ──Msg──▶ Dashboard::update ──bool──▶ redraw? ──▶ Dashboard::draw
//! ```
//!
//! A widget never renders itself unprompted: the host loop builds a [`Msg`],
//! applies it, and only repaints when [`Dashboard::update`] reports a change (or
//! the Wayland lifecycle forces a frame). Each widget also exposes a
//! [`measure`](Widget::measure) so the dashboard can pack it to exactly its
//! content in one of three zones (left, center, right), and carries a resolved
//! [`WidgetStyle`] describing its pill colors, glyph, and state-driven accents.
//! See [`Widget`] for the per-widget contract and [`clock::ClockWidget`] for the
//! reference implementation.

use std::path::PathBuf;

use chrono::{DateTime, Local};

use crate::render::{Bounds, FG, RenderContext};

pub mod battery;
pub mod bluetooth;
pub mod clock;
pub mod network;
pub mod system;
pub mod title;
pub mod tray;
pub mod workspaces;

pub use battery::{Battery, BatteryState, BatteryWidget};
pub use bluetooth::{Bluetooth, BluetoothState, BluetoothWidget};
pub use clock::ClockWidget;
pub use network::{Network, NetworkState, NetworkWidget};
pub use system::{SystemStats, SystemWidget};
pub use title::{ActiveWindow, TitleWidget};
pub use tray::{TrayIcon, TrayItem, TrayState, TrayStatus, TrayWidget};
pub use workspaces::{WorkspaceWidget, Workspaces};

/// Alert pill background — a muted red, drawn behind a low-battery reading or a
/// tray item requesting attention.
const ALERT_BACKGROUND: (u8, u8, u8, u8) = (0xBF, 0x61, 0x6A, 0xFF);
/// Alert pill foreground — white, for contrast over [`ALERT_BACKGROUND`].
const ALERT_FOREGROUND: (u8, u8, u8, u8) = (0xFF, 0xFF, 0xFF, 0xFF);
/// Default pill corner radius, in logical pixels (scaled at render time).
const DEFAULT_RADIUS: u32 = 8;
/// Default inset between a pill edge and its text, in logical pixels.
const DEFAULT_PADDING: u32 = 8;
/// Default percent below which a discharging battery shows its warn colors.
const DEFAULT_WARN_THRESHOLD: u32 = 20;

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
    /// The local Bluetooth adapter state and connected-device count.
    ///
    /// Unlike the battery/network messages, the snapshot is **not** wrapped in
    /// an `Option`: the bluetooth widget always reserves a slot and falls
    /// back to showing `unavailable` before its first reading or when no
    /// adapter is present, so there is no value the producer could send that
    /// means "show nothing". The widget starts in
    /// [`Unavailable`](BluetoothState::Unavailable) and stays there until the
    /// producer hands it a reading.
    Bluetooth(Bluetooth),
    /// The set of registered StatusNotifierItem tray items changed; carries the
    /// normalized snapshot of what the tray should now show.
    Tray(TrayState),
    /// The active window on a specific Hyprland monitor changed.
    ///
    /// Each output's bar carries its own `TitleWidget` bound to a specific
    /// monitor name; the widget ignores messages for any other monitor so
    /// focus changes on monitor A update only monitor A's bar. `window` is
    /// `None` when no window on that monitor is focused (empty desktop or
    /// compositor has no active window on the named monitor), in which case
    /// the widget reserves no slot.
    ActiveWindow {
        /// The Hyprland connector name (e.g. `"DP-1"`) of the output whose
        /// bar should update.
        monitor: String,
        /// The current active window on that monitor.
        window: Option<ActiveWindow>,
    },
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
    /// Activate the tray item registered under this address (an SNI
    /// `Activate` request — primary click).
    ActivateTrayItem(String),
    /// Spawn the executable at this path as a child process directly (no
    /// shell), used to wire a per-widget `on-click` action (e.g. a Bluetooth
    /// manager launcher). The host executor resolves a leading `~` to the
    /// user's home before spawning.
    RunProgram(PathBuf),
}

/// How a widget chooses the glyph it draws before its label.
///
/// Resolved from the `icon` config field: an absent value keeps the widget's
/// built-in [`Default`](IconSetting::Default) glyph, `"none"` disables it, and
/// any other string overrides it with that exact glyph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IconSetting {
    /// Use the widget's built-in default glyph.
    #[default]
    Default,
    /// Draw no glyph at all.
    None,
    /// Use this exact string as the glyph.
    Custom(String),
}

/// A resolved pair of pill colors for one widget state (normal, warn, attention).
///
/// `background` is `None` when that state draws no pill — the text floats
/// directly on the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateColors {
    /// Pill fill behind the content; `None` draws no pill.
    pub background: Option<(u8, u8, u8, u8)>,
    /// Text/glyph color.
    pub foreground: (u8, u8, u8, u8),
}

/// The fully-resolved visual style for one widget.
///
/// Built from a `[widget.<name>]` config table folded onto the theme (see
/// `config::WidgetStyleConfig::resolve`): colors and geometry are concrete, and
/// the alert states ([`warn`](WidgetStyle::warn) / [`attention`](WidgetStyle::attention))
/// carry the colors a widget swaps to when its state crosses a threshold. A
/// `None` [`background`](WidgetStyle::background) means the normal state draws no
/// pill, which is what keeps the default bar visually flat.
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetStyle {
    /// Normal-state pill fill; `None` draws no pill.
    pub background: Option<(u8, u8, u8, u8)>,
    /// Normal-state text/glyph color.
    pub foreground: (u8, u8, u8, u8),
    /// Emphasis color (e.g. the active workspace pill).
    pub accent: (u8, u8, u8, u8),
    /// Pill corner radius, logical pixels.
    pub radius: u32,
    /// Inset between a pill edge and its text, logical pixels.
    pub padding: u32,
    /// Which glyph to draw before the label.
    pub icon: IconSetting,
    /// Percent below which a discharging battery uses [`warn`](WidgetStyle::warn).
    pub warn_threshold: u32,
    /// Colors for a low/warning state (e.g. low battery).
    pub warn: StateColors,
    /// Colors for an attention state (e.g. a tray item needing attention).
    pub attention: StateColors,
}

impl Default for WidgetStyle {
    fn default() -> Self {
        Self {
            background: None,
            foreground: FG,
            accent: FG,
            radius: DEFAULT_RADIUS,
            padding: DEFAULT_PADDING,
            icon: IconSetting::Default,
            warn_threshold: DEFAULT_WARN_THRESHOLD,
            warn: StateColors {
                background: Some(ALERT_BACKGROUND),
                foreground: ALERT_FOREGROUND,
            },
            attention: StateColors {
                background: Some(ALERT_BACKGROUND),
                foreground: ALERT_FOREGROUND,
            },
        }
    }
}

impl WidgetStyle {
    /// The glyph to draw: the widget's `default` when the icon is
    /// [`IconSetting::Default`], nothing for [`IconSetting::None`], or the exact
    /// configured string for [`IconSetting::Custom`].
    pub fn glyph<'a>(&'a self, default: &'a str) -> &'a str {
        match &self.icon {
            IconSetting::Default => default,
            IconSetting::None => "",
            IconSetting::Custom(s) => s,
        }
    }

    /// The normal-state pill colors (this style's own background and foreground),
    /// as a [`StateColors`] for [`draw_text_pill`].
    pub fn base_colors(&self) -> StateColors {
        StateColors {
            background: self.background,
            foreground: self.foreground,
        }
    }
}

/// Join a glyph and a label with a single space, dropping either when empty.
///
/// So `("\u{f017}", "08:09")` → `"\u{f017} 08:09"`, `("", "08:09")` → `"08:09"`
/// when the icon is disabled, and an empty label yields just the glyph.
pub(crate) fn glyph_label(glyph: &str, label: &str) -> String {
    match (glyph.is_empty(), label.is_empty()) {
        (true, _) => label.to_string(),
        (false, true) => glyph.to_string(),
        (false, false) => format!("{glyph} {label}"),
    }
}

/// The pill width `text` needs under `style`: its shaped width plus the style's
/// padding on both sides (scaled to physical pixels). Empty text needs no pill,
/// so it measures zero and the widget reserves no slot.
pub(crate) fn measure_text_pill(ctx: &mut RenderContext, style: &WidgetStyle, text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let pad = style.padding * ctx.scale_factor();
    ctx.measure_text(text) + 2 * pad
}

/// Paint a text pill: the rounded background (when `colors.background` is set),
/// then `text` in `colors.foreground`, inset horizontally from the pill edges by
/// the style's padding. A no-op for empty text or a zero-area slot, so a widget
/// with no background and no content paints nothing.
pub(crate) fn draw_text_pill(
    ctx: &mut RenderContext,
    style: &WidgetStyle,
    bounds: Bounds,
    text: &str,
    colors: StateColors,
) {
    if text.is_empty() || bounds.width == 0 || bounds.height == 0 {
        return;
    }
    let scale = ctx.scale_factor();
    if let Some(bg) = colors.background {
        ctx.fill_rounded_rect(bounds, bg, (style.radius * scale) as f32);
    }
    let pad = style.padding * scale;
    let inner = Bounds::new(
        bounds.x + pad,
        bounds.y,
        bounds.width.saturating_sub(2 * pad),
        bounds.height,
    );
    ctx.draw_text(text, inner, colors.foreground);
}

/// Draw `text` horizontally centered within `bounds` (vertical centering is
/// [`RenderContext::draw_text`]'s own), clamped so it never spills past the
/// slot's right edge. The per-item widgets (workspaces, tray) use this to set a
/// short id or glyph in the middle of a square cell rather than hard against its
/// left edge. A no-op for empty text or a zero-area slot.
pub(crate) fn draw_centered(
    ctx: &mut RenderContext,
    text: &str,
    bounds: Bounds,
    color: (u8, u8, u8, u8),
) {
    if text.is_empty() || bounds.width == 0 || bounds.height == 0 {
        return;
    }
    // Center by insetting the left edge by half the slack; the width shrinks by
    // the same inset so the text box's right edge stays inside the cell (and
    // over-wide text simply left-aligns and clips, never bleeding into a
    // neighbor).
    let pad = bounds.width.saturating_sub(ctx.measure_text(text)) / 2;
    let inner = Bounds::new(
        bounds.x + pad,
        bounds.y,
        bounds.width.saturating_sub(pad),
        bounds.height,
    );
    ctx.draw_text(text, inner, color);
}

/// A drawable, message-driven component of the bar.
///
/// The contract is deliberately small but complete for an event loop that only
/// repaints on change and lays widgets out by their content:
///
/// * [`update`](Widget::update) folds a [`Msg`] into the widget's state and
///   returns whether its *visible* state changed — the redraw signal.
/// * [`draw`](Widget::draw) paints the current state into the shared context;
///   it takes `&self` so drawing never mutates state.
/// * [`measure`](Widget::measure) reports the width the widget needs for the
///   current state, so the [`Dashboard`] can pack it to exactly its content.
/// * [`bounds`](Widget::bounds) / [`set_bounds`](Widget::set_bounds) expose the
///   layout slot so the host can place and hit-test the widget.
pub trait Widget {
    /// Apply `msg`; return `true` iff the widget's visible state changed and it
    /// therefore needs to be redrawn.
    fn update(&mut self, msg: &Msg) -> bool;

    /// Draw the current state into `ctx` at the widget's [`bounds`](Widget::bounds).
    fn draw(&self, ctx: &mut RenderContext);

    /// The width in physical pixels this widget needs to render its current
    /// state, given the inner pill `height` it will be laid out at.
    ///
    /// The [`Dashboard`] calls this before [`set_bounds`](Widget::set_bounds) to
    /// size each zone to its content. The default is zero — a widget with nothing
    /// to show reserves no slot — so a widget opts into the layout only by
    /// overriding this once it has content to measure.
    fn measure(&self, _ctx: &mut RenderContext, _height: u32) -> u32 {
        0
    }

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

/// The widgets composing the bar, grouped into left/center/right zones, plus the
/// redraw policy and content-aware layout over them.
///
/// [`update`](Dashboard::update) broadcasts a message to every widget in every
/// zone and reports whether *any* of them changed — that boolean is the host
/// loop's cue to repaint. [`draw`](Dashboard::draw) clears the background and
/// paints each widget. [`layout`](Dashboard::layout) measures each widget and
/// packs the three zones to their content, separated by a per-pill
/// [`gap`](Dashboard::with_spacing) and inset by a bar
/// [`margin`](Dashboard::with_spacing).
pub struct Dashboard {
    left: Vec<Box<dyn Widget>>,
    center: Vec<Box<dyn Widget>>,
    right: Vec<Box<dyn Widget>>,
    /// Inset around the whole pill row, in logical pixels.
    margin: u32,
    /// Gap between adjacent pills within a zone, in logical pixels.
    gap: u32,
}

impl Dashboard {
    /// Build a dashboard whose widgets all live in the left zone, with no margin
    /// or gap. This is the single-zone constructor the redraw harnesses and the
    /// host's simplest path use; richer layouts use [`with_zones`](Dashboard::with_zones).
    pub fn new(widgets: Vec<Box<dyn Widget>>) -> Self {
        Self {
            left: widgets,
            center: Vec::new(),
            right: Vec::new(),
            margin: 0,
            gap: 0,
        }
    }

    /// Build a dashboard from its three zones, with no margin or gap.
    pub fn with_zones(
        left: Vec<Box<dyn Widget>>,
        center: Vec<Box<dyn Widget>>,
        right: Vec<Box<dyn Widget>>,
    ) -> Self {
        Self {
            left,
            center,
            right,
            margin: 0,
            gap: 0,
        }
    }

    /// Set the bar `margin` (inset around the pill row) and the `gap` (between
    /// adjacent pills within a zone), in logical pixels, used by
    /// [`layout`](Dashboard::layout). Both default to zero.
    pub fn with_spacing(mut self, margin: u32, gap: u32) -> Self {
        self.margin = margin;
        self.gap = gap;
        self
    }

    /// Broadcast `msg` to every widget in every zone.
    ///
    /// Returns `true` if at least one widget reported a visible change, i.e. the
    /// frame is now dirty and should be redrawn. Every widget is updated even
    /// once a change is seen, so each keeps its state current.
    pub fn update(&mut self, msg: &Msg) -> bool {
        let mut dirty = false;
        for widget in self
            .left
            .iter_mut()
            .chain(&mut self.center)
            .chain(&mut self.right)
        {
            dirty |= widget.update(msg);
        }
        dirty
    }

    /// Assign layout slots for a `width * height` surface.
    ///
    /// Each widget is measured against the inner (post-margin) height, then the
    /// three zones are packed: the left zone from the left margin rightward, the
    /// right zone flush against the far margin, and the center zone centered in
    /// the leftover middle — yielding to (and clipping against) the edges when
    /// the middle is too tight. Pills within a zone are separated by the
    /// [`gap`](Dashboard::with_spacing); the whole row is inset by the
    /// [`margin`](Dashboard::with_spacing). A widget that measures zero is parked
    /// at zero width and reserves no space. Geometry is authored in logical
    /// pixels and scaled here by the context's output scale.
    pub fn layout(&mut self, ctx: &mut RenderContext, width: u32, height: u32) {
        let scale = ctx.scale_factor();
        let margin = self.margin * scale;
        let gap = self.gap * scale;
        let top = margin;
        let inner_h = height.saturating_sub(2 * margin);

        // Pass 1: measure every widget against the inner height. The owned width
        // vectors release the immutable borrow of `self` before placement takes a
        // mutable one.
        let left_w = measure_zone(&self.left, ctx, inner_h);
        let center_w = measure_zone(&self.center, ctx, inner_h);
        let right_w = measure_zone(&self.right, ctx, inner_h);

        let left_total = cluster_width(&left_w, gap);
        let center_total = cluster_width(&center_w, gap);
        let right_total = cluster_width(&right_w, gap);

        // Pass 2: place each zone. Left from the margin, right flush against the
        // far margin.
        let left_start = margin;
        place_cluster(&mut self.left, &left_w, left_start, gap, top, inner_h);

        let right_edge = width.saturating_sub(margin);
        let right_start = right_edge.saturating_sub(right_total);
        place_cluster(&mut self.right, &right_w, right_start, gap, top, inner_h);

        // The middle region the center may occupy, kept clear of both clusters
        // and their adjoining gaps.
        let lo = if left_total > 0 {
            left_start + left_total + gap
        } else {
            left_start
        };
        let hi = if right_total > 0 {
            right_start.saturating_sub(gap)
        } else {
            right_edge
        };

        if center_total > 0 && hi.saturating_sub(lo) >= center_total {
            let ideal = width.saturating_sub(center_total) / 2;
            let max_start = hi - center_total;
            let start = ideal.clamp(lo, max_start);
            place_cluster(&mut self.center, &center_w, start, gap, top, inner_h);
        } else {
            // No room (or nothing to show): the center yields — every center
            // widget collapses to zero width.
            let zeros = vec![0u32; center_w.len()];
            place_cluster(&mut self.center, &zeros, lo, gap, top, inner_h);
        }
    }

    /// Clear the background and paint every widget, left zone then center then
    /// right.
    pub fn draw(&self, ctx: &mut RenderContext) {
        ctx.fill_background();
        for widget in self.left.iter().chain(&self.center).chain(&self.right) {
            widget.draw(ctx);
        }
    }

    /// Route a click at surface pixel `(px, py)` to the widgets.
    ///
    /// Returns the [`Command`] of the first widget (in zone then draw order) that
    /// claims the pixel, or `None` if none do — clicks on empty space or
    /// display-only widgets produce nothing. Zones never overlap after
    /// [`layout`](Dashboard::layout), so order only breaks ties on empty space.
    pub fn on_click(&self, px: u32, py: u32) -> Option<Command> {
        self.left
            .iter()
            .chain(&self.center)
            .chain(&self.right)
            .find_map(|widget| widget.on_click(px, py))
    }
}

/// Measure every widget in `widgets` against the inner pill `height`, returning a
/// width per widget (parallel to the slice). A width of zero means the widget has
/// nothing to show and should reserve no slot.
fn measure_zone(widgets: &[Box<dyn Widget>], ctx: &mut RenderContext, height: u32) -> Vec<u32> {
    let mut widths = Vec::with_capacity(widgets.len());
    for widget in widgets {
        widths.push(widget.measure(ctx, height));
    }
    widths
}

/// The total width a zone occupies: the sum of its pill widths plus one `gap`
/// between each adjacent non-empty pair. Zero-width widgets reserve nothing and
/// introduce no gap.
fn cluster_width(widths: &[u32], gap: u32) -> u32 {
    let count = widths.iter().filter(|&&w| w > 0).count() as u32;
    let sum: u32 = widths.iter().sum();
    sum + gap * count.saturating_sub(1)
}

/// Assign bounds to a zone's widgets, packed left to right from `x`, each pill
/// `height` tall at `top`, separated by `gap`. A zero-width widget is parked at
/// the cursor with zero width (it draws nothing) and neither advances the cursor
/// nor adds a gap.
fn place_cluster(
    widgets: &mut [Box<dyn Widget>],
    widths: &[u32],
    mut x: u32,
    gap: u32,
    top: u32,
    height: u32,
) {
    let mut placed = false;
    for (widget, &w) in widgets.iter_mut().zip(widths) {
        if w == 0 {
            widget.set_bounds(Bounds::new(x, top, 0, height));
            continue;
        }
        if placed {
            x += gap;
        }
        widget.set_bounds(Bounds::new(x, top, w, height));
        x += w;
        placed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> Msg {
        Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, h, m, s).unwrap())
    }

    /// A workspace widget seeded with a single id, so it deterministically
    /// measures one square cell as wide as the laid-out row is tall, regardless
    /// of the font.
    fn one_workspace(id: i32) -> Box<dyn Widget> {
        let mut ws = WorkspaceWidget::new(Bounds::new(0, 0, 1, 1));
        ws.update(&Msg::Workspaces(Workspaces::new([id], id)));
        Box::new(ws)
    }

    #[test]
    fn update_is_dirty_only_when_a_widget_changes() {
        let mut dash = Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 320, 32)))]);

        // First message paints from the empty initial state: dirty.
        assert!(dash.update(&at(12, 0, 0)));
        // A later second in the same minute: no visible change, not dirty.
        assert!(!dash.update(&at(12, 0, 30)));
        // A new minute flips the displayed text: dirty again.
        assert!(dash.update(&at(12, 1, 0)));
    }

    #[test]
    fn empty_dashboard_never_reports_dirty() {
        let mut dash = Dashboard::new(vec![]);
        assert!(!dash.update(&at(12, 0, 0)));
    }

    #[test]
    fn layout_packs_left_and_right_zones_to_their_edges() {
        let mut dash =
            Dashboard::with_zones(vec![one_workspace(1)], vec![], vec![one_workspace(2)]);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // The left workspace packs from the origin; the right one packs flush
        // against the far edge (200 - 32 = 168), each a 32px square content cell.
        assert_eq!(dash.left[0].bounds(), Bounds::new(0, 0, 32, 32));
        assert_eq!(dash.right[0].bounds(), Bounds::new(168, 0, 32, 32));
    }

    #[test]
    fn layout_centers_the_center_zone_between_the_edges() {
        let mut dash = Dashboard::with_zones(vec![], vec![one_workspace(1)], vec![]);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // A lone 32px center cell centers on the surface: (200 - 32) / 2 = 84.
        assert_eq!(dash.center[0].bounds(), Bounds::new(84, 0, 32, 32));
    }

    #[test]
    fn layout_reserves_the_gap_between_pills_in_a_zone() {
        let mut dash =
            Dashboard::with_zones(vec![one_workspace(1), one_workspace(2)], vec![], vec![])
                .with_spacing(0, 10);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // First pill at the origin; the second starts one 10px gap after it
        // (32 + 10 = 42).
        assert_eq!(dash.left[0].bounds(), Bounds::new(0, 0, 32, 32));
        assert_eq!(dash.left[1].bounds(), Bounds::new(42, 0, 32, 32));
    }

    #[test]
    fn layout_insets_the_row_by_the_bar_margin() {
        let mut dash =
            Dashboard::with_zones(vec![one_workspace(1)], vec![], vec![]).with_spacing(4, 0);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // Margin 4 pushes the pill in by 4 and shrinks the row height by 8; the
        // square cell is then as wide as that inner height (24).
        assert_eq!(dash.left[0].bounds(), Bounds::new(4, 4, 24, 24));
    }

    #[test]
    fn an_unmeasured_widget_reserves_no_slot() {
        // A clock with no time yet measures zero, so it is parked at zero width
        // and the workspace after it still starts at the origin.
        let clock = ClockWidget::new(Bounds::new(0, 0, 1, 1));
        let mut dash =
            Dashboard::with_zones(vec![Box::new(clock), one_workspace(1)], vec![], vec![]);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        assert_eq!(dash.left[0].bounds().width, 0);
        assert_eq!(dash.left[1].bounds(), Bounds::new(0, 0, 32, 32));
    }

    #[test]
    fn on_click_routes_to_the_zone_that_owns_the_pixel() {
        // A display-only clock in the center, an interactive workspace on the
        // right edge: a click on the right switches, a click mid-surface (where
        // the empty clock holds nothing) commands nothing.
        let mut dash = Dashboard::with_zones(
            vec![],
            vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1)))],
            vec![one_workspace(1)],
        );
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // The right workspace cell owns [168, 200).
        assert_eq!(dash.on_click(180, 16), Some(Command::SwitchWorkspace(1)));
        assert_eq!(dash.on_click(100, 16), None);
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
