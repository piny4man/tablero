//! Configurable session power launcher.

use std::path::PathBuf;

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    ClickButton, Command, Msg, ResolvedIcon, Tooltip, Widget, WidgetStyle, draw_icon_content,
    measure_icon_content,
};

const DEFAULT_TOOLTIP_FORMAT: &str = "Power menu";

/// A static power glyph with independently configurable primary and secondary actions.
pub struct PowerWidget {
    bounds: Bounds,
    style: WidgetStyle,
    on_click: Option<PathBuf>,
    on_click_right: Option<PathBuf>,
    tooltip: bool,
    tooltip_format: String,
}

impl PowerWidget {
    /// Create a display-only power glyph.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            style: WidgetStyle::default(),
            on_click: None,
            on_click_right: None,
            tooltip: true,
            tooltip_format: DEFAULT_TOOLTIP_FORMAT.to_string(),
        }
    }

    /// Set the resolved visual style.
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

    /// Enable or disable the hover tooltip.
    pub fn with_tooltip(mut self, enabled: Option<bool>) -> Self {
        if let Some(enabled) = enabled {
            self.tooltip = enabled;
        }
        self
    }

    /// Set the hover tooltip text verbatim — no placeholders, since the power
    /// widget carries no per-instance state.
    pub fn with_tooltip_format(mut self, format: Option<String>) -> Self {
        if let Some(format) = format {
            self.tooltip_format = format;
        }
        self
    }

    /// The power widget is a lone always-present `{icon}` slot.
    fn template(&self) -> String {
        "{icon}".to_string()
    }

    /// The power icon, resolved against its [`Power`](BuiltinIcon::Power) default
    /// so a custom `icon`/`icon = "none"` still overrides it.
    fn icon(&self) -> ResolvedIcon {
        self.style.resolve_icon(BuiltinIcon::Power)
    }

    fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.bounds.x
            && px < self.bounds.x + self.bounds.width
            && py >= self.bounds.y
            && py < self.bounds.y + self.bounds.height
    }
}

impl Widget for PowerWidget {
    fn update(&mut self, _msg: &Msg) -> bool {
        false
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

    fn tooltip_at(&self, px: u32, py: u32) -> Option<Tooltip> {
        if !self.tooltip || !self.contains(px, py) {
            return None;
        }
        Some(Tooltip {
            text: self.tooltip_format.clone(),
            bounds: self.bounds,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::render::{Bounds, RenderContext};
    use crate::widget::{ClickButton, Command, Widget};

    use super::PowerWidget;

    #[test]
    fn power_button_is_always_visible() {
        let widget = PowerWidget::new(Bounds::new(0, 0, 32, 32));
        let mut ctx = RenderContext::new(64, 32);
        assert!(widget.measure(&mut ctx, 32) > 0);
    }

    #[test]
    fn left_and_right_clicks_run_their_configured_programs() {
        let left = PathBuf::from("wlogout");
        let right = PathBuf::from("hyprlock");
        let widget = PowerWidget::new(Bounds::new(10, 0, 32, 32))
            .with_on_click(Some(left.clone()))
            .with_on_click_right(Some(right.clone()));
        assert_eq!(
            widget.on_click(20, 10, ClickButton::Left),
            Some(Command::RunProgram(left))
        );
        assert_eq!(
            widget.on_click(20, 10, ClickButton::Right),
            Some(Command::RunProgram(right))
        );
    }

    #[test]
    fn unconfigured_or_outside_clicks_are_ignored() {
        let widget = PowerWidget::new(Bounds::new(10, 0, 32, 32));
        assert_eq!(widget.on_click(20, 10, ClickButton::Left), None);

        let widget = widget.with_on_click(Some(PathBuf::from("wlogout")));
        assert_eq!(widget.on_click(42, 10, ClickButton::Left), None);
    }

    #[test]
    fn tooltip_defaults_on_and_uses_supplied_format() {
        let widget = PowerWidget::new(Bounds::new(10, 0, 32, 32));
        assert_eq!(
            widget.tooltip_at(20, 10).map(|tooltip| tooltip.text),
            Some("Power menu".to_string())
        );
        assert_eq!(widget.tooltip_at(42, 10), None);
    }

    #[test]
    fn tooltip_format_is_honored_and_can_be_disabled() {
        let widget = PowerWidget::new(Bounds::new(10, 0, 32, 32))
            .with_tooltip_format(Some("Power: shutdown".to_string()))
            .with_tooltip(Some(false));
        assert_eq!(widget.tooltip_at(20, 10), None);

        let widget = PowerWidget::new(Bounds::new(10, 0, 32, 32))
            .with_tooltip_format(Some("Power: shutdown".to_string()));
        assert_eq!(
            widget.tooltip_at(20, 10).map(|tooltip| tooltip.text),
            Some("Power: shutdown".to_string())
        );
    }
}
