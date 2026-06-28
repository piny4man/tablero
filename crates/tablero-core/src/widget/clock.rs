//! The clock widget: an `HH:MM` readout driven by [`Msg::Tick`].

use crate::clock::format_time;
use crate::render::{Bounds, RenderContext};

use super::{Msg, Widget, WidgetStyle, draw_text_pill, glyph_label, measure_text_pill};

/// Default clock glyph: Nerd Font "clock" (`nf-fa-clock_o`).
const CLOCK_GLYPH: &str = "\u{f017}";

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
}

impl ClockWidget {
    /// Create a clock occupying `bounds`, with no text until its first tick and
    /// the default (flat, glyph-on) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            text: String::new(),
            style: WidgetStyle::default(),
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](ClockWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// The currently displayed clock text (empty before the first tick).
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The full pill text: the configured glyph joined to the clock readout, or
    /// empty before the first tick (so the widget reserves no slot).
    fn display_text(&self) -> String {
        if self.text.is_empty() {
            return String::new();
        }
        glyph_label(self.style.glyph(CLOCK_GLYPH), &self.text)
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
        draw_text_pill(
            ctx,
            &self.style,
            self.bounds,
            &self.display_text(),
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
}
