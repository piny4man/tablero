//! The system-tray widget and its normalized StatusNotifierItem model.
//!
//! [`TrayState`] is the typed, normalized snapshot a producer feeds in through
//! [`Msg::Tray`](super::Msg::Tray): the set of tray items currently registered,
//! each already reduced to what the bar draws and how it responds to menu versus
//! activation clicks. [`TrayWidget`] renders them left to right and turns pointer
//! input into typed tray commands.
//!
//! Everything here is pure and defensive: icon bytes that do not describe a valid
//! image are dropped to "no icon" rather than trusted, a duplicate item key is
//! collapsed, and item order is canonicalized by key so the same set of items
//! always compares equal regardless of the order DBus signals arrived in. That
//! makes the redraw decision a faithful "does this look different on screen?" test
//! and keeps malformed tray data from ever reaching a panic.

use std::collections::HashSet;

use crate::render::{Bounds, RenderContext};

use super::{Command, Msg, Widget, WidgetStyle, draw_centered};

/// A tray item's status, normalized from the SNI `Status` property.
///
/// `Passive` is the catch-all: it covers an explicit `"Passive"` and any value
/// the bar does not recognize (or a missing one), so an item with a malformed
/// status is shown as ordinary rather than dropped or mislabeled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayStatus {
    /// The item is present but not requesting attention (the default).
    Passive,
    /// The item is active.
    Active,
    /// The item is requesting user attention.
    NeedsAttention,
}

/// How an SNI item's exported menu participates in pointer interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrayMenuMode {
    /// The item exports no usable menu. Right-click still requests SNI
    /// `ContextMenu` as a protocol fallback.
    #[default]
    None,
    /// Left-click activates the item; right-click opens its menu.
    Secondary,
    /// Both left- and right-click open the menu.
    Primary,
}

/// One normalized `com.canonical.dbusmenu` tree ready for presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayMenu {
    /// The owning tray item's watcher registration address.
    pub key: String,
    /// DBusMenu layout revision used to reject stale updates.
    pub revision: u32,
    /// Visible root-level entries.
    pub items: Vec<TrayMenuItem>,
}

/// One normalized DBusMenu entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayMenuItem {
    /// Protocol item id used for events.
    pub id: i32,
    /// Display label.
    pub label: String,
    /// Whether activation is allowed.
    pub enabled: bool,
    /// Whether the entry should be presented.
    pub visible: bool,
    /// Whether this entry is a visual separator rather than an action.
    pub separator: bool,
    /// Optional checkbox/radio state.
    pub toggle: Option<TrayMenuToggle>,
    /// Nested submenu entries.
    pub children: Vec<TrayMenuItem>,
}

/// A menu item's normalized toggle indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrayMenuToggle {
    /// Checkbox versus mutually-exclusive radio indicator.
    pub kind: TrayMenuToggleKind,
    /// Current protocol state.
    pub state: TrayMenuToggleState,
}

/// DBusMenu toggle presentation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMenuToggleKind {
    /// Independent checkmark.
    Checkmark,
    /// Radio-group selection.
    Radio,
}

/// DBusMenu toggle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMenuToggleState {
    /// Not selected.
    Off,
    /// Selected.
    On,
    /// Mixed or unspecified state.
    Indeterminate,
}

impl TrayStatus {
    /// Normalize the SNI `Status` string. Unknown or missing values read as
    /// [`Passive`](TrayStatus::Passive) rather than failing.
    pub fn from_sni(status: &str) -> TrayStatus {
        match status {
            "Active" => TrayStatus::Active,
            "NeedsAttention" => TrayStatus::NeedsAttention,
            _ => TrayStatus::Passive,
        }
    }
}

/// A decoded tray icon held as premultiplied RGBA8888, ready to blit.
///
/// Tray icons arrive in two shapes: a raw ARGB32 pixmap embedded in the SNI
/// `IconPixmap` property, or a named icon resolved to a PNG file in an icon
/// theme. Both are normalized here into the one premultiplied-RGBA layout
/// [`RenderContext::draw_icon`] consumes, so the widget never has to know which
/// source an icon came from. Construction is fallible and total over its inputs:
/// dimensions that do not match the byte length, or bytes that are not a decodable
/// PNG, yield `None`/`Err` instead of a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayIcon {
    width: u32,
    height: u32,
    /// Premultiplied RGBA8888, `width * height * 4` bytes.
    rgba: Vec<u8>,
}

