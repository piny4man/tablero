//! The battery widget and its normalized power-state model.
//!
//! [`Battery`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Battery`](super::Msg::Battery); [`BatteryWidget`] renders it,
//! repainting only when the visible percentage or charge state actually changes.
//!
//! A missing battery (or an unavailable power daemon) is carried as
//! `Msg::Battery(None)`: the widget then shows nothing, exactly as it does before
//! its first reading, so a laptop without a battery never paints a stale or
//! placeholder value.

use crate::render::{Bounds, FG, RenderContext};

use super::{Msg, Widget};

/// The charge direction of a battery, normalized from a raw power-daemon state.
///
/// The many low-level UPower states collapse into these four so the bar shows a
/// single unambiguous word: a "pending charge" or "empty" reading is not a
/// distinct thing the user needs to see, only whether power is going in, out,
/// topped off, or indeterminate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryState {
    /// Power is flowing in (charging, or about to).
    Charging,
    /// Running on battery (discharging, empty, or about to discharge).
    Discharging,
    /// Charged to capacity.
    Full,
    /// State could not be determined.
    Unknown,
}

impl BatteryState {
    /// A short, human-readable label for the state.
    pub fn label(self) -> &'static str {
        match self {
            BatteryState::Charging => "charging",
            BatteryState::Discharging => "discharging",
            BatteryState::Full => "full",
            BatteryState::Unknown => "unknown",
        }
    }
}

/// A normalized snapshot of a present battery: charge state plus percentage.
///
/// Normalization happens once, at the producer boundary, so the widget and the
/// redraw policy compare clean, canonical values: the percentage is clamped to
/// `0..=100` and rounded to a whole percent, and a `NaN` reading degrades to `0`
/// rather than panicking. Equality is therefore a faithful "does this look
/// different on screen?" test — sub-percent jitter from the daemon never forces a
/// repaint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    state: BatteryState,
    percent: u8,
}

impl Battery {
    /// Build a normalized battery from a charge `state` and a raw `percent`.
    ///
    /// `percent` is clamped to `0.0..=100.0` and rounded to the nearest whole
    /// percent; a `NaN` input becomes `0`. Pass the daemon's reading verbatim —
    /// the clamping here is the single place out-of-range values are tamed.
    pub fn new(state: BatteryState, percent: f64) -> Self {
        let percent = if percent.is_nan() {
            0
        } else {
            percent.clamp(0.0, 100.0).round() as u8
        };
        Self { state, percent }
    }

    /// The normalized charge state.
    pub fn state(self) -> BatteryState {
        self.state
    }

    /// The normalized percentage, `0..=100`.
    pub fn percent(self) -> u8 {
        self.percent
    }

    /// The display label, e.g. `"85% discharging"`.
    ///
    /// Keeping this a pure function makes the rendered text deterministic and
    /// unit-testable without painting pixels.
    pub fn label(self) -> String {
        format!("{}% {}", self.percent, self.state.label())
    }
}

/// A bar widget showing battery percentage and charge state.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can report
/// a visible change only when the normalized snapshot actually differs. The
/// snapshot is an [`Option`]: `None` is "no battery / unavailable", which renders
/// as empty space — identical to the pre-first-reading state, so an absent
/// battery is shown the same whether it was never there or just went away.
pub struct BatteryWidget {
    bounds: Bounds,
    state: Option<Battery>,
}

impl BatteryWidget {
    /// Create a battery widget occupying `bounds`, empty until its first
    /// [`Msg::Battery`](super::Msg::Battery).
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
        }
    }

    /// The currently displayed label (empty before the first present reading, or
    /// while no battery is present).
    pub fn label(&self) -> String {
        self.state.map(Battery::label).unwrap_or_default()
    }
}

