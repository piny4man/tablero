//! StatusNotifierItem (SNI) system-tray source.
//!
//! Background applications expose tray icons over DBus using the
//! StatusNotifierItem spec: each item registers itself with a
//! *StatusNotifierWatcher*, and *hosts* (panels, bars) watch the watcher to learn
//! which items exist and read their properties. This module is the host: it
//! emits typed [`Msg::Tray`] snapshots through the [producer
//! bridge](crate::producer) and executes [`Command::ActivateTrayItem`] back over
//! DBus, so the rendering code never talks to the bus directly.
//!
//! Under a bare compositor such as Hyprland there is usually **no** watcher
//! running, so the producer hosts one itself: it serves the
//! `org.kde.StatusNotifierWatcher` interface and only falls back to an existing
//! watcher when another process already owns the name. Either way the host then
//! drives everything through a [`StatusNotifierWatcherProxy`], so the two paths
//! converge immediately after start-up.
//!
//! All the data-shaping logic — parsing a registration address, choosing a
//! pixmap, resolving an icon name to a themed PNG, folding raw item properties
//! into a [`TrayItem`] — lives in pure functions the unit tests drive directly.
//! The live DBus plumbing around them is kept deliberately thin.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures_util::stream::{StreamExt, select_all};
use log::warn;
use zbus::fdo::RequestNameReply;
use zbus::object_server::SignalEmitter;
use zbus::{Connection, interface, proxy};

use tablero_core::widget::{Command, Msg, TrayIcon, TrayItem, TrayState, TrayStatus};

use crate::command::CommandReceiver;
use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// The well-known bus name a StatusNotifierWatcher owns.
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
/// The object path the watcher interface is served at.
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// The default object path of an item registered by bus name alone.
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";

/// A single raw `IconPixmap` entry: `(width, height, ARGB32 bytes)`.
type RawPixmap = (i32, i32, Vec<u8>);

/// Split a watcher registration address into its `(bus_name, object_path)`.
///
/// An address is either a bare bus name (`":1.42"` or a well-known name), which
/// implies the [default item path](DEFAULT_ITEM_PATH), or a bus name immediately
/// followed by an object path (`":1.42/org/ayatana/NotificationItem/foo"`). The
/// split is at the first `/`. Returns `None` for an empty address or a bare path
/// with no bus name (which cannot be addressed) so a malformed registration is
/// skipped rather than trusted.
pub fn parse_item_address(address: &str) -> Option<(String, String)> {
    let address = address.trim();
    if address.is_empty() {
        return None;
    }
    match address.find('/') {
        // A leading '/' means no bus name precedes the path: unaddressable.
        Some(0) => None,
        Some(slash) => {
            let (name, path) = address.split_at(slash);
            Some((name.to_string(), path.to_string()))
        }
        None => Some((address.to_string(), DEFAULT_ITEM_PATH.to_string())),
    }
}

/// Choose the best [`IconPixmap`] entry and decode it into a [`TrayIcon`].
///
/// Items often ship the same icon at several sizes; the largest by pixel area is
/// picked so the bar scales down rather than up. Zero-sized or byte-inconsistent
/// entries are ignored, and an item with no usable pixmap yields `None` so the
/// caller can fall back to icon-name resolution.
pub fn select_pixmap(pixmaps: &[RawPixmap]) -> Option<TrayIcon> {
    pixmaps
        .iter()
        .filter(|(w, h, _)| *w > 0 && *h > 0)
        .max_by_key(|(w, h, _)| i64::from(*w) * i64::from(*h))
        .and_then(|(w, h, bytes)| TrayIcon::from_argb32(*w as u32, *h as u32, bytes))
}

/// Locate the PNG file backing an icon `name` within `dirs`.
///
/// An absolute path that points at a file is used directly (some items set
/// `IconName` to a full path). Otherwise each directory is probed for
/// `<name>.png`, and the first hit wins. Returns `None` when nothing matches, so
/// a name the theme cannot satisfy degrades to no icon rather than an error. The
/// directory list is supplied by the caller, which keeps the lookup a pure,
/// testable function of the filesystem.
pub fn find_icon_file(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let direct = Path::new(name);
    if direct.is_absolute() && direct.is_file() {
        return Some(direct.to_path_buf());
    }
    let file_name = format!("{name}.png");
    dirs.iter()
        .map(|dir| dir.join(&file_name))
        .find(|candidate| candidate.is_file())
}

