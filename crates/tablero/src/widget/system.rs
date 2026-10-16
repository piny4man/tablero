//! The system-stats widget and its normalized CPU/memory model.
//!
//! [`SystemStats`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::System`]; [`SystemWidget`] renders it, repainting
//! only when a visible percentage actually changes.
//!
//! Both readings are whole-percent CPU and memory load, shown compactly at a
//! glance: a cpu icon and a memory icon each labeling their own percent, or the
//! spelled-out `CPU x% MEM y%` when icons are opted out — basic system pressure
//! without opening another tool. Hovering shows CPU load and memory used over
//! total in a tooltip.

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    Msg, ResolvedIcon, Tooltip, Widget, WidgetStyle, draw_builtin_icons, draw_icon_content, expand,
    measure_icon_content,
};

/// Placeholders accepted by the system `format` and `tooltip-format`.
pub const SYSTEM_PLACEHOLDERS: &[&str] = &["icon", "cpu", "mem"];

/// Kibibytes in one gibibyte.
const KIB_PER_GIB: u64 = 1024 * 1024;

/// The built-in icons for the readout's two slots: cpu load, then memory load.
const SYSTEM_ICONS: [BuiltinIcon; 2] = [BuiltinIcon::System, BuiltinIcon::Memory];

/// A normalized snapshot of system pressure: CPU and memory load, each a whole
/// percent.
///
/// Normalization happens once, at the producer boundary, so the widget and the
/// redraw policy compare clean, canonical values: each reading is clamped to
/// `0..=100` and rounded to a whole percent, and a `NaN` reading degrades to `0`
/// rather than panicking. Equality is therefore a faithful "does this look
/// different on screen?" test — sub-percent jitter from sampling never forces a
/// repaint. Memory used/total ride along in tenths of a GiB for the tooltip,
/// so kilobyte-level churn never counts as a change either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemStats {
    cpu: u8,
    mem: u8,
    memory: Option<(u32, u32)>,
}

impl SystemStats {
    /// Build a normalized snapshot from raw CPU and memory load percentages.
    ///
    /// Each value is clamped to `0.0..=100.0` and rounded to the nearest whole
    /// percent; a `NaN` input becomes `0`. Pass the sampler's readings verbatim —
    /// the clamping here is the single place out-of-range values are tamed.
    pub fn new(cpu: f64, mem: f64) -> Self {
        Self {
            cpu: normalize_percent(cpu),
            mem: normalize_percent(mem),
            memory: None,
        }
    }

    /// Attach memory used and total in kibibytes, kept in tenths of a GiB.
    pub fn with_memory(mut self, used_kib: u64, total_kib: u64) -> Self {
        let tenths = |kib: u64| {
            u32::try_from((kib * 10 + KIB_PER_GIB / 2) / KIB_PER_GIB).unwrap_or(u32::MAX)
        };
        self.memory = (total_kib > 0).then(|| (tenths(used_kib.min(total_kib)), tenths(total_kib)));
        self
    }

    /// Memory used and total in GiB (one decimal), when known.
    pub fn memory_gib(self) -> Option<(f64, f64)> {
        self.memory
            .map(|(used, total)| (f64::from(used) / 10.0, f64::from(total) / 10.0))
    }

    /// The normalized CPU load, `0..=100`.
    pub fn cpu(self) -> u8 {
        self.cpu
    }

    /// The normalized memory load, `0..=100`.
    pub fn mem(self) -> u8 {
        self.mem
    }

    /// The display label, e.g. `"CPU 12% MEM 47%"`.
    ///
    /// Keeping this a pure function makes the rendered text deterministic and
    /// unit-testable without painting pixels.
    pub fn label(self) -> String {
        format!("CPU {}% MEM {}%", self.cpu, self.mem)
    }
}

/// Clamp a raw load percentage to `0..=100`, rounding to a whole percent and
/// mapping `NaN` to `0`.
fn normalize_percent(value: f64) -> u8 {
    if value.is_nan() {
        0
    } else {
        value.clamp(0.0, 100.0).round() as u8
    }
}

