//! The widget/message architecture: typed messages drive widget state, widgets
//! report whether their visible state changed, and the [`Dashboard`] turns those
//! reports into a dirty-flag redraw decision and a content-aware zone layout.
//!
//! ```text
//!   data source ──Msg──▶ Dashboard::update ──bool──▶ redraw? ──▶ Dashboard::draw
//! ```
//!
//! A widget never renders itself unprompted: the host loop builds a [`Msg`],
//! applies it, and only repaints when [`Dashboard::update`] reports a change (or
//! the Wayland lifecycle forces a frame). Each widget also exposes a
//! [`measure`](Widget::measure) so the dashboard can pack it to exactly its
//! content in one of three zones (left, center, right), and carries a resolved
//! [`WidgetStyle`] describing its pill colors, glyph, and state-driven accents.
//! See [`Widget`] for the per-widget contract and [`clock::ClockWidget`] for the
//! reference implementation.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use serde::Deserialize;
use serde::de::{Deserializer, Error as DeError};

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, FG, RenderContext};

pub mod backlight;
pub mod battery;
pub mod bluetooth;
pub mod clock;
pub mod hypridle;
pub mod network;
pub mod notifications;
pub mod power;
pub mod power_profiles;
pub mod system;
pub mod title;
pub mod tray;
pub mod updates;
pub mod volume;
pub mod workspaces;

pub use backlight::{Backlight, BacklightWidget};
pub use battery::{Battery, BatteryState, BatteryWidget};
pub use bluetooth::{Bluetooth, BluetoothState, BluetoothWidget};
pub use clock::ClockWidget;
pub use hypridle::{Hypridle, HypridleWidget};
pub use network::{Network, NetworkState, NetworkWidget};
pub use notifications::{Notifications, NotificationsWidget};
pub use power::PowerWidget;
pub use power_profiles::{PowerProfile, PowerProfilesState, PowerProfilesWidget};
pub use system::{SystemStats, SystemWidget};
// LaunchSpec is defined below with Command; re-exported for config and widgets.
pub use title::{ActiveWindow, TitleWidget};
pub use tray::{
    TrayIcon, TrayItem, TrayMenu, TrayMenuItem, TrayMenuMode, TrayMenuToggle, TrayMenuToggleKind,
    TrayMenuToggleState, TrayState, TrayStatus, TrayWidget,
};
pub use updates::{PackageUpdate, PackageUpdates, UpdatesWidget};
pub use volume::{DeviceKind, Volume, VolumeWidget};
pub use workspaces::{WorkspaceWidget, Workspaces};

/// Alert pill background — a muted red, drawn behind a low-battery reading or a
/// tray item requesting attention.
const ALERT_BACKGROUND: (u8, u8, u8, u8) = (0xBF, 0x61, 0x6A, 0xFF);
/// Alert pill foreground — white, for contrast over [`ALERT_BACKGROUND`].
const ALERT_FOREGROUND: (u8, u8, u8, u8) = (0xFF, 0xFF, 0xFF, 0xFF);
/// Default pill corner radius, in logical pixels (scaled at render time).
const DEFAULT_RADIUS: u32 = 8;
/// Default inset between a pill edge and its text, in logical pixels.
const DEFAULT_PADDING: u32 = 8;
/// Default widget border width, in logical pixels (scaled at render time).
const DEFAULT_BORDER_WIDTH: u32 = 1;
/// Default percent below which a discharging battery shows its warn colors.
const DEFAULT_WARN_THRESHOLD: u32 = 20;
/// Default charging-state foreground — a bright ARC Raiders green, so a charging
/// battery reads as "recovering" with no configuration. Only the battery widget
/// swaps to its [`charging`](WidgetStyle::charging) colors, so in practice this
/// default is battery-facing.
pub(crate) const CHARGING_FOREGROUND: (u8, u8, u8, u8) = (0x5F, 0xF5, 0xA0, 0xFF);

/// Every state update a widget can react to.
///
/// One typed model carries all inputs that can change what is on screen, so new
/// widgets and data sources plug in by adding variants rather than by rewriting
/// the host loop. Marked `#[non_exhaustive]`: downstream code must keep a
/// catch-all arm so new variants are non-breaking.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Msg {
    /// The wall clock advanced; carries the current local time.
    Tick(DateTime<Local>),
    /// The Hyprland workspace set or active workspace changed.
    Workspaces(Workspaces),
    /// The battery reading changed; `None` means no battery is present (or the
    /// power daemon is unavailable), so the widget shows nothing.
    Battery(Option<Battery>),
    /// The available Linux screen-backlight devices changed.
    Backlight(Vec<Backlight>),
    /// A fresh system-pressure sample: current CPU and memory load.
    System(SystemStats),
    /// The network connectivity changed; `None` means no network is available
    /// (or the network daemon is unreachable), so the widget shows nothing.
    Network(Option<Network>),
    /// The local Bluetooth adapter state and connected-device count.
    ///
    /// Unlike the battery/network messages, the snapshot is **not** wrapped in
    /// an `Option`: the bluetooth widget always reserves a slot and falls
    /// back to showing `unavailable` before its first reading or when no
    /// adapter is present, so there is no value the producer could send that
    /// means "show nothing". The widget starts in
    /// [`Unavailable`](BluetoothState::Unavailable) and stays there until the
    /// producer hands it a reading.
    Bluetooth(Bluetooth),
    /// The set of registered StatusNotifierItem tray items changed; carries the
    /// normalized snapshot of what the tray should now show.
    Tray(TrayState),
    /// A DBusMenu layout fetched for a tray interaction and ready to present.
    TrayMenu(TrayMenu),
    /// A tray menu request fell back to the item's own `ContextMenu`, so any
    /// pending native popup state can be released.
    TrayMenuUnavailable(String),
    /// The active window on a specific Hyprland monitor changed.
    ///
    /// Each output's bar carries its own `TitleWidget` bound to a specific
    /// monitor name; the widget ignores messages for any other monitor so
    /// focus changes on monitor A update only monitor A's bar. `window` is
    /// `None` when no window on that monitor is focused (empty desktop or
    /// compositor has no active window on the named monitor), in which case
    /// the widget reserves no slot.
    ActiveWindow {
        /// The Hyprland connector name (e.g. `"DP-1"`) of the output whose
        /// bar should update.
        monitor: String,
        /// The current active window on that monitor.
        window: Option<ActiveWindow>,
    },
    /// The active PipeWire output sink's volume / mute state changed.
    ///
    /// The snapshot carries the level (whole percent) and the current mute
    /// state, plus the device kind that drives the glyph. `None` means the
    /// PipeWire server is unreachable or no output sink exists, so the widget
    /// shows nothing — matching how the `network` and `system` widgets treat
    /// an absent source.
    Volume(Option<Volume>),
    /// The notification daemon's (swaync) visible state changed: pending count
    /// and Do-Not-Disturb.
    ///
    /// `None` means swaync is not on the session bus, so the widget shows
    /// nothing — the same absent-source convention as [`Msg::Volume`].
    Notifications(Option<Notifications>),
    /// The power-profiles-daemon state; `None` means the daemon is unavailable.
    PowerProfiles(Option<PowerProfilesState>),
    /// Available Arch repository and AUR updates; `None` hides the widget.
    Updates(Option<PackageUpdates>),
    /// Whether the session's Hypridle daemon is currently running.
    Hypridle(Hypridle),
}

