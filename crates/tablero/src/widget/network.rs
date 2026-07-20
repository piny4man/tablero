//! The network widget and its normalized connectivity model.
//!
//! [`Network`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Network`]; [`NetworkWidget`] renders it,
//! repainting only when the visible connection state or SSID actually changes.
//!
//! Unavailable connectivity (or an unreachable network daemon) is carried as
//! `Msg::Network(None)`: the widget then shows nothing, exactly as it does before
//! its first reading, so a machine with no network stack never paints a stale or
//! placeholder value.

use crate::render::{Bounds, RenderContext};

use super::{Msg, Widget, WidgetStyle, draw_text_pill, glyph_label, measure_text_pill};

/// Default network glyphs (Font Awesome, via Nerd Font), chosen by connection
/// state: a wifi arc, a wired sitemap, a cross when disconnected, and a question
/// mark when the state is indeterminate.
const WIRELESS_GLYPH: &str = "\u{f1eb}"; // nf-fa-wifi
const WIRED_GLYPH: &str = "\u{f0e8}"; // nf-fa-sitemap
const DISCONNECTED_GLYPH: &str = "\u{f00d}"; // nf-fa-times
const UNKNOWN_GLYPH: &str = "\u{f128}"; // nf-fa-question

/// The kind of network connection in use, normalized from a raw daemon reading.
///
/// The many low-level NetworkManager states collapse into these four so the bar
/// shows a single unambiguous word: the user only needs to know whether they are
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
        Self { state, ssid }
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

    /// The display label, e.g. `"wifi home-net"`, `"wired"`, or `"disconnected"`.
    ///
    /// A wireless link with a known SSID shows `"wifi <ssid>"`; without one it
    /// shows the bare state label. Keeping this a pure function makes the
    /// rendered text deterministic and unit-testable without painting pixels.
    pub fn label(&self) -> String {
        match (self.state, self.ssid.as_deref()) {
            (NetworkState::Wireless, Some(ssid)) => format!("wifi {ssid}"),
            _ => self.state.label().to_string(),
        }
    }
}

/// The default glyph for a connection state: a wifi arc on a wireless link, a
/// sitemap on a wired one, a cross when disconnected, and a question mark when
/// the state is indeterminate.
fn default_glyph(state: NetworkState) -> &'static str {
    match state {
        NetworkState::Wireless => WIRELESS_GLYPH,
        NetworkState::Wired => WIRED_GLYPH,
        NetworkState::Disconnected => DISCONNECTED_GLYPH,
        NetworkState::Unknown => UNKNOWN_GLYPH,
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
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](NetworkWidget::new) at build time.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// The currently displayed label (empty before the first reading, or while
    /// the network is unavailable).
    pub fn label(&self) -> String {
        self.state.as_ref().map(Network::label).unwrap_or_default()
    }

    /// The full pill text: the state-derived glyph joined to the label, or empty
    /// while the network is unavailable (so the widget reserves no slot).
    fn display_text(&self) -> String {
        match &self.state {
            Some(network) => glyph_label(
                self.style.glyph(default_glyph(network.state())),
                &network.label(),
            ),
            None => String::new(),
        }
    }
}

impl Widget for NetworkWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Network(next) => {
                if &self.state == next {
                    return false;
                }
                self.state = next.clone();
                true
            }
            _ => false,
        }
    }

    fn draw(&self, ctx: &mut RenderContext) {
        // An unavailable network leaves `display_text` empty, so the pill paints
        // nothing: the dashboard has already cleared the background.
        draw_text_pill(
            ctx,
            &self.style,
            self.bounds,
            &self.display_text(),
            self.style.base_colors(),
        );
    }

    fn measure(&self, ctx: &mut RenderContext, _height: u32) -> u32 {
        measure_text_pill(ctx, &self.style, &self.display_text())
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Bounds) {
        self.bounds = bounds;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn label_shows_ssid_on_wifi_and_the_bare_state_otherwise() {
        assert_eq!(
            Network::new(NetworkState::Wireless, Some("home-net")).label(),
            "wifi home-net"
        );
        // Wireless with no usable SSID falls back to the bare "wifi" label.
        assert_eq!(Network::new(NetworkState::Wireless, None).label(), "wifi");
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
        assert_eq!(widget.label(), "wifi home-net");
    }

    #[test]
    fn identical_reading_is_not_a_visible_change() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&network(NetworkState::Wireless, Some("home-net"))));
        // The same normalized snapshot again (untrimmed name normalizes equal):
        // nothing to repaint.
        assert!(!widget.update(&network(NetworkState::Wireless, Some("  home-net  "))));
        assert_eq!(widget.label(), "wifi home-net");
    }

    #[test]
    fn a_new_ssid_is_a_visible_change() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&network(NetworkState::Wireless, Some("home-net"))));
        assert!(widget.update(&network(NetworkState::Wireless, Some("cafe-net"))));
        assert_eq!(widget.label(), "wifi cafe-net");
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
    fn display_text_uses_a_state_driven_glyph() {
        let mut widget = NetworkWidget::new(Bounds::new(0, 0, 320, 32));
        // Nothing to show before the first reading: no glyph, no slot.
        assert_eq!(widget.display_text(), "");
        // Each connection state picks its own glyph ahead of the label.
        widget.update(&network(NetworkState::Wireless, Some("home-net")));
        assert_eq!(
            widget.display_text(),
            format!("{WIRELESS_GLYPH} wifi home-net")
        );
        widget.update(&network(NetworkState::Wired, None));
        assert_eq!(widget.display_text(), format!("{WIRED_GLYPH} wired"));
        widget.update(&network(NetworkState::Disconnected, None));
        assert_eq!(
            widget.display_text(),
            format!("{DISCONNECTED_GLYPH} disconnected")
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
}
