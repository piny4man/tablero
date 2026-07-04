//! Tiny diagnostic that connects to the running PipeWire server, lists audio
//! sinks, and prints the parsed volume / mute / device kind for each.
//!
//! This is a sanity check for the volume producer's parsing path against a
//! real PipeWire server — it exercises the same parse logic the producer
//! uses, just without running the timer loop.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use pipewire::context::ContextRc;
use pipewire::main_loop::MainLoopRc;
use pipewire::node::{Node, NodeInfoRef};
use pipewire::proxy::Listener;
use pipewire::spa::pod::Pod;
use pipewire::spa::sys::{SPA_PROP_mute, SPA_PROP_volume};
use pipewire::types::ObjectType;
use tablero_wayland::volume::device_kind_from_props;

fn parse_props(pod: &Pod) -> Option<(f32, bool)> {
    let obj = pod.as_object().ok()?;
    let volume_pod = obj.find_prop(pipewire::spa::utils::Id(SPA_PROP_volume))?;
    let mute_pod = obj.find_prop(pipewire::spa::utils::Id(SPA_PROP_mute))?;
    let volume = volume_pod.value().get_float().ok()?;
    let mute = mute_pod.value().get_bool().ok()?;
    Some((volume, mute))
}

fn main() {
    pipewire::init();
    let mainloop = MainLoopRc::new(None).expect("main loop");
    let context = ContextRc::new(&mainloop, None).expect("context");
    let core = context.connect_rc(None).expect("core");
    let registry = core.get_registry_rc().expect("registry");
    let registry_weak = registry.downgrade();

    // Per-Node listeners must outlive the closure, the same way pw-mon's
    // `Proxies` struct owns both the bound Node proxies and their
    // listeners. The whole thing stays on this single thread (PipeWire's
    // main loop is synchronous and the listener trait is `!Send`), so a
    // thread-local `Rc<RefCell<Vec<…>>>` is the right level of sharing.
    // The bound Node proxies are stored alongside: a listener's internal
    // hook references the Node, so dropping the Node invalidates the
    // listener.
    let node_proxies: Rc<RefCell<Vec<Node>>> = Rc::new(RefCell::new(Vec::new()));
    let node_proxies_for_cb = node_proxies.clone();
    let node_listeners: Rc<RefCell<Vec<Box<dyn Listener>>>> = Rc::new(RefCell::new(Vec::new()));
    let node_listeners_for_cb = node_listeners.clone();

    let sink_count: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
    let sink_count_for_cb = sink_count.clone();

    let _registry_listener = registry
        .add_listener_local()
        .global(move |obj| {
            if obj.type_ != ObjectType::Node {
                return;
            }
            let Some(props) = obj.props else { return };
            if props.get("media.class") != Some("Audio/Sink") {
                return;
            }
            *sink_count_for_cb.borrow_mut() += 1;

            let id = obj.id;
            let name = props.get("node.name").unwrap_or("?").to_string();
            let desc = props.get("node.description").unwrap_or("?").to_string();
            println!(
                "sink id={id} name={name:?} desc={desc:?} (registry-global props: icon={:?} form={:?})",
                props.get("device.icon-name"),
                props.get("device.form-factor"),
            );
            let Some(registry) = registry_weak.upgrade() else { return };
            let node: Node = match registry.bind(obj) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("  bind failed: {e}");
                    return;
                }
            };
            println!("  bound node {id}");
            let node_listener = node
                .add_listener_local()
                .info(move |info: &NodeInfoRef| {
                    let info_props = info.props();
                    let kind = info_props
                        .map(device_kind_from_props)
                        .unwrap_or(tablero_core::widget::DeviceKind::Other);
                    let icon = info_props.and_then(|p| p.get("device.icon-name"));
                    let form = info_props.and_then(|p| p.get("device.form-factor"));
                    println!(
                        "  info: state={:?} kind={kind:?} icon={icon:?} form={form:?}",
                        info.state()
                    );
                })
                .param(move |_seq, _id, _index, _next, param: Option<&Pod>| {
                    if let Some(p) = param {
                        if let Some((vol, mute)) = parse_props(p) {
                            println!("  param: volume={vol} mute={mute}");
                        } else {
                            println!("  param: (unparseable Props POD)");
                        }
                    }
                })
                .register();
            node_listeners_for_cb
                .borrow_mut()
                .push(Box::new(node_listener));
            node_proxies_for_cb.borrow_mut().push(node);
        })
        .register();

    // Let the main loop process events for a short window, then quit.
    let ml_weak = mainloop.downgrade();
    let timer = mainloop.loop_().add_timer(move |_| {
        if let Some(ml) = ml_weak.upgrade() {
            ml.quit();
        }
    });
    timer.update_timer(Some(Duration::from_secs(2)), Some(Duration::from_secs(2)));
    mainloop.run();

    if *sink_count.borrow() == 0 {
        eprintln!("No audio sinks found.");
    }
}
