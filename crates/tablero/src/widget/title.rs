//! The title widget: the Hyprland-focused window's title.
//!
//! [`ActiveWindow`] is the typed, normalized snapshot a producer feeds in
//! through [`Msg::ActiveWindow`]; [`TitleWidget`]
//! renders it, repainting only when the *displayed* text actually changes.
//!
//! The widget is display-only (no clicks) and shows nothing — exactly like
//! before its first reading — when no window is focused. Long titles are
//! truncated with a horizontal ellipsis (`…`) at
//! `DEFAULT_MAX_LENGTH` chars by default; `0` disables
//! truncation. Truncation is *char*-based so a multi-byte title is never cut
//! mid-codepoint, and the visible-change policy is *post*-truncation, so two
//! different raw titles that truncate to the same display report no repaint.
//!
//! App icons (class → themed PNG → bitmap blit) are a natural follow-up;
//! the icon-theme resolver already exists for the tray widget. The widget
//! currently exposes no glyph by default (`icon = "none"`); users can set a
//! glyph via `[widget.title] icon`.

use crate::render::{Bounds, RenderContext};

use super::{Msg, Widget, WidgetStyle, draw_text_pill, glyph_label, measure_text_pill};

/// Default truncation cap, in characters. Titles longer than this are
/// shortened to `cap - 1` chars followed by `…`. Override via
/// [`TitleWidget::with_max_length`]; `0` disables truncation.
const DEFAULT_MAX_LENGTH: u32 = 100;

/// The ellipsis appended to a truncated title.
const TRUNCATE_SUFFIX: char = '\u{2026}';

/// A normalized snapshot of the focused Hyprland window.
///
/// `class` and `title` are trimmed at construction, so a fully-empty
/// snapshot (e.g. a reading where both fields were blank or whitespace-only)
/// is [`is_empty`](ActiveWindow::is_empty) and the widget reserves no slot
/// for it. Equality compares the trimmed fields exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWindow {
    class: String,
    title: String,
}

impl ActiveWindow {
    /// Build a normalized snapshot from the raw `class` and `title` strings
    /// Hyprland's `j/activewindow` reports. Both fields are trimmed; the
    /// resulting snapshot is treated as "no window" by [`TitleWidget`] when
    /// both end up empty.
    pub fn new(class: impl Into<String>, title: impl Into<String>) -> Self {
        let class = class.into();
        let title = title.into();
        Self {
            class: class.trim().to_string(),
            title: title.trim().to_string(),
        }
    }

    /// The window class (e.g. `"firefox"`, `"kitty"`); empty when unset.
    pub fn class(&self) -> &str {
        &self.class
    }

    /// The window title (e.g. `"My Document — vim"`); empty when unset.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// True when both `class` and `title` are empty after trimming.
    /// The widget treats such a snapshot as "no window" — same as its
    /// pre-first-reading state — and reserves no slot.
    pub fn is_empty(&self) -> bool {
        self.class.is_empty() && self.title.is_empty()
    }
}

/// A bar widget showing the active window's title on a specific monitor.
///
/// Bound at build time to a Hyprland connector name (`monitor`); the
/// [`update`](Widget::update) path ignores messages addressed to any other
/// monitor, so focus changes on one output redraw only that output's bar.
/// Holds the latest snapshot plus the last *displayed* text — comparing on
/// the truncated text makes the visible-change policy robust against two
/// different raw titles that round to the same on-screen rendering. The
/// snapshot is an [`Option`]: `None` is "no window on this monitor /
/// unavailable", which renders as empty space — identical to the
/// pre-first-reading state, so an absent focused window is shown the same
/// whether it was never there or just went away. Its resolved
/// [`WidgetStyle`] decides the glyph, the optional pill, and the colors it
/// draws with.
///
/// Display is read-only: clicks produce no command. `max_length` caps the
/// drawn title; `0` disables truncation.
pub struct TitleWidget {
    bounds: Bounds,
    /// The Hyprland connector name this widget is bound to (e.g. `"DP-1"`).
    /// Updates addressed to any other monitor are ignored.
    monitor: String,
    /// The latest normalized snapshot for our monitor, retained so a
    /// future class-driven follow-up (themed icons) can read `class`
    /// without a re-query.
    state: Option<ActiveWindow>,
    /// The last *displayed* title (post-truncation, pre-glyph). Used as the
    /// redraw signal: identical to a recomputation means no visible change.
    text: String,
    style: WidgetStyle,
    max_length: u32,
}

