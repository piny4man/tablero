//! Hyprland IPC source for workspaces and the focused window's title.
//!
//! Reads the workspace set and the active window on the focused monitor from
//! Hyprland's IPC sockets, and emits typed [`Msg::Workspaces`] /
//! [`Msg::ActiveWindow`] snapshots through the [producer bridge](crate::producer),
//! so both reach the render loop the same way every other message does — the
//! rendering code never talks to Hyprland directly.
//!
//! Hyprland exposes two Unix sockets under `$XDG_RUNTIME_DIR/hypr/$SIGNATURE`:
//! `.socket.sock` answers one-shot JSON requests (`j/workspaces`,
//! `j/activeworkspace`, `j/monitors`, `j/activewindow`), and `.socket2.sock`
//! streams `EVENT>>DATA` lines. The producer queries an initial snapshot of
//! both, then on each coalesced burst of event lines dispatches to the right
//! endpoint:
//!
//! - **Workspace events** (`is_workspace_event`) drive a `j/workspaces` +
//!   `j/activeworkspace` + `j/monitors` refresh.
//!
//! - **Active-window events** ([`is_focus_or_lifecycle_event`]) —
//!   `activewindow>>`, `activewindowv2>>`, `focusedmon>>`, `openwindow>>`,
//!   `closewindow>>` — drive the per-monitor active-window tracking. The
//!   `activewindow`/`activewindowv2` payloads carry `class,title` inline and
//!   are parsed without an IPC round-trip; `focusedmon`, `openwindow`, and
//!   `closewindow` re-query `j/activewindow` because the payload alone does
//!   not describe the new state.
//!
//! Active-window messages are emitted *only* for the currently focused
//! monitor: each output's `TitleWidget` is bound to a connector name and
//! drops messages for any other monitor, so a focus change on monitor A
//! updates only monitor A's bar.

use std::env;
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use log::warn;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::widget::{ActiveWindow, Command, Msg, Workspaces};

use crate::command::CommandReceiver;
use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// One element of the `j/workspaces` array; the id and the monitor that owns it.
#[derive(Deserialize)]
struct RawWorkspace {
    id: i32,
    /// The connector name of the monitor this workspace lives on. Optional so a
    /// trimmed reply without it still parses (falling back to the global view).
    #[serde(default)]
    monitor: Option<String>,
}

/// The `j/activeworkspace` object; only the id is needed.
#[derive(Deserialize)]
struct RawActiveWorkspace {
    id: i32,
}

/// One element of the `j/monitors` array: a connector name and the workspace it
/// currently shows.
#[derive(Deserialize)]
struct RawMonitor {
    name: String,
    #[serde(rename = "activeWorkspace")]
    active_workspace: RawActiveWorkspace,
}

/// Parse the JSON array returned by `j/workspaces`, pairing each id with the
/// monitor that owns it (Hyprland always reports it; absent → `None`).
pub fn parse_workspace_entries(json: &str) -> serde_json::Result<Vec<(i32, Option<String>)>> {
    let raw: Vec<RawWorkspace> = serde_json::from_str(json)?;
    Ok(raw.into_iter().map(|w| (w.id, w.monitor)).collect())
}

/// Parse the JSON array returned by `j/workspaces` into its workspace ids,
/// discarding the per-monitor membership.
pub fn parse_workspaces(json: &str) -> serde_json::Result<Vec<i32>> {
    Ok(parse_workspace_entries(json)?
        .into_iter()
        .map(|(id, _)| id)
        .collect())
}

/// Parse the JSON object returned by `j/activeworkspace` into the active id.
pub fn parse_active(json: &str) -> serde_json::Result<i32> {
    let raw: RawActiveWorkspace = serde_json::from_str(json)?;
    Ok(raw.id)
}

/// Parse the JSON array returned by `j/monitors` into each monitor's active
/// workspace, as `(connector name, active workspace id)` pairs.
pub fn parse_monitors(json: &str) -> serde_json::Result<Vec<(String, i32)>> {
    let raw: Vec<RawMonitor> = serde_json::from_str(json)?;
    Ok(raw
        .into_iter()
        .map(|m| (m.name, m.active_workspace.id))
        .collect())
}

