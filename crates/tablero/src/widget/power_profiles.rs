//! power-profiles-daemon state, formatting, interaction, and rendering.

use std::collections::BTreeMap;

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    ClickButton, Command, IconSetting, Msg, ResolvedIcon, StateColors, Tooltip, Widget,
    WidgetStyle, draw_icon_content, measure_icon_content,
};

const DEFAULT_FORMAT: &str = "{icon}";
const DEFAULT_TOOLTIP_FORMAT: &str = "Power profile: {profile}\nDriver: {driver}";

/// Built-in per-profile foreground accents, following the ARC Raiders palette:
/// power-saver green, balanced blue, performance yellow. They recolor the glyph
/// (and any label/text override) while preserving the configured pill background
/// and border; a profile outside the three standard names keeps the base color.
const PROFILE_SAVER: (u8, u8, u8, u8) = (0x2D, 0xF1, 0x85, 0xFF);
const PROFILE_BALANCED: (u8, u8, u8, u8) = (0x4C, 0xA6, 0xFF, 0xFF);
const PROFILE_PERFORMANCE: (u8, u8, u8, u8) = (0xF9, 0xCF, 0x07, 0xFF);

/// The built-in foreground accent for a daemon profile name, or `None` for any
/// profile outside the three standard names.
fn accent_for_profile(name: &str) -> Option<(u8, u8, u8, u8)> {
    match name {
        "power-saver" => Some(PROFILE_SAVER),
        "balanced" => Some(PROFILE_BALANCED),
        "performance" => Some(PROFILE_PERFORMANCE),
        _ => None,
    }
}

/// The built-in vector icon for a daemon profile name, defaulting to the
/// balanced icon for any profile outside the three standard names.
fn builtin_for_profile(name: &str) -> BuiltinIcon {
    match name {
        "performance" => BuiltinIcon::PowerProfilePerformance,
        "power-saver" => BuiltinIcon::PowerProfileSaver,
        _ => BuiltinIcon::PowerProfileBalanced,
    }
}

/// Look up a profile's text glyph in a user-supplied named icon map, falling
/// back to a `default` entry and then to an empty string.
fn icon_text_for(icons: &BTreeMap<String, String>, name: &str) -> String {
    icons
        .get(name)
        .or_else(|| icons.get("default"))
        .cloned()
        .unwrap_or_default()
}

/// One profile advertised by power-profiles-daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerProfile {
    name: String,
    driver: String,
    cpu_driver: String,
    platform_driver: String,
}

impl PowerProfile {
    /// Build a profile with the normalized legacy and split driver fields.
    pub fn new(
        name: impl Into<String>,
        driver: impl Into<String>,
        cpu_driver: impl Into<String>,
        platform_driver: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            driver: driver.into(),
            cpu_driver: cpu_driver.into(),
            platform_driver: platform_driver.into(),
        }
    }

    /// D-Bus profile name (`balanced`, `performance`, or `power-saver`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Legacy combined driver field.
    pub fn driver(&self) -> &str {
        &self.driver
    }

    /// CPU power-profile driver.
    pub fn cpu_driver(&self) -> &str {
        &self.cpu_driver
    }

    /// Platform power-profile driver.
    pub fn platform_driver(&self) -> &str {
        &self.platform_driver
    }
}

/// Active profile plus the daemon-defined order used for click rotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PowerProfilesState {
    active: String,
    profiles: Vec<PowerProfile>,
}

impl PowerProfilesState {
    /// Build a snapshot. An unknown active profile is retained but renders blank.
    pub fn new(active: impl Into<String>, profiles: Vec<PowerProfile>) -> Self {
        Self {
            active: active.into(),
            profiles,
        }
    }

    /// Active profile name reported by the daemon.
    pub fn active_name(&self) -> &str {
        &self.active
    }

    /// Profiles in the order advertised by the daemon.
    pub fn profiles(&self) -> &[PowerProfile] {
        &self.profiles
    }

    /// The active profile, when it exists in the available list.
    pub fn active(&self) -> Option<&PowerProfile> {
        self.profiles
            .iter()
            .find(|profile| profile.name == self.active)
    }

