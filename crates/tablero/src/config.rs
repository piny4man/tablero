//! TOML configuration for the bar's visual theme, dimensions, fonts, zone
//! layout, and per-widget styling.
//!
//! The whole tree deserializes from a single TOML document via [`Config::from_toml_str`]
//! (or [`Config::load_from_path`] to read a file), and every field has a
//! documented default — an empty or partial document yields a fully-formed
//! [`Config`], so users only specify what they want to override. Anything the
//! schema does not recognize (an unknown key, an unknown widget name, a
//! malformed color) is a hard [`ConfigError`] rather than a silent fallback.
//!
//! Every field has a documented default, so an absent config file renders the
//! full default bar. Placement (which zone a widget sits in) lives in `[bar]`;
//! per-widget *styling* lives in separate `[widget.<name>]` tables, so moving a
//! widget between zones never disturbs its colors. Colors are `#rrggbb` or
//! `#rrggbbaa` hex (the eight-digit form adds an alpha channel for translucency):
//!
//! ```toml
//! height = 32
//!
//! [bar]
//! # margin insets the whole pill row; gap separates adjacent pills in a zone.
//! # Both are 0 by default (a flush, edge-to-edge bar).
//! margin = 0
//! gap    = 0
//! modules-left   = ["workspaces"]
//! modules-center = ["clock"]
//! modules-right  = ["battery", "system", "network"]
//! # background = "#181818cc"  # optional; overrides [theme] background (translucent bar)
//!
//! [theme]
//! background = "#181818"
//! foreground = "#eaeaea"
//! accent     = "#eaeaea"
//!
//! [font]
//! # family is unset by default (the system default font is used; a Nerd Font is
//! # recommended so widget glyph icons render).
//! size = 16.0
//!
//! # Per-widget styling is opt-in; an absent [widget.<name>] table leaves that
//! # widget flat (no pill). For example, a translucent rounded clock pill:
//! [widget.clock]
//! background = "#2e3440cc"
//! border     = "#59604d"
//! border-width = 1
//! radius     = 10
//!
//! # Per-monitor overrides: a [[monitor]] block restyles or re-lays-out one
//! # output, matched by connector name. Every field except `name` is optional and
//! # inherits the global config; a [monitor.bar] zone replaces the global list
//! # wholesale, a [monitor.widget.*] table folds onto the global widget style.
//! [[monitor]]
//! name = "DP-1"
//! height = 40
//! [monitor.bar]
//! modules-center = ["clock", "system"]
//! [monitor.widget.battery]
//! background = "#3b4252"
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{Deserializer, Error as _};

use crate::render::{Bounds, RenderSettings};
use crate::scale::Scale;
use crate::widget::{
    BacklightWidget, BatteryWidget, BluetoothWidget, ClockWidget, Dashboard, HypridleWidget,
    IconSetting, NetworkWidget, NotificationsWidget, PowerProfilesWidget, PowerWidget, StateColors,
    SystemWidget, TitleWidget, TrayWidget, UpdatesWidget, VolumeWidget, Widget, WidgetStyle,
    WorkspaceWidget, backlight::validate_backlight_format, battery::validate_battery_format,
    power_profiles::validate_power_profiles_format, updates::validate_updates_format,
};

/// Default bar height in pixels.
const DEFAULT_HEIGHT: u32 = 32;
/// Default text size in pixels.
const DEFAULT_FONT_SIZE: f32 = 16.0;
/// Default opaque dark background.
const DEFAULT_BACKGROUND: Color = Color::rgb(0x18, 0x18, 0x18);
/// Default light foreground (also the default accent).
const DEFAULT_FOREGROUND: Color = Color::rgb(0xEA, 0xEA, 0xEA);

/// Smallest accepted bar height, in pixels (a zero-height bar is a broken surface).
const MIN_HEIGHT: u32 = 1;
/// Largest accepted bar height, in pixels — anything larger is surely a mistake.
const MAX_HEIGHT: u32 = 4096;
/// Smallest accepted font size, in pixels.
const MIN_FONT_SIZE: f32 = 1.0;
/// Largest accepted font size, in pixels.
const MAX_FONT_SIZE: f32 = 512.0;
/// Largest accepted widget spacing/padding, in pixels.
const MAX_GAP: u32 = 4096;

/// An RGBA color, parsed from a `"#rrggbb"` (opaque) or `"#rrggbbaa"` hex string.
///
/// The leading `#` is optional in either form. Six hex digits yield an opaque
/// color; eight carry an explicit alpha channel, which is what makes transparent
/// bars and translucent pills expressible in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
    /// Alpha channel; `0xFF` is fully opaque, `0x00` fully transparent.
    pub a: u8,
}

impl Color {
    /// Construct an opaque color from its RGB channels.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xFF }
    }

    /// Construct a color from its RGBA channels.
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// The color as an `(r, g, b)` tuple, dropping alpha.
    pub fn to_rgb(self) -> (u8, u8, u8) {
        (self.r, self.g, self.b)
    }

    /// The color as an `(r, g, b, a)` tuple, the form the renderer consumes.
    pub fn to_rgba(self) -> (u8, u8, u8, u8) {
        (self.r, self.g, self.b, self.a)
    }

    /// Parse a `#rrggbb` / `#rrggbbaa` hex string (the `#` is optional) into a color.
    ///
    /// Six hex digits give an opaque color (alpha `0xFF`); eight set an explicit
    /// alpha. Any other length, or a non-hex digit, returns a human-readable
    /// message, so a typo surfaces as a clear configuration error rather than a
    /// wrong-but-silent color.
    pub fn parse_hex(s: &str) -> Result<Color, String> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        let valid_len = hex.len() == 6 || hex.len() == 8;
        if !valid_len || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "invalid color {s:?}: expected a \"#rrggbb\" or \"#rrggbbaa\" hex string"
            ));
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("validated hex");
        let a = if hex.len() == 8 { channel(6) } else { 0xFF };
        Ok(Color::rgba(channel(0), channel(2), channel(4), a))
    }
}

impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Color::parse_hex(&raw).map_err(D::Error::custom)
    }
}

/// The bar's color theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    /// Background fill behind every widget.
    pub background: Color,
    /// Default text color.
    pub foreground: Color,
    /// Emphasis color (e.g. the active workspace).
    pub accent: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background: DEFAULT_BACKGROUND,
            foreground: DEFAULT_FOREGROUND,
            accent: DEFAULT_FOREGROUND,
        }
    }
}

/// Font selection for rendered text.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Font {
    /// Font family name. `None` uses the system default font.
    pub family: Option<String>,
    /// Text size in pixels.
    pub size: f32,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            family: None,
            size: DEFAULT_FONT_SIZE,
        }
    }
}

/// One kind of widget the bar can render, named in the `widgets` order list.
///
/// Deserialized from a lowercase string (`"clock"`, `"workspaces"`, …); an
/// unrecognized name is a [`ConfigError`], never a silently-dropped widget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WidgetKind {
    /// The Hyprland workspace indicator.
    Workspaces,
    /// The wall clock.
    Clock,
    /// The battery indicator.
    Battery,
    /// The screen brightness indicator and scroll control (opt-in).
    Backlight,
    /// The CPU/memory indicator.
    System,
    /// The network connectivity indicator.
    Network,
    /// The StatusNotifierItem system tray.
    Tray,
    /// The focused window's title on the bound monitor.
    Title,
    /// The local Bluetooth adapter state (opt-in: not in the default zones).
    Bluetooth,
    /// The active PipeWire output sink's volume / mute state (opt-in: not in
    /// the default zones).
    Volume,
    /// The swaync notification indicator (opt-in: not in the default zones).
    Notifications,
    /// The active power-profiles-daemon profile.
    #[serde(rename = "power-profiles-daemon")]
    PowerProfilesDaemon,
    /// Available Arch repository and AUR package updates (opt-in).
    Updates,
    /// Whether the Hypridle daemon is active, with a primary-click toggle (opt-in).
    Hypridle,
    /// Configurable session power launcher (opt-in).
    Power,
}

/// The bar layout: which widgets populate each of the three zones, plus the
/// bar-wide spacing and an optional background that overrides the theme's.
///
/// Placement lives here; per-widget *styling* lives in the separate
/// [`WidgetStyles`] tables — so moving a widget between zones never disturbs its
/// colors, and restyling never disturbs its position. The default zones
/// reproduce a conventional bar: workspaces at the left, clock centered, the
/// status cluster (battery, system, network) at the right; the tray is opt-in.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bar {
    /// Bar background; `None` inherits the theme's background. An `#rrggbbaa`
    /// value with a non-opaque alpha makes the bar (the space between pills)
    /// transparent.
    pub background: Option<Color>,
    /// Inset around the whole pill row, in logical pixels — the gutter that lets
    /// pills float clear of the screen edge.
    pub margin: u32,
    /// Gap between adjacent pills within a zone, in logical pixels.
    pub gap: u32,
    /// Widgets clustered at the left edge.
    #[serde(rename = "modules-left")]
    pub modules_left: Vec<WidgetKind>,
    /// Widgets centered in the bar.
    #[serde(rename = "modules-center")]
    pub modules_center: Vec<WidgetKind>,
    /// Widgets clustered at the right edge.
    #[serde(rename = "modules-right")]
    pub modules_right: Vec<WidgetKind>,
}

impl Default for Bar {
    fn default() -> Self {
        Self {
            background: None,
            margin: 0,
            gap: 0,
            modules_left: vec![WidgetKind::Workspaces],
            modules_center: vec![WidgetKind::Title],
            modules_right: vec![
                WidgetKind::Clock,
                WidgetKind::Battery,
                WidgetKind::System,
                WidgetKind::Network,
                WidgetKind::PowerProfilesDaemon,
            ],
        }
    }
}

/// Optional colors for one widget state (warn, attention, or charging).
///
/// Each channel is optional and inherits that state's resolved defaults when
/// unset, so a `[widget.battery.warn]` table that sets only `foreground` keeps
/// the default red pill behind it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StateColorConfig {
    /// Pill fill for this state, if overridden.
    pub background: Option<Color>,
    /// Text/glyph color for this state, if overridden.
    pub foreground: Option<Color>,
    /// Border color for this state, if overridden.
    pub border: Option<Color>,
}

impl StateColorConfig {
    /// Copy each set channel of this override onto `base`, leaving unset ones
    /// untouched — the config-on-config merge a `[monitor.widget.*]` table uses.
    fn apply(&self, base: &mut StateColorConfig) {
        if self.background.is_some() {
            base.background = self.background;
        }
        if self.foreground.is_some() {
            base.foreground = self.foreground;
        }
        if self.border.is_some() {
            base.border = self.border;
        }
    }
}