/// Fold raw StatusNotifierItem properties into a normalized [`TrayItem`].
///
/// The `key` is the item's watcher address — its stable identity and the payload
/// an [`ActivateTrayItem`](Command::ActivateTrayItem) carries back. The display
/// title falls back to the item `id` when `title` is blank, so an item without a
/// human title still shows a meaningful initial. `icon` is the already-resolved
/// icon (pixmap or themed PNG), passed in so this stays pure and unit-testable;
/// `None` renders as the title's initial. Pure over its inputs.
pub fn tray_item_from_props(
    key: &str,
    id: &str,
    title: &str,
    status: &str,
    icon: Option<TrayIcon>,
) -> TrayItem {
    let display = if title.trim().is_empty() { id } else { title };
    TrayItem::new(key, display, TrayStatus::from_sni(status), icon)
}

/// Resolve an item's icon: prefer an embedded pixmap, else a themed PNG.
///
/// Pixmaps are self-contained and need no theme lookup, so they win when present;
/// otherwise the icon name is resolved against the item's `IconThemePath` and the
/// standard system icon directories. Any failure along the themed path (no file,
/// unreadable, undecodable) yields `None` — the item simply shows its initial.
fn resolve_icon(icon_name: &str, pixmaps: &[RawPixmap], theme_path: &str) -> Option<TrayIcon> {
    if let Some(icon) = select_pixmap(pixmaps) {
        return Some(icon);
    }
    let dirs = icon_search_dirs(theme_path);
    let file = find_icon_file(icon_name, &dirs)?;
    let bytes = std::fs::read(file).ok()?;
    TrayIcon::from_png_bytes(&bytes).ok()
}

/// The directories to search for a themed icon PNG, most specific first.
///
/// The item's own `IconThemePath` (when set) leads, followed by a pragmatic set
/// of common system icon roots. Full XDG icon-theme resolution (index.theme,
/// inheritance, size folders) is intentionally not implemented; the common
/// `hicolor` application sizes and `pixmaps` cover the apps a bar typically sees.
fn icon_search_dirs(theme_path: &str) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if !theme_path.trim().is_empty() {
        dirs.push(PathBuf::from(theme_path));
    }
    const SIZES: [&str; 8] = [
        "256x256", "128x128", "64x64", "48x48", "32x32", "24x24", "22x22", "16x16",
    ];
    for root in ["/usr/share/icons/hicolor", "/usr/local/share/icons/hicolor"] {
        for size in SIZES {
            dirs.push(Path::new(root).join(size).join("apps"));
            dirs.push(Path::new(root).join(size).join("status"));
        }
    }
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs
}

/// The host's view of a single registered item, read fresh from its proxy.
///
/// Reading degrades gracefully: a property the item does not expose falls back to
/// its empty default rather than failing the whole snapshot, so a partially
/// broken item still appears (with whatever it did provide) instead of crashing
/// the bar or vanishing the rest of the tray.
async fn read_item(conn: &Connection, key: &str) -> Option<TrayItem> {
    let (name, path) = parse_item_address(key)?;
    let item = StatusNotifierItemProxy::builder(conn)
        .destination(name)
        .ok()?
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;

    let id = item.id().await.unwrap_or_default();
    let title = item.title().await.unwrap_or_default();
    let status = item.status().await.unwrap_or_default();
    let icon_name = item.icon_name().await.unwrap_or_default();
    let theme_path = item.icon_theme_path().await.unwrap_or_default();
    let pixmaps = item.icon_pixmap().await.unwrap_or_default();

    let icon = resolve_icon(&icon_name, &pixmaps, &theme_path);
    Some(tray_item_from_props(key, &id, &title, &status, icon))
}

