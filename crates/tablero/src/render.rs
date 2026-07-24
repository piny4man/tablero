use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap};
use tiny_skia::{
    FillRule, FilterQuality, Paint, Path, PathBuilder, Pixmap, PixmapPaint, PixmapRef, Rect,
    Stroke, Transform,
};

use crate::icon::BuiltinIcon;

/// Opaque dark background color (R, G, B, A).
pub const BG: (u8, u8, u8, u8) = (0x18, 0x18, 0x18, 0xFF);
/// Opaque light foreground text color (R, G, B, A).
pub const FG: (u8, u8, u8, u8) = (0xEA, 0xEA, 0xEA, 0xFF);

/// Default font size (px) used when no configuration overrides it.
const FONT_SIZE: f32 = 16.0;

/// The visual settings a [`RenderContext`] paints with: theme colors and font.
///
/// One value resolved once (from configuration, or [`RenderSettings::default`]
/// for the built-in theme) and handed to the context, so widgets read the
/// active foreground/accent through the context rather than baking in constants.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSettings {
    /// Background fill behind every widget. A non-opaque alpha lets the desktop
    /// show through, which is what makes a "floating pills" bar possible.
    pub background: (u8, u8, u8, u8),
    /// Default text color.
    pub foreground: (u8, u8, u8, u8),
    /// Emphasis color (e.g. the active workspace).
    pub accent: (u8, u8, u8, u8),
    /// Text size in pixels.
    pub font_size: f32,
    /// Font family name, or `None` for the system default.
    pub font_family: Option<String>,
    /// Integer output scale (HiDPI). The font is *pre-scaled* into `font_size`;
    /// this factor scales the *geometry* widgets author in logical pixels
    /// (pill radius, padding, inter-pill gap, bar margin) at render/layout time.
    /// Always at least 1.
    pub scale: u32,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            background: BG,
            foreground: FG,
            accent: FG,
            font_size: FONT_SIZE,
            font_family: None,
            scale: 1,
        }
    }
}

/// An axis-aligned rectangle in surface pixel coordinates.
///
/// Carries the layout slot a widget occupies: where it draws and how much room
/// it has. Reported by [`crate::widget::Widget::bounds`] so future interaction
/// code (hit-testing, click routing) has the geometry it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Bounds {
    /// Construct bounds from an origin and size.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// True if `(px, py)` (surface pixel coordinates) falls within the rectangle.
    ///
    /// Half-open on the far edges, so adjacent bounds never both claim a pixel.
    pub fn contains(&self, px: u32, py: u32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

fn rounded_rect_path(bounds: Bounds, radius: f32, inset: f32) -> Option<Path> {
    let x = bounds.x as f32 + inset;
    let y = bounds.y as f32 + inset;
    let w = bounds.width as f32 - 2.0 * inset;
    let h = bounds.height as f32 - 2.0 * inset;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }

    let radius = radius.clamp(0.0, (w / 2.0).min(h / 2.0));
    if radius <= 0.0 {
        return Some(PathBuilder::from_rect(Rect::from_xywh(x, y, w, h)?));
    }

    let mut pb = PathBuilder::new();
    pb.move_to(x + radius, y);
    pb.line_to(x + w - radius, y);
    pb.quad_to(x + w, y, x + w, y + radius);
    pb.line_to(x + w, y + h - radius);
    pb.quad_to(x + w, y + h, x + w - radius, y + h);
    pb.line_to(x + radius, y + h);
    pb.quad_to(x, y + h, x, y + h - radius);
    pb.line_to(x, y + radius);
    pb.quad_to(x, y, x + radius, y);
    pb.close();
    pb.finish()
}

/// A reusable software-render target shared across widgets within one frame.
///
/// Owns the expensive font machinery (`FontSystem`, `SwashCache`) and the
/// destination `Pixmap` so a redraw does not reallocate them every tick.
/// Widgets clear the background once, then each draws its text into its own
/// [`Bounds`]; the finished frame is read back as premultiplied RGBA8888 via
/// [`pixels`](RenderContext::pixels).
pub struct RenderContext {
    font_system: FontSystem,
    swash_cache: SwashCache,
    pixmap: Pixmap,
    settings: RenderSettings,
}

