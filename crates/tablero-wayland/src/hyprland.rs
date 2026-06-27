//! Hyprland workspace source.
//!
//! Reads the workspace set from Hyprland's IPC sockets and emits typed
//! [`Msg::Workspaces`] snapshots through the [producer bridge](crate::producer),
//! so workspace state reaches the render loop the same way every other message
//! does — the rendering code never talks to Hyprland directly.
//!
//! Hyprland exposes two Unix sockets under `$XDG_RUNTIME_DIR/hypr/$SIGNATURE`:
//! `.socket.sock` answers one-shot JSON requests (`j/workspaces`,
//! `j/activeworkspace`), and `.socket2.sock` streams `EVENT>>DATA` lines. The
//! producer queries an initial snapshot, then re-queries whenever a
//! workspace-relevant event arrives. Normalization and de-duplication live in
//! [`Workspaces`], so emitting a snapshot that turns out unchanged is harmless —
//! the widget simply reports no visible change.

use std::env;
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};

use log::warn;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use tablero_core::widget::{Command, Msg, Workspaces};

use crate::command::CommandReceiver;
use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// One element of the `j/workspaces` array; only the id is needed.
#[derive(Deserialize)]
struct RawWorkspace {
    id: i32,
}

/// The `j/activeworkspace` object; only the id is needed.
#[derive(Deserialize)]
struct RawActiveWorkspace {
    id: i32,
}

/// Parse the JSON array returned by `j/workspaces` into its workspace ids.
pub fn parse_workspaces(json: &str) -> serde_json::Result<Vec<i32>> {
    let raw: Vec<RawWorkspace> = serde_json::from_str(json)?;
    Ok(raw.into_iter().map(|w| w.id).collect())
}

/// Parse the JSON object returned by `j/activeworkspace` into the active id.
pub fn parse_active(json: &str) -> serde_json::Result<i32> {
    let raw: RawActiveWorkspace = serde_json::from_str(json)?;
    Ok(raw.id)
}

/// Build a normalized [`Workspaces`] snapshot from the two raw IPC responses.
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

/// Query both workspace endpoints and fold them into a normalized snapshot.
async fn fetch_snapshot(dir: &Path) -> Result<Workspaces, Box<dyn Error + Send + Sync>> {
    let workspaces_json = query(dir, "j/workspaces").await?;
    let active_json = query(dir, "j/activeworkspace").await?;
    Ok(snapshot_from_json(&workspaces_json, &active_json)?)
}

/// A [`Producer`] that streams Hyprland workspace changes into the render loop.
///
/// Construct with [`new`](HyprlandProducer::new) and hand it to the producer
/// bridge; it queries an initial snapshot, then re-queries and emits on every
/// workspace-relevant compositor event until its sockets close or the render
/// loop shuts down.
pub struct HyprlandProducer;

impl HyprlandProducer {
    /// Create a Hyprland workspace producer.
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
        "hyprland-workspaces".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx))
    }
}

/// Drive the workspace stream: initial snapshot, then re-query on each event.
///
/// Returns `Ok(())` once the render loop has gone away (a [`send`] reports the
/// channel closed) or the event socket reaches EOF. Transient query failures are
/// logged and skipped rather than ending the stream.
///
/// [`send`]: MsgSender::send
async fn run(tx: MsgSender) -> ProducerResult {
    let dir = resolve_socket_dir()?;

    // Seed the bar with the current workspaces before the first event arrives.
    match fetch_snapshot(&dir).await {
        Ok(snapshot) => {
            if tx.send(Msg::Workspaces(snapshot)).is_err() {
                return Ok(());
            }
        }
        Err(e) => warn!("hyprland: initial workspace query failed: {e}"),
    }

    let events = UnixStream::connect(dir.join(".socket2.sock")).await?;
    let mut lines = BufReader::new(events).lines();
    while let Some(line) = lines.next_line().await? {
        if !is_workspace_event(&line) {
            continue;
        }
        match fetch_snapshot(&dir).await {
            Ok(snapshot) => {
                if tx.send(Msg::Workspaces(snapshot)).is_err() {
                    return Ok(());
                }
            }
            Err(e) => warn!("hyprland: workspace refresh failed: {e}"),
        }
    }

    Ok(())
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