impl TrayIcon {
    /// Build an icon from a raw SNI `IconPixmap` entry: `width * height` pixels of
    /// ARGB32 in network byte order (the wire layout `[A, R, G, B]` per pixel).
    ///
    /// The channels are reordered to RGBA and premultiplied by alpha. Returns
    /// `None` when the dimensions are zero or `argb` is shorter than
    /// `width * height * 4` bytes — a truncated or inconsistent pixmap is dropped,
    /// never trusted into an out-of-bounds read.
    pub fn from_argb32(width: u32, height: u32, argb: &[u8]) -> Option<TrayIcon> {
        let pixels = (width as usize).checked_mul(height as usize)?;
        let needed = pixels.checked_mul(4)?;
        if width == 0 || height == 0 || argb.len() < needed {
            return None;
        }
        let mut rgba = Vec::with_capacity(needed);
        for px in argb[..needed].chunks_exact(4) {
            // Wire order is ARGB (big-endian 0xAARRGGBB) → [A, R, G, B].
            let (a, r, g, b) = (px[0], px[1], px[2], px[3]);
            rgba.extend_from_slice(&premultiply(r, g, b, a));
        }
        Some(TrayIcon {
            width,
            height,
            rgba,
        })
    }

    /// Decode a PNG byte buffer (an icon-theme file) into a premultiplied icon.
    ///
    /// Any source color type is normalized to RGBA8 and premultiplied. A buffer
    /// that is not a valid PNG is an [`IconError`], so a corrupt or non-image file
    /// degrades to "no icon" rather than crashing the producer.
    pub fn from_png_bytes(bytes: &[u8]) -> Result<TrayIcon, IconError> {
        let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
            .map_err(|e| IconError(e.to_string()))?
            .into_rgba8();
        let (width, height) = image.dimensions();
        let mut rgba = Vec::with_capacity(image.as_raw().len());
        for px in image.as_raw().chunks_exact(4) {
            rgba.extend_from_slice(&premultiply(px[0], px[1], px[2], px[3]));
        }
        Ok(TrayIcon {
            width,
            height,
            rgba,
        })
    }

    /// Icon width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Icon height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Premultiplied RGBA8888 bytes, the layout [`RenderContext::draw_icon`] takes.
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
}

/// Multiply a straight-alpha `(r, g, b, a)` pixel into premultiplied RGBA bytes.
fn premultiply(r: u8, g: u8, b: u8, a: u8) -> [u8; 4] {
    let scale = |c: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
    [scale(r), scale(g), scale(b), a]
}

/// A tray icon could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconError(String);

impl std::fmt::Display for IconError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to decode tray icon: {}", self.0)
    }
}

impl std::error::Error for IconError {}

/// A single normalized tray item: what the bar needs to draw and act on it.
///
/// The `key` is the item's routing identity — the address a producer registered
/// it under — and doubles as the de-duplication key and tray-command payload. The `title`
/// is a human-readable fallback drawn when the item ships no usable icon. Both
/// strings are trimmed at construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayItem {
    key: String,
    title: String,
    status: TrayStatus,
    icon: Option<TrayIcon>,
    menu_mode: TrayMenuMode,
}

impl TrayItem {
    /// Build a normalized tray item from its routing `key`, display `title`,
    /// `status`, and optional decoded `icon`. The key and title are trimmed.
    pub fn new(
        key: impl Into<String>,
        title: impl Into<String>,
        status: TrayStatus,
        icon: Option<TrayIcon>,
    ) -> Self {
        Self {
            key: key.into().trim().to_string(),
            title: title.into().trim().to_string(),
            status,
            icon,
            menu_mode: TrayMenuMode::None,
        }
    }

    /// Set how this item responds when it exports a menu.
    pub fn with_menu(mut self, mode: TrayMenuMode) -> Self {
        self.menu_mode = mode;
        self
    }

    /// The item's normalized menu interaction behavior.
    pub fn menu_mode(&self) -> TrayMenuMode {
        self.menu_mode
    }

    /// The item's routing identity (the address it registered under).
    pub fn key(&self) -> &str {
        &self.key
    }

