//! Configurable session power launcher.

use std::path::PathBuf;

use crate::render::{Bounds, RenderContext};

use super::{
    ClickButton, Command, Msg, Widget, WidgetStyle, draw_icon_pill, glyph_label, measure_text_pill,
};

const POWER_GLYPH: &str = "\u{f011}";

/// A static power glyph with independently configurable primary and secondary actions.
pub struct PowerWidget {
    bounds: Bounds,
    style: WidgetStyle,
    on_click: Option<PathBuf>,
    on_click_right: Option<PathBuf>,
}

impl PowerWidget {
    /// Create a display-only power glyph.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            style: WidgetStyle::default(),
            on_click: None,
            on_click_right: None,
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

    fn display_text(&self) -> String {
        glyph_label(self.style.glyph(POWER_GLYPH), "")
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
        draw_icon_pill(
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
}
