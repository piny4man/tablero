//! TOML configuration for the bar's visual theme, dimensions, fonts, spacing,
//! and widget order.
//!
//! The whole tree deserializes from a single TOML document via [`Config::from_toml_str`]
//! (or [`Config::load_from_path`] to read a file), and every field has a
//! documented default — an empty or partial document yields a fully-formed
//! [`Config`], so users only specify what they want to override. Anything the
//! schema does not recognize (an unknown key, an unknown widget name, a
//! malformed color) is a hard [`ConfigError`] rather than a silent fallback.
//!
//! Every field has a documented default, so an absent config file renders the
//! full default bar:
//!
//! ```toml
//! height  = 32
//! spacing = 0
//! padding = 0
//! widgets = ["workspaces", "clock", "battery", "system", "network"]
//!
//! [theme]
//! background = "#181818"
//! foreground = "#eaeaea"
//! accent     = "#eaeaea"
//!
//! [font]
//! # family is unset by default (the system default font is used)
//! size = 16.0
//! ```

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::{Deserializer, Error as _};

use crate::render::{Bounds, RenderSettings};
use crate::scale::Scale;
use crate::widget::{
    BatteryWidget, ClockWidget, Dashboard, NetworkWidget, SystemWidget, TrayWidget, Widget,
    WorkspaceWidget,
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

/// An RGB color, parsed from a `"#rrggbb"` (or bare `"rrggbb"`) hex string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    /// Red channel.
    pub r: u8,
    /// Green channel.
    pub g: u8,
    /// Blue channel.
    pub b: u8,
}

impl Color {
    /// Construct a color from its channels.
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// The color as an `(r, g, b)` tuple, the form the renderer consumes.
    pub fn to_rgb(self) -> (u8, u8, u8) {
        (self.r, self.g, self.b)
    }

    /// Parse a `#rrggbb` or `rrggbb` hex string into a color.
    ///
    /// The leading `#` is optional; exactly six hex digits are required. Returns
    /// a human-readable message on anything else, so a typo surfaces as a clear
    /// configuration error rather than a wrong-but-silent color.
    pub fn parse_hex(s: &str) -> Result<Color, String> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "invalid color {s:?}: expected a \"#rrggbb\" hex string"
            ));
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("validated hex");
        Ok(Color::rgb(channel(0), channel(2), channel(4)))
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
    /// The CPU/memory indicator.
    System,
    /// The network connectivity indicator.
    Network,
    /// The StatusNotifierItem system tray.
    Tray,
}

/// The default left-to-right widget order, matching the pre-config bar.
fn default_widgets() -> Vec<WidgetKind> {
    vec![
        WidgetKind::Workspaces,
        WidgetKind::Clock,
        WidgetKind::Battery,
        WidgetKind::System,
        WidgetKind::Network,
    ]
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
    /// Widget-column spacing override.
    pub spacing: Option<u32>,
    /// Widget-column padding override.
    pub padding: Option<u32>,
    /// Widget order override.
    pub widgets: Option<Vec<WidgetKind>>,
    /// Theme channel overrides.
    pub theme: ThemeOverride,
    /// Font field overrides.
    pub font: FontOverride,
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
    /// Horizontal gap between adjacent widget columns, in pixels.
    pub spacing: u32,
    /// Inner padding inset on each widget column, in pixels.
    pub padding: u32,
    /// Widgets to render, left to right.
    pub widgets: Vec<WidgetKind>,
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
            spacing: 0,
            padding: 0,
            widgets: default_widgets(),
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
        config
            .validate()
            .map_err(|message| ConfigError::Invalid { path: None, message })?;
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
    /// an absurd spacing/padding, or a `[[monitor]]` override with no name (it
    /// can never match an output). Returns a message naming the first offending
    /// field.
    ///
    /// Called from both parse entry points so a present-but-invalid config is
    /// fatal at startup rather than silently producing an unusable bar.
    pub fn validate(&self) -> Result<(), String> {
        validate_range("height", self.height, MIN_HEIGHT, MAX_HEIGHT)?;
        validate_font_size("font.size", self.font.size)?;
        validate_range("spacing", self.spacing, 0, MAX_GAP)?;
        validate_range("padding", self.padding, 0, MAX_GAP)?;

        for monitor in &self.monitors {
            if monitor.name.trim().is_empty() {
                return Err(
                    "a [[monitor]] override has an empty `name`; it can never match an output"
                        .to_string(),
                );
            }
            let who = monitor.name.as_str();
            if let Some(height) = monitor.height {
                validate_range(&format!("monitor {who:?} height"), height, MIN_HEIGHT, MAX_HEIGHT)?;
            }
            if let Some(spacing) = monitor.spacing {
                validate_range(&format!("monitor {who:?} spacing"), spacing, 0, MAX_GAP)?;
            }
            if let Some(padding) = monitor.padding {
                validate_range(&format!("monitor {who:?} padding"), padding, 0, MAX_GAP)?;
            }
            if let Some(size) = monitor.font.size {
                validate_font_size(&format!("monitor {who:?} font.size"), size)?;
            }
        }
        Ok(())
    }

    /// The widget kinds to build, in display order, with duplicates removed
    /// (first occurrence wins).
    ///
    /// This is the single widget-order resolution both the dashboard builder and
    /// the tests rely on: a config that lists a widget twice still renders it
    /// once, in the position of its first mention.
    pub fn resolved_widgets(&self) -> Vec<WidgetKind> {
        let mut resolved: Vec<WidgetKind> = Vec::new();
        for &kind in &self.widgets {
            if !resolved.contains(&kind) {
                resolved.push(kind);
            }
        }
        resolved
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
        if let Some(spacing) = monitor.spacing {
            resolved.spacing = spacing;
        }
        if let Some(padding) = monitor.padding {
            resolved.padding = padding;
        }
        if let Some(widgets) = &monitor.widgets {
            resolved.widgets = widgets.clone();
        }
        monitor.theme.apply(&mut resolved.theme);
        monitor.font.apply(&mut resolved.font);

        resolved
    }
}

