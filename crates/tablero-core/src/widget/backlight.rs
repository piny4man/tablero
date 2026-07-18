//! Backlight state and widget rendering.

use crate::render::{Bounds, RenderContext};

use super::{
    Command, Msg, ScrollDirection, Widget, WidgetStyle, draw_text_pill, measure_text_pill,
};

const DEFAULT_FORMAT: &str = "{icon}  {percent}%";
const DEFAULT_ICONS: &[&str] = &[
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
];

/// One kernel backlight device normalized for display and selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backlight {
    name: String,
    percent: u8,
    max_brightness: u32,
}

impl Backlight {
    /// Build a snapshot from the raw brightness values exposed by sysfs.
    pub fn new(name: impl Into<String>, brightness: u32, max_brightness: u32) -> Self {
        let percent = if max_brightness == 0 {
            0
        } else {
            ((brightness.min(max_brightness) as f64 * 100.0) / max_brightness as f64).round() as u8
        };
        Self {
            name: name.into(),
            percent,
            max_brightness,
        }
    }

    /// Kernel device name, such as `intel_backlight`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Current brightness as a whole percentage.
    pub fn percent(&self) -> u8 {
        self.percent
    }

    /// Raw maximum brightness, used to choose the most capable default device.
    pub fn max_brightness(&self) -> u32 {
        self.max_brightness
    }
}

/// Select a preferred device, falling back to the device with the largest range.
pub fn select_backlight<'a>(
    devices: &'a [Backlight],
    preferred: Option<&str>,
) -> Option<&'a Backlight> {
    preferred
        .and_then(|name| devices.iter().find(|device| device.name == name))
        .or_else(|| devices.iter().max_by_key(|device| device.max_brightness))
}

/// Validate the small formatting language supported by the backlight widget.
pub fn validate_backlight_format(format: &str) -> Result<(), String> {
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
        if placeholder != "icon" && placeholder != "percent" {
            return Err(format!(
                "contains unsupported placeholder `{{{placeholder}}}`"
            ));
        }
        rest = &rest[close + 1..];
    }
    Ok(())
}

/// A text widget showing and controlling the selected screen backlight.
pub struct BacklightWidget {
    bounds: Bounds,
    state: Option<Backlight>,
    style: WidgetStyle,
    preferred_device: Option<String>,
    format: String,
    format_icons: Vec<String>,
    scroll_step: f64,
}

impl BacklightWidget {
    /// Create an empty backlight widget with Waybar-compatible defaults.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
            preferred_device: None,
            format: DEFAULT_FORMAT.to_string(),
            format_icons: DEFAULT_ICONS
                .iter()
                .map(|icon| (*icon).to_string())
                .collect(),
            scroll_step: 1.0,
        }
    }

    /// Set the resolved visual style.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// Select a kernel backlight device by name.
    pub fn with_device(mut self, device: Option<String>) -> Self {
        self.preferred_device = device;
        self
    }

    /// Set the output format.
    pub fn with_format(mut self, format: Option<String>) -> Self {
        if let Some(format) = format {
            self.format = format;
        }
        self
    }

    /// Set the icon ramp used by `{icon}`.
    pub fn with_format_icons(mut self, icons: Option<Vec<String>>) -> Self {
        if let Some(icons) = icons {
            self.format_icons = icons;
        }
        self
    }

    /// Set the percentage changed by one logical scroll step.
    pub fn with_scroll_step(mut self, step: Option<f64>) -> Self {
        if let Some(step) = step {
            self.scroll_step = step;
        }
        self
    }

    /// Current rendered text, empty while no device is available.
    pub fn label(&self) -> String {
        let Some(backlight) = &self.state else {
            return String::new();
        };
        let icon = match &self.style.icon {
            super::IconSetting::Default => self.icon(backlight.percent),
            super::IconSetting::None => "",
            super::IconSetting::Custom(icon) => icon,
        };
        self.format
            .replace("{icon}", icon)
            .replace("{percent}", &backlight.percent.to_string())
    }

    fn icon(&self, percent: u8) -> &str {
        let index = (percent as usize * self.format_icons.len()) / 101;
        &self.format_icons[index]
    }
}

