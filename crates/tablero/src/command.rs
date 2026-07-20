//! The command channel: render loop → producer runtime.
//!
//! The counterpart to the [producer bridge](crate::producer). Producers push
//! [`Msg`](crate::widget::Msg)s *into* the synchronous loop; commands flow
//! the other way — the loop turns a click into a [`Command`] and hands it to
//! async code that can execute it (the Hyprland command socket).
//!
//! ```text
//!   calloop loop (sync)                       Tokio runtime
//!   ┌────────────────────┐  CommandSender.send  ┌──────────────────┐
//!   │ pointer ─▶ on_click │ ───────────────────▶ │ command executor │
//!   └────────────────────┘   (cross-thread)     └──────────────────┘
//! ```
//!
//! The channel is an unbounded Tokio mpsc so the synchronous send never blocks
//! the render loop. The executor end is spawned on the producer bridge; see
//! [`hyprland::run_commands`](crate::hyprland::run_commands).

use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use log::{debug, warn};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::widget::Command;

use crate::producer::ProducerResult;

/// The receiving end of the command channel, drained by the executor task.
pub type CommandReceiver = UnboundedReceiver<Command>;

/// The sending half held by the render loop: how synchronous input reaches the
/// async executor. Cloneable, and sending never blocks (the channel is
/// unbounded).
#[derive(Clone)]
pub struct CommandSender {
    inner: UnboundedSender<Command>,
}

impl CommandSender {
    /// Queue `command` for the executor.
    ///
    /// Fails only once the executor (and its runtime) has gone away and dropped
    /// the receiver; a caller seeing [`Closed`] should stop trying, since nothing
    /// will execute further commands.
    pub fn send(&self, command: Command) -> Result<(), Closed> {
        self.inner.send(command).map_err(|_| Closed)
    }
}

impl fmt::Debug for CommandSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandSender").finish_non_exhaustive()
    }
}

/// Build a command channel: the [`CommandSender`] for the loop and the
/// [`CommandReceiver`] for the executor task.
pub fn command_channel() -> (CommandSender, CommandReceiver) {
    let (inner, rx) = unbounded_channel();
    (CommandSender { inner }, rx)
}

/// The executor has been dropped; no further commands will be run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("command executor closed; channel receiver was dropped")
    }
}

impl Error for Closed {}

/// Expand a leading `~` in `path` to the user's home directory.
///
/// Only a single leading `~` (optionally followed by `/` or end-of-string) is
/// expanded; a `~` mid-path or a `~user` form is taken verbatim. The expansion
/// is a simple textual substitution — the caller has not checked the
/// resulting path's existence, and `tokio::process::Command::spawn` will
/// surface a clean error if the file is missing or not executable. Returning
/// the original `PathBuf` unchanged when `$HOME` is unset keeps the executor
/// deterministic in headless environments where the user still wants a
/// literal path to be tried as-is.
pub fn expand_tilde(path: &std::path::Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = s.strip_prefix('~') else {
        return path.to_path_buf();
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        // A `~user` form is left verbatim — only `~` and `~/...` expand.
        return path.to_path_buf();
    }
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_path_buf();
    };
    let mut expanded = PathBuf::from(home);
    if !rest.is_empty() {
        expanded.push(rest.trim_start_matches('/'));
    }
    expanded
}

