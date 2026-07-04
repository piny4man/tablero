//! End-to-end test of the volume producer: spawns the real producer
//! through a real `ProducerBridge`, drains the channel, and prints every
//! `Msg::Volume` it sees. Compare against `pactl get-sink-volume` to
//! confirm the producer is emitting on every change.

use std::time::{Duration, Instant};

use calloop::EventLoop;
use calloop::channel::Event;
use tablero_core::widget::Msg;
use tablero_wayland::producer::ProducerBridge;
use tablero_wayland::volume::VolumeProducer;

fn main() {
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

    let deadline = Instant::now() + Duration::from_secs(6);
    let step = Duration::from_millis(50);
    let mut msgs: Vec<Msg> = Vec::new();
    while Instant::now() < deadline {
        harness.dispatch(Some(step), &mut msgs).expect("dispatch");
    }

    for msg in &msgs {
        if let Msg::Volume(snap) = msg {
            eprintln!("[bridge] Msg::Volume: {snap:?}");
        }
    }
}