/// One element of `j/activewindow`: the focused window's class and title.
///
/// Optional fields so a `j/activewindow` reply that omits either (older
/// Hyprland, or a focused window without one of the atoms reported) still
/// parses into a usable snapshot — the [`ActiveWindow`] defaults the missing
/// field to the empty string.
#[derive(Deserialize)]
struct RawActiveWindow {
    #[serde(default)]
    class: String,
    #[serde(default)]
    title: String,
}

/// Parse the JSON object returned by `j/activewindow` into a normalized
/// [`ActiveWindow`] snapshot.
///
/// A bare `{}` (no focused window / empty desktop) parses to an empty
/// snapshot, which the [`TitleWidget`](crate::widget::TitleWidget) treats as "no
/// window" and reserves no slot.
pub fn parse_active_window(json: &str) -> serde_json::Result<ActiveWindow> {
    let raw: RawActiveWindow = serde_json::from_str(json)?;
    Ok(ActiveWindow::new(raw.class, raw.title))
}

/// Parse an `activewindow>>…` or `activewindowv2>>…` stream line into the
/// focused window's class and title.
///
/// Both event names carry the same `class,title` payload, so a single
/// parser covers v1 and v2. Returns `None` for any other event name or a
/// payload that does not match the `,`-separated two-field shape — callers
/// should only invoke this on lines already confirmed to be an
/// `activewindow`-family event.
pub fn parse_activewindow_stream(line: &str) -> Option<(String, String)> {
    let (name, rest) = line.split_once(">>")?;
    if name != "activewindow" && name != "activewindowv2" {
        return None;
    }
    let (class, title) = rest.split_once(',')?;
    Some((class.to_string(), title.to_string()))
}

/// Build a normalized [`Workspaces`] snapshot from the two raw IPC responses,
/// with no per-monitor membership (every widget sees the global set).
///
/// Pure over its inputs: the integration tests drive the full
/// parse → message → widget path through this without a live compositor.
pub fn snapshot_from_json(
    workspaces_json: &str,
    active_json: &str,
) -> serde_json::Result<Workspaces> {
    let ids = parse_workspaces(workspaces_json)?;
    let active = parse_active(active_json)?;
    Ok(Workspaces::new(ids, active))
}

/// Build a monitor-aware [`Workspaces`] snapshot from the three raw IPC
/// responses.
///
/// `j/workspaces` supplies each workspace's owning monitor, `j/monitors` each
/// monitor's active workspace, and `j/activeworkspace` the globally focused one
/// (used by an unscoped widget). Pure over its inputs, so the per-monitor path
/// is unit-testable without a live compositor.
pub fn snapshot_with_monitors(
    workspaces_json: &str,
    active_json: &str,
    monitors_json: &str,
) -> serde_json::Result<Workspaces> {
    let workspaces: Vec<(i32, String)> = parse_workspace_entries(workspaces_json)?
        .into_iter()
        .filter_map(|(id, monitor)| monitor.map(|m| (id, m)))
        .collect();
    let focused = parse_active(active_json)?;
    let actives = parse_monitors(monitors_json)?;
    Ok(Workspaces::with_monitors(workspaces, actives, focused))
}

/// Translate a typed [`Command`] into the Hyprland `.socket.sock` request that
/// carries it out, or `None` for a command this source does not handle.
///
/// Pure mapping, so the translation is unit-testable without a compositor. The
/// wildcard arm keeps a forward-compatible [`Command`] (it is `#[non_exhaustive]`)
/// from breaking the build — an unknown command is simply ignored.
///
/// The request targets Hyprland 0.55+'s Lua-config IPC: a `dispatch <expr>`
/// request is evaluated as `return hl.dispatch(<expr>)`, where `<expr>` must be a
/// dispatcher object built from the `hl.dsp.*` namespace. Workspace switching
/// uses `hl.dsp.focus({ workspace = N })` — the same dispatcher Hyprland's own
/// keybindings invoke. The legacy `dispatch workspace N` string form no longer
/// parses under the Lua config (it becomes invalid Lua).
pub fn dispatch_request(command: &Command) -> Option<String> {
    match command {
        Command::SwitchWorkspace(id) => {
            Some(format!("dispatch hl.dsp.focus({{ workspace = {id} }})"))
        }
        _ => None,
    }
}