/// Build the current tray snapshot by reading every registered item.
///
/// Items that cannot be addressed at all are dropped; everything else is folded
/// into a normalized [`TrayState`] (which de-duplicates and sorts), so a
/// re-enumeration that finds the same set produces an equal snapshot and no
/// repaint.
async fn read_state(conn: &Connection, addresses: &[String]) -> TrayState {
    let mut items = Vec::new();
    for address in addresses {
        if let Some(item) = read_item(conn, address).await {
            items.push(item);
        }
    }
    TrayState::new(items)
}

/// A minimal in-process StatusNotifierWatcher.
///
/// Served only when no other watcher owns the name. It tracks the set of
/// registered item addresses, hands them out via the
/// `RegisteredStatusNotifierItems` property, and emits the registration signals
/// the host subscribes to. The host reads this back through a proxy exactly as it
/// would an external watcher, so hosting our own is transparent to the rest of
/// the producer.
struct Watcher {
    items: Arc<Mutex<HashSet<String>>>,
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    /// Register a tray item (the SNI item → watcher call). `service` is either a
    /// bare bus name or a `name + path`; a bare path is resolved against the
    /// caller's unique name from the message header.
    async fn register_status_notifier_item(
        &self,
        service: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) {
        // A bare object path carries no bus name; the real address is the
        // caller's unique name plus that path.
        let address = if service.starts_with('/') {
            match header.sender() {
                Some(sender) => format!("{sender}{service}"),
                None => return,
            }
        } else {
            service.to_string()
        };

        let inserted = self
            .items
            .lock()
            .map(|mut items| items.insert(address.clone()))
            .unwrap_or(false);
        if inserted {
            let _ = Self::status_notifier_item_registered(&emitter, &address).await;
        }
    }

    /// Register a host (the bar). The set of hosts is not tracked beyond
    /// answering `IsStatusNotifierHostRegistered`, which is always true here.
    async fn register_status_notifier_host(&self, _service: &str) {}

    /// The addresses of every currently registered item.
    #[zbus(property)]
    async fn registered_status_notifier_items(&self) -> Vec<String> {
        self.items
            .lock()
            .map(|items| items.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Whether a host is registered. Always true: this watcher only runs inside
    /// our host.
    #[zbus(property)]
    async fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    /// The implemented protocol version.
    #[zbus(property)]
    async fn protocol_version(&self) -> i32 {
        0
    }

    /// Emitted when an item registers.
    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    /// Emitted when an item unregisters.
    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;
}

/// The subset of `org.kde.StatusNotifierWatcher` the host consumes. zbus
/// generates `StatusNotifierWatcherProxy` from this.
#[proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    /// Register the host (the bar) with the watcher.
    fn register_status_notifier_host(&self, service: &str) -> zbus::Result<()>;

    /// The addresses of every currently registered item.
    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> zbus::Result<Vec<String>>;

    /// An item registered.
    #[zbus(signal)]
    fn status_notifier_item_registered(&self, service: String) -> zbus::Result<()>;

    /// An item unregistered.
    #[zbus(signal)]
    fn status_notifier_item_unregistered(&self, service: String) -> zbus::Result<()>;
}

/// The subset of `org.kde.StatusNotifierItem` the host reads and acts on. The
/// destination and path are dynamic, so this proxy is built per item.
#[proxy(
    interface = "org.kde.StatusNotifierItem",
    default_path = "/StatusNotifierItem"
)]
trait StatusNotifierItem {
    /// A stable, application-chosen identifier (used as a title fallback).
    #[zbus(property)]
    fn id(&self) -> zbus::Result<String>;

    /// A human-readable title.
    #[zbus(property)]
    fn title(&self) -> zbus::Result<String>;

    /// The item status (`"Passive"`, `"Active"`, `"NeedsAttention"`).
    #[zbus(property)]
    fn status(&self) -> zbus::Result<String>;

    /// The themed icon name, if any.
    #[zbus(property)]
    fn icon_name(&self) -> zbus::Result<String>;

    /// An item-supplied directory to resolve [`icon_name`](Self::icon_name) in.
    #[zbus(property)]
    fn icon_theme_path(&self) -> zbus::Result<String>;

