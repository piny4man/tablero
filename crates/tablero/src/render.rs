use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap};
use tiny_skia::{
    FillRule, FilterQuality, Paint, Path, PathBuilder, Pixmap, PixmapPaint, PixmapRef, Rect,
    Stroke, Transform,
};

use crate::icon::BuiltinIcon;

/// Shared font machinery for every bar surface and popup on the render thread.
///
/// Loading a [`FontSystem`] scans the system font set once; sharing one instance
/// (plus its [`SwashCache`]) across all [`RenderContext`]s avoids repeating that
/// work per monitor and per tooltip/tray popup. Rendered on the calloop thread
/// only — not `Send`/`Sync`.
pub struct FontResources {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl FontResources {
    /// Load system fonts and build an empty glyph cache.
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        }
    }
}

impl Default for FontResources {
    fn default() -> Self {
        Self::new()
    }
}

/// Reference-counted font machinery shared by every [`RenderContext`].
pub type SharedFonts = Rc<RefCell<FontResources>>;

/// Build a new shared font set for one bar process.
pub fn shared_fonts() -> SharedFonts {
    Rc::new(RefCell::new(FontResources::new()))
}

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

    /// The smallest rectangle covering both `self` and `other`.
    pub fn union(&self, other: &Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Self::new(x, y, right - x, bottom - y)
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

/// Work counters for the frames painted since the last
/// [`take_stats`](RenderContext::take_stats).
///
/// Plain integer increments, so they stay on in the normal path; the
/// diagnostics logger reads them to attribute a frame's cost to shaping or
/// glyph blitting without reading the clock inside the text primitives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderStats {
    /// Strings shaped from scratch (a text-cache miss).
    pub text_shapes: u32,
    /// Strings served from the text cache without shaping.
    pub text_cache_hits: u32,
    /// Glyph coverage pixels blended into the target.
    pub glyph_pixels: u32,
}

/// Upper bound on cached shaped strings per context. Values such as CPU
/// percentages churn through many short strings; when the bound is hit the
/// cache is dropped wholesale and the live strings re-shape once.
const TEXT_CACHE_LIMIT: usize = 256;

/// Upper bound on cached built-in icon rasters per context. A bar shows a
/// couple of dozen at most; theme or scale churn beyond that drops the cache.
const ICON_CACHE_LIMIT: usize = 64;

/// Identity of one rasterised built-in icon: artwork, slot size and
/// straight-alpha color.
type IconKey = (BuiltinIcon, (u32, u32), (u8, u8, u8, u8));

/// The shaping box a string was laid out in. Font size and family live in
/// [`RenderSettings`], and the cache is cleared whenever those change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ShapeKey {
    /// `f32::to_bits` of the line height.
    line_height: u32,
    /// Constrained box, or `None` for an unconstrained measurement.
    size: Option<(u32, u32)>,
}

/// Ink extents `(left, top, right, bottom)` of a shaped string, in buffer
/// coordinates.
type InkBounds = (i32, i32, i32, i32);

/// One shaped string, kept so an unchanged label is neither re-shaped between
/// layout and draw nor between frames.
struct ShapedText {
    buffer: Buffer,
    /// Widest layout run, rounded up.
    width: u32,
    /// Lazily computed visible-ink extents (`Some(None)` for inkless text).
    ink: Option<Option<InkBounds>>,
}

#[derive(Default)]
struct TextCache {
    by_shape: HashMap<ShapeKey, HashMap<String, ShapedText>>,
    len: usize,
}

impl TextCache {
    fn clear(&mut self) {
        self.by_shape.clear();
        self.len = 0;
    }