impl RenderContext {
    /// Create a context targeting a `width * height` pixel surface, using the
    /// built-in theme ([`RenderSettings::default`]).
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_settings(width, height, RenderSettings::default())
    }

    /// Create a context targeting a `width * height` pixel surface, painting with
    /// the supplied [`RenderSettings`].
    pub fn with_settings(width: u32, height: u32, settings: RenderSettings) -> Self {
        let pixmap = Pixmap::new(width.max(1), height.max(1)).expect("non-zero pixmap dimensions");
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
            pixmap,
            settings,
        }
    }

    /// The active foreground (default text) color.
    pub fn foreground(&self) -> (u8, u8, u8, u8) {
        self.settings.foreground
    }

    /// The active accent (emphasis) color.
    pub fn accent(&self) -> (u8, u8, u8, u8) {
        self.settings.accent
    }

    /// The settings this context paints with.
    pub fn settings(&self) -> &RenderSettings {
        &self.settings
    }

    /// Replace the settings this context paints with.
    ///
    /// Used when the output scale changes and the resolved physical font size
    /// must follow it. Cheap: the font machinery (`FontSystem`, `SwashCache`) is
    /// retained, only the visual settings are swapped.
    pub fn set_settings(&mut self, settings: RenderSettings) {
        self.settings = settings;
    }

    /// Current target width in pixels.
    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }

    /// Current target height in pixels.
    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    /// The active integer output scale (always at least 1).
    ///
    /// Widgets multiply logical geometry (radius, padding, gap, margin) by this
    /// to land physical pixels. The font is already scaled into `font_size`, so
    /// it must *not* be multiplied again — see [`RenderSettings::scale`].
    pub fn scale_factor(&self) -> u32 {
        self.settings.scale.max(1)
    }

    /// Resize the target to `width * height`, reallocating only when the size
    /// actually changed. A zero dimension is ignored (the protocol uses zero to
    /// mean "keep your current value").
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if self.pixmap.width() != width || self.pixmap.height() != height {
            self.pixmap = Pixmap::new(width, height).expect("non-zero pixmap dimensions");
        }
    }

    /// Fill the whole target with the configured background color, honoring its
    /// alpha (a transparent background lets the desktop show through).
    pub fn fill_background(&mut self) {
        let (r, g, b, a) = self.settings.background;
        self.pixmap.fill(tiny_skia::Color::from_rgba8(r, g, b, a));
    }

    /// Fill a rounded rectangle covering `bounds` with `color`, corner radius
    /// `radius` pixels.
    ///
    /// This is the "pill" primitive: a widget paints one behind its content to
    /// get the floating-pill look. `color` is *straight* (non-premultiplied)
    /// alpha — pass the raw `#rrggbbaa` channels and let tiny-skia premultiply,
    /// so a translucent pill blends correctly over a transparent bar with no
    /// dark halo on its anti-aliased edges. The radius is clamped to half the
    /// shorter side (an over-large radius yields a stadium/circle, not
    /// artifacts). Nothing is drawn for a zero-area slot or a fully transparent
    /// color, so a widget with no background color draws no pill at all.
    pub fn fill_rounded_rect(&mut self, bounds: Bounds, color: (u8, u8, u8, u8), radius: f32) {
        let (r, g, b, a) = color;
        if bounds.width == 0 || bounds.height == 0 || a == 0 {
            return;
        }

        let path =
            rounded_rect_path(bounds, radius, 0.0).expect("validated non-zero rounded rectangle");

        let mut paint = Paint::default();
        paint.set_color_rgba8(r, g, b, a);
        self.pixmap.fill_path(
            &path,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    /// Stroke a rounded rectangle inside `bounds` with `color` and `width`.
    ///
    /// The stroke is inset by half its width so it stays inside the widget's
    /// layout slot. `radius` and `width` are physical pixels; callers scale
    /// logical widget geometry before invoking this primitive.
    pub fn stroke_rounded_rect(
        &mut self,
        bounds: Bounds,
        color: (u8, u8, u8, u8),
        radius: f32,
        width: f32,
    ) {
        let (r, g, b, a) = color;
        if bounds.width == 0 || bounds.height == 0 || a == 0 || width <= 0.0 {
            return;
        }

        let max_width = bounds.width.min(bounds.height) as f32;
        let width = width.min(max_width);
        let inset = width / 2.0;
        let Some(path) = rounded_rect_path(bounds, (radius - inset).max(0.0), inset) else {
            return;
        };

        let mut paint = Paint::default();
        paint.set_color_rgba8(r, g, b, a);
        let stroke = Stroke {
            width,
            ..Stroke::default()
        };
        self.pixmap
            .stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }

    /// Shape and draw `text` in `color` within `bounds`.
    ///
    /// The text is left-aligned and vertically centered within the bounds (the
    /// line box is the full bound height), and never wraps. Glyphs are clipped
    /// to nothing outside the bounds only insofar as the shaping box limits
    /// them; callers size bounds to fit their content.
    pub fn draw_text(&mut self, text: &str, bounds: Bounds, color: (u8, u8, u8, u8)) {
        if bounds.width == 0 || bounds.height == 0 {
            return;
        }

        let RenderContext {
            font_system,
            swash_cache,
            pixmap,
            settings,
        } = self;

        let metrics = Metrics::new(settings.font_size, bounds.height as f32);
        let mut buffer = Buffer::new(font_system, metrics);
        buffer.set_size(Some(bounds.width as f32), Some(bounds.height as f32));
        buffer.set_wrap(Wrap::None);

        let mut attrs = Attrs::new();
        if let Some(family) = &settings.font_family {
            attrs = attrs.family(Family::Name(family));
        }
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(font_system, false);

        let text_color = Color::rgba(color.0, color.1, color.2, color.3);
        let (ox, oy) = (bounds.x as f32, bounds.y as f32);
        let mut dst = pixmap.as_mut();
        buffer.draw(font_system, swash_cache, text_color, |x, y, w, h, color| {
            let Some(rect) = Rect::from_xywh(ox + x as f32, oy + y as f32, w as f32, h as f32)
            else {
                return;
            };
            let mut paint = Paint::default();
            let rgba = color.as_rgba();
            paint.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
            dst.fill_rect(rect, &paint, Transform::default(), None);
        });
    }

    /// Shape and draw `text` with its visible ink centered within `bounds`.
    ///
    /// Intended for standalone icon glyphs whose font bearings can make the
    /// normal line-box alignment look off-center. Labels should keep using
    /// [`draw_text`](Self::draw_text) so their shared baseline remains stable.
    pub fn draw_text_centered(&mut self, text: &str, bounds: Bounds, color: (u8, u8, u8, u8)) {
        if text.is_empty() || bounds.width == 0 || bounds.height == 0 {
            return;
        }

        let RenderContext {
            font_system,
            swash_cache,
            pixmap,
            settings,
        } = self;

        let metrics = Metrics::new(settings.font_size, bounds.height as f32);
        let mut buffer = Buffer::new(font_system, metrics);
        buffer.set_size(Some(bounds.width as f32), Some(bounds.height as f32));
        buffer.set_wrap(Wrap::None);

        let mut attrs = Attrs::new();
        if let Some(family) = &settings.font_family {
            attrs = attrs.family(Family::Name(family));
        }
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(font_system, false);

        let text_color = Color::rgba(color.0, color.1, color.2, color.3);
        let mut ink_bounds: Option<(i32, i32, i32, i32)> = None;
        buffer.draw(font_system, swash_cache, text_color, |x, y, w, h, _| {
            let right = x.saturating_add(i32::try_from(w).unwrap_or(i32::MAX));
            let bottom = y.saturating_add(i32::try_from(h).unwrap_or(i32::MAX));
            ink_bounds = Some(match ink_bounds {
                Some((left, top, old_right, old_bottom)) => (
                    left.min(x),
                    top.min(y),
                    old_right.max(right),
                    old_bottom.max(bottom),
                ),
                None => (x, y, right, bottom),
            });
        });

        let Some((left, top, right, bottom)) = ink_bounds else {
            return;
        };
        let ink_width = i64::from(right - left);
        let ink_height = i64::from(bottom - top);
        let offset_x =
            i64::from(bounds.x) + (i64::from(bounds.width) - ink_width) / 2 - i64::from(left);
        let offset_y =
            i64::from(bounds.y) + (i64::from(bounds.height) - ink_height) / 2 - i64::from(top);
        let offset_x = offset_x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        let offset_y = offset_y.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;

        let mut dst = pixmap.as_mut();
        buffer.draw(font_system, swash_cache, text_color, |x, y, w, h, color| {
            let Some(rect) = Rect::from_xywh(
                (offset_x + x) as f32,
                (offset_y + y) as f32,
                w as f32,
                h as f32,
            ) else {
                return;
            };
            let mut paint = Paint::default();
            let rgba = color.as_rgba();
            paint.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
            dst.fill_rect(rect, &paint, Transform::default(), None);
        });
    }

    /// The width in pixels `text` would occupy when shaped with the active font,
    /// unconstrained (single line, no wrapping), rounded up.
    ///
    /// This is how the content-aware layout sizes a widget to exactly its
    /// content. The shaping matches [`draw_text`](RenderContext::draw_text) (same
    /// font, same `Shaping::Advanced` glyph fallback), so the measured width
    /// agrees with the drawn width up to the sub-pixel the `ceil` absorbs. Empty
    /// text measures zero, so an empty widget reserves no slot.
    pub fn measure_text(&mut self, text: &str) -> u32 {
        if text.is_empty() {
            return 0;
        }

        let RenderContext {
            font_system,
            settings,
            ..
        } = self;

        let metrics = Metrics::new(settings.font_size, settings.font_size);
        let mut buffer = Buffer::new(font_system, metrics);
        // Unconstrained box: the shaped line keeps its full advance width.
        buffer.set_size(None, None);
        buffer.set_wrap(Wrap::None);

        let mut attrs = Attrs::new();
        if let Some(family) = &settings.font_family {
            attrs = attrs.family(Family::Name(family));
        }
        buffer.set_text(text, &attrs, Shaping::Advanced, None);
        buffer.shape_until_scroll(font_system, false);

        buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0_f32, f32::max)
            .ceil() as u32
    }

    /// Blit a premultiplied RGBA8888 image into `bounds`, scaled to fit while
    /// preserving its aspect ratio and centered within the slot.
    ///
    /// `rgba` is `img_w * img_h` premultiplied `[R, G, B, A]` pixels — the same
    /// layout [`pixels`](RenderContext::pixels) reads back, so an icon decoded to
    /// this form draws directly. The image is downscaled (or upscaled) with
    /// bilinear filtering to the largest size that fits `bounds` on both axes, then
    /// centered; nothing is drawn for an empty slot, an empty image, or a byte
    /// slice whose length does not match `img_w * img_h * 4`. This is the one
    /// raster-image primitive the tray widget needs beyond text.
    pub fn draw_icon(&mut self, rgba: &[u8], img_w: u32, img_h: u32, bounds: Bounds) {
        if bounds.width == 0 || bounds.height == 0 || img_w == 0 || img_h == 0 {
            return;
        }
        let Some(src) = PixmapRef::from_bytes(rgba, img_w, img_h) else {
            // Mismatched length or zero dimension: skip rather than risk a panic.
            return;
        };

        // Largest uniform scale that fits the image inside the slot on both axes.
        let scale = (bounds.width as f32 / img_w as f32).min(bounds.height as f32 / img_h as f32);
        let draw_w = img_w as f32 * scale;
        let draw_h = img_h as f32 * scale;
        // Center the scaled image within the slot.
        let tx = bounds.x as f32 + (bounds.width as f32 - draw_w) / 2.0;
        let ty = bounds.y as f32 + (bounds.height as f32 - draw_h) / 2.0;
        // Use an explicit matrix so translation remains in destination pixels.
        // Composing `post_translate` onto a scale also scales the translation,
        // which lets tray icons escape an inset content box.
        let transform = Transform::from_row(scale, 0.0, 0.0, scale, tx, ty);

        let paint = PixmapPaint {
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        self.pixmap
            .as_mut()
            .draw_pixmap(0, 0, src, &paint, transform, None);
    }

    /// The square edge, in physical pixels, a built-in vector icon should occupy
    /// to sit visually alongside text at the active font size.
    ///
    /// The font is already pre-scaled into `font_size` (physical pixels), so the
    /// icon box tracks the text without a second scale multiply — the same
    /// invariant [`scale_factor`](Self::scale_factor) documents. Widgets center
    /// this box vertically within their pill and reserve it in their measured
    /// width.
    pub fn icon_edge(&self) -> u32 {
        self.settings.font_size.round().max(1.0) as u32
    }

    /// Fill a semantic [`BuiltinIcon`] into `bounds` in straight-alpha `color`.
    ///
    /// The vector artwork is scaled to fit the slot (preserving aspect ratio)
    /// and centered, inheriting the widget's state color so an icon matches the
    /// text beside it. A no-op for a zero-area slot or a fully transparent color.
    /// This is the vector counterpart to [`draw_icon`](Self::draw_icon)'s raster
    /// blit and the only path built-in icons take to the pixmap.
    pub fn draw_builtin_icon(
        &mut self,
        icon: BuiltinIcon,
        bounds: Bounds,
        color: (u8, u8, u8, u8),
    ) {
        if bounds.width == 0 || bounds.height == 0 || color.3 == 0 {
            return;
        }
        crate::icon::draw_into(&mut self.pixmap, icon, bounds, color);
    }

    /// Premultiplied RGBA8888 bytes of the current frame (`[R, G, B, A]` per
    /// pixel). Convert to the Wayland shared-memory layout with
    /// [`crate::blit::write_argb8888`] before committing.
    pub fn pixels(&self) -> &[u8] {
        self.pixmap.data()
    }
}

/// Render `text` over a dark background into a `width * height` pixel buffer.
///
/// Returns premultiplied RGBA8888 bytes (the native tiny-skia layout: each
/// pixel is `[R, G, B, A]`), of length `width * height * 4`. Convert to the
/// Wayland shared-memory layout with [`crate::blit::write_argb8888`] before
/// committing the buffer.
///
/// This is a thin one-shot wrapper over [`RenderContext`]; long-lived render
/// loops should hold a `RenderContext` and reuse it instead.
pub fn render_text(text: &str, width: u32, height: u32) -> Vec<u8> {
    let mut ctx = RenderContext::new(width, height);
    ctx.fill_background();
    ctx.draw_text(text, Bounds::new(0, 0, width, height), FG);
    ctx.pixels().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_text_has_rgba_length() {
        let px = render_text("12:00:00", 320, 32);
        assert_eq!(px.len(), 320 * 32 * 4);
    }

    #[test]
    fn render_text_fills_dark_opaque_background() {
        let px = render_text("12:00:00", 320, 32);
        // The bottom-right corner is well clear of the left-aligned glyphs, so
        // it must still be the opaque dark background we filled.
        let last = &px[px.len() - 4..];
        assert!(
            last[0] < 0x30 && last[1] < 0x30 && last[2] < 0x30,
            "corner not dark: {last:?}"
        );
        assert_eq!(last[3], 0xFF, "corner not opaque");
    }

    #[test]
    fn fill_background_alpha_is_transparent() {
        // A background color with zero alpha clears to fully transparent pixels,
        // so a transparent bar lets the desktop show through. tiny-skia stores
        // premultiplied bytes, so zero alpha zeroes every channel.
        let settings = RenderSettings {
            background: (0x80, 0x40, 0x20, 0x00),
            ..RenderSettings::default()
        };
        let mut ctx = RenderContext::with_settings(8, 4, settings);
        ctx.fill_background();
        assert_eq!(
            &ctx.pixels()[0..4],
            &[0, 0, 0, 0],
            "background not transparent"
        );
    }

    #[test]
    fn fill_rounded_rect_paints_interior_and_skips_transparent() {
        let mut ctx = RenderContext::new(40, 40);
        ctx.fill_background();
        // An opaque green pill across the middle of the surface.
        ctx.fill_rounded_rect(Bounds::new(4, 4, 32, 32), (0x00, 0xC0, 0x00, 0xFF), 8.0);
        let center = (20 * 40 + 20) * 4;
        {
            let px = ctx.pixels();
            // The center is deep inside the pill: fully covered, so green.
            assert!(
                px[center] < 0x30 && px[center + 1] > 0x80 && px[center + 2] < 0x30,
                "pill center not green: {:?}",
                &px[center..center + 4]
            );
        }

        // A fully transparent fill is a no-op: a corner outside the pill keeps
        // the background it had before the call.
        let before = ctx.pixels()[0..4].to_vec();
        ctx.fill_rounded_rect(Bounds::new(0, 0, 40, 40), (0xFF, 0x00, 0x00, 0x00), 8.0);
        assert_eq!(
            &ctx.pixels()[0..4],
            &before[..],
            "transparent fill changed pixels"
        );
    }

    #[test]
    fn stroke_rounded_rect_paints_an_inset_border_and_scales_width() {
        let mut ctx = RenderContext::new(24, 24);
        ctx.fill_background();
        ctx.fill_rounded_rect(Bounds::new(2, 2, 20, 20), (0x20, 0x40, 0x20, 0xFF), 2.0);
        ctx.stroke_rounded_rect(
            Bounds::new(2, 2, 20, 20),
            (0xE0, 0xA0, 0x20, 0xFF),
            2.0,
            2.0,
        );

        let px = ctx.pixels();
        let border = (12 * 24 + 2) * 4;
        let center = (12 * 24 + 12) * 4;
        assert!(
            px[border] > 0xB0 && px[border + 1] > 0x70 && px[border + 2] < 0x50,
            "border pixel not amber: {:?}",
            &px[border..border + 4]
        );
        assert!(
            px[center] < 0x40 && px[center + 1] > 0x30 && px[center + 2] < 0x40,
            "center fill was overwritten: {:?}",
            &px[center..center + 4]
        );
    }

    #[test]
    fn measure_text_grows_with_content_and_is_zero_for_empty() {
        let mut ctx = RenderContext::new(200, 32);
        assert_eq!(ctx.measure_text(""), 0, "empty text has zero width");
        let one = ctx.measure_text("8");
        let many = ctx.measure_text("88:88:88");
        assert!(one > 0, "a single glyph has nonzero width");
        assert!(many > one, "more text is wider: {many} !> {one}");
    }

    #[test]
    fn scale_factor_defaults_to_one_and_follows_settings() {
        let ctx = RenderContext::new(10, 10);
        assert_eq!(ctx.scale_factor(), 1);
        let scaled = RenderContext::with_settings(
            10,
            10,
            RenderSettings {
                scale: 2,
                ..RenderSettings::default()
            },
        );
        assert_eq!(scaled.scale_factor(), 2);
    }

    #[test]
    fn render_text_draws_some_foreground() {
        // Somewhere in the buffer a glyph must have lightened pixels above the
        // background level, proving text is actually rendered.
        let px = render_text("12:00:00", 320, 32);
        let has_light = px.chunks_exact(4).any(|p| p[0] > 0x60);
        assert!(has_light, "no foreground pixels found");
    }

    #[test]
    fn centered_text_centers_visible_ink_on_both_axes() {
        let settings = RenderSettings {
            background: (0, 0, 0, 0),
            ..RenderSettings::default()
        };
        let mut ctx = RenderContext::with_settings(60, 40, settings);
        let bounds = Bounds::new(7, 3, 40, 30);
        ctx.draw_text_centered("j", bounds, FG);

        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0;
        let mut max_y = 0;
        for (index, pixel) in ctx.pixels().chunks_exact(4).enumerate() {
            if pixel[3] == 0 {
                continue;
            }
            let x = index as u32 % ctx.width();
            let y = index as u32 / ctx.width();
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }

        assert_ne!(min_x, u32::MAX, "centered glyph painted no pixels");
        let ink_center_x_twice = min_x + max_x + 1;
        let ink_center_y_twice = min_y + max_y + 1;
        let bounds_center_x_twice = 2 * bounds.x + bounds.width;
        let bounds_center_y_twice = 2 * bounds.y + bounds.height;
        assert!(ink_center_x_twice.abs_diff(bounds_center_x_twice) <= 1);
        assert!(ink_center_y_twice.abs_diff(bounds_center_y_twice) <= 1);
    }

    #[test]
    fn bounds_contains_is_half_open() {
        let b = Bounds::new(10, 5, 100, 20);
        assert!(b.contains(10, 5), "top-left corner is inside");
        assert!(b.contains(109, 24), "last interior pixel is inside");
        assert!(!b.contains(110, 24), "right edge is exclusive");
        assert!(!b.contains(109, 25), "bottom edge is exclusive");
        assert!(!b.contains(9, 5), "left of origin is outside");
    }

    #[test]
    fn render_context_reuses_pixmap_on_same_size_resize() {
        let mut ctx = RenderContext::new(64, 16);
        ctx.fill_background();
        ctx.resize(64, 16); // no-op
        assert_eq!((ctx.width(), ctx.height()), (64, 16));
        ctx.resize(128, 16); // grows
        assert_eq!((ctx.width(), ctx.height()), (128, 16));
        // Zero dimensions are ignored, leaving the last good size in place.
        ctx.resize(0, 16);
        assert_eq!((ctx.width(), ctx.height()), (128, 16));
        assert_eq!(ctx.pixels().len(), 128 * 16 * 4);
    }

    #[test]
    fn draw_icon_blits_a_centered_image_into_its_slot() {
        // A 2x2 fully-opaque red icon, premultiplied (opaque red is unchanged).
        let red = [255u8, 0, 0, 255];
        let icon: Vec<u8> = red.iter().cycle().take(2 * 2 * 4).copied().collect();
        let mut ctx = RenderContext::new(64, 32);
        ctx.fill_background();
        ctx.draw_icon(&icon, 2, 2, Bounds::new(6, 6, 20, 20));
        let px = ctx.pixels();
        // The icon scales and translates into the requested slot.
        let center = (16 * 64 + 16) * 4;
        assert!(
            px[center] > 0xC0 && px[center + 1] < 0x30 && px[center + 2] < 0x30,
            "icon center not red: {:?}",
            &px[center..center + 4]
        );
        // Pixels immediately outside the offset slot stay untouched.
        let corner = (16 * 64 + 2) * 4;
        assert!(
            px[corner] < 0x30 && px[corner + 1] < 0x30 && px[corner + 2] < 0x30,
            "outside-icon area not background"
        );
    }

    #[test]
    fn draw_icon_ignores_malformed_byte_lengths() {
        // A byte slice too short for the claimed dimensions must be skipped, not
        // panic — malformed tray pixmap data must never crash a draw.
        let mut ctx = RenderContext::new(32, 32);
        ctx.fill_background();
        ctx.draw_icon(&[0xFF, 0x00, 0x00], 4, 4, Bounds::new(0, 0, 32, 32));
        // The whole surface is still the untouched background.
        let px = ctx.pixels();
        assert!(px[0] < 0x30 && px[1] < 0x30 && px[2] < 0x30);
    }

    #[test]
    fn draw_text_offsets_glyphs_by_bounds_origin() {
        // Text drawn into a bound that starts low and to the right must leave the
        // top-left of the surface untouched (still background).
        let mut ctx = RenderContext::new(200, 40);
        ctx.fill_background();
        ctx.draw_text("8", Bounds::new(150, 0, 40, 40), FG);
        let px = ctx.pixels();
        // Top-left pixel is far from the glyph: must remain dark background.
        assert!(
            px[0] < 0x30 && px[1] < 0x30 && px[2] < 0x30,
            "top-left not bg"
        );
        // Somewhere a foreground pixel exists, proving the glyph was drawn.
        assert!(px.chunks_exact(4).any(|p| p[0] > 0x60), "no glyph drawn");
    }
}
