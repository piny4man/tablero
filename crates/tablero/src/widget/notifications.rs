//! The notifications widget and its normalized swaync model.
//!
//! [`Notifications`] is the typed, normalized snapshot a producer feeds in
//! through [`Msg::Notifications`]: how many
//! notifications are waiting and whether Do-Not-Disturb is on.
//! [`NotificationsWidget`] renders it as a bell glyph — slashed and dimmed
//! under DND, with a small attention-colored dot in the pill's upper-right
//! corner while any notifications are pending.
//!
//! The snapshot is an [`Option`] at the message level: `None` means the
//! notification daemon (swaync) is not on the session bus, so the widget
//! measures zero and reserves no slot — matching how the `volume` and
//! `network` widgets treat an absent source. Clicks are typed intents rather
//! than spawned programs: the primary button asks the host to toggle the
//! swaync control-center panel, the secondary button to toggle DND, and the
//! producer that owns the swaync connection executes both.

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    ClickButton, Command, Msg, ResolvedIcon, Widget, WidgetStyle, draw_icon_content,
    measure_icon_content,
};

/// How far each color channel is scaled toward black for the DND look, percent.
const DIM_PERCENT: u32 = 55;

/// A normalized snapshot of the notification daemon's visible state: how many
/// notifications are waiting and whether Do-Not-Disturb is active.
///
/// Equality is a faithful "does this look different on screen?" test — the
/// producer drops the daemon fields the widget does not render (panel
/// visibility, inhibitors) before building the snapshot, so repeated readings
/// that only differ there never force a repaint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notifications {
    count: u32,
    dnd: bool,
}

impl Notifications {
    /// Build a snapshot from the pending-notification `count` and the current
    /// Do-Not-Disturb state.
    pub fn new(count: u32, dnd: bool) -> Self {
        Self { count, dnd }
    }

    /// How many notifications are waiting in the daemon's queue.
    pub fn count(self) -> u32 {
        self.count
    }

    /// Whether Do-Not-Disturb is on.
    pub fn dnd(self) -> bool {
        self.dnd
    }
}

/// Scale a color's RGB channels to [`DIM_PERCENT`] of their value, keeping
/// alpha — the muted look the bell takes under Do-Not-Disturb.
///
/// Kept a pure function so the exact channel math is unit-testable without
/// painting pixels.
fn dim((r, g, b, a): (u8, u8, u8, u8)) -> (u8, u8, u8, u8) {
    let scale = |c: u8| ((c as u32 * DIM_PERCENT) / 100) as u8;
    (scale(r), scale(g), scale(b), a)
}

/// A bar widget showing the notification daemon's state as a bell.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can
/// report a visible change only when the snapshot actually differs. `None` is
/// the pre-first-reading / daemon-absent state: the widget measures zero,
/// reserves no slot, and ignores clicks. Its resolved [`WidgetStyle`] decides
/// the glyph, the optional pill, and the colors; the pending-notification dot
/// uses the style's `attention` colors, so `[widget.notifications.attention]`
/// re-colors it from config.
pub struct NotificationsWidget {
    bounds: Bounds,
    state: Option<Notifications>,
    style: WidgetStyle,
}

impl NotificationsWidget {
    /// Create a notifications widget occupying `bounds`, empty until its first
    /// [`Msg::Notifications`] and carrying the
    /// default (flat, glyph-on) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](NotificationsWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// The template: a lone `{icon}` slot while the daemon is present, or empty
    /// while it is absent so the widget reserves no slot. The count is shown by
    /// the dot, never as text — the widget stays one icon wide.
    fn template(&self) -> String {
        match &self.state {
            Some(_) => "{icon}".to_string(),
            None => String::new(),
        }
    }

    /// The bell icon: slashed under Do-Not-Disturb, upright otherwise — resolved
    /// against its state-appropriate built-in so a custom `icon`/`icon = "none"`
    /// still overrides it.
    fn icon(&self) -> ResolvedIcon {
        let default = match self.state {
            Some(n) if n.dnd => BuiltinIcon::NotificationsOff,
            _ => BuiltinIcon::Notifications,
        };
        self.style.resolve_icon(default)
    }

    /// The foreground the bell is drawn in: the style's normal foreground,
    /// dimmed under Do-Not-Disturb.
    fn foreground(&self) -> (u8, u8, u8, u8) {
        match self.state {
            Some(n) if n.dnd => dim(self.style.foreground),
            _ => self.style.foreground,
        }
    }