    /// The item's human-readable title (possibly empty).
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The item's normalized status.
    pub fn status(&self) -> TrayStatus {
        self.status
    }

    /// The item's decoded icon, if it supplied a usable one.
    pub fn icon(&self) -> Option<&TrayIcon> {
        self.icon.as_ref()
    }

    /// The short text drawn in place of a missing icon: the first character of
    /// the title, uppercased, or `"?"` when there is no title to draw from.
    ///
    /// Keeping a deterministic fallback means an item with no icon and no name
    /// still occupies a visible, clickable cell rather than vanishing.
    pub fn fallback_label(&self) -> String {
        self.title
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string())
    }
}

/// A normalized snapshot of the registered tray items.
///
/// Construction canonicalizes the set: items are de-duplicated by key (first
/// occurrence wins) and sorted by key, so the same items registered in any order
/// produce an equal `TrayState`. That order-independence is what lets the widget
/// treat equality as "nothing visibly changed" and stay idle when a lifecycle
/// signal re-reports the same set.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrayState {
    items: Vec<TrayItem>,
}

impl TrayState {
    /// Build a snapshot from an item set, de-duplicating by key and sorting by
    /// key for a canonical order.
    pub fn new(items: impl IntoIterator<Item = TrayItem>) -> Self {
        let mut items: Vec<TrayItem> = items.into_iter().collect();
        let mut seen = HashSet::new();
        items.retain(|item| seen.insert(item.key.clone()));
        items.sort_by(|a, b| a.key.cmp(&b.key));
        Self { items }
    }

    /// The normalized, key-sorted items.
    pub fn items(&self) -> &[TrayItem] {
        &self.items
    }

    /// Whether there are no tray items to show.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The number of tray items.
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// A bar widget showing the registered tray items as a row of icon cells.
///
/// Holds the last [`TrayState`] it was given so [`update`](Widget::update)
/// reports a visible change only when the normalized snapshot actually differs.
/// The state is an [`Option`]: `None` is the pre-first-message state and renders
/// as empty space, identical to an empty tray.
pub struct TrayWidget {
    bounds: Bounds,
    state: Option<TrayState>,
    style: WidgetStyle,
}

impl TrayWidget {
    /// Create a tray widget occupying `bounds`, empty until its first
    /// [`Msg::Tray`](super::Msg::Tray), with the default (flat) style.
    pub fn new(bounds: Bounds) -> Self {
        Self {
            bounds,
            state: None,
            style: WidgetStyle::default(),
        }
    }

    /// Set the resolved visual style, consuming and returning `self` so it
    /// chains off [`new`](TrayWidget::new) at build time. The
    /// [`attention`](super::WidgetStyle::attention) colors back the pill drawn
    /// behind an item that requests attention.
    pub fn with_style(mut self, style: WidgetStyle) -> Self {
        self.style = style;
        self
    }

    /// The number of items currently shown (zero before the first message).
    pub fn len(&self) -> usize {
        self.state.as_ref().map(TrayState::len).unwrap_or(0)
    }

    /// Whether the widget is currently showing no items.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The per-item cells: each `(item, bounds)` pair is one tray item's square
    /// slot, packed left to right from the widget origin.
    ///
    /// Each cell is as wide as the widget is tall, so icons are square and scale
    /// with the bar height (and the output's pixel density, since layout runs in
    /// physical pixels). Cells are clipped to the widget's slot; items that would
    /// start past the right edge are not placed. Both [`draw`](Widget::draw) and
    /// [`on_click`](Widget::on_click) read this, so what is painted and what is
    /// clickable are the same regions by construction.
    fn item_cells(&self) -> Vec<(&TrayItem, Bounds)> {
        let Some(state) = &self.state else {
            return Vec::new();
        };
        let side = self.bounds.height;
        if self.bounds.width == 0 || side == 0 {
            return Vec::new();
        }

        let right = self.bounds.x + self.bounds.width;
        let mut cells = Vec::new();
        for (i, item) in state.items().iter().enumerate() {
            let x = self.bounds.x + side * i as u32;
            if x >= right {
                break;
            }
            let width = side.min(right - x);
            cells.push((
                item,
                Bounds::new(x, self.bounds.y, width, self.bounds.height),
            ));
        }
        cells
    }
}

impl Widget for TrayWidget {
    fn update(&mut self, msg: &Msg) -> bool {
        match msg {
            Msg::Tray(next) => {
                // The never-populated widget renders nothing, exactly like an
                // empty snapshot, so an empty first update is not a change.
                let unchanged = match &self.state {
                    Some(current) => current == next,
                    None => next.is_empty(),
                };
                if unchanged {
                    return false;
                }
                self.state = Some(next.clone());
                true
            }
            _ => false,
        }
    }

