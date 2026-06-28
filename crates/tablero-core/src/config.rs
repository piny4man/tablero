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
//! Defaults match the constants the bar shipped with before configuration
//! existed, so an absent config file renders exactly as the hardcoded bar did:
//!
//! ```toml
//! height  = 32
//! spacing = 0
//! padding = 0
//! widgets = ["workspaces", "clock", "battery", "system"]
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
use crate::widget::{BatteryWidget, ClockWidget, Dashboard, SystemWidget, Widget, WorkspaceWidget};

/// Default bar height in pixels.
const DEFAULT_HEIGHT: u32 = 32;
/// Default text size in pixels.
const DEFAULT_FONT_SIZE: f32 = 16.0;
/// Default opaque dark background.
const DEFAULT_BACKGROUND: Color = Color::rgb(0x18, 0x18, 0x18);
/// Default light foreground (also the default accent).
const DEFAULT_FOREGROUND: Color = Color::rgb(0xEA, 0xEA, 0xEA);

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
}

/// The default left-to-right widget order, matching the pre-config bar.
fn default_widgets() -> Vec<WidgetKind> {
    vec![
        WidgetKind::Workspaces,
        WidgetKind::Clock,
        WidgetKind::Battery,
        WidgetKind::System,
    ]
}

/// The fully-resolved bar configuration.
///
/// Build one with [`from_toml_str`](Config::from_toml_str) or
/// [`load_from_path`](Config::load_from_path); [`Config::default`] is the
/// documented baseline every field falls back to.
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
        }
    }
}

impl Config {
    /// Parse a configuration from a TOML document.
    ///
    /// Missing fields fall back to their documented defaults; unknown keys,
    /// unknown widget names, and malformed colors are reported as errors.
    pub fn from_toml_str(toml: &str) -> Result<Config, ConfigError> {
        toml::from_str(toml).map_err(|source| ConfigError::Parse { path: None, source })
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
            Ok(text) => toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: Some(path.to_path_buf()),
                source,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
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
}

impl WidgetKind {
    /// Construct the widget this kind names, occupying `bounds`.
    ///
    /// The bounds are a placeholder seed; real per-column slots are assigned by
    /// [`Dashboard::layout`] each frame.
    pub fn build(self, bounds: Bounds) -> Box<dyn Widget> {
        match self {
            WidgetKind::Workspaces => Box::new(WorkspaceWidget::new(bounds)),
            WidgetKind::Clock => Box::new(ClockWidget::new(bounds)),
            WidgetKind::Battery => Box::new(BatteryWidget::new(bounds)),
            WidgetKind::System => Box::new(SystemWidget::new(bounds)),
        }
    }
}

impl Config {
    /// The render settings (theme colors and font) this configuration resolves to.
    pub fn render_settings(&self) -> RenderSettings {
        RenderSettings {
            background: self.theme.background.to_rgb(),
            foreground: self.theme.foreground.to_rgb(),
            accent: self.theme.accent.to_rgb(),
            font_size: self.font.size,
            font_family: self.font.family.clone(),
        }
    }

    /// Build the dashboard this configuration describes.
    ///
    /// The resolved widgets are constructed in order, each seeded at `bounds`,
    /// and the dashboard carries the configured spacing and padding so its
    /// [`layout`](Dashboard::layout) tiles them as configured.
    pub fn build_dashboard(&self, bounds: Bounds) -> Dashboard {
        let widgets = self
            .resolved_widgets()
            .into_iter()
            .map(|kind| kind.build(bounds))
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
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Read { source, .. } => Some(source),
            ConfigError::Parse { source, .. } => Some(source),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_matches_the_pre_config_bar() {
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
    fn load_from_path_reports_the_file_on_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.toml");
        fs::write(&path, "height = \"tall\"\n").unwrap();
        let err = Config::load_from_path(&path).unwrap_err();
        let msg = err.to_string();
        // The error names the offending file, not a silent fallback to defaults.
        assert!(msg.contains("broken.toml"), "message: {msg}");
    }
}
