//! The system-stats widget and its normalized CPU/memory model.
//!
//! [`SystemStats`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::System`](super::Msg::System); [`SystemWidget`] renders it, repainting
//! only when a visible percentage actually changes.
//!
//! Both readings are whole-percent CPU and memory load, so the widget shows a
//! compact `CPU x% MEM y%` the user reads at a glance — basic system pressure
//! without opening another tool.

use crate::render::{Bounds, FG, RenderContext};

use super::{Msg, Widget};

/// A normalized snapshot of system pressure: CPU and memory load, each a whole
/// percent.
///
/// Normalization happens once, at the producer boundary, so the widget and the
/// redraw policy compare clean, canonical values: each reading is clamped to
/// `0..=100` and rounded to a whole percent, and a `NaN` reading degrades to `0`
/// rather than panicking. Equality is therefore a faithful "does this look
/// different on screen?" test — sub-percent jitter from sampling never forces a
/// repaint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemStats {
    cpu: u8,
    mem: u8,
}

impl SystemStats {
    /// Build a normalized snapshot from raw CPU and memory load percentages.
    ///
    /// Each value is clamped to `0.0..=100.0` and rounded to the nearest whole
    /// percent; a `NaN` input becomes `0`. Pass the sampler's readings verbatim —
    /// the clamping here is the single place out-of-range values are tamed.
    pub fn new(cpu: f64, mem: f64) -> Self {
        Self {
            cpu: normalize_percent(cpu),
            mem: normalize_percent(mem),
        }
    }

    /// The normalized CPU load, `0..=100`.
    pub fn cpu(self) -> u8 {
        self.cpu
    }

    /// The normalized memory load, `0..=100`.
    pub fn mem(self) -> u8 {
        self.mem
    }

    /// The display label, e.g. `"CPU 12% MEM 47%"`.
    ///
    /// Keeping this a pure function makes the rendered text deterministic and
    /// unit-testable without painting pixels.
    pub fn label(self) -> String {
        format!("CPU {}% MEM {}%", self.cpu, self.mem)
    }
}

/// Clamp a raw load percentage to `0..=100`, rounding to a whole percent and
/// mapping `NaN` to `0`.
fn normalize_percent(value: f64) -> u8 {
    if value.is_nan() {
        0
    } else {
        value.clamp(0.0, 100.0).round() as u8
    }
}

/// A bar widget showing CPU and memory load in compact form.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can report
/// a visible change only when the normalized snapshot actually differs — a
/// repeated identical sample keeps the loop idle. The snapshot is an [`Option`]:
/// `None` is the pre-first-sample state, which renders as empty space, so the
/// widget shows nothing until the first reading arrives.
pub struct SystemWidget {
    bounds: Bounds,
    state: Option<SystemStats>,
}

impl SystemWidget {
    /// Create a system-stats widget occupying `bounds`, empty until its first
    /// [`Msg::System`](super::Msg::System).
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
        }
    }

    /// The currently displayed label (empty before the first sample).
    pub fn label(&self) -> String {
        self.state.map(SystemStats::label).unwrap_or_default()
    }
}

impl Widget for SystemWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::System(next) => {
                if self.state == Some(*next) {
                    return false;
                }
                self.state = Some(*next);
                true
            }
            _ => false,
        }
    }

    fn draw(&self, ctx: &mut RenderContext) {
        // Before the first sample the widget draws nothing: the dashboard has
        // already cleared the background, so the slot is left blank.
        if let Some(stats) = self.state {
            ctx.draw_text(&stats.label(), self.bounds, FG);
        }
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

    fn stats(cpu: f64, mem: f64) -> Msg {
        Msg::System(SystemStats::new(cpu, mem))
    }

    #[test]
    fn new_rounds_each_reading_to_whole_percent() {
        let s = SystemStats::new(12.4, 46.6);
        assert_eq!(s.cpu(), 12);
        assert_eq!(s.mem(), 47);
    }

    #[test]
    fn new_clamps_out_of_range_readings() {
        let s = SystemStats::new(137.0, -5.0);
        assert_eq!(s.cpu(), 100);
        assert_eq!(s.mem(), 0);
    }

    #[test]
    fn new_treats_nan_readings_as_zero() {
        let s = SystemStats::new(f64::NAN, f64::NAN);
        assert_eq!(s.cpu(), 0);
        assert_eq!(s.mem(), 0);
    }

    #[test]
    fn label_joins_cpu_and_memory() {
        assert_eq!(SystemStats::new(12.0, 47.0).label(), "CPU 12% MEM 47%");
        assert_eq!(SystemStats::new(0.0, 100.0).label(), "CPU 0% MEM 100%");
    }

    #[test]
    fn first_sample_changes_state_and_sets_label() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.label(), "");
        assert!(widget.update(&stats(12.0, 47.0)));
        assert_eq!(widget.label(), "CPU 12% MEM 47%");
    }

    #[test]
    fn identical_sample_is_not_a_visible_change() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&stats(12.0, 47.0)));
        // Sub-percent jitter normalizes to the same whole percents: no repaint.
        assert!(!widget.update(&stats(12.2, 47.3)));
        assert_eq!(widget.label(), "CPU 12% MEM 47%");
    }

    #[test]
    fn a_new_cpu_reading_is_a_visible_change() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&stats(12.0, 47.0)));
        assert!(widget.update(&stats(30.0, 47.0)));
        assert_eq!(widget.label(), "CPU 30% MEM 47%");
    }

    #[test]
    fn a_new_memory_reading_is_a_visible_change() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&stats(12.0, 47.0)));
        assert!(widget.update(&stats(12.0, 63.0)));
        assert_eq!(widget.label(), "CPU 12% MEM 63%");
    }

    #[test]
    fn unrelated_message_is_ignored() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&stats(12.0, 47.0));
        let tick = Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, 8, 0, 0).unwrap());
        assert!(!widget.update(&tick));
        assert_eq!(widget.label(), "CPU 12% MEM 47%");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 1, 1));
        widget.set_bounds(Bounds::new(10, 0, 200, 32));
        assert_eq!(widget.bounds(), Bounds::new(10, 0, 200, 32));
    }
}
