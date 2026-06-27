use cosmic_text::{Attrs, Buffer, Color, FontSystem, Metrics, Shaping, SwashCache, Wrap};
use tiny_skia::{Paint, Pixmap, Rect, Transform};

/// Opaque dark background color (R, G, B).
const BG: (u8, u8, u8) = (0x18, 0x18, 0x18);
/// Light foreground text color (R, G, B).
const FG: (u8, u8, u8) = (0xEA, 0xEA, 0xEA);

/// Render `text` over a dark background into a `width * height` pixel buffer.
///
/// Returns premultiplied RGBA8888 bytes (the native tiny-skia layout: each
/// pixel is `[R, G, B, A]`), of length `width * height * 4`. Convert to the
/// Wayland shared-memory layout with [`crate::blit::write_argb8888`] before
/// committing the buffer.
pub fn render_text(text: &str, width: u32, height: u32) -> Vec<u8> {
    let mut font_system = FontSystem::new();
    let mut swash_cache = SwashCache::new();

    // Use the full surface height as the line box so the single line of text is
    // vertically centered within the bar.
    let metrics = Metrics::new(16.0, height as f32);
    let mut buffer = Buffer::new(&mut font_system, metrics);
    buffer.set_size(Some(width as f32), Some(height as f32));
    buffer.set_wrap(Wrap::None);

    let attrs = Attrs::new();
    buffer.set_text(text, &attrs, Shaping::Advanced, None);
    buffer.shape_until_scroll(&mut font_system, false);

    let mut pixmap = Pixmap::new(width, height).expect("non-zero pixmap dimensions");
    pixmap.fill(tiny_skia::Color::from_rgba8(BG.0, BG.1, BG.2, 0xFF));

    let text_color = Color::rgb(FG.0, FG.1, FG.2);
    {
        let mut dst = pixmap.as_mut();
        buffer.draw(
            &mut font_system,
            &mut swash_cache,
            text_color,
            |x, y, w, h, color| {
                let Some(rect) = Rect::from_xywh(x as f32, y as f32, w as f32, h as f32) else {
                    return;
                };
                let mut paint = Paint::default();
                let rgba = color.as_rgba();
                paint.set_color_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]);
                dst.fill_rect(rect, &paint, Transform::default(), None);
            },
        );
    }

    pixmap.take()
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
}
