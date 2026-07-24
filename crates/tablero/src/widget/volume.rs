//! The volume widget and its normalized audio-output model.
//!
//! [`Volume`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Volume`]; [`VolumeWidget`] renders it, repainting
//! only when the visible state (level, mute, or device kind) actually changes.
//!
//! The widget reserves a slot only when the source has something to show — a
//! fresh bar with no PipeWire yet (or a system running PipeWire but with no
//! output) measures zero, exactly as the `network` and `system` widgets do.
//! That keeps the right zone clean when audio is not in play; the bluetooth
//! "always show `unavailable`" pattern does not apply here because an absent
//! audio server usually means the user has none, not that the user is interested
//! in seeing a placeholder.
//!
//! The level bucket drives the icon, not the label — the label is the bare
//! `Vol N%` (or `Mute`) the user reads at a glance, and the icon is a small
//! visual hint that swaps between the low/medium/high/muted speaker glyphs as
//! the level crosses its thresholds. Keeping the label short is what keeps the
//! widget narrow enough to live in the right cluster next to the other status
//! pills.

use std::path::PathBuf;

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{Msg, ResolvedIcon, Widget, WidgetStyle, draw_icon_content, measure_icon_content};

/// The kind of audio output the active sink represents, derived from the
/// PipeWire `device.icon-name` (preferred) or `device.form-factor` (fallback).
///
/// The variant is retained on the snapshot so a sink switch counts as a visible
/// change worth repainting, even though the rendered icon now follows the
/// level bucket rather than the device. It is not surfaced in the label — that
/// stays `Vol N%` or `Mute` regardless of which device is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Headphones (`device.icon-name` in `audio-headphones*` or
    /// `device.form-factor` in `headphone`).
    Headphones,
    /// A headset (headphones with a microphone, `audio-headset*` / `headset`).
    Headset,
    /// Speakers (any of the `audio-speakers*` icons or `speaker` form factor).
    Speakers,
    /// A monitor / TV output (HDMI, DisplayPort) — `device.icon-name` matches
    /// `video-display*` or `device.form-factor` is `monitor` or `tv`.
    Monitor,
    /// A phone (e.g. a paired handset) — `phone` / `audio-handsfree*`.
    Phone,
    /// A television output (form factor `tv` when the icon name did not match
    /// the `Monitor` branch).
    Tv,
    /// Anything the heuristics could not classify (no icon, unknown form
    /// factor, or a custom value).
    Other,
}

/// A normalized snapshot of the active output sink: the playback level (whole
/// percent) and the current mute state, plus the device kind retained for
/// change detection.
///
/// Normalization happens once, at the producer boundary, so the widget and the
/// redraw policy compare clean, canonical values: `level` is clamped to
/// `0..=100` and rounded to a whole percent, `NaN`/negative inputs become zero,
/// and the mute flag is honored verbatim. Equality is therefore a faithful
/// "does this look different on screen?" test — sub-percent jitter from
/// sampling never forces a repaint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    level: u8,
    muted: bool,
    device: DeviceKind,
}

impl Volume {
    /// Build a normalized snapshot from a raw linear `level` in `0.0..=1.0`
    /// (the unit PipeWire returns), a `muted` flag, and a `device` kind.
    ///
    /// The level is clamped to `0.0..=1.0` and multiplied by 100, rounding to
    /// the nearest whole percent; a `NaN`/negative input becomes `0`, a value
    /// above `1.0` becomes `100`. Pass the producer's readings verbatim; the
    /// clamping here is the single place out-of-range values are tamed.
    pub fn new(level: f32, muted: bool, device: DeviceKind) -> Self {
        Self {
            level: normalize_level(level),
            muted,
            device,
        }
    }

    /// The normalized level, `0..=100`.
    pub fn level(self) -> u8 {
        self.level
    }

    /// Whether the active sink is muted.
    pub fn muted(self) -> bool {
        self.muted
    }

    /// The device kind the snapshot was taken from — retained so a sink switch
    /// registers as a visible change.
    pub fn device(self) -> DeviceKind {
        self.device
    }

    /// The display label, e.g. `"Vol 42%"` or `"Mute"` when muted.
    ///
    /// The label is intentionally device-blind: keeping the rendered text short
    /// is what lets the widget live next to the other status pills without
    /// pushing them off the bar, and the device kind already surfaces as the
    /// glyph. Keeping this a pure function makes the rendered text deterministic
    /// and unit-testable without painting pixels.
    pub fn label(self) -> String {
        if self.muted {
            "Mute".to_string()
        } else {
            format!("Vol {}%", self.level)
        }
    }
}