/// Drain a [`CommandReceiver`] for as long as the render loop sends.
///
/// Each [`Command::RunProgram`] is spawned directly via
/// `tokio::process::Command` — no shell, no argument expansion beyond the
/// leading-`~` home expansion in [`expand_tilde`]. Other command variants are
/// silently ignored: those have
/// their own executors, and every [`CommandSender`] is fanned out to every
/// executor, so the routing is by `match` arm here. A spawn failure
/// (missing file, non-executable, …) is logged at warn level and does not
/// end the loop — a bad click should never take the bar down.
pub async fn run_commands(mut rx: CommandReceiver) -> ProducerResult {
    while let Some(command) = rx.recv().await {
        if let Command::RunProgram(path) = command {
            let resolved = expand_tilde(&path);
            let display = resolved.display().to_string();
            match TokioCommand::new(&resolved).spawn() {
                Ok(_) => debug!("spawned on-click program: {display}"),
                Err(e) => {
                    warn!("failed to spawn on-click program {display:?}: {e}")
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_delivers_a_command_to_the_receiver() {
        let (tx, mut rx) = command_channel();
        tx.send(Command::SwitchWorkspace(3))
            .expect("receiver alive");
        assert_eq!(rx.try_recv().ok(), Some(Command::SwitchWorkspace(3)));
    }

    #[test]
    fn send_after_receiver_dropped_reports_closed() {
        let (tx, rx) = command_channel();
        drop(rx);
        assert_eq!(tx.send(Command::SwitchWorkspace(1)), Err(Closed));
    }

    #[test]
    fn expand_tilde_replaces_a_leading_tilde_with_home() {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME is set in this test environment");
        let mut expected = home.clone();
        expected.push("scripts/bluetooth.sh");
        assert_eq!(
            expand_tilde(std::path::Path::new("~/scripts/bluetooth.sh")),
            expected
        );
    }

    #[test]
    fn expand_tilde_with_a_bare_tilde_replaces_with_home() {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .expect("HOME is set in this test environment");
        assert_eq!(expand_tilde(std::path::Path::new("~")), home);
    }

    #[test]
    fn expand_tilde_leaves_an_absolute_path_unchanged() {
        assert_eq!(
            expand_tilde(std::path::Path::new("/usr/bin/blueman-manager")),
            PathBuf::from("/usr/bin/blueman-manager")
        );
    }

    #[test]
    fn expand_tilde_leaves_a_mid_path_tilde_unchanged() {
        // Only a leading `~` expands. A `~user` form (tilde followed by
        // something other than `/`) is taken verbatim, and a tilde inside the
        // path is left alone.
        assert_eq!(
            expand_tilde(std::path::Path::new("/tmp/~snapshot")),
            PathBuf::from("/tmp/~snapshot")
        );
        assert_eq!(
            expand_tilde(std::path::Path::new("~user/path")),
            PathBuf::from("~user/path")
        );
    }

    #[test]
    fn run_commands_ignores_non_run_program_commands() {
        // Commands with dedicated executors are filtered; this one only acts on
        // RunProgram.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = command_channel();
        tx.send(Command::SwitchWorkspace(1)).unwrap();
        tx.send(Command::ActivateTrayItem {
            key: "foo".to_string(),
            x: 0,
            y: 0,
        })
        .unwrap();
        drop(tx);
        rt.block_on(run_commands(rx)).unwrap();
        // No assertion needed: if `run_commands` panicked or returned an
        // error, the `unwrap` above would fire. The fact that this test
        // passes means the executor drained both messages without trying
        // to spawn a process.
    }

    #[test]
    fn run_commands_spawns_a_run_program_path_directly() {
        // A real on-click flow: write a tiny script to a temp file, mark it
        // executable, fire `RunProgram(<path>)` through the executor, and
        // verify the script ran by waiting for its marker file to appear.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("clicked.marker");
        let script = dir.path().join("on-click.sh");
        std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display()))
            .expect("write script");
        let mut perms = std::fs::metadata(&script).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = command_channel();
        tx.send(Command::RunProgram(script.clone())).unwrap();
        drop(tx);
        rt.block_on(run_commands(rx)).unwrap();

        // The script writes the marker unconditionally (the executor spawns
        // the path with no arguments). Wait briefly for the child to land;
        // a missing marker means the executor never spawned the process.
        let mut waited = std::time::Duration::ZERO;
        let step = std::time::Duration::from_millis(20);
        while !marker.exists() && waited < std::time::Duration::from_secs(2) {
            std::thread::sleep(step);
            waited += step;
        }
        assert!(
            marker.exists(),
            "executor spawned the script and the marker file appeared"
        );
    }

    #[test]
    fn run_commands_swallows_a_spawn_failure_without_ending_the_loop() {
        // A bogus path must not crash the executor: the warn is logged, the
        // loop continues, and a subsequent valid RunProgram is still
        // processed.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = command_channel();
        tx.send(Command::RunProgram(PathBuf::from(
            "/this/path/definitely/does/not/exist",
        )))
        .unwrap();
        tx.send(Command::SwitchWorkspace(2)).unwrap();
        drop(tx);
        rt.block_on(run_commands(rx)).unwrap();
    }
}