/// Drain `commands` from the render loop and execute each over Hyprland IPC.
///
/// Runs on the producer bridge as the executor end of the
/// [command channel](crate::command). Resolves the socket directory once, then
/// dispatches each command to `.socket.sock`. A failed dispatch is logged and
/// skipped — one bad command never ends the stream. Returns `Ok(())` when the
/// render loop drops its [`CommandSender`](crate::command::CommandSender) and the
/// channel closes.
pub async fn run_commands(mut commands: CommandReceiver) -> ProducerResult {
    let dir = resolve_socket_dir()?;
    while let Some(command) = commands.recv().await {
        let Some(request) = dispatch_request(&command) else {
            continue;
        };
        if let Err(e) = query(&dir, &request).await {
            warn!("hyprland: command {request:?} failed: {e}");
        }
    }
    Ok(())
}

/// The directory holding Hyprland's IPC sockets for `signature` under `base`.
///
/// Pure path join (`{base}/hypr/{signature}`); [`resolve_socket_dir`] applies
/// it to the real environment and picks the location that exists.
fn socket_dir(base: &str, signature: &str) -> PathBuf {
    Path::new(base).join("hypr").join(signature)
}

/// True if an event line from `.socket2.sock` warrants a workspace refresh.
///
/// Matches on the event name (the part before `>>`): anything mentioning a
/// workspace, plus monitor focus and special-workspace toggles, all of which can
/// change the visible set or the active workspace.
fn is_workspace_event(line: &str) -> bool {
    let name = line.split(">>").next().unwrap_or("");
    name.contains("workspace") || name == "focusedmon" || name == "activespecial"
}

/// True if an event name from `.socket2.sock` is one we handle for
/// active-window tracking.
///
/// Covers the dedicated focus events (`activewindow>>`, `activewindowv2>>`),
/// the window-lifecycle events (`openwindow>>`, `closewindow>>`), and the
/// monitor-focus event (`focusedmon>>`, since the active window can change
/// with the focused monitor without an `activewindow>>` immediately
/// following).
///
/// The lifecycle events are needed because Hyprland does not always re-emit
/// `activewindow>>` when the focused window closes — listening to the
/// lifecycle events directly ensures the title widget catches the transition.
pub fn is_focus_or_lifecycle_event(name: &str) -> bool {
    matches!(
        name,
        "activewindow" | "activewindowv2" | "openwindow" | "closewindow" | "focusedmon"
    )
}

/// Parse a `focusedmon>>monitor,workspace_id` event line into the focused
/// monitor's connector name.
///
/// `focusedmon>>DP-1,2` → `Some("DP-1".to_string())`. Returns `None` for
/// events that are not `focusedmon>>` or for payloads that do not have the
/// `,`-separated `monitor,workspace` shape — callers should gate on the
/// event name first, but the parser also refuses unrelated events as a
/// defense in depth.
pub fn parse_focusedmon(line: &str) -> Option<String> {
    let (name, rest) = line.split_once(">>")?;
    if name != "focusedmon" {
        return None;
    }
    let (monitor, _ws_id) = rest.split_once(',')?;
    Some(monitor.to_string())
}

/// Locate Hyprland's socket directory from the environment.
///
/// Prefers `$XDG_RUNTIME_DIR/hypr/$SIGNATURE` (current Hyprland) and falls back
/// to the legacy `/tmp/hypr/$SIGNATURE`. Fails when not running under Hyprland.
fn resolve_socket_dir() -> Result<PathBuf, Box<dyn Error + Send + Sync>> {
    let signature = env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .map_err(|_| "HYPRLAND_INSTANCE_SIGNATURE is unset; not running under Hyprland")?;

    if let Ok(runtime) = env::var("XDG_RUNTIME_DIR") {
        let dir = socket_dir(&runtime, &signature);
        if dir.exists() {
            return Ok(dir);
        }
    }

    let legacy = socket_dir("/tmp", &signature);
    if legacy.exists() {
        return Ok(legacy);
    }

    Err(format!("no Hyprland socket directory found for signature {signature}").into())
}

