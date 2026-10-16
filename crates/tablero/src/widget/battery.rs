//! The battery widget and its normalized power-state model.
//!
//! [`Battery`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Battery`]; [`BatteryWidget`] renders it,
//! repainting only when the visible percentage or charge state actually changes.
//!
//! A missing battery (or an unavailable power daemon) is carried as
//! `Msg::Battery(None)`: the widget then shows nothing, exactly as it does before
//! its first reading, so a laptop without a battery never paints a stale or
//! placeholder value.

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    Msg, ResolvedIcon, StateColors, Tooltip, Widget, WidgetStyle, draw_icon_content, expand,
    measure_icon_content,
};

/// Placeholders accepted by the battery `format` and `tooltip-format`.
pub const BATTERY_PLACEHOLDERS: &[&str] = &["icon", "percent", "state", "time"];

/// Validate the battery format placeholders accepted by [`BatteryWidget`].
pub fn validate_battery_format(format: &str) -> Result<(), String> {
    super::validate_placeholders(format, BATTERY_PLACEHOLDERS)
}

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
/// repaint. The time to empty/full rides along in whole minutes for the
/// tooltip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Battery {
    state: BatteryState,
    percent: u8,
    minutes: Option<u32>,
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
        Self {
            state,
            percent,
            minutes: None,
        }
    }

    /// Attach the daemon's time to empty (discharging) or full (charging) in
    /// seconds, rounded to whole minutes. Zero or negative means unknown.
    pub fn with_time(mut self, seconds: i64) -> Self {
        self.minutes = u32::try_from((seconds + 30) / 60)
            .ok()
            .filter(|&minutes| seconds > 0 && minutes > 0);
        self
    }

    /// The estimated minutes to empty or full, when the daemon knows.
    pub fn minutes(self) -> Option<u32> {
        self.minutes
    }

    /// The time estimate as `"2h 14m"` or `"45m"`, when known.
    pub fn time_label(self) -> Option<String> {
        self.minutes.map(|minutes| match minutes / 60 {
            0 => format!("{minutes}m"),
            hours => format!("{hours}h {}m", minutes % 60),
        })
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

/// Discharging-battery foreground by charge tier, following the ARC Raiders
/// palette: a healthy pack on battery power shifts orange → yellow → green as it
/// fills. The critical tier below the style's `warn_threshold` is painted by the
/// configurable [`warn`](WidgetStyle::warn) colors, and a charging pack by the
/// [`charging`](WidgetStyle::charging) colors, so these three cover the
/// everyday-discharge range.
const LEVEL_LOW: (u8, u8, u8, u8) = (0xF5, 0xC7, 0x0F, 0xFF);
const LEVEL_MID: (u8, u8, u8, u8) = (0xF9, 0xCF, 0x07, 0xFF);
const LEVEL_HIGH: (u8, u8, u8, u8) = (0x2D, 0xF1, 0x85, 0xFF);

/// The default semantic icon for a battery snapshot: a charging bolt while power
/// is flowing in, a full battery when topped off, otherwise a low/half/full
/// battery picked by charge tercile.
fn default_icon(battery: Battery) -> BuiltinIcon {
    match battery.state() {
        BatteryState::Charging => BuiltinIcon::BatteryCharging,
        BatteryState::Full => BuiltinIcon::BatteryFull,
        _ => match battery.percent() {
            0..=33 => BuiltinIcon::BatteryLow,
            34..=66 => BuiltinIcon::BatteryHalf,
            _ => BuiltinIcon::BatteryFull,
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
    format: Option<String>,
    tooltip: bool,
    tooltip_format: Option<String>,
}

impl BatteryWidget {
    /// Create a battery widget occupying `bounds`, empty until its first
    /// [`Msg::Battery`] and carrying the default (flat,
    /// glyph-on) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
            format: None,
            tooltip: true,
            tooltip_format: None,
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](BatteryWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// Set the optional `{icon}` / `{percent}` / `{state}` / `{time}` display
    /// format.
    pub fn with_format(mut self, format: Option<String>) -> Self {
        self.format = format;
        self
    }

    /// Enable or disable the hover tooltip.
    pub fn with_tooltip(mut self, tooltip: Option<bool>) -> Self {
        if let Some(tooltip) = tooltip {
            self.tooltip = tooltip;
        }
        self
    }

    /// Set a custom tooltip format (same placeholders as `format`).
    pub fn with_tooltip_format(mut self, format: Option<String>) -> Self {
        if format.is_some() {
            self.tooltip_format = format;
        }
        self
    }

    fn expand(format: &str, battery: Battery) -> String {
        expand(
            format,
            &[
                ("percent", &battery.percent().to_string()),
                ("state", battery.state().label()),
                ("time", &battery.time_label().unwrap_or_default()),
            ],
        )
    }

    /// State and percent plus the time to empty/full when known, or the custom
    /// `tooltip-format`.
    pub fn tooltip_text(&self) -> Option<String> {
        if !self.tooltip {
            return None;
        }
        let battery = self.state?;
        if let Some(format) = &self.tooltip_format {
            let icon = match self.icon(battery) {
                ResolvedIcon::Text(icon) => icon,
                _ => String::new(),
            };
            return Some(Self::expand(format, battery).replace("{icon}", &icon));
        }
        let mut text = format!(
            "Battery: {}% ({})",
            battery.percent(),
            battery.state().label()
        );
        let direction = match battery.state() {
            BatteryState::Discharging => Some("Empty"),
            BatteryState::Charging => Some("Full"),
            _ => None,
        };
        if let (Some(direction), Some(time)) = (direction, battery.time_label()) {
            text.push_str(&format!("\n{direction} in {time}"));
        }
        Some(text)
    }

    fn contains(&self, px: u32, py: u32) -> bool {
        let b = self.bounds;
        px >= b.x && px < b.x + b.width && py >= b.y && py < b.y + b.height
    }

    /// Everything the pill paints, so a tooltip-only change (the time
    /// estimate) is not a visible change.
    fn visible(&self) -> Option<(String, ResolvedIcon, StateColors)> {
        let battery = self.state?;
        Some((
            self.template(),
            self.icon(battery),
            self.state_colors(battery),
        ))
    }

    /// The currently displayed label (empty before the first present reading, or
    /// while no battery is present).
    pub fn label(&self) -> String {
        self.state.map(Battery::label).unwrap_or_default()
    }

    /// The format template — the icon slot marked by `{icon}`, with `{percent}`
    /// and `{state}` already substituted — or empty when no battery is present (so
    /// the widget reserves no slot).
    fn template(&self) -> String {
        match self.state {
            Some(battery) => match &self.format {
                Some(format) => Self::expand(format, battery),
                None => format!("{{icon}} {}", battery.label()),
            },
            None => String::new(),
        }
    }

    /// The battery's icon resolved against the state-derived semantic default.
    fn icon(&self, battery: Battery) -> ResolvedIcon {
        self.style.resolve_icon(default_icon(battery))
    }

    /// The pill colors for the current reading: the style's charging colors while
    /// power is flowing in, its warn colors when a discharging battery is below
    /// its threshold, otherwise the base colors recolored by charge tier.
    fn state_colors(&self, battery: Battery) -> StateColors {
        if battery.state() == BatteryState::Charging {
            self.style.charging
        } else if battery.state() == BatteryState::Discharging
            && u32::from(battery.percent()) < self.style.warn_threshold
        {
            self.style.warn
        } else {
            self.level_colors(battery.percent())
        }
    }

    /// The base colors with the foreground shifted to the charge-tier color, so a
    /// healthy pack on battery power reads orange → yellow → green as it fills
    /// while the configured pill background and border are preserved. The
    /// critical tier is handled separately by [`warn`](WidgetStyle::warn).
    fn level_colors(&self, percent: u8) -> StateColors {
        let foreground = match percent {
            0..=40 => LEVEL_LOW,
            41..=70 => LEVEL_MID,
            _ => LEVEL_HIGH,
        };
        StateColors {
            foreground,
            ..self.style.base_colors()
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
                let before = self.visible();
                self.state = *next;
                self.visible() != before
            }
            _ => false,
        }
    }

    #[hotpath::measure(impl_type = "BatteryWidget")]
    fn draw(&self, ctx: &mut RenderContext) {
        // An absent battery draws nothing: the dashboard has already cleared the
        // background, so the widget's slot is left blank.
        if let Some(battery) = self.state {
            draw_icon_content(
                ctx,
                &self.style,
                self.bounds,
                &self.icon(battery),
                &self.template(),
                self.state_colors(battery),
            );
        }
    }

    #[hotpath::measure(impl_type = "BatteryWidget")]
    fn measure(&self, ctx: &mut RenderContext, _height: u32) -> u32 {
        match self.state {
            Some(battery) => {
                measure_icon_content(ctx, &self.style, &self.icon(battery), &self.template())
            }
            None => 0,
        }
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Bounds) {
        self.bounds = bounds;
    }

    fn tooltip_at(&self, px: u32, py: u32) -> Option<Tooltip> {
        if !self.contains(px, py) {
            return None;
        }
        Some(Tooltip {
            text: self.tooltip_text()?,
            bounds: self.bounds,
        })
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
    fn default_icon_ramps_by_tercile_while_discharging() {
        let icon = |percent| default_icon(Battery::new(BatteryState::Discharging, percent));
        // Each third of the charge range steps the ramp up one notch, low→full.
        assert_eq!(icon(0.0), BuiltinIcon::BatteryLow);
        assert_eq!(icon(33.0), BuiltinIcon::BatteryLow);
        assert_eq!(icon(34.0), BuiltinIcon::BatteryHalf);
        assert_eq!(icon(66.0), BuiltinIcon::BatteryHalf);
        assert_eq!(icon(67.0), BuiltinIcon::BatteryFull);
        assert_eq!(icon(100.0), BuiltinIcon::BatteryFull);
    }

    #[test]
    fn charging_shows_the_bolt_and_full_shows_a_full_battery() {
        // On AC the ramp is irrelevant — a bolt marks power going in, and a full
        // battery marks a topped-off pack.
        assert_eq!(
            default_icon(Battery::new(BatteryState::Charging, 5.0)),
            BuiltinIcon::BatteryCharging
        );
        assert_eq!(
            default_icon(Battery::new(BatteryState::Full, 100.0)),
            BuiltinIcon::BatteryFull
        );
    }

    #[test]
    fn template_marks_the_icon_slot_before_the_label() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        // Nothing to show before the first reading: no slot.
        assert_eq!(widget.template(), "");
        widget.update(&battery(BatteryState::Discharging, 85.0));
        assert_eq!(widget.template(), "{icon} 85% discharging");
    }

    #[test]
    fn configured_format_can_keep_percentage_and_drop_state_text() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32))
            .with_format(Some("{icon} {percent}%".to_string()));
        widget.update(&battery(BatteryState::Charging, 81.0));
        // `{icon}` stays a marker for the layout helper; `{percent}` is filled in.
        assert_eq!(widget.template(), "{icon} 81%");
    }

    #[test]
    fn battery_format_rejects_unknown_or_unbalanced_placeholders() {
        assert_eq!(validate_battery_format("{icon}{percent}% {time}"), Ok(()));
        assert!(validate_battery_format("{icon} {percent}% {state}").is_ok());
        assert!(validate_battery_format("{watts}").is_err());
        assert!(validate_battery_format("{percent").is_err());
        assert!(validate_battery_format("percent}").is_err());
    }

    #[test]
    fn charging_battery_uses_its_charging_colors() {
        let charging = StateColors {
            background: Some((0x2D, 0xF1, 0x85, 0x20)),
            foreground: (0x5F, 0xF5, 0xA0, 0xFF),
            border: Some((0x2D, 0xF1, 0x85, 0xFF)),
        };
        let style = WidgetStyle {
            charging,
            ..WidgetStyle::default()
        };
        let widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32)).with_style(style);
        assert_eq!(
            widget.state_colors(Battery::new(BatteryState::Charging, 81.0)),
            charging
        );
    }

    #[test]
    fn a_low_discharging_battery_uses_the_warn_colors() {
        // Default style, default 20% threshold: below it while discharging swaps
        // to the warn colors.
        let widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        let style = WidgetStyle::default();
        assert_eq!(
            widget.state_colors(Battery::new(BatteryState::Discharging, 15.0)),
            style.warn
        );
    }

    #[test]
    fn a_healthy_discharging_battery_recolors_by_charge_tier() {
        // At or above the warn threshold the foreground steps orange → yellow →
        // green with the charge, while the base background and border are kept.
        let widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        let base = WidgetStyle::default().base_colors();
        let fg = |percent| {
            widget
                .state_colors(Battery::new(BatteryState::Discharging, percent))
                .foreground
        };
        assert_eq!(fg(30.0), LEVEL_LOW);
        assert_eq!(fg(55.0), LEVEL_MID);
        assert_eq!(fg(90.0), LEVEL_HIGH);
        // Only the foreground moves; the pill fill and border stay the base ones.
        let colors = widget.state_colors(Battery::new(BatteryState::Discharging, 55.0));
        assert_eq!(colors.background, base.background);
        assert_eq!(colors.border, base.border);
    }

    #[test]
    fn a_battery_on_ac_uses_the_charging_colors() {
        // The warn swap is for *discharging* only: a battery that is charging is
        // recovering, not in trouble, so it takes the charging colors (green by
        // default) regardless of level.
        let widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(
            widget.state_colors(Battery::new(BatteryState::Charging, 5.0)),
            WidgetStyle::default().charging
        );
    }

    fn timed(state: BatteryState, percent: f64, seconds: i64) -> Msg {
        Msg::Battery(Some(Battery::new(state, percent).with_time(seconds)))
    }

    #[test]
    fn time_is_whole_minutes_and_zero_means_unknown() {
        let b = Battery::new(BatteryState::Discharging, 50.0).with_time(2 * 3600 + 14 * 60 + 20);
        assert_eq!(b.minutes(), Some(134));
        assert_eq!(b.time_label().as_deref(), Some("2h 14m"));
        let b = Battery::new(BatteryState::Discharging, 50.0).with_time(45 * 60);
        assert_eq!(b.time_label().as_deref(), Some("45m"));
        assert_eq!(
            Battery::new(BatteryState::Discharging, 50.0)
                .with_time(0)
                .minutes(),
            None
        );
        assert_eq!(
            Battery::new(BatteryState::Discharging, 50.0)
                .with_time(-5)
                .minutes(),
            None
        );
    }

    #[test]
    fn default_tooltip_shows_state_percent_and_time_remaining() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.tooltip_text(), None);
        widget.update(&timed(BatteryState::Discharging, 85.0, 2 * 3600 + 14 * 60));
        assert_eq!(
            widget.tooltip_at(10, 10).map(|tip| tip.text).as_deref(),
            Some("Battery: 85% (discharging)\nEmpty in 2h 14m")
        );
        widget.update(&timed(BatteryState::Charging, 40.0, 30 * 60));
        assert_eq!(
            widget.tooltip_text().as_deref(),
            Some("Battery: 40% (charging)\nFull in 30m")
        );
        widget.update(&battery(BatteryState::Full, 100.0));
        assert_eq!(
            widget.tooltip_text().as_deref(),
            Some("Battery: 100% (full)")
        );
        assert_eq!(widget.tooltip_at(400, 10), None);
    }

    #[test]
    fn a_time_change_updates_the_tooltip_without_a_redraw() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&timed(BatteryState::Discharging, 85.0, 3600)));
        assert!(!widget.update(&timed(BatteryState::Discharging, 85.0, 3000)));
        assert_eq!(
            widget.tooltip_text().as_deref(),
            Some("Battery: 85% (discharging)\nEmpty in 50m")
        );
        // A format showing the time does redraw.
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32))
            .with_format(Some("{icon}{percent}% {time}".into()));
        widget.update(&timed(BatteryState::Discharging, 85.0, 3600));
        assert_eq!(widget.template(), "{icon}85% 1h 0m");
        assert!(widget.update(&timed(BatteryState::Discharging, 85.0, 3000)));
    }

    #[test]
    fn tooltip_format_and_opt_out() {
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32))
            .with_tooltip_format(Some("{percent} {state} {time}".into()));
        widget.update(&timed(BatteryState::Discharging, 85.0, 600));
        assert_eq!(widget.tooltip_text().as_deref(), Some("85 discharging 10m"));
        let mut widget = BatteryWidget::new(Bounds::new(0, 0, 320, 32)).with_tooltip(Some(false));
        widget.update(&battery(BatteryState::Full, 100.0));
        assert_eq!(widget.tooltip_at(10, 10), None);
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
