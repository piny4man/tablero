//! PipeWire native volume source.
//!
//! Reads the active output sink's level and mute state over the PipeWire
//! native wire protocol and emits typed [`Msg::Volume`] snapshots through the
//! [producer bridge](crate::producer), so the volume widget reaches the render
//! loop the same way every other message does — the rendering code never
//! talks to PipeWire directly.
//!
//! # Why a dedicated thread
//!
//! PipeWire's [`MainLoop`](pipewire::main_loop::MainLoop) is synchronous and
//! file-descriptor-driven, so it cannot live on the Tokio runtime the way the
//! zbus-based producers (bluetooth, …) do. The producer's [`run`](Producer::run)
//! future spawns an OS thread that owns the PipeWire main loop, and the
//! Tokio future simply `await`s the thread's completion via a
//! `tokio::sync::oneshot` — the bridge's [`MsgSender`] is moved into the
//! thread, so the loop reaches the render loop exactly the way every other
//! producer does. Errors propagate through the oneshot the same way the
//! bluetooth producer's `?` does.
//!
//! # Surviving a PipeWire server that is not there yet
//!
//! The bar is commonly started by the compositor's autostart *before* the
//! user's PipeWire daemon, and `pw_context_connect` against a socket-activated
//! `pipewire-0` **succeeds** in that window — the connection is accepted at the
//! socket level but the client handshake never completes. The server then
//! delivers no registry global and no error, so a single-shot main loop parks
//! in `epoll_wait` on a dead connection for the rest of the session and the
//! widget stays invisible, silently, forever.
//!
//! A connection is therefore only trusted once the core answers a
//! [`sync`](pipewire::core::Core::sync) with a `done` event. One is issued
//! immediately after connecting and a watchdog timer re-issues it every
//! `HEARTBEAT_INTERVAL`; an unanswered sync quits the main loop, and the
//! session supervisor reconnects on a backoff that starts at `RETRY_DELAY` and
//! caps at `MAX_RETRY_DELAY`. The same check recovers a daemon that dies
//! mid-session, and the backoff keeps a host with no PipeWire at all down to
//! one quiet attempt every 30 seconds.
//!
//! Reproducing the never-handshaked case without touching the session's audio:
//! bind a unix socket under `$XDG_RUNTIME_DIR` that accepts connections and
//! never answers, then point the producer at it with
//! `PIPEWIRE_REMOTE=<socket-name> cargo run --example volume-bridge -- 20`.
//! Sending it at a socket that does not exist yet, and creating it a few
//! seconds in, reproduces the boot race the same way.
//!
//! # Active-sink selection
//!
//! PipeWire may expose many sinks (multiple wired/wireless adapters, HDMI
//! outputs, USB headphones, virtual sinks). The bar shows a single level and
//! mute flag, so the readings are aggregated onto one *active* sink. The
//! heuristic, in order:
//!
//! 1. Pick a sink whose [`NodeState`] is [`Running`](NodeState::Running) — a
//!    sink is "active" the moment any of its streams is playing back. The
//!    selected id, when multiple are running, is the one with the
//!    alphabetically first node name (deterministic, no surprises; this is
//!    the rule the user picked).
//! 2. If no sink is running, fall back to the sink whose name matches the
//!    configured default audio sink in the `default.audio.sink` metadata —
//!    i.e. what the user has actually told the system to use.
//! 3. If neither resolves, fall back to the first sink by name, so a reachable
//!    audio server always shows a reading. Only a server with no sinks at all
//!    emits `Msg::Volume(None)`, which reserves no slot for the volume widget —
//!    matching the absent-source behavior of the `network` and `system`
//!    widgets.
//!
//! The volume can change at any time (key press, GUI slider, app that
//! auto-balances audio), so every sink `info` / `param` event and every
//! `default.audio.sink` change re-evaluates the snapshot immediately. A
//! 2-second ticker re-reads the same cached state as a safety net for anything
//! the server did not push. Either way a snapshot is only sent when the
//! normalized value actually differs, plus one forced re-emit every
//! `RESYNC_TICKS` ticks in case a message never reached the render loop.
//!
//! # Pure parsing
//!
//! Device-kind classification and active-sink selection are pure functions
//! tests drive directly with strings and slices — no PipeWire server needed.
//! The SPA POD decoding for the volume/mute Props is exercised by the unit
//! tests in this file, which serialize a real `SPA_TYPE_OBJECT_Props` POD with
//! libspa and round-trip it back through [`parse_props_pod`] — including the
//! volume-only and mute-only PODs PipeWire is free to push.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use log::{debug, info, warn};
use pipewire::context::ContextRc;
use pipewire::main_loop::MainLoopRc;
use pipewire::node::{Node, NodeInfoRef, NodeState};
use pipewire::proxy::Listener;
use pipewire::registry::{GlobalObject, RegistryRc};
use pipewire::spa::pod::Pod;
use pipewire::spa::utils::dict::DictRef;
use pipewire::types::ObjectType;
use tokio::sync::oneshot;

use crate::widget::{DeviceKind, Msg, Volume};

use crate::producer::{MsgSender, Producer, ProducerFuture, ProducerResult};

/// How often the producer polls the cached PipeWire state.
///
/// Two seconds is frequent enough to track volume keys as they happen, and
/// far too coarse to keep the loop busy: between ticks the producer is parked
/// on a timer and the render loop is idle, waking only when a sample changes
/// a visible label.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(2);

/// How long a fresh connection has to answer its first `core.sync` before it
/// is written off as never-handshaked and retried.
///
/// Two seconds is far longer than a live daemon needs (the handshake is a
/// couple of socket round-trips on the same machine) and short enough that a
/// bar racing its audio server recovers before the user looks at it. The
/// timeout fails safe: being too aggressive costs one reconnect that then
/// succeeds.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// How often an established connection is re-checked with a `core.sync`.
///
/// This is what notices a daemon that died without closing the socket cleanly.
/// Ten seconds keeps the recovery bound tight while costing two tiny protocol
/// messages per interval.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

/// How long [`supervise`] waits before the first reconnect attempt.
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// The ceiling the reconnect backoff doubles up to.
///
/// A host with no PipeWire at all never succeeds, so the backoff exists to keep
/// that case down to one cheap attempt every 30 seconds instead of one per
/// second for the life of the session.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);

/// How many poll ticks pass between forced re-emits of the current snapshot.
///
/// [`on_tick`] gates sends on a real change, which means a snapshot that never
/// reached the render loop is never re-sent and the widget stays blank until
/// the volume physically changes. Dropping the gate every 15th tick (~30s at
/// the default interval) bounds that to half a minute. It is nearly free:
/// `VolumeWidget::update` diffs its own state, so an unchanged value costs one
/// channel message and no redraw.
const RESYNC_TICKS: u32 = 15;

/// The object id of the PipeWire core itself (`PW_ID_CORE`), which the
/// `pipewire` crate does not re-export. `done` and `error` events carrying this
/// id are about the connection rather than about a bound proxy.
const PW_ID_CORE: u32 = 0;

/// The PipeWire metadata key whose value is the configured default audio sink.
///
/// Read from the `Metadata` global the server publishes; the value is a JSON
/// `{"name":"<sink-name>"}` payload, parsed with a simple key/string match
/// (the JSON is on a single line and the field is always named `name`).
const DEFAULT_AUDIO_SINK_KEY: &str = "default.audio.sink";

