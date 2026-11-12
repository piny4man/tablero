//! Arch Linux package-update widget.
//!
//! The asynchronous host producer reads anzen or checkupdates/paru and emits an
//! [`UpdatesStatus`]: either a [`PackageUpdates`] snapshot of the repository,
//! AUR and vendor lists with any per-source problems, or the reason the check
//! could not run at all.
//!
//! The widget hides only when a trustworthy check found nothing. A problem in
//! any source, stale repository databases, or a failed check stays visible with
//! a marker, so a failure never reads as "up to date".

use std::time::Duration;

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    ClickButton, Command, LaunchSpec, Msg, ResolvedIcon, Tooltip, Widget, WidgetStyle,
    draw_icon_content, measure_icon_content,
};

/// Validate the placeholders accepted by [`UpdatesWidget`].
pub fn validate_updates_format(format: &str) -> Result<(), String> {
    super::validate_placeholders(format, &["count", "icon"])
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

/// Marker appended to the count when a source has a problem.
const PROBLEM_MARKER: &str = "!";
/// Marker appended to the count when the repository databases are stale.
const STALE_MARKER: &str = "~";

/// One update check: repository, AUR and vendor updates, plus anything that
/// makes the count incomplete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageUpdates {
    official: Vec<PackageUpdate>,
    aur: Vec<PackageUpdate>,
    vendor: Vec<PackageUpdate>,
    problems: Vec<String>,
    stale_for: Option<Duration>,
}

impl PackageUpdates {
    /// Build a snapshot from the repository and AUR lists.
    pub fn new(official: Vec<PackageUpdate>, aur: Vec<PackageUpdate>) -> Self {
        Self {
            official,
            aur,
            ..Self::default()
        }
    }

    /// Add vendor package updates.
    pub fn with_vendor(mut self, vendor: Vec<PackageUpdate>) -> Self {
        self.vendor = vendor;
        self
    }

    /// Add the problems that make the count incomplete, one line each.
    pub fn with_problems(mut self, problems: Vec<String>) -> Self {
        self.problems = problems;
        self
    }

    /// Mark the repository databases stale, last synced `age` before the check.
    pub fn with_stale_for(mut self, age: Option<Duration>) -> Self {
        self.stale_for = age;
        self
    }

    /// Official repository updates.
    pub fn official(&self) -> &[PackageUpdate] {
        &self.official
    }

    /// AUR updates.
    pub fn aur(&self) -> &[PackageUpdate] {
        &self.aur
    }

    /// Vendor package updates.
    pub fn vendor(&self) -> &[PackageUpdate] {
        &self.vendor
    }

    /// Problems that make the count incomplete.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// How old the repository databases were, when they are stale.
    pub fn stale_for(&self) -> Option<Duration> {
        self.stale_for
    }

    /// Total visible update count, saturating rather than wrapping.
    pub fn total(&self) -> u32 {
        self.official
            .len()
            .saturating_add(self.aur.len())
            .saturating_add(self.vendor.len())
            .try_into()
            .unwrap_or(u32::MAX)
    }

    /// Whether a count of zero can be trusted as "up to date".
    fn is_complete(&self) -> bool {
        self.problems.is_empty() && self.stale_for.is_none()
    }
}

/// The outcome of one update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatesStatus {
    /// The check ran; sources may still report problems.
    Checked(PackageUpdates),
    /// The check could not run or its output was unusable.
    Failed(String),
}

/// A compact available-package count. Hidden only when a complete check found
/// no updates; problems and failures stay visible with a marker.
pub struct UpdatesWidget {
    bounds: Bounds,
    state: Option<UpdatesStatus>,
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

    /// The displayed count with its marker, or an empty string while hidden:
    /// `!` when a source has a problem or the check failed, `~` when the
    /// repository databases are stale.
    pub fn label(&self) -> String {
        match &self.state {
            None => String::new(),
            Some(UpdatesStatus::Failed(_)) => PROBLEM_MARKER.to_owned(),
            Some(UpdatesStatus::Checked(updates)) => {
                if updates.total() == 0 && updates.is_complete() {
                    return String::new();
                }
                let marker = if !updates.problems().is_empty() {
                    PROBLEM_MARKER
                } else if updates.stale_for().is_some() {
                    STALE_MARKER
                } else {
                    ""
                };
                format!("{}{marker}", updates.total())
            }
        }
    }

    /// Whether the count is incomplete enough to draw in the warn colors.
    fn needs_attention(&self) -> bool {
        match &self.state {
            Some(UpdatesStatus::Failed(_)) => true,
            Some(UpdatesStatus::Checked(updates)) => !updates.problems().is_empty(),
            None => false,
        }
    }

