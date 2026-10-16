//! The bluetooth widget and its normalized power-state model.
//!
//! [`Bluetooth`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Bluetooth`]; [`BluetoothWidget`] renders it,
//! repainting only when the visible state or connected-device count actually
//! changes.
//!
//! The widget is always visible — even when no adapter is present it shows
//! the off icon, matching what a user expects to see on a desktop without
//! Bluetooth hardware. This is a deliberate departure from the
//! battery/network pattern, where an absent source reserves no slot: here the
//! slot is always reserved so the user can see *why* nothing is connected
//! (powered off, or no adapter) and so a configured on-click still has a
//! target. The pill stays compact — off, on, or connected icon plus a device
//! count — and the hover tooltip names the adapter state and each device.

use std::collections::BTreeMap;

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    IconSetting, LaunchSpec, Msg, ResolvedIcon, Tooltip, Widget, WidgetStyle, draw_icon_content,
    expand, measure_icon_content,
};

/// The default label: the state icon, followed by the device count when one
/// or more devices are connected.
const DEFAULT_FORMAT: &str = "{icon}{count}";

/// Placeholders accepted by the bluetooth `format` and `tooltip-format`.
pub const BLUETOOTH_PLACEHOLDERS: &[&str] = &["icon", "count", "state"];

/// Named `format-icons` entries the bluetooth widget understands.
pub const BLUETOOTH_ICON_NAMES: &[&str] = &["on", "off", "connected"];

/// Validate a bluetooth `format` or `tooltip-format`.
pub fn validate_bluetooth_format(format: &str) -> Result<(), String> {
    super::validate_placeholders(format, BLUETOOTH_PLACEHOLDERS)
}

/// The power state of the local Bluetooth adapter, normalized from BlueZ.
///
/// The many low-level BlueZ signals collapse into these three so the bar shows
/// a single unambiguous word: the user only needs to know whether Bluetooth is
/// on, off, or whether the system has no adapter at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothState {
    /// The adapter is powered on.
    On,
    /// The adapter is powered off.
    Off,
    /// No adapter is present (or BlueZ is unreachable).
    Unavailable,
}

impl BluetoothState {
    /// A short, human-readable label for the state.
    pub fn label(self) -> &'static str {
        match self {
            BluetoothState::On => "on",
            BluetoothState::Off => "off",
            BluetoothState::Unavailable => "unavailable",
        }
    }
}

/// A normalized snapshot of the local Bluetooth adapter: its power state plus,
/// when powered, the number of connected devices.
///
/// Normalization happens once, at the producer boundary, so the widget and the
/// redraw policy compare clean, canonical values: the device count is clamped
/// to `0..=u16::MAX` and is forced to zero on any state other than
/// [`On`](BluetoothState::On), so a stale count from before an `Off`
/// transition can never leak back into the label. Equality is therefore a
/// faithful "does this look different on screen?" test. The connected
/// device names ride along for the tooltip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bluetooth {
    state: BluetoothState,
    connected: u16,
    devices: Vec<String>,
}

impl Bluetooth {
    /// Build a normalized snapshot from a power `state` and a raw connected
    /// device `count`.
    ///
    /// The count is clamped to `0..=u16::MAX`; on any state other than
    /// [`On`](BluetoothState::On) it is forced to zero — the count is only
    /// meaningful while the adapter is powered. Pass the daemon's readings
    /// verbatim; the clamping here is the single place out-of-range values are
    /// tamed.
    pub fn new(state: BluetoothState, count: u32) -> Self {
        let connected = if state == BluetoothState::On {
            count.min(u32::from(u16::MAX)) as u16
        } else {
            0
        };
        Self {
            state,
            connected,
            devices: Vec::new(),
        }
    }

    /// Attach the connected devices' display names, kept only while the
    /// adapter is on.
    pub fn with_devices(mut self, devices: Vec<String>) -> Self {
        if self.state == BluetoothState::On {
            self.devices = devices;
        }
        self
    }

    /// The normalized power state.
    pub fn state(&self) -> BluetoothState {
        self.state
    }