    /// Embedded ARGB32 icon data at one or more sizes.
    #[zbus(property)]
    fn icon_pixmap(&self) -> zbus::Result<Vec<RawPixmap>>;

    /// Primary activation (a left click), at screen coordinates `(x, y)`.
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;

    /// The icon changed.
    #[zbus(signal)]
    fn new_icon(&self) -> zbus::Result<()>;

    /// The title changed.
    #[zbus(signal)]
    fn new_title(&self) -> zbus::Result<()>;

    /// The status changed.
    #[zbus(signal)]
    fn new_status(&self, status: String) -> zbus::Result<()>;

    /// The attention icon changed.
    #[zbus(signal)]
    fn new_attention_icon(&self) -> zbus::Result<()>;
}

/// A [`Producer`] that streams system-tray changes into the render loop.
///
/// Construct with [`new`](SniHostProducer::new) and hand it to the producer
/// bridge; it hosts (or finds) a StatusNotifierWatcher, registers as a host,
/// emits the initial tray, then re-reads and emits on every lifecycle or item
/// change until the session bus closes or the render loop shuts down.
pub struct SniHostProducer;

impl SniHostProducer {
    /// Create a system-tray producer.
    pub fn new() -> Self {
        Self
    }
}

impl Default for SniHostProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for SniHostProducer {
    fn name(&self) -> String {
        "sni-host".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx))
    }
}

