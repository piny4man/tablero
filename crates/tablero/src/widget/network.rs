//! The network widget and its normalized connectivity model.
//!
//! [`Network`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Network`]; [`NetworkWidget`] renders it compactly,
//! repainting only when the visible connection state or SSID actually changes.
//!
//! Unavailable connectivity (or an unreachable network daemon) is carried as
//! `Msg::Network(None)`: the widget then shows nothing, exactly as it does before
//! its first reading, so a machine with no network stack never paints a stale or
//! placeholder value.

use crate::icon::BuiltinIcon;
use crate::render::{Bounds, RenderContext};

use std::collections::BTreeMap;

use super::{
    ClickButton, Command, IconSetting, LaunchSpec, Msg, ResolvedIcon, Tooltip, Widget, WidgetStyle,
    draw_icon_content, expand, measure_icon_content,
};

/// Placeholders accepted by the network `format` and `tooltip-format`.
pub const NETWORK_PLACEHOLDERS: &[&str] = &["icon", "ssid", "signal", "state"];

/// Named `format-icons` entries the network widget understands.
pub const NETWORK_ICON_NAMES: &[&str] = &["disconnected", "wired", "unknown"];

/// Validate a network `format` or `tooltip-format`.
pub fn validate_network_format(format: &str) -> Result<(), String> {
    super::validate_placeholders(format, NETWORK_PLACEHOLDERS)
}

/// The kind of network connection in use, normalized from a raw daemon reading.
///
/// The many low-level NetworkManager states collapse into these four so the bar
/// can choose one unambiguous glyph: the user only needs to know whether they are
/// off the network, on a wired link, on Wi-Fi, or in an indeterminate state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkState {
    /// No active connection.
    Disconnected,
    /// Connected over a wired (Ethernet) link.
    Wired,
    /// Connected over a wireless (Wi-Fi) link.
    Wireless,
    /// Connectivity could not be determined.
    Unknown,
}

impl NetworkState {
    /// A short, human-readable label for the state.
    pub fn label(self) -> &'static str {
        match self {
            NetworkState::Disconnected => "disconnected",
            NetworkState::Wired => "wired",
            NetworkState::Wireless => "wifi",
            NetworkState::Unknown => "unknown",
        }
    }
}

/// A normalized snapshot of connectivity: the connection state plus, for a
/// wireless link, the network name (SSID).
///
/// Normalization happens once, at the producer boundary, so the widget and the
/// redraw policy compare clean, canonical values. The SSID is meaningful only on
/// a wireless link, so it is retained solely when the state is
/// [`Wireless`](NetworkState::Wireless) and the name is non-empty after trimming;
/// in every other case it is dropped to `None`. A blank or whitespace-only SSID
/// is therefore never shown — the widget falls back to the bare state label
/// rather than painting an empty `"wifi "`. Equality is a faithful "does this
/// look different on screen?" test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    state: NetworkState,
    ssid: Option<String>,
    signal: Option<u8>,
}

impl Network {
    /// Build a normalized snapshot from a connection `state` and an optional raw
    /// `ssid`.
    ///
    /// The SSID is kept only on a wireless link and only when it has non-blank
    /// content (it is trimmed first); otherwise it is discarded. Pass the
    /// daemon's reading verbatim — the filtering here is the single place a
    /// missing or meaningless name is tamed.
    pub fn new(state: NetworkState, ssid: Option<&str>) -> Self {
        let ssid = if state == NetworkState::Wireless {
            ssid.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        } else {
            None
        };
        Self {
            state,
            ssid,
            signal: None,
        }
    }

    /// Attach the access point's signal strength in percent, clamped to
    /// `0..=100`. Kept only on a wireless link.
    pub fn with_signal(mut self, signal: Option<u8>) -> Self {
        self.signal = signal
            .filter(|_| self.state == NetworkState::Wireless)
            .map(|signal| signal.min(100));
        self
    }

    /// The Wi-Fi signal strength in percent, when known.
    pub fn signal(&self) -> Option<u8> {
        self.signal
    }

