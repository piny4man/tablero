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

use crate::render::{Bounds, RenderContext};

use super::{
    Msg, StateColors, Widget, WidgetStyle, draw_text_pill, glyph_label, measure_text_pill,
};

/// Default battery glyphs (Font Awesome, via Nerd Font): a discharge ramp picked
/// by charge quintile, plus a bolt shown while charging or topped off on AC.
const BATTERY_EMPTY: &str = "\u{f244}"; // nf-fa-battery_empty
const BATTERY_QUARTER: &str = "\u{f243}"; // nf-fa-battery_quarter
const BATTERY_HALF: &str = "\u{f242}"; // nf-fa-battery_half
const BATTERY_THREE_QUARTERS: &str = "\u{f241}"; // nf-fa-battery_three_quarters
const BATTERY_FULL: &str = "\u{f240}"; // nf-fa-battery_full
const BATTERY_CHARGING: &str = "\u{f0e7}"; // nf-fa-bolt

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

/// The default glyph for a battery snapshot: a bolt while charging or full on AC,
/// otherwise the discharge ramp picked by charge quintile.
fn default_glyph(battery: Battery) -> &'static str {
    match battery.state() {
        BatteryState::Charging | BatteryState::Full => BATTERY_CHARGING,
        _ => match battery.percent() {
            0..=19 => BATTERY_EMPTY,
            20..=39 => BATTERY_QUARTER,
            40..=59 => BATTERY_HALF,
            60..=79 => BATTERY_THREE_QUARTERS,
            _ => BATTERY_FULL,
        },
    }
}

/// A bar widget showing battery percentage and charge state.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can report
/// a visible change only when the normalized snapshot actually differs. The
/// snapshot is an [`Option`]: `None` is "no battery / unavailable", which renders
/// as empty space — identical to the pre-first-reading state, so an absent
/// battery is shown the same whether it was never there or just went away. Its
/// resolved [`WidgetStyle`] decides the glyph, the optional pill, and the colors
/// it draws with — swapping to the warn colors when a discharging battery falls
/// below the style's threshold.
pub struct BatteryWidget {
    bounds: Bounds,
    state: Option<Battery>,
    style: WidgetStyle,
}

impl BatteryWidget {
    /// Create a battery widget occupying `bounds`, empty until its first
    /// [`Msg::Battery`](super::Msg::Battery) and carrying the default (flat,
    /// glyph-on) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](BatteryWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// The currently displayed label (empty before the first present reading, or
    /// while no battery is present).
    pub fn label(&self) -> String {
        self.state.map(Battery::label).unwrap_or_default()
    }

    /// The full pill text: the state-derived glyph joined to the label, or empty
    /// when no battery is present (so the widget reserves no slot).
    fn display_text(&self) -> String {
        match self.state {
            Some(battery) => {
                glyph_label(self.style.glyph(default_glyph(battery)), &battery.label())
            }
            None => String::new(),
        }
    }

    /// The pill colors for the current reading: the style's warn colors when a
    /// discharging battery is below its threshold, otherwise the base colors.
    fn state_colors(&self, battery: Battery) -> StateColors {
        if battery.state() == BatteryState::Discharging
            && u32::from(battery.percent()) < self.style.warn_threshold
        {
            self.style.warn
        } else {
            self.style.base_colors()
        }
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
            draw_text_pill(
                ctx,
                &self.style,
                self.bounds,
                &self.display_text(),
                self.state_colors(battery),
            );
        }
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

    #[test]
    fn default_glyph_ramps_by_quintile_while_discharging() {
        let glyph = |percent| default_glyph(Battery::new(BatteryState::Discharging, percent));
        // Each fifth of the charge range steps the ramp up one notch, empty→full.
        assert_eq!(glyph(0.0), BATTERY_EMPTY);
        assert_eq!(glyph(19.0), BATTERY_EMPTY);
        assert_eq!(glyph(20.0), BATTERY_QUARTER);
        assert_eq!(glyph(39.0), BATTERY_QUARTER);
        assert_eq!(glyph(40.0), BATTERY_HALF);
        assert_eq!(glyph(59.0), BATTERY_HALF);
        assert_eq!(glyph(60.0), BATTERY_THREE_QUARTERS);
        assert_eq!(glyph(79.0), BATTERY_THREE_QUARTERS);
        assert_eq!(glyph(80.0), BATTERY_FULL);
        assert_eq!(glyph(100.0), BATTERY_FULL);
    }

    #[test]
    fn charging_or_full_shows_the_bolt_glyph() {
        // On AC the ramp is irrelevant — a bolt marks power going in or topped off.
        assert_eq!(
            default_glyph(Battery::new(BatteryState::Charging, 5.0)),
            BATTERY_CHARGING
        );
        assert_eq!(
            default_glyph(Battery::new(BatteryState::Full, 100.0)),
            BATTERY_CHARGING
        );
    }

    #[test]
    fn display_text_prefixes_the_state_glyph() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        // Nothing to show before the first reading: no glyph, no slot.
        assert_eq!(widget.display_text(), "");
        widget.update(&battery(BatteryState::Discharging, 85.0));
        assert_eq!(
            widget.display_text(),
            format!("{BATTERY_FULL} 85% discharging")
        );
    }

    #[test]
    fn a_low_discharging_battery_uses_the_warn_colors() {
        // Default style, default 20% threshold: below it while discharging swaps
        // to the warn colors; at or above it keeps the base colors.
        let widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        let style = WidgetStyle::default();
        assert_eq!(
            widget.state_colors(Battery::new(BatteryState::Discharging, 15.0)),
            style.warn
        );
        assert_eq!(
            widget.state_colors(Battery::new(BatteryState::Discharging, 50.0)),
            style.base_colors()
        );
    }

    #[test]
    fn a_low_battery_on_ac_keeps_the_base_colors() {
        // The warn swap is for *discharging* only: a low battery that is charging
        // is recovering, not in trouble, so it stays in the base colors.
        let widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(
            widget.state_colors(Battery::new(BatteryState::Charging, 5.0)),
            WidgetStyle::default().base_colors()
        );
    }

    #[test]
    fn an_absent_battery_measures_zero_a_present_one_reserves_a_slot() {
        let mut ctx = RenderContext::new(320, 32);
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.measure(&mut ctx, 32), 0);
        widget.update(&battery(BatteryState::Discharging, 85.0));
        assert!(widget.measure(&mut ctx, 32) > 0);
    }
}