/// A `[widget.<name>]` style table: every field optional, folded onto the theme
/// (and built-in defaults) by `WidgetStyleConfig::resolve`.
///
/// An entirely-absent table resolves to a flat widget — no pill, theme
/// foreground — which is what keeps the default bar looking like today's.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WidgetStyleConfig {
    /// Normal-state pill fill; absent means *no pill* (text floats on the bar).
    pub background: Option<Color>,
    /// Normal-state text/glyph color; inherits the theme foreground when unset.
    pub foreground: Option<Color>,
    /// Emphasis color (e.g. the active workspace); inherits the theme accent.
    pub accent: Option<Color>,
    /// Optional border around the widget pill or cell.
    pub border: Option<Color>,
    /// Border width in logical pixels; defaults to one when a border is set.
    #[serde(rename = "border-width")]
    pub border_width: Option<u32>,
    /// Pill corner radius in logical pixels.
    pub radius: Option<u32>,
    /// Inset between a pill edge and its text, in logical pixels.
    pub padding: Option<u32>,
    /// Glyph override: absent keeps the widget's built-in glyph, `"none"`
    /// disables it, any other string is used verbatim.
    pub icon: Option<String>,
    /// Percent below which a discharging battery shows its warn colors.
    #[serde(rename = "warn-threshold")]
    pub warn_threshold: Option<u32>,
    /// Colors for the warn state (e.g. low battery).
    pub warn: StateColorConfig,
    /// Colors for the attention state (e.g. a tray item needing attention).
    pub attention: StateColorConfig,
    /// Colors used while a battery is charging.
    pub charging: StateColorConfig,
    /// Executable path run when the widget is clicked.
    ///
    /// When `Some`, a click inside the widget's bounds emits a
    /// [`Command::RunProgram`](crate::widget::Command::RunProgram) the host
    /// executor spawns directly (no shell). When `None`, the widget is
    /// display-only and clicks yield nothing. The path is taken verbatim and
    /// may use a leading `~` for the user's home, which the executor expands
    /// at click time. Available on every widget kind for forward
    /// compatibility; today bluetooth, volume, and updates honor it.
    #[serde(rename = "on-click")]
    pub on_click: Option<std::path::PathBuf>,
    /// Executable path run when the widget is right-clicked.
    ///
    /// Uses the same direct-spawn semantics as [`on_click`](Self::on_click).
    /// Currently honored by the power widget.
    #[serde(rename = "on-click-right")]
    pub on_click_right: Option<std::path::PathBuf>,
    /// Widget label format. Supported placeholders depend on the widget.
    pub format: Option<String>,
    /// Widget-specific icon configuration: a backlight ramp or named profile map.
    #[serde(rename = "format-icons")]
    pub format_icons: Option<FormatIconsConfig>,
    /// Percentage points changed by one backlight scroll step.
    #[serde(rename = "scroll-step")]
    pub scroll_step: Option<f64>,
    /// Preferred kernel backlight device name.
    pub device: Option<String>,
    /// Whether a supported widget displays its hover tooltip.
    pub tooltip: Option<bool>,
    /// Tooltip format for widgets that expose structured hover details.
    #[serde(rename = "tooltip-format")]
    pub tooltip_format: Option<String>,
}

/// The two Waybar-style shapes accepted by `format-icons`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum FormatIconsConfig {
    /// Ordered low-to-high icon ramp, used by backlight.
    Ramp(Vec<String>),
    /// Named state icons, used by power-profiles-daemon.
    Named(BTreeMap<String, String>),
}

impl FormatIconsConfig {
    fn ramp(&self) -> Option<&Vec<String>> {
        match self {
            Self::Ramp(icons) => Some(icons),
            Self::Named(_) => None,
        }
    }

    fn named(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            Self::Named(icons) => Some(icons),
            Self::Ramp(_) => None,
        }
    }
}

impl WidgetStyleConfig {
    /// Resolve this table against `theme` into a concrete [`WidgetStyle`].
    ///
    /// Unset colors fall back to the theme (foreground/accent) or to the
    /// built-in alert colors (warn/attention); unset geometry falls back to
    /// [`WidgetStyle::default`]; an unset `background` resolves to `None`, which
    /// the widgets read as "draw no pill".
    fn resolve(&self, theme: &Theme) -> WidgetStyle {
        let defaults = WidgetStyle::default();
        let background = self.background.map(Color::to_rgba);
        let foreground = self
            .foreground
            .map(Color::to_rgba)
            .unwrap_or_else(|| theme.foreground.to_rgba());
        let border = self.border.map(Color::to_rgba);
        WidgetStyle {
            background,
            foreground,
            accent: self
                .accent
                .map(Color::to_rgba)
                .unwrap_or_else(|| theme.accent.to_rgba()),
            border,
            border_width: self.border_width.unwrap_or(defaults.border_width),
            radius: self.radius.unwrap_or(defaults.radius),
            padding: self.padding.unwrap_or(defaults.padding),
            icon: resolve_icon(self.icon.as_deref()),
            warn_threshold: self.warn_threshold.unwrap_or(defaults.warn_threshold),
            warn: resolve_state(self.warn, defaults.warn),
            attention: resolve_state(self.attention, defaults.attention),
            charging: resolve_state(
                self.charging,
                StateColors {
                    background,
                    foreground,
                    border,
                },
            ),
        }
    }

    /// Copy each set field of this override onto `base`, leaving unset ones
    /// untouched.
    ///
    /// This is the field-level merge a `[monitor.widget.<name>]` table uses to
    /// refine the global widget style for one output: a monitor that sets only
    /// `background` keeps the global foreground, radius, glyph, and so on. The
    /// merge happens on the still-unresolved [`WidgetStyleConfig`] (before
    /// [`resolve`](WidgetStyleConfig::resolve)), so theme fallbacks are applied
    /// once, afterwards, to the already-merged table.
    fn apply(&self, base: &mut WidgetStyleConfig) {
        if self.background.is_some() {
            base.background = self.background;
        }
        if self.foreground.is_some() {
            base.foreground = self.foreground;
        }
        if self.accent.is_some() {
            base.accent = self.accent;
        }
        if self.border.is_some() {
            base.border = self.border;
        }
        if self.border_width.is_some() {
            base.border_width = self.border_width;
        }
        if self.radius.is_some() {
            base.radius = self.radius;
        }
        if self.padding.is_some() {
            base.padding = self.padding;
        }
        if self.icon.is_some() {
            base.icon = self.icon.clone();
        }
        if self.warn_threshold.is_some() {
            base.warn_threshold = self.warn_threshold;
        }
        if self.on_click.is_some() {
            base.on_click = self.on_click.clone();
        }
        if self.on_click_right.is_some() {
            base.on_click_right = self.on_click_right.clone();
        }
        if self.format.is_some() {
            base.format = self.format.clone();
        }
        if self.format_icons.is_some() {
            base.format_icons = self.format_icons.clone();
        }
        if self.scroll_step.is_some() {
            base.scroll_step = self.scroll_step;
        }
        if self.device.is_some() {
            base.device = self.device.clone();
        }
        if self.tooltip.is_some() {
            base.tooltip = self.tooltip;
        }
        if self.tooltip_format.is_some() {
            base.tooltip_format = self.tooltip_format.clone();
        }
        self.warn.apply(&mut base.warn);
        self.attention.apply(&mut base.attention);
        self.charging.apply(&mut base.charging);
    }
}

/// The per-widget style tables, one named field per widget kind.
///
/// A closed struct rather than a map, so `[widget.<unknown>]` is rejected by
/// `deny_unknown_fields` exactly like any other typo.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WidgetStyles {
    /// Style for the workspaces widget.
    pub workspaces: WidgetStyleConfig,
    /// Style for the clock widget.
    pub clock: WidgetStyleConfig,
    /// Style for the battery widget.
    pub battery: WidgetStyleConfig,
    /// Style and behavior for the backlight widget.
    pub backlight: WidgetStyleConfig,
    /// Style for the system widget.
    pub system: WidgetStyleConfig,
    /// Style for the network widget.
    pub network: WidgetStyleConfig,
    /// Style for the tray widget.
    pub tray: WidgetStyleConfig,
    /// Style for the title widget.
    pub title: WidgetStyleConfig,
    /// Style for the bluetooth widget.
    pub bluetooth: WidgetStyleConfig,
    /// Style for the volume widget.
    pub volume: WidgetStyleConfig,
    /// Style for the notifications widget.
    pub notifications: WidgetStyleConfig,
    /// Style and behavior for the power-profiles-daemon widget.
    #[serde(rename = "power-profiles-daemon")]
    pub power_profiles_daemon: WidgetStyleConfig,
    /// Style and behavior for the Arch package-updates widget.
    pub updates: WidgetStyleConfig,
    /// Style for the Hypridle state widget.
    pub hypridle: WidgetStyleConfig,
    /// Style and launch actions for the power widget.
    pub power: WidgetStyleConfig,
}

impl WidgetStyles {
    /// The style table for `kind`.
    fn get(&self, kind: WidgetKind) -> &WidgetStyleConfig {
        match kind {
            WidgetKind::Workspaces => &self.workspaces,
            WidgetKind::Clock => &self.clock,
            WidgetKind::Battery => &self.battery,
            WidgetKind::Backlight => &self.backlight,
            WidgetKind::System => &self.system,
            WidgetKind::Network => &self.network,
            WidgetKind::Tray => &self.tray,
            WidgetKind::Title => &self.title,
            WidgetKind::Bluetooth => &self.bluetooth,
            WidgetKind::Volume => &self.volume,
            WidgetKind::Notifications => &self.notifications,
            WidgetKind::PowerProfilesDaemon => &self.power_profiles_daemon,
            WidgetKind::Updates => &self.updates,
            WidgetKind::Hypridle => &self.hypridle,
            WidgetKind::Power => &self.power,
        }
    }

    /// Fold each widget's set style fields from this override onto `base`,
    /// per-widget and per-field (see [`WidgetStyleConfig::apply`]).
    fn apply(&self, base: &mut WidgetStyles) {
        self.workspaces.apply(&mut base.workspaces);
        self.clock.apply(&mut base.clock);
        self.battery.apply(&mut base.battery);
        self.backlight.apply(&mut base.backlight);
        self.system.apply(&mut base.system);
        self.network.apply(&mut base.network);
        self.tray.apply(&mut base.tray);
        self.title.apply(&mut base.title);
        self.bluetooth.apply(&mut base.bluetooth);
        self.volume.apply(&mut base.volume);
        self.notifications.apply(&mut base.notifications);
        self.power_profiles_daemon
            .apply(&mut base.power_profiles_daemon);
        self.updates.apply(&mut base.updates);
        self.hypridle.apply(&mut base.hypridle);
        self.power.apply(&mut base.power);
    }
}