/// Reject a `u32` field outside `min..=max`, with a message naming the field.
fn validate_range(field: &str, value: u32, min: u32, max: u32) -> Result<(), String> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(format!("{field} must be between {min} and {max}, got {value}"))
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

impl WidgetKind {
    /// Construct the widget this kind names, occupying `bounds`.
    ///
    /// The bounds are a placeholder seed; real per-column slots are assigned by
    /// [`Dashboard::layout`] each frame. `monitor` is the connector name of the
    /// output this dashboard serves, threaded to the workspace widget so it
    /// shows only that monitor's workspaces; `None` builds the global fallback.
    pub fn build(self, bounds: Bounds, monitor: Option<&str>) -> Box<dyn Widget> {
        match self {
            WidgetKind::Workspaces => match monitor {
                Some(name) => Box::new(WorkspaceWidget::for_monitor(bounds, name)),
                None => Box::new(WorkspaceWidget::new(bounds)),
            },
            WidgetKind::Clock => Box::new(ClockWidget::new(bounds)),
            WidgetKind::Battery => Box::new(BatteryWidget::new(bounds)),
            WidgetKind::System => Box::new(SystemWidget::new(bounds)),
            WidgetKind::Network => Box::new(NetworkWidget::new(bounds)),
            WidgetKind::Tray => Box::new(TrayWidget::new(bounds)),
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
            background: self.theme.background.to_rgb(),
            foreground: self.theme.foreground.to_rgb(),
            accent: self.theme.accent.to_rgb(),
            font_size: self.font.size,
            font_family: self.font.family.clone(),
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
    /// The resolved widgets are constructed in order, each seeded at `bounds`,
    /// and the dashboard carries the configured spacing and padding so its
    /// [`layout`](Dashboard::layout) tiles them as configured. `monitor` is the
    /// output's connector name, threaded to the workspace widget so it shows
    /// only that monitor's workspaces; pass `None` for the global fallback.
    pub fn build_dashboard(&self, bounds: Bounds, monitor: Option<&str>) -> Dashboard {
        let widgets = self
            .resolved_widgets()
            .into_iter()
            .map(|kind| kind.build(bounds, monitor))
            .collect();
        Dashboard::new(widgets).with_layout(self.spacing, self.padding)
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

    #[test]
    fn default_is_the_documented_baseline() {
        let config = Config::default();
        assert_eq!(config.height, 32);
        assert_eq!(config.spacing, 0);
        assert_eq!(config.padding, 0);
        assert_eq!(config.theme.background, Color::rgb(0x18, 0x18, 0x18));
        assert_eq!(config.theme.foreground, Color::rgb(0xEA, 0xEA, 0xEA));
        assert_eq!(config.theme.accent, Color::rgb(0xEA, 0xEA, 0xEA));
        assert_eq!(config.font.family, None);
        assert_eq!(config.font.size, 16.0);
        assert_eq!(
            config.widgets,
            vec![
                WidgetKind::Workspaces,
                WidgetKind::Clock,
                WidgetKind::Battery,
                WidgetKind::System,
                WidgetKind::Network,
            ]
        );
    }

    #[test]
    fn empty_document_is_all_defaults() {
        // A blank config is valid: every field falls back to its default.
        assert_eq!(Config::from_toml_str("").unwrap(), Config::default());
    }

    #[test]
    fn partial_document_overrides_only_named_fields() {
        let config = Config::from_toml_str(
            r##"
            height = 40
            spacing = 6

            [theme]
            accent = "#ff8800"
            "##,
        )
        .unwrap();

        // Overridden fields take the new value...
        assert_eq!(config.height, 40);
        assert_eq!(config.spacing, 6);
        assert_eq!(config.theme.accent, Color::rgb(0xFF, 0x88, 0x00));
        // ...everything unmentioned keeps its default.
        assert_eq!(config.padding, 0);
        assert_eq!(config.theme.background, Color::rgb(0x18, 0x18, 0x18));
        assert_eq!(config.widgets, default_widgets());
    }

    #[test]
    fn full_document_parses_every_section() {
        let config = Config::from_toml_str(
            r##"
            height = 28
            spacing = 4
            padding = 2
            widgets = ["clock", "workspaces"]

            [theme]
            background = "#000000"
            foreground = "#ffffff"
            accent = "#00ff00"

            [font]
            family = "JetBrains Mono"
            size = 14.0
            "##,
        )
        .unwrap();

        assert_eq!(config.height, 28);
        assert_eq!(config.spacing, 4);
        assert_eq!(config.padding, 2);
        assert_eq!(config.theme.background, Color::rgb(0, 0, 0));
        assert_eq!(config.theme.foreground, Color::rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(config.theme.accent, Color::rgb(0, 0xFF, 0));
        assert_eq!(config.font.family.as_deref(), Some("JetBrains Mono"));
        assert_eq!(config.font.size, 14.0);
        assert_eq!(
            config.widgets,
            vec![WidgetKind::Clock, WidgetKind::Workspaces]
        );
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
        // The tray is not in the default set, but naming it is valid and builds.
        assert!(!default_widgets().contains(&WidgetKind::Tray));
        let config = Config::from_toml_str(r#"widgets = ["tray", "clock"]"#).unwrap();
        assert_eq!(config.widgets, vec![WidgetKind::Tray, WidgetKind::Clock]);
        // It constructs a widget without panicking.
        let _ = WidgetKind::Tray.build(Bounds::new(0, 0, 64, 32), None);
    }

    #[test]
    fn unknown_widget_name_is_an_error() {
        let err = Config::from_toml_str(r#"widgets = ["clock", "weather"]"#).unwrap_err();
        // serde reports the unknown variant; the loader wraps it as invalid config.
        assert!(err.to_string().contains("invalid config"));
    }

    #[test]
    fn unknown_key_is_rejected_rather_than_ignored() {
        // A typo'd key must fail rather than be silently dropped.
        let err = Config::from_toml_str("heigth = 40").unwrap_err();
        assert!(err.to_string().contains("invalid config"));
    }

    #[test]
    fn resolved_widgets_dedupes_preserving_first_position() {
        let config = Config::from_toml_str(
            r#"widgets = ["clock", "battery", "clock", "workspaces", "battery"]"#,
        )
        .unwrap();
        assert_eq!(
            config.resolved_widgets(),
            vec![
                WidgetKind::Clock,
                WidgetKind::Battery,
                WidgetKind::Workspaces
            ]
        );
    }

    #[test]
    fn resolved_widgets_on_empty_list_is_empty() {
        let config = Config::from_toml_str("widgets = []").unwrap();
        assert!(config.resolved_widgets().is_empty());
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
        // The font size doubles to physical pixels...
        assert_eq!(scaled.font_size, 32.0);
        // ...while everything scale-independent is carried verbatim.
        assert_eq!(scaled.foreground, (0x10, 0x20, 0x30));
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
        let config = Config::from_toml_str("height = 30\nwidgets = [\"clock\"]").unwrap();
        let resolved = config.resolve_for_output(Some("DP-1"));
        assert_eq!(resolved.height, 30);
        assert_eq!(resolved.widgets, vec![WidgetKind::Clock]);
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
            widgets = ["workspaces", "clock", "battery"]

            [theme]
            background = "#111111"
            foreground = "#eeeeee"

            [[monitor]]
            name = "HDMI-A-1"
            height = 28
            widgets = ["clock"]
            [monitor.theme]
            background = "#000000"
            "##,
        )
        .unwrap();

        let resolved = config.resolve_for_output(Some("HDMI-A-1"));
        // Overridden fields take the monitor's value...
        assert_eq!(resolved.height, 28);
        assert_eq!(resolved.widgets, vec![WidgetKind::Clock]);
        assert_eq!(resolved.theme.background, Color::rgb(0, 0, 0));
        // ...and fields the monitor left unset inherit from the base config,
        // including theme channels the override did not mention.
        assert_eq!(resolved.theme.foreground, Color::rgb(0xEE, 0xEE, 0xEE));
        assert_eq!(resolved.spacing, 0);
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
    fn monitor_font_and_spacing_overrides_apply() {
        let config = Config::from_toml_str(
            r##"
            spacing = 2
            [font]
            size = 16.0

            [[monitor]]
            name = "DP-2"
            spacing = 8
            [monitor.font]
            size = 20.0
            "##,
        )
        .unwrap();
        let resolved = config.resolve_for_output(Some("DP-2"));
        assert_eq!(resolved.spacing, 8);
        assert_eq!(resolved.font.size, 20.0);
        // The font family the override did not set still inherits the base.
        assert_eq!(resolved.font.family, None);
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
        assert!(msg.contains("DP-1") && msg.contains("height"), "message: {msg}");
    }

    #[test]
    fn boundary_values_are_accepted() {
        // The inclusive bounds themselves are valid.
        let config = Config::from_toml_str("height = 4096\n[font]\nsize = 1.0").unwrap();
        assert_eq!(config.height, 4096);
        assert_eq!(config.font.size, 1.0);
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
