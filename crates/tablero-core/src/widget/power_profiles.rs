//! power-profiles-daemon state, formatting, interaction, and rendering.

use std::collections::BTreeMap;

use crate::render::{Bounds, RenderContext};

use super::{
    ClickButton, Command, IconSetting, Msg, Tooltip, Widget, WidgetStyle, draw_text_pill,
    measure_text_pill,
};

const DEFAULT_FORMAT: &str = "{icon}";
const DEFAULT_TOOLTIP_FORMAT: &str = "Power profile: {profile}\nDriver: {driver}";

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
    format_icons: BTreeMap<String, String>,
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
            format_icons: BTreeMap::from([
                ("default".to_string(), "".to_string()),
                ("performance".to_string(), "".to_string()),
                ("balanced".to_string(), "".to_string()),
                ("power-saver".to_string(), "".to_string()),
            ]),
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

    /// Replace the named icon map.
    pub fn with_format_icons(mut self, icons: Option<BTreeMap<String, String>>) -> Self {
        if let Some(icons) = icons {
            self.format_icons = icons;
        }
        self
    }

    /// Current rendered label, empty while the daemon is unavailable.
    pub fn label(&self) -> String {
        self.active()
            .map(|profile| self.expand(&self.format, profile))
            .unwrap_or_default()
    }

    /// Current expanded tooltip, when enabled and available.
    pub fn tooltip_text(&self) -> Option<String> {
        self.tooltip
            .then(|| self.active().map(|p| self.expand(&self.tooltip_format, p)))
            .flatten()
    }

    fn active(&self) -> Option<&PowerProfile> {
        self.state.as_ref()?.active()
    }

    fn icon(&self, profile: &PowerProfile) -> &str {
        match &self.style.icon {
            IconSetting::None => "",
            IconSetting::Custom(icon) => icon,
            IconSetting::Default => self
                .format_icons
                .get(profile.name())
                .or_else(|| self.format_icons.get("default"))
                .map(String::as_str)
                .unwrap_or(""),
        }
    }

    fn expand(&self, format: &str, profile: &PowerProfile) -> String {
        format
            .replace("{icon}", self.icon(profile))
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
        draw_text_pill(
            ctx,
            &self.style,
            self.bounds,
            &self.label(),
            self.style.base_colors(),
        );
    }

    fn measure(&self, ctx: &mut RenderContext, _height: u32) -> u32 {
        measure_text_pill(ctx, &self.style, &self.label())
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
    fn default_label_uses_active_profile_icon() {
        let mut widget = PowerProfilesWidget::new(Bounds::new(0, 0, 64, 32));
        widget.update(&state("balanced"));
        assert_eq!(widget.label(), "");
    }

    #[test]
    fn formats_label_and_tooltip_fields() {
        let mut widget = PowerProfilesWidget::new(Bounds::new(0, 0, 64, 32))
            .with_format(Some("{icon} {profile}".into()))
            .with_tooltip_format(Some("{driver}/{cpu_driver}/{platform_driver}".into()));
        widget.update(&state("balanced"));
        assert_eq!(widget.label(), " balanced");
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
        assert_eq!(widget.label(), "");
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
