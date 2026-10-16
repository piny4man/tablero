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
    /// The power-profiles-daemon state, with an optional platform overlay;
    /// `None` means neither the daemon nor a known hardware backend is available.
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
    /// Select a power profile (PPD, plus a platform helper when detected).
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

/// What the pointer is over: everything a pointer motion needs to know, resolved
/// by [`Dashboard::hover_at`] in one hit-test.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hover {
    /// Whether a primary or secondary click here would do something.
    pub clickable: bool,
    /// The tooltip to show here, if any.
    pub tooltip: Option<Tooltip>,
    /// The hovered widget's index (zone order) when it is interactive —
    /// clickable or with a tooltip — and so worth highlighting.
    pub widget: Option<usize>,
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

/// Hover highlight colors from a `[widget.<name>.hover]` table. Each unset
/// field keeps the widget's current color, except the background, which
/// defaults to a tenth of the foreground mixed into the pill (or bar)
/// background — a subtle lift that works on any theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoverColors {
    /// Pill fill while hovered.
    pub background: Option<(u8, u8, u8, u8)>,
    /// Text/glyph color while hovered.
    pub foreground: Option<(u8, u8, u8, u8)>,
    /// Border color while hovered.
    pub border: Option<(u8, u8, u8, u8)>,
}

impl HoverColors {
    /// The hovered form of `colors`, painted on a bar of `bar_background`.
    pub fn apply(self, colors: StateColors, bar_background: (u8, u8, u8, u8)) -> StateColors {
        let mix =
            |base: u8, ink: u8| ((u32::from(base) * 90 + u32::from(ink) * 10 + 50) / 100) as u8;
        let (br, bg, bb, ba) = colors.background.unwrap_or(bar_background);
        let (fr, fg, fb, fa) = colors.foreground;
        StateColors {
            background: Some(self.background.unwrap_or((
                mix(br, fr),
                mix(bg, fg),
                mix(bb, fb),
                mix(ba, fa),
            ))),
            foreground: self.foreground.unwrap_or(colors.foreground),
            border: self.border.or(colors.border),
        }
    }
}

/// The colors a pill paints with: `colors`, or their hovered form while the
/// pointer is over the widget.
fn paint_colors(ctx: &RenderContext, style: &WidgetStyle, colors: StateColors) -> StateColors {
    if ctx.hovered() {
        style.hover.apply(colors, ctx.settings().background)
    } else {
        colors
    }
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
    /// Icon size in logical pixels: the built-in icon's edge, or a text
    /// glyph's font size. `None` follows the bar font size.
    pub icon_size: Option<u32>,
    /// Percent below which a discharging battery uses [`warn`](WidgetStyle::warn).
    pub warn_threshold: u32,
    /// Colors for a low/warning state (e.g. low battery).
    pub warn: StateColors,
    /// Colors for an attention state (e.g. a tray item needing attention).
    pub attention: StateColors,
    /// Colors for a battery while it is charging.
    pub charging: StateColors,
    /// Colors while the pointer is over an interactive widget.
    pub hover: HoverColors,
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
            icon_size: None,
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
            hover: HoverColors::default(),
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

/// Check that every `{name}` placeholder in `format` is one of `allowed` and
/// that braces pair up. The error reads as the tail of a sentence naming the
/// config key, e.g. `contains unsupported placeholder `{nope}``.
pub fn validate_placeholders(format: &str, allowed: &[&str]) -> Result<(), String> {
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
        if !allowed.contains(&placeholder) {
            return Err(format!(
                "contains unsupported placeholder `{{{placeholder}}}`"
            ));
        }
        rest = &rest[close + 1..];
    }
    Ok(())
}

/// Substitute each `{name}` in `format` with its value from `values`, in one
/// pass. Placeholders not listed (notably `{icon}`) are kept verbatim so the
/// icon layout helpers can still find them.
pub(crate) fn expand(format: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(format.len());
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        let value = tail.find('}').and_then(|close| {
            let name = &tail[1..close];
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value, close))
        });
        match value {
            Some((value, close)) => {
                out.push_str(value);
                rest = &tail[close + 1..];
            }
            None => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
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
    let colors = paint_colors(ctx, style, colors);
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
    let colors = paint_colors(ctx, style, colors);
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
            let edge = builtin_edge(ctx, style);
            let boxes = icon_boxes(ctx, edge, template);
            measure_boxes(ctx, style, &boxes, icon_gap(edge))
        }
        ResolvedIcon::Text(glyph) if style.icon_size.is_some() => {
            let (boxes, gap, _) = sized_glyph_boxes(ctx, style, glyph, template);
            measure_boxes(ctx, style, &boxes, gap)
        }
        ResolvedIcon::None => measure_text_pill(ctx, style, &icon_as_text(template, None)),
        ResolvedIcon::Text(s) => measure_text_pill(ctx, style, &icon_as_text(template, Some(s))),
    }
}

