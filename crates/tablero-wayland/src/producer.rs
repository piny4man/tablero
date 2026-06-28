//! The async producer bridge.
//!
//! The Wayland/render loop is strictly synchronous: it owns the app state,
//! renders, and commits buffers. Anything that needs to run asynchronously —
//! Hyprland IPC, DBus, polling external state — runs on a [Tokio] runtime,
//! entirely off the render thread, and reaches the loop only by sending typed
//! [`Msg`]s through a [`calloop`] channel.
//!
//! ```text
//!   Tokio runtime                         calloop loop (sync)
//!   ┌───────────────┐   MsgSender.send    ┌────────────────────┐
//!   │ producer task │ ──────────────────▶ │ Channel ─▶ Bar::handle │
//!   └───────────────┘   (cross-thread)    └────────────────────┘
//! ```
//!
//! A producer is any [`Producer`] (or async closure via [`from_fn`]); the
//! [`ProducerBridge`] owns the runtime and the channel sender and spawns
//! producers onto it. Producer failures are logged, not propagated, so one
//! misbehaving source can never take down the render loop. New producer kinds
//! plug in without changing the loop contract: the loop only ever sees `Msg`s
//! arriving on the channel.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;

use calloop::channel::{Channel, Sender, channel};
use log::{debug, error};
use tokio::runtime::{Builder, Runtime};

use tablero_core::widget::Msg;

/// The result a producer reports when it finishes (or fails).
pub type ProducerResult = Result<(), Box<dyn Error + Send + Sync>>;

/// A boxed, sendable future a producer runs to completion.
pub type ProducerFuture = Pin<Box<dyn Future<Output = ProducerResult> + Send>>;

/// The sender half handed to producers: the only way async code can reach the
/// render loop. Cloneable, so every producer gets its own handle.
#[derive(Clone)]
pub struct MsgSender {
    inner: Sender<Msg>,
}

impl MsgSender {
    /// Send a message into the render loop.
    ///
    /// Fails only once the loop has shut down and dropped its receiver; a
    /// producer seeing [`Closed`] should stop, since nothing will read further
    /// messages.
    pub fn send(&self, msg: Msg) -> Result<(), Closed> {
        self.inner.send(msg).map_err(|_| Closed)
    }
}

impl fmt::Debug for MsgSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MsgSender").finish_non_exhaustive()
    }
}

/// The render loop's receiver has been dropped; no further messages will be
/// delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("render loop closed; message channel receiver was dropped")
    }
}

impl Error for Closed {}

/// An async source of [`Msg`]s that runs on the producer runtime.
///
/// Implement this for long-lived, named sources (a Hyprland IPC listener, a
/// DBus signal subscriber, a poller). For one-off or test producers, prefer
/// [`from_fn`].
pub trait Producer: Send + 'static {
    /// A short name used in log lines.
    fn name(&self) -> String;

    /// Run the producer to completion, emitting messages through `tx`.
    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture;
}

/// Wrap an async closure as a [`Producer`].
///
/// ```no_run
/// use tablero_wayland::producer::{from_fn, MsgSender};
/// use tablero_core::widget::Msg;
///
/// let p = from_fn("clock", |tx: MsgSender| async move {
///     tx.send(Msg::tick_now())?;
///     Ok(())
/// });
/// ```
pub fn from_fn<F, Fut>(name: impl Into<String>, f: F) -> Box<dyn Producer>
where
    F: FnOnce(MsgSender) -> Fut + Send + 'static,
    Fut: Future<Output = ProducerResult> + Send + 'static,
{
    struct FnProducer<F> {
        name: String,
        f: F,
    }

    impl<F, Fut> Producer for FnProducer<F>
    where
        F: FnOnce(MsgSender) -> Fut + Send + 'static,
        Fut: Future<Output = ProducerResult> + Send + 'static,
    {
        fn name(&self) -> String {
            self.name.clone()
        }

        fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
            Box::pin((self.f)(tx))
        }
    }

    Box::new(FnProducer {
        name: name.into(),
        f,
    })
}

/// Owns the Tokio runtime that drives producers and the sender feeding the
/// render loop.
///
/// Constructing a bridge starts a runtime with **no** Wayland involvement (see
/// [`ProducerBridge::new`]); the loop registers the returned [`Channel`] and
/// later calls [`spawn`](ProducerBridge::spawn) for each producer. The bridge
/// must outlive the render loop — dropping it shuts the runtime down and ends
/// every producer.
pub struct ProducerBridge {
    runtime: Runtime,
    sender: Sender<Msg>,
}

impl ProducerBridge {
    /// Build the producer runtime and the channel into the render loop.
    ///
    /// Returns the bridge and the [`Channel`] receiver to register with the
    /// calloop event loop. A single worker thread drives every producer future
    /// concurrently — they are I/O-bound and light, so one thread is plenty —
    /// and it touches no Wayland state, so the bridge is safe to create before
    /// or independently of the Wayland connection.
    pub fn new() -> io::Result<(Self, Channel<Msg>)> {
        // A multi-thread runtime with one worker. Producers are spawned and
        // detached — nothing calls `block_on` to drive them — so the runtime
        // needs its own thread; `new_current_thread()` would never run them.
        // One worker suffices for a handful of async, I/O-bound producers.
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("tablero-producer")
            .enable_all()
            .build()?;
        let (sender, channel) = channel();
        Ok((Self { runtime, sender }, channel))
    }

    /// A fresh sender handle for code that wants to emit messages directly.
    pub fn sender(&self) -> MsgSender {
        MsgSender {
            inner: self.sender.clone(),
        }
    }

    /// Spawn `producer` onto the runtime.
    ///
    /// The producer's future is wrapped so a returned error is logged rather
    /// than propagated: a failing producer never panics the runtime or stalls
    /// the render loop.
    pub fn spawn(&self, producer: Box<dyn Producer>) {
        let tx = self.sender();
        let name = producer.name();
        self.runtime.spawn(async move {
            match producer.run(tx).await {
                Ok(()) => debug!("producer {name} finished"),
                Err(e) => error!("producer {name} failed: {e}"),
            }
        });
    }

    /// Spawn a bare named future onto the runtime.
    ///
    /// Like [`spawn`](Self::spawn) but for a task that is not a [`Producer`] —
    /// for example the command executor that drains the [command
    /// channel](crate::command) and acts on the compositor. Errors are logged,
    /// never propagated, with the same isolation guarantee as producers.
    pub fn spawn_task<F>(&self, name: impl Into<String>, future: F)
    where
        F: Future<Output = ProducerResult> + Send + 'static,
    {
        let name = name.into();
        self.runtime.spawn(async move {
            match future.await {
                Ok(()) => debug!("task {name} finished"),
                Err(e) => error!("task {name} failed: {e}"),
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_after_receiver_dropped_reports_closed() {
        // Building the bridge starts a runtime with no Wayland objects at all.
        let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
        let tx = bridge.sender();
        drop(channel); // the render loop went away
        assert_eq!(tx.send(Msg::tick_now()), Err(Closed));
    }

    #[test]
    fn from_fn_carries_its_name() {
        let producer = from_fn("hyprland", |_tx| async { Ok(()) });
        assert_eq!(producer.name(), "hyprland");
    }
}
