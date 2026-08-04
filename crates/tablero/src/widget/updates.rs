//! Arch Linux package-update widget.
//!
//! The asynchronous host producer counts official repository and optional AUR
//! updates, then emits a normalized [`PackageUpdates`] snapshot. A `None`
//! snapshot means there is nothing to show: zero updates, a missing helper, and
//! the pre-first-check state all reserve no space in the bar.

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    ClickButton, Command, LaunchSpec, Msg, ResolvedIcon, Tooltip, Widget, WidgetStyle,
    draw_icon_content, measure_icon_content,
};

/// Validate the placeholders accepted by [`UpdatesWidget`].
pub fn validate_updates_format(format: &str) -> Result<(), String> {
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
        if !matches!(placeholder, "count" | "icon") {
            return Err(format!(
                "contains unsupported placeholder `{{{placeholder}}}`"
            ));
        }
        rest = &rest[close + 1..];
    }
    Ok(())
}

/// One package with an available version upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageUpdate {
    name: String,
    current_version: String,
    new_version: String,
}

impl PackageUpdate {
    /// Build one normalized package upgrade.
    pub fn new(
        name: impl Into<String>,
        current_version: impl Into<String>,
        new_version: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            current_version: current_version.into(),
            new_version: new_version.into(),
        }
    }

    /// Package name reported by the update helper.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Currently installed version.
    pub fn current_version(&self) -> &str {
        &self.current_version
    }

    /// Version available after upgrading.
    pub fn new_version(&self) -> &str {
        &self.new_version
    }
}

/// Available official-repository and AUR package updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageUpdates {
    official: Vec<PackageUpdate>,
    aur: Vec<PackageUpdate>,
}

impl PackageUpdates {
    /// Build a snapshot from the two independently collected package lists.
    pub fn new(official: Vec<PackageUpdate>, aur: Vec<PackageUpdate>) -> Self {
        Self { official, aur }
    }

    /// Official repository updates reported by `checkupdates`.
    pub fn official(&self) -> &[PackageUpdate] {
        &self.official
    }

    /// AUR updates reported by `paru -Qua`.
    pub fn aur(&self) -> &[PackageUpdate] {
        &self.aur
    }

    /// Total visible update count, saturating rather than wrapping.
    pub fn total(&self) -> u32 {
        self.official
            .len()
            .saturating_add(self.aur.len())
            .try_into()
            .unwrap_or(u32::MAX)
    }
}

/// A compact available-package count, hidden when no updates are available.
pub struct UpdatesWidget {
    bounds: Bounds,
    state: Option<PackageUpdates>,
    style: WidgetStyle,
    format: Option<String>,
    on_click: Option<LaunchSpec>,
}

impl UpdatesWidget {
    /// Create an empty updates widget with the default flat style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
            format: None,
            on_click: None,
        }
    }

    /// Set the resolved visual style.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// Set the optional `{count}` / `{icon}` display format.
    pub fn with_format(mut self, format: Option<String>) -> Self {
        self.format = format;
        self
    }

    /// Set the executable path run on a left click.
    pub fn with_on_click(mut self, path: Option<LaunchSpec>) -> Self {
        self.on_click = path;
        self
    }

    /// The displayed total, or an empty string while hidden.
    pub fn label(&self) -> String {
        self.state
            .as_ref()
            .map(|updates| updates.total().to_string())
            .unwrap_or_default()
    }

    /// The format template — the icon slot marked by `{icon}`, with `{count}`
    /// already substituted — or empty while no updates are available (so the
    /// widget reserves no slot).
    fn template(&self) -> String {
        let Some(updates) = &self.state else {
            return String::new();
        };
        let count = updates.total().to_string();
        match &self.format {
            Some(format) => format.replace("{count}", &count),
            None => format!("{{icon}} {count}"),
        }
    }

    /// The updates icon resolved against its [`Updates`](BuiltinIcon::Updates)
    /// default.
    fn icon(&self) -> ResolvedIcon {
        self.style.resolve_icon(BuiltinIcon::Updates)
    }

    fn tooltip_text(&self) -> Option<String> {
        let updates = self.state.as_ref()?;
        let mut sections = Vec::new();
        if !updates.official().is_empty() {
            sections.push(format_update_section(
                "Official repositories",
                updates.official(),
            ));
        }
        if !updates.aur().is_empty() {
            sections.push(format_update_section("AUR", updates.aur()));
        }
        (!sections.is_empty()).then(|| sections.join("\n\n"))
    }

    fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.bounds.x
            && px < self.bounds.x + self.bounds.width
            && py >= self.bounds.y
            && py < self.bounds.y + self.bounds.height
    }
}