    fn measure(&self, _ctx: &mut RenderContext, height: u32) -> u32 {
        // One square cell per item, each as wide as the row is tall, matching the
        // cells [`item_cells`](TrayWidget::item_cells) will draw.
        self.len() as u32 * height
    }

    fn draw(&self, ctx: &mut RenderContext) {
        let scale = ctx.scale_factor();
        let radius = (self.style.radius * scale) as f32;
        for (item, cell) in self.item_cells() {
            let needs_attention = item.status() == TrayStatus::NeedsAttention;
            // A pill sits behind the cell when the item needs attention (the
            // alert color) or when a base background is configured; the attention
            // color wins so a flagged item always stands out.
            let pill = if needs_attention {
                self.style.attention.background
            } else {
                self.style.background
            };
            if let Some(bg) = pill {
                ctx.fill_rounded_rect(cell, bg, radius);
            }
            // Keep icon artwork clear of the cell edge. The full cell remains the
            // background, outline, and hit target; padding only affects content.
            let requested_inset = self.style.padding * scale;
            let max_inset = cell.width.min(cell.height).saturating_sub(1) / 2;
            let inset = requested_inset.min(max_inset);
            let content = Bounds::new(
                cell.x + inset,
                cell.y + inset,
                cell.width - 2 * inset,
                cell.height - 2 * inset,
            );
            // A usable icon is blitted into the padded content area; an item
            // without one falls back to its centered initial.
            match item.icon() {
                Some(icon) => ctx.draw_icon(icon.rgba(), icon.width(), icon.height(), content),
                None => {
                    let color = if needs_attention {
                        self.style.attention.foreground
                    } else {
                        self.style.foreground
                    };
                    draw_centered(ctx, &item.fallback_label(), content, color);
                }
            }
            // Paint the outline last so an opaque full-cell tray icon cannot
            // cover the equipment-cell edge.
            if let Some(border) = self.style.border {
                ctx.stroke_rounded_rect(
                    cell,
                    border,
                    radius,
                    (self.style.border_width * scale) as f32,
                );
            }
        }
    }

    fn bounds(&self) -> Bounds {
        self.bounds
    }

    fn set_bounds(&mut self, bounds: Bounds) {
        self.bounds = bounds;
    }