    /// The signal bucketed into the four-step icon ramp (`0..=3`), when known.
    pub fn level(&self) -> Option<u8> {
        self.signal.map(|signal| signal.min(99) / 25)
    }

    /// The normalized connection state.
    pub fn state(&self) -> NetworkState {
        self.state
    }

    /// The wireless network name, present only on a wireless link with a
    /// non-blank SSID.
    pub fn ssid(&self) -> Option<&str> {
        self.ssid.as_deref()
    }

    /// The compact display label, e.g. `"home-net"`, `"wired"`, or
    /// `"disconnected"`.
    ///
    /// The Wi-Fi icon already communicates the connection type, so a wireless
    /// link shows only its SSID. Without one, the icon stands alone. Keeping
    /// this a pure function makes the rendered text deterministic and
    /// unit-testable without painting pixels.
    pub fn label(&self) -> String {
        match (self.state, self.ssid.as_deref()) {
            (NetworkState::Wireless, Some(ssid)) => ssid.to_string(),
            (NetworkState::Wireless, None) => String::new(),
            _ => self.state.label().to_string(),
        }
    }
}

/// The default semantic icon for a connection state: a wired symbol on an
/// Ethernet link, otherwise the wireless symbol — there is no distinct
/// disconnected/unknown artwork, so those states reuse the generic network mark
/// and lean on the accompanying label to disambiguate.
fn default_icon(network: &Network) -> BuiltinIcon {
    match network.state() {
        NetworkState::Wired => BuiltinIcon::NetworkWired,
        _ => BuiltinIcon::NetworkWireless(network.level().unwrap_or(3)),
    }
}

/// A bar widget showing the network connection state and, on Wi-Fi, the SSID.
///
/// Holds the last snapshot it was given so [`update`](Widget::update) can report
/// a visible change only when the normalized snapshot actually differs. The
/// snapshot is an [`Option`]: `None` is "no network / unavailable", which renders
/// as empty space — identical to the pre-first-reading state, so unavailable
/// connectivity is shown the same whether it was never there or just went away.
/// Its resolved [`WidgetStyle`] decides the glyph, the optional pill, and the
/// colors it draws with.
pub struct NetworkWidget {
    bounds: Bounds,
    state: Option<Network>,
    style: WidgetStyle,
    on_click: Option<LaunchSpec>,
    on_click_right: Option<LaunchSpec>,
    /// A custom label format; `None` keeps the built-in `icon + label`.
    format: Option<String>,
    /// A low-to-high Wi-Fi text icon ramp.
    format_icons: Option<Vec<String>>,
    /// Text icons for the non-Wi-Fi states (`disconnected`, `wired`, `unknown`).
    state_icons: BTreeMap<String, String>,
    tooltip: bool,
    tooltip_format: Option<String>,
}