impl Widget for BacklightWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        let Msg::Backlight(devices) = msg else {
            return false;
        };
        let next = select_backlight(devices, self.preferred_device.as_deref()).cloned();
        if self.state == next {
            return false;
        }
        self.state = next;
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

    fn on_scroll(&self, px: u32, py: u32, direction: ScrollDirection) -> Option<Command> {
        let b = self.bounds;
        if px < b.x || px >= b.x + b.width || py < b.y || py >= b.y + b.height {
            return None;
        }
        let device = self.state.as_ref()?;
        Some(Command::AdjustBacklight {
            device: device.name.clone(),
            direction,
            step: self.scroll_step,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::{IconSetting, Widget};

    fn devices() -> Msg {
        Msg::Backlight(vec![
            Backlight::new("acpi_video0", 5, 10),
            Backlight::new("intel_backlight", 300, 1000),
        ])
    }

    #[test]
    fn raw_brightness_is_clamped_and_rounded() {
        assert_eq!(Backlight::new("panel", 424, 1000).percent(), 42);
        assert_eq!(Backlight::new("panel", 2000, 1000).percent(), 100);
        assert_eq!(Backlight::new("panel", 1, 0).percent(), 0);
    }

    #[test]
    fn preferred_device_wins_and_missing_preference_falls_back() {
        let values = match devices() {
            Msg::Backlight(values) => values,
            _ => unreachable!(),
        };
        assert_eq!(
            select_backlight(&values, Some("acpi_video0")).map(Backlight::name),
            Some("acpi_video0")
        );
        assert_eq!(
            select_backlight(&values, Some("missing")).map(Backlight::name),
            Some("intel_backlight")
        );
    }

    #[test]
    fn format_uses_percent_and_icon_ramp() {
        let mut widget = BacklightWidget::new(Bounds::new(0, 0, 200, 32))
            .with_format(Some("{icon}  {percent}%".to_string()))
            .with_format_icons(Some(vec!["low".into(), "mid".into(), "high".into()]));
        widget.update(&Msg::Backlight(vec![Backlight::new("panel", 50, 100)]));
        assert_eq!(widget.label(), "mid  50%");
        widget.update(&Msg::Backlight(vec![Backlight::new("panel", 100, 100)]));
        assert_eq!(widget.label(), "high  100%");
    }

    #[test]
    fn defaults_use_the_documented_brightness_icon_ramp() {
        let mut widget = BacklightWidget::new(Bounds::new(0, 0, 200, 32));
        widget.update(&Msg::Backlight(vec![Backlight::new("panel", 30, 100)]));
        assert_eq!(widget.label(), "  30%");
    }

    #[test]
    fn style_icon_override_and_none_take_precedence() {
        let mut widget = BacklightWidget::new(Bounds::new(0, 0, 200, 32))
            .with_format(Some("{icon}:{percent}".to_string()))
            .with_style(WidgetStyle {
                icon: IconSetting::Custom("fixed".into()),
                ..WidgetStyle::default()
            });
        widget.update(&Msg::Backlight(vec![Backlight::new("panel", 50, 100)]));
        assert_eq!(widget.label(), "fixed:50");
        widget.style.icon = IconSetting::None;
        assert_eq!(widget.label(), ":50");
    }

    #[test]
    fn scroll_inside_emits_adjustment_for_selected_device() {
        let mut widget =
            BacklightWidget::new(Bounds::new(10, 0, 100, 32)).with_scroll_step(Some(2.5));
        widget.update(&devices());
        assert_eq!(
            widget.on_scroll(20, 10, ScrollDirection::Increase),
            Some(Command::AdjustBacklight {
                device: "intel_backlight".into(),
                direction: ScrollDirection::Increase,
                step: 2.5,
            })
        );
        assert_eq!(widget.on_scroll(0, 10, ScrollDirection::Increase), None);
    }

    #[test]
    fn invalid_formats_are_rejected() {
        assert!(validate_backlight_format("{icon} {percent}%").is_ok());
        assert!(validate_backlight_format("{wat}").is_err());
        assert!(validate_backlight_format("{percent").is_err());
        assert!(validate_backlight_format("percent}").is_err());
    }
}
