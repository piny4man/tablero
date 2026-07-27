//! End-to-end test of the volume producer: spawns the real producer
//! through a real `ProducerBridge`, drains the channel, and prints every
//! `Msg::Volume` it sees. Compare against `pactl get-sink-volume` to
//! confirm the producer is emitting on every change.
//!
//! Takes an optional run length in seconds (default 6) — long enough to watch
//! the reconnect backoff when the PipeWire daemon is stopped:
//!
//! ```text
//! RUST_LOG=tablero=debug cargo run --example volume-bridge -- 30
//! ```

use std::time::{Duration, Instant};

use calloop::EventLoop;
use calloop::channel::Event;
use tablero::producer::ProducerBridge;
use tablero::volume::VolumeProducer;
use tablero::widget::Msg;

fn main() {
    // The producer's own diagnostics (connect, handshake, reconnect) are the
    // interesting half when the daemon is missing, so honour `RUST_LOG`.
    env_logger::init();

    let seconds: u64 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(6);

    let mut harness = EventLoop::<Vec<Msg>>::try_new().expect("event loop");
    let (bridge, channel) = ProducerBridge::new().expect("bridge");
    let _source = harness
        .handle()
        .insert_source(
            channel,
            |event: Event<Msg>, _: &mut (), msgs: &mut Vec<Msg>| {
                if let Event::Msg(msg) = event {
                    msgs.push(msg);
                }
            },
        )
        .expect("channel registers");
    bridge.spawn(Box::new(VolumeProducer::new()));

    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    let step = Duration::from_millis(50);
    let mut msgs: Vec<Msg> = Vec::new();
    let mut printed = 0;
    while Instant::now() < deadline {
        harness.dispatch(Some(step), &mut msgs).expect("dispatch");
        // Print as they land, interleaved with the producer's log lines — a
        // batch dumped at exit cannot show *when* a reconnect delivered.
        for msg in &msgs[printed..] {
            if let Msg::Volume(snap) = msg {
                eprintln!(
                    "[bridge] +{:.1}s Msg::Volume: {snap:?}",
                    start.elapsed().as_secs_f32()
                );
            }
        }
        printed = msgs.len();
    }
}