    /// The number of connected devices, present only when the adapter is on.
    pub fn connected(&self) -> u16 {
        self.connected
    }

    /// The connected devices' display names, for the tooltip.
    pub fn devices(&self) -> &[String] {
        &self.devices
    }

    /// The display label, e.g. `"on"`, `"off"`, `"2 connected"`, or
    /// `"unavailable"`.
    ///
    /// A powered adapter with zero connected devices shows the bare state label
    /// (`"on"`); with one or more it shows `"N connected"`. The
    /// [`Off`](BluetoothState::Off) and
    /// [`Unavailable`](BluetoothState::Unavailable) states always show their
    /// bare label, since the device count is not meaningful in either. Keeping
    /// this a pure function makes the rendered text deterministic and
    /// unit-testable without painting pixels.
    pub fn label(&self) -> String {
        match (self.state, self.connected) {
            (BluetoothState::On, 0) => "on".to_string(),
            (BluetoothState::On, 1) => "1 connected".to_string(),
            (BluetoothState::On, n) => format!("{n} connected"),
            (BluetoothState::Off, _) => "off".to_string(),
            (BluetoothState::Unavailable, _) => "unavailable".to_string(),
        }
    }
}

/// A bar widget showing the Bluetooth adapter state and, when on, the number of
/// connected devices.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can report
/// a visible change only when the normalized snapshot actually differs. The
/// widget is always visible — its initial state is
/// [`Unavailable`](BluetoothState::Unavailable) and it stays so until the
/// producer hands it a reading — so a desktop without Bluetooth hardware
/// still shows the off icon rather than reserving nothing. Its resolved
/// [`WidgetStyle`] decides the glyph, the optional pill, and the colors it
/// draws with. An optional `on_click` path turns clicks into a
/// [`Command::RunProgram`](super::Command::RunProgram) so the user can wire a
/// Bluetooth manager launcher without writing Rust.
pub struct BluetoothWidget {
    bounds: Bounds,
    state: Bluetooth,
    style: WidgetStyle,
    on_click: Option<LaunchSpec>,
    format: String,
    /// Named text icons (`on`, `off`, `connected`) replacing the built-in art.
    format_icons: BTreeMap<String, String>,
    tooltip: bool,
    tooltip_format: Option<String>,
}

impl BluetoothWidget {
    /// Create a bluetooth widget occupying `bounds`, starting in
    /// [`Unavailable`](BluetoothState::Unavailable) (so a fresh bar with no
    /// producer yet shows nothing surprising) and carrying the default (flat,
    /// glyph-on) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: Bluetooth::new(BluetoothState::Unavailable, 0),
            style: WidgetStyle::default(),
            on_click: None,
            format: DEFAULT_FORMAT.to_string(),
            format_icons: BTreeMap::new(),
            tooltip: true,
            tooltip_format: None,
        }
    }

    /// Set the label format (`{icon}`, `{count}`, `{state}`).
    pub fn with_format(mut self, format: Option<String>) -> Self {
        if let Some(format) = format {
            self.format = format;
        }
        self
    }

    /// Set named text icons (`on`, `off`, `connected`).
    pub fn with_format_icons(mut self, icons: Option<BTreeMap<String, String>>) -> Self {
        if let Some(icons) = icons {
            self.format_icons = icons;
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

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](BluetoothWidget::new) at build time.
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
    /// [`new`](BluetoothWidget::new) at build time.
    pub fn with_on_click(mut self, path: Option<LaunchSpec>) -> Self {
        self.on_click = path;
        self
    }

    /// A plain-text summary of the current state, e.g. `"2 connected"`.
    pub fn label(&self) -> String {
        self.state.label()
    }

    /// The configured format with `{count}`/`{state}` filled in. The default
    /// format drops a zero count, so off and idle states show the icon alone.
    fn template(&self) -> String {
        self.expand(&self.format)
    }

    fn expand(&self, format: &str) -> String {
        let count = self.state.connected();
        let count = if format == DEFAULT_FORMAT && count == 0 {
            String::new()
        } else {
            count.to_string()
        };
        expand(
            format,
            &[("count", &count), ("state", self.state.state().label())],
        )
    }

    /// The state's `format-icons` key and built-in art.
    fn state_icon(&self) -> (&'static str, BuiltinIcon) {
        match (self.state.state(), self.state.connected()) {
            (BluetoothState::On, 0) => ("on", BuiltinIcon::Bluetooth),
            (BluetoothState::On, _) => ("connected", BuiltinIcon::BluetoothConnected),
            _ => ("off", BuiltinIcon::BluetoothOff),
        }
    }

    /// The off/on/connected icon: a custom/opted-out style icon, a named
    /// `format-icons` entry, or the built-in art.
    fn icon(&self) -> ResolvedIcon {
        let (name, builtin) = self.state_icon();
        if self.style.icon == IconSetting::Default
            && let Some(icon) = self.format_icons.get(name)
        {
            return ResolvedIcon::Text(icon.clone());
        }
        self.style.resolve_icon(builtin)
    }

    /// The adapter state followed by one line per connected device, or the
    /// custom `tooltip-format`.
    pub fn tooltip_text(&self) -> Option<String> {
        if !self.tooltip {
            return None;
        }
        if let Some(format) = &self.tooltip_format {
            let icon = match self.icon() {
                ResolvedIcon::Text(icon) => icon,
                _ => String::new(),
            };
            return Some(self.expand(format).replace("{icon}", &icon));
        }
        let mut text = format!("Bluetooth: {}", self.state.state().label());
        for device in self.state.devices() {
            text.push('\n');
            text.push_str(device);
        }
        Some(text)
    }

    fn contains(&self, px: u32, py: u32) -> bool {
        let b = self.bounds;
        px >= b.x && px < b.x + b.width && py >= b.y && py < b.y + b.height
    }
}

