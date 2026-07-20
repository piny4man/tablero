//! swaync (SwayNotificationCenter) notification source and click executor.
//!
//! Talks to swaync natively over the session D-Bus and emits typed
//! [`Msg::Notifications`] snapshots through the [producer
//! bridge](crate::producer) — no `swaync-client` subprocess, no JSON. swaync
//! stays the notification daemon (it draws the popups and the control-center
//! panel); the bar only renders the indicator and issues the two toggles.
//!
//! swaync's control-center service lives at the well-known name
//! `SWAYNC_NAME`, same-named interface, path `/org/erikreider/swaync/cc`:
//!
//! * `GetSubscribeData() -> (dnd, cc_open, count, inhibited)` — the seed read;
//! * signal `SubscribeV2(count, dnd, cc_open, inhibited)` — pushed on every
//!   state change;
//! * `ToggleVisibility()` / `ToggleDnd()` — the click actions.
//!
//! ⚠️ The two tuple orders really are transposed between the seed method and
//! the signal (verified against swaync 0.12.6 introspection) — don't "fix"
//! one to match the other.
//!
//! The run loop is fully event-driven, in line with the project's idle-cost
//! rules: it parks on the `SubscribeV2` stream, plus an arg0-filtered
//! `NameOwnerChanged` watch so a swaync restart (or late start) wakes it to
//! re-seed and re-subscribe. While swaync is off the bus the widget holds a
//! `None` snapshot and shows nothing; nothing polls in the meantime.

use futures_util::stream::{StreamExt, select};
use log::warn;
use zbus::{Connection, proxy};

use crate::widget::{Command, Msg, Notifications};

use crate::command::CommandReceiver;
use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// swaync's well-known bus name (also its interface name).
const SWAYNC_NAME: &str = "org.erikreider.swaync.cc";

/// The subset of swaync's control-center interface the bar uses. zbus
/// generates `SwayNcProxy` from this.
#[proxy(
    interface = "org.erikreider.swaync.cc",
    default_service = "org.erikreider.swaync.cc",
    default_path = "/org/erikreider/swaync/cc"
)]
trait SwayNc {
    /// The current daemon state, for seeding: `(dnd, cc_open, count,
    /// inhibited)`. Note the field order differs from [`subscribe_v2`]'s.
    ///
    /// swaync is D-Bus-activatable (its name ships a service file), and a
    /// plain method call to an activatable name *starts* it. The seed is a
    /// presence probe — it must observe whether swaync is running, never
    /// resurrect a daemon the user stopped — hence `no_autostart`. The click
    /// toggles below keep the default: they are only reachable while the
    /// widget is visible, i.e. while swaync is alive.
    ///
    /// [`subscribe_v2`]: SwayNcProxy::receive_subscribe_v2
    #[zbus(no_autostart)]
    fn get_subscribe_data(&self) -> zbus::Result<(bool, bool, u32, bool)>;

    /// Toggle the control-center panel open/closed.
    fn toggle_visibility(&self) -> zbus::Result<()>;

    /// Toggle Do-Not-Disturb; returns the new state (unused — the
    /// `SubscribeV2` signal is the single source of truth).
    fn toggle_dnd(&self) -> zbus::Result<bool>;

    /// Pushed by swaync on every state change: `(count, dnd, cc_open,
    /// inhibited)`. Note the field order differs from `GetSubscribeData`'s.
    #[zbus(signal)]
    fn subscribe_v2(&self, count: u32, dnd: bool, cc_open: bool, inhibited: bool);
}

/// Normalize a swaync reading into a [`Notifications`] snapshot.
///
/// The daemon also reports whether the panel is open (`cc_open`) and whether
/// notifications are inhibited; both are deliberately dropped — the bar
/// renders only the pending count and DND, and folding unrendered fields into
/// the snapshot would force repaints on invisible changes. Pure over its
/// inputs so the tests drive the reading → message → widget path without a
/// live bus.
pub fn notifications_from_swaync(count: u32, dnd: bool) -> Notifications {
    Notifications::new(count, dnd)
}

/// A [`Producer`] that follows swaync's state and emits
/// [`Msg::Notifications`] snapshots on every change.
///
/// Construct with [`new`](NotificationsProducer::new) and hand it to the
/// producer bridge; it seeds from `GetSubscribeData`, then parks on the
/// `SubscribeV2` signal until the session bus closes or the render loop shuts
/// down.
pub struct NotificationsProducer;

impl NotificationsProducer {
    /// Create a notifications producer.
    pub fn new() -> Self {
        Self
    }
}

impl Default for NotificationsProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for NotificationsProducer {
    fn name(&self) -> String {
        "notifications".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx))
    }
}