impl Msg {
    /// Build a [`Msg::Tick`] sampling the current local time.
    ///
    /// Keeps the wall-clock read on the message-producer side so widgets stay
    /// pure functions of the messages they receive.
    pub fn tick_now() -> Self {
        Msg::Tick(Local::now())
    }
}

/// An outbound action a widget requests in response to input.
///
/// Where [`Msg`] flows *in* (data sources update widgets), a `Command` flows
/// *out*: a widget turns a click into an intent, and the host loop hands it to
/// whatever can execute it (for workspace switches, the Hyprland command
/// socket). Keeping the intent typed and execution-free here means click
/// handling stays a pure, testable decision. Marked `#[non_exhaustive]` for the
/// same forward-compatibility reason as [`Msg`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Switch the compositor to the workspace with this id.
    SwitchWorkspace(i32),
    /// Activate the tray item registered under this address (an SNI
    /// `Activate` request), at the click's screen coordinates.
    ActivateTrayItem {
        /// The item's watcher registration address.
        key: String,
        /// Horizontal screen coordinate.
        x: i32,
        /// Vertical screen coordinate.
        y: i32,
    },
    /// Open an item's exported DBusMenu, falling back to SNI `ContextMenu` when
    /// it does not export one.
    OpenTrayMenu {
        /// The item's watcher registration address.
        key: String,
        /// Horizontal screen coordinate.
        x: i32,
        /// Vertical screen coordinate.
        y: i32,
    },
    /// Activate one entry from an open tray DBusMenu.
    ActivateTrayMenuItem {
        /// The owning tray item's watcher registration address.
        key: String,
        /// DBusMenu item id.
        id: i32,
    },
    /// Spawn a configured program directly (no shell), used to wire a
    /// per-widget `on-click` action (e.g. a Bluetooth manager launcher). The
    /// host executor expands a leading `~`, resolves bare names via `PATH`,
    /// and passes any arguments without shell expansion.
    RunProgram(LaunchSpec),

    /// Toggle the notification daemon's control-center panel (swaync
    /// `ToggleVisibility` — the notifications widget's primary click).
    ToggleNotificationPanel,
    /// Toggle the notification daemon's Do-Not-Disturb mode (swaync
    /// `ToggleDnd` — the notifications widget's secondary click).
    ToggleNotificationsDnd,
    /// Adjust one kernel backlight by a percentage step through systemd-logind.
    AdjustBacklight {
        /// Kernel device name under `/sys/class/backlight`.
        device: String,
        /// Whether brightness should increase or decrease.
        direction: ScrollDirection,
        /// Percentage points changed by one logical scroll step.
        step: f64,
    },
    /// Select one of power-profiles-daemon's advertised profiles.
    SetPowerProfile(String),
    /// Set whether the session's Hypridle daemon should be running.
    SetHypridle(bool),
}

/// A normalized logical scroll direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    /// Increase the widget's controlled value.
    Increase,
    /// Decrease the widget's controlled value.
    Decrease,
}

/// Program + args for a direct (no shell) on-click launch.
///
/// Built from config as either a single string (`"pavucontrol"` or
/// `"gtk-launch org.gnome.Calendar"`, split on whitespace) or a string list
/// (`["gtk-launch", "org.gnome.Calendar"]`). The first token is the program;
/// the rest are argv. No quoting, pipes, or env expansion — wrap multi-step
/// logic in a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    program: PathBuf,
    args: Vec<String>,
}

impl LaunchSpec {
    /// A program with no arguments.
    pub fn program_only(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// A program with explicit arguments.
    pub fn with_args(program: impl Into<PathBuf>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
        }
    }

    /// Parse a single config string into program + args by splitting on ASCII
    /// whitespace. Empty / whitespace-only input is an error.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut tokens = s.split_whitespace();
        let Some(program) = tokens.next() else {
            return Err("on-click is empty".to_string());
        };
        Ok(Self {
            program: PathBuf::from(program),
            args: tokens.map(str::to_owned).collect(),
        })
    }

    /// Build from an argv list (first element is the program).
    pub fn from_argv(argv: Vec<String>) -> Result<Self, String> {
        let mut iter = argv.into_iter();
        let Some(program) = iter.next() else {
            return Err("on-click list is empty".to_string());
        };
        if program.is_empty() {
            return Err("on-click program is empty".to_string());
        }
        Ok(Self {
            program: PathBuf::from(program),
            args: iter.collect(),
        })
    }

    /// The executable path or bare name.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Arguments after the program (never includes argv0).
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Human-readable form for logs: `program arg1 arg2`.
    pub fn display(&self) -> String {
        if self.args.is_empty() {
            self.program.display().to_string()
        } else {
            let mut out = self.program.display().to_string();
            for arg in &self.args {
                out.push(' ');
                out.push_str(arg);
            }
            out
        }
    }
}