/// Clamp a raw linear level to `0.0..=1.0`, multiply by 100, round to a whole
/// percent, and map `NaN`/negative to `0`.
fn normalize_level(value: f32) -> u8 {
    if value.is_nan() || value <= 0.0 {
        0
    } else if value >= 1.0 {
        100
    } else {
        (value * 100.0).round() as u8
    }
}

/// A bar widget showing the active output sink's volume or mute state.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can
/// report a visible change only when the normalized snapshot actually differs
/// — a repeated identical reading keeps the loop idle. The snapshot is an
/// [`Option`]: `None` is the pre-first-reading state, which renders as empty
/// space (the widget measures zero and reserves no slot) — so a system
/// without PipeWire, or a PipeWire install with no audio sinks, is shown the
/// same whether the source is absent or just hasn't reported yet. Its resolved
/// [`WidgetStyle`] decides the glyph, the optional pill, and the colors it
/// draws with. An optional `on_click` path turns clicks into a
/// [`Command::RunProgram`](super::Command::RunProgram) so the user can wire a
/// volume manager launcher (e.g. `pavucontrol`, `wpctl`-wrapping script)
/// without writing Rust.
pub struct VolumeWidget {
    bounds: Bounds,
    state: Option<Volume>,
    style: WidgetStyle,
    on_click: Option<PathBuf>,
}

impl VolumeWidget {
    /// Create a volume widget occupying `bounds`, empty until its first
    /// [`Msg::Volume`] and carrying the default (flat,
    /// glyph-on) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
            on_click: None,
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](VolumeWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// Set the executable path run when the widget is clicked.
    ///
    /// When `None`, the widget is display-only and clicks yield nothing; when
    /// `Some(path)`, a left-click anywhere inside the widget's bounds produces
    /// a [`Command::RunProgram`](super::Command::RunProgram) the host executor
    /// spawns directly (no shell). Consuming and returning `self` chains off
    /// [`new`](VolumeWidget::new) at build time.
    pub fn with_on_click(mut self, path: Option<PathBuf>) -> Self {
        self.on_click = path;
        self
    }

    /// The currently displayed label (empty before the first reading, or while
    /// the source is unavailable).
    pub fn label(&self) -> String {
        self.state.map(Volume::label).unwrap_or_default()
    }

    /// The pill template: an `{icon}` slot paired with the level label. Empty
    /// before the first reading, so the widget reserves no slot.
    fn template(&self) -> String {
        match &self.state {
            Some(volume) => format!("{{icon}} {}", volume.label()),
            None => String::new(),
        }
    }

    /// The speaker icon for the current level bucket — muted, then low/medium/high
    /// by percent — resolved against the style so a custom `icon`/`icon = "none"`
    /// still overrides it. `None` before the first reading (nothing to show).
    fn icon(&self) -> ResolvedIcon {
        let default = match &self.state {
            Some(volume) if volume.muted() => BuiltinIcon::VolumeMuted,
            Some(volume) => match volume.level() {
                0..=33 => BuiltinIcon::VolumeLow,
                34..=66 => BuiltinIcon::VolumeMedium,
                _ => BuiltinIcon::VolumeHigh,
            },
            None => return ResolvedIcon::None,
        };
        self.style.resolve_icon(default)
    }
}

