//! Offscreen frame-cost probe: paints the configured bar into a pixmap and
//! prints per-phase timings, without opening a Wayland surface.
//!
//! The bar's real frame cost is measured with `TABLERO_PERF=1` (see
//! `docs/interaction-latency.md`); this is the quick loop for comparing two
//! builds or power profiles on identical input:
//!
//! ```text
//! cargo run --release -p tablero --example frame-cost -- [config.toml] [scale] [idle-ms]
//! ```
//!
//! A tight loop keeps the core clocked up and the caches warm, which flatters
//! the balanced and power-saver profiles. Pass `idle-ms` (e.g. `500`) to sleep
//! between frames, so each one is the cold burst after idle a real bar paints.
//!
//! Each frame changes the system widget's value, as a live bar's most frequent
//! redraw does, so the numbers include the text-cache misses that causes.

use std::time::{Duration, Instant};

use chrono::Local;
use tablero::blit::write_argb8888;
use tablero::config::Config;
use tablero::render::{Bounds, RenderContext};
use tablero::scale::Scale;
use tablero::widget::{Battery, BatteryState, Msg, SystemStats, Workspaces};

const LOGICAL_WIDTH: u32 = 1920;
/// Frames painted back to back, and with an idle gap between them.
const FRAMES: u32 = 200;
const IDLE_FRAMES: u32 = 40;

fn percentile(sorted: &[Duration], pct: usize) -> Duration {
    sorted[(sorted.len() * pct / 100).min(sorted.len() - 1)]
}

fn report(name: &str, samples: &mut [Duration]) {
    samples.sort_unstable();
    println!(
        "{name:<8} median={:>7.3}ms p95={:>7.3}ms max={:>7.3}ms",
        percentile(samples, 50).as_secs_f64() * 1e3,
        percentile(samples, 95).as_secs_f64() * 1e3,
        samples[samples.len() - 1].as_secs_f64() * 1e3,
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let config = match args.next() {
        Some(path) => Config::load_from_path(path)?,
        None => Config::default(),
    };
    let scale = Scale::new(args.next().map_or(Ok(2), |s| s.parse())?);
    let idle = Duration::from_millis(args.next().map_or(Ok(0), |s| s.parse())?);
    let frames = if idle.is_zero() { FRAMES } else { IDLE_FRAMES };

    let settings = config.scaled_render_settings(scale);
    let width = scale.to_physical(LOGICAL_WIDTH);
    let height = config.physical_height(scale);
    let mut dashboard = config.build_dashboard(Bounds::new(0, 0, width, height), None);
    let mut ctx = RenderContext::with_settings(width, height, settings);
    let mut canvas = vec![0_u8; (width * height * 4) as usize];

    dashboard.update(&Msg::Tick(Local::now()));
    dashboard.update(&Msg::Workspaces(Workspaces::new([1, 2, 3, 4, 5], 2)));
    dashboard.update(&Msg::Battery(Some(Battery::new(
        BatteryState::Discharging,
        73.0,
    ))));

    let (mut layout, mut draw, mut blit, mut render) = (vec![], vec![], vec![], vec![]);
    for frame in 0..frames {
        std::thread::sleep(idle);
        dashboard.update(&Msg::System(SystemStats::new(
            f64::from(frame % 100),
            f64::from((frame * 7) % 100),
        )));

        let started = Instant::now();
        dashboard.layout(&mut ctx, width, height);
        let laid_out = Instant::now();
        dashboard.draw(&mut ctx);
        let drawn = Instant::now();
        write_argb8888(ctx.pixels(), &mut canvas);
        let done = Instant::now();

        layout.push(laid_out - started);
        draw.push(drawn - laid_out);
        blit.push(done - drawn);
        render.push(done - started);
    }

    // The first frame loads font faces and shapes every label from cold; it is
    // a startup cost, reported apart so it does not pose as a steady-state max.
    println!("{width}x{height} physical, {frames} frames, {idle:?} idle between");
    println!(
        "first frame: layout={:.3}ms render={:.3}ms",
        layout.remove(0).as_secs_f64() * 1e3,
        render.remove(0).as_secs_f64() * 1e3,
    );
    draw.remove(0);
    blit.remove(0);
    report("layout", &mut layout);
    report("draw", &mut draw);
    report("blit", &mut blit);
    report("render", &mut render);
    let stats = ctx.take_stats();
    println!(
        "text_shapes={} text_cache_hits={} glyph_pixels={}",
        stats.text_shapes, stats.text_cache_hits, stats.glyph_pixels
    );
    Ok(())
}