/// Map an `icon` config value to its resolved [`IconSetting`]: absent keeps the
/// widget's built-in glyph, `"none"` disables it, any other string overrides it.
fn resolve_icon(icon: Option<&str>) -> IconSetting {
    match icon {
        None => IconSetting::Default,
        Some("none") => IconSetting::None,
        Some(custom) => IconSetting::Custom(custom.to_string()),
    }
}

/// Fold a [`StateColorConfig`] onto resolved defaults, taking each set channel
/// and inheriting the rest.
fn resolve_state(config: StateColorConfig, default: StateColors) -> StateColors {
    StateColors {
        background: match config.background {
            Some(color) => Some(color.to_rgba()),
            None => default.background,
        },
        foreground: config
            .foreground
            .map(Color::to_rgba)
            .unwrap_or(default.foreground),
        border: config.border.map(Color::to_rgba).or(default.border),
    }
}

/// Per-channel theme overrides for one monitor.
///
/// Each channel is optional: a set channel replaces the base theme's, an unset
/// one inherits it. This field-level merge is why a monitor that overrides only
/// `background` keeps the global `foreground` and `accent` rather than resetting
/// them to the [`Theme`] defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeOverride {
    /// Background fill, if overridden.
    pub background: Option<Color>,
    /// Text color, if overridden.
    pub foreground: Option<Color>,
    /// Emphasis color, if overridden.
    pub accent: Option<Color>,
}

impl ThemeOverride {
    /// Apply the set channels onto `base`, leaving unset ones untouched.
    fn apply(self, base: &mut Theme) {
        if let Some(background) = self.background {
            base.background = background;
        }
        if let Some(foreground) = self.foreground {
            base.foreground = foreground;
        }
        if let Some(accent) = self.accent {
            base.accent = accent;
        }
    }
}

/// Per-field font overrides for one monitor (see [`ThemeOverride`] for the
/// inherit-when-unset rationale).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontOverride {
    /// Font family, if overridden.
    pub family: Option<String>,
    /// Text size, if overridden.
    pub size: Option<f32>,
}

impl FontOverride {
    /// Apply the set fields onto `base`, leaving unset ones untouched.
    fn apply(&self, base: &mut Font) {
        if let Some(family) = &self.family {
            base.family = Some(family.clone());
        }
        if let Some(size) = self.size {
            base.size = size;
        }
    }
}

/// Per-field bar overrides for one monitor: a `[monitor.bar]` table.
///
/// Every field is optional (see [`ThemeOverride`] for the inherit-when-unset
/// rationale). A set zone list *replaces* the global one wholesale — a monitor
/// states the entire zone it wants, not a delta — while an unset zone inherits
/// the global layout. Spacing and background follow the same set-replaces rule.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BarOverride {
    /// Bar background, if overridden.
    pub background: Option<Color>,
    /// Pill-row inset, if overridden.
    pub margin: Option<u32>,
    /// Inter-pill gap, if overridden.
    pub gap: Option<u32>,
    /// Left zone, if overridden (replaces the global list wholesale).
    #[serde(rename = "modules-left")]
    pub modules_left: Option<Vec<WidgetKind>>,
    /// Center zone, if overridden (replaces the global list wholesale).
    #[serde(rename = "modules-center")]
    pub modules_center: Option<Vec<WidgetKind>>,
    /// Right zone, if overridden (replaces the global list wholesale).
    #[serde(rename = "modules-right")]
    pub modules_right: Option<Vec<WidgetKind>>,
}

impl BarOverride {
    /// Apply the set fields onto `base`, leaving unset ones untouched.
    fn apply(&self, base: &mut Bar) {
        if self.background.is_some() {
            base.background = self.background;
        }
        if let Some(margin) = self.margin {
            base.margin = margin;
        }
        if let Some(gap) = self.gap {
            base.gap = gap;
        }
        if let Some(left) = &self.modules_left {
            base.modules_left = left.clone();
        }
        if let Some(center) = &self.modules_center {
            base.modules_center = center.clone();
        }
        if let Some(right) = &self.modules_right {
            base.modules_right = right.clone();
        }
    }
}

/// A per-monitor configuration override, matched to an output by connector name.
///
/// Every field except [`name`](MonitorConfig::name) is optional and inherits the
/// global [`Config`] when unset, so a monitor entry only states what differs.
/// Resolved by [`Config::resolve_for_output`].
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    /// The output connector name this override applies to (e.g. `"DP-1"`).
    pub name: String,
    /// Bar height override.
    pub height: Option<u32>,
    /// Theme channel overrides.
    pub theme: ThemeOverride,
    /// Font field overrides.
    pub font: FontOverride,
    /// Bar layout overrides: per-zone module lists, spacing, and background.
    pub bar: BarOverride,
    /// Per-widget style overrides, folded onto the global widget tables.
    pub widget: WidgetStyles,
}

/// The fully-resolved bar configuration.
///
/// Build one with [`from_toml_str`](Config::from_toml_str) or
/// [`load_from_path`](Config::load_from_path); [`Config::default`] is the
/// documented baseline every field falls back to. On a multi-monitor setup,
/// [`resolve_for_output`](Config::resolve_for_output) folds any matching
/// [`MonitorConfig`] into a per-output config.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Bar height in pixels.
    pub height: u32,
    /// Color theme.
    pub theme: Theme,
    /// Font selection.
    pub font: Font,
    /// Bar layout: zone module lists, spacing, and optional background.
    pub bar: Bar,
    /// Per-widget style tables.
    pub widget: WidgetStyles,
    /// Per-monitor overrides, matched to outputs by connector name.
    #[serde(rename = "monitor")]
    pub monitors: Vec<MonitorConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            height: DEFAULT_HEIGHT,
            theme: Theme::default(),
            font: Font::default(),
            bar: Bar::default(),
            widget: WidgetStyles::default(),
            monitors: Vec::new(),
        }
    }
}

impl Config {
    /// Parse a configuration from a TOML document.
    ///
    /// Missing fields fall back to their documented defaults; unknown keys,
    /// unknown widget names, and malformed colors are reported as errors.
    pub fn from_toml_str(toml: &str) -> Result<Config, ConfigError> {
        let config: Config =
            toml::from_str(toml).map_err(|source| ConfigError::Parse { path: None, source })?;
        config.validate().map_err(|message| ConfigError::Invalid {
            path: None,
            message,
        })?;
        Ok(config)
    }

    /// Load a configuration from a TOML file.
    ///
    /// A missing file is not an error — it resolves to [`Config::default`], so a
    /// first run with no config still works. A file that exists but cannot be
    /// read or parsed is a hard [`ConfigError`]: invalid configuration never
    /// silently degrades to defaults.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
        let path = path.as_ref();
        match fs::read_to_string(path) {
            Ok(text) => {
                let config: Config =
                    toml::from_str(&text).map_err(|source| ConfigError::Parse {
                        path: Some(path.to_path_buf()),
                        source,
                    })?;
                config.validate().map_err(|message| ConfigError::Invalid {
                    path: Some(path.to_path_buf()),
                    message,
                })?;
                Ok(config)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Validate the resolved values, rejecting ones that deserialize fine but
    /// would render a blank or broken bar: an out-of-range height or font size,
    /// an absurd bar margin or gap, or a `[[monitor]]` override with no name (it
    /// can never match an output). Returns a message naming the first offending
    /// field.
    ///
    /// Called from both parse entry points so a present-but-invalid config is
    /// fatal at startup rather than silently producing an unusable bar.
    pub fn validate(&self) -> Result<(), String> {
        validate_range("height", self.height, MIN_HEIGHT, MAX_HEIGHT)?;
        validate_font_size("font.size", self.font.size)?;
        validate_range("bar.margin", self.bar.margin, 0, MAX_GAP)?;
        validate_range("bar.gap", self.bar.gap, 0, MAX_GAP)?;
        validate_battery_config("widget.battery", &self.widget.battery)?;
        validate_backlight_config("widget.backlight", &self.widget.backlight)?;
        validate_power_profiles_config(
            "widget.power-profiles-daemon",
            &self.widget.power_profiles_daemon,
        )?;
        validate_updates_config("widget.updates", &self.widget.updates)?;

        for monitor in &self.monitors {
            if monitor.name.trim().is_empty() {
                return Err(
                    "a [[monitor]] override has an empty `name`; it can never match an output"
                        .to_string(),
                );
            }
            let who = monitor.name.as_str();
            if let Some(height) = monitor.height {
                validate_range(
                    &format!("monitor {who:?} height"),
                    height,
                    MIN_HEIGHT,
                    MAX_HEIGHT,
                )?;
            }
            if let Some(size) = monitor.font.size {
                validate_font_size(&format!("monitor {who:?} font.size"), size)?;
            }
            if let Some(margin) = monitor.bar.margin {
                validate_range(&format!("monitor {who:?} bar.margin"), margin, 0, MAX_GAP)?;
            }
            if let Some(gap) = monitor.bar.gap {
                validate_range(&format!("monitor {who:?} bar.gap"), gap, 0, MAX_GAP)?;
            }
            validate_backlight_config(
                &format!("monitor {who:?} widget.backlight"),
                &monitor.widget.backlight,
            )?;
            validate_battery_config(
                &format!("monitor {who:?} widget.battery"),
                &monitor.widget.battery,
            )?;
            validate_power_profiles_config(
                &format!("monitor {who:?} widget.power-profiles-daemon"),
                &monitor.widget.power_profiles_daemon,
            )?;
            validate_updates_config(
                &format!("monitor {who:?} widget.updates"),
                &monitor.widget.updates,
            )?;
        }
        Ok(())
    }

    /// The widgets each zone builds, with duplicates removed across *all three*
    /// zones (first occurrence wins).
    ///
    /// A widget is a single live instance, so naming it in two zones would
    /// otherwise build it twice; cross-zone de-duplication keeps the first
    /// mention and drops the rest, mirroring the within-zone rule. This is the
    /// single zone resolution both [`build_dashboard`](Config::build_dashboard)
    /// and the tests rely on.
    pub fn resolved_zones(&self) -> ResolvedZones {
        let mut seen: Vec<WidgetKind> = Vec::new();
        let left = dedup_zone(&self.bar.modules_left, &mut seen);
        let center = dedup_zone(&self.bar.modules_center, &mut seen);
        let right = dedup_zone(&self.bar.modules_right, &mut seen);
        ResolvedZones {
            left,
            center,
            right,
        }
    }

    /// Whether a widget appears in the global layout or any monitor override.
    pub fn uses_widget(&self, kind: WidgetKind) -> bool {
        let bar_contains = |bar: &Bar| {
            bar.modules_left.contains(&kind)
                || bar.modules_center.contains(&kind)
                || bar.modules_right.contains(&kind)
        };
        bar_contains(&self.bar)
            || self.monitors.iter().any(|monitor| {
                monitor
                    .bar
                    .modules_left
                    .as_ref()
                    .is_some_and(|zone| zone.contains(&kind))
                    || monitor
                        .bar
                        .modules_center
                        .as_ref()
                        .is_some_and(|zone| zone.contains(&kind))
                    || monitor
                        .bar
                        .modules_right
                        .as_ref()
                        .is_some_and(|zone| zone.contains(&kind))
            })
    }

    /// Resolve the effective config for one output, by connector name.
    ///
    /// The first [`MonitorConfig`] whose `name` matches `output` is folded onto a
    /// clone of the base config: each `Some` override replaces the corresponding
    /// field, each unset one inherits the global value (field-level merge — see
    /// [`ThemeOverride`]). When `output` is `None`, or no monitor entry matches,
    /// the global defaults stand. The returned config carries an empty
    /// [`monitors`](Config::monitors) list — it is already specialized for one
    /// output and is not resolved again.
    pub fn resolve_for_output(&self, output: Option<&str>) -> Config {
        let mut resolved = self.clone();
        resolved.monitors = Vec::new();

        let Some(name) = output else {
            return resolved;
        };
        let Some(monitor) = self.monitors.iter().find(|m| m.name == name) else {
            return resolved;
        };

        if let Some(height) = monitor.height {
            resolved.height = height;
        }
        monitor.theme.apply(&mut resolved.theme);
        monitor.font.apply(&mut resolved.font);
        monitor.bar.apply(&mut resolved.bar);
        monitor.widget.apply(&mut resolved.widget);

        resolved
    }
}

/// The widgets to build for each zone, after cross-zone de-duplication.
///
/// Returned by [`Config::resolved_zones`] and consumed by
/// [`Config::build_dashboard`]; each list is in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedZones {
    /// Widgets clustered at the left edge.
    pub left: Vec<WidgetKind>,
    /// Widgets centered in the bar.
    pub center: Vec<WidgetKind>,
    /// Widgets clustered at the right edge.
    pub right: Vec<WidgetKind>,
}

/// Append the not-yet-seen widgets of `zone` to a fresh list, recording each in
/// `seen` so later zones skip any already placed (first-mention-wins).
fn dedup_zone(zone: &[WidgetKind], seen: &mut Vec<WidgetKind>) -> Vec<WidgetKind> {
    let mut resolved = Vec::new();
    for &kind in zone {
        if !seen.contains(&kind) {
            seen.push(kind);
            resolved.push(kind);
        }
    }
    resolved
}

/// Reject a `u32` field outside `min..=max`, with a message naming the field.
fn validate_range(field: &str, value: u32, min: u32, max: u32) -> Result<(), String> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(format!(
            "{field} must be between {min} and {max}, got {value}"
        ))
    }
}