impl Widget for VolumeWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Volume(next) => {
                if self.state == *next {
                    return false;
                }
                self.state = *next;
                true
            }
            _ => false,
        }
    }

    fn draw(&self, ctx: &mut RenderContext) {
        // An absent volume leaves the template empty, so the pill paints
        // nothing: the dashboard has already cleared the background.
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

    fn on_click(&self, px: u32, py: u32, button: super::ClickButton) -> Option<super::Command> {
        // Single-action widget: only the primary button launches the mixer.
        if button != super::ClickButton::Left {
            return None;
        }
        // Only interactive when an on-click path was configured at build time;
        // otherwise the default (None) is correct.
        let path = self.on_click.as_ref()?;
        let b = self.bounds;
        if px < b.x || px >= b.x + b.width || py < b.y || py >= b.y + b.height {
            return None;
        }
        Some(super::Command::RunProgram(path.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::ClickButton;
    use chrono::{Local, TimeZone};

    fn vol(level: f32, muted: bool, device: DeviceKind) -> Msg {
        Msg::Volume(Some(Volume::new(level, muted, device)))
    }

    #[test]
    fn new_clamps_a_negative_level_to_zero() {
        let v = Volume::new(-0.5, false, DeviceKind::Speakers);
        assert_eq!(v.level(), 0);
    }

    #[test]
    fn new_clamps_an_above_one_level_to_one_hundred() {
        let v = Volume::new(2.5, false, DeviceKind::Speakers);
        assert_eq!(v.level(), 100);
    }

    #[test]
    fn new_rounds_a_sub_percent_level_to_the_nearest_whole_percent() {
        let v = Volume::new(0.424, false, DeviceKind::Speakers);
        assert_eq!(v.level(), 42);
        let v = Volume::new(0.426, false, DeviceKind::Speakers);
        assert_eq!(v.level(), 43);
    }

    #[test]
    fn new_treats_nan_and_zero_levels_as_zero() {
        assert_eq!(Volume::new(f32::NAN, false, DeviceKind::Other).level(), 0);
        assert_eq!(Volume::new(0.0, false, DeviceKind::Other).level(), 0);
    }

    #[test]
    fn label_shows_vol_n_percent_when_unmuted() {
        assert_eq!(
            Volume::new(0.42, false, DeviceKind::Speakers).label(),
            "Vol 42%"
        );
    }

    #[test]
    fn label_shows_mute_when_muted() {
        // Mute always reads "Mute" — the level is irrelevant.
        assert_eq!(
            Volume::new(0.42, true, DeviceKind::Speakers).label(),
            "Mute"
        );
        assert_eq!(Volume::new(0.0, true, DeviceKind::Other).label(), "Mute");
        assert_eq!(
            Volume::new(1.0, true, DeviceKind::Headphones).label(),
            "Mute"
        );
    }

    #[test]
    fn label_does_not_include_the_device_name() {
        // The label stays short regardless of the device kind — the glyph is
        // the device hint, the label is the level.
        for device in [
            DeviceKind::Headphones,
            DeviceKind::Headset,
            DeviceKind::Speakers,
            DeviceKind::Monitor,
            DeviceKind::Phone,
            DeviceKind::Tv,
            DeviceKind::Other,
        ] {
            assert_eq!(Volume::new(0.5, false, device).label(), "Vol 50%");
        }
    }

    #[test]
    fn first_reading_changes_state_and_sets_label() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.label(), "");
        assert!(widget.update(&vol(0.42, false, DeviceKind::Speakers)));
        assert_eq!(widget.label(), "Vol 42%");
    }

    #[test]
    fn identical_reading_is_not_a_visible_change() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&vol(0.42, false, DeviceKind::Speakers)));
        // Sub-percent jitter normalizes to the same whole percent: no repaint.
        assert!(!widget.update(&vol(0.424, false, DeviceKind::Speakers)));
        assert_eq!(widget.label(), "Vol 42%");
    }

    #[test]
    fn a_new_level_is_a_visible_change() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&vol(0.42, false, DeviceKind::Speakers)));
        assert!(widget.update(&vol(0.65, false, DeviceKind::Speakers)));
        assert_eq!(widget.label(), "Vol 65%");
    }

    #[test]
    fn a_toggle_to_mute_is_a_visible_change() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&vol(0.42, false, DeviceKind::Speakers)));
        assert!(widget.update(&vol(0.42, true, DeviceKind::Speakers)));
        assert_eq!(widget.label(), "Mute");
    }

    #[test]
    fn a_toggle_back_from_mute_is_a_visible_change() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&vol(0.42, true, DeviceKind::Speakers)));
        assert!(widget.update(&vol(0.42, false, DeviceKind::Speakers)));
        assert_eq!(widget.label(), "Vol 42%");
    }

    #[test]
    fn a_new_device_kind_is_a_visible_change() {
        // Even at the same level and mute state, switching from speakers to
        // headphones changes the rendered glyph — the user needs to see the
        // difference, so the snapshot carries the device and equality says
        // they are not the same.
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&vol(0.5, false, DeviceKind::Speakers)));
        assert!(widget.update(&vol(0.5, false, DeviceKind::Headphones)));
    }

    #[test]
    fn a_volume_going_absent_is_a_visible_change_then_blank() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&vol(0.42, false, DeviceKind::Speakers)));
        // PipeWire gone or no sinks: the snapshot is now None.
        assert!(widget.update(&Msg::Volume(None)));
        assert_eq!(widget.label(), "");
    }

    #[test]
    fn absent_reading_before_any_volume_is_not_a_change() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        // "Unavailable" matches the empty initial state, so nothing to repaint.
        assert!(!widget.update(&Msg::Volume(None)));
        assert_eq!(widget.label(), "");
    }

    #[test]
    fn unrelated_message_is_ignored() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&vol(0.42, false, DeviceKind::Speakers));
        let tick = Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, 8, 0, 0).unwrap());
        assert!(!widget.update(&tick));
        assert_eq!(widget.label(), "Vol 42%");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 1, 1));
        widget.set_bounds(Bounds::new(10, 0, 200, 32));
        assert_eq!(widget.bounds(), Bounds::new(10, 0, 200, 32));
    }

    #[test]
    fn icon_reflects_the_level_bucket_and_mute() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        // Nothing to show before the first reading: no icon, no slot.
        assert_eq!(widget.icon(), ResolvedIcon::None);
        assert_eq!(widget.template(), "");
        // Low / medium / high buckets by level.
        widget.update(&vol(0.10, false, DeviceKind::Speakers));
        assert_eq!(widget.icon(), ResolvedIcon::Builtin(BuiltinIcon::VolumeLow));
        widget.update(&vol(0.50, false, DeviceKind::Speakers));
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::VolumeMedium)
        );
        widget.update(&vol(0.90, false, DeviceKind::Speakers));
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::VolumeHigh)
        );
        // Muted always wins over the level.
        widget.update(&vol(0.90, true, DeviceKind::Speakers));
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::VolumeMuted)
        );
    }

    #[test]
    fn template_pairs_the_icon_slot_with_the_label() {
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&vol(0.42, false, DeviceKind::Speakers));
        assert_eq!(widget.template(), "{icon} Vol 42%");
        widget.update(&vol(0.42, true, DeviceKind::Speakers));
        assert_eq!(widget.template(), "{icon} Mute");
    }

    #[test]
    fn a_custom_icon_setting_overrides_the_built_in() {
        // The widget's resolved WidgetStyle can override the built-in icon with
        // a text glyph of the user's choosing.
        let style = WidgetStyle {
            icon: crate::widget::IconSetting::Custom("\u{f013}".to_string()),
            ..WidgetStyle::default()
        };
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32)).with_style(style);
        widget.update(&vol(0.5, false, DeviceKind::Speakers));
        assert_eq!(widget.icon(), ResolvedIcon::Text("\u{f013}".into()));
    }

    #[test]
    fn icon_none_setting_opts_out_but_keeps_the_label() {
        let style = WidgetStyle {
            icon: crate::widget::IconSetting::None,
            ..WidgetStyle::default()
        };
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32)).with_style(style);
        widget.update(&vol(0.5, false, DeviceKind::Speakers));
        assert_eq!(widget.icon(), ResolvedIcon::None);
        // The level label still shows even with the icon opted out.
        assert_eq!(widget.template(), "{icon} Vol 50%");
    }

    #[test]
    fn no_volume_measures_zero_a_sampled_one_reserves_a_slot() {
        let mut ctx = RenderContext::new(320, 32);
        let mut widget = VolumeWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.measure(&mut ctx, 32), 0);
        widget.update(&vol(0.5, false, DeviceKind::Speakers));
        assert!(widget.measure(&mut ctx, 32) > 0);
    }

    #[test]
    fn without_on_click_a_click_is_a_no_op() {
        let widget = VolumeWidget::new(Bounds::new(0, 0, 200, 32));
        assert_eq!(widget.on_click(10, 10, ClickButton::Left), None);
    }

    #[test]
    fn with_on_click_a_click_inside_bounds_runs_the_program() {
        let widget = VolumeWidget::new(Bounds::new(0, 0, 200, 32))
            .with_on_click(Some(PathBuf::from("/usr/bin/pavucontrol")));
        assert_eq!(
            widget.on_click(10, 10, ClickButton::Left),
            Some(super::super::Command::RunProgram(PathBuf::from(
                "/usr/bin/pavucontrol"
            )))
        );
    }

    #[test]
    fn on_click_outside_bounds_is_ignored_even_when_configured() {
        let widget = VolumeWidget::new(Bounds::new(10, 0, 200, 32))
            .with_on_click(Some(PathBuf::from("/usr/bin/pavucontrol")));
        // x=0 is left of bounds.x=10; click is outside the pill.
        assert_eq!(widget.on_click(0, 10, ClickButton::Left), None);
        // x=210 is at/past the right edge (x + width = 210), outside.
        assert_eq!(widget.on_click(210, 10, ClickButton::Left), None);
        // y=32 is at the bottom edge (height = 32 → y range [0, 32)), outside.
        assert_eq!(widget.on_click(50, 32, ClickButton::Left), None);
    }
}
