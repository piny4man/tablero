//! The bluetooth widget and its normalized power-state model.
//!
//! [`Bluetooth`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Bluetooth`]; [`BluetoothWidget`] renders it,
//! repainting only when the visible state or connected-device count actually
//! changes.
//!
//! The widget is always visible — even when no adapter is present it shows
//! `unavailable`, matching what a user expects to see on a desktop without
//! Bluetooth hardware. This is a deliberate departure from the
//! battery/network pattern, where an absent source reserves no slot: here the
//! slot is always reserved so the user can see *why* nothing is connected
//! (powered off, or no adapter) and so a configured on-click still has a
//! target.

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use super::{
    LaunchSpec, Msg, ResolvedIcon, Widget, WidgetStyle, draw_icon_content, measure_icon_content,
};

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
/// faithful "does this look different on screen?" test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bluetooth {
    state: BluetoothState,
    connected: u16,
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
        Self { state, connected }
    }

    /// The normalized power state.
    pub fn state(self) -> BluetoothState {
        self.state
    }

    /// The number of connected devices, present only when the adapter is on.
    pub fn connected(self) -> u16 {
        self.connected
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
    pub fn label(self) -> String {
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
/// still shows `unavailable` rather than reserving nothing. Its resolved
/// [`WidgetStyle`] decides the glyph, the optional pill, and the colors it
/// draws with. An optional `on_click` path turns clicks into a
/// [`Command::RunProgram`](super::Command::RunProgram) so the user can wire a
/// Bluetooth manager launcher without writing Rust.
pub struct BluetoothWidget {
    bounds: Bounds,
    state: Bluetooth,
    style: WidgetStyle,
    on_click: Option<LaunchSpec>,
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
        }
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

    /// The currently displayed label.
    pub fn label(&self) -> String {
        self.state.label()
    }

    /// The format template — the icon slot marked by `{icon}` ahead of the label.
    /// Always non-empty, so the widget always reserves a slot.
    fn template(&self) -> String {
        format!("{{icon}} {}", self.state.label())
    }

    /// The bluetooth icon resolved against its
    /// [`Bluetooth`](BuiltinIcon::Bluetooth) default.
    fn icon(&self) -> ResolvedIcon {
        self.style.resolve_icon(BuiltinIcon::Bluetooth)
    }
}

impl Widget for BluetoothWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Bluetooth(next) => {
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

    #[test]
    fn template_marks_the_icon_slot_before_the_label() {
        let mut widget = BluetoothWidget::new(Bounds::new(0, 0, 320, 32));
        // The bluetooth icon is constant; only the label tracks state.
        assert_eq!(widget.icon(), ResolvedIcon::Builtin(BuiltinIcon::Bluetooth));
        // Initial Unavailable state already reserves the icon slot + label.
        assert_eq!(widget.template(), "{icon} unavailable");
        widget.update(&bt(BluetoothState::On, 2));
        assert_eq!(widget.template(), "{icon} 2 connected");
        widget.update(&bt(BluetoothState::Off, 0));
        assert_eq!(widget.template(), "{icon} off");
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
