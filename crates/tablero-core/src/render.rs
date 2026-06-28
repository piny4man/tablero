use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap};
use tiny_skia::{Paint, Pixmap, Rect, Transform};

/// Opaque dark background color (R, G, B).
pub const BG: (u8, u8, u8) = (0x18, 0x18, 0x18);
/// Light foreground text color (R, G, B).
pub const FG: (u8, u8, u8) = (0xEA, 0xEA, 0xEA);

/// Default font size (px) used when no configuration overrides it.
const FONT_SIZE: f32 = 16.0;

/// The visual settings a [`RenderContext`] paints with: theme colors and font.
///
/// One value resolved once (from configuration, or [`RenderSettings::default`]
/// for the built-in theme) and handed to the context, so widgets read the
/// active foreground/accent through the context rather than baking in constants.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSettings {
    /// Background fill behind every widget.
    pub background: (u8, u8, u8),
    /// Default text color.
    pub foreground: (u8, u8, u8),
    /// Emphasis color (e.g. the active workspace).
    pub accent: (u8, u8, u8),
    /// Text size in pixels.
    pub font_size: f32,
    /// Font family name, or `None` for the system default.
    pub font_family: Option<String>,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            background: BG,
            foreground: FG,
            accent: FG,
            font_size: FONT_SIZE,
            font_family: None,
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
    pub fn foreground(&self) -> (u8, u8, u8) {
        self.settings.foreground
    }

    /// The active accent (emphasis) color.
    pub fn accent(&self) -> (u8, u8, u8) {
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

    /// Fill the whole target with the configured opaque background.
    pub fn fill_background(&mut self) {
        let (r, g, b) = self.settings.background;
        self.pixmap
            .fill(tiny_skia::Color::from_rgba8(r, g, b, 0xFF));
    }

    /// Shape and draw `text` in `color` within `bounds`.
    ///
    /// The text is left-aligned and vertically centered within the bounds (the
    /// line box is the full bound height), and never wraps. Glyphs are clipped
    /// to nothing outside the bounds only insofar as the shaping box limits
    /// them; callers size bounds to fit their content.
    pub fn draw_text(&mut self, text: &str, bounds: Bounds, color: (u8, u8, u8)) {
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

        let text_color = Color::rgb(color.0, color.1, color.2);
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
    fn render_text_draws_some_foreground() {
        // Somewhere in the buffer a glyph must have lightened pixels above the
        // background level, proving text is actually rendered.
        let px = render_text("12:00:00", 320, 32);
        let has_light = px.chunks_exact(4).any(|p| p[0] > 0x60);
        assert!(has_light, "no foreground pixels found");
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