impl From<PathBuf> for LaunchSpec {
    fn from(program: PathBuf) -> Self {
        Self::program_only(program)
    }
}

impl From<&str> for LaunchSpec {
    fn from(s: &str) -> Self {
        Self::parse(s).unwrap_or_else(|_| Self::program_only(PathBuf::from(s)))
    }
}

impl From<String> for LaunchSpec {
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl<'de> Deserialize<'de> for LaunchSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            String(String),
            List(Vec<String>),
        }
        match Raw::deserialize(deserializer)? {
            Raw::String(s) => Self::parse(&s).map_err(DeError::custom),
            Raw::List(list) => Self::from_argv(list).map_err(DeError::custom),
        }
    }
}

/// Which pointer button produced a click, normalized from the compositor's
/// input codes at the platform boundary.
///
/// Widgets branch on this to offer distinct primary/secondary actions (e.g.
/// the notifications widget toggles the panel on `Left` and Do-Not-Disturb on
/// `Right`). Buttons the bar does not handle are filtered out before reaching
/// the widgets, so no `Other` variant is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickButton {
    /// The primary (left) pointer button.
    Left,
    /// The secondary (right) pointer button.
    Right,
}

/// Tooltip content and the widget bounds it should be anchored to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tooltip {
    /// Expanded, possibly multiline tooltip text.
    pub text: String,
    /// The widget's current physical-pixel layout slot.
    pub bounds: Bounds,
}

/// How a widget chooses the glyph it draws before its label.
///
/// Resolved from the `icon` config field: an absent value keeps the widget's
/// built-in [`Default`](IconSetting::Default) glyph, `"none"` disables it, and
/// any other string overrides it with that exact glyph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IconSetting {
    /// Use the widget's built-in default glyph.
    #[default]
    Default,
    /// Draw no glyph at all.
    None,
    /// Use this exact string as the glyph.
    Custom(String),
}

/// An [`IconSetting`] resolved against a widget's semantic default.
///
/// A widget calls [`WidgetStyle::resolve_icon`] with the [`BuiltinIcon`] its
/// current state calls for; the result tells the shared layout helpers what to
/// draw in the icon slot: nothing, the built-in vector art, or a literal user
/// override string rendered as text.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedIcon {
    /// The icon is opted out (`icon = "none"`): draw nothing in the slot.
    None,
    /// Draw this built-in vector icon.
    Builtin(BuiltinIcon),
    /// Draw this literal string as text (a custom `icon`/`format-icons` glyph).
    Text(String),
}

/// Resolved pill colors for one widget state (normal, warn, attention, charging).
///
/// `background` is `None` when that state draws no pill — the text floats
/// directly on the bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateColors {
    /// Pill fill behind the content; `None` draws no pill.
    pub background: Option<(u8, u8, u8, u8)>,
    /// Text/glyph color.
    pub foreground: (u8, u8, u8, u8),
    /// Optional state-specific border color.
    pub border: Option<(u8, u8, u8, u8)>,
}

/// The fully-resolved visual style for one widget.
///
/// Built from a `[widget.<name>]` config table folded onto the theme (see
/// `config::WidgetStyleConfig::resolve`): colors and geometry are concrete, and
/// the state styles ([`warn`](WidgetStyle::warn),
/// [`attention`](WidgetStyle::attention), and
/// [`charging`](WidgetStyle::charging)) carry colors a widget swaps to as its
/// state changes. A
/// `None` [`background`](WidgetStyle::background) means the normal state draws no
/// pill, which is what keeps the default bar visually flat.
#[derive(Debug, Clone, PartialEq)]
pub struct WidgetStyle {
    /// Normal-state pill fill; `None` draws no pill.
    pub background: Option<(u8, u8, u8, u8)>,
    /// Normal-state text/glyph color.
    pub foreground: (u8, u8, u8, u8),
    /// Emphasis color (e.g. the active workspace pill).
    pub accent: (u8, u8, u8, u8),
    /// Optional border around the widget pill or cell.
    pub border: Option<(u8, u8, u8, u8)>,
    /// Border width in logical pixels.
    pub border_width: u32,
    /// Pill corner radius, logical pixels.
    pub radius: u32,
    /// Inset between a pill edge and its text, logical pixels.
    pub padding: u32,
    /// Which glyph to draw before the label.
    pub icon: IconSetting,
    /// Percent below which a discharging battery uses [`warn`](WidgetStyle::warn).
    pub warn_threshold: u32,
    /// Colors for a low/warning state (e.g. low battery).
    pub warn: StateColors,
    /// Colors for an attention state (e.g. a tray item needing attention).
    pub attention: StateColors,
    /// Colors for a battery while it is charging.
    pub charging: StateColors,
}

impl Default for WidgetStyle {
    fn default() -> Self {
        Self {
            background: None,
            foreground: FG,
            accent: FG,
            border: None,
            border_width: DEFAULT_BORDER_WIDTH,
            radius: DEFAULT_RADIUS,
            padding: DEFAULT_PADDING,
            icon: IconSetting::Default,
            warn_threshold: DEFAULT_WARN_THRESHOLD,
            warn: StateColors {
                background: Some(ALERT_BACKGROUND),
                foreground: ALERT_FOREGROUND,
                border: None,
            },
            attention: StateColors {
                background: Some(ALERT_BACKGROUND),
                foreground: ALERT_FOREGROUND,
                border: None,
            },
            charging: StateColors {
                background: None,
                foreground: CHARGING_FOREGROUND,
                border: None,
            },
        }
    }
}

impl WidgetStyle {
    /// The glyph to draw: the widget's `default` when the icon is
    /// [`IconSetting::Default`], nothing for [`IconSetting::None`], or the exact
    /// configured string for [`IconSetting::Custom`].
    pub fn glyph<'a>(&'a self, default: &'a str) -> &'a str {
        match &self.icon {
            IconSetting::Default => default,
            IconSetting::None => "",
            IconSetting::Custom(s) => s,
        }
    }