/// Reject a font size that is not a finite number within the accepted range.
fn validate_font_size(field: &str, size: f32) -> Result<(), String> {
    if size.is_finite() && (MIN_FONT_SIZE..=MAX_FONT_SIZE).contains(&size) {
        Ok(())
    } else {
        Err(format!(
            "{field} must be a finite number between {MIN_FONT_SIZE} and {MAX_FONT_SIZE}, got {size}"
        ))
    }
}

fn validate_backlight_config(field: &str, config: &WidgetStyleConfig) -> Result<(), String> {
    if let Some(format) = &config.format {
        validate_backlight_format(format).map_err(|error| format!("{field}.format {error}"))?;
    }
    if let Some(icons) = &config.format_icons {
        let Some(icons) = icons.ramp() else {
            return Err(format!("{field}.format-icons must be an icon array"));
        };
        if icons.is_empty() || icons.iter().any(String::is_empty) {
            return Err(format!("{field}.format-icons must contain non-empty icons"));
        }
    }
    if let Some(step) = config.scroll_step
        && (!step.is_finite() || step <= 0.0 || step > 100.0)
    {
        return Err(format!(
            "{field}.scroll-step must be a finite number between 0 and 100, got {step}"
        ));
    }
    if let Some(device) = &config.device
        && (device.trim().is_empty() || device.contains('/') || device == "." || device == "..")
    {
        return Err(format!(
            "{field}.device must be a non-empty sysfs device name, got {device:?}"
        ));
    }
    Ok(())
}

fn validate_battery_config(field: &str, config: &WidgetStyleConfig) -> Result<(), String> {
    if let Some(format) = &config.format {
        validate_battery_format(format).map_err(|error| format!("{field}.format {error}"))?;
    }
    Ok(())
}

fn validate_power_profiles_config(field: &str, config: &WidgetStyleConfig) -> Result<(), String> {
    if let Some(format) = &config.format {
        validate_power_profiles_format(format)
            .map_err(|error| format!("{field}.format {error}"))?;
    }
    if let Some(format) = &config.tooltip_format {
        validate_power_profiles_format(format)
            .map_err(|error| format!("{field}.tooltip-format {error}"))?;
    }
    if let Some(icons) = &config.format_icons {
        let Some(icons) = icons.named() else {
            return Err(format!("{field}.format-icons must be a named icon table"));
        };
        if icons.is_empty()
            || icons
                .iter()
                .any(|(name, icon)| name.trim().is_empty() || icon.is_empty())
        {
            return Err(format!(
                "{field}.format-icons must contain non-empty names and icons"
            ));
        }
    }
    Ok(())
}

fn validate_updates_config(field: &str, config: &WidgetStyleConfig) -> Result<(), String> {
    if let Some(format) = &config.format {
        validate_updates_format(format).map_err(|error| format!("{field}.format {error}"))?;
    }
    Ok(())
}

impl WidgetKind {
    /// Construct the widget this kind names, occupying `bounds` and carrying its
    /// resolved `style`.
    ///
    /// The bounds are a placeholder seed; real per-column slots are assigned by
    /// [`Dashboard::layout`] each frame. `monitor` is the connector name of the
    /// output this dashboard serves, threaded to the workspace widget so it
    /// shows only that monitor's workspaces; `None` builds the global fallback.
    /// `on_click` is the executable path the widget spawns when clicked —
    /// `None` makes the widget display-only; today only the bluetooth and
    /// volume widgets honor it, but every kind accepts the argument so
    /// per-monitor `on-click` overrides apply uniformly.
    pub fn build(
        self,
        bounds: Bounds,
        style: WidgetStyle,
        monitor: Option<&str>,
        on_click: Option<std::path::PathBuf>,
    ) -> Box<dyn Widget> {
        match self {
            WidgetKind::Workspaces => match monitor {
                Some(name) => {
                    Box::new(WorkspaceWidget::for_monitor(bounds, name).with_style(style))
                }
                None => Box::new(WorkspaceWidget::new(bounds).with_style(style)),
            },
            WidgetKind::Clock => Box::new(ClockWidget::new(bounds).with_style(style)),
            WidgetKind::Battery => Box::new(BatteryWidget::new(bounds).with_style(style)),
            WidgetKind::Backlight => Box::new(BacklightWidget::new(bounds).with_style(style)),
            WidgetKind::System => Box::new(SystemWidget::new(bounds).with_style(style)),
            WidgetKind::Network => Box::new(NetworkWidget::new(bounds).with_style(style)),
            WidgetKind::Tray => Box::new(TrayWidget::new(bounds).with_style(style)),
            WidgetKind::Title => Box::new(
                TitleWidget::new(bounds)
                    .with_style(style)
                    .with_monitor(monitor.unwrap_or("")),
            ),
            WidgetKind::Bluetooth => Box::new(
                BluetoothWidget::new(bounds)
                    .with_style(style)
                    .with_on_click(on_click),
            ),
            WidgetKind::Volume => Box::new(
                VolumeWidget::new(bounds)
                    .with_style(style)
                    .with_on_click(on_click),
            ),
            // Clicks are typed swaync commands, not a spawned program, so the
            // per-widget `on-click` path does not apply here.
            WidgetKind::Notifications => {
                Box::new(NotificationsWidget::new(bounds).with_style(style))
            }
            WidgetKind::PowerProfilesDaemon => {
                Box::new(PowerProfilesWidget::new(bounds).with_style(style))
            }
            WidgetKind::Updates => Box::new(
                UpdatesWidget::new(bounds)
                    .with_style(style)
                    .with_on_click(on_click),
            ),
            WidgetKind::Hypridle => Box::new(HypridleWidget::new(bounds).with_style(style)),
            WidgetKind::Power => Box::new(
                PowerWidget::new(bounds)
                    .with_style(style)
                    .with_on_click(on_click),
            ),
        }
    }
}

impl Config {
    /// The render settings (theme colors and font) this configuration resolves to.
    ///
    /// Sizes are logical pixels — the values as written in the config. For a
    /// scaled output, resolve physical settings with
    /// [`scaled_render_settings`](Config::scaled_render_settings).
    pub fn render_settings(&self) -> RenderSettings {
        RenderSettings {
            background: self
                .bar
                .background
                .unwrap_or(self.theme.background)
                .to_rgba(),
            foreground: self.theme.foreground.to_rgba(),
            accent: self.theme.accent.to_rgba(),
            font_size: self.font.size,
            font_family: self.font.family.clone(),
            scale: 1,
        }
    }

    /// The render settings for an output at `scale`, with the font size resolved
    /// to physical pixels.
    ///
    /// Only the font size scales; colors and family are scale-independent. At
    /// [`Scale::ONE`] this equals [`render_settings`](Config::render_settings),
    /// so the unscaled path is unchanged. The font is scaled here, once, and the
    /// layout is scaled separately at the surface boundary — the two never
    /// compound, which is what keeps text from being double-scaled.
    pub fn scaled_render_settings(&self, scale: Scale) -> RenderSettings {
        RenderSettings {
            font_size: scale.scale_font(self.font.size),
            scale: scale.get(),
            ..self.render_settings()
        }
    }

    /// The bar's physical height in device pixels for an output at `scale`.
    ///
    /// The configured [`height`](Config::height) is logical, so the surface keeps
    /// a consistent apparent size across scales: 32 logical px is 32 device px at
    /// scale 1 and 64 at scale 2, matching the higher pixel density. This is the
    /// buffer/render-target height; the layer-shell size request stays logical.
    pub fn physical_height(&self, scale: Scale) -> u32 {
        scale.to_physical(self.height)
    }

