//! Semantic built-in icons rendered as lightweight vector paths.
//!
//! The bar draws status glyphs from a small, semantic set — a clock, battery
//! states, a network arc, and so on — instead of Nerd Font private-use
//! codepoints. Each [`BuiltinIcon`] maps to path data flattened from the
//! project's `icons/*.svg` at build time (see `tools/icongen`) and stored in the
//! generated [`builtin_icon_paths`] module. Rendering is a handful of
//! [`tiny_skia`] path fills per icon, so no SVG parser, icon font, or other
//! icon-rendering dependency ships in the binary.
//!
//! Widgets never touch this module's geometry directly: they name a semantic
//! [`BuiltinIcon`] and paint it through
//! [`RenderContext::draw_builtin_icon`](crate::render::RenderContext::draw_builtin_icon),
//! which fills the paths in the widget's own state color.

use tiny_skia::{FillRule, Paint, Path, PathBuilder, Pixmap, Transform};

use crate::render::Bounds;

#[path = "builtin_icon_paths.rs"]
mod builtin_icon_paths;

use builtin_icon_paths::{Cmd, ICONS, RawIcon};

/// A semantic status icon, independent of the artwork backing it.
///
/// Widgets choose a variant from their current state (a battery picks
/// [`BatteryCharging`](BuiltinIcon::BatteryCharging) while charging, a volume
/// control picks a level); the concrete vector art each maps to is an
/// implementation detail resolved by [`stem`](BuiltinIcon::stem).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinIcon {
    /// The clock readout's leading icon.
    Clock,
    /// A battery at a low charge level.
    BatteryLow,
    /// A battery at roughly half charge.
    BatteryHalf,
    /// A battery at a high charge level.
    BatteryFull,
    /// A battery while charging, regardless of level.
    BatteryCharging,
    /// A backlight/brightness level on a seven-step ramp, `0` (off) to `6`
    /// (full). Values above `6` saturate to the full-brightness icon.
    Backlight(u8),
    /// A wireless (Wi-Fi) network link.
    NetworkWireless,
    /// A wired (Ethernet) network link.
    NetworkWired,
    /// System CPU load.
    System,
    /// System memory load.
    Memory,
    /// A Bluetooth adapter.
    Bluetooth,
    /// Pending package updates.
    Updates,
    /// Notifications enabled.
    Notifications,
    /// Notifications suppressed (do-not-disturb).
    NotificationsOff,
    /// The idle daemon is active (protecting the session) — shown with an open
    /// padlock.
    HypridleActive,
    /// The idle daemon is inhibited/paused — shown with a closed padlock.
    HypridleInactive,
    /// A power/session control.
    Power,
    /// The performance power profile.
    PowerProfilePerformance,
    /// The balanced power profile.
    PowerProfileBalanced,
    /// The power-saver power profile.
    PowerProfileSaver,
    /// Audio volume at a low level.
    VolumeLow,
    /// Audio volume at a medium level.
    VolumeMedium,
    /// Audio volume at a high level.
    VolumeHigh,
    /// Audio muted.
    VolumeMuted,
}

impl BuiltinIcon {
    /// The source SVG stem (matching a `RawIcon::name`) backing this icon.
    fn stem(self) -> &'static str {
        match self {
            BuiltinIcon::Clock => "clock-filled",
            BuiltinIcon::BatteryLow => "battery-low2-filled",
            BuiltinIcon::BatteryHalf => "battery-half2-filled",
            BuiltinIcon::BatteryFull => "battery-full2-filled",
            BuiltinIcon::BatteryCharging => "battery-charge2-filled",
            BuiltinIcon::Backlight(level) => match level {
                0 => "bright-empty",
                1 => "bright-1",
                2 => "bright-2",
                3 => "bright-3",
                4 => "bright-4",
                5 => "bright-5",
                _ => "bright-full",
            },
            BuiltinIcon::NetworkWireless => "wifi-filled",
            BuiltinIcon::NetworkWired => "ethernet-filled",
            BuiltinIcon::System => "cpu-filled",
            BuiltinIcon::Memory => "memory-filled",
            BuiltinIcon::Bluetooth => "bluetooth-outline",
            BuiltinIcon::Updates => "box5-filled",
            BuiltinIcon::Notifications => "bell-filled",
            BuiltinIcon::NotificationsOff => "bell-off2-filled",
            BuiltinIcon::HypridleActive => "lock-open-filled",
            BuiltinIcon::HypridleInactive => "lock-filled",
            BuiltinIcon::Power => "power-off-filled",
            BuiltinIcon::PowerProfilePerformance => "bolt-lightning-filled",
            BuiltinIcon::PowerProfileBalanced => "balance-filled",
            BuiltinIcon::PowerProfileSaver => "feather-filled",
            BuiltinIcon::VolumeLow => "volume-down-filled",
            BuiltinIcon::VolumeMedium => "volume-filled",
            BuiltinIcon::VolumeHigh => "volume-up-filled",
            BuiltinIcon::VolumeMuted => "volume-x-filled",
        }
    }

    /// The flattened artwork for this icon, or `None` if the generated table has
    /// no matching entry (which would be a generator/mapping bug, handled by
    /// simply drawing nothing rather than panicking mid-frame).
    fn raw(self) -> Option<&'static RawIcon> {
        let stem = self.stem();
        ICONS.iter().find(|icon| icon.name == stem)
    }
}