    /// Resolve the icon slot against a widget's semantic `default`.
    ///
    /// [`IconSetting::Default`] adopts the built-in vector art, [`None`] opts
    /// out, and [`Custom`] keeps the user's literal string as a text override —
    /// the vector-era counterpart to [`glyph`](Self::glyph). Widgets that pick
    /// their default from state (a battery level, a volume level) pass the
    /// state-appropriate [`BuiltinIcon`] here each frame.
    ///
    /// [`None`]: IconSetting::None
    /// [`Custom`]: IconSetting::Custom
    pub fn resolve_icon(&self, default: BuiltinIcon) -> ResolvedIcon {
        match &self.icon {
            IconSetting::Default => ResolvedIcon::Builtin(default),
            IconSetting::None => ResolvedIcon::None,
            IconSetting::Custom(s) => ResolvedIcon::Text(s.clone()),
        }
    }

    /// The normal-state pill colors (this style's own background and foreground),
    /// as a [`StateColors`] for the internal text-pill renderer.
    pub fn base_colors(&self) -> StateColors {
        StateColors {
            background: self.background,
            foreground: self.foreground,
            border: self.border,
        }
    }
}

/// Join a glyph and a label with a single space, dropping either when empty.
///
/// So `("\u{f017}", "08:09")` → `"\u{f017} 08:09"`, `("", "08:09")` → `"08:09"`
/// when the icon is disabled, and an empty label yields just the glyph.
pub(crate) fn glyph_label(glyph: &str, label: &str) -> String {
    match (glyph.is_empty(), label.is_empty()) {
        (true, _) => label.to_string(),
        (false, true) => glyph.to_string(),
        (false, false) => format!("{glyph} {label}"),
    }
}

/// The pill width `text` needs under `style`: its shaped width plus the style's
/// padding on both sides (scaled to physical pixels). Empty text needs no pill,
/// so it measures zero and the widget reserves no slot.
pub(crate) fn measure_text_pill(ctx: &mut RenderContext, style: &WidgetStyle, text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let pad = style.padding * ctx.scale_factor();
    ctx.measure_text(text) + 2 * pad
}

/// Paint a text pill: the rounded background (when `colors.background` is set),
/// then `text` in `colors.foreground`, inset horizontally from the pill edges by
/// the style's padding. A no-op for empty text or a zero-area slot, so a widget
/// with no background and no content paints nothing.
pub(crate) fn draw_text_pill(
    ctx: &mut RenderContext,
    style: &WidgetStyle,
    bounds: Bounds,
    text: &str,
    colors: StateColors,
) {
    if text.is_empty() || bounds.width == 0 || bounds.height == 0 {
        return;
    }
    let scale = ctx.scale_factor();
    if let Some(bg) = colors.background {
        ctx.fill_rounded_rect(bounds, bg, (style.radius * scale) as f32);
    }
    if let Some(border) = colors.border.or(style.border) {
        ctx.stroke_rounded_rect(
            bounds,
            border,
            (style.radius * scale) as f32,
            (style.border_width * scale) as f32,
        );
    }
    let pad = style.padding * scale;
    let inner = Bounds::new(
        bounds.x + pad,
        bounds.y,
        bounds.width.saturating_sub(2 * pad),
        bounds.height,
    );
    ctx.draw_text(text, inner, colors.foreground);
}

/// Paint a pill whose content is a standalone icon, centering the glyph's
/// visible ink rather than its font line box.
pub(crate) fn draw_icon_pill(
    ctx: &mut RenderContext,
    style: &WidgetStyle,
    bounds: Bounds,
    icon: &str,
    colors: StateColors,
) {
    if icon.is_empty() || bounds.width == 0 || bounds.height == 0 {
        return;
    }
    let scale = ctx.scale_factor();
    if let Some(bg) = colors.background {
        ctx.fill_rounded_rect(bounds, bg, (style.radius * scale) as f32);
    }
    if let Some(border) = colors.border.or(style.border) {
        ctx.stroke_rounded_rect(
            bounds,
            border,
            (style.radius * scale) as f32,
            (style.border_width * scale) as f32,
        );
    }
    ctx.draw_text_centered(icon, bounds, colors.foreground);
}

/// One horizontal box in an icon-plus-label layout: either the icon slot or a
/// measured run of text.
struct ContentBox {
    /// The text to draw, or `None` for the icon slot.
    text: Option<String>,
    /// The box's width in physical pixels (the icon edge, or the shaped text).
    width: u32,
}

/// The gap between an icon and an adjacent text run, in physical pixels. A
/// quarter of the icon edge tracks a normal inter-word space at the font size.
fn icon_gap(edge: u32) -> u32 {
    (edge / 4).max(1)
}

/// Split a format template on `{icon}` markers into drawable boxes, measuring
/// each text run. Whitespace bordering a marker is dropped — it becomes the
/// [`icon_gap`] between boxes — so the icon reads as its own element rather than
/// depending on the font preserving a leading space. Empty runs are skipped, so
/// a bare `{icon}` yields a single icon box.
fn icon_boxes(ctx: &mut RenderContext, edge: u32, template: &str) -> Vec<ContentBox> {
    let mut boxes = Vec::new();
    let mut rest = template;
    loop {
        match rest.find("{icon}") {
            Some(idx) => {
                let text = rest[..idx].trim();
                if !text.is_empty() {
                    let width = ctx.measure_text(text);
                    boxes.push(ContentBox {
                        text: Some(text.to_string()),
                        width,
                    });
                }
                boxes.push(ContentBox {
                    text: None,
                    width: edge,
                });
                rest = &rest[idx + "{icon}".len()..];
            }
            None => {
                let text = rest.trim();
                if !text.is_empty() {
                    let width = ctx.measure_text(text);
                    boxes.push(ContentBox {
                        text: Some(text.to_string()),
                        width,
                    });
                }
                break;
            }
        }
    }
    boxes
}

