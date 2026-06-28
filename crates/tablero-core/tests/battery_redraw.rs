//! Integration coverage for the battery message → app-state → redraw decision.
//!
//! Exercises the public widget architecture exactly as the Wayland host loop
//! does: build a [`Dashboard`] holding a [`BatteryWidget`], feed it
//! [`Msg::Battery`] readings, and assert the dirty-flag the host uses to decide
//! whether to repaint — then confirm an absent battery paints a blank slot.

use tablero_core::render::{Bounds, RenderContext};
use tablero_core::widget::{Battery, BatteryState, BatteryWidget, Dashboard, Msg};

fn reading(state: BatteryState, percent: f64) -> Msg {
    Msg::Battery(Some(Battery::new(state, percent)))
}

/// A reading that changes the visible battery state yields a redraw decision of
/// `true`; one that normalizes to the same screen value yields `false`.
#[test]
fn battery_reading_drives_exact_redraw_decision() {
    let mut dash = Dashboard::new(vec![Box::new(BatteryWidget::new(Bounds::new(
        0, 0, 320, 32,
    )))]);

    // From the empty initial state, the first reading is a visible change.
    assert!(
        dash.update(&reading(BatteryState::Discharging, 85.0)),
        "first reading must request a redraw"
    );

    // A sub-percent jitter normalizes to the same whole percent → no redraw.
    assert!(
        !dash.update(&reading(BatteryState::Discharging, 85.3)),
        "an unchanged whole-percent reading must not request a redraw"
    );

    // Crossing into a new whole percent changes the label → redraw again.
    assert!(
        dash.update(&reading(BatteryState::Discharging, 84.0)),
        "a new percentage must request a redraw"
    );

    // Plugging in flips the state word → redraw.
    assert!(
        dash.update(&reading(BatteryState::Charging, 84.0)),
        "a new charge state must request a redraw"
    );
}

/// An absent battery (`Msg::Battery(None)`) clears a previously shown reading and
/// paints a blank, fully-dark slot — no stale percentage left behind.
#[test]
fn absent_battery_paints_a_blank_slot() {
    let mut dash = Dashboard::new(vec![Box::new(BatteryWidget::new(Bounds::new(
        0, 0, 320, 32,
    )))]);
    let mut ctx = RenderContext::new(320, 32);

    // Show a reading, then report the battery gone.
    assert!(dash.update(&reading(BatteryState::Discharging, 85.0)));
    assert!(
        dash.update(&Msg::Battery(None)),
        "going absent is a visible change"
    );

    dash.draw(&mut ctx);
    let px = ctx.pixels();
    assert_eq!(px.len(), 320 * 32 * 4, "frame covers the whole surface");

    // With no battery present, every pixel is the opaque dark background: no
    // glyph was painted anywhere.
    assert!(
        px.chunks_exact(4)
            .all(|p| p[0] < 0x30 && p[1] < 0x30 && p[2] < 0x30 && p[3] == 0xFF),
        "absent battery left non-background pixels"
    );
}