/// A bar widget showing CPU and memory load in compact form.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can report
/// a visible change only when the normalized snapshot actually differs — a
/// repeated identical sample keeps the loop idle. The snapshot is an [`Option`]:
/// `None` is the pre-first-sample state, which renders as empty space, so the
/// widget shows nothing until the first reading arrives. Its resolved
/// [`WidgetStyle`] decides the glyph, the optional pill, and the colors it draws
/// with.
pub struct SystemWidget {
    bounds: Bounds,
    state: Option<SystemStats>,
    style: WidgetStyle,
    /// A custom label format; `None` keeps the built-in per-stat layout.
    format: Option<String>,
    tooltip: bool,
    tooltip_format: Option<String>,
}

impl SystemWidget {
    /// Create a system-stats widget occupying `bounds`, empty until its first
    /// [`Msg::System`] and carrying the default (flat,
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

    /// Set the label format (`{icon}`, `{cpu}`, `{mem}`). With built-in icons,
    /// successive `{icon}` markers draw the cpu then the memory icon.
    pub fn with_format(mut self, format: Option<String>) -> Self {
        if format.is_some() {
            self.format = format;
        }
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

    fn expand(format: &str, stats: SystemStats) -> String {
        expand(
            format,
            &[
                ("cpu", &stats.cpu().to_string()),
                ("mem", &stats.mem().to_string()),
            ],
        )
    }

    /// CPU load and memory used over total, or the custom `tooltip-format`.
    pub fn tooltip_text(&self) -> Option<String> {
        if !self.tooltip {
            return None;
        }
        let stats = self.state?;
        if let Some(format) = &self.tooltip_format {
            let icon = match self.icon() {
                ResolvedIcon::Text(icon) => icon,
                _ => String::new(),
            };
            return Some(Self::expand(format, stats).replace("{icon}", &icon));
        }
        Some(match stats.memory_gib() {
            Some((used, total)) => format!("CPU {}%\nMemory {used:.1}/{total:.1} GiB", stats.cpu()),
            None => format!("CPU {}%\nMemory {}%", stats.cpu(), stats.mem()),
        })
    }

    fn contains(&self, px: u32, py: u32) -> bool {
        let b = self.bounds;
        px >= b.x && px < b.x + b.width && py >= b.y && py < b.y + b.height
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](SystemWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// The currently displayed label (empty before the first sample).
    pub fn label(&self) -> String {
        self.state.map(SystemStats::label).unwrap_or_default()
    }

    /// The pill template, or empty before the first sample (so the widget
    /// reserves no slot).
    ///
    /// With the built-in icons, each `{icon}` slot labels its own percent — a cpu
    /// icon before CPU load, a memory icon before memory load — so the readout
    /// stays legible without the `CPU`/`MEM` words. A custom glyph or the
    /// opted-out state keeps the spelled-out labels behind a single leading slot,
    /// so the readout is unambiguous when there is no per-stat icon to name it.
    fn template(&self) -> String {
        let Some(stats) = self.state else {
            return String::new();
        };
        if let Some(format) = &self.format {
            return Self::expand(format, stats);
        }
        match self.icon() {
            ResolvedIcon::Builtin(_) => {
                format!("{{icon}} {}% {{icon}} {}%", stats.cpu(), stats.mem())
            }
            _ => format!("{{icon}} {}", stats.label()),
        }
    }

    /// The system icon, resolved against its [`System`](BuiltinIcon::System)
    /// default so a custom `icon`/`icon = "none"` still overrides it. This is the
    /// widget's icon *setting* — [`Builtin`](ResolvedIcon::Builtin) means the pair
    /// of built-in icons in [`SYSTEM_ICONS`] is drawn, one per slot. `None` before
    /// the first sample (nothing to show).
    fn icon(&self) -> ResolvedIcon {
        match self.state {
            Some(_) => self.style.resolve_icon(BuiltinIcon::System),
            None => ResolvedIcon::None,
        }
    }
}

impl Widget for SystemWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::System(next) => {
                if self.state == Some(*next) {
                    return false;
                }
                // Memory totals only reach the tooltip; repaint only when the
                // pill itself changes.
                let before = (self.template(), self.icon());
                self.state = Some(*next);
                (self.template(), self.icon()) != before
            }
            _ => false,
        }
    }

    #[hotpath::measure(impl_type = "SystemWidget")]
    fn draw(&self, ctx: &mut RenderContext) {
        // Before the first sample the template is empty, so the pill paints
        // nothing: the dashboard has already cleared the background.
        let template = self.template();
        let colors = self.style.base_colors();
        match self.icon() {
            // Two distinct built-in icons, one per slot (cpu, then memory).
            ResolvedIcon::Builtin(_) => {
                draw_builtin_icons(
                    ctx,
                    &self.style,
                    self.bounds,
                    &SYSTEM_ICONS,
                    &template,
                    colors,
                );
            }
            // A custom glyph or the opted-out state renders through the single-icon
            // text pill, exactly as before per-stat icons existed.
            other => draw_icon_content(ctx, &self.style, self.bounds, &other, &template, colors),
        }
    }

    #[hotpath::measure(impl_type = "SystemWidget")]
    fn measure(&self, ctx: &mut RenderContext, _height: u32) -> u32 {
        measure_icon_content(ctx, &self.style, &self.icon(), &self.template())
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

    fn stats(cpu: f64, mem: f64) -> Msg {
        Msg::System(SystemStats::new(cpu, mem))
    }

    #[test]
    fn new_rounds_each_reading_to_whole_percent() {
        let s = SystemStats::new(12.4, 46.6);
        assert_eq!(s.cpu(), 12);
        assert_eq!(s.mem(), 47);
    }

    #[test]
    fn new_clamps_out_of_range_readings() {
        let s = SystemStats::new(137.0, -5.0);
        assert_eq!(s.cpu(), 100);
        assert_eq!(s.mem(), 0);
    }

    #[test]
    fn new_treats_nan_readings_as_zero() {
        let s = SystemStats::new(f64::NAN, f64::NAN);
        assert_eq!(s.cpu(), 0);
        assert_eq!(s.mem(), 0);
    }

    #[test]
    fn label_joins_cpu_and_memory() {
        assert_eq!(SystemStats::new(12.0, 47.0).label(), "CPU 12% MEM 47%");
        assert_eq!(SystemStats::new(0.0, 100.0).label(), "CPU 0% MEM 100%");
    }

    #[test]
    fn first_sample_changes_state_and_sets_label() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.label(), "");
        assert!(widget.update(&stats(12.0, 47.0)));
        assert_eq!(widget.label(), "CPU 12% MEM 47%");
    }

    #[test]
    fn identical_sample_is_not_a_visible_change() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&stats(12.0, 47.0)));
        // Sub-percent jitter normalizes to the same whole percents: no repaint.
        assert!(!widget.update(&stats(12.2, 47.3)));
        assert_eq!(widget.label(), "CPU 12% MEM 47%");
    }

    #[test]
    fn a_new_cpu_reading_is_a_visible_change() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&stats(12.0, 47.0)));
        assert!(widget.update(&stats(30.0, 47.0)));
        assert_eq!(widget.label(), "CPU 30% MEM 47%");
    }

    #[test]
    fn a_new_memory_reading_is_a_visible_change() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&stats(12.0, 47.0)));
        assert!(widget.update(&stats(12.0, 63.0)));
        assert_eq!(widget.label(), "CPU 12% MEM 63%");
    }

    #[test]
    fn unrelated_message_is_ignored() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&stats(12.0, 47.0));
        let tick = Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, 8, 0, 0).unwrap());
        assert!(!widget.update(&tick));
        assert_eq!(widget.label(), "CPU 12% MEM 47%");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 1, 1));
        widget.set_bounds(Bounds::new(10, 0, 200, 32));
        assert_eq!(widget.bounds(), Bounds::new(10, 0, 200, 32));
    }

    #[test]
    fn template_gives_each_built_in_icon_its_own_percent() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        // Nothing to show before the first sample: no icon, no slot.
        assert_eq!(widget.icon(), ResolvedIcon::None);
        assert_eq!(widget.template(), "");
        widget.update(&stats(12.0, 47.0));
        // With built-in icons, a cpu slot labels the CPU percent and a memory slot
        // labels the memory percent — no spelled-out words.
        assert_eq!(widget.template(), "{icon} 12% {icon} 47%");
        assert_eq!(widget.icon(), ResolvedIcon::Builtin(BuiltinIcon::System));
    }

    #[test]
    fn a_custom_icon_setting_overrides_the_built_in() {
        let style = WidgetStyle {
            icon: crate::widget::IconSetting::Custom("\u{f2db}".to_string()),
            ..WidgetStyle::default()
        };
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32)).with_style(style);
        widget.update(&stats(12.0, 47.0));
        assert_eq!(widget.icon(), ResolvedIcon::Text("\u{f2db}".into()));
    }

    #[test]
    fn icon_none_setting_opts_out_but_keeps_the_readout() {
        let style = WidgetStyle {
            icon: crate::widget::IconSetting::None,
            ..WidgetStyle::default()
        };
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32)).with_style(style);
        widget.update(&stats(12.0, 47.0));
        assert_eq!(widget.icon(), ResolvedIcon::None);
        assert_eq!(widget.template(), "{icon} CPU 12% MEM 47%");
    }

    const GIB: u64 = 1024 * 1024;

    fn stats_gib(cpu: f64, used_kib: u64, total_kib: u64) -> Msg {
        Msg::System(
            SystemStats::new(cpu, used_kib as f64 / total_kib as f64 * 100.0)
                .with_memory(used_kib, total_kib),
        )
    }

    #[test]
    fn memory_totals_are_kept_in_tenths_of_a_gib() {
        let s = SystemStats::new(1.0, 40.0).with_memory(GIB * 61 / 10, GIB * 155 / 10);
        assert_eq!(s.memory_gib(), Some((6.1, 15.5)));
        assert_eq!(SystemStats::new(1.0, 40.0).memory_gib(), None);
    }

    #[test]
    fn custom_format_expands_cpu_and_mem() {
        let mut widget =
            SystemWidget::new(Bounds::new(0, 0, 320, 32)).with_format(Some("{icon}{cpu}%".into()));
        widget.update(&stats(12.0, 47.0));
        assert_eq!(widget.template(), "{icon}12%");
        // Memory is off the label, so a memory-only move does not redraw.
        assert!(!widget.update(&stats(12.0, 60.0)));
        assert!(widget.update(&stats(13.0, 60.0)));
    }

    #[test]
    fn default_tooltip_shows_cpu_and_memory_used_over_total() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.tooltip_text(), None);
        widget.update(&stats_gib(12.0, GIB * 61 / 10, GIB * 155 / 10));
        assert_eq!(
            widget.tooltip_at(10, 10).map(|tip| tip.text).as_deref(),
            Some("CPU 12%\nMemory 6.1/15.5 GiB")
        );
        widget.update(&stats(12.0, 47.0));
        assert_eq!(
            widget.tooltip_text().as_deref(),
            Some("CPU 12%\nMemory 47%")
        );
        assert_eq!(widget.tooltip_at(400, 10), None);
    }

    #[test]
    fn a_memory_total_change_updates_the_tooltip_without_a_redraw() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&stats_gib(12.0, GIB * 4, GIB * 16)));
        // 25% either way, but 4.0 → 4.1 GiB used.
        let next = Msg::System(SystemStats::new(12.0, 25.0).with_memory(GIB * 41 / 10, GIB * 16));
        assert!(!widget.update(&next));
        assert_eq!(
            widget.tooltip_text().as_deref(),
            Some("CPU 12%\nMemory 4.1/16.0 GiB")
        );
    }

    #[test]
    fn tooltip_format_and_opt_out() {
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32))
            .with_tooltip_format(Some("{cpu}/{mem}".into()));
        widget.update(&stats(12.0, 47.0));
        assert_eq!(widget.tooltip_text().as_deref(), Some("12/47"));
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32)).with_tooltip(Some(false));
        widget.update(&stats(12.0, 47.0));
        assert_eq!(widget.tooltip_at(10, 10), None);
    }

    #[test]
    fn no_sample_measures_zero_a_sampled_one_reserves_a_slot() {
        let mut ctx = RenderContext::new(320, 32);
        let mut widget = SystemWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.measure(&mut ctx, 32), 0);
        widget.update(&stats(12.0, 47.0));
        assert!(widget.measure(&mut ctx, 32) > 0);
    }
}