/// Map a `device.icon-name` value (e.g. `audio-headphones`, `video-display`)
/// to a [`DeviceKind`].
///
/// Falls back to [`DeviceKind::Other`] for anything the heuristics did not
/// classify, so a custom icon on an unusual sink never breaks the bar. The
/// `Option` input is what [`DictRef::get`] returns for a missing key.
///
/// The wireplumber/pipewire-pulse default icon naming scheme is
/// `<category>-<kind>[-<variant>]`:
/// `audio-headphones`, `audio-headset`, `audio-speakers`,
/// `audio-card-analog`, `video-display`, `video-monitor`, `tv`, `phone`.
/// The `audio-card-*` family is what wireplumber emits for built-in sound
/// cards and HDMI outputs without a more specific icon — `audio-card-analog`
/// is a typical internal speaker, `audio-card-hdmi` an HDMI output, etc.
/// All `audio-card-*` icons default to [`DeviceKind::Speakers`] (an internal
/// card without headphones plugged in is the most common state), with the
/// `audio-card-hdmi` / `audio-card-spdif` / `*-dp` variants falling through
/// to [`DeviceKind::Monitor`] when the icon name says so.
pub fn device_kind_from_icon_name(icon: Option<&str>) -> DeviceKind {
    let Some(icon) = icon else {
        return DeviceKind::Other;
    };
    if icon.starts_with("audio-headset") {
        DeviceKind::Headset
    } else if icon.starts_with("audio-headphone") {
        DeviceKind::Headphones
    } else if icon.starts_with("audio-speaker") {
        DeviceKind::Speakers
    } else if icon.starts_with("audio-card-hdmi")
        || icon.starts_with("audio-card-dp")
        || icon.starts_with("audio-card-displayport")
    {
        DeviceKind::Monitor
    } else if icon.starts_with("audio-card") {
        // `audio-card-analog`, `audio-card-spdif`, `audio-card-usb`, …
        // Built-in sound cards without a more specific signal. Internal
        // speakers are the most common case.
        DeviceKind::Speakers
    } else if icon.starts_with("video-display") || icon.starts_with("video-monitor") {
        DeviceKind::Monitor
    } else if icon.starts_with("audio-handsfree")
        || icon.starts_with("audio-cellphone")
        || icon == "phone"
    {
        DeviceKind::Phone
    } else if icon.starts_with("tv") || icon.starts_with("television") {
        DeviceKind::Tv
    } else {
        DeviceKind::Other
    }
}

/// Map a `device.form-factor` value (e.g. `headphone`, `speaker`, `monitor`)
/// to a [`DeviceKind`].
///
/// Used as a fallback when `device.icon-name` is absent. Like the icon-name
/// mapper, returns [`DeviceKind::Other`] for anything unrecognized.
pub fn device_kind_from_form_factor(form: Option<&str>) -> DeviceKind {
    let Some(form) = form else {
        return DeviceKind::Other;
    };
    match form {
        "headphone" | "earphone" => DeviceKind::Headphones,
        "headset" => DeviceKind::Headset,
        "speaker" | "desk" | "hifi" | "computer" | "portable" => DeviceKind::Speakers,
        "monitor" => DeviceKind::Monitor,
        "tv" => DeviceKind::Tv,
        "handset" | "phone" | "car" => DeviceKind::Phone,
        _ => DeviceKind::Other,
    }
}

/// Classify a node's device kind from its properties dictionary.
///
/// Tries `device.icon-name` first (the cleanest signal wireplumber / pipewire
/// publish — it is a stable, XDG-style icon name) and falls back to
/// `device.form-factor` (a coarser enum) for sinks that only set the latter.
pub fn device_kind_from_props(props: &DictRef) -> DeviceKind {
    let icon = props.get("device.icon-name");
    let kind = device_kind_from_icon_name(icon);
    if kind != DeviceKind::Other {
        return kind;
    }
    device_kind_from_form_factor(props.get("device.form-factor"))
}

/// Whether a node's `media.class` property marks it as an audio output sink.
///
/// PipeWire publishes `media.class = "Audio/Sink"` for output sinks and
/// `"Audio/Source"` for inputs / microphones. The bar reads output only, so
/// anything that is not `"Audio/Sink"` is filtered out at the registry
/// listener.
pub fn is_audio_sink(props: &DictRef) -> bool {
    props.get("media.class") == Some("Audio/Sink")
}

/// Pick the id of the active sink from a list of candidates.
///
/// Filters to the sinks that are currently running, then sorts the survivors
/// by their name and returns the id of the first one — a deterministic
/// tiebreak that is independent of registry iteration order. Returns
/// `None` when no candidate is running.
pub fn pick_active_sink_id(candidates: &[(u32, &str, bool)]) -> Option<u32> {
    candidates
        .iter()
        .filter(|(_, _, running)| *running)
        .min_by(|a, b| a.1.cmp(b.1))
        .map(|(id, _, _)| *id)
}

/// Whether the node is currently playing back, regardless of which
/// [`NodeState`] variant the runtime emits.
///
/// `Running` is the only state we treat as "active"; `Suspended` and `Idle`
/// both mean the sink is not making sound, and `Creating` / `Error(_)` are
/// transient startup / failure states that should also fall through to the
/// configured default.
fn is_node_running(state: &NodeState<'_>) -> bool {
    matches!(state, NodeState::Running)
}

/// The volume and/or mute state carried by one `ParamType::Props` POD.
///
/// Both fields are optional because PipeWire pushes `Props` updates
/// incrementally: a POD may carry `channelVolumes` without `mute`, or the other
/// way round. Requiring both in the same POD used to discard those updates
/// silently, which could leave a sink's cached volume unset forever — and an
/// unset volume renders as no widget at all. Each present field is merged onto
/// the cached sink entry instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PropsUpdate {
    /// User-volume in `[0, 1]`, from `channelVolumes` when present.
    pub volume: Option<f32>,
    /// Mute flag, from `mute` when present.
    pub muted: Option<bool>,
}

/// Extract the volume and/or mute state from a PipeWire `ParamType::Props` POD.
///
/// `pod_bytes` is the raw byte slice of a `spa_pod` whose body is a
/// `SPA_TYPE_OBJECT_Props` (the `ParamType::Props` object). The function
/// looks up the `channelVolumes` (per-channel Float array) and `mute`
/// (Bool) properties, either of which may be absent. Returns `None` only when
/// the input is malformed or carries neither property — a transient type drift
/// never takes the source down.
///
/// **Why `channelVolumes` and not `volume`.** In wireplumber /
/// pipewire-pulse, the `SPA_PROP_volume` field is the **sink's max
/// linear amplitude** (always 1.0 in a typical wireplumber setup) and the
/// per-channel linear amplitudes live in `SPA_PROP_channelVolumes`. The
/// per-channel values are the **cubic** of the user-set "percent" pactl
/// shows: pactl at 30% reports `channelVolumes = 0.30^3 = 0.027` and
/// `SPA_PROP_volume = 1.0`. This function therefore takes the average of
/// `channelVolumes`, applies `cbrt` to recover the user-volume in
/// `[0, 1]`, clamps to `[0, 1]`, and returns that — so the widget shows
/// the same number pactl / waybar do.
pub fn parse_props_pod(pod_bytes: &[u8]) -> Option<PropsUpdate> {
    use pipewire::spa::pod::Value;
    use pipewire::spa::sys::{SPA_PROP_channelVolumes, SPA_PROP_mute};

    let pod = Pod::from_bytes(pod_bytes)?;
    let obj = pod.as_object().ok()?;

    // `SPA_PROP_channelVolumes` is a per-channel Float array. The
    // libspa-sys bindgen output pins the key at `0x10008`; the test in
    // this file asserts the wire format to catch a header / bindgen
    // drift.
    let volume = obj
        .find_prop(pipewire::spa::utils::Id(SPA_PROP_channelVolumes))
        .and_then(|channel_volumes_pod| {
            // Deserialize the channel-volumes value into a `Value` enum so we
            // can pull the per-channel Float array out of whatever shape the
            // serializer chose.
            let value: Value =
                pipewire::spa::pod::deserialize::PodDeserializer::deserialize_any_from(
                    channel_volumes_pod.value().as_bytes(),
                )
                .ok()
                .map(|(_, v)| v)?;
            // Compute the user-volume from the per-channel Float array (or a
            // single Float for mono sinks). The `Value` owns the data, so we
            // borrow into it; binding `value` to a name keeps it alive for the
            // duration of the borrow.
            match &value {
                Value::ValueArray(pipewire::spa::pod::ValueArray::Float(v)) => {
                    average_to_user_volume(v)
                }
                Value::Float(v) => average_to_user_volume(&[*v]),
                _ => None,
            }
        });
    let muted = obj
        .find_prop(pipewire::spa::utils::Id(SPA_PROP_mute))
        .and_then(|mute_pod| mute_pod.value().get_bool().ok());

    // A POD that carries neither property tells us nothing; anything else is a
    // partial update worth merging.
    if volume.is_none() && muted.is_none() {
        return None;
    }
    Some(PropsUpdate { volume, muted })
}