impl Widget for BluetoothWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Bluetooth(next) => {
                if self.state == *next {
                    return false;
                }
                // Device names only reach the tooltip; repaint only when the
                // pill itself changes.
                let before = (self.template(), self.icon());
                self.state = next.clone();
                (self.template(), self.icon()) != before
            }
            _ => false,
        }
    }

    #[hotpath::measure(impl_type = "BluetoothWidget")]
    fn draw(&self, ctx: &mut RenderContext) {
        // Always draws: the initial Unavailable state still has a non-empty
        // label, so the slot is reserved from the very first frame.
        draw_icon_content(
            ctx,
            &self.style,
            self.bounds,
            &self.icon(),
            &self.template(),
            self.style.base_colors(),
        );
    }

    #[hotpath::measure(impl_type = "BluetoothWidget")]
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
        // Single-action widget: only the primary button launches the manager.
        if button != super::ClickButton::Left {
            return None;
        }
        // Only interactive when an on-click path was configured at build time;
        // otherwise the default (None) is correct.
        let path = self.on_click.as_ref()?;
        if !self.contains(px, py) {
            return None;
        }
        Some(super::Command::RunProgram(path.clone()))
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
    use crate::widget::ClickButton;
    use chrono::{Local, TimeZone};

    fn bt(state: BluetoothState, count: u32) -> Msg {
        Msg::Bluetooth(Bluetooth::new(state, count))
    }

    #[test]
    fn state_labels_are_unambiguous_words() {
        assert_eq!(BluetoothState::On.label(), "on");
        assert_eq!(BluetoothState::Off.label(), "off");
        assert_eq!(BluetoothState::Unavailable.label(), "unavailable");
    }

    #[test]
    fn new_clamps_an_absurd_count_to_u16_max() {
        let bt = Bluetooth::new(BluetoothState::On, u32::from(u16::MAX) + 5);
        assert_eq!(bt.connected(), u16::MAX);
    }

    #[test]
    fn new_zeroes_the_count_when_the_adapter_is_off() {
        // A stale count from before the adapter was powered down is dropped.
        assert_eq!(Bluetooth::new(BluetoothState::Off, 4).connected(), 0);
        assert_eq!(
            Bluetooth::new(BluetoothState::Unavailable, 7).connected(),
            0
        );
    }

    #[test]
    fn label_shows_on_for_a_powered_adapter_with_zero_devices() {
        assert_eq!(Bluetooth::new(BluetoothState::On, 0).label(), "on");
    }

    #[test]
    fn label_shows_singular_for_one_device() {
        assert_eq!(Bluetooth::new(BluetoothState::On, 1).label(), "1 connected");
    }

    #[test]
    fn label_shows_plural_for_many_devices() {
        assert_eq!(Bluetooth::new(BluetoothState::On, 2).label(), "2 connected");
        assert_eq!(
            Bluetooth::new(BluetoothState::On, 99).label(),
            "99 connected"
        );
    }

    #[test]
    fn label_drops_the_count_when_off_or_unavailable() {
        assert_eq!(Bluetooth::new(BluetoothState::Off, 0).label(), "off");
        assert_eq!(
            Bluetooth::new(BluetoothState::Unavailable, 0).label(),
            "unavailable"
        );
    }

    #[test]
    fn first_reading_changes_state_and_sets_label() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.label(), "unavailable");
        assert!(widget.update(&bt(BluetoothState::On, 0)));
        assert_eq!(widget.label(), "on");
    }

    #[test]
    fn identical_reading_is_not_a_visible_change() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&bt(BluetoothState::On, 2)));
        // The same normalized snapshot: nothing to repaint.
        assert!(!widget.update(&bt(BluetoothState::On, 2)));
        assert_eq!(widget.label(), "2 connected");
    }

    #[test]
    fn a_new_count_is_a_visible_change() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&bt(BluetoothState::On, 1)));
        assert!(widget.update(&bt(BluetoothState::On, 2)));
        assert_eq!(widget.label(), "2 connected");
    }

    #[test]
    fn a_new_state_is_a_visible_change() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&bt(BluetoothState::On, 3)));
        // On → Off: count is dropped, label changes, repaint.
        assert!(widget.update(&bt(BluetoothState::Off, 0)));
        assert_eq!(widget.label(), "off");
    }

    #[test]
    fn unavailable_reading_before_any_state_is_not_a_change() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        // The default state is Unavailable, so a fresh Unavailable reading
        // matches the empty initial state: nothing to repaint.
        assert!(!widget.update(&bt(BluetoothState::Unavailable, 0)));
        assert_eq!(widget.label(), "unavailable");
    }

    #[test]
    fn unrelated_message_is_ignored() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&bt(BluetoothState::On, 2));
        let tick = Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, 8, 0, 0).unwrap());
        assert!(!widget.update(&tick));
        assert_eq!(widget.label(), "2 connected");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 1, 1));
        widget.set_bounds(Bounds::new(10, 0, 200, 32));
        assert_eq!(widget.bounds(), Bounds::new(10, 0, 200, 32));
    }

    fn bt_with(devices: &[&str]) -> Msg {
        Msg::Bluetooth(
            Bluetooth::new(BluetoothState::On, devices.len() as u32)
                .with_devices(devices.iter().map(|d| d.to_string()).collect()),
        )
    }

    #[test]
    fn device_names_are_kept_only_while_on() {
        let off = Bluetooth::new(BluetoothState::Off, 0).with_devices(vec!["Buds".into()]);
        assert!(off.devices().is_empty());
        let on = Bluetooth::new(BluetoothState::On, 1).with_devices(vec!["Buds".into()]);
        assert_eq!(on.devices(), ["Buds".to_string()]);
    }

    #[test]
    fn display_is_off_icon_on_icon_or_connected_icon_with_count() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        // No adapter: the off icon alone.
        assert_eq!(widget.template(), "{icon}");
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::BluetoothOff)
        );
        widget.update(&bt(BluetoothState::Off, 0));
        assert_eq!(widget.template(), "{icon}");
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::BluetoothOff)
        );
        widget.update(&bt(BluetoothState::On, 0));
        assert_eq!(widget.template(), "{icon}");
        assert_eq!(widget.icon(), ResolvedIcon::Builtin(BuiltinIcon::Bluetooth));
        widget.update(&bt_with(&["Buds", "Mouse"]));
        assert_eq!(widget.template(), "{icon}2");
        assert_eq!(
            widget.icon(),
            ResolvedIcon::Builtin(BuiltinIcon::BluetoothConnected)
        );
    }

    #[test]
    fn named_format_icons_override_each_state() {
        let icons = [("on", "B"), ("off", "X"), ("connected", "C")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut widget =
            BluetoothWidget::new(Bounds::new(0, 0, 320, 32)).with_format_icons(Some(icons));
        assert_eq!(widget.icon(), ResolvedIcon::Text("X".into()));
        widget.update(&bt(BluetoothState::On, 0));
        assert_eq!(widget.icon(), ResolvedIcon::Text("B".into()));
        widget.update(&bt_with(&["Buds"]));
        assert_eq!(widget.icon(), ResolvedIcon::Text("C".into()));
    }

    #[test]
    fn custom_format_expands_count_and_state() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32))
            .with_format(Some("{icon} {count} {state}".into()));
        widget.update(&bt_with(&["Buds"]));
        assert_eq!(widget.template(), "{icon} 1 on");
        widget.update(&bt(BluetoothState::On, 0));
        assert_eq!(widget.template(), "{icon} 0 on");
    }

    #[test]
    fn tooltip_lists_the_adapter_state_and_each_device() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(
            widget.tooltip_text().as_deref(),
            Some("Bluetooth: unavailable")
        );
        widget.update(&bt(BluetoothState::On, 0));
        assert_eq!(widget.tooltip_text().as_deref(), Some("Bluetooth: on"));
        widget.update(&bt_with(&["Buds", "Mouse"]));
        assert_eq!(
            widget.tooltip_at(10, 10).map(|tip| tip.text).as_deref(),
            Some("Bluetooth: on\nBuds\nMouse")
        );
        assert_eq!(widget.tooltip_at(400, 10), None);
    }

    #[test]
    fn a_device_name_change_updates_the_tooltip_without_a_redraw() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&bt_with(&["Buds"])));
        assert!(!widget.update(&bt_with(&["Headset"])));
        assert_eq!(
            widget.tooltip_text().as_deref(),
            Some("Bluetooth: on\nHeadset")
        );
    }

    #[test]
    fn tooltip_format_and_opt_out() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32))
            .with_tooltip_format(Some("{state}: {count}".into()));
        widget.update(&bt_with(&["Buds"]));
        assert_eq!(widget.tooltip_text().as_deref(), Some("on: 1"));
        let widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32)).with_tooltip(Some(false));
        assert_eq!(widget.tooltip_at(10, 10), None);
    }

    #[test]
    fn without_on_click_a_click_is_a_no_op() {
        let widget = BluetoothWidget::new(Bounds::new(0, 0, 200, 32));
        assert_eq!(widget.on_click(10, 10, ClickButton::Left), None);
    }

    #[test]
    fn with_on_click_a_click_inside_bounds_runs_the_program() {
        let widget = BluetoothWidget::new(Bounds::new(0, 0, 200, 32))
            .with_on_click(Some(LaunchSpec::program_only("/usr/bin/blueman-manager")));
        assert_eq!(
            widget.on_click(10, 10, ClickButton::Left),
            Some(super::super::Command::RunProgram(LaunchSpec::program_only(
                "/usr/bin/blueman-manager"
            )))
        );
    }

    #[test]
    fn on_click_outside_bounds_is_ignored_even_when_configured() {
        let widget = BluetoothWidget::new(Bounds::new(10, 0, 200, 32))
            .with_on_click(Some(LaunchSpec::program_only("/usr/bin/blueman-manager")));
        // x=0 is left of bounds.x=10; click is outside the pill.
        assert_eq!(widget.on_click(0, 10, ClickButton::Left), None);
        // x=210 is at/past the right edge (x + width = 210), outside.
        assert_eq!(widget.on_click(210, 10, ClickButton::Left), None);
        // y=32 is at the bottom edge (height = 32 → y range [0, 32)), outside.
        assert_eq!(widget.on_click(50, 32, ClickButton::Left), None);
    }
}