/// Drive the event loop: seed, subscribe, park; re-seed whenever swaync's
/// bus name changes owner.
///
/// The seed read doubles as presence detection — an error means the name has
/// no owner (swaync not running), which degrades to a `None` snapshot so the
/// widget reserves no slot. The `NameOwnerChanged` watch is arg0-filtered to
/// [`SWAYNC_NAME`], so the loop wakes only when swaync itself appears,
/// vanishes, or restarts — any owner change tears down the signal stream and
/// re-enters the seed step, because a restarted daemon is a new unique name
/// the old stream no longer matches. A failed session-bus connection
/// propagates as an error the bridge logs and isolates.
async fn run(tx: MsgSender) -> ProducerResult {
    let conn = Connection::session().await?;
    let proxy = SwayNcProxy::new(&conn).await?;
    let dbus = zbus::fdo::DBusProxy::new(&conn).await?;
    let mut owner_changes = dbus
        .receive_name_owner_changed_with_args(&[(0, SWAYNC_NAME)])
        .await?;

    loop {
        let seed = match proxy.get_subscribe_data().await {
            Ok((dnd, _cc_open, count, _inhibited)) => Some(notifications_from_swaync(count, dnd)),
            // The call failing means the name has no owner: swaync isn't
            // running. Not an error for the bar — the widget just hides.
            Err(_) => None,
        };
        if tx.send(Msg::Notifications(seed)).is_err() {
            return Ok(());
        }

        // Merge the two wake sources into one stream (the workspace tokio
        // build has no `macros` feature, so no `select!`): swaync state
        // pushes, and the owner watch that says the state stream is stale.
        // The owner stream is borrowed, not consumed — it must outlive each
        // re-subscription so a restart while re-seeding is never missed.
        enum BusEvent {
            State(SubscribeV2),
            OwnerChanged,
        }
        let signals = proxy.receive_subscribe_v2().await?.map(BusEvent::State);
        let owners = (&mut owner_changes).map(|_| BusEvent::OwnerChanged);
        let mut merged = select(signals, owners);

        let mut reseed = false;
        while let Some(event) = merged.next().await {
            match event {
                BusEvent::State(signal) => match signal.args() {
                    Ok(args) => {
                        let snapshot = notifications_from_swaync(*args.count(), *args.dnd());
                        if tx.send(Msg::Notifications(Some(snapshot))).is_err() {
                            return Ok(());
                        }
                    }
                    // A malformed signal body is logged and skipped — one
                    // bad message never takes the source down.
                    Err(e) => warn!("notifications: bad SubscribeV2 args: {e}"),
                },
                // swaync appeared, vanished, or restarted: the signal stream
                // is pinned to the old owner, so tear it down, re-seed, and
                // re-subscribe against the new one.
                BusEvent::OwnerChanged => {
                    reseed = true;
                    break;
                }
            }
        }
        if !reseed {
            // Both streams ended: the session bus itself is gone — nothing
            // left to follow.
            return Ok(());
        }
    }
}

/// Drain `commands` from the render loop and execute the swaync toggles.
///
/// Runs on the producer bridge as the executor end of the
/// [command channel](crate::command), alongside the Hyprland and SNI
/// executors. Commands this executor does not handle are ignored. A failed
/// toggle is logged and skipped — the widget state is never updated
/// optimistically here, because the `SubscribeV2` signal already reports the
/// outcome. Returns `Ok(())` when the render loop drops its sender.
pub async fn run_commands(mut commands: CommandReceiver) -> ProducerResult {
    let conn = Connection::session().await?;
    let proxy = SwayNcProxy::new(&conn).await?;
    while let Some(command) = commands.recv().await {
        let result = match command {
            Command::ToggleNotificationPanel => proxy.toggle_visibility().await,
            Command::ToggleNotificationsDnd => proxy.toggle_dnd().await.map(|_| ()),
            _ => continue,
        };
        if let Err(e) = result {
            warn!("notifications: executing {command:?} failed: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readings_normalize_into_snapshots() {
        let quiet = notifications_from_swaync(0, false);
        assert_eq!(quiet.count(), 0);
        assert!(!quiet.dnd());

        let busy = notifications_from_swaync(7, true);
        assert_eq!(busy.count(), 7);
        assert!(busy.dnd());
    }

    #[test]
    fn snapshots_differing_only_in_dropped_fields_are_equal() {
        // cc_open / inhibited never reach the snapshot, so two daemon states
        // differing only there produce equal snapshots — and equal snapshots
        // are exactly what the widget's update() ignores without a repaint.
        assert_eq!(
            notifications_from_swaync(3, false),
            notifications_from_swaync(3, false)
        );
    }
}
