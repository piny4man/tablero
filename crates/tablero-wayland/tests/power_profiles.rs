//! Producer-bridge coverage for power-profiles-daemon state and click rotation.

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero_core::render::Bounds;
use tablero_core::widget::{
    ClickButton, Command, Dashboard, Msg, PowerProfile, PowerProfilesState, PowerProfilesWidget,
};
use tablero_wayland::producer::{ProducerBridge, from_fn};

struct Harness {
    dashboard: Dashboard,
    messages: usize,
    redraws: usize,
}

fn snapshot(active: &str) -> PowerProfilesState {
    PowerProfilesState::new(
        active,
        vec![
            PowerProfile::new("power-saver", "multiple", "amd_pstate", "placeholder"),
            PowerProfile::new("balanced", "multiple", "amd_pstate", "placeholder"),
            PowerProfile::new("performance", "amd_pstate", "amd_pstate", "amd_pstate"),
        ],
    )
}

#[test]
fn snapshots_cross_the_bridge_and_drive_click_rotation() {
    let widget = PowerProfilesWidget::new(Bounds::new(0, 0, 64, 32));
    let mut harness = Harness {
        dashboard: Dashboard::new(vec![Box::new(widget)]),
        messages: 0,
        redraws: 0,
    };
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let (bridge, channel) = ProducerBridge::new().expect("runtime");
    event_loop
        .handle()
        .insert_source(channel, |event, _, harness| {
            if let ChannelEvent::Msg(msg) = event {
                harness.messages += 1;
                harness.redraws += usize::from(harness.dashboard.update(&msg));
            }
        })
        .expect("channel");
    bridge.spawn(from_fn("fake-power-profiles", |tx| async move {
        tx.send(Msg::PowerProfiles(Some(snapshot("balanced"))))?;
        tx.send(Msg::PowerProfiles(Some(snapshot("performance"))))?;
        Ok(())
    }));

    let mut waited = Duration::ZERO;
    while harness.messages < 2 && waited < Duration::from_secs(5) {
        event_loop
            .dispatch(Duration::from_millis(20), &mut harness)
            .expect("dispatch");
        waited += Duration::from_millis(20);
    }
    assert_eq!(harness.messages, 2);
    assert_eq!(harness.redraws, 2);
    assert_eq!(
        harness.dashboard.on_click(20, 16, ClickButton::Left),
        Some(Command::SetPowerProfile("power-saver".into()))
    );
}