/// Flatten an icon template to plain text, substituting the `{icon}` markers
/// with `repl` (or dropping them for the opted-out state). Runs are joined with
/// a single space so no stray whitespace survives a removed marker.
fn icon_as_text(template: &str, repl: Option<&str>) -> String {
    let repl = repl.map(str::trim).filter(|s| !s.is_empty());
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = template;
    loop {
        match rest.find("{icon}") {
            Some(idx) => {
                let before = rest[..idx].trim();
                if !before.is_empty() {
                    parts.push(before);
                }
                if let Some(repl) = repl {
                    parts.push(repl);
                }
                rest = &rest[idx + "{icon}".len()..];
            }
            None => {
                let last = rest.trim();
                if !last.is_empty() {
                    parts.push(last);
                }
                break;
            }
        }
    }
    parts.join(" ")
}

/// Whether `template`'s only content is the icon slot(s) — no accompanying text.
/// Such a widget renders as a single centered glyph, so a text override keeps the
/// ink-centered look rather than left-aligning.
fn is_icon_only(template: &str) -> bool {
    template.contains("{icon}") && template.replace("{icon}", " ").trim().is_empty()
}

/// The pill width an icon `template` needs under `style`, resolved for `icon`.
///
/// For a [`Builtin`](ResolvedIcon::Builtin) icon this reserves the icon edge plus
/// each text run plus the gaps between them; for a text override or the opted-out
/// state it falls back to [`measure_text_pill`] over the flattened string. Empty
/// content measures zero, so an opted-out icon-only widget reserves no slot.
pub(crate) fn measure_icon_content(
    ctx: &mut RenderContext,
    style: &WidgetStyle,
    icon: &ResolvedIcon,
    template: &str,
) -> u32 {
    match icon {
        ResolvedIcon::Builtin(_) => {
            let edge = ctx.icon_edge();
            let boxes = icon_boxes(ctx, edge, template);
            if boxes.is_empty() {
                return 0;
            }
            let gap = icon_gap(edge);
            let content: u32 =
                boxes.iter().map(|b| b.width).sum::<u32>() + gap * (boxes.len() as u32 - 1);
            content + 2 * style.padding * ctx.scale_factor()
        }
        ResolvedIcon::None => measure_text_pill(ctx, style, &icon_as_text(template, None)),
        ResolvedIcon::Text(s) => measure_text_pill(ctx, style, &icon_as_text(template, Some(s))),
    }
}

/// Paint an icon `template` resolved for `icon`, in `colors`.
///
/// A [`Builtin`](ResolvedIcon::Builtin) icon lays the vector art and its text
/// runs out horizontally, each in `colors.foreground`, behind the style's pill.
/// A text override or the opted-out state renders through the text-pill path
/// (ink-centered when the widget is icon-only, left-aligned otherwise), so a
/// custom glyph looks exactly as it did before built-in icons existed. A no-op
/// for empty content or a zero-area slot.
pub(crate) fn draw_icon_content(
    ctx: &mut RenderContext,
    style: &WidgetStyle,
    bounds: Bounds,
    icon: &ResolvedIcon,
    template: &str,
    colors: StateColors,
) {
    match icon {
        ResolvedIcon::Builtin(builtin) => {
            draw_builtin_content(ctx, style, bounds, &[*builtin], template, colors);
        }
        ResolvedIcon::None => {
            draw_text_pill(ctx, style, bounds, &icon_as_text(template, None), colors);
        }
        ResolvedIcon::Text(s) => {
            let text = icon_as_text(template, Some(s));
            if is_icon_only(template) {
                draw_icon_pill(ctx, style, bounds, &text, colors);
            } else {
                draw_text_pill(ctx, style, bounds, &text, colors);
            }
        }
    }
}

/// Paint an icon `template` whose successive `{icon}` markers are drawn with
/// successive entries from `icons`, letting a widget pair distinct icons with
/// distinct readouts — a cpu icon before the CPU load, a memory icon before the
/// memory load. When the template holds more markers than `icons` supplies, the
/// last icon repeats. This is the multi-icon cousin of [`draw_icon_content`]'s
/// built-in path; a custom glyph or the opted-out state stays on
/// [`draw_icon_content`], which renders through the single-icon text pill.
pub(crate) fn draw_builtin_icons(
    ctx: &mut RenderContext,
    style: &WidgetStyle,
    bounds: Bounds,
    icons: &[BuiltinIcon],
    template: &str,
    colors: StateColors,
) {
    draw_builtin_content(ctx, style, bounds, icons, template, colors);
}

/// Draw the built-in vector icons and text runs of `template` across `bounds`,
/// consuming `icons` one per `{icon}` marker (saturating to the last entry).
fn draw_builtin_content(
    ctx: &mut RenderContext,
    style: &WidgetStyle,
    bounds: Bounds,
    icons: &[BuiltinIcon],
    template: &str,
    colors: StateColors,
) {
    if bounds.width == 0 || bounds.height == 0 || icons.is_empty() {
        return;
    }
    let edge = ctx.icon_edge();
    let boxes = icon_boxes(ctx, edge, template);
    if boxes.is_empty() {
        return;
    }
    let scale = ctx.scale_factor();
    if let Some(bg) = colors.background {
        ctx.fill_rounded_rect(bounds, bg, (style.radius * scale) as f32);
    }
    if let Some(border) = colors.border.or(style.border) {
        ctx.stroke_rounded_rect(
            bounds,
            border,
            (style.radius * scale) as f32,
            (style.border_width * scale) as f32,
        );
    }
    let gap = icon_gap(edge);
    let pad = style.padding * scale;
    let mut x = bounds.x + pad;
    let mut icon_slot = 0;
    for (index, content) in boxes.iter().enumerate() {
        if index > 0 {
            x += gap;
        }
        match &content.text {
            None => {
                let builtin = icons[icon_slot.min(icons.len() - 1)];
                icon_slot += 1;
                let iy = bounds.y + bounds.height.saturating_sub(edge) / 2;
                ctx.draw_builtin_icon(builtin, Bounds::new(x, iy, edge, edge), colors.foreground);
            }
            Some(text) => {
                ctx.draw_text(
                    text,
                    Bounds::new(x, bounds.y, content.width, bounds.height),
                    colors.foreground,
                );
            }
        }
        x += content.width;
    }
}