    /// Build the dashboard this configuration describes for one output.
    ///
    /// Each zone's widgets are constructed in order, seeded at `bounds` and
    /// carrying their resolved per-widget [`WidgetStyle`], and the dashboard
    /// carries the bar margin and inter-pill gap so its
    /// [`layout`](Dashboard::layout) packs the three zones as configured.
    /// `monitor` is the output's connector name, threaded to the workspace
    /// widget so it shows only that monitor's workspaces; pass `None` for the
    /// global fallback.
    pub fn build_dashboard(&self, bounds: Bounds, monitor: Option<&str>) -> Dashboard {
        let zones = self.resolved_zones();
        let build_zone = |kinds: &[WidgetKind]| -> Vec<Box<dyn Widget>> {
            kinds
                .iter()
                .map(|&kind| {
                    let style_config = self.widget.get(kind);
                    let style = style_config.resolve(&self.theme);
                    match kind {
                        WidgetKind::Battery => Box::new(
                            BatteryWidget::new(bounds)
                                .with_style(style)
                                .with_format(style_config.format.clone()),
                        ) as Box<dyn Widget>,
                        WidgetKind::Backlight => Box::new(
                            BacklightWidget::new(bounds)
                                .with_style(style)
                                .with_device(style_config.device.clone())
                                .with_format(style_config.format.clone())
                                .with_format_icons(
                                    style_config
                                        .format_icons
                                        .as_ref()
                                        .and_then(FormatIconsConfig::ramp)
                                        .cloned(),
                                )
                                .with_scroll_step(style_config.scroll_step),
                        ) as Box<dyn Widget>,
                        WidgetKind::PowerProfilesDaemon => Box::new(
                            PowerProfilesWidget::new(bounds)
                                .with_style(style)
                                .with_format(style_config.format.clone())
                                .with_format_icons(
                                    style_config
                                        .format_icons
                                        .as_ref()
                                        .and_then(FormatIconsConfig::named)
                                        .cloned(),
                                )
                                .with_tooltip(style_config.tooltip)
                                .with_tooltip_format(style_config.tooltip_format.clone()),
                        ),
                        WidgetKind::Updates => Box::new(
                            UpdatesWidget::new(bounds)
                                .with_style(style)
                                .with_format(style_config.format.clone())
                                .with_on_click(style_config.on_click.clone()),
                        ),
                        WidgetKind::Power => Box::new(
                            PowerWidget::new(bounds)
                                .with_style(style)
                                .with_on_click(style_config.on_click.clone())
                                .with_on_click_right(style_config.on_click_right.clone()),
                        ),
                        _ => kind.build(bounds, style, monitor, style_config.on_click.clone()),
                    }
                })
                .collect()
        };
        let left = build_zone(&zones.left);
        let center = build_zone(&zones.center);
        let right = build_zone(&zones.right);
        Dashboard::with_zones(left, center, right).with_spacing(self.bar.margin, self.bar.gap)
    }
}

/// Why a configuration could not be loaded.
#[derive(Debug)]
pub enum ConfigError {
    /// The config file exists but could not be read.
    Read {
        /// The file that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: io::Error,
    },
    /// The config document could not be parsed against the schema.
    Parse {
        /// The file the document came from, if it was read from disk.
        path: Option<PathBuf>,
        /// The underlying TOML error (carries line/column and a message).
        source: toml::de::Error,
    },
    /// The document parsed but holds a value that would render a broken bar.
    Invalid {
        /// The file the document came from, if it was read from disk.
        path: Option<PathBuf>,
        /// What was wrong, naming the offending field.
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => {
                write!(f, "failed to read config {}: {source}", path.display())
            }
            ConfigError::Parse {
                path: Some(path),
                source,
            } => write!(f, "invalid config {}: {source}", path.display()),
            ConfigError::Parse { path: None, source } => {
                write!(f, "invalid config: {source}")
            }
            ConfigError::Invalid {
                path: Some(path),
                message,
            } => write!(f, "invalid config {}: {message}", path.display()),
            ConfigError::Invalid {
                path: None,
                message,
            } => write!(f, "invalid config: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Read { source, .. } => Some(source),
            ConfigError::Parse { source, .. } => Some(source),
            ConfigError::Invalid { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::RenderContext;
    use crate::widget::ClickButton;

    #[test]
    fn default_is_the_documented_baseline() {
        let config = Config::default();
        assert_eq!(config.height, 32);
        assert_eq!(config.bar.margin, 0);
        assert_eq!(config.bar.gap, 0);
        assert_eq!(config.bar.background, None);
        assert_eq!(config.theme.background, Color::rgb(0x18, 0x18, 0x18));
        assert_eq!(config.theme.foreground, Color::rgb(0xEA, 0xEA, 0xEA));
        assert_eq!(config.theme.accent, Color::rgb(0xEA, 0xEA, 0xEA));
        assert_eq!(config.font.family, None);
        assert_eq!(config.font.size, 16.0);
        // The default zones: workspaces left, title centered, the time-and-status
        // cluster right (clock first, then battery/system/network and power
        // profiles); the tray stays opt-in.
        assert_eq!(config.bar.modules_left, vec![WidgetKind::Workspaces]);
        assert_eq!(config.bar.modules_center, vec![WidgetKind::Title]);
        assert_eq!(
            config.bar.modules_right,
            vec![
                WidgetKind::Clock,
                WidgetKind::Battery,
                WidgetKind::System,
                WidgetKind::Network,
                WidgetKind::PowerProfilesDaemon,
            ],
        );
    }

    #[test]
    fn empty_document_is_all_defaults() {
        // A blank config is valid: every field falls back to its default.
        assert_eq!(Config::from_toml_str("").unwrap(), Config::default());
    }

    #[test]
    fn the_shipped_example_config_parses_as_the_themed_desktop_loadout() {
        // The shipped file is deliberately opinionated, but must remain valid and
        // retain the defining layout and palette of the documented preset.
        let example = include_str!("../config.example.toml");
        let parsed = Config::from_toml_str(example).expect("example config parses");
        assert_eq!(parsed.height, 38);
        assert_eq!(parsed.theme.background, Color::rgb(0x14, 0x09, 0x17));
        assert_eq!(parsed.theme.foreground, Color::rgb(0xEA, 0xE2, 0xCF));
        assert_eq!(parsed.theme.accent, Color::rgb(0x7D, 0xEC, 0xFF));
        assert_eq!(
            parsed.bar.background,
            Some(Color::rgba(0x00, 0x00, 0x00, 0x00))
        );
        assert_eq!(
            parsed.bar.modules_left,
            vec![WidgetKind::Workspaces, WidgetKind::Title]
        );
        assert!(parsed.bar.modules_center.is_empty());
        assert_eq!(
            parsed.bar.modules_right,
            vec![
                WidgetKind::Updates,
                WidgetKind::Bluetooth,
                WidgetKind::Backlight,
                WidgetKind::Volume,
                WidgetKind::System,
                WidgetKind::Network,
                WidgetKind::Tray,
                WidgetKind::Notifications,
                WidgetKind::Battery,
                WidgetKind::PowerProfilesDaemon,
                WidgetKind::Hypridle,
                WidgetKind::Power,
                WidgetKind::Clock,
            ]
        );
        assert_eq!(
            parsed.widget.workspaces.border,
            Some(Color::rgb(0x3D, 0x1F, 0x3A))
        );
        assert_eq!(parsed.widget.workspaces.border_width, Some(1));
        assert_eq!(
            parsed.widget.updates.format.as_deref(),
            Some("{icon} {count}")
        );
        assert_eq!(parsed.widget.updates.padding, Some(6));
        assert_eq!(
            parsed.widget.power.on_click.as_deref(),
            Some(std::path::Path::new("wlogout"))
        );
        assert_eq!(
            parsed.widget.power.on_click_right.as_deref(),
            Some(std::path::Path::new("hyprlock"))
        );
        assert_eq!(
            parsed.widget.battery.format.as_deref(),
            Some("{icon} {percent}%")
        );
        assert_eq!(
            parsed.widget.battery.charging.foreground,
            Some(Color::rgb(0x5F, 0xF5, 0xA0))
        );
        assert_eq!(
            parsed.widget.battery.charging.border,
            Some(Color::rgb(0x2D, 0xF1, 0x85))
        );
    }

    #[test]
    fn partial_document_overrides_only_named_fields() {
        let config = Config::from_toml_str(
            r##"
            height = 40

            [bar]
            margin = 6

            [theme]
            accent = "#ff8800"
            "##,
        )
        .unwrap();

        // Overridden fields take the new value...
        assert_eq!(config.height, 40);
        assert_eq!(config.bar.margin, 6);
        assert_eq!(config.theme.accent, Color::rgb(0xFF, 0x88, 0x00));
        // ...everything unmentioned keeps its default, including the zone lists a
        // partial [bar] table did not restate.
        assert_eq!(config.bar.gap, 0);
        assert_eq!(config.bar.modules_left, vec![WidgetKind::Workspaces]);
        assert_eq!(config.bar.modules_center, vec![WidgetKind::Title]);
        assert_eq!(config.theme.background, Color::rgb(0x18, 0x18, 0x18));
    }

    #[test]
    fn full_document_parses_every_section() {
        let config = Config::from_toml_str(
            r##"
            height = 28

            [bar]
            margin = 4
            gap = 2
            background = "#101010cc"
            modules-left = ["clock"]
            modules-center = []
            modules-right = ["workspaces"]

            [theme]
            background = "#000000"
            foreground = "#ffffff"
            accent = "#00ff00"

            [font]
            family = "JetBrains Mono"
            size = 14.0

            [widget.clock]
            background = "#222222"
            border = "#445566"
            border-width = 2
            radius = 10
            icon = "none"
            "##,
        )
        .unwrap();

        assert_eq!(config.height, 28);
        assert_eq!(config.bar.margin, 4);
        assert_eq!(config.bar.gap, 2);
        assert_eq!(
            config.bar.background,
            Some(Color::rgba(0x10, 0x10, 0x10, 0xCC))
        );
        assert_eq!(config.theme.background, Color::rgb(0, 0, 0));
        assert_eq!(config.theme.foreground, Color::rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(config.theme.accent, Color::rgb(0, 0xFF, 0));
        assert_eq!(config.font.family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(config.font.size, 14.0);
        assert_eq!(config.bar.modules_left, vec![WidgetKind::Clock]);
        assert!(config.bar.modules_center.is_empty());
        assert_eq!(config.bar.modules_right, vec![WidgetKind::Workspaces]);
        // The per-widget style table is captured verbatim.
        assert_eq!(
            config.widget.clock.background,
            Some(Color::rgb(0x22, 0x22, 0x22))
        );
        assert_eq!(config.widget.clock.radius, Some(10));
        assert_eq!(
            config.widget.clock.border,
            Some(Color::rgb(0x44, 0x55, 0x66))
        );
        assert_eq!(config.widget.clock.border_width, Some(2));
        assert_eq!(config.widget.clock.icon.as_deref(), Some("none"));
    }

    #[test]
    fn color_accepts_hash_and_bare_hex() {
        assert_eq!(
            Color::parse_hex("#1a2b3c").unwrap(),
            Color::rgb(0x1A, 0x2B, 0x3C)
        );
        assert_eq!(
            Color::parse_hex("1a2b3c").unwrap(),
            Color::rgb(0x1A, 0x2B, 0x3C)
        );
        // Six digits are opaque: alpha defaults to 0xFF.
        assert_eq!(Color::parse_hex("#1a2b3c").unwrap().a, 0xFF);
        // Eight digits carry an explicit alpha channel.
        assert_eq!(
            Color::parse_hex("#1a2b3c80").unwrap(),
            Color::rgba(0x1A, 0x2B, 0x3C, 0x80)
        );
        assert_eq!(
            Color::parse_hex("00ff0040").unwrap(),
            Color::rgba(0x00, 0xFF, 0x00, 0x40)
        );
    }

    #[test]
    fn color_rejects_malformed_hex_with_a_clear_message() {
        // Wrong length, non-hex digits, and empty all fail loudly.
        for bad in ["#fff", "#12345g", "not-a-color", ""] {
            let err = Color::parse_hex(bad).unwrap_err();
            assert!(err.contains("expected"), "unhelpful message: {err}");
        }
    }

    #[test]
    fn invalid_color_in_document_is_an_error_not_a_default() {
        let err = Config::from_toml_str(
            r##"
            [theme]
            foreground = "#xyzxyz"
            "##,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid config"), "message: {msg}");
    }

    #[test]
    fn tray_is_an_opt_in_widget_name() {
        // The tray is in none of the default zones, but naming it in one is valid
        // and builds.
        let bar = Bar::default();
        let in_default_zones = bar
            .modules_left
            .iter()
            .chain(&bar.modules_center)
            .chain(&bar.modules_right)
            .any(|&kind| kind == WidgetKind::Tray);
        assert!(!in_default_zones);

        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-right = ["tray", "clock"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.bar.modules_right,
            vec![WidgetKind::Tray, WidgetKind::Clock]
        );

        // It constructs a widget without panicking.
        let _ = WidgetKind::Tray.build(
            Bounds::new(0, 0, 64, 32),
            WidgetStyle::default(),
            None,
            None,
        );
    }

    #[test]
    fn updates_is_an_opt_in_widget_name() {
        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-right = ["updates", "clock"]

            [widget.updates]
            format = "{count} {icon}"
            on-click = "/usr/local/bin/update-system"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.bar.modules_right,
            vec![WidgetKind::Updates, WidgetKind::Clock]
        );
        assert_eq!(
            config.widget.updates.format.as_deref(),
            Some("{count} {icon}")
        );
        assert_eq!(
            config.widget.updates.on_click.as_deref(),
            Some(std::path::Path::new("/usr/local/bin/update-system"))
        );
    }

    #[test]
    fn uses_widget_checks_global_and_monitor_zones() {
        let global = Config::from_toml_str(
            r#"
            [bar]
            modules-right = ["updates"]
            "#,
        )
        .unwrap();
        assert!(global.uses_widget(WidgetKind::Updates));

        let monitor = Config::from_toml_str(
            r#"
            [[monitor]]
            name = "eDP-1"
            [monitor.bar]
            modules-right = ["updates"]
            "#,
        )
        .unwrap();
        assert!(monitor.uses_widget(WidgetKind::Updates));
        assert!(!Config::default().uses_widget(WidgetKind::Updates));
    }

    #[test]
    fn hypridle_and_power_are_opt_in_widget_names() {
        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-right = ["hypridle", "power", "clock"]

            [widget.power]
            on-click = "wlogout"
            on-click-right = "hyprlock"
            "#,
        )
        .unwrap();

        assert_eq!(
            config.bar.modules_right,
            vec![WidgetKind::Hypridle, WidgetKind::Power, WidgetKind::Clock]
        );
        assert_eq!(
            config.widget.power.on_click_right.as_deref(),
            Some(std::path::Path::new("hyprlock"))
        );
        assert!(config.uses_widget(WidgetKind::Hypridle));
        assert!(config.uses_widget(WidgetKind::Power));
    }

    #[test]
    fn power_actions_reach_the_built_dashboard() {
        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-left = []
            modules-center = []
            modules-right = ["power"]

            [widget.power]
            on-click = "wlogout"
            on-click-right = "hyprlock"
            "#,
        )
        .unwrap();
        let mut dashboard = config.build_dashboard(Bounds::new(0, 0, 100, 32), None);
        let mut ctx = RenderContext::new(100, 32);
        dashboard.layout(&mut ctx, 100, 32);

        assert_eq!(
            dashboard.on_click(99, 16, ClickButton::Left),
            Some(crate::widget::Command::RunProgram("wlogout".into()))
        );
        assert_eq!(
            dashboard.on_click(99, 16, ClickButton::Right),
            Some(crate::widget::Command::RunProgram("hyprlock".into()))
        );
    }

    #[test]
    fn on_click_right_folds_onto_a_monitor_override() {
        let config = Config::from_toml_str(
            r#"
            [widget.power]
            on-click-right = "global-lock"

            [[monitor]]
            name = "DP-1"
            [monitor.widget.power]
            on-click-right = "monitor-lock"
            "#,
        )
        .unwrap();

        assert_eq!(
            config
                .resolve_for_output(Some("DP-1"))
                .widget
                .power
                .on_click_right
                .as_deref(),
            Some(std::path::Path::new("monitor-lock"))
        );
        assert_eq!(
            config
                .resolve_for_output(Some("HDMI-A-1"))
                .widget
                .power
                .on_click_right
                .as_deref(),
            Some(std::path::Path::new("global-lock"))
        );
    }

    #[test]
    fn unknown_widget_name_is_an_error() {
        let err = Config::from_toml_str(
            r#"
            [bar]
            modules-left = ["clock", "weather"]
            "#,
        )
        .unwrap_err();
        // serde reports the unknown variant; the loader wraps it as invalid config.
        assert!(err.to_string().contains("invalid config"));
    }

    #[test]
    fn bluetooth_is_an_opt_in_widget_name_and_builds() {
        // The bluetooth widget is not in any default zone, but naming it in one
        // is valid and builds — matching the tray pattern. An on-click path is
        // also accepted on the widget table and threaded through to the widget.
        let bar = Bar::default();
        let in_default_zones = bar
            .modules_left
            .iter()
            .chain(&bar.modules_center)
            .chain(&bar.modules_right)
            .any(|&kind| kind == WidgetKind::Bluetooth);
        assert!(!in_default_zones);

        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-right = ["bluetooth", "clock"]
            [widget.bluetooth]
            on-click = "/usr/bin/blueman-manager"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.bar.modules_right,
            vec![WidgetKind::Bluetooth, WidgetKind::Clock]
        );
        assert_eq!(
            config.widget.bluetooth.on_click.as_deref(),
            Some(std::path::Path::new("/usr/bin/blueman-manager"))
        );

        // It constructs a widget without panicking, and the configured
        // on-click path reaches the widget unchanged.
        let widget = WidgetKind::Bluetooth.build(
            Bounds::new(0, 0, 64, 32),
            WidgetStyle::default(),
            None,
            config.widget.bluetooth.on_click.clone(),
        );
        let click = widget.on_click(10, 10, ClickButton::Left);
        assert_eq!(
            click,
            Some(crate::widget::Command::RunProgram(
                std::path::PathBuf::from("/usr/bin/blueman-manager")
            ))
        );
    }