impl TitleWidget {
    /// Create a title widget occupying `bounds`, empty until its first
    /// matching [`Msg::ActiveWindow`] and carrying
    /// the default (flat, no glyph) style with `DEFAULT_MAX_LENGTH`
    /// truncation.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            monitor: String::new(),
            state: None,
            text: String::new(),
            style: WidgetStyle::default(),
            max_length: DEFAULT_MAX_LENGTH,
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](TitleWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// Override the truncation cap. `0` disables truncation. Builder entry
    /// point so tests can construct both ends of the spectrum without
    /// threading config yet.
    pub fn with_max_length(mut self, max_length: u32) -> Self {
        self.max_length = max_length;
        self
    }

    /// Bind this widget to the Hyprland output connector it represents
    /// (e.g. `"DP-1"`, `"HDMI-A-1"`). [`update`](Widget::update) drops
    /// messages addressed to any other monitor, keeping the title in sync
    /// with only this output's focus events.
    ///
    /// An empty monitor name is treated as a wildcard: the widget accepts
    /// any [`Msg::ActiveWindow`] regardless of
    /// its `monitor` field. This is the pre-multi-monitor-fallback and is
    /// useful in tests; production always sets a real connector name.
    pub fn with_monitor(mut self, monitor: impl Into<String>) -> Self {
        self.monitor = monitor.into();
        self
    }

    /// The currently displayed title text (post-truncation, pre-glyph), or
    /// empty before the first reading / when no window is focused.
    pub fn title(&self) -> &str {
        &self.text
    }

    /// The full pill text: the configured glyph joined to the displayed
    /// title, or empty before the first reading / when no window is focused
    /// (so the widget reserves no slot).
    fn display_text(&self) -> String {
        if self.text.is_empty() {
            return String::new();
        }
        glyph_label(self.style.glyph(""), &self.text)
    }
}

impl Widget for TitleWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::ActiveWindow { monitor, window } => {
                // Per-monitor routing: drop messages addressed to any other
                // output. The empty-string widget is a wildcard (used in
                // tests and in the pre-multi-monitor fallback path) — it
                // accepts every message.
                if !self.monitor.is_empty() && self.monitor != monitor.as_str() {
                    return false;
                }
                // A trimmed-empty snapshot is "no window on this monitor"
                // and reserves no slot, exactly like the pre-first state.
                let normalized = window.clone().filter(|w| !w.is_empty());
                let next_text = match normalized.as_ref() {
                    Some(w) => truncate_chars(w.title(), self.max_length),
                    None => String::new(),
                };
                if self.text == next_text {
                    return false;
                }
                self.state = normalized;
                self.text = next_text;
                true
            }
            _ => false,
        }
    }

    fn draw(&self, ctx: &mut RenderContext) {
        let text = self.display_text();
        if text.is_empty() {
            return;
        }
        draw_text_pill(
            ctx,
            &self.style,
            self.bounds,
            &text,
            self.style.base_colors(),
        );
    }

    fn measure(&self, ctx: &mut RenderContext, _height: u32) -> u32 {
        measure_text_pill(ctx, &self.style, &self.display_text())
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Bounds) {
        self.bounds = bounds;
    }
}