    fn rotated(&self, forward: bool) -> Option<&PowerProfile> {
        if self.profiles.is_empty() {
            return None;
        }
        let current = self
            .profiles
            .iter()
            .position(|profile| profile.name == self.active)
            .unwrap_or(0);
        let next = if forward {
            (current + 1) % self.profiles.len()
        } else {
            (current + self.profiles.len() - 1) % self.profiles.len()
        };
        self.profiles.get(next)
    }
}

/// Validate power-profile label and tooltip placeholders.
pub fn validate_power_profiles_format(format: &str) -> Result<(), String> {
    let mut rest = format;
    while !rest.is_empty() {
        let next_open = rest.find('{');
        let next_close = rest.find('}');
        if next_close.is_some_and(|close| next_open.is_none_or(|open| close < open)) {
            return Err("contains an unmatched `}`".to_string());
        }
        let Some(open) = next_open else {
            break;
        };
        rest = &rest[open + 1..];
        let Some(close) = rest.find('}') else {
            return Err("contains an unmatched `{`".to_string());
        };
        let placeholder = &rest[..close];
        if !matches!(
            placeholder,
            "icon" | "profile" | "driver" | "cpu_driver" | "platform_driver"
        ) {
            return Err(format!(
                "contains unsupported placeholder `{{{placeholder}}}`"
            ));
        }
        rest = &rest[close + 1..];
    }
    Ok(())
}

/// A text widget showing and rotating power-profiles-daemon profiles.
pub struct PowerProfilesWidget {
    bounds: Bounds,
    state: Option<PowerProfilesState>,
    style: WidgetStyle,
    format: String,
    tooltip_format: String,
    tooltip: bool,
    /// A user-supplied Waybar-style named icon map. `None` selects the built-in
    /// per-profile vector icons; `Some` overrides them with text glyphs keyed by
    /// profile name (with an optional `default` fallback).
    format_icons: Option<BTreeMap<String, String>>,
}