/// Draw `text` horizontally centered within `bounds` (vertical centering is
/// [`RenderContext::draw_text`]'s own), clamped so it never spills past the
/// slot's right edge. The per-item widgets (workspaces, tray) use this to set a
/// short id or glyph in the middle of a square cell rather than hard against its
/// left edge. A no-op for empty text or a zero-area slot.
pub(crate) fn draw_centered(
    ctx: &mut RenderContext,
    text: &str,
    bounds: Bounds,
    color: (u8, u8, u8, u8),
) {
    if text.is_empty() || bounds.width == 0 || bounds.height == 0 {
        return;
    }
    // Center by insetting the left edge by half the slack; the width shrinks by
    // the same inset so the text box's right edge stays inside the cell (and
    // over-wide text simply left-aligns and clips, never bleeding into a
    // neighbor).
    let pad = bounds.width.saturating_sub(ctx.measure_text(text)) / 2;
    let inner = Bounds::new(
        bounds.x + pad,
        bounds.y,
        bounds.width.saturating_sub(pad),
        bounds.height,
    );
    ctx.draw_text(text, inner, color);
}

/// A drawable, message-driven component of the bar.
///
/// The contract is deliberately small but complete for an event loop that only
/// repaints on change and lays widgets out by their content:
///
/// * [`update`](Widget::update) folds a [`Msg`] into the widget's state and
///   returns whether its *visible* state changed — the redraw signal.
/// * [`draw`](Widget::draw) paints the current state into the shared context;
///   it takes `&self` so drawing never mutates state.
/// * [`measure`](Widget::measure) reports the width the widget needs for the
///   current state, so the [`Dashboard`] can pack it to exactly its content.
/// * [`bounds`](Widget::bounds) / [`set_bounds`](Widget::set_bounds) expose the
///   layout slot so the host can place and hit-test the widget.
pub trait Widget {
    /// Apply `msg`; return `true` iff the widget's visible state changed and it
    /// therefore needs to be redrawn.
    fn update(&mut self, msg: &Msg) -> bool;

    /// Draw the current state into `ctx` at the widget's [`bounds`](Widget::bounds).
    fn draw(&self, ctx: &mut RenderContext);

    /// The width in physical pixels this widget needs to render its current
    /// state, given the inner pill `height` it will be laid out at.
    ///
    /// The [`Dashboard`] calls this before [`set_bounds`](Widget::set_bounds) to
    /// size each zone to its content. The default is zero — a widget with nothing
    /// to show reserves no slot — so a widget opts into the layout only by
    /// overriding this once it has content to measure.
    fn measure(&self, _ctx: &mut RenderContext, _height: u32) -> u32 {
        0
    }

    /// The widget's current layout slot.
    fn bounds(&self) -> Bounds;

    /// Reposition the widget into a new layout slot.
    fn set_bounds(&mut self, bounds: Bounds);

    /// Handle a `button` click at surface pixel `(px, py)`.
    ///
    /// Returns a [`Command`] when the widget owns an interactive region at that
    /// pixel, or `None` when it does not. The default is non-interactive, so a
    /// widget opts into input only by overriding this — clicks on display-only
    /// widgets (and on empty space) are ignored safely. Widgets with a single
    /// action should answer only [`ClickButton::Left`] and ignore the rest.
    fn on_click(&self, _px: u32, _py: u32, _button: ClickButton) -> Option<Command> {
        None
    }

    /// Handle one logical scroll step at surface pixel `(px, py)`.
    fn on_scroll(&self, _px: u32, _py: u32, _direction: ScrollDirection) -> Option<Command> {
        None
    }

    /// Return a tooltip when `(px, py)` lies over a tooltip-enabled region.
    fn tooltip_at(&self, _px: u32, _py: u32) -> Option<Tooltip> {
        None
    }
}

/// The widgets composing the bar, grouped into left/center/right zones, plus the
/// redraw policy and content-aware layout over them.
///
/// [`update`](Dashboard::update) broadcasts a message to every widget in every
/// zone and reports whether *any* of them changed — that boolean is the host
/// loop's cue to repaint. [`draw`](Dashboard::draw) clears the background and
/// paints each widget. [`layout`](Dashboard::layout) measures each widget and
/// packs the three zones to their content, separated by a per-pill
/// [`gap`](Dashboard::with_spacing) and inset by a bar
/// [`margin`](Dashboard::with_spacing).
pub struct Dashboard {
    left: Vec<Box<dyn Widget>>,
    center: Vec<Box<dyn Widget>>,
    right: Vec<Box<dyn Widget>>,
    /// Inset around the whole pill row, in logical pixels.
    margin: u32,
    /// Gap between adjacent pills within a zone, in logical pixels.
    gap: u32,
}

impl Dashboard {
    /// Build a dashboard whose widgets all live in the left zone, with no margin
    /// or gap. This is the single-zone constructor the redraw harnesses and the
    /// host's simplest path use; richer layouts use [`with_zones`](Dashboard::with_zones).
    pub fn new(widgets: Vec<Box<dyn Widget>>) -> Self {
        Self {
            left: widgets,
            center: Vec::new(),
            right: Vec::new(),
            margin: 0,
            gap: 0,
        }
    }

    /// Build a dashboard from its three zones, with no margin or gap.
    pub fn with_zones(
        left: Vec<Box<dyn Widget>>,
        center: Vec<Box<dyn Widget>>,
        right: Vec<Box<dyn Widget>>,
    ) -> Self {
        Self {
            left,
            center,
            right,
            margin: 0,
            gap: 0,
        }
    }

    /// Set the bar `margin` (inset around the pill row) and the `gap` (between
    /// adjacent pills within a zone), in logical pixels, used by
    /// [`layout`](Dashboard::layout). Both default to zero.
    pub fn with_spacing(mut self, margin: u32, gap: u32) -> Self {
        self.margin = margin;
        self.gap = gap;
        self
    }

    /// Broadcast `msg` to every widget in every zone.
    ///
    /// Returns `true` if at least one widget reported a visible change, i.e. the
    /// frame is now dirty and should be redrawn. Every widget is updated even
    /// once a change is seen, so each keeps its state current.
    pub fn update(&mut self, msg: &Msg) -> bool {
        let mut dirty = false;
        for widget in self
            .left
            .iter_mut()
            .chain(&mut self.center)
            .chain(&mut self.right)
        {
            dirty |= widget.update(msg);
        }
        dirty
    }