/// Cap `s` at `max_length` chars; append `…` when the cap is exceeded.
///
/// `max_length == 0` is the explicit "no cap" sentinel, so a long title
/// still renders in full. Char-counting (not byte-counting) keeps a
/// multi-byte title from being cut mid-codepoint.
fn truncate_chars(s: &str, max_length: u32) -> String {
    if max_length == 0 {
        return s.to_string();
    }
    let total = s.chars().count();
    let cap = max_length as usize;
    if total <= cap {
        return s.to_string();
    }
    let keep = cap.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push(TRUNCATE_SUFFIX);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::ClickButton;

    /// A widget pre-bound to `"DP-1"` so we exercise the per-monitor routing.
    fn widget() -> TitleWidget {
        TitleWidget::new(Bounds::new(0, 0, 320, 32)).with_monitor("DP-1")
    }

    fn focus(monitor: &str, class: &str, title: &str) -> Msg {
        Msg::ActiveWindow {
            monitor: monitor.to_string(),
            window: Some(ActiveWindow::new(class, title)),
        }
    }

    fn absent(monitor: &str) -> Msg {
        Msg::ActiveWindow {
            monitor: monitor.to_string(),
            window: None,
        }
    }

    #[test]
    fn active_window_trims_class_and_title() {
        let window = ActiveWindow::new("  firefox  ", "  GitHub  ");
        assert_eq!(window.class(), "firefox");
        assert_eq!(window.title(), "GitHub");
        assert!(!window.is_empty());
    }

    #[test]
    fn active_window_normalizes_whitespace_only_to_empty() {
        let window = ActiveWindow::new("   ", "\t");
        assert_eq!(window.class(), "");
        assert_eq!(window.title(), "");
        assert!(window.is_empty());
    }

    #[test]
    fn first_reading_changes_state_and_sets_title() {
        let mut w = widget();
        assert_eq!(w.title(), "");
        assert!(w.update(&focus("DP-1", "firefox", "GitHub")));
        assert_eq!(w.title(), "GitHub");
    }

    #[test]
    fn identical_reading_is_not_a_visible_change() {
        let mut w = widget();
        assert!(w.update(&focus("DP-1", "firefox", "GitHub")));
        // Same trimmed values from the producer: no redraw.
        assert!(!w.update(&focus("DP-1", "firefox", "GitHub")));
        assert_eq!(w.title(), "GitHub");
    }

    #[test]
    fn trimmed_equivalent_inputs_are_not_a_visible_change() {
        let mut w = widget();
        assert!(w.update(&focus("DP-1", "firefox", "GitHub")));
        // Different raw, identical post-trim → no redraw.
        assert!(!w.update(&focus("DP-1", "  firefox  ", "  GitHub  ")));
        assert_eq!(w.title(), "GitHub");
    }

    #[test]
    fn title_change_is_a_visible_change() {
        let mut w = widget();
        assert!(w.update(&focus("DP-1", "firefox", "GitHub")));
        assert!(w.update(&focus("DP-1", "firefox", "GitLab")));
        assert_eq!(w.title(), "GitLab");
    }

    #[test]
    fn window_going_absent_is_a_visible_change_then_blank() {
        let mut w = widget();
        assert!(w.update(&focus("DP-1", "firefox", "GitHub")));
        // No focused window on this monitor: snapshot collapses to "".
        assert!(w.update(&absent("DP-1")));
        assert_eq!(w.title(), "");
    }

    #[test]
    fn absent_reading_before_any_window_is_not_a_change() {
        let mut w = widget();
        // No focused window matches the empty initial state: no repaint.
        assert!(!w.update(&absent("DP-1")));
        assert_eq!(w.title(), "");
    }

    #[test]
    fn whitespace_only_snapshot_normalizes_to_absent() {
        let mut w = widget();
        assert!(w.update(&focus("DP-1", "firefox", "GitHub")));
        // Whitespace-only fields trim to empty, normalized to "".
        assert!(w.update(&focus("DP-1", "   ", "  ")));
        assert_eq!(w.title(), "");
    }

    #[test]
    fn messages_addressed_to_other_monitors_are_dropped() {
        // Two DP-1 widgets and one HDMI-A-1 widget: the focus-change on
        // HDMI-A-1 only redraws the matching widget.
        let mut dp = widget();
        let mut hdmi = TitleWidget::new(Bounds::new(0, 0, 320, 32)).with_monitor("HDMI-A-1");
        assert!(dp.update(&focus("DP-1", "firefox", "GitHub")));
        // DP-1 already shows "GitHub"; this HDMI event must not change it.
        assert!(hdmi.update(&focus("HDMI-A-1", "kitty", "vim")));
        assert!(!dp.update(&focus("HDMI-A-1", "kitty", "vim")));
        assert_eq!(dp.title(), "GitHub");
        // Now an event addressed to DP-1 (a real change on its monitor).
        assert!(dp.update(&focus("DP-1", "firefox", "GitLab")));
        assert_eq!(dp.title(), "GitLab");
        // The HDMI widget has not seen a DP-1 event: still "vim".
        assert_eq!(hdmi.title(), "vim");
    }

    #[test]
    fn empty_monitor_widget_is_a_wildcard_accepting_every_message() {
        // The legacy / un-bound widget accepts messages regardless of which
        // monitor they're addressed to. Useful as a pre-multi-monitor
        // fallback; also asserted by tests above via the no-monitor
        // construction pattern.
        let mut w = TitleWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(w.update(&focus("DP-1", "firefox", "GitHub")));
        assert!(w.update(&focus("HDMI-A-1", "kitty", "vim")));
        assert_eq!(w.title(), "vim");
    }

    #[test]
    fn max_length_zero_disables_truncation_even_on_a_long_title() {
        let mut w = TitleWidget::new(Bounds::new(0, 0, 320, 32))
            .with_monitor("DP-1")
            .with_max_length(0);
        let long = "x".repeat(5000);
        assert!(w.update(&focus("DP-1", "kitty", &long)));
        assert_eq!(w.title().chars().count(), 5000);
        assert!(!w.update(&focus("DP-1", "kitty", &long)));
    }

    #[test]
    fn max_length_caps_long_title_with_ellipsis() {
        let mut w = TitleWidget::new(Bounds::new(0, 0, 320, 32))
            .with_monitor("DP-1")
            .with_max_length(10);
        let long = "abcdefghijklmno"; // 15 chars
        assert!(w.update(&focus("DP-1", "kitty", long)));
        let displayed = w.title();
        // We kept (10 - 1) = 9 chars + the ellipsis = 10 visible chars.
        assert_eq!(displayed.chars().count(), 10);
        assert!(displayed.ends_with('\u{2026}'));
        assert_eq!(
            &displayed[..displayed.len() - '\u{2026}'.len_utf8()],
            "abcdefghi"
        );
    }

    #[test]
    fn max_length_at_exact_boundary_does_not_truncate() {
        let mut w = TitleWidget::new(Bounds::new(0, 0, 320, 32))
            .with_monitor("DP-1")
            .with_max_length(5);
        assert!(w.update(&focus("DP-1", "kitty", "abcde")));
        assert_eq!(w.title(), "abcde");
    }

    #[test]
    fn max_length_truncation_reports_visible_change_only_when_output_differs() {
        let mut w = TitleWidget::new(Bounds::new(0, 0, 320, 32))
            .with_monitor("DP-1")
            .with_max_length(5);
        // Two different raw titles that truncate to identical "abcd…":
        // no visible change despite different inputs.
        assert!(w.update(&focus("DP-1", "kitty", "abcdefgh")));
        assert!(!w.update(&focus("DP-1", "kitty", "abcdefxyz")));
        assert_eq!(w.title(), "abcd…");
    }

    #[test]
    fn max_length_counts_chars_not_bytes_for_multibyte_titles() {
        let mut w = TitleWidget::new(Bounds::new(0, 0, 320, 32))
            .with_monitor("DP-1")
            .with_max_length(5);
        let title = "日本語のとても長いタイトル";
        assert!(w.update(&focus("DP-1", "kitty", title)));
        assert_eq!(w.title().chars().count(), 5);
        assert_eq!(w.title(), "日本語の…");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut widget = TitleWidget::new(Bounds::new(0, 0, 1, 1));
        widget.set_bounds(Bounds::new(0, 0, 1920, 32));
        assert_eq!(widget.bounds(), Bounds::new(0, 0, 1920, 32));
    }

    #[test]
    fn click_is_a_no_op() {
        let mut widget = TitleWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&focus("DP-1", "firefox", "GitHub")));
        // Display-only widget: no command on click.
        assert_eq!(widget.on_click(16, 16, ClickButton::Left), None);
    }
}