impl PowerProfilesWidget {
    /// Create an empty widget with the requested Waybar-style defaults.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
            format: DEFAULT_FORMAT.to_string(),
            tooltip_format: DEFAULT_TOOLTIP_FORMAT.to_string(),
            tooltip: true,
            format_icons: None,
        }
    }

    /// Set the resolved visual style.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// Set the bar label format.
    pub fn with_format(mut self, format: Option<String>) -> Self {
        if let Some(format) = format {
            self.format = format;
        }
        self
    }

    /// Set the hover tooltip format.
    pub fn with_tooltip_format(mut self, format: Option<String>) -> Self {
        if let Some(format) = format {
            self.tooltip_format = format;
        }
        self
    }

    /// Enable or disable hover tooltips.
    pub fn with_tooltip(mut self, tooltip: Option<bool>) -> Self {
        if let Some(tooltip) = tooltip {
            self.tooltip = tooltip;
        }
        self
    }

    /// Override the built-in per-profile vector icons with a Waybar-style named
    /// icon map. `Some` swaps to text glyphs keyed by profile name; `None` keeps
    /// the built-in icons.
    pub fn with_format_icons(mut self, icons: Option<BTreeMap<String, String>>) -> Self {
        if let Some(icons) = icons {
            self.format_icons = Some(icons);
        }
        self
    }

    /// The bar template: the format with the profile/driver fields expanded and
    /// the `{icon}` marker preserved, or empty while the daemon is unavailable so
    /// the widget reserves no slot.
    fn template(&self) -> String {
        self.active()
            .map(|profile| self.expand_template(&self.format, profile))
            .unwrap_or_default()
    }

    /// The active profile's icon: opted out, a fixed custom string, a Waybar-style
    /// named text glyph when the user supplied `format-icons`, or the built-in
    /// per-profile vector icon otherwise. Resolves to [`None`](ResolvedIcon::None)
    /// while the daemon is unavailable.
    fn icon(&self) -> ResolvedIcon {
        match &self.style.icon {
            IconSetting::None => ResolvedIcon::None,
            IconSetting::Custom(icon) => ResolvedIcon::Text(icon.clone()),
            IconSetting::Default => match self.active() {
                None => ResolvedIcon::None,
                Some(profile) => match &self.format_icons {
                    Some(icons) => ResolvedIcon::Text(icon_text_for(icons, profile.name())),
                    None => ResolvedIcon::Builtin(builtin_for_profile(profile.name())),
                },
            },
        }
    }

    /// Current expanded tooltip, when enabled and available.
    pub fn tooltip_text(&self) -> Option<String> {
        self.tooltip
            .then(|| {
                self.active()
                    .map(|p| self.expand_tooltip(&self.tooltip_format, p))
            })
            .flatten()
    }

    fn active(&self) -> Option<&PowerProfile> {
        self.state.as_ref()?.active()
    }

    /// The base colors with the foreground shifted to the active profile's
    /// built-in accent (green saver, blue balanced, yellow performance), keeping
    /// the configured pill background and border. An unknown profile — or none —
    /// keeps the base foreground.
    fn state_colors(&self) -> StateColors {
        let base = self.style.base_colors();
        match self.active().and_then(|p| accent_for_profile(p.name())) {
            Some(foreground) => StateColors { foreground, ..base },
            None => base,
        }
    }

    /// The plain-text glyph for `{icon}` in a tooltip: a custom override, a
    /// user-configured named glyph, or empty (the built-in vector icon has no
    /// text form to place in a tooltip string).
    fn icon_text(&self, profile: &PowerProfile) -> String {
        match &self.style.icon {
            IconSetting::None => String::new(),
            IconSetting::Custom(icon) => icon.clone(),
            IconSetting::Default => match &self.format_icons {
                Some(icons) => icon_text_for(icons, profile.name()),
                None => String::new(),
            },
        }
    }

    /// Expand the profile/driver fields, leaving the `{icon}` marker for the
    /// layout helpers to fill with the resolved icon.
    fn expand_template(&self, format: &str, profile: &PowerProfile) -> String {
        format
            .replace("{profile}", profile.name())
            .replace("{driver}", profile.driver())
            .replace("{cpu_driver}", profile.cpu_driver())
            .replace("{platform_driver}", profile.platform_driver())
    }

    /// Expand every field for a plain-text tooltip, rendering `{icon}` as its
    /// text form.
    fn expand_tooltip(&self, format: &str, profile: &PowerProfile) -> String {
        format
            .replace("{icon}", &self.icon_text(profile))
            .replace("{profile}", profile.name())
            .replace("{driver}", profile.driver())
            .replace("{cpu_driver}", profile.cpu_driver())
            .replace("{platform_driver}", profile.platform_driver())
    }

    fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.bounds.x
            && px < self.bounds.x + self.bounds.width
            && py >= self.bounds.y
            && py < self.bounds.y + self.bounds.height
    }
}

