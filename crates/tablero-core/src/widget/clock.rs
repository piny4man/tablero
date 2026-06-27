//! The clock widget: an `HH:MM:SS` readout driven by [`Msg::Tick`].

use crate::clock::format_time;
use crate::render::{Bounds, FG, RenderContext};

use super::{Msg, Widget};

/// A live clock rendered through the widget architecture.
///
/// Holds the currently displayed text so [`update`](Widget::update) can report a
/// visible change only when the formatted second actually differs — sub-second
/// ticks that format identically are reported as unchanged, so the host loop
/// stays idle between visible flips.
pub struct ClockWidget {
    bounds: Bounds,
    text: String,
}

impl ClockWidget {
    /// Create a clock occupying `bounds`, with no text until its first tick.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            text: String::new(),
        }
    }

    /// The currently displayed clock text (empty before the first tick).
    pub fn text(&self) -> &str {
        &self.text
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
        ctx.draw_text(&self.text, self.bounds, FG);
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
        assert_eq!(clock.text(), "08:09:07");
    }

    #[test]
    fn repeated_same_second_is_not_a_visible_change() {
        let mut clock = ClockWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(clock.update(&tick(8, 9, 7)));
        assert!(!clock.update(&tick(8, 9, 7)));
        assert_eq!(clock.text(), "08:09:07");
    }

    #[test]
    fn advancing_a_second_is_a_visible_change() {
        let mut clock = ClockWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(clock.update(&tick(8, 9, 7)));
        assert!(clock.update(&tick(8, 9, 8)));
        assert_eq!(clock.text(), "08:09:08");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut clock = ClockWidget::new(Bounds::new(0, 0, 1, 1));
        clock.set_bounds(Bounds::new(0, 0, 1920, 32));
        assert_eq!(clock.bounds(), Bounds::new(0, 0, 1920, 32));
    }
}
