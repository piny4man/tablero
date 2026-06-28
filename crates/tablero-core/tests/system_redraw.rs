//! Integration coverage for the system-stats message → app-state → redraw path.
//!
//! Exercises the public widget architecture exactly as the Wayland host loop
//! does: build a [`Dashboard`] holding a [`SystemWidget`], feed it
//! [`Msg::System`] samples, and assert the dirty-flag the host uses to decide
//! whether to repaint — then confirm the sampled stats actually reach renderable
//! widget state.

use tablero_core::render::{Bounds, RenderContext};
use tablero_core::widget::{Dashboard, Msg, SystemStats, SystemWidget};

fn sample(cpu: f64, mem: f64) -> Msg {
    Msg::System(SystemStats::new(cpu, mem))
}

/// A sample that changes a visible whole percent yields a redraw decision of
/// `true`; one that normalizes to the same screen value yields `false`.
#[test]
fn system_sample_drives_exact_redraw_decision() {
    let mut dash = Dashboard::new(vec![Box::new(SystemWidget::new(Bounds::new(
        0, 0, 320, 32,
    )))]);

    // From the empty initial state, the first sample is a visible change.
    assert!(
        dash.update(&sample(12.0, 47.0)),
        "first sample must request a redraw"
    );

    // Sub-percent jitter normalizes to the same whole percents → no redraw.
    assert!(
        !dash.update(&sample(12.3, 47.4)),
        "an unchanged whole-percent sample must not request a redraw"
    );

    // Crossing into a new whole percent changes the label → redraw again.
    assert!(
        dash.update(&sample(30.0, 47.0)),
        "a new CPU reading must request a redraw"
    );

    // Memory moving is just as visible → redraw.
    assert!(
        dash.update(&sample(30.0, 63.0)),
        "a new memory reading must request a redraw"
    );
}

/// A sampled snapshot reaches renderable widget state: after an update the
/// widget paints foreground glyphs over the dark background.
#[test]
fn sampled_stats_appear_in_rendered_output() {
    let mut dash = Dashboard::new(vec![Box::new(SystemWidget::new(Bounds::new(
        0, 0, 320, 32,
    )))]);
    let mut ctx = RenderContext::new(320, 32);

    // Before any sample, the slot is blank: every pixel is the dark background.
    dash.draw(&mut ctx);
    assert!(
        ctx.pixels()
            .chunks_exact(4)
            .all(|p| p[0] < 0x30 && p[1] < 0x30 && p[2] < 0x30 && p[3] == 0xFF),
        "widget painted something before its first sample"
    );

    // After a sample, the label is rendered: some foreground pixels appear.
    assert!(dash.update(&sample(12.0, 47.0)));
    dash.draw(&mut ctx);
    assert!(
        ctx.pixels().chunks_exact(4).any(|p| p[0] > 0x60),
        "sampled stats were not rendered as foreground glyphs"
    );
}