/// The built-in icon edge in physical pixels: the style's `icon-size`, or the
/// bar font size.
fn builtin_edge(ctx: &RenderContext, style: &WidgetStyle) -> u32 {
    style
        .icon_size
        .map_or_else(|| ctx.icon_edge(), |size| size * ctx.scale_factor())
}

/// Boxes for a text glyph enlarged to the style's `icon-size`: the glyph box
/// is as wide as the glyph at that size, while labels keep the bar font.
/// Returns the boxes, the gap between them, and the glyph's font size.
fn sized_glyph_boxes(
    ctx: &mut RenderContext,
    style: &WidgetStyle,
    glyph: &str,
    template: &str,
) -> (Vec<ContentBox>, u32, Option<f32>) {
    let size = style
        .icon_size
        .map(|size| (size * ctx.scale_factor()) as f32);
    let width = ctx.measure_text_sized(glyph.trim(), size);
    let gap = icon_gap(size.map_or_else(|| ctx.icon_edge(), |size| size as u32));
    (icon_boxes(ctx, width, template), gap, size)
}

/// The pill width for `boxes` separated by `gap`, plus the style's padding;
/// zero when there is nothing to show.
fn measure_boxes(ctx: &RenderContext, style: &WidgetStyle, boxes: &[ContentBox], gap: u32) -> u32 {
    if boxes.is_empty() {
        return 0;
    }
    let content: u32 = boxes.iter().map(|b| b.width).sum::<u32>() + gap * (boxes.len() as u32 - 1);
    content + 2 * style.padding * ctx.scale_factor()
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
        ResolvedIcon::Text(glyph) if style.icon_size.is_some() => {
            if bounds.width == 0 || bounds.height == 0 {
                return;
            }
            let (boxes, gap, size) = sized_glyph_boxes(ctx, style, glyph, template);
            let glyph = glyph.trim();
            draw_boxes(
                ctx,
                style,
                bounds,
                &boxes,
                gap,
                colors,
                |ctx, _, slot, color| {
                    ctx.draw_text_centered_sized(glyph, slot, color, size);
                },
            );
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
    let edge = builtin_edge(ctx, style);
    let boxes = icon_boxes(ctx, edge, template);
    draw_boxes(
        ctx,
        style,
        bounds,
        &boxes,
        icon_gap(edge),
        colors,
        |ctx, slot_index, slot, color| {
            let builtin = icons[slot_index.min(icons.len() - 1)];
            let iy = slot.y + slot.height.saturating_sub(edge) / 2;
            ctx.draw_builtin_icon(builtin, Bounds::new(slot.x, iy, edge, edge), color);
        },
    );
}

/// Paint the pill behind `boxes`, then lay them out left to right `gap`
/// apart: text runs at the bar font, and each icon slot handed to
/// `draw_icon` with its index among the icon slots, its full-height box, and
/// the foreground color.
fn draw_boxes(
    ctx: &mut RenderContext,
    style: &WidgetStyle,
    bounds: Bounds,
    boxes: &[ContentBox],
    gap: u32,
    colors: StateColors,
    mut draw_icon: impl FnMut(&mut RenderContext, usize, Bounds, (u8, u8, u8, u8)),
) {
    if boxes.is_empty() {
        return;
    }
    let colors = paint_colors(ctx, style, colors);
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
    let mut x = bounds.x + pad;
    let mut icon_slot = 0;
    for (index, content) in boxes.iter().enumerate() {
        if index > 0 {
            x += gap;
        }
        match &content.text {
            None => {
                let slot = Bounds::new(x, bounds.y, content.width, bounds.height);
                draw_icon(ctx, icon_slot, slot, colors.foreground);
                icon_slot += 1;
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
    /// Which widgets (zone order) reported a change since the last
    /// [`take_damage`](Dashboard::take_damage).
    changed: Vec<bool>,
    /// Every widget's slot as of the last `take_damage`; `None` until a frame
    /// has been accounted for, or after [`invalidate`](Dashboard::invalidate).
    painted: Option<Vec<Bounds>>,
    /// The widget (zone order) under the pointer, painted highlighted.
    hovered: Option<usize>,
}

/// The part of a freshly painted frame that differs from the one before it.
///
/// It bounds what [`draw_damage`](Dashboard::draw_damage) repaints, what is
/// copied into the shared-memory buffer, and how much the compositor
/// recomposites (its blur and blend work).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Damage {
    /// Anything may have changed: first frame, moved slots, or a forced repaint.
    Full,
    /// Only pixels inside this rectangle changed.
    Region(Bounds),
}

impl Damage {
    /// The damage covering both `self` and `other`.
    pub fn union(self, other: Self) -> Self {
        match (self, other) {
            (Self::Region(a), Self::Region(b)) => Self::Region(a.union(&b)),
            _ => Self::Full,
        }
    }
}

impl Dashboard {
    /// Build a dashboard whose widgets all live in the left zone, with no margin
    /// or gap. This is the single-zone constructor the redraw harnesses and the
    /// host's simplest path use; richer layouts use [`with_zones`](Dashboard::with_zones).
    pub fn new(widgets: Vec<Box<dyn Widget>>) -> Self {
        Self::with_zones(widgets, Vec::new(), Vec::new())
    }

    /// Build a dashboard from its three zones, with no margin or gap.
    pub fn with_zones(
        left: Vec<Box<dyn Widget>>,
        center: Vec<Box<dyn Widget>>,
        right: Vec<Box<dyn Widget>>,
    ) -> Self {
        let widgets = left.len() + center.len() + right.len();
        Self {
            left,
            center,
            right,
            margin: 0,
            gap: 0,
            changed: vec![false; widgets],
            painted: None,
            hovered: None,
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
    #[hotpath::measure(impl_type = "Dashboard")]
    pub fn update(&mut self, msg: &Msg) -> bool {
        let mut dirty = false;
        for (widget, changed) in self
            .left
            .iter_mut()
            .chain(&mut self.center)
            .chain(&mut self.right)
            .zip(&mut self.changed)
        {
            let updated = widget.update(msg);
            *changed |= updated;
            dirty |= updated;
        }
        dirty
    }

    /// Account for a frame: what it changed relative to the frame accounted for
    /// by the previous call. Call after [`layout`](Dashboard::layout), once per
    /// committed frame.
    ///
    /// A widget paints only inside its slot, so while every slot stays put the
    /// damage is the union of the changed widgets' slots. A moved or resized
    /// slot shifts its neighbours and exposes background, and a repaint nothing
    /// asked for has an unknown reason; both damage the whole bar.
    pub fn take_damage(&mut self) -> Damage {
        let bounds: Vec<Bounds> = self.widgets().map(|widget| widget.bounds()).collect();
        let region = if self.painted.as_ref() == Some(&bounds) {
            bounds
                .iter()
                .zip(&self.changed)
                .filter(|(_, changed)| **changed)
                .map(|(bounds, _)| *bounds)
                .reduce(|damage, bounds| damage.union(&bounds))
        } else {
            None
        };
        self.painted = Some(bounds);
        self.changed.fill(false);
        region.map_or(Damage::Full, Damage::Region)
    }

    /// Forget the last accounted frame, so the next damages the whole bar. For a
    /// frame that was painted but never reached the compositor.
    pub fn invalidate(&mut self) {
        self.painted = None;
    }

    /// Highlight the widget at zone-order `index`, or none. Returns `true`
    /// when the highlight moved; only the old and new slots are marked
    /// changed, so the next frame repaints just those two pills. An index
    /// past the end (a hover left over from before a reload) counts as none.
    pub fn set_hovered(&mut self, index: Option<usize>) -> bool {
        let index = index.filter(|&i| i < self.changed.len());
        if index == self.hovered {
            return false;
        }
        for i in [self.hovered, index].into_iter().flatten() {
            self.changed[i] = true;
        }
        self.hovered = index;
        true
    }

    fn widgets(&self) -> impl Iterator<Item = &Box<dyn Widget>> {
        self.left.iter().chain(&self.center).chain(&self.right)
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
    #[hotpath::measure(impl_type = "Dashboard")]
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
    #[hotpath::measure(impl_type = "Dashboard")]
    pub fn draw(&self, ctx: &mut RenderContext) {
        ctx.fill_background();
        for (index, widget) in self.widgets().enumerate() {
            ctx.set_hovered(self.hovered == Some(index));
            widget.draw(ctx);
        }
        ctx.set_hovered(false);
    }

    /// Repaint only what `damage` covers, on top of the previous frame still in
    /// `ctx`. Returns the region actually repainted.
    ///
    /// A widget repaints its whole slot over a cleared background, so the
    /// region first grows until every slot it touches lies inside it; nothing
    /// outside is painted twice, and the result matches a full repaint.
    #[hotpath::measure(impl_type = "Dashboard")]
    pub fn draw_damage(&self, ctx: &mut RenderContext, damage: Damage) -> Damage {
        let Damage::Region(mut region) = damage else {
            self.draw(ctx);
            return Damage::Full;
        };
        loop {
            let grown = self
                .widgets()
                .map(|widget| widget.bounds())
                .filter(|bounds| bounds.intersects(&region))
                .fold(region, |region, bounds| region.union(&bounds));
            if grown == region {
                break;
            }
            region = grown;
        }
        ctx.fill_background_in(region);
        for (index, widget) in self.widgets().enumerate() {
            if widget.bounds().intersects(&region) {
                ctx.set_hovered(self.hovered == Some(index));
                widget.draw(ctx);
            }
        }
        ctx.set_hovered(false);
        Damage::Region(region)
    }

    /// Route a `button` click at surface pixel `(px, py)` to the widgets.
    ///
    /// Returns the [`Command`] of the first widget (in zone then draw order) that
    /// claims the pixel, or `None` if none do — clicks on empty space or
    /// display-only widgets produce nothing. Zones never overlap after
    /// [`layout`](Dashboard::layout), so order only breaks ties on empty space.
    #[hotpath::measure(impl_type = "Dashboard")]
    pub fn on_click(&self, px: u32, py: u32, button: ClickButton) -> Option<Command> {
        self.left
            .iter()
            .chain(&self.center)
            .chain(&self.right)
            .find_map(|widget| widget.on_click(px, py, button))
    }

    /// Resolve what the pointer is over at `(px, py)`.
    ///
    /// Pointer motion arrives far more often than anything else the bar handles,
    /// so this finds the one widget whose slot holds the pixel and asks only it,
    /// rather than offering the pixel to every widget once per question. Slots
    /// never overlap after [`layout`](Dashboard::layout), and a widget's
    /// interactive regions lie inside its slot.
    #[hotpath::measure(impl_type = "Dashboard")]
    pub fn hover_at(&self, px: u32, py: u32) -> Hover {
        let Some((index, widget)) = self
            .widgets()
            .enumerate()
            .find(|(_, widget)| widget.bounds().contains(px, py))
        else {
            return Hover::default();
        };
        let clickable = widget.on_click(px, py, ClickButton::Left).is_some()
            || widget.on_click(px, py, ClickButton::Right).is_some();
        let tooltip = widget.tooltip_at(px, py);
        Hover {
            widget: (clickable || tooltip.is_some()).then_some(index),
            clickable,
            tooltip,
        }
    }

    /// Route one logical scroll step to the widget under the pointer.
    #[hotpath::measure(impl_type = "Dashboard")]
    pub fn on_scroll(&self, px: u32, py: u32, direction: ScrollDirection) -> Option<Command> {
        self.left
            .iter()
            .chain(&self.center)
            .chain(&self.right)
            .find_map(|widget| widget.on_scroll(px, py, direction))
    }
}

/// Measure every widget in `widgets` against the inner pill `height`, returning a
/// width per widget (parallel to the slice). A width of zero means the widget has
/// nothing to show and should reserve no slot.
#[hotpath::measure]
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
    fn validate_placeholders_accepts_only_allowed_names() {
        assert_eq!(
            validate_placeholders("{icon}{percent}%", &["icon", "percent"]),
            Ok(())
        );
        assert_eq!(
            validate_placeholders("{icon} {nope}", &["icon"]),
            Err("contains unsupported placeholder `{nope}`".to_string())
        );
    }

    #[test]
    fn validate_placeholders_rejects_unmatched_braces() {
        assert_eq!(
            validate_placeholders("{icon", &["icon"]),
            Err("contains an unmatched `{`".to_string())
        );
        assert_eq!(
            validate_placeholders("icon}", &["icon"]),
            Err("contains an unmatched `}`".to_string())
        );
    }

    #[test]
    fn expand_fills_values_and_keeps_icon_marker() {
        assert_eq!(
            expand(
                "{icon}{percent}% {state}",
                &[("percent", "42"), ("state", "on")]
            ),
            "{icon}42% on"
        );
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

        assert!(dash.hover_at(180, 16).clickable);
        assert!(!dash.hover_at(100, 16).clickable);
    }

    /// Workspaces (clickable, no tooltip), a clock (neither) and Hypridle
    /// (clickable with a tooltip), laid out on a 400x32 bar.
    fn hoverable_dashboard() -> Dashboard {
        let mut workspaces = WorkspaceWidget::new(Bounds::new(0, 0, 1, 1));
        workspaces.update(&Msg::Workspaces(Workspaces::new([1, 2], 1)));
        let mut dash = Dashboard::with_zones(
            vec![Box::new(workspaces)],
            vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1)))],
            vec![Box::new(HypridleWidget::new(Bounds::new(0, 0, 1, 1)))],
        );
        dash.update(&at(12, 0, 0));
        dash.update(&Msg::Hypridle(Hypridle::new(true)));
        dash.layout(&mut RenderContext::new(400, 32), 400, 32);
        dash
    }

    #[test]
    fn hovering_a_widget_reports_its_click_action_and_tooltip_together() {
        let dash = hoverable_dashboard();
        let hypridle = dash.right[0].bounds();
        let (px, py) = (
            hypridle.x + hypridle.width / 2,
            hypridle.y + hypridle.height / 2,
        );

        let hover = dash.hover_at(px, py);

        assert!(hover.clickable);
        assert_eq!(
            hover.tooltip,
            Some(Tooltip {
                text: "Hypridle active".to_owned(),
                bounds: hypridle,
            })
        );
    }

    #[test]
    fn hovering_empty_bar_space_reports_nothing() {
        let dash = hoverable_dashboard();
        let gap = dash.left[0].bounds().x + dash.left[0].bounds().width + 1;
        assert!(
            !dash.center[0].bounds().contains(gap, 16),
            "probe is in a gap"
        );

        assert_eq!(dash.hover_at(gap, 16), Hover::default());
        assert_eq!(Dashboard::new(vec![]).hover_at(0, 0), Hover::default());
    }

    #[test]
    fn hover_agrees_with_asking_every_widget_at_every_pixel() {
        // `hover_at` consults only the widget whose slot holds the pixel. That is
        // sound only while no widget answers outside its slot, so compare it
        // against the exhaustive scan it stands in for, across the whole bar.
        let dash = hoverable_dashboard();
        for py in 0..32 {
            for px in 0..400 {
                let clickable = dash.on_click(px, py, ClickButton::Left).is_some()
                    || dash.on_click(px, py, ClickButton::Right).is_some();
                let tooltip = dash.widgets().find_map(|widget| widget.tooltip_at(px, py));
                let widget = (clickable || tooltip.is_some())
                    .then(|| dash.widgets().position(|w| w.bounds().contains(px, py)))
                    .flatten();
                let exhaustive = Hover {
                    clickable,
                    tooltip,
                    widget,
                };
                assert_eq!(dash.hover_at(px, py), exhaustive, "at ({px}, {py})");
            }
        }
    }

    #[test]
    fn hover_names_only_interactive_widgets() {
        let dash = hoverable_dashboard();
        let center = |b: Bounds| (b.x + b.width / 2, b.y + b.height / 2);
        let (px, py) = center(dash.right[0].bounds());
        assert_eq!(dash.hover_at(px, py).widget, Some(2));
        // The clock neither clicks nor shows a tooltip: nothing to highlight.
        let (px, py) = center(dash.center[0].bounds());
        assert_eq!(dash.hover_at(px, py).widget, None);
    }

    #[test]
    fn changing_the_hovered_widget_damages_only_the_old_and_new_slots() {
        let mut dash = hoverable_dashboard();
        let mut ctx = RenderContext::new(400, 32);
        dash.layout(&mut ctx, 400, 32);
        let _ = dash.take_damage();

        assert!(!dash.set_hovered(None));
        assert!(dash.set_hovered(Some(2)));
        assert_eq!(dash.take_damage(), Damage::Region(dash.right[0].bounds()));
        assert!(!dash.set_hovered(Some(2)));
        assert!(dash.set_hovered(Some(0)));
        assert_eq!(
            dash.take_damage(),
            Damage::Region(dash.left[0].bounds().union(&dash.right[0].bounds()))
        );
        assert!(dash.set_hovered(None));
        assert_eq!(dash.take_damage(), Damage::Region(dash.left[0].bounds()));
        // An out-of-range index (a stale hover after a reload) is ignored.
        assert!(!dash.set_hovered(Some(99)));
    }

    #[test]
    fn a_hovered_pill_paints_its_hover_colors() {
        let style = WidgetStyle {
            hover: HoverColors {
                background: Some((0xFF, 0, 0, 0xFF)),
                ..HoverColors::default()
            },
            ..WidgetStyle::default()
        };
        let mut idle = HypridleWidget::new(Bounds::new(0, 0, 1, 1)).with_style(style);
        idle.update(&Msg::Hypridle(Hypridle::new(true)));
        let mut dash = Dashboard::new(vec![Box::new(idle)]);
        let mut ctx = RenderContext::new(200, 32);
        dash.layout(&mut ctx, 200, 32);
        let slot = dash.left[0].bounds();
        let probe = |ctx: &RenderContext| {
            let i = ((slot.y + slot.height / 2) * 200 + slot.x + 2) as usize * 4;
            ctx.pixels()[i..i + 4].to_vec()
        };

        dash.draw(&mut ctx);
        assert_ne!(probe(&ctx), [0xFF, 0, 0, 0xFF]);
        dash.set_hovered(Some(0));
        dash.draw(&mut ctx);
        assert_eq!(probe(&ctx), [0xFF, 0, 0, 0xFF]);
        dash.set_hovered(None);
        dash.draw(&mut ctx);
        assert_ne!(probe(&ctx), [0xFF, 0, 0, 0xFF]);
    }

    #[test]
    fn default_hover_mixes_a_tenth_of_the_foreground_into_the_background() {
        let hover = HoverColors::default();
        let base = StateColors {
            background: Some((0, 0, 0, 0xFF)),
            foreground: (200, 100, 50, 0xFF),
            border: None,
        };
        let colors = hover.apply(base, (10, 10, 10, 0xFF));
        assert_eq!(colors.background, Some((20, 10, 5, 0xFF)));
        assert_eq!(colors.foreground, base.foreground);
        // No pill: the mix starts from the bar background.
        let flat = hover.apply(
            StateColors {
                background: None,
                ..base
            },
            (0, 0, 0, 0),
        );
        assert_eq!(flat.background, Some((20, 10, 5, 26)));
        let custom = HoverColors {
            background: Some((1, 2, 3, 4)),
            foreground: Some((5, 6, 7, 8)),
            border: Some((9, 9, 9, 9)),
        }
        .apply(base, (0, 0, 0, 0));
        assert_eq!(
            custom,
            StateColors {
                background: Some((1, 2, 3, 4)),
                foreground: (5, 6, 7, 8),
                border: Some((9, 9, 9, 9)),
            }
        );
    }

    /// Rows of `ctx` holding any pixel brighter than the dark background.
    fn ink_rows(ctx: &RenderContext, width: u32) -> u32 {
        ctx.pixels()
            .chunks(width as usize * 4)
            .filter(|row| row.chunks(4).any(|px| px[0] > 0x80))
            .count() as u32
    }

    #[test]
    fn icon_size_sizes_a_builtin_icon_slot() {
        let mut ctx = RenderContext::new(200, 48);
        let style = WidgetStyle {
            icon_size: Some(30),
            padding: 0,
            ..WidgetStyle::default()
        };
        let icon = ResolvedIcon::Builtin(BuiltinIcon::Power);
        assert_eq!(measure_icon_content(&mut ctx, &style, &icon, "{icon}"), 30);
        let default = WidgetStyle {
            padding: 0,
            ..WidgetStyle::default()
        };
        assert_eq!(
            measure_icon_content(&mut ctx, &default, &icon, "{icon}"),
            ctx.icon_edge()
        );
    }

    #[test]
    fn icon_size_enlarges_a_text_glyph_but_not_its_label() {
        let draw = |icon_size: Option<u32>, template: &str| {
            let mut ctx = RenderContext::new(200, 48);
            let style = WidgetStyle {
                icon_size,
                ..WidgetStyle::default()
            };
            let icon = ResolvedIcon::Text("█".into());
            let width = measure_icon_content(&mut ctx, &style, &icon, template);
            ctx.fill_background();
            draw_icon_content(
                &mut ctx,
                &style,
                Bounds::new(0, 0, 200, 48),
                &icon,
                template,
                style.base_colors(),
            );
            (width, ink_rows(&ctx, 200))
        };
        let (small_w, small_h) = draw(None, "{icon}");
        let (big_w, big_h) = draw(Some(32), "{icon}");
        assert!(big_w > small_w, "{big_w} > {small_w}");
        assert!(big_h > small_h, "{big_h} > {small_h}");
        // A label next to the glyph keeps the bar font size: the pill grows
        // only by the glyph's extra width.
        let (label_small, _) = draw(None, "{icon}50%");
        let (label_big, _) = draw(Some(32), "{icon}50%");
        assert!(label_big > label_small);
        assert!(label_big - label_small < big_w);
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

    /// Workspaces on the left, a clock on the right: a workspace message changes
    /// only the first, a tick only the second, and neither moves a slot.
    fn workspaces_and_clock() -> (Dashboard, RenderContext) {
        let mut workspaces = WorkspaceWidget::new(Bounds::new(0, 0, 1, 1));
        workspaces.update(&Msg::Workspaces(Workspaces::new([1, 2], 1)));
        let mut dash = Dashboard::with_zones(
            vec![Box::new(workspaces)],
            vec![],
            vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1)))],
        );
        dash.update(&at(12, 0, 0));
        (dash, RenderContext::new(400, 32))
    }

    fn frame(dash: &mut Dashboard, ctx: &mut RenderContext) -> (Damage, Vec<u8>) {
        dash.layout(ctx, 400, 32);
        let damage = dash.take_damage();
        dash.draw(ctx);
        (damage, ctx.pixels().to_vec())
    }

    #[test]
    fn the_first_frame_damages_the_whole_bar() {
        let (mut dash, mut ctx) = workspaces_and_clock();
        assert_eq!(frame(&mut dash, &mut ctx).0, Damage::Full);
    }

    #[test]
    fn a_change_inside_fixed_slots_damages_only_the_changed_widgets() {
        let (mut dash, mut ctx) = workspaces_and_clock();
        frame(&mut dash, &mut ctx);

        assert!(dash.update(&Msg::Workspaces(Workspaces::new([1, 2], 2))));
        let workspaces = dash.left[0].bounds();
        assert_eq!(frame(&mut dash, &mut ctx).0, Damage::Region(workspaces));
    }

    #[test]
    fn widgets_changed_by_coalesced_messages_damage_the_union_of_their_slots() {
        let strip = |active| {
            let mut widget = WorkspaceWidget::new(Bounds::new(0, 0, 1, 1));
            widget.update(&Msg::Workspaces(Workspaces::new([1, 2], active)));
            widget
        };
        let mut dash =
            Dashboard::with_zones(vec![Box::new(strip(1))], vec![], vec![Box::new(strip(1))]);
        let mut ctx = RenderContext::new(400, 32);
        frame(&mut dash, &mut ctx);

        assert!(dash.update(&Msg::Workspaces(Workspaces::new([1, 2], 2))));
        let (damage, _) = frame(&mut dash, &mut ctx);
        let (left, right) = (dash.left[0].bounds(), dash.right[0].bounds());
        assert_eq!(damage, Damage::Region(left.union(&right)));
    }

    #[test]
    fn every_pixel_that_differs_between_frames_lies_inside_the_damage() {
        let (mut dash, mut ctx) = workspaces_and_clock();
        let (_, before) = frame(&mut dash, &mut ctx);
        dash.update(&Msg::Workspaces(Workspaces::new([1, 2], 2)));
        let (damage, after) = frame(&mut dash, &mut ctx);

        let Damage::Region(region) = damage else {
            panic!("fixed slots should give partial damage, got {damage:?}");
        };
        let differing: Vec<(u32, u32)> = before
            .as_chunks::<4>()
            .0
            .iter()
            .zip(after.as_chunks::<4>().0)
            .enumerate()
            .filter(|(_, (before, after))| before != after)
            .map(|(i, _)| (i as u32 % 400, i as u32 / 400))
            .collect();
        assert!(!differing.is_empty(), "the active workspace moved");
        assert!(
            differing.iter().all(|&(x, y)| region.contains(x, y)),
            "pixels outside {region:?} changed"
        );
    }

    #[test]
    fn a_slot_that_moves_or_resizes_damages_the_whole_bar() {
        let (mut dash, mut ctx) = workspaces_and_clock();
        frame(&mut dash, &mut ctx);
        // A third workspace widens the left slot.
        dash.update(&Msg::Workspaces(Workspaces::new([1, 2, 3], 1)));
        assert_eq!(frame(&mut dash, &mut ctx).0, Damage::Full);
    }

    #[test]
    fn a_repaint_nothing_asked_for_and_an_invalidated_frame_damage_the_whole_bar() {
        let (mut dash, mut ctx) = workspaces_and_clock();
        frame(&mut dash, &mut ctx);
        assert_eq!(frame(&mut dash, &mut ctx).0, Damage::Full);

        dash.update(&Msg::Workspaces(Workspaces::new([1, 2], 2)));
        dash.invalidate();
        assert_eq!(frame(&mut dash, &mut ctx).0, Damage::Full);
    }

    /// Repainting just the damage over the previous frame yields exactly the
    /// pixels a full repaint does, on opaque and translucent bars alike.
    #[test]
    fn a_damage_limited_repaint_matches_a_full_repaint() {
        for background in [(0x18, 0x18, 0x18, 0xFF), (0x10, 0x20, 0x30, 0x80)] {
            let settings = crate::render::RenderSettings {
                background,
                ..crate::render::RenderSettings::default()
            };
            let (mut dash, _) = workspaces_and_clock();
            let mut ctx = RenderContext::with_settings(400, 32, settings.clone());
            frame(&mut dash, &mut ctx);

            assert!(dash.update(&Msg::Workspaces(Workspaces::new([1, 2], 2))));
            dash.layout(&mut ctx, 400, 32);
            let damage = dash.take_damage();
            let repainted = dash.draw_damage(&mut ctx, damage);
            assert!(
                matches!(repainted, Damage::Region(region) if region.width < 400),
                "only part of the bar is repainted: {repainted:?}"
            );

            let mut full = RenderContext::with_settings(400, 32, settings);
            dash.layout(&mut full, 400, 32);
            dash.draw(&mut full);
            assert!(ctx.pixels() == full.pixels(), "partial repaint differs");
        }
    }

    #[test]
    fn damage_union_widens_regions_and_full_absorbs() {
        let a = Damage::Region(Bounds::new(0, 0, 10, 10));
        let b = Damage::Region(Bounds::new(20, 0, 10, 10));
        assert_eq!(a.union(b), Damage::Region(Bounds::new(0, 0, 30, 10)));
        assert_eq!(a.union(Damage::Full), Damage::Full);
        assert_eq!(Damage::Full.union(b), Damage::Full);
    }

    #[test]
    fn bounds_union_covers_both_rectangles() {
        let a = Bounds::new(10, 4, 20, 8);
        let b = Bounds::new(50, 0, 5, 30);
        assert_eq!(a.union(&b), Bounds::new(10, 0, 45, 30));
        assert_eq!(a.union(&a), a);
    }

    /// ARC-style workspace cells at buffer scale 2: translucent rounded fill,
    /// a border, and a fully transparent bar. Anti-aliased ink can land outside
    /// the slot the damage union reports; those pixels must still be covered or
    /// the compositor keeps the previous highlight.
    #[test]
    fn styled_workspace_pixels_at_scale_two_lie_inside_the_damage() {
        let style = WidgetStyle {
            background: Some((0x1c, 0x0e, 0x1e, 0xe8)),
            foreground: (0x9a, 0x8a, 0x98, 0xff),
            accent: (0x7d, 0xec, 0xff, 0xff),
            border: Some((0x3d, 0x1f, 0x3a, 0xff)),
            border_width: 1,
            radius: 2,
            ..WidgetStyle::default()
        };
        let mut workspaces = WorkspaceWidget::new(Bounds::new(0, 0, 1, 1)).with_style(style);
        workspaces.update(&Msg::Workspaces(Workspaces::new([1, 2], 1)));
        let mut dash = Dashboard::with_zones(
            vec![Box::new(workspaces)],
            vec![],
            vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 1, 1)))],
        )
        .with_spacing(4, 4);
        dash.update(&at(12, 0, 0));

        const WIDTH: u32 = 400;
        const HEIGHT: u32 = 76;
        let mut ctx = RenderContext::with_settings(
            WIDTH,
            HEIGHT,
            crate::render::RenderSettings {
                background: (0, 0, 0, 0),
                scale: 2,
                font_size: 30.0,
                ..crate::render::RenderSettings::default()
            },
        );

        let paint = |dash: &mut Dashboard, ctx: &mut RenderContext| {
            dash.layout(ctx, WIDTH, HEIGHT);
            let damage = dash.take_damage();
            dash.draw_damage(ctx, damage);
            (damage, ctx.pixels().to_vec())
        };
        let (_, before) = paint(&mut dash, &mut ctx);
        assert!(dash.update(&Msg::Workspaces(Workspaces::new([1, 2], 2))));
        let (damage, after) = paint(&mut dash, &mut ctx);

        let Damage::Region(region) = damage else {
            panic!("fixed slots should give partial damage, got {damage:?}");
        };
        let outside: Vec<(u32, u32)> = before
            .as_chunks::<4>()
            .0
            .iter()
            .zip(after.as_chunks::<4>().0)
            .enumerate()
            .filter(|(_, (before, after))| before != after)
            .map(|(i, _)| (i as u32 % WIDTH, i as u32 / WIDTH))
            .filter(|&(x, y)| !region.contains(x, y))
            .collect();
        assert!(
            outside.is_empty(),
            "styled workspace pixels outside {region:?}: {outside:?}"
        );

        // The partial repaint left exactly what a full repaint paints.
        let mut full = RenderContext::with_settings(WIDTH, HEIGHT, ctx.settings().clone());
        dash.layout(&mut full, WIDTH, HEIGHT);
        dash.draw(&mut full);
        assert!(after == full.pixels(), "partial repaint differs");
    }
}
