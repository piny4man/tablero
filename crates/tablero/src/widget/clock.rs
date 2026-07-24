//! The clock widget: an `HH:MM` readout driven by [`Msg::Tick`].

use std::path::PathBuf;

use crate::clock::format_time;
use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    ClickButton, Command, Msg, ResolvedIcon, Widget, WidgetStyle, draw_icon_content,
    measure_icon_content,
};

/// A live clock rendered through the widget architecture.
///
/// Holds the currently displayed text so [`update`](Widget::update) can report a
/// visible change only when the formatted minute actually differs — ticks within
/// the same minute format identically and are reported as unchanged, so the host
/// loop stays idle between visible flips. Its resolved [`WidgetStyle`] decides
/// the glyph, the optional pill, and the colors it draws with.
pub struct ClockWidget {
    bounds: Bounds,
    text: String,
    style: WidgetStyle,
    on_click: Option<PathBuf>,
    on_click_right: Option<PathBuf>,
}

impl ClockWidget {
    /// Create a clock occupying `bounds`, with no text until its first tick and
    /// the default (flat, glyph-on) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            text: String::new(),
            style: WidgetStyle::default(),
            on_click: None,
            on_click_right: None,
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](ClockWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// Set the executable run on a primary click.
    pub fn with_on_click(mut self, path: Option<PathBuf>) -> Self {
        self.on_click = path;
        self
    }

    /// Set the executable run on a secondary click.
    pub fn with_on_click_right(mut self, path: Option<PathBuf>) -> Self {
        self.on_click_right = path;
        self
    }

    /// The currently displayed clock text (empty before the first tick).
    pub fn text(&self) -> &str {
        &self.text
    }

    fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.bounds.x
            && px < self.bounds.x + self.bounds.width
            && py >= self.bounds.y
            && py < self.bounds.y + self.bounds.height
    }

    /// The format template — the icon slot marked by `{icon}` ahead of the clock
    /// readout — or empty before the first tick (so the widget reserves no slot).
    fn template(&self) -> String {
        if self.text.is_empty() {
            return String::new();
        }
        format!("{{icon}} {}", self.text)
    }

    /// The clock's icon resolved against its [`Clock`](BuiltinIcon::Clock) default.
    fn icon(&self) -> ResolvedIcon {
        self.style.resolve_icon(BuiltinIcon::Clock)
    }
}

impl Widget for ClockWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Tick(now) => {
                let next = format_time(*now);
                if next == self.text {
                    return false;
                }
                self.text = next;
                true
            }
            _ => false,
        }
    }

    fn draw(&self, ctx: &mut RenderContext) {
        draw_icon_content(
            ctx,
            &self.style,
            self.bounds,
            &self.icon(),
            &self.template(),
            self.style.base_colors(),
        );
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
        if !self.contains(px, py) {
            return None;
        }
        let path = match button {
            ClickButton::Left => self.on_click.as_ref(),
            ClickButton::Right => self.on_click_right.as_ref(),
        }?;
        Some(Command::RunProgram(path.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn tick(h: u32, m: u32, s: u32) -> Msg {
        Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, h, m, s).unwrap())
    }

    #[test]
    fn first_tick_changes_state_and_sets_text() {
        let mut clock = ClockWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(clock.text(), "");
        assert!(clock.update(&tick(8, 9, 7)));
        assert_eq!(clock.text(), "08:09");
    }

    #[test]
    fn ticks_within_the_same_minute_are_not_a_visible_change() {
        let mut clock = ClockWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(clock.update(&tick(8, 9, 7)));
        // A later second in the same minute formats identically: no redraw.
        assert!(!clock.update(&tick(8, 9, 42)));
        assert_eq!(clock.text(), "08:09");
    }

    #[test]
    fn advancing_a_minute_is_a_visible_change() {
        let mut clock = ClockWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(clock.update(&tick(8, 9, 7)));
        assert!(clock.update(&tick(8, 10, 0)));
        assert_eq!(clock.text(), "08:10");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut clock = ClockWidget::new(Bounds::new(0, 0, 1, 1));
        clock.set_bounds(Bounds::new(0, 0, 1920, 32));
        assert_eq!(clock.bounds(), Bounds::new(0, 0, 1920, 32));
    }

    #[test]
    fn left_and_right_clicks_run_their_configured_programs() {
        let left = PathBuf::from("gsimplecal");
        let right = PathBuf::from("gnome-calendar");
        let widget = ClockWidget::new(Bounds::new(10, 0, 100, 32))
            .with_on_click(Some(left.clone()))
            .with_on_click_right(Some(right.clone()));
        assert_eq!(
            widget.on_click(20, 16, ClickButton::Left),
            Some(Command::RunProgram(left))
        );
        assert_eq!(
            widget.on_click(20, 16, ClickButton::Right),
            Some(Command::RunProgram(right))
        );
    }

    #[test]
    fn unconfigured_or_outside_clicks_are_ignored() {
        let widget = ClockWidget::new(Bounds::new(10, 0, 100, 32));
        assert_eq!(widget.on_click(20, 16, ClickButton::Left), None);

        let widget = widget.with_on_click(Some(PathBuf::from("gsimplecal")));
        assert_eq!(widget.on_click(200, 16, ClickButton::Left), None);
    }
}