/// Average a slice of per-channel linear amplitudes and apply the cubic
/// inverse (`cbrt`) to recover the user-volume in `[0, 1]`. Returns
/// `None` for an empty slice.
pub fn average_to_user_volume(channels: &[f32]) -> Option<f32> {
    if channels.is_empty() {
        return None;
    }
    let sum: f32 = channels.iter().filter(|v| v.is_finite() && **v > 0.0).sum();
    let n = channels
        .iter()
        .filter(|v| v.is_finite() && **v > 0.0)
        .count() as f32;
    if n == 0.0 {
        return Some(0.0);
    }
    let avg = sum / n;
    // cbrt is monotonic and well-defined for non-negative finite values;
    // we already filtered negatives, and a NaN/Inf upstream yields a
    // user-volume of 0.
    let cbrt = avg.max(0.0).cbrt();
    Some(cbrt.clamp(0.0, 1.0))
}

/// The producer's outbound channel plus the shutdown flag that survives a
/// reconnect.
///
/// [`supervise`] outlives any single PipeWire session, so it needs to tell a
/// server that went away (reconnect) apart from a render loop that went away
/// (stop). The only signal for the latter is a closed channel, which is
/// observed deep inside the event callbacks — this carries the flag back out.
#[derive(Clone)]
struct Emitter {
    tx: MsgSender,
    closed: Arc<AtomicBool>,
}

