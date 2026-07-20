//! RGBA → Wayland shared-memory byte-order conversion.
//!
//! tiny-skia renders into premultiplied RGBA8888, where each pixel is the byte
//! sequence `[R, G, B, A]`. A `wl_shm` buffer created with
//! [`Format::Argb8888`](wayland_client::protocol::wl_shm::Format::Argb8888)
//! stores each pixel as the native-endian 32-bit value `0xAARRGGBB`. On the
//! little-endian platforms we target, that serializes to the byte sequence
//! `[B, G, R, A]`.
//!
//! So committing a software-rendered frame requires swapping the red and blue
//! channels of every pixel; that is exactly what these helpers do.

/// Convert premultiplied RGBA8888 bytes into little-endian `ARGB8888` bytes,
/// returning a freshly allocated buffer.
pub fn rgba_to_argb8888(rgba: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; rgba.len()];
    write_argb8888(rgba, &mut out);
    out
}

/// Convert premultiplied RGBA8888 bytes into little-endian `ARGB8888` bytes,
/// writing into `dst` (typically an `mmap`'d shared-memory canvas).
///
/// Converts `min(rgba.len(), dst.len())` rounded down to whole pixels; any
/// trailing bytes in `dst` are left untouched.
pub fn write_argb8888(rgba: &[u8], dst: &mut [u8]) {
    for (src, out) in rgba.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        out[0] = src[2]; // B
        out[1] = src[1]; // G
        out[2] = src[0]; // R
        out[3] = src[3]; // A
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_red_and_blue_preserving_alpha() {
        let rgba = [0x10, 0x20, 0x30, 0x40, 0xAA, 0xBB, 0xCC, 0xDD];
        let out = rgba_to_argb8888(&rgba);
        assert_eq!(out, vec![0x30, 0x20, 0x10, 0x40, 0xCC, 0xBB, 0xAA, 0xDD]);
    }

    #[test]
    fn write_into_canvas_only_touches_pixel_bytes() {
        let rgba = [1, 2, 3, 4];
        let mut dst = [0xFFu8; 8];
        write_argb8888(&rgba, &mut dst);
        assert_eq!(&dst[..4], &[3, 2, 1, 4]);
        // Trailing pixel in dst is left untouched.
        assert_eq!(&dst[4..], &[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn output_length_matches_input() {
        let rgba = vec![0u8; 320 * 32 * 4];
        assert_eq!(rgba_to_argb8888(&rgba).len(), rgba.len());
    }
}