    /// The shaped form of `text` in the `key` box, shaping it on a miss.
    fn get_or_shape(
        &mut self,
        font_system: &mut FontSystem,
        settings: &RenderSettings,
        stats: &mut RenderStats,
        text: &str,
        key: ShapeKey,
    ) -> &mut ShapedText {
        let cached = self
            .by_shape
            .get(&key)
            .is_some_and(|texts| texts.contains_key(text));
        if cached {
            stats.text_cache_hits += 1;
        } else {
            if self.len >= TEXT_CACHE_LIMIT {
                self.clear();
            }
            stats.text_shapes += 1;
            let shaped = shape_text(font_system, settings, text, key);
            self.by_shape
                .entry(key)
                .or_default()
                .insert(text.to_owned(), shaped);
            self.len += 1;
        }
        self.by_shape
            .get_mut(&key)
            .and_then(|texts| texts.get_mut(text))
            .expect("entry was just found or inserted")
    }
}

fn shape_text(
    font_system: &mut FontSystem,
    settings: &RenderSettings,
    text: &str,
    key: ShapeKey,
) -> ShapedText {
    let metrics = Metrics::new(settings.font_size, f32::from_bits(key.line_height));
    let mut buffer = Buffer::new(font_system, metrics);
    let (width, height) = match key.size {
        Some((width, height)) => (Some(width as f32), Some(height as f32)),
        // Unconstrained box: the shaped line keeps its full advance width.
        None => (None, None),
    };
    buffer.set_size(width, height);
    buffer.set_wrap(Wrap::None);

    let mut attrs = Attrs::new();
    if let Some(family) = &settings.font_family {
        attrs = attrs.family(Family::Name(family));
    }
    buffer.set_text(text, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(font_system, false);

    let width = buffer
        .layout_runs()
        .map(|run| run.line_w)
        .fold(0.0_f32, f32::max)
        .ceil() as u32;
    ShapedText {
        buffer,
        width,
        ink: None,
    }
}

/// Rounded `value / 255` for `value <= 255 * 255`.
fn div255(value: u32) -> u32 {
    (value + 127) / 255
}

/// Source-over blend a straight-alpha `rgba` rectangle into premultiplied
/// RGBA8888 `data` (`width * height` pixels), clipped to the target.
///
/// This is the glyph blitter: cosmic-text reports coverage as 1x1 rectangles,
/// including fully transparent ones across each glyph's bounding box, so the
/// zero-alpha early-out and the absence of any per-call paint setup are what
/// keep text cheap. Returns the number of pixels written.
fn blend_rect(
    data: &mut [u8],
    width: u32,
    height: u32,
    (x, y): (i64, i64),
    (w, h): (u32, u32),
    rgba: [u8; 4],
) -> u32 {
    let alpha = u32::from(rgba[3]);
    if alpha == 0 {
        return 0;
    }
    let x0 = x.clamp(0, i64::from(width)) as usize;
    let y0 = y.clamp(0, i64::from(height)) as usize;
    let x1 = x.saturating_add(i64::from(w)).clamp(0, i64::from(width)) as usize;
    let y1 = y.saturating_add(i64::from(h)).clamp(0, i64::from(height)) as usize;
    if x0 >= x1 || y0 >= y1 {
        return 0;
    }

    let src = [
        div255(u32::from(rgba[0]) * alpha),
        div255(u32::from(rgba[1]) * alpha),
        div255(u32::from(rgba[2]) * alpha),
        alpha,
    ];
    let inverse = 255 - alpha;
    let stride = width as usize * 4;
    for row in y0..y1 {
        let line = &mut data[row * stride + x0 * 4..row * stride + x1 * 4];
        for pixel in line.as_chunks_mut::<4>().0 {
            for (dst, src) in pixel.iter_mut().zip(src) {
                *dst = (src + div255(u32::from(*dst) * inverse)) as u8;
            }
        }
    }
    ((x1 - x0) * (y1 - y0)) as u32
}

/// A reusable software-render target shared across widgets within one frame.
///
/// Holds a [`SharedFonts`] handle (system font set + swash cache), a
/// destination `Pixmap`, and a cache of shaped strings so a redraw neither
/// reallocates them nor re-shapes unchanged labels every tick.
/// Widgets clear the background once, then each draws its text into its own
/// [`Bounds`]; the finished frame is read back as premultiplied RGBA8888 via
/// [`pixels`](RenderContext::pixels).
pub struct RenderContext {
    fonts: SharedFonts,
    pixmap: Pixmap,
    settings: RenderSettings,
    text_cache: TextCache,
    /// Built-in icons rasterised at their slot size, so a steady frame
    /// composites them instead of rebuilding and anti-aliasing vector paths.
    icon_cache: HashMap<IconKey, Pixmap>,
    stats: RenderStats,
}

impl RenderContext {
    /// Create a context targeting a `width * height` pixel surface, using the
    /// built-in theme ([`RenderSettings::default`]) and a private font set.
    ///
    /// Prefer [`with_fonts`](Self::with_fonts) in the live bar so every surface
    /// shares one [`FontSystem`].
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_settings(width, height, RenderSettings::default())
    }

    /// Create a context targeting a `width * height` pixel surface, painting with
    /// the supplied [`RenderSettings`] and a private font set.
    pub fn with_settings(width: u32, height: u32, settings: RenderSettings) -> Self {
        Self::with_fonts(width, height, settings, shared_fonts())
    }

    /// Create a context that paints with `settings` and the process-wide
    /// [`SharedFonts`] (one font load for every monitor and popup).
    pub fn with_fonts(
        width: u32,
        height: u32,
        settings: RenderSettings,
        fonts: SharedFonts,
    ) -> Self {
        let pixmap = Pixmap::new(width.max(1), height.max(1)).expect("non-zero pixmap dimensions");
        Self {
            fonts,
            pixmap,
            settings,
            text_cache: TextCache::default(),
            icon_cache: HashMap::new(),
            stats: RenderStats::default(),
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
    /// must follow it. Cheap: the shared font machinery is retained, only the
    /// visual settings are swapped. Shaped text depends on the font, so the
    /// text cache is dropped when the font size or family changes.
    pub fn set_settings(&mut self, settings: RenderSettings) {
        if settings.font_size != self.settings.font_size
            || settings.font_family != self.settings.font_family
        {
            self.text_cache.clear();
        }
        self.settings = settings;
    }

    /// Return the work counters accumulated since the last call and reset them.
    pub fn take_stats(&mut self) -> RenderStats {
        std::mem::take(&mut self.stats)
    }

    /// The shared font resources this context paints with.
    pub fn fonts(&self) -> &SharedFonts {
        &self.fonts
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
        let offset = (i64::from(bounds.x), i64::from(bounds.y));
        self.blit_text(text, bounds, color, |_| Some(offset));
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
        self.blit_text(text, bounds, color, |ink| {
            let (left, top, right, bottom) = ink?;
            let ink_width = i64::from(right) - i64::from(left);
            let ink_height = i64::from(bottom) - i64::from(top);
            Some((
                i64::from(bounds.x) + (i64::from(bounds.width) - ink_width) / 2 - i64::from(left),
                i64::from(bounds.y) + (i64::from(bounds.height) - ink_height) / 2 - i64::from(top),
            ))
        });
    }

    /// Blend the (cached) shaped `text` into the target at the offset `place`
    /// derives from its ink extents; `None` draws nothing.
    ///
    /// The ink extents cost a pass over the glyph grid, so they are computed
    /// only when `place` is handed them lazily — once per cached string.
    fn blit_text(
        &mut self,
        text: &str,
        bounds: Bounds,
        color: (u8, u8, u8, u8),
        place: impl FnOnce(Option<InkBounds>) -> Option<(i64, i64)>,
    ) {
        let mut fonts = self.fonts.borrow_mut();
        let FontResources {
            font_system,
            swash_cache,
        } = &mut *fonts;
        let key = ShapeKey {
            line_height: (bounds.height as f32).to_bits(),
            size: Some((bounds.width, bounds.height)),
        };
        let shaped =
            self.text_cache
                .get_or_shape(font_system, &self.settings, &mut self.stats, text, key);

        let text_color = Color::rgba(color.0, color.1, color.2, color.3);
        let ink = *shaped.ink.get_or_insert_with(|| {
            let mut ink: Option<InkBounds> = None;
            shaped
                .buffer
                .draw(font_system, swash_cache, text_color, |x, y, w, h, _| {
                    let right = x.saturating_add(i32::try_from(w).unwrap_or(i32::MAX));
                    let bottom = y.saturating_add(i32::try_from(h).unwrap_or(i32::MAX));
                    ink = Some(match ink {
                        Some((left, top, old_right, old_bottom)) => (
                            left.min(x),
                            top.min(y),
                            old_right.max(right),
                            old_bottom.max(bottom),
                        ),
                        None => (x, y, right, bottom),
                    });
                });
            ink
        });
        let Some((ox, oy)) = place(ink) else {
            return;
        };

        let (width, height) = (self.pixmap.width(), self.pixmap.height());
        let data = self.pixmap.data_mut();
        let mut blended = 0;
        shaped
            .buffer
            .draw(font_system, swash_cache, text_color, |x, y, w, h, color| {
                blended += blend_rect(
                    data,
                    width,
                    height,
                    (ox + i64::from(x), oy + i64::from(y)),
                    (w, h),
                    color.as_rgba(),
                );
            });
        self.stats.glyph_pixels += blended;
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

        let mut fonts = self.fonts.borrow_mut();
        let key = ShapeKey {
            line_height: self.settings.font_size.to_bits(),
            size: None,
        };
        self.text_cache
            .get_or_shape(
                &mut fonts.font_system,
                &self.settings,
                &mut self.stats,
                text,
                key,
            )
            .width
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
        let key = (icon, (bounds.width, bounds.height), color);
        if !self.icon_cache.contains_key(&key) {
            let Some(mut raster) = Pixmap::new(bounds.width, bounds.height) else {
                return;
            };
            let slot = Bounds::new(0, 0, bounds.width, bounds.height);
            crate::icon::draw_into(&mut raster, icon, slot, color);
            if self.icon_cache.len() >= ICON_CACHE_LIMIT {
                self.icon_cache.clear();
            }
            self.icon_cache.insert(key, raster);
        }
        let (Some(raster), Ok(x), Ok(y)) = (
            self.icon_cache.get(&key),
            i32::try_from(bounds.x),
            i32::try_from(bounds.y),
        ) else {
            return;
        };
        self.pixmap.as_mut().draw_pixmap(
            x,
            y,
            raster.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
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
        let has_light = px.as_chunks::<4>().0.iter().any(|p| p[0] > 0x60);
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
        for (index, pixel) in ctx.pixels().as_chunks::<4>().0.iter().enumerate() {
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
        assert!(
            px.as_chunks::<4>().0.iter().any(|p| p[0] > 0x60),
            "no glyph drawn"
        );
    }

    /// The glyph path this module used before [`blend_rect`]: one tiny-skia
    /// `fill_rect` per coverage rectangle. Kept only as the golden reference.
    fn reference_draw_text(
        ctx: &mut RenderContext,
        text: &str,
        bounds: Bounds,
        color: (u8, u8, u8, u8),
    ) {
        let fonts = ctx.fonts.clone();
        let mut fonts = fonts.borrow_mut();
        let FontResources {
            font_system,
            swash_cache,
        } = &mut *fonts;
        let key = ShapeKey {
            line_height: (bounds.height as f32).to_bits(),
            size: Some((bounds.width, bounds.height)),
        };
        let mut shaped = shape_text(font_system, &ctx.settings, text, key);
        let text_color = Color::rgba(color.0, color.1, color.2, color.3);
        let (ox, oy) = (bounds.x as f32, bounds.y as f32);
        let dst = &mut ctx.pixmap;
        shaped
            .buffer
            .draw(font_system, swash_cache, text_color, |x, y, w, h, color| {
                let Some(rect) = Rect::from_xywh(ox + x as f32, oy + y as f32, w as f32, h as f32)
                else {
                    return;
                };
                let mut paint = Paint::default();
                paint.set_color_rgba8(color.r(), color.g(), color.b(), color.a());
                dst.fill_rect(rect, &paint, Transform::default(), None);
            });
    }

    fn assert_pixels_within_one(actual: &[u8], expected: &[u8]) {
        assert_eq!(actual.len(), expected.len());
        let worst = actual
            .iter()
            .zip(expected)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap_or(0);
        assert!(worst <= 1, "channel differs by {worst}");
    }

    #[test]
    fn draw_text_matches_the_fill_rect_reference_on_opaque_and_clear_backgrounds() {
        let transparent = RenderSettings {
            background: (0, 0, 0, 0),
            ..RenderSettings::default()
        };
        for settings in [RenderSettings::default(), transparent] {
            let bounds = Bounds::new(7, 3, 180, 34);
            let mut expected = RenderContext::with_settings(200, 40, settings.clone());
            expected.fill_background();
            reference_draw_text(&mut expected, "Wg 12:34 é", bounds, FG);

            let mut actual = RenderContext::with_settings(200, 40, settings);
            actual.fill_background();
            actual.draw_text("Wg 12:34 é", bounds, FG);

            assert!(actual.take_stats().glyph_pixels > 0, "nothing was blended");
            assert_pixels_within_one(actual.pixels(), expected.pixels());
        }
    }

    #[test]
    fn blend_rect_skips_transparent_coverage() {
        let mut data = [9_u8; 4];
        assert_eq!(
            blend_rect(&mut data, 1, 1, (0, 0), (1, 1), [255, 255, 255, 0]),
            0
        );
        assert_eq!(data, [9; 4]);
    }

    #[test]
    fn blend_rect_composites_source_over_premultiplied() {
        // Opaque source replaces the destination outright.
        let mut data = [10, 20, 30, 255];
        blend_rect(&mut data, 1, 1, (0, 0), (1, 1), [200, 100, 50, 255]);
        assert_eq!(data, [200, 100, 50, 255]);

        // Half coverage over transparent stores the premultiplied source.
        let mut data = [0; 4];
        blend_rect(&mut data, 1, 1, (0, 0), (1, 1), [255, 255, 255, 128]);
        assert_eq!(data, [128; 4]);
    }

    #[test]
    fn blend_rect_clips_to_the_target_without_panicking() {
        let mut data = [0_u8; 2 * 2 * 4];
        let white = [255; 4];
        // Overhangs the top-left corner: only pixel (0, 0) is inside.
        assert_eq!(blend_rect(&mut data, 2, 2, (-3, -3), (4, 4), white), 1);
        assert_eq!(data[..4], [255; 4]);
        assert_eq!(data[4..], [0; 12]);
        // Entirely outside, on every side and at the numeric extremes.
        for origin in [
            (2, 0),
            (0, 2),
            (-1, 0),
            (0, -1),
            (i64::MAX, 0),
            (i64::MIN, 0),
        ] {
            assert_eq!(blend_rect(&mut data, 2, 2, origin, (1, 1), white), 0);
        }
    }

    #[test]
    fn text_overhanging_the_surface_is_clipped() {
        let mut ctx = RenderContext::new(20, 10);
        ctx.fill_background();
        ctx.draw_text("clipped", Bounds::new(15, 8, 400, 40), FG);
        ctx.draw_text_centered("W", Bounds::new(10, 2, 400, 40), FG);
    }

    #[test]
    fn repeated_text_is_shaped_once_per_shaping_box() {
        let mut ctx = RenderContext::new(200, 40);
        let bounds = Bounds::new(0, 0, 200, 40);

        let first = ctx.measure_text("42%");
        ctx.draw_text("42%", bounds, FG);
        // One shape for the unconstrained measurement, one for the draw box.
        assert_eq!(ctx.take_stats().text_shapes, 2);

        assert_eq!(ctx.measure_text("42%"), first);
        ctx.draw_text("42%", bounds, FG);
        ctx.draw_text_centered("42%", bounds, FG);
        let stats = ctx.take_stats();
        assert_eq!((stats.text_shapes, stats.text_cache_hits), (0, 3));
    }

    #[test]
    fn cached_text_draws_the_same_pixels_as_freshly_shaped_text() {
        let bounds = Bounds::new(4, 0, 100, 40);
        let mut ctx = RenderContext::new(120, 40);
        ctx.fill_background();
        ctx.draw_text_centered("A", bounds, FG);
        let fresh = ctx.pixels().to_vec();

        ctx.fill_background();
        ctx.draw_text_centered("A", bounds, FG);
        assert_eq!(ctx.pixels(), fresh);
    }

    #[test]
    fn changing_the_font_drops_shaped_text_but_a_recolor_keeps_it() {
        let mut ctx = RenderContext::new(200, 40);
        let small = ctx.measure_text("resize me");

        ctx.set_settings(RenderSettings {
            accent: (1, 2, 3, 255),
            ..ctx.settings.clone()
        });
        ctx.measure_text("resize me");
        assert_eq!(ctx.take_stats().text_cache_hits, 1);

        ctx.set_settings(RenderSettings {
            font_size: ctx.settings.font_size * 2.0,
            ..ctx.settings.clone()
        });
        assert!(ctx.measure_text("resize me") > small);
        assert_eq!(ctx.take_stats().text_shapes, 1);
    }

    #[test]
    fn text_cache_never_exceeds_its_bound() {
        let mut ctx = RenderContext::new(1, 1);
        for value in 0..TEXT_CACHE_LIMIT * 2 + 1 {
            ctx.measure_text(&value.to_string());
            assert!(ctx.text_cache.len <= TEXT_CACHE_LIMIT);
        }
        let entries: usize = ctx.text_cache.by_shape.values().map(HashMap::len).sum();
        assert_eq!(entries, ctx.text_cache.len);
    }

    #[test]
    fn cached_builtin_icon_matches_the_direct_vector_fill() {
        let bounds = Bounds::new(9, 5, 24, 24);
        let mut expected = RenderContext::new(48, 36);
        expected.fill_background();
        crate::icon::draw_into(&mut expected.pixmap, BuiltinIcon::Clock, bounds, FG);

        let mut actual = RenderContext::new(48, 36);
        // Twice: the first call rasterises, the second composites the cache.
        for _ in 0..2 {
            actual.fill_background();
            actual.draw_builtin_icon(BuiltinIcon::Clock, bounds, FG);
            assert_pixels_within_one(actual.pixels(), expected.pixels());
        }
        assert_eq!(actual.icon_cache.len(), 1);
        assert_ne!(actual.pixels(), RenderContext::new(48, 36).pixels());
    }

    #[test]
    fn builtin_icon_cache_distinguishes_color_and_size_and_stays_bounded() {
        let mut ctx = RenderContext::new(64, 64);
        ctx.draw_builtin_icon(BuiltinIcon::Clock, Bounds::new(0, 0, 16, 16), FG);
        ctx.draw_builtin_icon(
            BuiltinIcon::Clock,
            Bounds::new(0, 0, 16, 16),
            (1, 2, 3, 255),
        );
        ctx.draw_builtin_icon(BuiltinIcon::Clock, Bounds::new(0, 0, 20, 20), FG);
        assert_eq!(ctx.icon_cache.len(), 3);

        for edge in 1..=(ICON_CACHE_LIMIT as u32 * 2) {
            ctx.draw_builtin_icon(BuiltinIcon::Clock, Bounds::new(0, 0, edge, edge), FG);
            assert!(ctx.icon_cache.len() <= ICON_CACHE_LIMIT);
        }
    }
}