/// Build a fillable path from one sub-path's command stream.
fn build_path(cmds: &[Cmd]) -> Option<Path> {
    let mut pb = PathBuilder::new();
    for cmd in cmds {
        match *cmd {
            Cmd::Move(x, y) => pb.move_to(x, y),
            Cmd::Line(x, y) => pb.line_to(x, y),
            Cmd::Cubic(x1, y1, x2, y2, x, y) => pb.cubic_to(x1, y1, x2, y2, x, y),
            Cmd::Close => pb.close(),
        }
    }
    pb.finish()
}

/// Fill `icon` into `bounds` on `pixmap`, uniformly scaled to fit while
/// preserving aspect ratio and centered, painted in straight-alpha `color`.
///
/// Each source `<path>` is filled with its own winding rule, so shapes that
/// carve holes (a slashed bell, a battery outline) render correctly. Nothing is
/// drawn for an unmapped icon; callers guard zero-area slots and transparent
/// colors before reaching here.
pub fn draw_into(pixmap: &mut Pixmap, icon: BuiltinIcon, bounds: Bounds, color: (u8, u8, u8, u8)) {
    let Some(raw) = icon.raw() else {
        return;
    };
    if raw.view_w <= 0.0 || raw.view_h <= 0.0 {
        return;
    }

    // Largest uniform scale that fits the viewBox inside the slot on both axes,
    // then center the scaled artwork within the slot.
    let scale = (bounds.width as f32 / raw.view_w).min(bounds.height as f32 / raw.view_h);
    let draw_w = raw.view_w * scale;
    let draw_h = raw.view_h * scale;
    let tx = bounds.x as f32 + (bounds.width as f32 - draw_w) / 2.0;
    let ty = bounds.y as f32 + (bounds.height as f32 - draw_h) / 2.0;
    let transform = Transform::from_row(scale, 0.0, 0.0, scale, tx, ty);

    let (r, g, b, a) = color;
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;

    for sub in raw.paths {
        let Some(path) = build_path(sub.cmds) else {
            continue;
        };
        let rule = if sub.even_odd {
            FillRule::EvenOdd
        } else {
            FillRule::Winding
        };
        pixmap.fill_path(&path, &paint, rule, transform, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_semantic_icon_resolves_to_artwork() {
        // The seven-step backlight ramp plus every other semantic variant must
        // map to a generated icon; a missing entry means art or mapping drift.
        let mut icons = vec![
            BuiltinIcon::Clock,
            BuiltinIcon::BatteryLow,
            BuiltinIcon::BatteryHalf,
            BuiltinIcon::BatteryFull,
            BuiltinIcon::BatteryCharging,
            BuiltinIcon::NetworkWireless,
            BuiltinIcon::NetworkWired,
            BuiltinIcon::System,
            BuiltinIcon::Memory,
            BuiltinIcon::Bluetooth,
            BuiltinIcon::Updates,
            BuiltinIcon::Notifications,
            BuiltinIcon::NotificationsOff,
            BuiltinIcon::HypridleActive,
            BuiltinIcon::HypridleInactive,
            BuiltinIcon::Power,
            BuiltinIcon::PowerProfilePerformance,
            BuiltinIcon::PowerProfileBalanced,
            BuiltinIcon::PowerProfileSaver,
            BuiltinIcon::VolumeLow,
            BuiltinIcon::VolumeMedium,
            BuiltinIcon::VolumeHigh,
            BuiltinIcon::VolumeMuted,
        ];
        icons.extend((0..=7).map(BuiltinIcon::Backlight));
        for icon in icons {
            assert!(
                icon.raw().is_some(),
                "no artwork for {icon:?} (stem {:?})",
                icon.stem()
            );
        }
    }

    #[test]
    fn backlight_ramp_saturates_at_full() {
        // Levels past the ramp reuse the full-brightness art rather than falling
        // off the end of the table.
        assert_eq!(BuiltinIcon::Backlight(6).stem(), "bright-full");
        assert_eq!(BuiltinIcon::Backlight(99).stem(), "bright-full");
        assert_eq!(BuiltinIcon::Backlight(0).stem(), "bright-empty");
    }

    #[test]
    fn draw_into_fills_pixels_in_the_requested_color() {
        let mut pixmap = Pixmap::new(32, 32).unwrap();
        draw_into(
            &mut pixmap,
            BuiltinIcon::System,
            Bounds::new(0, 0, 32, 32),
            (0xFF, 0x00, 0x00, 0xFF),
        );
        // Some pixel must have picked up the fill color; a fully blank pixmap
        // would mean the path never rendered.
        let painted = pixmap
            .data()
            .chunks_exact(4)
            .any(|px| px[3] > 0x40 && px[0] > 0x80);
        assert!(painted, "icon fill produced no visible pixels");
    }

    #[test]
    fn draw_into_leaves_a_zero_scale_slot_untouched() {
        // A zero-width slot yields a zero scale: nothing should be drawn, and
        // the call must not panic on the degenerate transform.
        let mut pixmap = Pixmap::new(16, 16).unwrap();
        draw_into(
            &mut pixmap,
            BuiltinIcon::Clock,
            Bounds::new(0, 0, 0, 16),
            (0xFF, 0xFF, 0xFF, 0xFF),
        );
        assert!(pixmap.data().iter().all(|&b| b == 0));
    }
}
