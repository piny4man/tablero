//! Integration coverage for the system-stats path, without sampling the machine.
//!
//! Drives a real `calloop` event loop wired exactly as the bar wires it: a
//! [`ProducerBridge`] runs a fake producer that emits [`Msg::System`] samples
//! built from the same pure parsers the live producer uses ([`parse_cpu_times`],
//! [`cpu_usage`], [`parse_mem_usage`]), and the resulting messages cross a
//! calloop channel into the same app-state update path the render loop uses
//! ([`Dashboard::update`]). This proves sampled procfs text reaches renderable
//! widget state — exercising every layer except the procfs read itself.

use std::time::Duration;

use calloop::EventLoop;
use calloop::channel::Event as ChannelEvent;
use tablero::producer::{ProducerBridge, from_fn};
use tablero::render::Bounds;
use tablero::sysmon::{cpu_usage, parse_cpu_times, parse_mem_usage};
use tablero::widget::{Dashboard, Msg, SystemStats, SystemWidget};

/// One fake sample: the `/proc/stat` and `/proc/meminfo` text for this tick.
type Sample = (&'static str, &'static str);

/// Stands in for the bar: owns a system-stats widget and records what it rendered.
struct Harness {
    dashboard: Dashboard,
    labels: Vec<String>,
    redraws: usize,
}

impl Harness {
    fn new() -> Self {
        Self {
            dashboard: Dashboard::new(vec![Box::new(SystemWidget::new(Bounds::new(
                0, 0, 320, 32,
            )))]),
            labels: Vec::new(),
            redraws: 0,
        }
    }
}

/// Pump the loop until `done(&harness)` holds or the timeout elapses.
fn pump_until(
    event_loop: &mut EventLoop<Harness>,
    harness: &mut Harness,
    done: impl Fn(&Harness) -> bool,
) {
    let mut waited = Duration::ZERO;
    let step = Duration::from_millis(20);
    while !done(harness) && waited < Duration::from_secs(5) {
        event_loop
            .dispatch(step, harness)
            .expect("loop dispatch succeeds");
        waited += step;
    }
}

/// Build a normalized snapshot from a pair of consecutive procfs samples, the
/// same way the live producer folds two readings into one emitted message.
fn stats_between(prev: Sample, cur: Sample) -> SystemStats {
    let prev_cpu = parse_cpu_times(prev.0).expect("valid prev /proc/stat");
    let cur_cpu = parse_cpu_times(cur.0).expect("valid cur /proc/stat");
    let cpu = cpu_usage(prev_cpu, cur_cpu);
    let mem = parse_mem_usage(cur.1).expect("valid /proc/meminfo");
    SystemStats::new(cpu, mem)
}

/// Run a sequence of already-normalized samples through the bridge and return the
/// labels the widget rendered plus how many samples forced a redraw.
fn drive(samples: Vec<SystemStats>) -> (Vec<String>, usize) {
    let mut harness = Harness::new();
    let mut event_loop: EventLoop<Harness> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let (bridge, channel) = ProducerBridge::new().expect("runtime starts");
    let expected = samples.len();
    handle
        .insert_source(channel, |event, _, h: &mut Harness| {
            if let ChannelEvent::Msg(msg) = event {
                if h.dashboard.update(&msg) {
                    h.redraws += 1;
                }
                if let Msg::System(stats) = &msg {
                    h.labels.push(stats.label());
                }
            }
        })
        .expect("channel registers");

    // A fake "procfs" producer: emits each pre-normalized sample, exactly the
    // messages the live producer would send after reading the bus of files.
    bridge.spawn(from_fn("fake-sysmon", move |tx| async move {
        for stats in samples {
            tx.send(Msg::System(stats))?;
        }
        Ok(())
    }));

    pump_until(&mut event_loop, &mut harness, |h| {
        h.labels.len() >= expected
    });

    drop(bridge);
    (harness.labels, harness.redraws)
}

// Two consecutive `/proc/stat` readings 100 jiffies apart, 25 of them idle:
// 75% busy between them.
const STAT_A: &str = "cpu  100 0 50 800 30 0 20 0 0 0\ncpu0 50 0 25 400 15 0 10\n";
const STAT_B: &str = "cpu  150 0 70 825 30 0 25 0 0 0\ncpu0 75 0 35 412 15 0 12\n";
const MEMINFO: &str = "MemTotal: 16000000 kB\nMemAvailable: 8000000 kB\n";

#[test]
fn sampled_procfs_text_reaches_rendered_widget_state() {
    // STAT_A -> STAT_B: total delta 100, idle delta 25 -> 75% CPU; memory 50%.
    let stats = stats_between((STAT_A, MEMINFO), (STAT_B, MEMINFO));
    let (labels, redraws) = drive(vec![stats]);

    assert_eq!(labels, vec!["CPU 75% MEM 50%".to_string()]);
    assert_eq!(redraws, 1, "the first sample paints once");
}

#[test]
fn a_changed_sample_repaints_an_unchanged_one_does_not() {
    let first = stats_between((STAT_A, MEMINFO), (STAT_B, MEMINFO)); // CPU 75% MEM 50%
    // An identical follow-up (same readings) normalizes to the same percents,
    // then a different memory reading moves a visible value.
    let same = SystemStats::new(75.0, 50.0);
    let moved = SystemStats::new(75.0, 63.0);
    let (labels, redraws) = drive(vec![first, same, moved]);

    assert_eq!(
        labels,
        vec![
            "CPU 75% MEM 50%".to_string(),
            "CPU 75% MEM 50%".to_string(),
            "CPU 75% MEM 63%".to_string(),
        ]
    );
    // First sample paints, the identical one is filtered, the moved one repaints.
    assert_eq!(redraws, 2, "only visible changes repaint");
}
