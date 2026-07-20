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
//!    configured default audio sink in the `default.configured.audio.sink`
//!    metadata — i.e. what the user has actually told the system to use.
//! 3. If neither resolves, emit `Msg::Volume(None)` and `warn!` — a fresh
//!    bar without a usable audio source reserves no slot for the volume
//!    widget, matching the absent-source behavior of the `network` and
//!    `system` widgets.
//!
//! Polling (rather than per-property signal subscriptions) is what keeps the
//! implementation tractable: the volume can change at any time (key press,
//! GUI slider, app that auto-balances audio), so a static signal subscription
//! would miss many of those events. A 2-second ticker reads the cached state
//! and only re-emits when the normalized snapshot actually differs.
//!
//! # Pure parsing
//!
//! Device-kind classification and active-sink selection are pure functions
//! tests drive directly with strings and slices — no PipeWire server needed.
//! The SPA POD decoding for the volume/mute Props is a thin FFI wrapper around
//! [`Pod::get_float`] / [`Pod::get_bool`] and is exercised by the integration
//! test in `tests/volume.rs`, not the unit tests.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use log::debug;
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

/// Extract `(volume, mute)` from a PipeWire `ParamType::Props` POD.
///
/// `pod_bytes` is the raw byte slice of a `spa_pod` whose body is a
/// `SPA_TYPE_OBJECT_Props` (the `ParamType::Props` object). The function
/// looks up the `channelVolumes` (per-channel Float array) and `mute`
/// (Bool) properties and returns the parsed values. Returns `None` for any
/// malformed input — a transient type drift never takes the source down.
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
pub fn parse_props_pod(pod_bytes: &[u8]) -> Option<(f32, bool)> {
    use pipewire::spa::pod::Value;
    use pipewire::spa::sys::{SPA_PROP_channelVolumes, SPA_PROP_mute};

    let pod = Pod::from_bytes(pod_bytes)?;
    let obj = pod.as_object().ok()?;

    // `SPA_PROP_channelVolumes` is a per-channel Float array. The
    // libspa-sys bindgen output pins the key at `0x10008`; the test in
    // this file asserts the wire format to catch a header / bindgen
    // drift.
    let channel_volumes_pod = obj.find_prop(pipewire::spa::utils::Id(SPA_PROP_channelVolumes))?;
    let mute_pod = obj.find_prop(pipewire::spa::utils::Id(SPA_PROP_mute))?;

    // Deserialize the channel-volumes value into a `Value` enum so we
    // can pull the per-channel Float array out of whatever shape the
    // serializer chose.
    let value: Value = pipewire::spa::pod::deserialize::PodDeserializer::deserialize_any_from(
        channel_volumes_pod.value().as_bytes(),
    )
    .ok()
    .map(|(_, v)| v)?;
    // Compute the user-volume from the per-channel Float array (or a
    // single Float for mono sinks). The `Value` owns the data, so we
    // borrow into it; binding `value` to a name keeps it alive for the
    // duration of the borrow.
    let user_volume = match &value {
        Value::ValueArray(pipewire::spa::pod::ValueArray::Float(v)) => average_to_user_volume(v)?,
        Value::Float(v) => average_to_user_volume(&[*v])?,
        _ => return None,
    };
    let mute = mute_pod.value().get_bool().ok()?;
    Some((user_volume, mute))
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

/// Cached state for one PipeWire sink node.
#[derive(Clone)]
struct SinkEntry {
    name: String,
    device: DeviceKind,
    running: bool,
    /// `(volume, mute)` from the last Props POD we successfully parsed.
    volume: Option<(f32, bool)>,
}

/// A [`Producer`] that polls PipeWire for the active output sink and emits
/// [`Msg::Volume`] snapshots on every change.
///
/// Construct with [`new`](VolumeProducer::new) and hand it to the producer
/// bridge; it spawns a dedicated PipeWire main-loop thread on [`run`](Producer::run),
/// reads an initial snapshot, then re-emits on every tick until the system
/// PipeWire server closes or the render loop shuts down.
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

/// Drive the producer: bring up the PipeWire main loop on a dedicated OS
/// thread, seed the bar with the first reading, and re-emit on every tick.
///
/// A failed system-PipeWire connection propagates as an error the bridge logs
/// and isolates — the bar keeps running, the volume widget simply stays
/// blank. A per-tick error (malformed POD, dropped metadata, …) degrades to
/// [`Msg::Volume(None)`] and `warn!`s; the next tick retries the live read.
async fn run(tx: MsgSender, period: Duration) -> ProducerResult {
    let (done_tx, done_rx) = oneshot::channel();
    let tx_for_thread = tx.clone();
    thread::Builder::new()
        .name("tablero-volume".to_string())
        .spawn(move || {
            // The thread owns the PipeWire main loop and exits when the loop
            // terminates (server disconnect, OS error, …). The result
            // travels back via the oneshot so the producer bridge can log it
            // like any other producer failure.
            let result = run_main_loop(tx_for_thread, period);
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

/// Run the PipeWire main loop until the server goes away or an unrecoverable
/// error surfaces.
///
/// Every read of PipeWire state is funneled through the shared
/// [`Arc<Mutex<State>>`]; a periodic timer re-evaluates the active sink and
/// emits a [`Msg::Volume`] if the normalized snapshot changed.
fn run_main_loop(tx: MsgSender, period: Duration) -> ProducerResult {
    // Initialize PipeWire once. The `init`/`deinit` pair is a global
    // reference count; calling it per thread is safe, and the static
    // refcount means a leak-free shutdown is the OS's job when the
    // process exits.
    pipewire::init();

    let mainloop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;

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
    let timer = mainloop.loop_().add_timer(move |_expirations| {
        on_tick(&state_for_timer, &tx_for_timer);
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
#[derive(Default)]
struct State {
    sinks: HashMap<u32, SinkEntry>,
    default_sink_name: Option<String>,
    last_emitted: Option<Option<Volume>>,
}

/// Compute the snapshot to emit on a tick and send it through the
/// cross-thread `MsgSender` if (and only if) it differs from the last one.
///
/// A re-emit happens when the normalized [`Volume`] value changes (level,
/// mute, or device kind) or when the source goes from absent to present (or
/// vice versa). Identical snapshots are dropped, so a steady-state machine
/// costs one PipeWire event and zero render-loop work per tick.
fn on_tick(state: &Arc<Mutex<State>>, tx: &MsgSender) {
    let snapshot = compute_snapshot(state);
    let changed = {
        let state = state.lock().expect("volume state poisoned");
        state.last_emitted.as_ref() != Some(&snapshot)
    };
    if changed {
        {
            let mut state = state.lock().expect("volume state poisoned");
            state.last_emitted = Some(snapshot);
        }
        if tx.send(Msg::Volume(snapshot)).is_err() {
            debug!("volume: tx.send returned Closed");
        }
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
/// A sink that has not yet reported a Props POD (the `enum_params` round-
/// trip is still in flight) reports the bar as "absent" until the first
/// parsing succeeds, so a sink appearing in the registry does not flip
/// the widget to `Vol 0%` and back once the real reading lands.
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

    let (level, muted) = entry.volume?;
    Some(Volume::new(level, muted, entry.device))
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
    tx: MsgSender,
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
                    let parsed = parse_props_pod(param.as_bytes());
                    let Some((level, muted)) = parsed else {
                        // A malformed POD is not a hard failure — just skip
                        // this tick for the sink; the next param event will
                        // retry.
                        return;
                    };
                    {
                        let mut s = state_for_param.lock().expect("volume state poisoned");
                        if let Some(entry) = s.sinks.get_mut(&id) {
                            entry.volume = Some((level, muted));
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
    tx: MsgSender,
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
                Err(_) => return,
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

    #[test]
    fn parse_props_pod_round_trip_extracts_volume_and_mute() {
        // Build a real `ParamType::Props` POD the same way wireplumber does,
        // round-trip through the libspa serialize / deserialize machinery.
        // A future bindgen / header drift that renumbers the property keys
        // will fail this test loudly.
        use pipewire::spa::pod::PropertyFlags;
        use pipewire::spa::pod::serialize::{GenError, PodSerialize, PodSerializer};
        use pipewire::spa::sys::{
            SPA_PARAM_Props, SPA_PROP_channelVolumes, SPA_PROP_mute, SPA_TYPE_OBJECT_Props,
        };
        use std::io::Cursor;

        struct MyProps {
            /// The per-channel linear amplitudes. The user-volume percent
            /// `pactl` reports is `cbrt(avg(channels)) * 100`; this test
            /// stores `0.5^3` per channel so the expected `cbrt` is 0.5.
            channels: Vec<f32>,
            muted: bool,
        }

        impl PodSerialize for MyProps {
            fn serialize<O: std::io::Write + std::io::Seek>(
                &self,
                serializer: PodSerializer<O>,
            ) -> Result<pipewire::spa::pod::serialize::SerializeSuccess<O>, GenError> {
                let mut obj_serializer =
                    serializer.serialize_object(SPA_TYPE_OBJECT_Props, SPA_PARAM_Props)?;
                obj_serializer.serialize_property(
                    SPA_PROP_channelVolumes,
                    self.channels.as_slice(),
                    PropertyFlags::empty(),
                )?;
                obj_serializer.serialize_property(
                    SPA_PROP_mute,
                    &self.muted,
                    PropertyFlags::empty(),
                )?;
                obj_serializer.end()
            }
        }

        // Sanity: the constants the production code uses match the values
        // bindgen ships today. If a future libspa-sys bumps the header
        // version, the test catches it before the producer does.
        assert_eq!(
            SPA_PROP_channelVolumes, 65544,
            "SPA_PROP_channelVolumes drifted"
        );
        assert_eq!(SPA_PROP_mute, 65540, "SPA_PROP_mute drifted");

        // Wireplumber stores user-volume as `user^3` per channel; 0.5^3
        // = 0.125 per channel, average = 0.125, cbrt(0.125) = 0.5.
        let cubic = 0.5_f32.powi(3);
        let props = MyProps {
            channels: vec![cubic, cubic],
            muted: true,
        };
        let bytes = PodSerializer::serialize(Cursor::new(Vec::new()), &props)
            .unwrap()
            .0
            .into_inner();
        let parsed = parse_props_pod(&bytes);
        assert_eq!(parsed, Some((0.5, true)));
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
                volume: Some((0.5, false)),
            },
        );
        state.sinks.insert(
            2,
            SinkEntry {
                name: "speakers".to_string(),
                device: DeviceKind::Speakers,
                running: false,
                volume: Some((0.3, true)),
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
                volume: Some((0.5, false)),
            },
        );
        state.sinks.insert(
            2,
            SinkEntry {
                name: "speakers".to_string(),
                device: DeviceKind::Speakers,
                running: false,
                volume: Some((0.3, true)),
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
            },
        );
        let state = Arc::new(Mutex::new(state));
        assert!(compute_snapshot(&state).is_none());
    }
}
