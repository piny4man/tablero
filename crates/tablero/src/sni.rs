//! StatusNotifierItem (SNI) system-tray source.
//!
//! Background applications expose tray icons over DBus using the
//! StatusNotifierItem spec: each item registers itself with a
//! *StatusNotifierWatcher*, and *hosts* (panels, bars) watch the watcher to learn
//! which items exist and read their properties. This module is the host: it
//! emits typed tray snapshots and menu layouts through the [producer
//! bridge](crate::producer), then executes tray commands back over DBus, so the
//! rendering code never talks to the bus directly.
//!
//! Under a bare compositor such as Hyprland there is usually **no** watcher
//! running, so the producer hosts one itself: it serves the
//! `org.kde.StatusNotifierWatcher` interface and only falls back to an existing
//! watcher when another process already owns the name. Either way the host then
//! drives everything through a `StatusNotifierWatcherProxy`, so the two paths
//! converge immediately after start-up.
//!
//! All the data-shaping logic — parsing a registration address, choosing a
//! pixmap, resolving an icon name to a themed PNG, folding raw item properties
//! into a [`TrayItem`] — lives in pure functions the unit tests drive directly.
//! The live DBus plumbing around them is kept deliberately thin.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::{Either, select};
use futures_util::stream::{BoxStream, SelectAll, StreamExt, pending, select_all};
use log::warn;
use zbus::fdo::{DBusProxy, RequestNameReply};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, interface, proxy};

use crate::widget::{
    Command, Msg, TrayIcon, TrayItem, TrayMenu, TrayMenuItem, TrayMenuMode, TrayMenuToggle,
    TrayMenuToggleKind, TrayMenuToggleState, TrayState, TrayStatus,
};

use crate::command::CommandReceiver;
use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// The well-known bus name a StatusNotifierWatcher owns.
const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
/// The object path the watcher interface is served at.
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
/// The default object path of an item registered by bus name alone.
const DEFAULT_ITEM_PATH: &str = "/StatusNotifierItem";
/// The upper bound on any single DBus call to an item or menu. zbus applies no
/// call timeout of its own, so an unanswered call parks the awaiting task
/// forever — seen at session start, when items register before they are ready
/// to serve properties. Bounding every peer call keeps one unresponsive app
/// from freezing the tray for the whole session: timed-out reads retain cached
/// state and retry, timed-out commands are logged and skipped.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// A single raw `IconPixmap` entry: `(width, height, ARGB32 bytes)`.
type RawPixmap = (i32, i32, Vec<u8>);
/// Recursive DBusMenu layout node `(id, properties, variant children)`.
type RawMenuLayout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// Normalize one recursive DBusMenu layout node.
pub fn parse_menu_layout(value: OwnedValue) -> Option<TrayMenuItem> {
    let (id, properties, children): RawMenuLayout = value.try_into().ok()?;
    parse_raw_menu_layout((id, properties, children))
}

fn parse_raw_menu_layout((id, properties, children): RawMenuLayout) -> Option<TrayMenuItem> {
    let string_property = |name: &str| {
        properties
            .get(name)
            .and_then(|value| <&str>::try_from(value).ok())
            .unwrap_or_default()
            .to_string()
    };
    let bool_property = |name: &str, default: bool| {
        properties
            .get(name)
            .and_then(|value| bool::try_from(value).ok())
            .unwrap_or(default)
    };

    let toggle = match string_property("toggle-type").as_str() {
        "checkmark" => Some(TrayMenuToggle {
            kind: TrayMenuToggleKind::Checkmark,
            state: toggle_state(&properties),
        }),
        "radio" => Some(TrayMenuToggle {
            kind: TrayMenuToggleKind::Radio,
            state: toggle_state(&properties),
        }),
        _ => None,
    };
    Some(TrayMenuItem {
        id,
        label: string_property("label"),
        enabled: bool_property("enabled", true),
        visible: bool_property("visible", true),
        separator: string_property("type") == "separator",
        toggle,
        children: children.into_iter().filter_map(parse_menu_layout).collect(),
    })
}

fn toggle_state(properties: &HashMap<String, OwnedValue>) -> TrayMenuToggleState {
    match properties
        .get("toggle-state")
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(-1)
    {
        0 => TrayMenuToggleState::Off,
        1 => TrayMenuToggleState::On,
        _ => TrayMenuToggleState::Indeterminate,
    }
}

/// Split a watcher registration address into its `(bus_name, object_path)`.
///
/// An address is either a bare bus name (`":1.42"` or a well-known name), which
/// implies the default item path, or a bus name immediately
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

/// Choose the best icon-pixmap entry and decode it into a [`TrayIcon`].
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
/// tray commands carry back. The display
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

/// Normalize optional SNI menu properties into click behavior.
pub fn menu_mode(menu_path: Option<&str>, item_is_menu: Option<bool>) -> TrayMenuMode {
    let has_menu = menu_path.is_some_and(|path| {
        let path = path.trim();
        !path.is_empty() && path != "/" && path != "/NO_DBUSMENU"
    });
    if !has_menu {
        TrayMenuMode::None
    } else if item_is_menu.unwrap_or(true) {
        // AppIndicator implementations frequently omit ItemIsMenu despite being
        // menu-only; defaulting to Primary matches established tray hosts.
        TrayMenuMode::Primary
    } else {
        TrayMenuMode::Secondary
    }
}