    #[test]
    fn bluetooth_with_no_on_click_is_display_only() {
        // A bluetooth widget without an on-click path configured never emits
        // a Command on click, so the host's command fan-out is a no-op for it.
        let widget = WidgetKind::Bluetooth.build(
            Bounds::new(0, 0, 64, 32),
            WidgetStyle::default(),
            None,
            None,
        );
        assert_eq!(widget.on_click(10, 10, ClickButton::Left), None);
    }

    #[test]
    fn bluetooth_on_click_folds_onto_a_monitor_override() {
        // A [monitor.widget.bluetooth] table sets on-click; a per-monitor
        // override merges onto the global table and changes the widget's path.
        let config = Config::from_toml_str(
            r#"
            [widget.bluetooth]
            on-click = "/global/launcher.sh"
            [[monitor]]
            name = "DP-1"
            [monitor.widget.bluetooth]
            on-click = "/per-monitor/launcher.sh"
            "#,
        )
        .unwrap();
        // The global path stays where it was.
        assert_eq!(
            config.widget.bluetooth.on_click.as_deref(),
            Some(std::path::Path::new("/global/launcher.sh"))
        );

        // The resolved config for DP-1 sees the per-monitor override.
        let resolved = config.resolve_for_output(Some("DP-1"));
        assert_eq!(
            resolved.widget.bluetooth.on_click.as_deref(),
            Some(std::path::Path::new("/per-monitor/launcher.sh"))
        );

        // An output with no matching monitor keeps the global on-click.
        let other = config.resolve_for_output(Some("HDMI-A-1"));
        assert_eq!(
            other.widget.bluetooth.on_click.as_deref(),
            Some(std::path::Path::new("/global/launcher.sh"))
        );
    }

    #[test]
    fn volume_is_an_opt_in_widget_name_and_builds() {
        // The volume widget is not in any default zone, but naming it in one
        // is valid and builds — matching the bluetooth / tray pattern. An
        // on-click path is also accepted on the widget table and threaded
        // through to the widget.
        let bar = Bar::default();
        let in_default_zones = bar
            .modules_left
            .iter()
            .chain(&bar.modules_center)
            .chain(&bar.modules_right)
            .any(|&kind| kind == WidgetKind::Volume);
        assert!(!in_default_zones);

        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-right = ["volume", "clock"]
            [widget.volume]
            on-click = "/usr/bin/pavucontrol"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.bar.modules_right,
            vec![WidgetKind::Volume, WidgetKind::Clock]
        );
        assert_eq!(
            config.widget.volume.on_click.as_deref(),
            Some(std::path::Path::new("/usr/bin/pavucontrol"))
        );

