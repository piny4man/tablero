//! Integration coverage across native sysfs readings and the core widget.

use std::fs;

use tablero_core::render::Bounds;
use tablero_core::widget::{BacklightWidget, Command, Msg, ScrollDirection, Widget};
use tablero_wayland::backlight::read_backlights;

#[test]
fn sysfs_snapshot_drives_render_state_and_scroll_command() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let panel = temp.path().join("intel_backlight");
    fs::create_dir(&panel).unwrap();
    fs::write(panel.join("brightness"), "400\n").unwrap();
    fs::write(panel.join("actual_brightness"), "420\n").unwrap();
    fs::write(panel.join("max_brightness"), "1000\n").unwrap();

    let devices = runtime.block_on(read_backlights(temp.path())).unwrap();
    let mut widget = BacklightWidget::new(Bounds::new(0, 0, 200, 32))
        .with_format(Some("{percent}%".into()))
        .with_scroll_step(Some(2.0));
    assert!(widget.update(&Msg::Backlight(devices)));
    assert_eq!(widget.label(), "42%");
    assert_eq!(
        widget.on_scroll(10, 10, ScrollDirection::Increase),
        Some(Command::AdjustBacklight {
            device: "intel_backlight".into(),
            direction: ScrollDirection::Increase,
            step: 2.0,
        })
    );
}

#[test]
fn missing_sysfs_class_keeps_the_widget_absent() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let missing = temp.path().join("missing");
    let devices = runtime.block_on(read_backlights(&missing)).unwrap();
    let mut widget = BacklightWidget::new(Bounds::new(0, 0, 200, 32));
    assert!(!widget.update(&Msg::Backlight(devices)));
    assert_eq!(widget.label(), "");
}