/// Send a one-shot request to `.socket.sock` and return the full response.
async fn query(dir: &Path, request: &str) -> io::Result<String> {
    let mut stream = UnixStream::connect(dir.join(".socket.sock")).await?;
    stream.write_all(request.as_bytes()).await?;
    let mut response = String::new();
    stream.read_to_string(&mut response).await?;
    Ok(response)
}

/// Query the workspace, active-workspace and monitor endpoints and fold them
/// into a normalized, monitor-aware snapshot.
async fn fetch_snapshot(dir: &Path) -> Result<Workspaces, Box<dyn Error + Send + Sync>> {
    let workspaces_json = query(dir, "j/workspaces").await?;
    let active_json = query(dir, "j/activeworkspace").await?;
    let monitors_json = query(dir, "j/monitors").await?;
    Ok(snapshot_with_monitors(
        &workspaces_json,
        &active_json,
        &monitors_json,
    )?)
}

/// Query `j/activewindow` and return a normalized [`ActiveWindow`] snapshot.
///
/// Hyprland returns `{}` when no window is focused (empty desktop, or a
/// compositor state where the global focus has no addressable window); the
/// parser folds that into an empty snapshot, which the
/// [`TitleWidget`](crate::TitleWidget) treats as "no window" and reserves no
/// slot for.
async fn fetch_activewindow(dir: &Path) -> Result<ActiveWindow, Box<dyn Error + Send + Sync>> {
    let json = query(dir, "j/activewindow").await?;
    Ok(parse_active_window(&json)?)
}

/// Resolve the focused monitor name *and* the active window in one startup
/// pass.
///
/// Hyprland exposes the globally active window only via `j/activewindow`
/// and the globally active workspace only via `j/activeworkspace`. The
/// focused monitor is the one whose `j/monitors` entry has the same
/// `activeWorkspace.id` as the global active workspace — that's the
/// monitor the active window lives on. Combining the three gives us a
/// `(focused_monitor, active_window)` pair to seed the per-monitor
/// tracking without waiting for the first `focusedmon>>` event.
async fn fetch_initial_focused_activewindow(
    dir: &Path,
) -> Result<(String, ActiveWindow), Box<dyn Error + Send + Sync>> {
    let active_ws_json = query(dir, "j/activeworkspace").await?;
    let active_ws_id = parse_active(&active_ws_json)?;
    let monitors_json = query(dir, "j/monitors").await?;
    let monitors = parse_monitors(&monitors_json)?;
    let focused = monitors
        .iter()
        .find(|(_, ws)| *ws == active_ws_id)
        .map(|(name, _)| name.clone())
        .ok_or("no monitor matches the globally active workspace")?;
    let active_json = query(dir, "j/activewindow").await?;
    let window = parse_active_window(&active_json)?;
    Ok((focused, window))
}

/// A [`Producer`] that streams Hyprland workspace and focus changes into the
/// render loop.
///
/// Construct with [`new`](HyprlandProducer::new) and hand it to the producer
/// bridge; it queries an initial snapshot of both workspaces and the
/// per-monitor active window, then on each event stream line dispatches to
/// the right endpoint and emits a typed [`Msg`] addressed to the affected
/// monitor — until its sockets close or the render loop shuts down.
pub struct HyprlandProducer;

impl HyprlandProducer {
    /// Create a Hyprland IPC producer.
    ///
    /// The same single producer drives both the workspace stream and the
    /// active-window stream, sharing one connection to `.socket2.sock` and
    /// dispatching each incoming event to the right one-shot query.
    pub fn new() -> Self {
        Self
    }
}

impl Default for HyprlandProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for HyprlandProducer {
    fn name(&self) -> String {
        "hyprland".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx))
    }
}

/// How long to wait for more socket2 lines before querying. A workspace switch
/// emits several events within a few milliseconds; one snapshot covers the burst.
const EVENT_COALESCE: Duration = Duration::from_millis(8);

/// Pause before opening the event socket again after it drops.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// What one burst of socket2 lines asks the producer to do.
struct EventWork {
    /// Several workspace lines still need only one `j/workspaces` refresh.
    refresh_workspaces: bool,
    /// Focus and lifecycle lines, in arrival order, applied after that refresh.
    focus_lines: Vec<String>,
}

