//! The command channel: render loop → producer runtime.
//!
//! The counterpart to the [producer bridge](crate::producer). Producers push
//! [`Msg`](tablero_core::widget::Msg)s *into* the synchronous loop; commands flow
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

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use tablero_core::widget::Command;

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
}