    fn on_click(&self, px: u32, py: u32, button: super::ClickButton) -> Option<Command> {
        self.item_cells()
            .into_iter()
            .find(|(_, cell)| cell.contains(px, py))
            .map(|(item, _)| {
                let key = item.key().to_string();
                let (x, y) = (px as i32, py as i32);
                if button == super::ClickButton::Right || item.menu_mode() == TrayMenuMode::Primary
                {
                    Command::OpenTrayMenu { key, x, y }
                } else {
                    Command::ActivateTrayItem { key, x, y }
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widget::ClickButton;
    use std::io::Cursor;

    /// A solid-color RGBA PNG of the given size, for icon-decode tests.
    fn solid_png(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut img = image::RgbaImage::new(width, height);
        for px in img.pixels_mut() {
            *px = image::Rgba(rgba);
        }
        let mut bytes = Cursor::new(Vec::new());
        img.write_to(&mut bytes, image::ImageFormat::Png)
            .expect("encode png");
        bytes.into_inner()
    }

    fn item(key: &str, title: &str) -> TrayItem {
        TrayItem::new(key, title, TrayStatus::Active, None)
    }

    #[test]
    fn status_normalizes_known_values_and_defaults_the_rest() {
        assert_eq!(TrayStatus::from_sni("Active"), TrayStatus::Active);
        assert_eq!(
            TrayStatus::from_sni("NeedsAttention"),
            TrayStatus::NeedsAttention
        );
        assert_eq!(TrayStatus::from_sni("Passive"), TrayStatus::Passive);
        // Unknown or empty status reads as Passive, never panics or drops.
        assert_eq!(TrayStatus::from_sni("bogus"), TrayStatus::Passive);
        assert_eq!(TrayStatus::from_sni(""), TrayStatus::Passive);
    }

    #[test]
    fn argb32_pixmap_is_reordered_and_premultiplied() {
        // One pixel, half-opaque pure red, in ARGB wire order [A, R, G, B].
        let icon = TrayIcon::from_argb32(1, 1, &[128, 255, 0, 0]).expect("valid pixmap");
        assert_eq!((icon.width(), icon.height()), (1, 1));
        // Premultiplied: R = 255 * 128/255 ≈ 128, G/B = 0, A = 128.
        assert_eq!(icon.rgba(), &[128, 0, 0, 128]);
    }

    #[test]
    fn argb32_pixmap_with_inconsistent_length_is_dropped() {
        // Claims 2x2 (needs 16 bytes) but supplies only 4: dropped, not trusted.
        assert_eq!(TrayIcon::from_argb32(2, 2, &[0, 0, 0, 0]), None);
        // Zero dimensions are never a valid icon.
        assert_eq!(TrayIcon::from_argb32(0, 4, &[0, 0, 0, 0]), None);
    }

    #[test]
    fn png_icon_decodes_to_premultiplied_rgba() {
        let png = solid_png(3, 2, [0, 255, 0, 255]); // opaque green
        let icon = TrayIcon::from_png_bytes(&png).expect("valid png");
        assert_eq!((icon.width(), icon.height()), (3, 2));
        assert_eq!(icon.rgba().len(), 3 * 2 * 4);
        // Opaque green is unchanged by premultiplication.
        assert_eq!(&icon.rgba()[..4], &[0, 255, 0, 255]);
    }

    #[test]
    fn non_png_bytes_are_a_decode_error_not_a_panic() {
        assert!(TrayIcon::from_png_bytes(b"not a png at all").is_err());
    }

    #[test]
    fn item_trims_key_and_title() {
        let it = TrayItem::new("  :1.42/Item  ", "  Volume  ", TrayStatus::Active, None);
        assert_eq!(it.key(), ":1.42/Item");
        assert_eq!(it.title(), "Volume");
    }

    #[test]
    fn fallback_label_is_the_titles_initial_or_a_placeholder() {
        assert_eq!(item("k", "discord").fallback_label(), "D");
        // No title at all still yields a stable, visible placeholder.
        assert_eq!(item("k", "").fallback_label(), "?");
    }

    #[test]
    fn state_dedupes_by_key_and_sorts() {
        let state = TrayState::new([
            item(":1.3", "c"),
            item(":1.1", "a"),
            item(":1.1", "duplicate"),
            item(":1.2", "b"),
        ]);
        // Sorted by key, duplicate key collapsed (first occurrence kept).
        let keys: Vec<&str> = state.items().iter().map(TrayItem::key).collect();
        assert_eq!(keys, [":1.1", ":1.2", ":1.3"]);
        assert_eq!(state.items()[0].title(), "a");
        assert_eq!(state.len(), 3);
    }

    #[test]
    fn order_of_registration_does_not_change_the_snapshot() {
        // The same items in different arrival orders normalize equal — a
        // re-reported set is not a visible change.
        let a = TrayState::new([item(":1.1", "a"), item(":1.2", "b")]);
        let b = TrayState::new([item(":1.2", "b"), item(":1.1", "a")]);
        assert_eq!(a, b);
    }

    #[test]
    fn first_message_changes_state_and_unrelated_message_is_ignored() {
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.is_empty());
        assert!(widget.update(&Msg::Tray(TrayState::new([item(":1.1", "a")]))));
        assert_eq!(widget.len(), 1);

        // A tick is not a tray message: no change.
        let tick = Msg::tick_now();
        assert!(!widget.update(&tick));
        assert_eq!(widget.len(), 1);
    }

    #[test]
    fn an_empty_first_snapshot_is_not_a_visible_change() {
        // A fresh widget already renders nothing, so an empty tray snapshot
        // matches what is on screen and must not force a repaint.
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(!widget.update(&Msg::Tray(TrayState::default())));
        assert!(widget.is_empty());
        // But the first non-empty snapshot is a change.
        assert!(widget.update(&Msg::Tray(TrayState::new([item(":1.1", "a")]))));
    }

    #[test]
    fn identical_snapshot_is_not_a_visible_change() {
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&Msg::Tray(TrayState::new([
            item(":1.1", "a"),
            item(":1.2", "b")
        ]))));
        // Same set, reversed input order — normalizes equal, nothing to repaint.
        assert!(!widget.update(&Msg::Tray(TrayState::new([
            item(":1.2", "b"),
            item(":1.1", "a")
        ]))));
    }

    #[test]
    fn adding_an_item_is_a_visible_change() {
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 320, 32));
        assert!(widget.update(&Msg::Tray(TrayState::new([item(":1.1", "a")]))));
        assert!(widget.update(&Msg::Tray(TrayState::new([
            item(":1.1", "a"),
            item(":1.2", "b")
        ]))));
        assert_eq!(widget.len(), 2);
    }