fn format_update_section(title: &str, packages: &[PackageUpdate]) -> String {
    let mut lines = Vec::with_capacity(packages.len() + 1);
    lines.push(format!("{title} ({})", packages.len()));
    lines.extend(packages.iter().map(|package| {
        format!(
            "{}  {} -> {}",
            package.name(),
            package.current_version(),
            package.new_version()
        )
    }));
    lines.join("\n")
}

impl Widget for UpdatesWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        let Msg::Updates(next) = msg else {
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
        if button != ClickButton::Left {
            return None;
        }
        let path = self.on_click.as_ref()?;
        if !self.contains(px, py) {
            return None;
        }
        Some(Command::RunProgram(path.clone()))
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

    fn package(name: &str) -> PackageUpdate {
        PackageUpdate::new(name, "1.0", "2.0")
    }

    fn updates(official: u32, aur: u32) -> Msg {
        let official = (0..official)
            .map(|index| package(&format!("official-{index}")))
            .collect();
        let aur = (0..aur)
            .map(|index| package(&format!("aur-{index}")))
            .collect();
        Msg::Updates(Some(PackageUpdates::new(official, aur)))
    }

    #[test]
    fn total_combines_official_and_aur_counts() {
        let updates = PackageUpdates::new(vec![package("linux")], vec![package("paru-bin")]);
        assert_eq!(updates.official().len(), 1);
        assert_eq!(updates.aur().len(), 1);
        assert_eq!(updates.total(), 2);
    }

    #[test]
    fn tooltip_groups_packages_with_current_and_new_versions() {
        let mut widget = UpdatesWidget::new(Bounds::new(10, 0, 100, 32));
        widget.update(&Msg::Updates(Some(PackageUpdates::new(
            vec![PackageUpdate::new("linux", "6.15.6", "6.15.7")],
            vec![PackageUpdate::new("paru-bin", "2.0.4", "2.0.5")],
        ))));

        assert_eq!(
            widget.tooltip_at(20, 10).map(|tooltip| tooltip.text),
            Some(
                "Official repositories (1)\nlinux  6.15.6 -> 6.15.7\n\nAUR (1)\nparu-bin  2.0.4 -> 2.0.5"
                    .to_string()
            )
        );
        assert_eq!(widget.tooltip_at(200, 10), None);
    }

    #[test]
    fn first_count_changes_state_and_repeated_count_does_not() {
        let mut widget = UpdatesWidget::new(Bounds::new(0, 0, 100, 32));
        assert!(widget.update(&updates(7, 3)));
        assert_eq!(widget.label(), "10");
        assert!(!widget.update(&updates(7, 3)));
    }

    #[test]
    fn none_hides_a_previous_count() {
        let mut widget = UpdatesWidget::new(Bounds::new(0, 0, 100, 32));
        widget.update(&updates(7, 3));
        assert!(widget.update(&Msg::Updates(None)));
        assert_eq!(widget.label(), "");
        assert_eq!(widget.template(), "");
    }

    #[test]
    fn configured_format_keeps_the_icon_slot_and_substitutes_count() {
        let mut widget = UpdatesWidget::new(Bounds::new(0, 0, 100, 32))
            .with_format(Some("{count}{icon}".to_string()));
        widget.update(&updates(4, 2));
        assert_eq!(widget.template(), "6{icon}");
        assert_eq!(widget.icon(), ResolvedIcon::Builtin(BuiltinIcon::Updates));
    }

    #[test]
    fn invalid_format_placeholders_are_rejected() {
        assert!(validate_updates_format("{count} {icon}").is_ok());
        assert!(validate_updates_format("{percent}").is_err());
        assert!(validate_updates_format("{count").is_err());
    }

    #[test]
    fn configured_click_emits_a_direct_program_command() {
        let path = LaunchSpec::program_only("/usr/local/bin/update-system");
        let widget =
            UpdatesWidget::new(Bounds::new(10, 0, 100, 32)).with_on_click(Some(path.clone()));
        assert_eq!(
            widget.on_click(20, 10, ClickButton::Left),
            Some(Command::RunProgram(path))
        );
        assert_eq!(widget.on_click(20, 10, ClickButton::Right), None);
    }

    #[test]
    fn unrelated_messages_are_ignored() {
        let mut widget = UpdatesWidget::new(Bounds::new(0, 0, 100, 32));
        let tick = Msg::Tick(Local.with_ymd_and_hms(2026, 7, 18, 8, 0, 0).unwrap());
        assert!(!widget.update(&tick));
    }
}