/// Ensure a StatusNotifierWatcher exists on `conn`, hosting one if necessary.
///
/// The watcher object is served first, then the name is requested without
/// queueing. Becoming the primary owner means we are the watcher; otherwise an
/// external watcher already owns the name and we defer to it (our served object
/// is harmless, simply never addressed). Either way the host then talks to the
/// watcher purely through its proxy.
async fn ensure_watcher(conn: &Connection) -> zbus::Result<()> {
    let watcher = Watcher {
        items: Arc::new(Mutex::new(HashSet::new())),
    };
    conn.object_server().at(WATCHER_PATH, watcher).await?;
    let reply = conn
        .request_name_with_flags(WATCHER_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        .await;
    interpret_watcher_name(reply)
}

/// Decide whether requesting the watcher name leaves us in a usable state.
///
/// Owning the name (primary or already-owner) means *we* are the watcher. Any
/// other outcome means an external watcher already holds the name, and we defer
/// to it — including the `DoNotQueue` "name exists" case, which zbus surfaces as
/// [`NameTaken`](zbus::Error::NameTaken) rather than an `Ok` reply. Only a
/// genuine bus error (lost connection, malformed reply) is propagated, since the
/// producer cannot proceed without the bus.
fn interpret_watcher_name(reply: zbus::Result<RequestNameReply>) -> zbus::Result<()> {
    match reply {
        // We own it, or someone else already does — both are fine: we host or
        // defer, then drive everything through the watcher proxy regardless.
        Ok(_) | Err(zbus::Error::NameTaken) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Drive the tray stream: ensure a watcher, register as host, then emit a fresh
/// snapshot on every lifecycle or per-item change.
///
/// Re-enumeration is deliberately whole-snapshot: any change rebuilds the full
/// [`TrayState`], which is cheap relative to the change rate and keeps dynamic
/// item sets correct without bespoke per-item bookkeeping. Returns `Ok(())` once
/// the render loop drops its receiver or the watcher's signal streams end. A
/// failed session-bus connection propagates as an error the bridge logs and
/// isolates — the bar keeps running, the tray simply stays empty.
async fn run(tx: MsgSender) -> ProducerResult {
    let conn = Connection::session().await?;
    ensure_watcher(&conn).await?;

    let watcher = StatusNotifierWatcherProxy::new(&conn).await?;
    let host_name = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    // Best-effort: a watcher that rejects host registration still lets us read
    // items, so a failure here is logged, not fatal.
    if let Err(e) = watcher.register_status_notifier_host(&host_name).await {
        warn!("sni: registering host failed: {e}");
    }

    loop {
        let addresses = watcher
            .registered_status_notifier_items()
            .await
            .unwrap_or_default();
        if tx
            .send(Msg::Tray(read_state(&conn, &addresses).await))
            .is_err()
        {
            return Ok(());
        }

        // Wait for any change — an item appearing/disappearing, or any item
        // mutating an icon/title/status — then loop to re-read the whole tray.
        // Rebuilding the stream set each iteration is what keeps a dynamic item
        // set correct as items come and go.
        let mut streams = vec![
            watcher
                .receive_status_notifier_item_registered()
                .await?
                .map(|_| ())
                .boxed(),
            watcher
                .receive_status_notifier_item_unregistered()
                .await?
                .map(|_| ())
                .boxed(),
        ];
        for address in &addresses {
            if let Some(item) = item_change_stream(&conn, address).await {
                streams.push(item);
            }
        }

        let mut changes = select_all(streams);
        if changes.next().await.is_none() {
            return Ok(());
        }
    }
}

/// A merged "this item changed" stream for one item: icon, title, status, or
/// attention-icon updates collapsed to `()` ticks.
///
/// Returns `None` if the item proxy cannot be built (it is gone or unreadable),
/// in which case its absence simply means no per-item wakeups — the next
/// lifecycle signal will re-enumerate anyway.
async fn item_change_stream(
    conn: &Connection,
    address: &str,
) -> Option<futures_util::stream::BoxStream<'static, ()>> {
    let (name, path) = parse_item_address(address)?;
    let item = StatusNotifierItemProxy::builder(conn)
        .destination(name)
        .ok()?
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;
    let merged = select_all([
        item.receive_new_icon().await.ok()?.map(|_| ()).boxed(),
        item.receive_new_title().await.ok()?.map(|_| ()).boxed(),
        item.receive_new_status().await.ok()?.map(|_| ()).boxed(),
        item.receive_new_attention_icon()
            .await
            .ok()?
            .map(|_| ())
            .boxed(),
    ]);
    Some(merged.boxed())
}

/// Drain `commands` from the render loop and execute each tray activation.
///
/// Runs on the producer bridge as the executor end of the
/// [command channel](crate::command), alongside the Hyprland executor. Commands
/// this source does not handle (anything but
/// [`ActivateTrayItem`](Command::ActivateTrayItem)) are ignored, mirroring the
/// Hyprland executor. A failed activation is logged and skipped — one bad item
/// never ends the stream. Returns `Ok(())` when the render loop drops its sender.
pub async fn run_commands(mut commands: CommandReceiver) -> ProducerResult {
    let conn = Connection::session().await?;
    while let Some(command) = commands.recv().await {
        let Command::ActivateTrayItem(key) = &command else {
            continue;
        };
        if let Err(e) = activate(&conn, key).await {
            warn!("sni: activating {key:?} failed: {e}");
        }
    }
    Ok(())
}

/// Send a primary `Activate(0, 0)` to the item registered under `key`.
async fn activate(conn: &Connection, key: &str) -> zbus::Result<()> {
    let (name, path) = parse_item_address(key)
        .ok_or_else(|| zbus::Error::Failure(format!("bad address {key}")))?;
    let item = StatusNotifierItemProxy::builder(conn)
        .destination(name)?
        .path(path)?
        .build()
        .await?;
    item.activate(0, 0).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owning_the_watcher_name_is_usable() {
        assert!(interpret_watcher_name(Ok(RequestNameReply::PrimaryOwner)).is_ok());
        assert!(interpret_watcher_name(Ok(RequestNameReply::AlreadyOwner)).is_ok());
    }

    #[test]
    fn an_existing_external_watcher_is_deferred_to_not_fatal() {
        // zbus reports the DoNotQueue "name exists" outcome as NameTaken; the
        // producer must treat it as "defer to the external watcher", not crash.
        assert!(interpret_watcher_name(Err(zbus::Error::NameTaken)).is_ok());
        assert!(interpret_watcher_name(Ok(RequestNameReply::Exists)).is_ok());
    }

    #[test]
    fn a_genuine_bus_error_propagates() {
        assert!(interpret_watcher_name(Err(zbus::Error::Unsupported)).is_err());
    }

    #[test]
    fn bare_bus_name_uses_the_default_item_path() {
        assert_eq!(
            parse_item_address(":1.42"),
            Some((":1.42".to_string(), "/StatusNotifierItem".to_string()))
        );
    }

    #[test]
    fn well_known_name_uses_the_default_item_path() {
        assert_eq!(
            parse_item_address("org.kde.StatusNotifierItem-9-1"),
            Some((
                "org.kde.StatusNotifierItem-9-1".to_string(),
                "/StatusNotifierItem".to_string()
            ))
        );
    }

    #[test]
    fn name_and_path_split_at_the_first_slash() {
        assert_eq!(
            parse_item_address(":1.42/org/ayatana/NotificationItem/foo"),
            Some((
                ":1.42".to_string(),
                "/org/ayatana/NotificationItem/foo".to_string()
            ))
        );
    }

    #[test]
    fn empty_or_bare_path_addresses_are_rejected() {
        assert_eq!(parse_item_address(""), None);
        assert_eq!(parse_item_address("   "), None);
        // A leading slash means no bus name precedes the path: unaddressable.
        assert_eq!(parse_item_address("/StatusNotifierItem"), None);
    }

    #[test]
    fn select_pixmap_picks_the_largest_and_decodes_it() {
        // One opaque-red pixel at 1x1 and a 2x1 — the larger wins.
        let small = (1, 1, vec![255, 255, 0, 0]);
        let large = (2, 1, vec![255, 0, 255, 0, 255, 0, 0, 255]);
        let icon = select_pixmap(&[small, large]).expect("a pixmap is chosen");
        assert_eq!((icon.width(), icon.height()), (2, 1));
    }

    #[test]
    fn select_pixmap_skips_invalid_entries() {
        // Zero-sized and length-inconsistent entries are not usable icons.
        let zero = (0, 0, vec![]);
        let truncated = (4, 4, vec![0, 0, 0, 0]); // claims 64 bytes, has 4
        assert!(select_pixmap(&[zero, truncated]).is_none());
        assert!(select_pixmap(&[]).is_none());
    }

    #[test]
    fn find_icon_file_returns_the_first_existing_match() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("discord.png");
        std::fs::write(&present, b"not really a png, but it exists").unwrap();

        let dirs = vec![PathBuf::from("/nonexistent"), dir.path().to_path_buf()];
        assert_eq!(find_icon_file("discord", &dirs), Some(present));
        // A name with no backing file resolves to nothing.
        assert_eq!(find_icon_file("missing", &dirs), None);
        // An empty name never matches.
        assert_eq!(find_icon_file("", &dirs), None);
    }

    #[test]
    fn find_icon_file_accepts_an_absolute_file_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("icon.png");
        std::fs::write(&file, b"x").unwrap();
        let name = file.to_string_lossy().into_owned();
        assert_eq!(find_icon_file(&name, &[]), Some(file));
    }

    #[test]
    fn item_props_fall_back_to_id_when_the_title_is_blank() {
        let item = tray_item_from_props(":1.1", "discord", "", "Active", None);
        assert_eq!(item.key(), ":1.1");
        // Blank title falls back to the id so the initial is meaningful.
        assert_eq!(item.title(), "discord");
        assert_eq!(item.status(), TrayStatus::Active);
        assert_eq!(item.fallback_label(), "D");
    }

    #[test]
    fn item_props_prefer_the_title_over_the_id() {
        let item = tray_item_from_props(":1.1", "discord", "Discord — 3 unread", "Bogus", None);
        assert_eq!(item.title(), "Discord — 3 unread");
        // An unknown status string normalizes to Passive rather than failing.
        assert_eq!(item.status(), TrayStatus::Passive);
    }

    #[test]
    fn icon_search_dirs_leads_with_the_item_theme_path() {
        let dirs = icon_search_dirs("/home/u/.local/share/icons");
        assert_eq!(
            dirs.first(),
            Some(&PathBuf::from("/home/u/.local/share/icons"))
        );
        // A blank theme path contributes no leading entry.
        let plain = icon_search_dirs("");
        assert!(!plain.contains(&PathBuf::from("")));
    }
}