impl NetworkWidget {
    /// Create a network widget occupying `bounds`, empty until its first
    /// [`Msg::Network`] and carrying the default (flat,
    /// glyph-on) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
            on_click: None,
            on_click_right: None,
            format: None,
            format_icons: None,
            state_icons: BTreeMap::new(),
            tooltip: true,
            tooltip_format: None,
        }
    }

    /// Set the label format (`{icon}`, `{ssid}`, `{signal}`, `{state}`).
    pub fn with_format(mut self, format: Option<String>) -> Self {
        if format.is_some() {
            self.format = format;
        }
        self
    }

    /// Set a low-to-high Wi-Fi text icon ramp, indexed by signal strength.
    pub fn with_format_icons(mut self, icons: Option<Vec<String>>) -> Self {
        self.format_icons = icons.filter(|icons| !icons.is_empty());
        self
    }

    /// Set the text icon for a named non-Wi-Fi state.
    pub fn with_state_icon(mut self, name: &str, icon: Option<String>) -> Self {
        if let Some(icon) = icon {
            self.state_icons.insert(name.to_string(), icon);
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
    /// chains off [`new`](NetworkWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// Set the executable run on a primary click.
    pub fn with_on_click(mut self, path: Option<LaunchSpec>) -> Self {
        self.on_click = path;
        self
    }

    /// Set the executable run on a secondary click.
    pub fn with_on_click_right(mut self, path: Option<LaunchSpec>) -> Self {
        self.on_click_right = path;
        self
    }

    /// The currently displayed label (empty before the first reading, or while
    /// the network is unavailable).
    pub fn label(&self) -> String {
        self.state.as_ref().map(Network::label).unwrap_or_default()
    }

    /// The format template — the icon slot marked by `{icon}` ahead of the label
    /// — or empty while the network is unavailable (so the widget reserves no
    /// slot). A wireless link with no SSID collapses to a bare icon.
    fn template(&self) -> String {
        match (&self.state, &self.format) {
            (Some(network), Some(format)) => Self::expand(format, network),
            (Some(network), None) => format!("{{icon}} {}", network.label()),
            (None, _) => String::new(),
        }
    }

    fn expand(format: &str, network: &Network) -> String {
        let signal = network.signal().map(|s| s.to_string()).unwrap_or_default();
        expand(
            format,
            &[
                ("ssid", network.ssid().unwrap_or("")),
                ("signal", &signal),
                ("state", network.state().label()),
            ],
        )
    }

    /// The network's icon: a custom/opted-out style icon, a configured text
    /// icon for the state or Wi-Fi level, or the built-in art.
    fn icon(&self, network: &Network) -> ResolvedIcon {
        if self.style.icon == IconSetting::Default {
            let text = match network.state() {
                NetworkState::Wireless => self.format_icons.as_ref().map(|icons| {
                    let signal = usize::from(network.signal().unwrap_or(100));
                    icons[(signal * icons.len()) / 101].clone()
                }),
                NetworkState::Disconnected => self.state_icons.get("disconnected").cloned(),
                NetworkState::Wired => self.state_icons.get("wired").cloned(),
                NetworkState::Unknown => self.state_icons.get("unknown").cloned(),
            };
            if let Some(text) = text {
                return ResolvedIcon::Text(text);
            }
        }
        self.style.resolve_icon(default_icon(network))
    }

    /// Connection details kept off the bar's compact label.
    pub fn tooltip_text(&self) -> Option<String> {
        if !self.tooltip {
            return None;
        }
        let network = self.state.as_ref()?;
        if let Some(format) = &self.tooltip_format {
            let icon = match self.icon(network) {
                ResolvedIcon::Text(icon) => icon,
                _ => String::new(),
            };
            return Some(Self::expand(format, network).replace("{icon}", &icon));
        }
        Some(match network.state() {
            NetworkState::Wireless => {
                let mut text = "Connection: Wi-Fi".to_string();
                if let Some(ssid) = network.ssid() {
                    text.push_str(&format!("\nSSID: {ssid}"));
                }
                if let Some(signal) = network.signal() {
                    text.push_str(&format!("\nSignal: {signal}%"));
                }
                text
            }
            NetworkState::Wired => "Connection: Wired".to_string(),
            NetworkState::Disconnected => "Status: Disconnected".to_string(),
            NetworkState::Unknown => "Status: Unknown".to_string(),
        })
    }

    /// What the bar actually paints for the current snapshot, so a reading
    /// that only moves tooltip-only fields (e.g. signal inside one level) is
    /// not a visible change.
    fn visible(&self) -> Option<(String, ResolvedIcon)> {
        let network = self.state.as_ref()?;
        Some((self.template(), self.icon(network)))
    }

    fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.bounds.x
            && px < self.bounds.x + self.bounds.width
            && py >= self.bounds.y
            && py < self.bounds.y + self.bounds.height
    }
}