    /// The format template — the icon slot marked by `{icon}`, with `{count}`
    /// already substituted — or empty while hidden (so the widget reserves no
    /// slot).
    fn template(&self) -> String {
        let count = self.label();
        if count.is_empty() {
            return String::new();
        }
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
        let updates = match self.state.as_ref()? {
            UpdatesStatus::Failed(error) => return Some(format!("Update check failed\n{error}")),
            UpdatesStatus::Checked(updates) => updates,
        };
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
        if !updates.vendor().is_empty() {
            sections.push(format_update_section("Vendor", updates.vendor()));
        }
        if !updates.problems().is_empty() {
            let mut lines = vec!["Incomplete check".to_owned()];
            lines.extend(updates.problems().iter().cloned());
            sections.push(lines.join("\n"));
        }
        if let Some(age) = updates.stale_for() {
            sections.push(format!(
                "Repository databases last synced {} ago",
                format_age(age)
            ));
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

/// A coarse age: hours under two days, days after.
fn format_age(age: Duration) -> String {
    let hours = age.as_secs() / 3600;
    if hours < 48 {
        format!("{hours} h")
    } else {
        format!("{} days", hours / 24)
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
        if self.state.as_ref() == Some(next) {
            return false;
        }
        self.state = Some(next.clone());
        true
    }

    #[hotpath::measure(impl_type = "UpdatesWidget")]
    fn draw(&self, ctx: &mut RenderContext) {
        draw_icon_content(
            ctx,
            &self.style,
            self.bounds,
            &self.icon(),
            &self.template(),
            if self.needs_attention() {
                self.style.warn
            } else {
                self.style.base_colors()
            },
        );
    }

    #[hotpath::measure(impl_type = "UpdatesWidget")]
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
        checked(PackageUpdates::new(official, aur))
    }

    fn checked(updates: PackageUpdates) -> Msg {
        Msg::Updates(UpdatesStatus::Checked(updates))
    }

    fn widget_with(msg: &Msg) -> UpdatesWidget {
        let mut widget = UpdatesWidget::new(Bounds::new(10, 0, 100, 32));
        widget.update(msg);
        widget
    }

    fn tooltip(widget: &UpdatesWidget) -> String {
        widget
            .tooltip_at(20, 10)
            .map(|tooltip| tooltip.text)
            .unwrap_or_default()
    }

    #[test]
    fn total_combines_repository_aur_and_vendor_counts() {
        let updates = PackageUpdates::new(vec![package("linux")], vec![package("paru-bin")])
            .with_vendor(vec![package("vendor-app")]);
        assert_eq!(updates.official().len(), 1);
        assert_eq!(updates.aur().len(), 1);
        assert_eq!(updates.vendor().len(), 1);
        assert_eq!(updates.total(), 3);
    }

    #[test]
    fn tooltip_groups_packages_with_current_and_new_versions() {
        let mut widget = UpdatesWidget::new(Bounds::new(10, 0, 100, 32));
        widget.update(&checked(PackageUpdates::new(
            vec![PackageUpdate::new("linux", "6.15.6", "6.15.7")],
            vec![PackageUpdate::new("paru-bin", "2.0.4", "2.0.5")],
        )));

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
    fn the_widget_is_hidden_before_the_first_check() {
        let widget = UpdatesWidget::new(Bounds::new(0, 0, 100, 32));
        assert_eq!(widget.label(), "");
        assert_eq!(widget.template(), "");
        assert_eq!(tooltip(&widget), "");
    }

    #[test]
    fn a_complete_check_with_no_updates_hides_a_previous_count() {
        let mut widget = widget_with(&updates(7, 3));
        assert!(widget.update(&updates(0, 0)));
        assert_eq!(widget.label(), "");
        assert_eq!(widget.template(), "");
    }

    #[test]
    fn a_failed_check_stays_visible_instead_of_reading_as_up_to_date() {
        let mut widget = widget_with(&updates(7, 3));
        assert!(widget.update(&Msg::Updates(UpdatesStatus::Failed(
            "could not run anzen: No such file or directory".to_owned()
        ))));
        assert_eq!(widget.label(), "!");
        assert!(widget.needs_attention());
        assert_eq!(
            tooltip(&widget),
            "Update check failed\ncould not run anzen: No such file or directory"
        );
    }

    #[test]
    fn a_source_problem_marks_the_count_even_at_zero() {
        let widget =
            widget_with(&checked(PackageUpdates::default().with_problems(vec![
                "AUR unavailable: AUR returned 503".to_owned(),
            ])));
        assert_eq!(widget.label(), "0!");
        assert!(widget.needs_attention());
        assert_eq!(
            tooltip(&widget),
            "Incomplete check\nAUR unavailable: AUR returned 503"
        );
    }

    #[test]
    fn a_partial_check_keeps_the_updates_it_found() {
        let widget = widget_with(&checked(
            PackageUpdates::new(vec![PackageUpdate::new("linux", "1", "2")], Vec::new())
                .with_problems(vec!["vendor tool: download failed".to_owned()]),
        ));
        assert_eq!(widget.label(), "1!");
        assert_eq!(
            tooltip(&widget),
            "Official repositories (1)\nlinux  1 -> 2\n\nIncomplete check\nvendor tool: download failed"
        );
    }

    #[test]
    fn stale_databases_get_their_own_marker_and_age() {
        let widget = widget_with(&checked(
            PackageUpdates::new(vec![package("linux")], Vec::new())
                .with_stale_for(Some(Duration::from_secs(30 * 3600))),
        ));
        assert_eq!(widget.label(), "1~");
        assert!(!widget.needs_attention());
        assert!(tooltip(&widget).ends_with("Repository databases last synced 30 h ago"));

        let zero = widget_with(&checked(
            PackageUpdates::default().with_stale_for(Some(Duration::from_secs(5 * 86_400))),
        ));
        assert_eq!(zero.label(), "0~");
        assert_eq!(
            tooltip(&zero),
            "Repository databases last synced 5 days ago"
        );
    }

    #[test]
    fn a_problem_marker_wins_over_the_stale_marker() {
        let widget = widget_with(&checked(
            PackageUpdates::default()
                .with_problems(vec!["refresh failed".to_owned()])
                .with_stale_for(Some(Duration::from_secs(3 * 86_400))),
        ));
        assert_eq!(widget.label(), "0!");
    }

    #[test]
    fn the_tooltip_lists_vendor_updates() {
        let widget = widget_with(&checked(
            PackageUpdates::default()
                .with_vendor(vec![PackageUpdate::new("app-deb", "1.0-1", "1.1-1")]),
        ));
        assert_eq!(tooltip(&widget), "Vendor (1)\napp-deb  1.0-1 -> 1.1-1");
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