/// Resolve an item's icon: prefer an embedded pixmap, else a themed PNG.
///
/// Pixmaps are self-contained and need no theme lookup, so they win when present;
/// otherwise the icon name is resolved against the item's `IconThemePath` and the
/// standard system icon directories. Any failure along the themed path (no file,
/// unreadable, undecodable) yields `None` — the item simply shows its initial.
///
/// The PNG read is async so it never blocks the single-worker producer runtime.
async fn resolve_icon(
    icon_name: &str,
    pixmaps: &[RawPixmap],
    theme_path: &str,
) -> Option<TrayIcon> {
    if let Some(icon) = select_pixmap(pixmaps) {
        return Some(icon);
    }
    let dirs = icon_search_dirs(theme_path);
    let file = find_icon_file(icon_name, &dirs)?;
    let bytes = tokio::fs::read(file).await.ok()?;
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
/// `Id` is mandatory and doubles as a liveness check: once the item's bus owner
/// exits, the failed call drops the stale watcher registration instead of
/// rendering it as an unknown `?` icon. Optional presentation properties still
/// degrade to empty defaults so a partially implemented live item remains usable.
///
/// The read is bounded by [`CALL_TIMEOUT`]: an item that accepts a call but
/// never replies is treated as unreadable, exactly like a fast failure — the
/// cached item is retained and the read is retried, so one hung peer can never
/// park the producer.
async fn read_item(conn: &Connection, key: &str) -> Option<TrayItem> {
    match tokio::time::timeout(CALL_TIMEOUT, read_item_props(conn, key)).await {
        Ok(item) => item,
        Err(_) => {
            warn!("sni: reading item {key:?} timed out; treating it as unreadable");
            None
        }
    }
}

/// The raw, unbounded property read behind [`read_item`]; separated so the
/// caller can apply the timeout.
async fn read_item_props(conn: &Connection, key: &str) -> Option<TrayItem> {
    let (name, path) = parse_item_address(key)?;
    let item = StatusNotifierItemProxy::builder(conn)
        .destination(name)
        .ok()?
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;

    let id = item.id().await.ok()?;
    let title = item.title().await.unwrap_or_default();
    let status = item.status().await.unwrap_or_default();
    let icon_name = item.icon_name().await.unwrap_or_default();
    let theme_path = item.icon_theme_path().await.unwrap_or_default();
    let pixmaps = item.icon_pixmap().await.unwrap_or_default();
    let menu = item.menu().await.ok();
    let item_is_menu = item.item_is_menu().await.ok();

    let icon = resolve_icon(&icon_name, &pixmaps, &theme_path).await;
    let mode = menu_mode(menu.as_deref().map(|path| path.as_str()), item_is_menu);
    Some(tray_item_from_props(key, &id, &title, &status, icon).with_menu(mode))
}

/// Build the current tray snapshot by reading every registered item.
///
/// Items are read concurrently so one slow item cannot delay the rest, and
/// each read is bounded by [`CALL_TIMEOUT`] (see [`read_item`]). Items that
/// cannot be read at all are dropped; everything else is folded into a
/// normalized [`TrayState`] (which de-duplicates and sorts), so a
/// re-enumeration that finds the same set produces an equal snapshot and no
/// repaint.
async fn reconcile_state(
    conn: &Connection,
    addresses: &[String],
    items: &mut HashMap<String, TrayItem>,
) -> (TrayState, bool) {
    let readings = futures_util::future::join_all(addresses.iter().map(|address| async move {
        let item = read_item(conn, address).await;
        if item.is_none() {
            warn!("sni: reading registered item {address:?} failed; retaining cached state");
        }
        (address.clone(), item)
    }))
    .await;
    let needs_retry = reconcile_items(items, addresses, readings);
    (TrayState::new(items.values().cloned()), needs_retry)
}

/// Merge fresh item reads into the last known-good snapshot.
///
/// Watcher membership is authoritative for removal. A failed property read for
/// an address that is still registered retains its cached item and requests a
/// retry, preventing one transient DBus error from clearing the whole tray.
fn reconcile_items(
    items: &mut HashMap<String, TrayItem>,
    addresses: &[String],
    readings: Vec<(String, Option<TrayItem>)>,
) -> bool {
    let registered: HashSet<&str> = addresses.iter().map(String::as_str).collect();
    items.retain(|address, _| registered.contains(address.as_str()));

    let mut needs_retry = false;
    for (address, reading) in readings {
        match reading {
            Some(item) => {
                items.insert(address, item);
            }
            None => needs_retry = true,
        }
    }
    needs_retry
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
        let Some(owner) = header.sender().map(ToString::to_string) else {
            return;
        };
        // A bare object path carries no bus name; the real address is the
        // caller's unique name plus that path.
        let address = if service.starts_with('/') {
            format!("{owner}{service}")
        } else {
            service.to_string()
        };
        // Install the match rule before acknowledging registration so an app
        // that exits immediately cannot leave a stale address behind.
        let owner_lost = match DBusProxy::new(emitter.connection()).await {
            Ok(dbus) => dbus
                .receive_name_owner_changed_with_args(&[(0, owner.as_str()), (2, "")])
                .await
                .ok(),
            Err(_) => None,
        };

        let inserted = self
            .items
            .lock()
            .map(|mut items| items.insert(address.clone()))
            .unwrap_or(false);
        if inserted {
            if let Err(e) = Self::status_notifier_item_registered(&emitter, &address).await {
                warn!("failed to emit StatusNotifierItemRegistered for {address}: {e}");
            }
            // PropertiesChanged is what keeps caching proxies (other hosts, or
            // a host built before this registration) from serving a frozen
            // membership list.
            if let Err(e) = self
                .registered_status_notifier_items_changed(&emitter)
                .await
            {
                warn!("failed to emit PropertiesChanged for {address}: {e}");
            }
        }
        if inserted && let Some(mut owner_lost) = owner_lost {
            let emitter = emitter.to_owned();
            let items = Arc::clone(&self.items);
            tokio::spawn(async move {
                if owner_lost.next().await.is_none() {
                    return;
                }
                let removed = remove_registered_item(&items, &address);
                if removed {
                    if let Err(error) =
                        Self::status_notifier_item_unregistered(&emitter, &address).await
                    {
                        warn!(
                            "failed to emit StatusNotifierItemUnregistered for {address}: {error}"
                        );
                    }
                    let watcher = Watcher { items };
                    if let Err(error) = watcher
                        .registered_status_notifier_items_changed(&emitter)
                        .await
                    {
                        warn!("failed to emit PropertiesChanged for {address}: {error}");
                    }
                }
            });
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

fn remove_registered_item(items: &Mutex<HashSet<String>>, address: &str) -> bool {
    items
        .lock()
        .map(|mut items| items.remove(address))
        .unwrap_or(false)
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

    /// Object path implementing `com.canonical.dbusmenu`.
    #[zbus(property)]
    fn menu(&self) -> zbus::Result<OwnedObjectPath>;

    /// Whether primary activation should open [`menu`](Self::menu).
    #[zbus(property)]
    fn item_is_menu(&self) -> zbus::Result<bool>;

    /// Primary activation (a left click), at screen coordinates `(x, y)`.
    fn activate(&self, x: i32, y: i32) -> zbus::Result<()>;

    /// Ask the item to show its own context menu.
    fn context_menu(&self, x: i32, y: i32) -> zbus::Result<()>;

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

/// The subset of `com.canonical.dbusmenu` needed to present and activate menus.
#[proxy(interface = "com.canonical.dbusmenu")]
trait DBusMenu {
    /// Fetch one recursive menu layout and its revision.
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: &[&str],
    ) -> zbus::Result<(u32, RawMenuLayout)>;

    /// Give the exporter a chance to refresh an entry before it is shown.
    fn about_to_show(&self, id: i32) -> zbus::Result<bool>;

    /// Deliver a user interaction to a menu entry.
    fn event(&self, id: i32, event_id: &str, data: OwnedValue, timestamp: u32) -> zbus::Result<()>;

    /// The menu layout revision changed.
    #[zbus(signal)]
    fn layout_updated(&self, revision: u32, parent: i32) -> zbus::Result<()>;

    /// Properties changed without replacing the full layout.
    #[zbus(signal)]
    fn items_properties_updated(
        &self,
        updated: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;
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

/// Build the long-lived watcher proxy the host reads membership through.
///
/// Property caching must stay off: zbus refreshes a cached property only on
/// `PropertiesChanged`, which not every watcher emits for
/// `RegisteredStatusNotifierItems`. A cached read would freeze the membership
/// list at its startup value for the whole session — the tray would populate
/// once and never show later registrations or removals.
async fn build_watcher_proxy(
    conn: &Connection,
) -> zbus::Result<StatusNotifierWatcherProxy<'static>> {
    StatusNotifierWatcherProxy::builder(conn)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
}

/// One startup attempt: connect to the session bus, host (or defer to) the
/// StatusNotifierWatcher, and register as a host, ready for enumeration.
///
/// Retried by [`run`] until it succeeds: at compositor start the session bus
/// or an external watcher may not be ready yet, and giving up would leave the
/// tray dead for the whole session.
async fn connect_and_register() -> zbus::Result<(Connection, StatusNotifierWatcherProxy<'static>)> {
    let conn = Connection::session().await?;
    ensure_watcher(&conn).await?;

    let watcher = build_watcher_proxy(&conn).await?;
    let host_name = format!("org.kde.StatusNotifierHost-{}-0", std::process::id());
    // External watchers such as Waybar validate that a host owns the service
    // name it registers. Without claiming it, initial enumeration can work but
    // subsequent Registered/Unregistered lifecycle delivery is unreliable.
    conn.request_name(host_name.as_str()).await?;
    // Best-effort: a watcher that rejects host registration still lets us read
    // items, so a failure here is logged, not fatal.
    if let Err(e) = watcher.register_status_notifier_host(&host_name).await {
        warn!("sni: registering host failed: {e}");
    }
    Ok((conn, watcher))
}

/// Drive the tray stream: ensure a watcher, register as host, then emit a fresh
/// snapshot on every lifecycle or per-item change.
///
/// Re-enumeration is deliberately whole-snapshot: any change rebuilds the full
/// [`TrayState`], which is cheap relative to the change rate and keeps dynamic
/// item set correct without bespoke per-item bookkeeping. A low-cost periodic
/// membership check recovers missed or closed signal streams without repeatedly
/// decoding unchanged icon pixmaps. Startup is retried until the session bus
/// and watcher are usable — failing here would leave the tray dead for the
/// whole session when the bar starts before the bus is ready. Returns `Ok(())`
/// once the render loop drops its receiver.
async fn run(tx: MsgSender) -> ProducerResult {
    const RETRY_DELAY: Duration = Duration::from_secs(1);
    const RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

    let (conn, watcher) = loop {
        match connect_and_register().await {
            Ok(setup) => break setup,
            Err(error) => {
                warn!("sni: startup failed: {error}; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    };

    // Keep lifecycle subscriptions alive across snapshot rebuilds. Recreating
    // them after every read leaves a window where a newly opened app can
    // register with the watcher without waking this host.
    let mut lifecycle = lifecycle_stream(&watcher).await?;
    let mut lifecycle_closed = false;
    let mut items = HashMap::new();
    let mut addresses = loop {
        match watcher.registered_status_notifier_items().await {
            Ok(addresses) => break normalize_addresses(addresses),
            Err(error) => {
                warn!("sni: reading registered items failed: {error}; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    };
    let (state, mut needs_retry) = reconcile_state(&conn, &addresses, &mut items).await;
    if tx.send(Msg::Tray(state)).is_err() {
        return Ok(());
    }
    let mut changes = item_change_streams(&conn, &addresses).await;

    loop {
        let delay = if needs_retry {
            RETRY_DELAY
        } else {
            RECONCILE_INTERVAL
        };
        let wake = wait_for_sni_change(&mut lifecycle, &mut changes, delay).await;
        let mut rebuild_changes = matches!(wake, SniWake::Item(None));

        if matches!(wake, SniWake::Lifecycle(None)) {
            warn!("sni: watcher lifecycle stream closed; falling back to reconciliation polling");
            lifecycle = pending().boxed();
            lifecycle_closed = true;
        }

        if matches!(wake, SniWake::Poll) && lifecycle_closed {
            match lifecycle_stream(&watcher).await {
                Ok(stream) => {
                    lifecycle = stream;
                    lifecycle_closed = false;
                }
                Err(error) => warn!("sni: restoring watcher lifecycle stream failed: {error}"),
            }
        }

        let current_addresses = match watcher.registered_status_notifier_items().await {
            Ok(addresses) => normalize_addresses(addresses),
            Err(error) => {
                warn!("sni: reading registered items failed: {error}; preserving tray state");
                needs_retry = true;
                continue;
            }
        };
        let addresses_changed = current_addresses != addresses;

        // A healthy poll only checks membership. Item properties are read again
        // when a signal fires, membership changes, or an earlier read failed.
        if matches!(wake, SniWake::Poll) && !addresses_changed && !needs_retry && !rebuild_changes {
            continue;
        }

        addresses = current_addresses;
        let (state, retry) = reconcile_state(&conn, &addresses, &mut items).await;
        needs_retry = retry;
        if tx.send(Msg::Tray(state)).is_err() {
            return Ok(());
        }
        rebuild_changes |= addresses_changed;
        if rebuild_changes {
            changes = item_change_streams(&conn, &addresses).await;
        }
    }
}

fn normalize_addresses(mut addresses: Vec<String>) -> Vec<String> {
    addresses.sort_unstable();
    addresses.dedup();
    addresses
}

async fn lifecycle_stream<'a>(
    watcher: &'a StatusNotifierWatcherProxy<'_>,
) -> zbus::Result<BoxStream<'a, ()>> {
    // No RegisteredStatusNotifierItems property stream here: the watcher proxy
    // runs with property caching off, and a PropertyStream never yields without
    // the cache. The two lifecycle signals plus the reconcile poll cover
    // membership changes.
    Ok(select_all(vec![
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
    ])
    .boxed())
}

async fn item_change_streams(
    conn: &Connection,
    addresses: &[String],
) -> SelectAll<BoxStream<'static, ()>> {
    let mut streams = Vec::new();
    for address in addresses {
        if let Some(item) = item_change_stream(conn, address).await {
            streams.push(item);
        }
    }
    select_all(streams)
}

enum SniWake {
    Lifecycle(Option<()>),
    Item(Option<()>),
    Poll,
}

async fn wait_for_sni_change(
    lifecycle: &mut BoxStream<'_, ()>,
    changes: &mut SelectAll<BoxStream<'static, ()>>,
    delay: Duration,
) -> SniWake {
    if changes.is_empty() {
        let lifecycle_change = lifecycle.next();
        let poll = tokio::time::sleep(delay);
        futures_util::pin_mut!(lifecycle_change, poll);
        return match select(lifecycle_change, poll).await {
            Either::Left((change, _)) => SniWake::Lifecycle(change),
            Either::Right(_) => SniWake::Poll,
        };
    }

    let lifecycle_change = lifecycle.next();
    let item_change = changes.next();
    futures_util::pin_mut!(lifecycle_change, item_change);
    let change = select(lifecycle_change, item_change);
    let poll = tokio::time::sleep(delay);
    futures_util::pin_mut!(change, poll);
    match select(change, poll).await {
        Either::Left((Either::Left((change, _)), _)) => SniWake::Lifecycle(change),
        Either::Left((Either::Right((change, _)), _)) => SniWake::Item(change),
        Either::Right(_) => SniWake::Poll,
    }
}

/// A merged "this item changed" stream for one item: icon, title, status,
/// attention-icon, or bus-owner updates collapsed to `()` ticks.
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
        .destination(name.clone())
        .ok()?
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;
    // Keep whichever subscriptions succeed: one failure must not silently
    // strip the item of every wake source, which would leave it stale until a
    // membership change.
    let mut streams: Vec<BoxStream<'static, ()>> = Vec::new();
    if let Ok(dbus) = DBusProxy::new(conn).await
        && let Ok(owner_changes) = dbus
            .receive_name_owner_changed_with_args(&[(0, name.as_str())])
            .await
    {
        streams.push(owner_changes.map(|_| ()).boxed());
    }
    if let Ok(stream) = item.receive_new_icon().await {
        streams.push(stream.map(|_| ()).boxed());
    }
    if let Ok(stream) = item.receive_new_title().await {
        streams.push(stream.map(|_| ()).boxed());
    }
    if let Ok(stream) = item.receive_new_status().await {
        streams.push(stream.map(|_| ()).boxed());
    }
    if let Ok(stream) = item.receive_new_attention_icon().await {
        streams.push(stream.map(|_| ()).boxed());
    }
    if streams.is_empty() {
        warn!("sni: no change subscriptions succeeded for {address}");
        return None;
    }
    if streams.len() < 5 {
        warn!(
            "sni: only {} of 5 change subscriptions succeeded for {address}",
            streams.len()
        );
    }
    Some(select_all(streams).boxed())
}

/// Run one DBus `call` bounded by [`CALL_TIMEOUT`].
///
/// zbus applies no call timeout of its own, so without a bound an unanswered
/// call parks the awaiting task forever. A timeout is reported as a generic
/// failure so the caller's existing log-and-skip (or retry) handling covers it.
async fn bounded<F, T>(call: F) -> zbus::Result<T>
where
    F: Future<Output = zbus::Result<T>>,
{
    bounded_for(CALL_TIMEOUT, call).await
}

/// [`bounded`] with an explicit timeout, so tests can use tiny durations.
async fn bounded_for<F, T>(timeout: Duration, call: F) -> zbus::Result<T>
where
    F: Future<Output = zbus::Result<T>>,
{
    match tokio::time::timeout(timeout, call).await {
        Ok(result) => result,
        Err(_) => Err(zbus::Error::Failure("call timed out".to_string())),
    }
}

/// Drain tray commands from the render loop and execute SNI/DBusMenu calls.
///
/// Runs on the producer bridge as the executor end of the
/// [command channel](crate::command), alongside the Hyprland executor. This
/// executor handles item activation, menu opening, and menu-entry events;
/// unrelated commands are ignored, mirroring the Hyprland executor. Every call
/// is bounded by the internal call timeout; a failed or timed-out call is
/// logged and skipped, so one bad item never ends the stream. The session-bus
/// connection is retried like the producer's startup — dying here would leave
/// tray
/// clicks and menus dead for the whole session when the bar starts before the
/// bus is ready. Returns `Ok(())` when the render loop drops its sender.
pub async fn run_commands(mut commands: CommandReceiver, updates: MsgSender) -> ProducerResult {
    const RETRY_DELAY: Duration = Duration::from_secs(1);
    let conn = loop {
        match Connection::session().await {
            Ok(conn) => break conn,
            Err(error) => {
                warn!("sni: command executor cannot reach the session bus: {error}; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    };
    let mut monitored_menus = HashSet::new();
    while let Some(command) = commands.recv().await {
        match command {
            Command::ActivateTrayItem { key, x, y } => {
                if let Err(e) = bounded(activate(&conn, &key, x, y)).await {
                    warn!("sni: activating {key:?} failed: {e}");
                }
            }
            Command::OpenTrayMenu { key, x, y } => {
                if let Err(e) = bounded(open_menu(&conn, &updates, &key, x, y)).await {
                    warn!("sni: opening menu for {key:?} failed: {e}");
                } else if monitored_menus.insert(key.clone()) {
                    let monitor_conn = conn.clone();
                    let monitor_updates = updates.clone();
                    tokio::spawn(async move {
                        if let Err(error) =
                            monitor_menu(monitor_conn, monitor_updates, key.clone()).await
                        {
                            warn!("sni: monitoring menu for {key:?} failed: {error}");
                        }
                    });
                }
            }
            Command::ActivateTrayMenuItem { key, id } => {
                if let Err(e) = bounded(activate_menu_item(&conn, &key, id)).await {
                    warn!("sni: activating menu item {id} for {key:?} failed: {e}");
                }
            }
            _ => {}
        }
    }
    Ok(())
}

async fn item_proxy<'a>(
    conn: &'a Connection,
    key: &str,
) -> zbus::Result<StatusNotifierItemProxy<'a>> {
    let (name, path) = parse_item_address(key)
        .ok_or_else(|| zbus::Error::Failure(format!("bad address {key}")))?;
    StatusNotifierItemProxy::builder(conn)
        .destination(name)?
        .path(path)?
        .build()
        .await
}

async fn menu_proxy<'a>(
    conn: &'a Connection,
    key: &str,
) -> zbus::Result<Option<DBusMenuProxy<'a>>> {
    let (name, _) = parse_item_address(key)
        .ok_or_else(|| zbus::Error::Failure(format!("bad address {key}")))?;
    let Ok(path) = item_proxy(conn, key).await?.menu().await else {
        return Ok(None);
    };
    if menu_mode(Some(path.as_str()), Some(false)) == TrayMenuMode::None {
        return Ok(None);
    }
    Ok(Some(
        DBusMenuProxy::builder(conn)
            .destination(name)?
            .path(path)?
            .build()
            .await?,
    ))
}

async fn open_menu(
    conn: &Connection,
    updates: &MsgSender,
    key: &str,
    x: i32,
    y: i32,
) -> zbus::Result<()> {
    let Some(menu) = menu_proxy(conn, key).await? else {
        return context_menu_fallback(conn, updates, key, x, y).await;
    };

    // A false return only means the exporter did not change the entry.
    let _ = menu.about_to_show(0).await;
    if send_menu_layout(&menu, updates, key).await.is_err() {
        context_menu_fallback(conn, updates, key, x, y).await
    } else {
        Ok(())
    }
}

async fn context_menu_fallback(
    conn: &Connection,
    updates: &MsgSender,
    key: &str,
    x: i32,
    y: i32,
) -> zbus::Result<()> {
    let result = item_proxy(conn, key).await?.context_menu(x, y).await;
    let _ = updates.send(Msg::TrayMenuUnavailable(key.to_string()));
    result
}

async fn send_menu_layout(
    menu: &DBusMenuProxy<'_>,
    updates: &MsgSender,
    key: &str,
) -> zbus::Result<()> {
    const PROPERTIES: [&str; 7] = [
        "type",
        "label",
        "enabled",
        "visible",
        "toggle-type",
        "toggle-state",
        "children-display",
    ];
    let (mut revision, raw_root) = menu.get_layout(0, -1, &PROPERTIES).await?;
    let mut root = parse_raw_menu_layout(raw_root)
        .ok_or_else(|| zbus::Error::Failure("invalid DBusMenu root layout".into()))?;
    let mut submenu_ids = Vec::new();
    collect_submenu_ids(&root.children, &mut submenu_ids);
    let mut changed = false;
    for id in submenu_ids {
        changed |= menu.about_to_show(id).await.unwrap_or(false);
    }
    if changed {
        let refreshed = menu.get_layout(0, -1, &PROPERTIES).await?;
        revision = refreshed.0;
        root = parse_raw_menu_layout(refreshed.1)
            .ok_or_else(|| zbus::Error::Failure("invalid refreshed DBusMenu layout".into()))?;
    }
    updates
        .send(Msg::TrayMenu(TrayMenu {
            key: key.to_string(),
            revision,
            items: root.children,
        }))
        .map_err(|_| zbus::Error::Failure("render loop closed".into()))
}

fn collect_submenu_ids(items: &[TrayMenuItem], ids: &mut Vec<i32>) {
    for item in items {
        if !item.children.is_empty() {
            ids.push(item.id);
            collect_submenu_ids(&item.children, ids);
        }
    }
}

async fn monitor_menu(conn: Connection, updates: MsgSender, key: String) -> zbus::Result<()> {
    let Some(menu) = menu_proxy(&conn, &key).await? else {
        return Ok(());
    };
    let mut changes = select_all([
        menu.receive_layout_updated().await?.map(|_| ()).boxed(),
        menu.receive_items_properties_updated()
            .await?
            .map(|_| ())
            .boxed(),
    ]);
    while changes.next().await.is_some() {
        // Re-fetching the normalized tree handles both structural and property
        // signals with one consistent path and rejects malformed partial deltas.
        let _ = bounded(menu.about_to_show(0)).await;
        // A failed or timed-out refresh must not end the monitor: the menu is
        // never re-monitored for the session (`monitored_menus` retains the
        // key), so log and wait for the next change signal instead.
        if let Err(error) = bounded(send_menu_layout(&menu, &updates, &key)).await {
            warn!("sni: refreshing menu for {key:?} failed: {error}");
        }
    }
    Ok(())
}

async fn activate_menu_item(conn: &Connection, key: &str, id: i32) -> zbus::Result<()> {
    let Some(menu) = menu_proxy(conn, key).await? else {
        return Ok(());
    };
    menu.event(id, "clicked", OwnedValue::from(0i32), 0).await
}

/// Send a primary `Activate(x, y)` to the item registered under `key`.
async fn activate(conn: &Connection, key: &str, x: i32, y: i32) -> zbus::Result<()> {
    item_proxy(conn, key).await?.activate(x, y).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    fn property(value: impl Into<Value<'static>>) -> OwnedValue {
        OwnedValue::try_from(value.into()).expect("owned DBus value")
    }

    fn raw_layout(
        id: i32,
        properties: HashMap<String, OwnedValue>,
        children: Vec<OwnedValue>,
    ) -> OwnedValue {
        property((id, properties, children))
    }

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
    fn owner_loss_allows_the_same_item_address_to_register_again() {
        let address = "org.example.Tray/StatusNotifierItem";
        let items = Mutex::new(HashSet::from([address.to_string()]));

        assert!(remove_registered_item(&items, address));
        assert!(items.lock().unwrap().insert(address.to_string()));
    }

    #[test]
    fn transient_item_read_failure_preserves_the_cached_item() {
        let first = tray_item_from_props(":1.1", "first", "First", "Active", None);
        let second = tray_item_from_props(":1.2", "second", "Second", "Active", None);
        let updated_second =
            tray_item_from_props(":1.2", "second", "Second updated", "Active", None);
        let mut items = HashMap::from([
            (first.key().to_string(), first.clone()),
            (second.key().to_string(), second),
        ]);

        let needs_retry = reconcile_items(
            &mut items,
            &[":1.1".to_string(), ":1.2".to_string()],
            vec![
                (":1.1".to_string(), None),
                (":1.2".to_string(), Some(updated_second.clone())),
            ],
        );

        assert!(needs_retry);
        assert_eq!(items.get(":1.1"), Some(&first));
        assert_eq!(items.get(":1.2"), Some(&updated_second));
    }

    #[test]
    fn watcher_membership_removes_items_even_when_no_read_succeeds() {
        let removed = tray_item_from_props(":1.1", "first", "First", "Active", None);
        let remaining = tray_item_from_props(":1.2", "second", "Second", "Active", None);
        let mut items = HashMap::from([
            (removed.key().to_string(), removed),
            (remaining.key().to_string(), remaining.clone()),
        ]);

        let needs_retry = reconcile_items(
            &mut items,
            &[":1.2".to_string()],
            vec![(":1.2".to_string(), None)],
        );

        assert!(needs_retry);
        assert_eq!(items.len(), 1);
        assert_eq!(items.get(":1.2"), Some(&remaining));
    }

    #[test]
    fn unreadable_new_item_is_retried_without_clearing_the_tray() {
        let existing = tray_item_from_props(":1.1", "first", "First", "Active", None);
        let mut items = HashMap::from([(existing.key().to_string(), existing.clone())]);

        let needs_retry = reconcile_items(
            &mut items,
            &[":1.1".to_string(), ":1.2".to_string()],
            vec![
                (":1.1".to_string(), Some(existing.clone())),
                (":1.2".to_string(), None),
            ],
        );

        assert!(needs_retry);
        assert_eq!(
            items,
            HashMap::from([(existing.key().to_string(), existing)])
        );
    }

    #[test]
    fn closed_lifecycle_stream_is_reported_for_recovery() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let mut lifecycle = futures_util::stream::empty().boxed();
                let mut changes = select_all(Vec::<BoxStream<'static, ()>>::new());

                let wake =
                    wait_for_sni_change(&mut lifecycle, &mut changes, Duration::from_secs(1)).await;

                assert!(matches!(wake, SniWake::Lifecycle(None)));
            });
    }

    #[test]
    fn reconciliation_poll_wakes_without_item_streams() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let mut lifecycle = pending().boxed();
                let mut changes = select_all(Vec::<BoxStream<'static, ()>>::new());

                let wake = wait_for_sni_change(&mut lifecycle, &mut changes, Duration::ZERO).await;

                assert!(matches!(wake, SniWake::Poll));
            });
    }

    #[test]
    fn an_unanswered_call_is_bounded_instead_of_parking_forever() {
        // zbus applies no call timeout, so a peer that never replies must be
        // cut off by our own bound, surfacing as an ordinary failure.
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let never = std::future::pending::<zbus::Result<()>>();
                let result = bounded_for(Duration::from_millis(10), never).await;
                assert!(matches!(result, Err(zbus::Error::Failure(_))));
            });
    }

    #[test]
    fn a_prompt_answer_passes_through_the_bound() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let prompt = async { zbus::Result::Ok(7) };
                assert_eq!(
                    bounded_for(Duration::from_secs(1), prompt).await.ok(),
                    Some(7)
                );
            });
    }

    #[test]
    fn an_inner_error_passes_through_the_bound() {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let failing = async { zbus::Result::<()>::Err(zbus::Error::Unsupported) };
                let result = bounded_for(Duration::from_secs(1), failing).await;
                assert!(matches!(result, Err(zbus::Error::Unsupported)));
            });
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
    fn menu_mode_defaults_missing_item_is_menu_to_primary() {
        assert_eq!(menu_mode(Some("/MenuBar"), None), TrayMenuMode::Primary);
        assert_eq!(
            menu_mode(Some("/MenuBar"), Some(false)),
            TrayMenuMode::Secondary
        );
        assert_eq!(
            menu_mode(Some("/MenuBar"), Some(true)),
            TrayMenuMode::Primary
        );
    }

    #[test]
    fn absent_or_sentinel_menu_paths_have_no_exported_menu() {
        assert_eq!(menu_mode(None, Some(true)), TrayMenuMode::None);
        assert_eq!(menu_mode(Some("/"), Some(true)), TrayMenuMode::None);
        assert_eq!(
            menu_mode(Some("/NO_DBUSMENU"), Some(true)),
            TrayMenuMode::None
        );
    }

    #[test]
    fn dbusmenu_layout_normalizes_nested_toggles_separators_and_visibility() {
        let child = raw_layout(
            2,
            HashMap::from([
                ("label".into(), property("Nested")),
                ("enabled".into(), property(false)),
                ("toggle-type".into(), property("checkmark")),
                ("toggle-state".into(), property(1i32)),
            ]),
            vec![],
        );
        let separator = raw_layout(
            3,
            HashMap::from([("type".into(), property("separator"))]),
            vec![],
        );
        let hidden = raw_layout(
            4,
            HashMap::from([
                ("label".into(), property("Hidden")),
                ("visible".into(), property(false)),
            ]),
            vec![],
        );
        let root = raw_layout(
            0,
            HashMap::from([("label".into(), property("root"))]),
            vec![child, separator, hidden],
        );

        let parsed = parse_menu_layout(root).expect("valid recursive layout");
        assert_eq!(parsed.id, 0);
        assert_eq!(parsed.children.len(), 3);
        assert_eq!(parsed.children[0].label, "Nested");
        assert!(!parsed.children[0].enabled);
        assert_eq!(
            parsed.children[0].toggle,
            Some(TrayMenuToggle {
                kind: TrayMenuToggleKind::Checkmark,
                state: TrayMenuToggleState::On,
            })
        );
        assert!(parsed.children[1].separator);
        assert!(!parsed.children[2].visible);
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

    /// A private session bus for live-bus tests, killed on drop.
    struct PrivateBus {
        child: std::process::Child,
        address: String,
    }

    impl PrivateBus {
        /// Spawn `dbus-daemon` on a fresh address, or `None` if unavailable.
        fn spawn() -> Option<Self> {
            let mut child = std::process::Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .ok()?;
            let stdout = child.stdout.take()?;
            let mut address = String::new();
            let read =
                std::io::BufRead::read_line(&mut std::io::BufReader::new(stdout), &mut address);
            let address = address.trim().to_string();
            if read.is_err() || address.is_empty() {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Some(Self { child, address })
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    async fn connect(address: &str) -> Connection {
        zbus::connection::Builder::address(address)
            .expect("valid bus address")
            .build()
            .await
            .expect("connection to private bus")
    }

    /// Regression test for the frozen tray: membership read through the
    /// production watcher proxy must reflect registrations and removals that
    /// happen *after* the proxy is built, and the hosted watcher must emit
    /// `PropertiesChanged` so caching consumers stay current too.
    #[test]
    fn hosted_watcher_propagates_membership_changes_after_startup() {
        let Some(bus) = PrivateBus::spawn() else {
            eprintln!("skipping: dbus-daemon is not available");
            return;
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let host = connect(&bus.address).await;
                ensure_watcher(&host).await.expect("hosting the watcher");
                let watcher = build_watcher_proxy(&host).await.expect("watcher proxy");
                assert!(
                    watcher
                        .registered_status_notifier_items()
                        .await
                        .expect("initial membership read")
                        .is_empty()
                );

                // A caching observer stays current only via PropertiesChanged;
                // its reads regression-test the watcher's signal emission.
                let observer = StatusNotifierWatcherProxy::new(&host)
                    .await
                    .expect("caching observer proxy");
                assert!(
                    observer
                        .registered_status_notifier_items()
                        .await
                        .expect("priming the observer cache")
                        .is_empty()
                );

                let app = connect(&bus.address).await;
                let expected = format!(
                    "{}{DEFAULT_ITEM_PATH}",
                    app.unique_name().expect("unique name")
                );
                app.call_method(
                    Some(WATCHER_NAME),
                    WATCHER_PATH,
                    Some("org.kde.StatusNotifierWatcher"),
                    "RegisterStatusNotifierItem",
                    &DEFAULT_ITEM_PATH,
                )
                .await
                .expect("registering the item");

                // The production (uncached) proxy sees the new item at once.
                assert_eq!(
                    watcher
                        .registered_status_notifier_items()
                        .await
                        .expect("membership read after registration"),
                    vec![expected.clone()]
                );
                // The caching observer converges once PropertiesChanged lands.
                wait_for_membership(&observer, std::slice::from_ref(&expected)).await;

                // Dropping the app's connection must unregister its item.
                drop(app);
                wait_for_membership(&watcher, &[]).await;
                wait_for_membership(&observer, &[]).await;
            });
    }

    /// Poll `proxy` until its membership equals `expected`, panicking after a
    /// bound so a regression fails instead of hanging.
    async fn wait_for_membership(proxy: &StatusNotifierWatcherProxy<'_>, expected: &[String]) {
        let converged = async {
            loop {
                let items = proxy
                    .registered_status_notifier_items()
                    .await
                    .unwrap_or_default();
                if items == expected {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), converged)
            .await
            .expect("membership converged to the expected set");
    }
}