impl Widget for NetworkWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Network(next) => {
                if &self.state == next {
                    return false;
                }
                let before = self.visible();
                self.state = next.clone();
                self.visible() != before
            }
            _ => false,
        }
    }

    #[hotpath::measure(impl_type = "NetworkWidget")]
    fn draw(&self, ctx: &mut RenderContext) {
        // An unavailable network leaves the template empty, so the pill paints
        // nothing: the dashboard has already cleared the background.
        if let Some(network) = &self.state {
            draw_icon_content(
                ctx,
                &self.style,
                self.bounds,
                &self.icon(network),
                &self.template(),
                self.style.base_colors(),
            );
        }
    }

    #[hotpath::measure(impl_type = "NetworkWidget")]
    fn measure(&self, ctx: &mut RenderContext, _height: u32) -> u32 {
        match &self.state {
            Some(network) => {
                measure_icon_content(ctx, &self.style, &self.icon(network), &self.template())
            }
            None => 0,
        }
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Bounds) {
        self.bounds = bounds;
    }

    fn on_click(&self, px: u32, py: u32, button: ClickButton) -> Option<Command> {
        if !self.contains(px, py) {
            return None;
        }
        let path = match button {
            ClickButton::Left => self.on_click.as_ref(),
            ClickButton::Right => self.on_click_right.as_ref(),
        }?;
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
    use crate::widget::{ClickButton, Command};
    use chrono::{Local, TimeZone};

    fn network(state: NetworkState, ssid: Option<&str>) -> Msg {
        Msg::Network(Some(Network::new(state, ssid)))
    }

    #[test]
    fn state_labels_are_unambiguous_words() {
        assert_eq!(NetworkState::Disconnected.label(), "disconnected");
        assert_eq!(NetworkState::Wired.label(), "wired");
        assert_eq!(NetworkState::Wireless.label(), "wifi");
        assert_eq!(NetworkState::Unknown.label(), "unknown");
    }

    #[test]
    fn ssid_is_kept_only_on_a_wireless_link() {
        // A wired/disconnected/unknown link never carries an SSID, even if one is
        // offered by the daemon.
        assert_eq!(
            Network::new(NetworkState::Wired, Some("home-net")).ssid(),
            None
        );
        assert_eq!(
            Network::new(NetworkState::Disconnected, Some("home-net")).ssid(),
            None
        );
        assert_eq!(
            Network::new(NetworkState::Unknown, Some("home-net")).ssid(),
            None
        );
        assert_eq!(
            Network::new(NetworkState::Wireless, Some("home-net")).ssid(),
            Some("home-net")
        );
    }

    #[test]
    fn ssid_is_trimmed_and_blank_names_are_dropped() {
        assert_eq!(
            Network::new(NetworkState::Wireless, Some("  home-net  ")).ssid(),
            Some("home-net")
        );
        // Empty and whitespace-only names are meaningless: dropped to None.
        assert_eq!(Network::new(NetworkState::Wireless, Some("")).ssid(), None);
        assert_eq!(
            Network::new(NetworkState::Wireless, Some("   ")).ssid(),
            None
        );
        assert_eq!(Network::new(NetworkState::Wireless, None).ssid(), None);
    }

    #[test]
    fn label_shows_only_the_ssid_on_wifi_and_the_state_otherwise() {
        assert_eq!(
            Network::new(NetworkState::Wireless, Some("home-net")).label(),
            "home-net"
        );
        // With no usable SSID, the state glyph stands alone.
        assert_eq!(Network::new(NetworkState::Wireless, None).label(), "");
        assert_eq!(Network::new(NetworkState::Wired, None).label(), "wired");
        assert_eq!(
            Network::new(NetworkState::Disconnected, None).label(),
            "disconnected"
        );
        assert_eq!(Network::new(NetworkState::Unknown, None).label(), "unknown");
    }

    #[test]
    fn first_reading_changes_state_and_sets_label() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.label(), "");
        assert!(widget.update(&network(NetworkState::Wireless, Some("home-net"))));
        assert_eq!(widget.label(), "home-net");
    }

    #[test]
    fn identical_reading_is_not_a_visible_change() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&network(NetworkState::Wireless, Some("home-net"))));
        // The same normalized snapshot again (untrimmed name normalizes equal):
        // nothing to repaint.
        assert!(!widget.update(&network(NetworkState::Wireless, Some("  home-net  "))));
        assert_eq!(widget.label(), "home-net");
    }

    #[test]
    fn a_new_ssid_is_a_visible_change() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&network(NetworkState::Wireless, Some("home-net"))));
        assert!(widget.update(&network(NetworkState::Wireless, Some("cafe-net"))));
        assert_eq!(widget.label(), "cafe-net");
    }

    #[test]
    fn a_new_state_is_a_visible_change() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&network(NetworkState::Wireless, Some("home-net"))));
        assert!(widget.update(&network(NetworkState::Wired, None)));
        assert_eq!(widget.label(), "wired");
    }

    #[test]
    fn network_going_absent_is_a_visible_change_then_blank() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&network(NetworkState::Wireless, Some("home-net"))));
        // Daemon gone / network stack unavailable: the snapshot is now absent.
        assert!(widget.update(&Msg::Network(None)));
        assert_eq!(widget.label(), "");
    }

    #[test]
    fn absent_reading_before_any_network_is_not_a_change() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        // "Unavailable" matches the empty initial state, so nothing to repaint.
        assert!(!widget.update(&Msg::Network(None)));
        assert_eq!(widget.label(), "");
    }

    #[test]
    fn unrelated_message_is_ignored() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&network(NetworkState::Wired, None));
        let tick = Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, 8, 0, 0).unwrap());
        assert!(!widget.update(&tick));
        assert_eq!(widget.label(), "wired");
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 1, 1));
        widget.set_bounds(Bounds::new(10, 0, 200, 32));
        assert_eq!(widget.bounds(), Bounds::new(10, 0, 200, 32));
    }

    #[test]
    fn template_marks_the_icon_slot_and_icon_follows_the_state() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        // Nothing to show before the first reading: no slot.
        assert_eq!(widget.template(), "");
        // Each connection state resolves its own semantic icon ahead of the label.
        widget.update(&network(NetworkState::Wireless, Some("home-net")));
        assert_eq!(widget.template(), "{icon} home-net");
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Builtin(BuiltinIcon::NetworkWireless(3))
        );
        widget.update(&network(NetworkState::Wireless, None));
        // A wireless link with no usable SSID collapses to a bare icon slot.
        assert_eq!(widget.template(), "{icon} ");
        widget.update(&network(NetworkState::Wired, None));
        assert_eq!(widget.template(), "{icon} wired");
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Builtin(BuiltinIcon::NetworkWired)
        );
        widget.update(&network(NetworkState::Disconnected, None));
        // No disconnect artwork exists, so the generic network icon stands in.
        assert_eq!(widget.template(), "{icon} disconnected");
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Builtin(BuiltinIcon::NetworkWireless(3))
        );
    }

    #[test]
    fn an_absent_network_measures_zero_a_present_one_reserves_a_slot() {
        let mut ctx = RenderContext::new(320, 32);
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.measure(&mut ctx, 32), 0);
        widget.update(&network(NetworkState::Wired, None));
        assert!(widget.measure(&mut ctx, 32) > 0);
    }

    #[test]
    fn tooltip_keeps_connection_details_off_the_compact_label() {
        let mut widget = NetworkWidget::new(Bounds::new(10, 0, 100, 32));
        widget.update(&network(NetworkState::Wireless, Some("home-net")));

        assert_eq!(widget.label(), "home-net");
        assert_eq!(
            widget.tooltip_at(20, 16).map(|tooltip| tooltip.text),
            Some("Connection: Wi-Fi\nSSID: home-net".to_string())
        );
        assert_eq!(widget.tooltip_at(200, 16), None);
    }

    fn wifi(ssid: &str, signal: u8) -> Msg {
        Msg::Network(Some(
            Network::new(NetworkState::Wireless, Some(ssid)).with_signal(Some(signal)),
        ))
    }

    #[test]
    fn signal_is_kept_only_on_wifi_and_bucketed_into_four_levels() {
        assert_eq!(
            Network::new(NetworkState::Wired, None)
                .with_signal(Some(80))
                .signal(),
            None
        );
        let level = |signal| {
            Network::new(NetworkState::Wireless, None)
                .with_signal(Some(signal))
                .level()
        };
        assert_eq!(level(0), Some(0));
        assert_eq!(level(24), Some(0));
        assert_eq!(level(25), Some(1));
        assert_eq!(level(50), Some(2));
        assert_eq!(level(75), Some(3));
        assert_eq!(level(100), Some(3));
        // Out-of-range readings clamp to 100%.
        assert_eq!(
            Network::new(NetworkState::Wireless, None)
                .with_signal(Some(200))
                .signal(),
            Some(100)
        );
        assert_eq!(Network::new(NetworkState::Wireless, None).level(), None);
    }

    #[test]
    fn built_in_wifi_icon_follows_the_signal_level() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&wifi("home", 10));
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Builtin(BuiltinIcon::NetworkWireless(0))
        );
        widget.update(&wifi("home", 60));
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Builtin(BuiltinIcon::NetworkWireless(2))
        );
        // Unknown strength keeps the full icon.
        widget.update(&network(NetworkState::Wireless, Some("home")));
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Builtin(BuiltinIcon::NetworkWireless(3))
        );
    }

    #[test]
    fn a_signal_change_inside_one_level_updates_the_tooltip_without_a_redraw() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&wifi("home", 60)));
        assert!(!widget.update(&wifi("home", 62)));
        assert_eq!(
            widget.tooltip_text().as_deref(),
            Some("Connection: Wi-Fi\nSSID: home\nSignal: 62%")
        );
        // Crossing into the next level changes the icon.
        assert!(widget.update(&wifi("home", 80)));
    }

    #[test]
    fn format_expands_placeholders_and_a_visible_signal_redraws() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32))
            .with_format(Some("{icon}{signal}% {ssid} {state}".into()));
        widget.update(&wifi("home", 60));
        assert_eq!(widget.template(), "{icon}60% home wifi");
        assert!(widget.update(&wifi("home", 61)));

        let mut widget =
            NetworkWidget::new(Bounds::new(0, 0, 320, 32)).with_format(Some("{icon}".into()));
        widget.update(&wifi("home", 60));
        assert_eq!(widget.template(), "{icon}");
    }

    #[test]
    fn format_icons_ramp_and_named_state_icons() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32))
            .with_format_icons(Some(vec!["0".into(), "1".into(), "2".into(), "3".into()]))
            .with_state_icon("disconnected", Some("off".into()));
        widget.update(&wifi("home", 30));
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Text("1".into())
        );
        widget.update(&network(NetworkState::Wireless, Some("home")));
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Text("3".into())
        );
        widget.update(&network(NetworkState::Disconnected, None));
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Text("off".into())
        );
        // No named wired icon: the built-in wired art stays.
        widget.update(&network(NetworkState::Wired, None));
        assert_eq!(
            widget.icon(widget.state.as_ref().unwrap()),
            ResolvedIcon::Builtin(BuiltinIcon::NetworkWired)
        );
    }

    #[test]
    fn tooltip_format_and_opt_out() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32))
            .with_tooltip_format(Some("{ssid} {signal}%".into()));
        widget.update(&wifi("home", 60));
        assert_eq!(widget.tooltip_text().as_deref(), Some("home 60%"));
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32)).with_tooltip(Some(false));
        widget.update(&wifi("home", 60));
        assert_eq!(widget.tooltip_at(10, 10), None);
    }

    #[test]
    fn left_and_right_clicks_run_their_configured_programs() {
        let left = LaunchSpec::program_only("networkmanager_dmenu");
        let right = LaunchSpec::program_only("nm-connection-editor");
        let widget = NetworkWidget::new(Bounds::new(10, 0, 100, 32))
            .with_on_click(Some(left.clone()))
            .with_on_click_right(Some(right.clone()));
        assert_eq!(
            widget.on_click(20, 16, ClickButton::Left),
            Some(Command::RunProgram(left))
        );
        assert_eq!(
            widget.on_click(20, 16, ClickButton::Right),
            Some(Command::RunProgram(right))
        );
    }

    #[test]
    fn unconfigured_or_outside_clicks_are_ignored() {
        let widget = NetworkWidget::new(Bounds::new(10, 0, 100, 32));
        assert_eq!(widget.on_click(20, 16, ClickButton::Left), None);

        let widget = widget.with_on_click(Some(LaunchSpec::program_only("networkmanager_dmenu")));
        assert_eq!(widget.on_click(200, 16, ClickButton::Left), None);
    }
}