impl Widget for BatteryWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Battery(next) => {
                if &self.state == next {
                    return false;
                }
                self.state = *next;
                true
            }
            _ => false,
        }
    }

    fn draw(&self, ctx: &mut RenderContext) {
        // An absent battery draws nothing: the dashboard has already cleared the
        // background, so the widget's slot is left blank.
        if let Some(battery) = self.state {
            ctx.draw_text(&battery.label(), self.bounds, FG);
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

    fn battery(state: BatteryState, percent: f64) -> Msg {
        Msg::Battery(Some(Battery::new(state, percent)))
    }

    #[test]
    fn new_rounds_percentage_to_whole_percent() {
        assert_eq!(Battery::new(BatteryState::Discharging, 84.6).percent(), 85);
        assert_eq!(Battery::new(BatteryState::Discharging, 84.4).percent(), 84);
    }

    #[test]
    fn new_clamps_out_of_range_percentages() {
        assert_eq!(Battery::new(BatteryState::Full, 137.0).percent(), 100);
        assert_eq!(Battery::new(BatteryState::Discharging, -5.0).percent(), 0);
    }

    #[test]
    fn new_treats_nan_percentage_as_zero() {
        assert_eq!(Battery::new(BatteryState::Unknown, f64::NAN).percent(), 0);
    }

    #[test]
    fn state_labels_are_unambiguous_words() {
        assert_eq!(BatteryState::Charging.label(), "charging");
        assert_eq!(BatteryState::Discharging.label(), "discharging");
        assert_eq!(BatteryState::Full.label(), "full");
        assert_eq!(BatteryState::Unknown.label(), "unknown");
    }

    #[test]
    fn label_joins_percentage_and_state() {
        assert_eq!(
            Battery::new(BatteryState::Discharging, 85.0).label(),
            "85% discharging"
        );
        assert_eq!(
            Battery::new(BatteryState::Charging, 12.0).label(),
            "12% charging"
        );
    }

    #[test]
    fn first_reading_changes_state_and_sets_label() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.label(), "");
        assert!(widget.update(&battery(BatteryState::Discharging, 85.0)));
        assert_eq!(widget.label(), "85% discharging");
    }

    #[test]
    fn identical_reading_is_not_a_visible_change() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&battery(BatteryState::Discharging, 85.0)));
        // A sub-percent jitter normalizes to the same whole percent: no repaint.
        assert!(!widget.update(&battery(BatteryState::Discharging, 85.2)));
        assert_eq!(widget.label(), "85% discharging");
    }

    #[test]
    fn a_new_percentage_is_a_visible_change() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&battery(BatteryState::Discharging, 85.0)));
        assert!(widget.update(&battery(BatteryState::Discharging, 84.0)));
        assert_eq!(widget.label(), "84% discharging");
    }

    #[test]
    fn a_new_state_is_a_visible_change() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&battery(BatteryState::Discharging, 85.0)));
        assert!(widget.update(&battery(BatteryState::Charging, 85.0)));
        assert_eq!(widget.label(), "85% charging");
    }

    #[test]
    fn battery_going_absent_is_a_visible_change_then_blank() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&battery(BatteryState::Discharging, 85.0)));
        // Battery removed / daemon gone: the snapshot is now absent.
        assert!(widget.update(&Msg::Battery(None)));
        assert_eq!(widget.label(), "");
    }

    #[test]
    fn absent_reading_before_any_battery_is_not_a_change() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        // "No battery" matches the empty initial state, so nothing to repaint.
        assert!(!widget.update(&Msg::Battery(None)));
        assert_eq!(widget.label(), "");
    }

    #[test]
    fn unrelated_message_is_ignored() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&battery(BatteryState::Full, 100.0));
        let tick = Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, 8, 0, 0).unwrap());
        assert!(!widget.update(&tick));
        assert_eq!(widget.label(), "100% full");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 1, 1));
        widget.set_bounds(Bounds::new(10, 0, 200, 32));
        assert_eq!(widget.bounds(), Bounds::new(10, 0, 200, 32));
    }
}
