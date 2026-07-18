//! Integration coverage: a configuration drives the *initialized* widget
//! placement across zones, and the render settings the bar paints with.
//!
//! These exercise the public assembly path the host loop uses —
//! [`Config::build_dashboard`] and [`Config::render_settings`] — so a change in
//! the TOML is observable in what the bar would render and where each widget
//! sits, without opening a Wayland surface.

use tablero_core::config::Config;
use tablero_core::render::{Bounds, RenderContext};
use tablero_core::widget::{
    Battery, BatteryState, ClickButton, Command, Dashboard, Msg, Workspaces,
};

/// Build the dashboard a TOML document describes over a 200x32 surface, seed it
/// with a one-workspace snapshot, then lay it out.
///
/// The seed comes *before* layout on purpose: each widget is packed to the size
/// of its current content, so the workspace widget only reserves (and exposes a
/// clickable cell for) its column once it actually holds a workspace. This
/// mirrors the host loop, which updates from its data sources and then lays the
/// surface out before painting.
fn laid_out(toml: &str) -> Dashboard {
    let config = Config::from_toml_str(toml).expect("valid config");
    let mut dash = config.build_dashboard(Bounds::new(0, 0, 200, 32), None);
    dash.update(&Msg::Workspaces(Workspaces::new([1], 1)));
    let mut ctx = RenderContext::new(200, 32);
    dash.layout(&mut ctx, 200, 32);
    dash
}

#[test]
fn zone_placement_packs_widgets_to_their_configured_edges() {
    // Workspaces on the left edge: its lone square (32px) cell sits at the
    // origin, so a click near the left switches, and the empty middle commands
    // nothing (the clock, never ticked, holds nothing clickable).
    let left = laid_out(
        r#"
        [bar]
        modules-left = ["workspaces"]
        modules-center = []
        modules-right = ["clock"]
        "#,
    );
    assert_eq!(
        left.on_click(10, 16, ClickButton::Left),
        Some(Command::SwitchWorkspace(1))
    );
    assert_eq!(left.on_click(100, 16, ClickButton::Left), None);

    // Moving workspaces to the right edge moves its clickable cell with it: the
    // single cell packs flush against the far edge at [168, 200).
    let right = laid_out(
        r#"
        [bar]
        modules-left = ["clock"]
        modules-center = []
        modules-right = ["workspaces"]
        "#,
    );
    assert_eq!(
        right.on_click(180, 16, ClickButton::Left),
        Some(Command::SwitchWorkspace(1))
    );
    assert_eq!(right.on_click(10, 16, ClickButton::Left), None);
}

#[test]
fn a_single_widget_builds_just_that_widget() {
    // A config naming a single widget builds exactly that widget, packed at the
    // origin — clicking its cell switches.
    let dash = laid_out(
        r#"
        [bar]
        modules-left = ["workspaces"]
        modules-center = []
        modules-right = []
        "#,
    );
    assert_eq!(
        dash.on_click(0, 0, ClickButton::Left),
        Some(Command::SwitchWorkspace(1))
    );
}

#[test]
fn config_theme_drives_render_settings_and_painted_pixels() {
    let config = Config::from_toml_str(
        r##"
        [theme]
        background = "#0a0b0c"
        foreground = "#102030"
        accent = "#ddeeff"

        [font]
        size = 20.0
        "##,
    )
    .expect("valid config");

    // The resolved settings carry the configured theme verbatim (opaque alpha,
    // since the colors are written as six-digit hex).
    let settings = config.render_settings();
    assert_eq!(settings.background, (0x0a, 0x0b, 0x0c, 0xFF));
    assert_eq!(settings.foreground, (0x10, 0x20, 0x30, 0xFF));
    assert_eq!(settings.accent, (0xdd, 0xee, 0xff, 0xFF));
    assert_eq!(settings.font_size, 20.0);

    // And those settings actually reach pixels: a cleared frame is the configured
    // background color (opaque, so the premultiplied bytes are the raw channels).
    let mut ctx = RenderContext::with_settings(20, 8, settings);
    ctx.fill_background();
    assert_eq!(&ctx.pixels()[0..4], &[0x0a, 0x0b, 0x0c, 0xFF]);
}

#[test]
fn bar_background_overrides_the_theme_for_painted_pixels() {
    // A [bar] background takes precedence over the theme background in what the
    // bar actually clears to — the seam that lets the bar go translucent.
    let config = Config::from_toml_str(
        r##"
        [theme]
        background = "#0a0b0c"

        [bar]
        background = "#204060"
        "##,
    )
    .expect("valid config");

    let settings = config.render_settings();
    assert_eq!(settings.background, (0x20, 0x40, 0x60, 0xFF));
}

#[test]
fn widget_border_config_reaches_painted_workspace_pixels() {
    let config = Config::from_toml_str(
        r##"
        [bar]
        modules-left = ["workspaces"]
        modules-center = []
        modules-right = []

        [widget.workspaces]
        background = "#204020"
        accent = "#204020"
        border = "#e0a020"
        border-width = 2
        radius = 0
        "##,
    )
    .expect("valid config");
    let mut dash = config.build_dashboard(Bounds::new(0, 0, 64, 32), None);
    dash.update(&Msg::Workspaces(Workspaces::new([1], 1)));
    let mut ctx = RenderContext::with_settings(64, 32, config.render_settings());
    dash.layout(&mut ctx, 64, 32);
    dash.draw(&mut ctx);

    let px = ctx.pixels();
    let border = (16 * 64) * 4;
    assert!(
        px[border] > 0xB0 && px[border + 1] > 0x70 && px[border + 2] < 0x50,
        "configured workspace border did not render: {:?}",
        &px[border..border + 4]
    );
}

#[test]
fn charging_battery_config_reaches_its_border_and_compact_format() {
    let config = Config::from_toml_str(
        r##"
        [bar]
        modules-left = ["battery"]
        modules-center = []
        modules-right = []

        [widget.battery]
        format = "{icon} {percent}%"
        padding = 4
        radius = 0

        [widget.battery.charging]
        background = "#2df18520"
        foreground = "#5ff5a0"
        border = "#2df185"
        "##,
    )
    .expect("valid config");
    let mut dash = config.build_dashboard(Bounds::new(0, 0, 96, 32), None);
    dash.update(&Msg::Battery(Some(Battery::new(
        BatteryState::Charging,
        81.0,
    ))));
    let mut ctx = RenderContext::with_settings(96, 32, config.render_settings());
    dash.layout(&mut ctx, 96, 32);
    dash.draw(&mut ctx);

    let px = ctx.pixels();
    let border = (16 * 96) * 4;
    assert!(
        px[border + 1] > 0xB0 && px[border + 2] > 0x60,
        "charging border did not render green: {:?}",
        &px[border..border + 4]
    );
}