    #[test]
    fn empty_tray_before_any_message_takes_no_clicks() {
        let widget = TrayWidget::new(Bounds::new(0, 0, 320, 32));
        assert_eq!(widget.on_click(0, 0, ClickButton::Left), None);
    }

    #[test]
    fn click_on_an_item_activates_it_by_key() {
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 320, 32));
        widget.update(&Msg::Tray(TrayState::new([
            item(":1.1", "a"),
            item(":1.2", "b"),
        ])));
        // Square cells of side 32 packed from the origin: :1.1 -> [0,32),
        // :1.2 -> [32,64).
        assert_eq!(
            widget.on_click(10, 16, ClickButton::Left),
            Some(Command::ActivateTrayItem {
                key: ":1.1".to_string(),
                x: 10,
                y: 16,
            })
        );
        assert_eq!(
            widget.on_click(40, 16, ClickButton::Left),
            Some(Command::ActivateTrayItem {
                key: ":1.2".to_string(),
                x: 40,
                y: 16,
            })
        );
        // Past the last cell is empty space.
        assert_eq!(widget.on_click(100, 16, ClickButton::Left), None);
    }

    #[test]
    fn primary_menu_item_opens_its_menu_on_left_click() {
        let menu_item = item(":1.1", "app").with_menu(TrayMenuMode::Primary);
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 32, 32));
        widget.update(&Msg::Tray(TrayState::new([menu_item])));

        assert_eq!(
            widget.on_click(12, 18, ClickButton::Left),
            Some(Command::OpenTrayMenu {
                key: ":1.1".to_string(),
                x: 12,
                y: 18,
            })
        );
    }

    #[test]
    fn secondary_menu_item_activates_on_left_and_opens_on_right() {
        let menu_item = item(":1.1", "app").with_menu(TrayMenuMode::Secondary);
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 32, 32));
        widget.update(&Msg::Tray(TrayState::new([menu_item])));

        assert!(matches!(
            widget.on_click(12, 18, ClickButton::Left),
            Some(Command::ActivateTrayItem { .. })
        ));
        assert_eq!(
            widget.on_click(12, 18, ClickButton::Right),
            Some(Command::OpenTrayMenu {
                key: ":1.1".to_string(),
                x: 12,
                y: 18,
            })
        );
    }

    #[test]
    fn right_click_requests_context_menu_fallback_even_without_exported_menu() {
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 32, 32));
        widget.update(&Msg::Tray(TrayState::new([item(":1.1", "app")])));

        assert_eq!(
            widget.on_click(12, 18, ClickButton::Right),
            Some(Command::OpenTrayMenu {
                key: ":1.1".to_string(),
                x: 12,
                y: 18,
            })
        );
    }

    #[test]
    fn items_are_clipped_to_the_widget_slot() {
        // A slot only wide enough for one square cell drops the overflow item.
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 32, 32));
        widget.update(&Msg::Tray(TrayState::new([
            item(":1.1", "a"),
            item(":1.2", "b"),
        ])));
        // The first cell fills the whole slot; the second item would start at
        // x=32 (past the 32px slot) and is never placed, so no click anywhere in
        // the bar can reach it.
        for x in [0, 16, 31] {
            assert_eq!(
                widget.on_click(x, 16, ClickButton::Left),
                Some(Command::ActivateTrayItem {
                    key: ":1.1".to_string(),
                    x: x as i32,
                    y: 16,
                })
            );
        }
    }

    #[test]
    fn draw_renders_icons_and_initial_fallbacks_without_panicking() {
        // One item with a decoded icon, one without: both must draw cleanly.
        let png = solid_png(8, 8, [0, 0, 255, 255]);
        let icon = TrayIcon::from_png_bytes(&png).unwrap();
        let with_icon = TrayItem::new(":1.1", "blue", TrayStatus::Active, Some(icon));
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 64, 32));
        widget.update(&Msg::Tray(TrayState::new([with_icon, item(":1.2", "x")])));

        let mut ctx = RenderContext::new(64, 32);
        ctx.fill_background();
        widget.draw(&mut ctx);
        assert_eq!(ctx.pixels().len(), 64 * 32 * 4);
        // The first cell holds the blue icon: a bluish pixel exists near its center.
        let px = ctx.pixels();
        let center = (16 * 64 + 16) * 4;
        assert!(px[center + 2] > 0x80, "icon cell not blue");
    }

    #[test]
    fn configured_padding_insets_icons_without_shrinking_the_cell() {
        let png = solid_png(8, 8, [0, 0, 255, 255]);
        let icon = TrayIcon::from_png_bytes(&png).unwrap();
        let tray_item = TrayItem::new(":1.1", "blue", TrayStatus::Active, Some(icon));
        let style = WidgetStyle {
            background: Some((0x20, 0x40, 0x20, 0xFF)),
            padding: 6,
            radius: 0,
            ..WidgetStyle::default()
        };
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 32, 32)).with_style(style);
        widget.update(&Msg::Tray(TrayState::new([tray_item])));

        let mut ctx = RenderContext::new(32, 32);
        ctx.fill_background();
        widget.draw(&mut ctx);

        let px = ctx.pixels();
        let padded_edge = (16 * 32 + 2) * 4;
        let center = (16 * 32 + 16) * 4;
        assert_eq!(&px[padded_edge..padded_edge + 4], &[0x20, 0x40, 0x20, 0xFF]);
        assert!(px[center + 2] > 0x80, "padded icon center not blue");
        assert_eq!(
            widget.on_click(2, 16, ClickButton::Left),
            Some(Command::ActivateTrayItem {
                key: ":1.1".to_string(),
                x: 2,
                y: 16,
            })
        );
    }

    #[test]
    fn an_attention_item_draws_an_alert_pill_behind_its_cell() {
        // An item requesting attention gets the default alert pill (a muted red)
        // painted behind it, so it stands out even with no icon.
        let attn = TrayItem::new(":1.1", "urgent", TrayStatus::NeedsAttention, None);
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 32, 32));
        widget.update(&Msg::Tray(TrayState::new([attn])));

        let mut ctx = RenderContext::new(32, 32);
        ctx.fill_background();
        widget.draw(&mut ctx);
        // A point on the cell's left-middle edge (clear of the centered initial)
        // reads distinctly red — redder than it is blue — unlike the dark bar.
        let px = ctx.pixels();
        let p = (16 * 32 + 3) * 4;
        assert!(
            px[p] > 0x80 && px[p] > px[p + 2],
            "attention cell not alert-filled"
        );
    }

    #[test]
    fn a_passive_item_with_the_default_style_draws_no_pill() {
        // The default style has no base background, so an ordinary item leaves
        // the bar showing through behind its centered initial.
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 32, 32));
        widget.update(&Msg::Tray(TrayState::new([item(":1.1", "app")])));

        let mut ctx = RenderContext::new(32, 32);
        ctx.fill_background();
        widget.draw(&mut ctx);
        // The cell's top-left corner holds no pill fill: it is bare bar.
        let px = ctx.pixels();
        assert_eq!(&px[0..4], &[0x18, 0x18, 0x18, 0xFF]);
    }

    #[test]
    fn set_bounds_repositions_the_widget() {
        let mut widget = TrayWidget::new(Bounds::new(0, 0, 1, 1));
        widget.set_bounds(Bounds::new(10, 0, 200, 32));
        assert_eq!(widget.bounds(), Bounds::new(10, 0, 200, 32));
    }
}