        // It constructs a widget without panicking, and the configured
        // on-click path reaches the widget unchanged.
        let widget = WidgetKind::Volume.build(
            Bounds::new(0, 0, 64, 32),
            WidgetStyle::default(),
            None,
            config.widget.volume.on_click.clone(),
        );
        let click = widget.on_click(10, 10, ClickButton::Left);
        assert_eq!(
            click,
            Some(crate::widget::Command::RunProgram(
                std::path::PathBuf::from("/usr/bin/pavucontrol")
            ))
        );
    }

    #[test]
    fn volume_with_no_on_click_is_display_only() {
        // A volume widget without an on-click path configured never emits
        // a Command on click, so the host's command fan-out is a no-op for it.
        let widget = WidgetKind::Volume.build(
            Bounds::new(0, 0, 64, 32),
            WidgetStyle::default(),
            None,
            None,
        );
        assert_eq!(widget.on_click(10, 10, ClickButton::Left), None);
    }

    #[test]
    fn volume_on_click_folds_onto_a_monitor_override() {
        // A [monitor.widget.volume] table sets on-click; a per-monitor
        // override merges onto the global table and changes the widget's path.
        let config = Config::from_toml_str(
            r#"
            [widget.volume]
            on-click = "/global/launcher.sh"
            [[monitor]]
            name = "DP-1"
            [monitor.widget.volume]
            on-click = "/per-monitor/launcher.sh"
            "#,
        )
        .unwrap();
        // The global path stays where it was.
        assert_eq!(
            config.widget.volume.on_click.as_deref(),
            Some(std::path::Path::new("/global/launcher.sh"))
        );

        // The resolved config for DP-1 sees the per-monitor override.
        let resolved = config.resolve_for_output(Some("DP-1"));
        assert_eq!(
            resolved.widget.volume.on_click.as_deref(),
            Some(std::path::Path::new("/per-monitor/launcher.sh"))
        );

        // An output with no matching monitor keeps the global on-click.
        let other = config.resolve_for_output(Some("HDMI-A-1"));
        assert_eq!(
            other.widget.volume.on_click.as_deref(),
            Some(std::path::Path::new("/global/launcher.sh"))
        );
    }

    #[test]
    fn notifications_is_an_opt_in_widget_name_and_builds() {
        // The notifications widget is not in any default zone, but naming it
        // in one is valid and builds — matching the volume / bluetooth
        // pattern. Its clicks are typed swaync commands, so before its first
        // reading (daemon absent) it stays display-only.
        let bar = Bar::default();
        let in_default_zones = bar
            .modules_left
            .iter()
            .chain(&bar.modules_center)
            .chain(&bar.modules_right)
            .any(|&kind| kind == WidgetKind::Notifications);
        assert!(!in_default_zones);

        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-right = ["notifications", "clock"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.bar.modules_right,
            vec![WidgetKind::Notifications, WidgetKind::Clock]
        );

        let widget = WidgetKind::Notifications.build(
            Bounds::new(0, 0, 64, 32),
            WidgetStyle::default(),
            None,
            None,
        );
        assert_eq!(widget.on_click(10, 10, ClickButton::Left), None);
    }

    #[test]
    fn notifications_style_table_resolves_and_folds_per_monitor() {
        // A [widget.notifications] table is accepted and per-monitor overrides
        // fold onto it field-by-field — including the attention colors that
        // drive the pending-notification dot.
        let config = Config::from_toml_str(
            r##"
            [widget.notifications]
            foreground = "#111111"
            [widget.notifications.attention]
            background = "#ff0000"

            [[monitor]]
            name = "DP-1"
            [monitor.widget.notifications]
            foreground = "#222222"
            "##,
        )
        .unwrap();

        let resolved = config.resolve_for_output(Some("DP-1"));
        let notifications = &resolved.widget.notifications;
        // The per-monitor foreground wins...
        assert_eq!(notifications.foreground, Some(Color::rgb(0x22, 0x22, 0x22)));
        // ...while the untouched attention background is inherited from the
        // global table, not reset.
        assert_eq!(
            notifications.attention.background,
            Some(Color::rgb(0xFF, 0x00, 0x00))
        );
    }

    #[test]
    fn title_is_a_known_widget_name_and_builds() {
        // The title widget is recognized by the schema.
        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-center = ["title"]
            "#,
        )
        .unwrap();
        assert_eq!(config.bar.modules_center, vec![WidgetKind::Title]);

        // And the Unknown-or-known serde arm rejected only the *unknown*
        // name; "title" constructs without panicking.
        let _ = WidgetKind::Title.build(
            Bounds::new(0, 0, 64, 32),
            WidgetStyle::default(),
            None,
            None,
        );
    }

    #[test]
    fn unknown_key_is_rejected_rather_than_ignored() {
        // A typo'd key must fail rather than be silently dropped.
        let err = Config::from_toml_str("heigth = 40").unwrap_err();
        assert!(err.to_string().contains("invalid config"));
    }

    #[test]
    fn resolved_zones_dedupe_across_zones_first_position_wins() {
        // A widget named in two zones is built once, at its first mention; the
        // duplicate in a later zone is dropped.
        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-left = ["clock", "battery", "clock"]
            modules-center = ["battery", "workspaces"]
            modules-right = ["clock", "network"]
            "#,
        )
        .unwrap();
        let zones = config.resolved_zones();
        assert_eq!(zones.left, vec![WidgetKind::Clock, WidgetKind::Battery]);
        // `battery` already appeared on the left, so center keeps only workspaces.
        assert_eq!(zones.center, vec![WidgetKind::Workspaces]);
        // `clock` already appeared, so the right keeps only network.
        assert_eq!(zones.right, vec![WidgetKind::Network]);
    }

    #[test]
    fn resolved_zones_on_empty_zones_are_empty() {
        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-left = []
            modules-center = []
            modules-right = []
            "#,
        )
        .unwrap();
        let zones = config.resolved_zones();
        assert!(zones.left.is_empty());
        assert!(zones.center.is_empty());
        assert!(zones.right.is_empty());
    }

    #[test]
    fn load_from_missing_path_is_defaults_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        assert_eq!(Config::load_from_path(missing).unwrap(), Config::default());
    }

    #[test]
    fn load_from_path_reads_and_parses_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "height = 48\n").unwrap();
        let config = Config::load_from_path(&path).unwrap();
        assert_eq!(config.height, 48);
    }

    #[test]
    fn physical_height_scales_the_logical_height() {
        let config = Config::from_toml_str("height = 30").unwrap();
        // At scale 1 the physical height equals the configured logical height...
        assert_eq!(config.physical_height(Scale::ONE), 30);
        // ...and at higher scales it grows by the integer factor.
        assert_eq!(config.physical_height(Scale::new(2)), 60);
        assert_eq!(config.physical_height(Scale::new(3)), 90);
    }

    #[test]
    fn scaled_render_settings_scale_only_the_font() {
        let config = Config::from_toml_str(
            r##"
            [theme]
            foreground = "#102030"

            [font]
            family = "JetBrains Mono"
            size = 16.0
            "##,
        )
        .unwrap();

        let scaled = config.scaled_render_settings(Scale::new(2));
        // The font size doubles to physical pixels, and the scale factor is
        // recorded so widget geometry (radius/padding/gap) can follow it...
        assert_eq!(scaled.font_size, 32.0);
        assert_eq!(scaled.scale, 2);
        // ...while everything scale-independent is carried verbatim.
        assert_eq!(scaled.foreground, (0x10, 0x20, 0x30, 0xFF));
        assert_eq!(scaled.font_family.as_deref(), Some("JetBrains Mono"));
    }

    #[test]
    fn scaled_render_settings_at_scale_one_equal_the_logical_settings() {
        // The unscaled path must be untouched: scale 1 resolves identically to
        // the plain logical settings.
        let config = Config::from_toml_str("[font]\nsize = 18.0").unwrap();
        assert_eq!(
            config.scaled_render_settings(Scale::ONE),
            config.render_settings()
        );
    }

    #[test]
    fn no_monitor_overrides_resolves_to_the_base_config() {
        // With no [[monitor]] tables, every output resolves to the global config,
        // and the resolved config carries no leftover monitor list.
        let config = Config::from_toml_str(
            r#"
            height = 30
            [bar]
            modules-left = ["clock"]
            "#,
        )
        .unwrap();
        let resolved = config.resolve_for_output(Some("DP-1"));
        assert_eq!(resolved.height, 30);
        assert_eq!(resolved.bar.modules_left, vec![WidgetKind::Clock]);
        assert!(resolved.monitors.is_empty());
    }

    #[test]
    fn an_output_with_no_name_resolves_to_the_base_config() {
        let config = Config::from_toml_str(
            r#"
            height = 30
            [[monitor]]
            name = "DP-1"
            height = 40
            "#,
        )
        .unwrap();
        // An output the compositor gave no connector name falls back to global.
        assert_eq!(config.resolve_for_output(None).height, 30);
    }

    #[test]
    fn a_matching_monitor_overrides_only_its_named_fields() {
        let config = Config::from_toml_str(
            r##"
            height = 32

            [theme]
            background = "#111111"
            foreground = "#eeeeee"

            [[monitor]]
            name = "HDMI-A-1"
            height = 28
            [monitor.theme]
            background = "#000000"
            "##,
        )
        .unwrap();

        let resolved = config.resolve_for_output(Some("HDMI-A-1"));
        // Overridden fields take the monitor's value...
        assert_eq!(resolved.height, 28);
        assert_eq!(resolved.theme.background, Color::rgb(0, 0, 0));
        // ...and fields the monitor left unset inherit from the base config,
        // including theme channels the override did not mention.
        assert_eq!(resolved.theme.foreground, Color::rgb(0xEE, 0xEE, 0xEE));
        assert_eq!(resolved.bar.modules_left, vec![WidgetKind::Workspaces]);
    }

    #[test]
    fn a_non_matching_output_name_resolves_to_the_base_config() {
        let config = Config::from_toml_str(
            r#"
            height = 32
            [[monitor]]
            name = "DP-1"
            height = 50
            "#,
        )
        .unwrap();
        // An output whose name matches no [[monitor]] entry uses global defaults.
        assert_eq!(config.resolve_for_output(Some("eDP-1")).height, 32);
    }

    #[test]
    fn monitor_font_overrides_apply() {
        let config = Config::from_toml_str(
            r##"
            [font]
            size = 16.0

            [[monitor]]
            name = "DP-2"
            [monitor.font]
            size = 20.0
            "##,
        )
        .unwrap();
        let resolved = config.resolve_for_output(Some("DP-2"));
        assert_eq!(resolved.font.size, 20.0);
        // The font family the override did not set still inherits the base.
        assert_eq!(resolved.font.family, None);
    }

    #[test]
    fn monitor_zone_override_replaces_only_the_named_zone() {
        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-left   = ["workspaces"]
            modules-center = ["clock"]
            modules-right  = ["battery"]

            [[monitor]]
            name = "DP-1"
            [monitor.bar]
            modules-center = ["system", "network"]
            "#,
        )
        .unwrap();

        let resolved = config.resolve_for_output(Some("DP-1"));
        // The named zone is replaced wholesale by the monitor's list...
        assert_eq!(
            resolved.bar.modules_center,
            vec![WidgetKind::System, WidgetKind::Network]
        );
        // ...and the zones the monitor left unset inherit the global layout.
        assert_eq!(resolved.bar.modules_left, vec![WidgetKind::Workspaces]);
        assert_eq!(resolved.bar.modules_right, vec![WidgetKind::Battery]);
        // And the replacement reaches the de-duplicated zones the dashboard builds.
        assert_eq!(
            resolved.resolved_zones().center,
            vec![WidgetKind::System, WidgetKind::Network]
        );
    }

    #[test]
    fn monitor_widget_override_merges_per_field_onto_the_global_style() {
        let config = Config::from_toml_str(
            r##"
            [widget.battery]
            foreground = "#eeeeee"
            border = "#445566"
            radius = 6

            [[monitor]]
            name = "DP-1"
            [monitor.widget.battery]
            background = "#bf616aff"
            border-width = 2
            "##,
        )
        .unwrap();

        let resolved = config.resolve_for_output(Some("DP-1"));
        let battery = &resolved.widget.battery;
        // The monitor's set field lands on the resolved widget style...
        assert_eq!(
            battery.background,
            Some(Color::rgba(0xBF, 0x61, 0x6A, 0xFF))
        );
        // ...while the global fields the monitor left unset are preserved, not
        // reset to the bare default.
        assert_eq!(battery.foreground, Some(Color::rgb(0xEE, 0xEE, 0xEE)));
        assert_eq!(battery.border, Some(Color::rgb(0x44, 0x55, 0x66)));
        assert_eq!(battery.border_width, Some(2));
        assert_eq!(battery.radius, Some(6));
        // A widget the monitor never mentioned is untouched by the merge.
        assert_eq!(resolved.widget.clock, WidgetStyleConfig::default());
    }

    #[test]
    fn title_widget_style_folds_onto_per_monitor_override() {
        // The same per-field-merge contract the other widgets enjoy extends
        // to the new title widget.
        let config = Config::from_toml_str(
            r##"
            [widget.title]
            foreground = "#eaeaea"
            radius     = 8

            [[monitor]]
            name = "DP-1"
            [monitor.widget.title]
            background = "#2e3440"
            "##,
        )
        .unwrap();

        let resolved = config.resolve_for_output(Some("DP-1"));
        let title = &resolved.widget.title;
        // Monitor-only field lands...
        assert_eq!(title.background, Some(Color::rgb(0x2E, 0x34, 0x40)));
        // ...global fields the monitor left unset are preserved, not reset.
        assert_eq!(title.foreground, Some(Color::rgb(0xEA, 0xEA, 0xEA)));
        assert_eq!(title.radius, Some(8));
        // A widget the monitor never mentioned is untouched by the merge.
        assert_eq!(resolved.widget.clock, WidgetStyleConfig::default());
    }

    #[test]
    fn monitor_bar_background_and_spacing_override_apply() {
        let config = Config::from_toml_str(
            r##"
            [bar]
            margin = 4
            gap    = 4
            background = "#181818"

            [[monitor]]
            name = "DP-1"
            [monitor.bar]
            background = "#00000080"
            margin = 8
            "##,
        )
        .unwrap();

        let resolved = config.resolve_for_output(Some("DP-1"));
        // The set bar fields take the monitor's values...
        assert_eq!(resolved.bar.margin, 8);
        assert_eq!(resolved.bar.background, Some(Color::rgba(0, 0, 0, 0x80)));
        // ...and the gap the monitor left unset inherits the global bar.
        assert_eq!(resolved.bar.gap, 4);
        // The translucent background reaches the render settings this output clears to.
        assert_eq!(resolved.render_settings().background, (0, 0, 0, 0x80));
    }

    #[test]
    fn duplicate_monitor_entries_resolve_to_the_first_match() {
        let config = Config::from_toml_str(
            r#"
            [[monitor]]
            name = "DP-1"
            height = 40
            [[monitor]]
            name = "DP-1"
            height = 99
            "#,
        )
        .unwrap();
        // First-match wins, mirroring widget de-duplication's first-wins rule.
        assert_eq!(config.resolve_for_output(Some("DP-1")).height, 40);
    }

    #[test]
    fn load_from_path_reports_the_file_on_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.toml");
        fs::write(&path, "height = \"tall\"\n").unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        let msg = err.to_string();
        // The error names the offending file, not a silent fallback to defaults.
        assert!(msg.contains("broken.toml"), "message: {msg}");
    }

    #[test]
    fn zero_height_is_rejected() {
        let err = Config::from_toml_str("height = 0").unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid { .. }),
            "wrong variant: {err:?}"
        );
        assert!(err.to_string().contains("height"), "message: {err}");
    }

    #[test]
    fn absurdly_tall_bar_is_rejected() {
        let err = Config::from_toml_str("height = 100000").unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid { .. }),
            "wrong variant: {err:?}"
        );
    }

    #[test]
    fn zero_font_size_is_rejected() {
        let err = Config::from_toml_str("[font]\nsize = 0.0").unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid { .. }),
            "wrong variant: {err:?}"
        );
        assert!(err.to_string().contains("font.size"), "message: {err}");
    }

    #[test]
    fn non_finite_font_size_is_rejected() {
        // TOML spells these `nan` / `inf`; both must be refused, not rendered.
        for doc in ["[font]\nsize = nan", "[font]\nsize = inf"] {
            let err = Config::from_toml_str(doc).unwrap_err();
            assert!(
                matches!(err, ConfigError::Invalid { .. }),
                "{doc:?} -> {err:?}"
            );
        }
    }

    #[test]
    fn negative_font_size_is_rejected() {
        let err = Config::from_toml_str("[font]\nsize = -4.0").unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid { .. }),
            "wrong variant: {err:?}"
        );
    }

    #[test]
    fn nameless_monitor_override_is_rejected() {
        // A [[monitor]] with no name can never match an output — surely a mistake.
        let err = Config::from_toml_str("[[monitor]]\nheight = 30").unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid { .. }),
            "wrong variant: {err:?}"
        );
        assert!(err.to_string().contains("name"), "message: {err}");
    }

    #[test]
    fn out_of_range_monitor_override_is_rejected() {
        let err = Config::from_toml_str("[[monitor]]\nname = \"DP-1\"\nheight = 0").unwrap_err();
        assert!(
            matches!(err, ConfigError::Invalid { .. }),
            "wrong variant: {err:?}"
        );
        // The message points at the specific monitor and field.
        let msg = err.to_string();
        assert!(
            msg.contains("DP-1") && msg.contains("height"),
            "message: {msg}"
        );
    }

    #[test]
    fn boundary_values_are_accepted() {
        // The inclusive bounds themselves are valid.
        let config = Config::from_toml_str("height = 4096\n[font]\nsize = 1.0").unwrap();
        assert_eq!(config.height, 4096);
        assert_eq!(config.font.size, 1.0);
    }

    #[test]
    fn backlight_configuration_parses_and_merges_per_monitor() {
        let config = Config::from_toml_str(
            r#"
            [bar]
            modules-right = ["backlight"]
            [widget.backlight]
            device = "intel_backlight"
            format = "{icon}  {percent}%"
            format-icons = ["low", "high"]
            scroll-step = 1.5

            [[monitor]]
            name = "eDP-1"
            [monitor.widget.backlight]
            scroll-step = 2.0
            "#,
        )
        .unwrap();
        assert_eq!(config.bar.modules_right, vec![WidgetKind::Backlight]);
        assert_eq!(
            config.widget.backlight.device.as_deref(),
            Some("intel_backlight")
        );
        assert_eq!(
            config
                .widget
                .backlight
                .format_icons
                .as_ref()
                .and_then(FormatIconsConfig::ramp)
                .map(Vec::as_slice),
            Some(&["low".to_string(), "high".to_string()][..])
        );
        let resolved = config.resolve_for_output(Some("eDP-1"));
        assert_eq!(resolved.widget.backlight.scroll_step, Some(2.0));
        assert_eq!(
            resolved.widget.backlight.device.as_deref(),
            Some("intel_backlight")
        );
    }

    #[test]
    fn invalid_backlight_configuration_is_rejected() {
        for doc in [
            "[widget.backlight]\nformat = \"{unknown}\"",
            "[widget.backlight]\nformat-icons = []",
            "[widget.backlight]\nscroll-step = 0.0",
            "[widget.backlight]\ndevice = \"../panel\"",
        ] {
            let error = Config::from_toml_str(doc).unwrap_err();
            assert!(
                matches!(error, ConfigError::Invalid { .. }),
                "{doc}: {error:?}"
            );
            assert!(error.to_string().contains("widget.backlight"));
        }
    }

    #[test]
    fn invalid_battery_format_is_rejected() {
        let error = Config::from_toml_str("[widget.battery]\nformat = \"{watts}\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("widget.battery.format"), "message: {error}");
        assert!(error.contains("{watts}"), "message: {error}");
    }

    #[test]
    fn invalid_updates_format_is_rejected() {
        let error = Config::from_toml_str("[widget.updates]\nformat = \"{percent}\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("widget.updates.format"), "message: {error}");
        assert!(error.contains("{percent}"), "message: {error}");
    }

    #[test]
    fn power_profiles_configuration_parses_and_merges_per_monitor() {
        let config = Config::from_toml_str(
            r#"
            [widget.power-profiles-daemon]
            format = "{icon} {profile}"
            tooltip = true
            tooltip-format = "Power profile: {profile}\nDriver: {driver}"
            [widget.power-profiles-daemon.format-icons]
            default = "sun"
            balanced = "balance"
            performance = "bolt"
            power-saver = "leaf"

            [[monitor]]
            name = "eDP-1"
            [monitor.widget.power-profiles-daemon]
            tooltip = false
            "#,
        )
        .unwrap();
        let widget = &config.widget.power_profiles_daemon;
        assert_eq!(widget.format.as_deref(), Some("{icon} {profile}"));
        assert_eq!(widget.tooltip, Some(true));
        let icons = widget
            .format_icons
            .as_ref()
            .and_then(FormatIconsConfig::named)
            .expect("named icons");
        assert_eq!(icons.get("performance").map(String::as_str), Some("bolt"));

        let resolved = config.resolve_for_output(Some("eDP-1"));
        assert_eq!(resolved.widget.power_profiles_daemon.tooltip, Some(false));
        assert_eq!(
            resolved.widget.power_profiles_daemon.format.as_deref(),
            Some("{icon} {profile}")
        );
    }

    #[test]
    fn invalid_power_profiles_configuration_is_rejected() {
        for doc in [
            "[widget.power-profiles-daemon]\nformat = \"{percent}\"",
            "[widget.power-profiles-daemon]\ntooltip-format = \"{unknown}\"",
            "[widget.power-profiles-daemon]\nformat-icons = []",
        ] {
            let error = Config::from_toml_str(doc).unwrap_err().to_string();
            assert!(error.contains("power-profiles-daemon"), "message: {error}");
        }
    }

    #[test]
    fn an_invalid_file_names_the_file_and_the_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        fs::write(&path, "height = 0\n").unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bad.toml"), "message: {msg}");
        assert!(msg.contains("height"), "message: {msg}");
    }
}