impl Emitter {
    fn new(tx: MsgSender) -> Self {
        Self {
            tx,
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Send a message, latching [`Emitter::is_closed`] when the render loop has
    /// dropped its receiver. Nothing else can be done about a closed channel,
    /// so the error is recorded rather than propagated through every callback.
    fn send(&self, msg: Msg) {
        if self.tx.send(msg).is_err() {
            debug!("volume: tx.send returned Closed");
            self.closed.store(true, Ordering::Relaxed);
        }
    }

    /// Whether the render loop has gone away, which is the producer's cue to
    /// stop reconnecting and return `Ok(())`.
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

/// Cached state for one PipeWire sink node.
///
/// The volume and mute flags are tracked separately because PipeWire is free to
/// push a `Props` POD carrying only one of them; see [`PropsUpdate`].
#[derive(Clone)]
struct SinkEntry {
    name: String,
    device: DeviceKind,
    running: bool,
    /// User-volume in `[0, 1]` from the last `channelVolumes` we parsed.
    volume: Option<f32>,
    /// Mute flag from the last `mute` we parsed.
    muted: Option<bool>,
}

/// A [`Producer`] that polls PipeWire for the active output sink and emits
/// [`Msg::Volume`] snapshots on every change.
///
/// Construct with [`new`](VolumeProducer::new) and hand it to the producer
/// bridge; it spawns a dedicated PipeWire main-loop thread on
/// [`run`](Producer::run). Snapshots are event-driven — every sink `info`,
/// `param`, and metadata change re-evaluates the active sink — with a poll on
/// `interval` as the safety net for anything the server did not push. It
/// reconnects for as long as the render loop is alive, so a PipeWire daemon
/// that is not up yet (or restarts later) costs a few seconds, not the session.
pub struct VolumeProducer {
    interval: Duration,
}

impl VolumeProducer {
    /// Create a volume producer sampling at the default cadence.
    pub fn new() -> Self {
        Self {
            interval: DEFAULT_INTERVAL,
        }
    }

    /// Create a producer sampling at a custom `interval` (used by tests).
    pub fn with_interval(interval: Duration) -> Self {
        Self { interval }
    }
}

impl Default for VolumeProducer {
    fn default() -> Self {
        Self::new()
    }
}

impl Producer for VolumeProducer {
    fn name(&self) -> String {
        "volume".to_string()
    }

    fn run(self: Box<Self>, tx: MsgSender) -> ProducerFuture {
        Box::pin(run(tx, self.interval))
    }
}

/// Drive the producer: run the PipeWire main loop on a dedicated OS thread,
/// reconnecting for as long as the render loop wants messages.
///
/// The thread runs [`supervise`], which only returns once the render loop has
/// dropped its receiver. A PipeWire server that is missing, not up yet, or
/// restarted is a recoverable condition handled inside that loop, so — unlike
/// every other producer — a connection failure here does not end the producer.
/// The widget goes blank via [`Msg::Volume(None)`] while there is no reachable
/// audio server and fills back in on reconnect.
async fn run(tx: MsgSender, period: Duration) -> ProducerResult {
    let (done_tx, done_rx) = oneshot::channel();
    let emitter = Emitter::new(tx);
    thread::Builder::new()
        .name("tablero-volume".to_string())
        .spawn(move || {
            // The thread owns every PipeWire main loop this producer creates and
            // exits when the render loop goes away. The result travels back via
            // the oneshot so the producer bridge can log it like any other
            // producer failure.
            let result = supervise(&emitter, period);
            let _ = done_tx.send(result);
        })
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;

    match done_rx.await {
        Ok(result) => result,
        // The thread was lost (panic, join error, …) — the producer is
        // effectively dead. Surface the cancellation as an error so the
        // bridge logs it and isolates, rather than pretending success.
        Err(_) => Err("volume producer thread dropped its result".into()),
    }
}

/// What one [`run_session`] attempt achieved before it ended.
struct SessionOutcome {
    /// Whether the core ever answered a sync, i.e. whether we were genuinely
    /// talking to a PipeWire server. This is what [`supervise`] uses to tell
    /// "the server restarted under us" (recover fast, say so once) from "there
    /// is no server here" (back off quietly).
    handshook: bool,
    /// Why the session ended. `Ok(())` means the main loop was asked to quit —
    /// by the watchdog, a core error, or the server closing the connection.
    result: ProducerResult,
}

/// Own successive PipeWire sessions until the render loop goes away.
///
/// Reconnects with an exponential backoff from [`RETRY_DELAY`] to
/// [`MAX_RETRY_DELAY`], resetting to the fast cadence whenever a session
/// actually reached the server, so:
///
/// * a bar that started before its audio server recovers in a couple of
///   seconds and says so once, at `warn` level;
/// * a daemon that restarts mid-session is picked up on the next heartbeat and
///   recovers at the same fast cadence;
/// * a host with no PipeWire at all settles at one cheap attempt every 30
///   seconds, logged at `debug`, rather than warning once a second forever.
///
/// The widget is only blanked once a *retry* has failed. A session that
/// handshaked and then died usually comes back on the very next attempt, and
/// emitting [`Msg::Volume(None)`] eagerly would just blink the pill.
fn supervise(tx: &Emitter, period: Duration) -> ProducerResult {
    // Initialize PipeWire once for the thread. The `init`/`deinit` pair is a
    // global reference count; the static refcount means a leak-free shutdown is
    // the OS's job when the process exits.
    pipewire::init();

    let mut delay = RETRY_DELAY;
    let mut failures: u32 = 0;
    loop {
        let outcome = run_session(tx, period);
        if tx.is_closed() {
            // The render loop dropped its receiver: nothing left to feed.
            return Ok(());
        }
        if outcome.handshook {
            // The server was reachable this time, so whatever just happened is
            // a fresh problem rather than a host without PipeWire.
            failures = 0;
            delay = RETRY_DELAY;
        }
        failures += 1;
        let ended = match &outcome.result {
            Ok(()) => "ended".to_string(),
            Err(e) => format!("failed: {e}"),
        };
        if failures == 1 {
            warn!("volume: PipeWire session {ended}; reconnecting in {delay:?}");
        } else {
            debug!("volume: PipeWire session {ended} (attempt {failures}); retrying in {delay:?}");
            // Only now is the source demonstrably gone rather than restarting,
            // so the pill can be blanked without blinking it.
            tx.send(Msg::Volume(None));
        }
        thread::sleep(delay);
        delay = (delay * 2).min(MAX_RETRY_DELAY);
    }
}

/// Run one PipeWire session: connect, watch sinks, and emit snapshots until the
/// main loop quits.
///
/// Returns how the attempt went rather than a bare `Result`, because
/// [`supervise`] needs to know whether the server was ever actually reachable.
fn run_session(tx: &Emitter, period: Duration) -> SessionOutcome {
    // Set by the core's `done` callback. `Cell` rather than an atomic because
    // the whole session is single-threaded — PipeWire's main loop and every
    // listener run on this thread.
    let handshook = Rc::new(Cell::new(false));
    let result = run_main_loop(tx, period, &handshook);
    SessionOutcome {
        handshook: handshook.get(),
        result,
    }
}

/// Run the PipeWire main loop until the server goes away, the watchdog gives up
/// on it, or an unrecoverable error surfaces.
///
/// Every read of PipeWire state is funneled through the shared
/// [`Arc<Mutex<State>>`]; a periodic timer re-evaluates the active sink and
/// emits a [`Msg::Volume`] if the normalized snapshot changed. `handshook` is
/// set once the core answers a sync, which is what proves the connection is
/// real; see the module docs on surviving a server that is not there yet.
fn run_main_loop(tx: &Emitter, period: Duration, handshook: &Rc<Cell<bool>>) -> ProducerResult {
    let mainloop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    debug!("volume: connected to PipeWire; awaiting the core handshake");

    // Liveness. `pw_context_connect` succeeds against a socket-activated
    // `pipewire-0` even when no daemon has accepted the connection yet, and the
    // server then sends neither a global nor an error — so an affirmative check
    // is the only way to notice. `pending_sync` tracks whether the sync we
    // issued is still outstanding; the watchdog below quits the loop when one
    // goes unanswered, and `supervise` reconnects.
    let pending_sync = Rc::new(Cell::new(false));
    let handshook_for_done = handshook.clone();
    let pending_for_done = pending_sync.clone();
    let mainloop_for_error = mainloop.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, _seq| {
            if id != PW_ID_CORE {
                return;
            }
            pending_for_done.set(false);
            if !handshook_for_done.replace(true) {
                info!("volume: PipeWire core handshake complete");
            }
        })
        .error(move |id, _seq, res, message| {
            warn!("volume: PipeWire reported an error on object {id}: {message} ({res})");
            // An error on a single proxy (a sink that vanished mid-bind) is the
            // registry's problem; an error on the core is the connection's.
            if id == PW_ID_CORE
                && let Some(mainloop) = mainloop_for_error.upgrade()
            {
                mainloop.quit();
            }
        })
        .register();

    // Issue the first sync before the loop starts, so the watchdog's first fire
    // at `CONNECT_TIMEOUT` already has something outstanding to find. Without
    // this the never-handshaked case would take a full heartbeat longer to
    // detect.
    core.sync(PW_ID_CORE as i32)?;
    pending_sync.set(true);

    let core_for_watchdog = core.clone();
    let mainloop_for_watchdog = mainloop.downgrade();
    let pending_for_watchdog = pending_sync.clone();
    let watchdog = mainloop.loop_().add_timer(move |_expirations| {
        if pending_for_watchdog.get() {
            warn!("volume: PipeWire did not answer a sync; dropping the connection");
            if let Some(mainloop) = mainloop_for_watchdog.upgrade() {
                mainloop.quit();
            }
            return;
        }
        match core_for_watchdog.sync(PW_ID_CORE as i32) {
            Ok(_) => pending_for_watchdog.set(true),
            Err(e) => {
                warn!("volume: PipeWire sync failed: {e}; dropping the connection");
                if let Some(mainloop) = mainloop_for_watchdog.upgrade() {
                    mainloop.quit();
                }
            }
        }
    });
    // First fire proves the handshake landed; later fires prove the server is
    // still alive.
    watchdog.update_timer(Some(CONNECT_TIMEOUT), Some(HEARTBEAT_INTERVAL));

    // The shared state: the active-sink candidate set, the configured
    // default sink name (set by the `default.audio.sink` metadata), and the
    // most-recently-emitted snapshot, so re-emits are gated on a real change.
    // The state is shared by the periodic timer (the safety-net poll) AND
    // by the per-sink / metadata listeners (instant feedback on every
    // PipeWire event).
    let state: Arc<Mutex<State>> = Arc::new(Mutex::new(State::default()));
    let state_for_timer = state.clone();
    let tx_for_timer = tx.clone();

    // The per-Node listeners (and the Node proxies themselves — the
    // listener's internal hook references the proxy, so dropping the
    // proxy invalidates the listener) must outlive the callback that
    // creates them. The whole producer is single-threaded (PipeWire's
    // main loop is synchronous and the `Listener` trait is `!Send`),
    // so a thread-local `Rc<RefCell<Vec<…>>>` is the right level of
    // sharing — exactly the `Proxies` struct pw-mon uses for the same
    // reason.
    let node_proxies: Rc<RefCell<Vec<Node>>> = Rc::new(RefCell::new(Vec::new()));
    let node_listeners: Rc<RefCell<Vec<Box<dyn Listener>>>> = Rc::new(RefCell::new(Vec::new()));
    let node_proxies_for_registry = node_proxies.clone();
    let node_listeners_for_registry = node_listeners.clone();
    let node_listeners_for_metadata = node_listeners.clone();

    // Periodic timer: a safety-net poll that re-evaluates the snapshot
    // every `period`. The per-sink and metadata listeners call `on_tick`
    // directly on every PipeWire event, so this is the fallback for any
    // missed push (a server that doesn't push, a sink that disconnects
    // before its `global_remove` is delivered, etc.). The timer is
    // driven by the PipeWire main loop's own event source, so the thread
    // stays parked between ticks.
    let mainloop_for_timer = mainloop.downgrade();
    let timer = mainloop.loop_().add_timer(move |_expirations| {
        on_poll_tick(&state_for_timer, &tx_for_timer);
        // Shutdown: once the render loop has dropped its receiver there is
        // nothing left to feed, so end the session and let `supervise` return
        // instead of reconnecting to a bar that is gone. A closed channel is
        // only observable through a failed send, and `on_tick` sends only on a
        // change — the [`RESYNC_TICKS`] re-emit is what bounds the detection to
        // ~30s on an otherwise idle machine.
        if tx_for_timer.is_closed()
            && let Some(mainloop) = mainloop_for_timer.upgrade()
        {
            debug!("volume: render loop went away; ending the PipeWire session");
            mainloop.quit();
        }
    });
    timer.update_timer(Some(period), Some(period));

    // The registry listener is the source of truth for which nodes are
    // sinks and what their current state is. It captures `state` (a clone
    // of the shared `Arc<Mutex<State>>`) and the registry (a weak upgrade)
    // so the callback can bind a `Node` proxy for every audio sink.
    let _registry_listener: pipewire::registry::Listener = attach_registry_listener(
        registry.clone(),
        state.clone(),
        node_proxies_for_registry,
        node_listeners_for_registry,
        tx.clone(),
    );

    // The metadata listener tracks the `default.audio.sink` value, which
    // is the fallback when no sink is currently running. A separate global
    // registration makes the metadata node available to the listener.
    let metadata_proxies: Rc<RefCell<Vec<pipewire::metadata::Metadata>>> =
        Rc::new(RefCell::new(Vec::new()));
    let _metadata_listener: pipewire::registry::Listener = attach_metadata_listener(
        &registry,
        state.clone(),
        metadata_proxies,
        node_listeners_for_metadata,
        tx.clone(),
    );

    mainloop.run();

    // PipeWire's `deinit` is `unsafe`, and the workspace lints forbid an
    // `unsafe` block here. Skipping the deinit is safe — the process is
    // about to exit, so the per-process PipeWire state goes away with it.
    Ok(())
}

/// The shared, mutable producer state: cached sinks, the configured default,
/// and the most recent snapshot we emitted.
///
/// One instance per PipeWire session — a reconnect starts from
/// [`State::default`], so `last_emitted` never gates a snapshot on a value the
/// previous connection sent.
#[derive(Default)]
struct State {
    sinks: HashMap<u32, SinkEntry>,
    default_sink_name: Option<String>,
    last_emitted: Option<Option<Volume>>,
    /// Poll ticks since the change-gate was last dropped; see [`RESYNC_TICKS`].
    ticks_since_resync: u32,
}

/// Handle one safety-net poll tick.
///
/// Same as [`on_tick`], except that every [`RESYNC_TICKS`]th tick clears the
/// change-gate first, so the current snapshot is re-sent even if it has not
/// changed. Without this, a `Msg::Volume` that never reached the render loop
/// leaves the widget blank until the volume physically changes, because
/// `last_emitted` is written on send rather than on delivery.
fn on_poll_tick(state: &Arc<Mutex<State>>, tx: &Emitter) {
    {
        let mut s = state.lock().expect("volume state poisoned");
        s.ticks_since_resync += 1;
        if s.ticks_since_resync >= RESYNC_TICKS {
            s.ticks_since_resync = 0;
            s.last_emitted = None;
        }
    }
    on_tick(state, tx);
}

/// Compute the snapshot to emit on a tick and send it through the
/// cross-thread [`Emitter`] if (and only if) it differs from the last one.
///
/// A re-emit happens when the normalized [`Volume`] value changes (level,
/// mute, or device kind) or when the source goes from absent to present (or
/// vice versa). Identical snapshots are dropped, so a steady-state machine
/// costs one PipeWire event and zero render-loop work per tick.
fn on_tick(state: &Arc<Mutex<State>>, tx: &Emitter) {
    let snapshot = compute_snapshot(state);
    let changed = {
        let state = state.lock().expect("volume state poisoned");
        state.last_emitted.as_ref() != Some(&snapshot)
    };
    if changed {
        let first = {
            let mut state = state.lock().expect("volume state poisoned");
            // `None` (nothing sent yet) and `Some(None)` (sent "no source")
            // both mean the widget is not showing a reading right now.
            let first = !matches!(state.last_emitted, Some(Some(_)));
            state.last_emitted = Some(snapshot);
            first
        };
        if let (true, Some(volume)) = (first, snapshot) {
            info!(
                "volume: active sink at {}%{}",
                volume.level(),
                if volume.muted() { " (muted)" } else { "" }
            );
        }
        tx.send(Msg::Volume(snapshot));
    }
}

/// Compute the normalized [`Volume`] snapshot to emit on a tick.
///
/// 1. Pick the active sink id (running, deterministic by name), or
/// 2. The configured default (any state) from `default.audio.sink` metadata,
///    or
/// 3. The first sink sorted by name (so the volume is *always* shown when
///    at least one sink exists — even when nothing is playing right now
///    and the wireplumber user daemon has not pushed the metadata). This
///    is what keeps the widget useful: a `Vol 50%` reading is the user's
///    most common reference point, regardless of whether audio is
///    playing.
///
/// A sink that has not yet reported a volume (the `enum_params` round-trip is
/// still in flight) reports the bar as "absent" until the first parsing
/// succeeds, so a sink appearing in the registry does not flip the widget to
/// `Vol 0%` and back once the real reading lands. A sink that reported a volume
/// but no mute flag yet renders as unmuted rather than staying invisible —
/// PipeWire may push the two in separate PODs.
fn compute_snapshot(state: &Arc<Mutex<State>>) -> Option<Volume> {
    let state = state.lock().expect("volume state poisoned");
    let sinks = &state.sinks;
    if sinks.is_empty() {
        return None;
    }

    // Build the candidate set, in a shape the pure helper can sort against.
    let candidates: Vec<(u32, &str, bool)> = sinks
        .iter()
        .map(|(id, entry)| (*id, entry.name.as_str(), entry.running))
        .collect();

    // (1) running wins.
    let active = pick_active_sink_id(&candidates)
        // (2) configured default (any state) wins next.
        .or_else(|| {
            let default_name = state.default_sink_name.as_deref()?;
            let mut sorted = candidates.clone();
            sorted.sort_by(|a, b| a.1.cmp(b.1));
            sorted
                .into_iter()
                .find(|(_, name, _)| *name == default_name)
                .map(|(id, _, _)| id)
        })
        // (3) deterministic-by-name: the first sink alphabetically. This
        // is what keeps the widget always showing a value when at least
        // one sink is reachable.
        .or_else(|| {
            let mut sorted = candidates.clone();
            sorted.sort_by(|a, b| a.1.cmp(b.1));
            sorted.into_iter().next().map(|(id, _, _)| id)
        });

    let id = active?;
    let entry = sinks.get(&id)?;

    let level = entry.volume?;
    Some(Volume::new(
        level,
        entry.muted.unwrap_or(false),
        entry.device,
    ))
}

/// Install the registry listener that builds the cached [`SinkEntry`] map.
///
/// On every `global` event for an `ObjectType::Node` whose `media.class` is
/// `"Audio/Sink"`, the listener binds a `Node` proxy, subscribes to its
/// `ParamType::Props`, and installs `info` and `param` callbacks that
/// update the shared state. On `global_remove`, the entry is dropped.
///
/// The registry's `global` callback only exposes the global-level
/// properties (`factory.id`, `client.id`, `node.name`, `media.class`).
/// The full per-node properties (`device.icon-name`, `device.form-factor`,
/// `node.description`, `node.nick`, …) arrive in the `Node`'s `info`
/// callback, so the device-kind classification uses the info-time props.
///
/// `node_proxies` owns every bound `Node` (a listener's internal hook
/// references the proxy, so the proxy must outlive the listener);
/// `node_listeners` owns the listener itself. Both are move-cloned into
/// the closure so the registry's `Fn` bound is satisfied and the
/// resources live for the rest of the main loop.
///
/// The `tx` cross-thread sender is also captured: every per-sink event
/// (`info` and `param`) re-evaluates the active-sink snapshot via
/// [`on_tick`] and re-emits on a real change, which is what gives the
/// widget instant feedback — the 2-second safety-net poll in
/// [`run_main_loop`] is just a fallback for events the server didn't push.
fn attach_registry_listener(
    registry: RegistryRc,
    state: Arc<Mutex<State>>,
    node_proxies: Rc<RefCell<Vec<Node>>>,
    node_listeners: Rc<RefCell<Vec<Box<dyn Listener>>>>,
    tx: Emitter,
) -> pipewire::registry::Listener {
    let registry_weak = registry.downgrade();
    let state_for_global = state.clone();
    let state_for_remove = state;
    let node_proxies_for_cb = node_proxies.clone();
    let node_listeners_for_cb = node_listeners.clone();
    let tx_for_global = tx.clone();
    let tx_for_remove = tx;
    registry
        .add_listener_local()
        .global(move |obj| {
            if obj.type_ != ObjectType::Node {
                return;
            }
            let Some(props) = obj.props else {
                return;
            };
            if !is_audio_sink(props) {
                return;
            }
            let Some(registry) = registry_weak.upgrade() else {
                return;
            };
            let node: Node = match registry.bind(obj) {
                Ok(n) => n,
                Err(e) => {
                    debug!("volume: bind sink node failed: {e}");
                    return;
                }
            };
            let id = obj.id;
            let name = props.get("node.name").unwrap_or("?").to_string();
            debug!("volume: found audio sink {id} ({name})");

            // Seed the entry immediately with a placeholder device kind so
            // the tick can resolve a name even before the first `info`
            // event lands. The `info` listener overwrites `device` with the
            // full classification once the per-node properties arrive.
            {
                let mut s = state_for_global.lock().expect("volume state poisoned");
                s.sinks.insert(
                    id,
                    SinkEntry {
                        name: name.clone(),
                        device: DeviceKind::Other,
                        running: false,
                        volume: None,
                        muted: None,
                    },
                );
            }

            let state_for_info = state_for_global.clone();
            let state_for_param = state_for_global.clone();
            let tx_for_info = tx_for_global.clone();
            let tx_for_param = tx_for_global.clone();
            let state_for_tick_after_info = state_for_global.clone();
            let state_for_tick_after_param = state_for_global.clone();
            let node_listener = node
                .add_listener_local()
                .info(move |info: &NodeInfoRef| {
                    let running = is_node_running(&info.state());
                    let props = info.props();
                    // The full per-node properties (device.icon-name,
                    // device.form-factor, node.description, …) only arrive
                    // in the info callback. Re-classify on every info
                    // event so a wireplumber metadata update that adds an
                    // icon (e.g. when headphones are plugged in) flips
                    // the widget's glyph on the next tick.
                    let device = props
                        .map(device_kind_from_props)
                        .unwrap_or(DeviceKind::Other);
                    {
                        let mut s = state_for_info.lock().expect("volume state poisoned");
                        if let Some(entry) = s.sinks.get_mut(&id) {
                            entry.running = running;
                            // Only downgrade device-kind on a re-info when
                            // the props actually carry icon/form-factor.
                            // A second `info` event for a state change
                            // (Suspended ↔ Idle) often arrives with
                            // `info.props()` empty — do not clobber the
                            // real classification in that case.
                            if device != DeviceKind::Other || entry.device == DeviceKind::Other {
                                entry.device = device;
                            }
                        }
                    }
                    // A state change (Running ↔ Suspended ↔ Idle) is a
                    // visible change too — the active-sink selection
                    // depends on it, so re-evaluate immediately rather
                    // than waiting for the next 2s poll.
                    on_tick(&state_for_tick_after_info, &tx_for_info);
                })
                .param(move |_seq, _id, _index, _next, param: Option<&Pod>| {
                    let Some(param) = param else {
                        return;
                    };
                    let Some(update) = parse_props_pod(param.as_bytes()) else {
                        // Routine: a sink's `Props` also carry device metadata
                        // (`device`, `deviceName`, `cardName`) with no volume in
                        // sight. A malformed POD lands here too, and is equally
                        // harmless — the next param event carries the state.
                        debug!("volume: sink {id} sent a Props POD with no volume or mute");
                        return;
                    };
                    {
                        let mut s = state_for_param.lock().expect("volume state poisoned");
                        if let Some(entry) = s.sinks.get_mut(&id) {
                            // Merge rather than replace: PipeWire may push the
                            // volume and the mute flag in separate PODs, and
                            // dropping a partial update would strand the entry.
                            if let Some(volume) = update.volume {
                                entry.volume = Some(volume);
                            }
                            if let Some(muted) = update.muted {
                                entry.muted = Some(muted);
                            }
                        }
                    }
                    // The volume or mute just changed — re-evaluate the
                    // active-sink snapshot and re-emit on a real change.
                    // This is the path that makes the widget feel
                    // instant: pactl/wpctl triggers a Props push, the
                    // param event fires, and the widget updates within a
                    // frame.
                    on_tick(&state_for_tick_after_param, &tx_for_param);
                })
                .register();
            // Both the listener and the Node it references need to outlive
            // the closure. The subscriptions are issued on a `&Node`
            // borrow first; then the listener is moved into the shared
            // Vec; then the Node itself is moved into the proxy Vec so
            // the listener's internal hook reference stays valid for the
            // rest of the main loop.
            node.subscribe_params(&[pipewire::spa::param::ParamType::Props]);
            node.enum_params(0, Some(pipewire::spa::param::ParamType::Props), 0, 1);
            node_listeners_for_cb
                .borrow_mut()
                .push(Box::new(node_listener));
            node_proxies_for_cb.borrow_mut().push(node);
            // The sink just appeared in the registry, which is also a
            // visible change (a new candidate for active-sink selection).
            // Emit a fresh snapshot so the widget shows up on the first
            // bind, without waiting for the 2s safety-net poll.
            on_tick(&state_for_global, &tx_for_global);
        })
        .global_remove(move |id| {
            // Scope the guard to a block: this callback then calls
            // `on_tick`, which takes the same `Mutex` from the same
            // thread. A `std::sync::Mutex` is non-reentrant — keeping
            // the guard alive across the `on_tick` call would deadlock
            // the entire PipeWire main loop (the same pitfall that
            // caused the widget to stop updating on any
            // non-sink client disconnect).
            {
                let mut s = state_for_remove.lock().expect("volume state poisoned");
                s.sinks.remove(&id);
            }
            // A sink dropping is also a visible change (the active-sink
            // selection can flip to the next candidate). Re-evaluate
            // immediately.
            on_tick(&state_for_remove, &tx_for_remove);
        })
        .register()
}

/// Install the metadata listener that tracks the configured default sink.
///
/// PipeWire exposes the user's "default audio sink" as a `Metadata` global
/// with property `default.audio.sink`; the value is a JSON payload of the
/// form `{"name":"<sink-name>"}`. The listener parses it and stashes the
/// name in the shared state, where the tick handler uses it as the fallback
/// when no sink is currently running.
///
/// The function returns the outer registry listener; the inner
/// per-Metadata listener is stored in the shared `node_listeners` vec
/// (see [`attach_registry_listener`] for the lifetime rationale). When
/// the `default.audio.sink` property arrives or changes, the per-Metadata
/// `property` callback re-evaluates the snapshot via [`on_tick`], which
/// is what makes the widget track the user's "default sink" choice the
/// moment wireplumber publishes the change.
fn attach_metadata_listener(
    registry: &RegistryRc,
    state: Arc<Mutex<State>>,
    node_proxies: Rc<RefCell<Vec<pipewire::metadata::Metadata>>>,
    node_listeners: Rc<RefCell<Vec<Box<dyn Listener>>>>,
    tx: Emitter,
) -> pipewire::registry::Listener {
    use pipewire::metadata::Metadata;

    let registry_weak = registry.downgrade();
    let node_listeners_for_cb = node_listeners.clone();
    let tx_for_metadata = tx;
    registry
        .add_listener_local()
        .global(move |obj: &GlobalObject<&DictRef>| {
            if obj.type_ != ObjectType::Metadata {
                return;
            }
            let Some(registry) = registry_weak.upgrade() else {
                return;
            };
            let metadata: Metadata = match registry.bind(obj) {
                Ok(m) => m,
                Err(err) => {
                    debug!("volume: could not bind metadata global {}: {err}", obj.id);
                    return;
                }
            };
            let state_for_property = state.clone();
            let state_for_tick = state.clone();
            let tx_for_tick = tx_for_metadata.clone();
            let md_listener = metadata
                .add_listener_local()
                .property(move |_subject, key, _type_, value| {
                    if key != Some(DEFAULT_AUDIO_SINK_KEY) {
                        return 0;
                    }
                    let Some(value) = value else {
                        return 0;
                    };
                    let name = parse_metadata_default_name(value);
                    match &name {
                        Some(name) => debug!("volume: default sink is {name}"),
                        None => debug!("volume: default-sink metadata had no usable name: {value}"),
                    }
                    {
                        let mut s = state_for_property.lock().expect("volume state poisoned");
                        s.default_sink_name = name;
                    }
                    // The default-sink choice is a visible change too —
                    // re-evaluate immediately so the widget follows the
                    // user's `wpctl set-default` without the 2s poll.
                    on_tick(&state_for_tick, &tx_for_tick);
                    0
                })
                .register();
            node_listeners_for_cb
                .borrow_mut()
                .push(Box::new(md_listener));
            // The Metadata proxy has to outlive the listener too, but the
            // wireplumber "default" metadata is a single global, so leaking
            // it would be acceptable. We keep a ref-counted pointer in a
            // separate Vec.
            node_proxies.borrow_mut().push(metadata);
        })
        .register()
}

/// Parse the `default.audio.sink` metadata value, a JSON string of the form
/// `{"name":"<sink-name>"}`.
///
/// The full JSON is on a single line; the only field we care about is
/// `name`. A simple `key`/`value` scan extracts the string without dragging
/// in a JSON dependency for one line of input.
fn parse_metadata_default_name(value: &str) -> Option<String> {
    let key = "\"name\"";
    let name_start = value.find(key)?;
    let after_key = &value[name_start + key.len()..];
    let colon = after_key.find(':')?;
    let after_colon = after_key[colon + 1..].trim_start();
    let open_quote = after_colon.find('"')?;
    let after_quote = &after_colon[open_quote + 1..];
    let close_quote = after_quote.find('"')?;
    Some(after_quote[..close_quote].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- device_kind_from_icon_name ----

    #[test]
    fn icon_name_headphones_prefix_maps_to_headphones() {
        assert_eq!(
            device_kind_from_icon_name(Some("audio-headphones")),
            DeviceKind::Headphones
        );
        assert_eq!(
            device_kind_from_icon_name(Some("audio-headphones-analog")),
            DeviceKind::Headphones
        );
    }

    #[test]
    fn icon_name_headset_prefix_maps_to_headset() {
        assert_eq!(
            device_kind_from_icon_name(Some("audio-headset")),
            DeviceKind::Headset
        );
    }

    #[test]
    fn icon_name_speakers_prefix_maps_to_speakers() {
        assert_eq!(
            device_kind_from_icon_name(Some("audio-speakers")),
            DeviceKind::Speakers
        );
    }

    #[test]
    fn icon_name_video_display_maps_to_monitor() {
        assert_eq!(
            device_kind_from_icon_name(Some("video-display")),
            DeviceKind::Monitor
        );
        assert_eq!(
            device_kind_from_icon_name(Some("video-monitor")),
            DeviceKind::Monitor
        );
    }

    #[test]
    fn icon_name_phone_maps_to_phone() {
        assert_eq!(
            device_kind_from_icon_name(Some("audio-handsfree")),
            DeviceKind::Phone
        );
        assert_eq!(device_kind_from_icon_name(Some("phone")), DeviceKind::Phone);
    }

    #[test]
    fn icon_name_tv_maps_to_tv() {
        assert_eq!(device_kind_from_icon_name(Some("tv")), DeviceKind::Tv);
        assert_eq!(
            device_kind_from_icon_name(Some("television")),
            DeviceKind::Tv
        );
    }

    #[test]
    fn icon_name_missing_or_unknown_falls_back_to_other() {
        assert_eq!(device_kind_from_icon_name(None), DeviceKind::Other);
        // A truly unknown icon (not even an `audio-card-*` prefix) keeps
        // the generic `Other` so the bar never shows a wrong glyph.
        assert_eq!(
            device_kind_from_icon_name(Some("totally-unknown-icon")),
            DeviceKind::Other
        );
    }

    #[test]
    fn icon_name_audio_card_analog_maps_to_speakers() {
        // The wireplumber default for a built-in sound card without
        // headphones plugged in: `audio-card-analog`. It is the most
        // common case and maps to Speakers (internal sound).
        assert_eq!(
            device_kind_from_icon_name(Some("audio-card-analog")),
            DeviceKind::Speakers
        );
        assert_eq!(
            device_kind_from_icon_name(Some("audio-card")),
            DeviceKind::Speakers
        );
    }

    #[test]
    fn icon_name_audio_card_hdmi_maps_to_monitor() {
        // HDMI / DisplayPort output of a built-in card uses the
        // `audio-card-hdmi` / `audio-card-dp` / `audio-card-displayport`
        // icon family and is treated as a monitor.
        assert_eq!(
            device_kind_from_icon_name(Some("audio-card-hdmi")),
            DeviceKind::Monitor
        );
        assert_eq!(
            device_kind_from_icon_name(Some("audio-card-dp")),
            DeviceKind::Monitor
        );
    }

    // ---- device_kind_from_form_factor ----

    #[test]
    fn form_factor_known_values_map_to_their_kinds() {
        assert_eq!(
            device_kind_from_form_factor(Some("headphone")),
            DeviceKind::Headphones
        );
        assert_eq!(
            device_kind_from_form_factor(Some("headset")),
            DeviceKind::Headset
        );
        assert_eq!(
            device_kind_from_form_factor(Some("speaker")),
            DeviceKind::Speakers
        );
        assert_eq!(
            device_kind_from_form_factor(Some("monitor")),
            DeviceKind::Monitor
        );
        assert_eq!(device_kind_from_form_factor(Some("tv")), DeviceKind::Tv);
        assert_eq!(
            device_kind_from_form_factor(Some("handset")),
            DeviceKind::Phone
        );
    }

    #[test]
    fn form_factor_missing_or_unknown_falls_back_to_other() {
        assert_eq!(device_kind_from_form_factor(None), DeviceKind::Other);
        assert_eq!(
            device_kind_from_form_factor(Some("wonderful")),
            DeviceKind::Other
        );
    }

    // ---- pick_active_sink_id ----

    #[test]
    fn pick_active_sink_returns_none_for_no_candidates() {
        assert_eq!(pick_active_sink_id(&[]), None);
    }

    #[test]
    fn pick_active_sink_returns_none_when_nothing_is_running() {
        let candidates = &[(1u32, "alpha", false), (2, "beta", false)];
        assert_eq!(pick_active_sink_id(candidates), None);
    }

    #[test]
    fn pick_active_sink_picks_the_running_sink() {
        let candidates = &[
            (1u32, "alpha", false),
            (2, "beta", true),
            (3, "gamma", false),
        ];
        assert_eq!(pick_active_sink_id(candidates), Some(2));
    }

    #[test]
    fn pick_active_sink_tiebreaks_by_alphabetical_name() {
        // The deterministic tiebreak: among running sinks, the
        // alphabetically first name wins. The candidate ordering is
        // irrelevant — the function sorts.
        let a = &[
            (1u32, "charlie", true),
            (2, "alpha", true),
            (3, "bravo", true),
        ];
        assert_eq!(pick_active_sink_id(a), Some(2));
        let b = &[
            (2u32, "alpha", true),
            (3, "bravo", true),
            (1, "charlie", true),
        ];
        assert_eq!(pick_active_sink_id(b), Some(2));
    }

    #[test]
    fn pick_active_sink_ignores_non_running_even_with_earlier_name() {
        // A non-running "alpha" loses to a running "beta" — the running
        // filter is applied before the sort, so the sort never sees the
        // non-running candidate.
        let candidates = &[(1u32, "alpha", false), (2, "beta", true)];
        assert_eq!(pick_active_sink_id(candidates), Some(2));
    }

    // ---- parse_metadata_default_name ----

    #[test]
    fn parse_metadata_default_name_extracts_name_from_json() {
        let value = r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#;
        assert_eq!(
            parse_metadata_default_name(value).as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo")
        );
    }

    #[test]
    fn parse_metadata_default_name_handles_extra_whitespace() {
        let value = r#"{ "name" :   "my-sink"  }"#;
        assert_eq!(
            parse_metadata_default_name(value).as_deref(),
            Some("my-sink")
        );
    }

    #[test]
    fn parse_metadata_default_name_returns_none_for_missing_name() {
        assert_eq!(parse_metadata_default_name(r#"{"foo":"bar"}"#), None);
        assert_eq!(parse_metadata_default_name(""), None);
    }

    // ---- parse_props_pod ----

    /// The per-channel amplitudes and mute flag to serialize, each optional
    /// so a test can build the partial PODs PipeWire is free to push.
    struct TestProps {
        /// The per-channel linear amplitudes. The user-volume percent
        /// `pactl` reports is `cbrt(avg(channels)) * 100`; the tests store
        /// `x^3` per channel so the expected `cbrt` is `x`.
        channels: Option<Vec<f32>>,
        muted: Option<bool>,
    }

    impl pipewire::spa::pod::serialize::PodSerialize for TestProps {
        fn serialize<O: std::io::Write + std::io::Seek>(
            &self,
            serializer: pipewire::spa::pod::serialize::PodSerializer<O>,
        ) -> Result<
            pipewire::spa::pod::serialize::SerializeSuccess<O>,
            pipewire::spa::pod::serialize::GenError,
        > {
            use pipewire::spa::pod::PropertyFlags;
            use pipewire::spa::sys::{
                SPA_PARAM_Props, SPA_PROP_channelVolumes, SPA_PROP_mute, SPA_TYPE_OBJECT_Props,
            };

            let mut obj_serializer =
                serializer.serialize_object(SPA_TYPE_OBJECT_Props, SPA_PARAM_Props)?;
            if let Some(channels) = &self.channels {
                obj_serializer.serialize_property(
                    SPA_PROP_channelVolumes,
                    channels.as_slice(),
                    PropertyFlags::empty(),
                )?;
            }
            if let Some(muted) = &self.muted {
                obj_serializer.serialize_property(SPA_PROP_mute, muted, PropertyFlags::empty())?;
            }
            obj_serializer.end()
        }
    }

    /// Build a real `ParamType::Props` POD the same way wireplumber does,
    /// round-tripping through the libspa serialize machinery so the tests
    /// exercise the same deserialize path the producer uses.
    fn serialize_props(props: &TestProps) -> Vec<u8> {
        pipewire::spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            props,
        )
        .expect("serialize props")
        .0
        .into_inner()
    }

    #[test]
    fn spa_prop_keys_match_the_shipped_headers() {
        // Sanity: the constants the production code uses match the values
        // bindgen ships today. If a future libspa-sys bumps the header
        // version, the test catches it before the producer does.
        use pipewire::spa::sys::{SPA_PROP_channelVolumes, SPA_PROP_mute};
        assert_eq!(
            SPA_PROP_channelVolumes, 65544,
            "SPA_PROP_channelVolumes drifted"
        );
        assert_eq!(SPA_PROP_mute, 65540, "SPA_PROP_mute drifted");
    }

    #[test]
    fn parse_props_pod_round_trip_extracts_volume_and_mute() {
        // Wireplumber stores user-volume as `user^3` per channel; 0.5^3
        // = 0.125 per channel, average = 0.125, cbrt(0.125) = 0.5.
        let cubic = 0.5_f32.powi(3);
        let bytes = serialize_props(&TestProps {
            channels: Some(vec![cubic, cubic]),
            muted: Some(true),
        });
        assert_eq!(
            parse_props_pod(&bytes),
            Some(PropsUpdate {
                volume: Some(0.5),
                muted: Some(true),
            })
        );
    }

    #[test]
    fn parse_props_pod_accepts_a_volume_only_pod() {
        // PipeWire is free to push either property alone. Dropping such a
        // POD is what kept the pill invisible on a sink that had only
        // announced its level, so the parse has to survive it.
        let cubic = 0.3_f32.powi(3);
        let bytes = serialize_props(&TestProps {
            channels: Some(vec![cubic, cubic]),
            muted: None,
        });
        let parsed = parse_props_pod(&bytes).expect("volume-only POD parses");
        assert!((parsed.volume.expect("volume") - 0.3).abs() < 1e-5);
        assert_eq!(parsed.muted, None);
    }

    #[test]
    fn parse_props_pod_accepts_a_mute_only_pod() {
        // The mirror case: a mute toggle with no channel volumes must
        // update the mute flag and leave the stored level untouched.
        let bytes = serialize_props(&TestProps {
            channels: None,
            muted: Some(true),
        });
        assert_eq!(
            parse_props_pod(&bytes),
            Some(PropsUpdate {
                volume: None,
                muted: Some(true),
            })
        );
    }

    #[test]
    fn parse_props_pod_returns_none_when_neither_property_is_present() {
        // A Props POD that carries neither key tells us nothing, so it is
        // reported as unusable rather than merged as an empty update.
        let bytes = serialize_props(&TestProps {
            channels: None,
            muted: None,
        });
        assert_eq!(parse_props_pod(&bytes), None);
    }

    #[test]
    fn average_to_user_volume_inverts_wireplumbers_cubic_ramp() {
        // The wireplumber per-channel volume is the user-volume raised to
        // the third power. For a 50% user-volume the per-channel is
        // 0.5^3 = 0.125, and the inverse is `cbrt(0.125) = 0.5`.
        let channels = vec![0.5_f32.powi(3), 0.5_f32.powi(3)];
        let user_volume = average_to_user_volume(&channels).unwrap();
        assert!((user_volume - 0.5).abs() < 1e-5);

        // Same with 30% user-volume: 0.3^3 = 0.027 per channel, cbrt
        // recovers 0.3. This is the pactl `Volume: 19661 / 30%` reading
        // mapped back to the linear 0..=1 scale.
        let cubic = 0.3_f32.powi(3);
        let user_volume = average_to_user_volume(&[cubic, cubic]).unwrap();
        assert!((user_volume - 0.3).abs() < 1e-5);

        // An empty channel list returns None — the snapshot stays absent
        // until the server publishes real channels.
        assert_eq!(average_to_user_volume(&[]), None);

        // Non-finite and non-positive channel values are filtered out, so
        // a transient NaN/negative reading cannot poison the snapshot.
        let user_volume = average_to_user_volume(&[f32::NAN, 0.5_f32.powi(3)]).unwrap();
        assert!((user_volume - 0.5).abs() < 1e-5);
        let user_volume = average_to_user_volume(&[-0.1, 0.5_f32.powi(3)]).unwrap();
        assert!((user_volume - 0.5).abs() < 1e-5);
    }

    #[test]
    fn parse_props_pod_returns_none_for_garbage() {
        // Anything that does not parse as a `ParamType::Props` object is
        // skipped — a transient type drift never takes the source down.
        assert_eq!(parse_props_pod(&[]), None);
        assert_eq!(parse_props_pod(&[0xff; 16]), None);
    }

    // ---- compute_snapshot ----

    #[test]
    fn compute_snapshot_returns_none_for_no_sinks() {
        let state = Arc::new(Mutex::new(State::default()));
        assert!(compute_snapshot(&state).is_none());
    }

    #[test]
    fn compute_snapshot_uses_the_running_sink_when_available() {
        let mut state = State::default();
        state.sinks.insert(
            1,
            SinkEntry {
                name: "headphones".to_string(),
                device: DeviceKind::Headphones,
                running: true,
                volume: Some(0.5),
                muted: Some(false),
            },
        );
        state.sinks.insert(
            2,
            SinkEntry {
                name: "speakers".to_string(),
                device: DeviceKind::Speakers,
                running: false,
                volume: Some(0.3),
                muted: Some(true),
            },
        );
        let state = Arc::new(Mutex::new(state));
        let snap = compute_snapshot(&state).expect("snapshot");
        assert_eq!(snap.level(), 50);
        assert!(!snap.muted());
        assert_eq!(snap.device(), DeviceKind::Headphones);
    }

    #[test]
    fn compute_snapshot_falls_back_to_default_sink_when_none_running() {
        let mut state = State::default();
        state.sinks.insert(
            1,
            SinkEntry {
                name: "headphones".to_string(),
                device: DeviceKind::Headphones,
                running: false,
                volume: Some(0.5),
                muted: Some(false),
            },
        );
        state.sinks.insert(
            2,
            SinkEntry {
                name: "speakers".to_string(),
                device: DeviceKind::Speakers,
                running: false,
                volume: Some(0.3),
                muted: Some(true),
            },
        );
        state.default_sink_name = Some("speakers".to_string());
        let state = Arc::new(Mutex::new(state));
        let snap = compute_snapshot(&state).expect("snapshot");
        // The default-sink path is what wins when no sink is running.
        assert_eq!(snap.device(), DeviceKind::Speakers);
        assert!(snap.muted());
    }

    #[test]
    fn compute_snapshot_returns_none_when_sink_has_no_volume_yet() {
        // A sink that has not yet reported a Props POD has `volume: None`,
        // so the snapshot stays absent until the first parsing lands.
        let mut state = State::default();
        state.sinks.insert(
            1,
            SinkEntry {
                name: "speakers".to_string(),
                device: DeviceKind::Speakers,
                running: true,
                volume: None,
                muted: None,
            },
        );
        let state = Arc::new(Mutex::new(state));
        assert!(compute_snapshot(&state).is_none());
    }

    #[test]
    fn compute_snapshot_shows_a_sink_that_has_not_reported_mute_yet() {
        // A level with no mute flag is worth showing: treating the missing
        // flag as "not muted" beats hiding the pill until a mute POD
        // happens to arrive.
        let mut state = State::default();
        state.sinks.insert(
            1,
            SinkEntry {
                name: "speakers".to_string(),
                device: DeviceKind::Speakers,
                running: true,
                volume: Some(0.42),
                muted: None,
            },
        );
        let state = Arc::new(Mutex::new(state));
        let snap = compute_snapshot(&state).expect("snapshot");
        assert_eq!(snap.level(), 42);
        assert!(!snap.muted());
    }
}
