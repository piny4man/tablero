//! Hypridle process-state indicator and toggle.

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    ClickButton, Command, Msg, ResolvedIcon, StateColors, Tooltip, Widget, WidgetStyle,
    draw_icon_content, measure_icon_content,
};

/// Whether the session's Hypridle daemon is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hypridle {
    active: bool,
}

impl Hypridle {
    /// Build a normalized process-state snapshot.
    pub fn new(active: bool) -> Self {
        Self { active }
    }

    /// Whether Hypridle is currently running.
    pub fn active(self) -> bool {
        self.active
    }
}

/// A compact lock glyph whose color reflects the Hypridle process state.
pub struct HypridleWidget {
    bounds: Bounds,
    state: Option<Hypridle>,
    style: WidgetStyle,
}

impl HypridleWidget {
    /// Create an empty widget that appears after the first process reading.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
        }
    }

    /// Set the resolved visual style.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// A lone `{icon}` slot once a reading has arrived, or empty before it so the
    /// widget reserves no slot.
    fn template(&self) -> String {
        match self.state {
            Some(_) => "{icon}".to_string(),
            None => String::new(),
        }
    }

    /// The lock icon: open while the idle daemon is active, closed while it is
    /// inhibited — resolved against its state-appropriate built-in so a custom
    /// `icon`/`icon = "none"` still overrides it.
    fn icon(&self) -> ResolvedIcon {
        let default = if self.state.is_some_and(Hypridle::active) {
            BuiltinIcon::HypridleActive
        } else {
            BuiltinIcon::HypridleInactive
        };
        self.style.resolve_icon(default)
    }

    fn colors(&self) -> StateColors {
        StateColors {
            background: self.style.background,
            foreground: if self.state.is_some_and(Hypridle::active) {
                self.style.accent
            } else {
                self.style.foreground
            },
            border: self.style.border,
        }
    }

    fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.bounds.x
            && px < self.bounds.x + self.bounds.width
            && py >= self.bounds.y
            && py < self.bounds.y + self.bounds.height
    }
}

impl Widget for HypridleWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        let Msg::Hypridle(next) = msg else {
            return false;
        };
        if self.state == Some(*next) {
            return false;
        }
        self.state = Some(*next);
        true
    }

    fn draw(&self, ctx: &mut RenderContext) {
        draw_icon_content(
            ctx,
            &self.style,
            self.bounds,
            &self.icon(),
            &self.template(),
            self.colors(),
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
        if button != ClickButton::Left || !self.contains(px, py) {
            return None;
        }
        Some(Command::SetHypridle(!self.state?.active()))
    }

    fn tooltip_at(&self, px: u32, py: u32) -> Option<Tooltip> {
        if !self.contains(px, py) {
            return None;
        }
        let status = if self.state?.active() {
            "active"
        } else {
            "inactive"
        };
        Some(Tooltip {
            text: format!("Hypridle {status}"),
            bounds: self.bounds,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::render::{Bounds, RenderContext};
    use crate::widget::{ClickButton, Command, Msg, Widget};

    use super::{Hypridle, HypridleWidget};

    #[test]
    fn state_changes_redraw_and_identical_readings_do_not() {
        let mut widget = HypridleWidget::new(Bounds::new(0, 0, 32, 32));
        assert!(widget.update(&Msg::Hypridle(Hypridle::new(true))));
        assert!(!widget.update(&Msg::Hypridle(Hypridle::new(true))));
        assert!(widget.update(&Msg::Hypridle(Hypridle::new(false))));
    }

    #[test]
    fn left_click_requests_the_opposite_state() {
        let mut widget = HypridleWidget::new(Bounds::new(10, 0, 32, 32));
        widget.update(&Msg::Hypridle(Hypridle::new(true)));
        assert_eq!(
            widget.on_click(20, 10, ClickButton::Left),
            Some(Command::SetHypridle(false))
        );
        widget.update(&Msg::Hypridle(Hypridle::new(false)));
        assert_eq!(
            widget.on_click(20, 10, ClickButton::Left),
            Some(Command::SetHypridle(true))
        );
        assert_eq!(widget.on_click(20, 10, ClickButton::Right), None);
    }

    #[test]
    fn widget_is_hidden_and_inert_before_the_first_reading() {
        let widget = HypridleWidget::new(Bounds::new(0, 0, 32, 32));
        let mut ctx = RenderContext::new(64, 32);
        assert_eq!(widget.measure(&mut ctx, 32), 0);
        assert_eq!(widget.on_click(10, 10, ClickButton::Left), None);
    }

    #[test]
    fn tooltip_reports_the_current_state() {
        let mut widget = HypridleWidget::new(Bounds::new(0, 0, 32, 32));
        widget.update(&Msg::Hypridle(Hypridle::new(true)));
        assert_eq!(
            widget.tooltip_at(10, 10).map(|tooltip| tooltip.text),
            Some("Hypridle active".to_string())
        );
        widget.update(&Msg::Hypridle(Hypridle::new(false)));
        assert_eq!(
            widget.tooltip_at(10, 10).map(|tooltip| tooltip.text),
            Some("Hypridle inactive".to_string())
        );
    }

    #[test]
    fn unrelated_messages_are_ignored() {
        let mut widget = HypridleWidget::new(Bounds::new(0, 0, 32, 32));
        assert!(!widget.update(&Msg::tick_now()));
    }
}