    /// Assign layout slots for a `width * height` surface.
    ///
    /// Each widget is measured against the inner (post-margin) height, then the
    /// three zones are packed: the left zone from the left margin rightward, the
    /// right zone flush against the far margin, and the center zone centered in
    /// the leftover middle — yielding to (and clipping against) the edges when
    /// the middle is too tight. Pills within a zone are separated by the
    /// [`gap`](Dashboard::with_spacing); the whole row is inset by the
    /// [`margin`](Dashboard::with_spacing). A widget that measures zero is parked
    /// at zero width and reserves no space. Geometry is authored in logical
    /// pixels and scaled here by the context's output scale.
    pub fn layout(&mut self, ctx: &mut RenderContext, width: u32, height: u32) {
        let scale = ctx.scale_factor();
        let margin = self.margin * scale;
        let gap = self.gap * scale;
        let top = margin;
        let inner_h = height.saturating_sub(2 * margin);

        // Pass 1: measure every widget against the inner height. The owned width
        // vectors release the immutable borrow of `self` before placement takes a
        // mutable one.
        let left_w = measure_zone(&self.left, ctx, inner_h);
        let center_w = measure_zone(&self.center, ctx, inner_h);
        let right_w = measure_zone(&self.right, ctx, inner_h);

        let left_total = cluster_width(&left_w, gap);
        let center_total = cluster_width(&center_w, gap);
        let right_total = cluster_width(&right_w, gap);

        // Pass 2: place each zone. Left from the margin, right flush against the
        // far margin.
        let left_start = margin;
        place_cluster(&mut self.left, &left_w, left_start, gap, top, inner_h);

        let right_edge = width.saturating_sub(margin);
        let right_start = right_edge.saturating_sub(right_total);
        place_cluster(&mut self.right, &right_w, right_start, gap, top, inner_h);

        // The middle region the center may occupy, kept clear of both clusters
        // and their adjoining gaps.
        let lo = if left_total > 0 {
            left_start + left_total + gap
        } else {
            left_start
        };
        let hi = if right_total > 0 {
            right_start.saturating_sub(gap)
        } else {
            right_edge
        };

        if center_total > 0 && hi.saturating_sub(lo) >= center_total {
            let ideal = width.saturating_sub(center_total) / 2;
            let max_start = hi - center_total;
            let start = ideal.clamp(lo, max_start);
            place_cluster(&mut self.center, &center_w, start, gap, top, inner_h);
        } else {
            // No room (or nothing to show): the center yields — every center
            // widget collapses to zero width.
            let zeros = vec![0u32; center_w.len()];
            place_cluster(&mut self.center, &zeros, lo, gap, top, inner_h);
        }
    }

    /// Clear the background and paint every widget, left zone then center then
    /// right.
    pub fn draw(&self, ctx: &mut RenderContext) {
        ctx.fill_background();
        for widget in self.left.iter().chain(&self.center).chain(&self.right) {
            widget.draw(ctx);
        }
    }

    /// Route a `button` click at surface pixel `(px, py)` to the widgets.
    ///
    /// Returns the [`Command`] of the first widget (in zone then draw order) that
    /// claims the pixel, or `None` if none do — clicks on empty space or
    /// display-only widgets produce nothing. Zones never overlap after
    /// [`layout`](Dashboard::layout), so order only breaks ties on empty space.
    pub fn on_click(&self, px: u32, py: u32, button: ClickButton) -> Option<Command> {
        self.left
            .iter()
            .chain(&self.center)
            .chain(&self.right)
            .find_map(|widget| widget.on_click(px, py, button))
    }

    /// Whether any widget exposes a primary or secondary click at `(px, py)`.
    pub fn is_clickable_at(&self, px: u32, py: u32) -> bool {
        self.on_click(px, py, ClickButton::Left).is_some()
            || self.on_click(px, py, ClickButton::Right).is_some()
    }

    /// Route one logical scroll step to the widget under the pointer.
    pub fn on_scroll(&self, px: u32, py: u32, direction: ScrollDirection) -> Option<Command> {
        self.left
            .iter()
            .chain(&self.center)
            .chain(&self.right)
            .find_map(|widget| widget.on_scroll(px, py, direction))
    }

    /// Resolve tooltip content for the widget under `(px, py)`.
    pub fn tooltip_at(&self, px: u32, py: u32) -> Option<Tooltip> {
        self.left
            .iter()
            .chain(&self.center)
            .chain(&self.right)
            .find_map(|widget| widget.tooltip_at(px, py))
    }
}

/// Measure every widget in `widgets` against the inner pill `height`, returning a
/// width per widget (parallel to the slice). A width of zero means the widget has
/// nothing to show and should reserve no slot.
fn measure_zone(widgets: &[Box<dyn Widget>], ctx: &mut RenderContext, height: u32) -> Vec<u32> {
    let mut widths = Vec::with_capacity(widgets.len());
    for widget in widgets {
        widths.push(widget.measure(ctx, height));
    }
    widths
}

/// The total width a zone occupies: the sum of its pill widths plus one `gap`
/// between each adjacent non-empty pair. Zero-width widgets reserve nothing and
/// introduce no gap.
fn cluster_width(widths: &[u32], gap: u32) -> u32 {
    let count = widths.iter().filter(|&&w| w > 0).count() as u32;
    let sum: u32 = widths.iter().sum();
    sum + gap * count.saturating_sub(1)
}

