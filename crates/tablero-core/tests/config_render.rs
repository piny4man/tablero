//! Integration coverage: a configuration drives the *initialized* widget order
//! and the render settings the bar paints with.
//!
//! These exercise the public assembly path the host loop uses —
//! [`Config::build_dashboard`] and [`Config::render_settings`] — so a change in
//! the TOML is observable in what the bar would render and where each widget
//! sits, without opening a Wayland surface.

use tablero_core::config::Config;
use tablero_core::render::{Bounds, RenderContext};
use tablero_core::widget::{Command, Dashboard, Msg, Workspaces};

/// Build the dashboard a TOML document describes over a 200x32 surface, laid out
/// and seeded with a one-workspace snapshot so the workspace widget exposes a
/// clickable cell at its column origin.
fn laid_out(toml: &str) -> Dashboard {
    let config = Config::from_toml_str(toml).expect("valid config");
    let mut dash = config.build_dashboard(Bounds::new(0, 0, 200, 32), None);
    dash.layout(200, 32);
    dash.update(&Msg::Workspaces(Workspaces::new([1], 1)));
    dash
}

#[test]
fn widget_order_in_config_places_widgets_left_to_right() {
    // Clock then workspaces: clock owns the left column, workspaces the right.
    let dash = laid_out(r#"widgets = ["clock", "workspaces"]"#);
    // The left column is the display-only clock: a click there commands nothing.
    assert_eq!(dash.on_click(10, 16), None);
    // The right column is the workspace switcher: a click there switches.
    assert_eq!(dash.on_click(110, 16), Some(Command::SwitchWorkspace(1)));

    // Reversing the configured order moves the interactive column to the left.
    let reversed = laid_out(r#"widgets = ["workspaces", "clock"]"#);
    assert_eq!(reversed.on_click(10, 16), Some(Command::SwitchWorkspace(1)));
    assert_eq!(reversed.on_click(110, 16), None);
}

#[test]
fn config_with_one_widget_resolves_to_a_single_column() {
    // A config naming a single widget builds exactly that widget, spanning the
    // whole surface — clicking anywhere in it switches.
    let dash = laid_out(r#"widgets = ["workspaces"]"#);
    assert_eq!(dash.on_click(0, 0), Some(Command::SwitchWorkspace(1)));
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

    // The resolved settings carry the configured theme verbatim.
    let settings = config.render_settings();
    assert_eq!(settings.background, (0x0a, 0x0b, 0x0c));
    assert_eq!(settings.foreground, (0x10, 0x20, 0x30));
    assert_eq!(settings.accent, (0xdd, 0xee, 0xff));
    assert_eq!(settings.font_size, 20.0);

    // And those settings actually reach pixels: a cleared frame is the configured
    // background color (opaque, so the premultiplied bytes are the raw channels).
    let mut ctx = RenderContext::with_settings(20, 8, settings);
    ctx.fill_background();
    assert_eq!(&ctx.pixels()[0..4], &[0x0a, 0x0b, 0x0c, 0xFF]);
}
