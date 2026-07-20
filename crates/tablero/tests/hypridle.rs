//! End-to-end message and interaction coverage without controlling the live daemon.

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero::producer::{ProducerBridge, from_fn};
use tablero::render::{Bounds, RenderContext};
use tablero::widget::{ClickButton, Command, Dashboard, Hypridle, HypridleWidget, Msg};

struct Harness {
    dashboard: Dashboard,
    seen: usize,
    redraws: usize,
}

#[test]
fn producer_messages_update_state_without_repainting_duplicates() {
    let mut harness = Harness {
        dashboard: Dashboard::new(vec![Box::new(HypridleWidget::new(Bounds::new(
            0, 0, 32, 32,
        )))]),
        seen: 0,
        redraws: 0,
    };
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().unwrap();
    let (bridge, channel) = ProducerBridge::new().unwrap();
    event_loop
        .handle()
        .insert_source(channel, |event, _, harness| {
            if let ChannelEvent::Msg(msg @ Msg::Hypridle(_)) = event {
                harness.seen += 1;
                harness.redraws += usize::from(harness.dashboard.update(&msg));
            }
        })
        .unwrap();
    bridge.spawn(from_fn("fake-hypridle", |tx| async move {
        tx.send(Msg::Hypridle(Hypridle::new(true)))?;
        tx.send(Msg::Hypridle(Hypridle::new(true)))?;
        tx.send(Msg::Hypridle(Hypridle::new(false)))?;
        Ok(())
    }));

    let mut waited = Duration::ZERO;
    while harness.seen < 3 && waited < Duration::from_secs(2) {
        event_loop
            .dispatch(Duration::from_millis(20), &mut harness)
            .unwrap();
        waited += Duration::from_millis(20);
    }
    assert_eq!(harness.seen, 3);
    assert_eq!(harness.redraws, 2);
}

#[test]
fn sampled_state_drives_the_dashboard_toggle_command() {
    let mut dashboard = Dashboard::new(vec![Box::new(HypridleWidget::new(Bounds::new(
        0, 0, 32, 32,
    )))]);
    dashboard.update(&Msg::Hypridle(Hypridle::new(true)));
    let mut ctx = RenderContext::new(64, 32);
    dashboard.layout(&mut ctx, 64, 32);

    assert_eq!(
        dashboard.on_click(10, 10, ClickButton::Left),
        Some(Command::SetHypridle(false))
    );
}