    /// The geometry of the pending-notification dot: a small circle tucked
    /// into the pill's upper-right corner, or `None` when nothing is pending
    /// (or the widget has no area to draw in).
    fn dot_bounds(&self, scale: u32) -> Option<Bounds> {
        let n = self.state?;
        if n.count() == 0 || self.bounds.width == 0 || self.bounds.height == 0 {
            return None;
        }
        // A fifth of the pill height reads as a badge without covering the
        // glyph; the floor keeps it visible on small or unscaled bars.
        let d = (self.bounds.height / 5).max(3 * scale);
        let inset = 2 * scale;
        let x = (self.bounds.x + self.bounds.width).saturating_sub(d + inset);
        let y = self.bounds.y + inset;
        Some(Bounds::new(x, y, d, d))
    }
}

impl Widget for NotificationsWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Notifications(next) => {
                if self.state == *next {
                    return false;
                }
                self.state = *next;
                true
            }
            _ => false,
        }
    }

    fn draw(&self, ctx: &mut RenderContext) {
        // An absent daemon leaves the template empty, so the content paints
        // nothing: the dashboard has already cleared the background.
        let colors = super::StateColors {
            background: self.style.background,
            foreground: self.foreground(),
            border: self.style.border,
        };
        draw_icon_content(
            ctx,
            &self.style,
            self.bounds,
            &self.icon(),
            &self.template(),
            colors,
        );

        // The dot is painted over the pill with the opaque attention foreground.
        // Attention backgrounds are commonly translucent state fills and make
        // poor badge colors at this small size.
        if let Some(dot) = self.dot_bounds(ctx.scale_factor()) {
            ctx.fill_rounded_rect(dot, self.style.attention.foreground, dot.width as f32 / 2.0);
        }
    }

    fn measure(&self, ctx: &mut RenderContext, _height: u32) -> u32 {
        measure_icon_content(ctx, &self.style, &self.icon(), &self.template())
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Bounds) {
        self.bounds = bounds;
    }

    fn on_click(&self, px: u32, py: u32, button: ClickButton) -> Option<Command> {
        // Not interactive until the daemon has reported in — there is no
        // panel to toggle when swaync is not running.
        self.state?;
        let b = self.bounds;
        if px < b.x || px >= b.x + b.width || py < b.y || py >= b.y + b.height {
            return None;
        }
        match button {
            ClickButton::Left => Some(Command::ToggleNotificationPanel),
            ClickButton::Right => Some(Command::ToggleNotificationsDnd),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::IconSetting;

    fn some(count: u32, dnd: bool) -> Msg {
        Msg::Notifications(Some(Notifications::new(count, dnd)))
    }

    #[test]
    fn first_reading_is_a_visible_change() {
        let mut widget = NotificationsWidget::new(Bounds::new(0, 0, 100, 32));
        assert!(widget.update(&some(0, false)));
    }

    #[test]
    fn identical_reading_is_not_a_redraw() {
        let mut widget = NotificationsWidget::new(Bounds::new(0, 0, 100, 32));
        widget.update(&some(2, false));
        assert!(!widget.update(&some(2, false)));
    }

    #[test]
    fn count_and_dnd_changes_are_visible_changes() {
        let mut widget = NotificationsWidget::new(Bounds::new(0, 0, 100, 32));
        widget.update(&some(0, false));
        // A first pending notification lights the dot.
        assert!(widget.update(&some(1, false)));
        // Toggling DND swaps the glyph.
        assert!(widget.update(&some(1, true)));
        // The daemon going away empties the slot.
        assert!(widget.update(&Msg::Notifications(None)));
        // And coming back fills it again.
        assert!(widget.update(&some(0, false)));
    }

    #[test]
    fn unrelated_messages_are_ignored() {
        let mut widget = NotificationsWidget::new(Bounds::new(0, 0, 100, 32));
        widget.update(&some(1, false));
        assert!(!widget.update(&Msg::tick_now()));
    }

    #[test]
    fn a_present_daemon_shows_the_upright_bell_icon() {
        let mut widget = NotificationsWidget::new(Bounds::new(0, 0, 100, 32));
        widget.update(&some(3, false));
        assert_eq!(widget.template(), "{icon}");
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::Notifications)
        );
    }

    #[test]
    fn dnd_swaps_to_the_slashed_bell_and_dims_the_foreground() {
        let mut widget = NotificationsWidget::new(Bounds::new(0, 0, 100, 32));
        widget.update(&some(0, true));
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::NotificationsOff)
        );
        assert_eq!(widget.foreground(), dim(widget.style.foreground));
    }

    #[test]
    fn a_disabled_icon_leaves_nothing_to_show() {
        let mut widget =
            NotificationsWidget::new(Bounds::new(0, 0, 100, 32)).with_style(WidgetStyle {
                icon: IconSetting::None,
                ..WidgetStyle::default()
            });
        widget.update(&some(1, false));
        // The template still reserves an icon slot, but the resolved icon opts
        // out, so the content — measured and drawn — collapses to nothing.
        assert_eq!(widget.icon(), ResolvedIcon::None);
        let mut ctx = RenderContext::new(200, 32);
        assert_eq!(widget.measure(&mut ctx, 32), 0);
    }

    #[test]
    fn dim_scales_rgb_and_keeps_alpha() {
        assert_eq!(dim((200, 100, 0, 0xFF)), (110, 55, 0, 0xFF));
        assert_eq!(dim((0, 0, 0, 0x80)), (0, 0, 0, 0x80));
    }

    #[test]
    fn the_widget_reserves_no_slot_before_any_reading() {
        let widget = NotificationsWidget::new(Bounds::new(0, 0, 100, 32));
        let mut ctx = RenderContext::new(200, 32);
        assert_eq!(widget.measure(&mut ctx, 32), 0);
    }

    #[test]
    fn a_seeded_widget_measures_its_pill() {
        let mut widget = NotificationsWidget::new(Bounds::new(0, 0, 100, 32));
        widget.update(&some(0, false));
        let mut ctx = RenderContext::new(200, 32);
        assert!(widget.measure(&mut ctx, 32) > 0);
    }

    #[test]
    fn the_dot_appears_only_while_notifications_are_pending() {
        let mut widget = NotificationsWidget::new(Bounds::new(0, 0, 40, 32));
        widget.update(&some(0, false));
        assert_eq!(widget.dot_bounds(1), None);
        widget.update(&some(1, false));
        let dot = widget.dot_bounds(1).expect("a pending count draws a dot");
        // A 32px pill yields a 32/5 = 6px dot inset 2px from the top-right
        // corner: x = 40 - (6 + 2) = 32, y = 2.
        assert_eq!(dot, Bounds::new(32, 2, 6, 6));
    }

    #[test]
    fn drawing_paints_the_attention_dot_over_the_pill() {
        let mut style = WidgetStyle::default();
        style.attention.background = Some((200, 0, 0, 16));
        style.attention.foreground = (255, 74, 77, 255);
        let mut widget = NotificationsWidget::new(Bounds::new(0, 0, 40, 32)).with_style(style);
        widget.update(&some(1, false));
        let mut ctx = RenderContext::new(64, 32);
        widget.draw(&mut ctx);
        // Probe the dot's center: 4 bytes per pixel, row-major RGBA. The dot
        // spans x in [32, 38), y in [2, 8) — its center sits at (35, 5).
        let px = ctx.pixels();
        let center = ((5 * 64 + 35) * 4) as usize;
        assert_eq!(&px[center..center + 4], &[255, 74, 77, 255]);
    }

    #[test]
    fn clicks_map_buttons_to_panel_and_dnd_toggles() {
        let mut widget = NotificationsWidget::new(Bounds::new(10, 0, 40, 32));
        widget.update(&some(0, false));
        assert_eq!(
            widget.on_click(20, 16, ClickButton::Left),
            Some(Command::ToggleNotificationPanel)
        );
        assert_eq!(
            widget.on_click(20, 16, ClickButton::Right),
            Some(Command::ToggleNotificationsDnd)
        );
    }

    #[test]
    fn clicks_outside_bounds_or_before_any_reading_are_ignored() {
        let empty = NotificationsWidget::new(Bounds::new(10, 0, 40, 32));
        // No reading yet: not interactive anywhere.
        assert_eq!(empty.on_click(20, 16, ClickButton::Left), None);

        let mut widget = NotificationsWidget::new(Bounds::new(10, 0, 40, 32));
        widget.update(&some(0, false));
        // x=0 is left of bounds.x=10; x=50 is at the right edge (half-open).
        assert_eq!(widget.on_click(0, 16, ClickButton::Left), None);
        assert_eq!(widget.on_click(50, 16, ClickButton::Left), None);
        // y=32 is at the bottom edge (height 32 → y range [0, 32)).
        assert_eq!(widget.on_click(20, 32, ClickButton::Right), None);
    }
}