/// Collapse a burst of event lines into one workspace refresh plus the focus
/// lines that still have to be applied.
fn event_work<'a>(lines: impl IntoIterator<Item = &'a str>) -> EventWork {
    let mut refresh_workspaces = false;
    let mut focus_lines = Vec::new();
    for line in lines {
        if is_workspace_event(line) {
            refresh_workspaces = true;
        }
        let name = line.split(">>").next().unwrap_or("");
        if is_focus_or_lifecycle_event(name) {
            focus_lines.push(line.to_string());
        }
    }
    EventWork {
        refresh_workspaces,
        focus_lines,
    }
}

/// A closed render-loop channel stops the producer. Any other session failure
/// reconnects, so a dropped event socket cannot freeze the workspace strip.
fn reconnect_after(channel_closed: bool) -> bool {
    !channel_closed
}

/// Why one Hyprland session ended.
enum SessionEnd {
    /// The render loop dropped the message channel. Do not reconnect.
    Shutdown,
    /// The event socket closed or could not be opened. Reconnect.
    Disconnected(String),
}

impl SessionEnd {
    fn channel_closed(&self) -> bool {
        matches!(self, Self::Shutdown)
    }

    fn reason(&self) -> &str {
        match self {
            Self::Shutdown => "render loop closed",
            Self::Disconnected(reason) => reason,
        }
    }
}

/// Keep the Hyprland event stream alive.
///
/// One session seeds the current snapshot, then reads `.socket2.sock`. Lines
/// that arrive within [`EVENT_COALESCE`] share one workspace query. A dropped
/// socket or a failed connect is logged and the session starts again, so the
/// workspace strip cannot freeze while the rest of the bar keeps running.
/// Returns `Ok(())` only when the render loop has closed the channel.
///
/// [`send`]: MsgSender::send
async fn run(tx: MsgSender) -> ProducerResult {
    loop {
        let end = session(&tx).await;
        if !reconnect_after(end.channel_closed()) {
            return Ok(());
        }
        warn!("hyprland: {}; reconnecting", end.reason());
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

/// One connection to the event socket: seed, then dispatch coalesced bursts
/// until the channel closes or the socket drops.
async fn session(tx: &MsgSender) -> SessionEnd {
    let dir = match resolve_socket_dir() {
        Ok(dir) => dir,
        Err(error) => return SessionEnd::Disconnected(error.to_string()),
    };

    if let Ok(snapshot) = fetch_snapshot(&dir).await {
        if tx.send(Msg::Workspaces(snapshot)).is_err() {
            return SessionEnd::Shutdown;
        }
    } else {
        warn!("hyprland: initial workspace query failed");
    }

    let mut last_focused_monitor: Option<String> = None;
    if let Ok((monitor, window)) = fetch_initial_focused_activewindow(&dir).await {
        last_focused_monitor = Some(monitor.clone());
        let window = (!window.is_empty()).then_some(window);
        if tx.send(Msg::ActiveWindow { monitor, window }).is_err() {
            return SessionEnd::Shutdown;
        }
    } else {
        warn!("hyprland: initial activewindow query failed");
    }

    let events = match UnixStream::connect(dir.join(".socket2.sock")).await {
        Ok(events) => events,
        Err(error) => return SessionEnd::Disconnected(error.to_string()),
    };
    let mut lines = BufReader::new(events).lines();
    loop {
        let batch = match read_batch(&mut lines).await {
            Ok(Some(batch)) => batch,
            Ok(None) => return SessionEnd::Disconnected("event socket closed".to_string()),
            Err(error) => return SessionEnd::Disconnected(error.to_string()),
        };
        let work = event_work(batch.iter().map(String::as_str));
        if work.refresh_workspaces {
            match fetch_snapshot(&dir).await {
                Ok(snapshot) => {
                    if tx.send(Msg::Workspaces(snapshot)).is_err() {
                        return SessionEnd::Shutdown;
                    }
                }
                Err(error) => warn!("hyprland: workspace refresh failed: {error}"),
            }
        }
        for line in &work.focus_lines {
            if emit_focus_line(&dir, tx, &mut last_focused_monitor, line).await {
                return SessionEnd::Shutdown;
            }
        }
    }
}

/// Read one event line, then any further lines that arrive before
/// [`EVENT_COALESCE`] elapses, so a switch's burst shares one snapshot.
async fn read_batch<R>(lines: &mut tokio::io::Lines<R>) -> io::Result<Option<Vec<String>>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let Some(first) = lines.next_line().await? else {
        return Ok(None);
    };
    let mut batch = vec![first];
    let deadline = tokio::time::Instant::now() + EVENT_COALESCE;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, lines.next_line()).await {
            Ok(Ok(Some(line))) => batch.push(line),
            Ok(Ok(None)) | Err(_) => break,
            Ok(Err(error)) => return Err(error),
        }
    }
    Ok(Some(batch))
}

