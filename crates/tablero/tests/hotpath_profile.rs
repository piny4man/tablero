//! The opt-in `hotpath` build profiles the render path.
//!
//! Compiled only with `--features hotpath`: paints a few offscreen frames
//! under a profiling guard and checks the report names every render phase, so
//! an instrumentation attribute lost in a refactor fails here instead of
//! silently thinning the next profiling run.
#![cfg(feature = "hotpath")]

use chrono::Local;
use tablero::blit::write_argb8888;
use tablero::config::Config;
use tablero::render::{Bounds, RenderContext};
use tablero::scale::Scale;
use tablero::widget::{Msg, SystemStats, Workspaces};

#[test]
fn the_report_names_every_render_phase() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let guard = hotpath::HotpathGuardBuilder::new("hotpath_profile")
        .format(hotpath::Format::Json)
        .output_path(&report)
        .functions_limit(0)
        .build();

    let config = Config::default();
    let scale = Scale::new(1);
    let (width, height) = (800, config.physical_height(scale));
    let mut dashboard = config.build_dashboard(Bounds::new(0, 0, width, height), None);
    let mut ctx = RenderContext::with_settings(width, height, config.scaled_render_settings(scale));
    let mut canvas = vec![0_u8; (width * height * 4) as usize];
    dashboard.update(&Msg::Tick(Local::now()));
    dashboard.update(&Msg::Workspaces(Workspaces::new([1, 2, 3], 2)));
    for frame in 0..3 {
        dashboard.update(&Msg::System(SystemStats::new(f64::from(frame), 50.0)));
        dashboard.layout(&mut ctx, width, height);
        dashboard.draw(&mut ctx);
        write_argb8888(ctx.pixels(), &mut canvas);
    }
    drop(guard);

    let json = std::fs::read_to_string(&report).unwrap();
    for name in [
        "Dashboard::layout",
        "Dashboard::draw",
        "RenderContext::measure_text",
        "write_argb8888",
    ] {
        assert!(json.contains(name), "{name} missing from report:\n{json}");
    }
}