/// Assign bounds to a zone's widgets, packed left to right from `x`, each pill
/// `height` tall at `top`, separated by `gap`. A zero-width widget is parked at
/// the cursor with zero width (it draws nothing) and neither advances the cursor
/// nor adds a gap.
fn place_cluster(
    widgets: &mut [Box<dyn Widget>],
    widths: &[u32],
    mut x: u32,
    gap: u32,
    top: u32,
    height: u32,
) {
    let mut placed = false;
    for (widget, &w) in widgets.iter_mut().zip(widths) {
        if w == 0 {
            widget.set_bounds(Bounds::new(x, top, 0, height));
            continue;
        }
        if placed {
            x += gap;
        }
        widget.set_bounds(Bounds::new(x, top, w, height));
        x += w;
        placed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> Msg {
        Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, h, m, s).unwrap())
    }

    /// A workspace widget seeded with a single id, so it deterministically
    /// measures one square cell as wide as the laid-out row is tall, regardless
    /// of the font.
    fn one_workspace(id: i32) -> Box<dyn Widget> {
        let mut ws = WorkspaceWidget::new(Bounds::new(0, 0, 1, 1));
        ws.update(&Msg::Workspaces(Workspaces::new([id], id)));
        Box::new(ws)
    }

    #[test]
    fn update_is_dirty_only_when_a_widget_changes() {
        let mut dash = Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 320, 32)))]);

        // First message paints from the empty initial state: dirty.
        assert!(dash.update(&at(12, 0, 0)));
        // A later second in the same minute: no visible change, not dirty.
        assert!(!dash.update(&at(12, 0, 30)));
        // A new minute flips the displayed text: dirty again.
        assert!(dash.update(&at(12, 1, 0)));
    }

    #[test]
    fn empty_dashboard_never_reports_dirty() {
        let mut dash = Dashboard::new(vec![]);
        assert!(!dash.update(&at(12, 0, 0)));
    }

    #[test]
    fn layout_packs_left_and_right_zones_to_their_edges() {
        let mut dash =
            Dashboard::with_zones(vec![one_workspace(1)], vec![], vec![one_workspace(2)]);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // The left workspace packs from the origin; the right one packs flush
        // against the far edge (200 - 32 = 168), each a 32px square content cell.
        assert_eq!(dash.left[0].bounds(), Bounds::new(0, 0, 32, 32));
        assert_eq!(dash.right[0].bounds(), Bounds::new(168, 0, 32, 32));
    }

    #[test]
    fn layout_centers_the_center_zone_between_the_edges() {
        let mut dash = Dashboard::with_zones(vec![], vec![one_workspace(1)], vec![]);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // A lone 32px center cell centers on the surface: (200 - 32) / 2 = 84.
        assert_eq!(dash.center[0].bounds(), Bounds::new(84, 0, 32, 32));
    }

    #[test]
    fn layout_reserves_the_gap_between_pills_in_a_zone() {
        let mut dash =
            Dashboard::with_zones(vec![one_workspace(1), one_workspace(2)], vec![], vec![])
                .with_spacing(0, 10);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // First pill at the origin; the second starts one 10px gap after it
        // (32 + 10 = 42).
        assert_eq!(dash.left[0].bounds(), Bounds::new(0, 0, 32, 32));
        assert_eq!(dash.left[1].bounds(), Bounds::new(42, 0, 32, 32));
    }

    #[test]
    fn layout_insets_the_row_by_the_bar_margin() {
        let mut dash =
            Dashboard::with_zones(vec![one_workspace(1)], vec![], vec![]).with_spacing(4, 0);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // Margin 4 pushes the pill in by 4 and shrinks the row height by 8; the
        // square cell is then as wide as that inner height (24).
        assert_eq!(dash.left[0].bounds(), Bounds::new(4, 4, 24, 24));
    }

    #[test]
    fn an_unmeasured_widget_reserves_no_slot() {
        // A clock with no time yet measures zero, so it is parked at zero width
        // and the workspace after it still starts at the origin.
        let clock = ClockWidget::new(Bounds::new(0, 0, 1, 1));
        let mut dash =
            Dashboard::with_zones(vec![Box::new(clock), one_workspace(1)], vec![], vec![]);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        assert_eq!(dash.left[0].bounds().width, 0);
        assert_eq!(dash.left[1].bounds(), Bounds::new(0, 0, 32, 32));
    }

    #[test]
    fn on_click_routes_to_the_zone_that_owns_the_pixel() {
        // A display-only clock in the center, an interactive workspace on the
        // right edge: a click on the right switches, a click mid-surface (where
        // the empty clock holds nothing) commands nothing.
        let mut dash = Dashboard::with_zones(
            vec![],
            vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1)))],
            vec![one_workspace(1)],
        );
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        // The right workspace cell owns [168, 200).
        assert_eq!(
            dash.on_click(180, 16, ClickButton::Left),
            Some(Command::SwitchWorkspace(1))
        );
        assert_eq!(dash.on_click(100, 16, ClickButton::Left), None);
    }

    #[test]
    fn on_click_threads_the_button_through_to_the_widget() {
        // The workspace widget only answers the primary button, so the same
        // pixel that switches on a left click stays inert on a right click —
        // proving the dashboard hands the button through unchanged.
        let mut dash = Dashboard::with_zones(vec![], vec![], vec![one_workspace(1)]);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        assert_eq!(
            dash.on_click(180, 16, ClickButton::Left),
            Some(Command::SwitchWorkspace(1))
        );
        assert_eq!(dash.on_click(180, 16, ClickButton::Right), None);
    }

    #[test]
    fn clickable_regions_match_widgets_with_click_actions() {
        let mut dash = Dashboard::with_zones(vec![], vec![], vec![one_workspace(1)]);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);

        assert!(dash.is_clickable_at(180, 16));
        assert!(!dash.is_clickable_at(100, 16));
    }

    #[test]
    fn on_click_on_empty_dashboard_is_none() {
        let dash = Dashboard::new(vec![]);
        assert_eq!(dash.on_click(0, 0, ClickButton::Left), None);
    }

    #[test]
    fn draw_clears_background_each_frame() {
        let mut dash = Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 320, 32)))]);
        dash.update(&at(12, 0, 0));
        let mut ctx = RenderContext::new(320, 32);
        dash.draw(&mut ctx);
        // Far corner is clear of the left-aligned clock: opaque dark background.
        let px = ctx.pixels();
        let last = &px[px.len() - 4..];
        assert!(last[0] < 0x30 && last[1] < 0x30 && last[2] < 0x30);
        assert_eq!(last[3], 0xFF);
    }
}