/// Apply one focus or lifecycle line. Returns whether the render loop has
/// closed the channel.
async fn emit_focus_line(
    dir: &Path,
    tx: &MsgSender,
    last_focused_monitor: &mut Option<String>,
    line: &str,
) -> bool {
    let name = line.split(">>").next().unwrap_or("");
    match name {
        "focusedmon" => {
            let Some(mon) = parse_focusedmon(line) else {
                return false;
            };
            *last_focused_monitor = Some(mon.clone());
            match fetch_activewindow(dir).await {
                Ok(window) => {
                    let window = (!window.is_empty()).then_some(window);
                    tx.send(Msg::ActiveWindow {
                        monitor: mon,
                        window,
                    })
                    .is_err()
                }
                Err(error) => {
                    warn!("hyprland: focusedmon activewindow refresh failed: {error}");
                    false
                }
            }
        }
        "activewindow" | "activewindowv2" => {
            let Some(monitor) = last_focused_monitor.clone() else {
                return false;
            };
            let Some((class, title)) = parse_activewindow_stream(line) else {
                return false;
            };
            tx.send(Msg::ActiveWindow {
                monitor,
                window: Some(ActiveWindow::new(class, title)),
            })
            .is_err()
        }
        "openwindow" | "closewindow" => {
            let Some(monitor) = last_focused_monitor.clone() else {
                return false;
            };
            match fetch_activewindow(dir).await {
                Ok(window) => {
                    let window = (!window.is_empty()).then_some(window);
                    tx.send(Msg::ActiveWindow { monitor, window }).is_err()
                }
                Err(error) => {
                    warn!("hyprland: openwindow/closewindow refresh failed: {error}");
                    false
                }
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed but realistically-shaped `j/workspaces` reply: extra fields the
    // parser must ignore, ids out of order.
    const WORKSPACES_JSON: &str = r#"[
        {"id": 3, "name": "3", "monitor": "DP-1", "windows": 2},
        {"id": 1, "name": "1", "monitor": "DP-1", "windows": 5},
        {"id": 2, "name": "2", "monitor": "DP-1", "windows": 0}
    ]"#;
    const ACTIVE_JSON: &str = r#"{"id": 2, "name": "2", "monitor": "DP-1"}"#;

    #[test]
    fn parse_workspaces_extracts_ids_ignoring_other_fields() {
        let ids = parse_workspaces(WORKSPACES_JSON).expect("valid json");
        assert_eq!(ids, vec![3, 1, 2]);
    }

    #[test]
    fn parse_active_extracts_the_active_id() {
        assert_eq!(parse_active(ACTIVE_JSON).expect("valid json"), 2);
    }

    #[test]
    fn snapshot_from_json_normalizes_into_a_workspaces() {
        let snapshot = snapshot_from_json(WORKSPACES_JSON, ACTIVE_JSON).expect("valid json");
        assert_eq!(snapshot.ids(), &[1, 2, 3]);
        assert_eq!(snapshot.active(), 2);
        assert_eq!(snapshot.label(), "1 [2] 3");
    }

    // A two-monitor setup: DP-1 owns 1,2 (active 2); HDMI-A-1 owns 5,6 (active 5).
    const MULTI_WORKSPACES_JSON: &str = r#"[
        {"id": 2, "name": "2", "monitor": "DP-1", "windows": 1},
        {"id": 6, "name": "6", "monitor": "HDMI-A-1", "windows": 0},
        {"id": 1, "name": "1", "monitor": "DP-1", "windows": 4},
        {"id": 5, "name": "5", "monitor": "HDMI-A-1", "windows": 2}
    ]"#;
    const MONITORS_JSON: &str = r#"[
        {"id": 0, "name": "DP-1", "activeWorkspace": {"id": 2, "name": "2"}},
        {"id": 1, "name": "HDMI-A-1", "activeWorkspace": {"id": 5, "name": "5"}}
    ]"#;

    #[test]
    fn parse_workspace_entries_pairs_each_id_with_its_monitor() {
        let entries = parse_workspace_entries(MULTI_WORKSPACES_JSON).expect("valid json");
        assert_eq!(
            entries,
            vec![
                (2, Some("DP-1".to_string())),
                (6, Some("HDMI-A-1".to_string())),
                (1, Some("DP-1".to_string())),
                (5, Some("HDMI-A-1".to_string())),
            ]
        );
    }

    #[test]
    fn parse_monitors_extracts_each_monitors_active_workspace() {
        let monitors = parse_monitors(MONITORS_JSON).expect("valid json");
        assert_eq!(
            monitors,
            vec![("DP-1".to_string(), 2), ("HDMI-A-1".to_string(), 5)]
        );
    }

    #[test]
    fn snapshot_with_monitors_scopes_each_monitors_workspaces() {
        let snapshot = snapshot_with_monitors(MULTI_WORKSPACES_JSON, ACTIVE_JSON, MONITORS_JSON)
            .expect("valid json");
        // Each monitor sees only its own workspaces, with its own active.
        assert_eq!(snapshot.ids_for("DP-1"), vec![1, 2]);
        assert_eq!(snapshot.active_for("DP-1"), Some(2));
        assert_eq!(snapshot.ids_for("HDMI-A-1"), vec![5, 6]);
        assert_eq!(snapshot.active_for("HDMI-A-1"), Some(5));
        // The global view spans both, with the focused active from j/activeworkspace.
        assert_eq!(snapshot.ids(), &[1, 2, 5, 6]);
        assert_eq!(snapshot.active(), 2);
    }

    #[test]
    fn parse_workspaces_rejects_malformed_json() {
        assert!(parse_workspaces("not json").is_err());
    }

    #[test]
    fn socket_dir_joins_base_hypr_and_signature() {
        assert_eq!(
            socket_dir("/run/user/1000", "abc123"),
            PathBuf::from("/run/user/1000/hypr/abc123")
        );
    }

    #[test]
    fn workspace_events_are_recognized() {
        assert!(is_workspace_event("workspace>>2"));
        assert!(is_workspace_event("workspacev2>>2,name"));
        assert!(is_workspace_event("createworkspacev2>>5,5"));
        assert!(is_workspace_event("destroyworkspace>>3"));
        assert!(is_workspace_event("moveworkspace>>2,DP-1"));
        assert!(is_workspace_event("focusedmon>>DP-1,2"));
        assert!(is_workspace_event("activespecial>>scratch,DP-1"));
    }

    #[test]
    fn non_workspace_events_are_ignored() {
        assert!(!is_workspace_event("activewindow>>class,title"));
        assert!(!is_workspace_event("openwindow>>0x55,2,class,title"));
        assert!(!is_workspace_event("submap>>resize"));
    }

    // A realistically-shaped `j/activewindow` reply: includes extras the
    // parser must ignore, plus a class-only variant and the modern form
    // with both class and title.
    const ACTIVEWINDOW_JSON: &str =
        r#"{"address": "0x55", "class": "firefox", "title": "GitHub", "workspace": {"id": 2}}"#;
    const ACTIVEWINDOW_CLASS_ONLY: &str = r#"{"address": "0x55", "class": "kitty"}"#;

    #[test]
    fn parse_active_window_extracts_class_and_title_ignoring_extras() {
        let window = parse_active_window(ACTIVEWINDOW_JSON).expect("valid json");
        assert_eq!(window.class(), "firefox");
        assert_eq!(window.title(), "GitHub");
    }

    #[test]
    fn parse_active_window_tolerates_a_missing_title() {
        let window = parse_active_window(ACTIVEWINDOW_CLASS_ONLY).expect("valid json");
        assert_eq!(window.class(), "kitty");
        assert_eq!(window.title(), "");
    }

    #[test]
    fn parse_active_window_folds_empty_object_to_empty_snapshot() {
        let window = parse_active_window("{}").expect("valid json");
        assert!(window.is_empty());
    }

    #[test]
    fn parse_active_window_rejects_malformed_json() {
        assert!(parse_active_window("not json").is_err());
    }

    #[test]
    fn parse_activewindow_stream_handles_both_v1_and_v2_event_names() {
        assert_eq!(
            parse_activewindow_stream("activewindow>>firefox,GitHub"),
            Some(("firefox".to_string(), "GitHub".to_string()))
        );
        assert_eq!(
            parse_activewindow_stream("activewindowv2>>kitty,vim"),
            Some(("kitty".to_string(), "vim".to_string()))
        );
    }

    #[test]
    fn parse_activewindow_stream_returns_none_on_unrelated_events() {
        // Defensive: callers gate on the event name, but the parser itself
        // must still refuse non-activewindow payloads.
        assert!(parse_activewindow_stream("workspace>>2").is_none());
        assert!(parse_activewindow_stream("activewindow>>").is_none());
        assert!(parse_activewindow_stream("not an event line").is_none());
    }

    #[test]
    fn parse_focusedmon_extracts_the_monitor_name() {
        assert_eq!(
            parse_focusedmon("focusedmon>>DP-1,2"),
            Some("DP-1".to_string())
        );
        assert_eq!(
            parse_focusedmon("focusedmon>>HDMI-A-1,5"),
            Some("HDMI-A-1".to_string())
        );
        assert!(parse_focusedmon("focusedmon>>").is_none());
        // The parser refuses non-focusedmon payloads even if they have the
        // `,`-separated shape — callers gate on the event name, but
        // defense in depth catches misuse.
        assert!(parse_focusedmon("activewindow>>firefox,GitHub").is_none());
        assert!(parse_focusedmon("not an event line").is_none());
    }

    #[test]
    fn focus_or_lifecycle_events_are_recognized() {
        assert!(is_focus_or_lifecycle_event("activewindow"));
        assert!(is_focus_or_lifecycle_event("activewindowv2"));
        assert!(is_focus_or_lifecycle_event("openwindow"));
        assert!(is_focus_or_lifecycle_event("closewindow"));
        assert!(is_focus_or_lifecycle_event("focusedmon"));
    }

    #[test]
    fn non_focus_events_are_ignored_for_active_window_tracking() {
        // Workspace and config events stay on the workspace / no-op path.
        assert!(!is_focus_or_lifecycle_event("workspace"));
        assert!(!is_focus_or_lifecycle_event("workspacev2"));
        assert!(!is_focus_or_lifecycle_event("submap"));
    }

    #[test]
    fn a_burst_of_workspace_lines_is_one_snapshot_fetch() {
        let lines = [
            "workspace>>2",
            "workspacev2>>2,2",
            "createworkspace>>4",
            "destroyworkspace>>3",
            "focusedmon>>DP-1,2",
        ];
        let work = event_work(lines);
        assert_eq!(usize::from(work.refresh_workspaces), 1);
        assert_eq!(
            work.focus_lines,
            vec!["focusedmon>>DP-1,2".to_string()],
            "coalescing must not drop the focus line that arrived with the burst"
        );
    }

    #[test]
    fn a_burst_without_a_workspace_event_fetches_no_snapshot() {
        let work = event_work(["activewindow>>class,title", "submap>>resize"]);
        assert!(!work.refresh_workspaces);
        assert_eq!(
            work.focus_lines,
            vec!["activewindow>>class,title".to_string()]
        );
    }

    #[test]
    fn a_dropped_event_socket_reconnects_but_a_closed_channel_stops() {
        assert!(reconnect_after(false));
        assert!(!reconnect_after(true));
    }

    #[test]
    fn switch_workspace_maps_to_a_dispatch_request() {
        // Hyprland 0.55+ evaluates `dispatch <expr>` as `return hl.dispatch(<expr>)`,
        // so the payload must be the `hl.dsp.focus` dispatcher object, not the
        // legacy `workspace N` string form.
        assert_eq!(
            dispatch_request(&Command::SwitchWorkspace(4)).as_deref(),
            Some("dispatch hl.dsp.focus({ workspace = 4 })")
        );
    }
}