impl Widget for PowerProfilesWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        let Msg::PowerProfiles(next) = msg else {
            return false;
        };
        if self.state == *next {
            return false;
        }
        self.state = next.clone();
        true
    }

    fn draw(&self, ctx: &mut RenderContext) {
        draw_icon_content(
            ctx,
            &self.style,
            self.bounds,
            &self.icon(),
            &self.template(),
            self.state_colors(),
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
        let state = self.state.as_ref()?;
        let profile = state.rotated(button == ClickButton::Left)?;
        Some(Command::SetPowerProfile(profile.name().to_string()))
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

    fn state(active: &str) -> Msg {
        Msg::PowerProfiles(Some(PowerProfilesState::new(
            active,
            vec![
                PowerProfile::new("power-saver", "multiple", "amd_pstate", "placeholder"),
                PowerProfile::new("balanced", "multiple", "amd_pstate", "placeholder"),
                PowerProfile::new("performance", "amd_pstate", "amd_pstate", "Unavailable"),
            ],
        )))
    }

    #[test]
    fn default_uses_the_built_in_per_profile_icon() {
        let mut widget = PowerProfilesWidget::new(Bounds::new(0, 0, 64, 32));
        widget.update(&state("balanced"));
        // The default `{icon}` template keeps its icon slot and resolves to the
        // profile's built-in vector art.
        assert_eq!(widget.template(), "{icon}");
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::PowerProfileBalanced)
        );
        widget.update(&state("performance"));
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::PowerProfilePerformance)
        );
        widget.update(&state("power-saver"));
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::PowerProfileSaver)
        );
    }

    #[test]
    fn each_profile_recolors_the_foreground_to_its_accent() {
        let mut widget = PowerProfilesWidget::new(Bounds::new(0, 0, 64, 32));
        let base = WidgetStyle::default().base_colors();
        widget.update(&state("power-saver"));
        assert_eq!(widget.state_colors().foreground, PROFILE_SAVER);
        widget.update(&state("balanced"));
        assert_eq!(widget.state_colors().foreground, PROFILE_BALANCED);
        // The pill background/border come from the base style, unchanged.
        assert_eq!(widget.state_colors().background, base.background);
        assert_eq!(widget.state_colors().border, base.border);
        widget.update(&state("performance"));
        assert_eq!(widget.state_colors().foreground, PROFILE_PERFORMANCE);
    }

    #[test]
    fn an_unknown_or_absent_profile_keeps_the_base_colors() {
        let mut widget = PowerProfilesWidget::new(Bounds::new(0, 0, 64, 32));
        // No reading yet: base colors.
        assert_eq!(widget.state_colors(), WidgetStyle::default().base_colors());
        // A profile outside the three standard names is not recolored.
        widget.update(&Msg::PowerProfiles(Some(PowerProfilesState::new(
            "turbo",
            vec![PowerProfile::new("turbo", "driver", "cpu", "platform")],
        ))));
        assert_eq!(widget.state_colors(), WidgetStyle::default().base_colors());
    }

    #[test]
    fn a_named_format_icons_map_is_a_text_override() {
        let mut widget = PowerProfilesWidget::new(Bounds::new(0, 0, 64, 32)).with_format_icons(
            Some(BTreeMap::from([
                ("balanced".to_string(), "B".to_string()),
                ("default".to_string(), "?".to_string()),
            ])),
        );
        widget.update(&state("balanced"));
        assert_eq!(widget.icon(), ResolvedIcon::Text("B".into()));
        widget.update(&state("performance"));
        assert_eq!(widget.icon(), ResolvedIcon::Text("?".into()));
    }

    #[test]
    fn formats_label_and_tooltip_fields() {
        let mut widget = PowerProfilesWidget::new(Bounds::new(0, 0, 64, 32))
            .with_format(Some("{icon} {profile}".into()))
            .with_tooltip_format(Some("{driver}/{cpu_driver}/{platform_driver}".into()));
        widget.update(&state("balanced"));
        assert_eq!(widget.template(), "{icon} balanced");
        assert_eq!(
            widget.tooltip_text().as_deref(),
            Some("multiple/amd_pstate/placeholder")
        );
    }

    #[test]
    fn clicks_rotate_in_daemon_order() {
        let mut widget = PowerProfilesWidget::new(Bounds::new(10, 0, 64, 32));
        widget.update(&state("balanced"));
        assert_eq!(
            widget.on_click(20, 16, ClickButton::Left),
            Some(Command::SetPowerProfile("performance".into()))
        );
        assert_eq!(
            widget.on_click(20, 16, ClickButton::Right),
            Some(Command::SetPowerProfile("power-saver".into()))
        );
    }

    #[test]
    fn unavailable_daemon_hides_and_disables_interaction() {
        let mut widget = PowerProfilesWidget::new(Bounds::new(0, 0, 64, 32));
        widget.update(&Msg::PowerProfiles(None));
        assert_eq!(widget.template(), "");
        assert_eq!(widget.icon(), ResolvedIcon::None);
        assert_eq!(widget.on_click(10, 10, ClickButton::Left), None);
        assert_eq!(widget.tooltip_at(10, 10), None);
    }

    #[test]
    fn validates_supported_placeholders() {
        assert!(validate_power_profiles_format("{icon} {profile} {driver}").is_ok());
        assert!(validate_power_profiles_format("{cpu_driver} {platform_driver}").is_ok());
        assert!(validate_power_profiles_format("{percent}").is_err());
        assert!(validate_power_profiles_format("{profile").is_err());
    }
}
