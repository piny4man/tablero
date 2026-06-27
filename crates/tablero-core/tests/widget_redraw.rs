//! Integration coverage for the message → app-state → redraw-decision path.
//!
//! Exercises the public widget architecture exactly as the Wayland host loop
//! does: build a [`Dashboard`], feed it [`Msg`]s, and assert the dirty-flag the
//! host uses to decide whether to repaint — then confirm the painted frame
//! reflects the latest message.

use chrono::{Local, TimeZone};
use tablero_core::render::{Bounds, RenderContext};
use tablero_core::widget::{ClockWidget, Dashboard, Msg};

fn tick(h: u32, m: u32, s: u32) -> Msg {
    Msg::Tick(Local.with_ymd_and_hms(2026, 6, 27, h, m, s).unwrap())
}

/// A message that changes a widget's visible state yields a redraw decision of
/// `true`; one that does not yields `false`.
#[test]
fn message_updates_state_and_drives_exact_redraw_decision() {
    let mut dash = Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 320, 32)))]);

    // From the empty initial state, the first tick is a visible change → redraw.
    assert!(
        dash.update(&tick(23, 59, 58)),
        "first tick must request a redraw"
    );

    // The same wall-clock second produces identical text → no redraw.
    assert!(
        !dash.update(&tick(23, 59, 58)),
        "an unchanged second must not request a redraw"
    );

    // Crossing a second boundary changes the text → redraw again.
    assert!(
        dash.update(&tick(23, 59, 59)),
        "a new second must request a redraw"
    );
}

/// After a redraw-worthy message, the painted frame is a valid full-surface
/// buffer with the dark background and rendered glyphs.
#[test]
fn redraw_after_message_paints_the_current_state() {
    let mut dash = Dashboard::new(vec![Box::new(ClockWidget::new(Bounds::new(0, 0, 320, 32)))]);
    let mut ctx = RenderContext::new(320, 32);

    assert!(dash.update(&tick(12, 0, 0)));
    dash.draw(&mut ctx);

    let px = ctx.pixels();
    assert_eq!(px.len(), 320 * 32 * 4, "frame covers the whole surface");

    // Opaque dark background in the far corner, away from the left-aligned text.
    let corner = &px[px.len() - 4..];
    assert!(
        corner[0] < 0x30 && corner[1] < 0x30 && corner[2] < 0x30,
        "corner is not the dark background: {corner:?}"
    );
    assert_eq!(corner[3], 0xFF, "corner is not opaque");

    // The clock glyphs lighten some pixels above the background level.
    assert!(
        px.chunks_exact(4).any(|p| p[0] > 0x60),
        "no rendered clock glyphs found"
    );
}
