//! Output scale: the single conversion between logical and physical pixels.
//!
//! A Wayland surface is sized in *logical* coordinates, but a shared-memory
//! buffer must hold *physical* device pixels to stay crisp on a scaled (HiDPI)
//! output. [`Scale`] is the one place that conversion happens, so layout math
//! scales exactly once — the surface request and exclusive zone stay logical,
//! while the buffer, render target, layout slots, and font size all go physical
//! through this type. Keeping the conversion in a single, pure value is what
//! prevents the classic double-scaling bug (scaling both the buffer *and* the
//! geometry inside it).
//!
//! The factor is an integer. Compositors advertise an integer buffer scale per
//! surface (`wl_surface` preferred buffer scale / `wl_output` scale); Hyprland
//! reports the ceiling of a fractional scale there, so an integer buffer scale
//! renders crisply on integer outputs and slightly oversized — then downscaled
//! by the compositor — on fractional ones.

/// An integer output scale factor: physical device pixels per logical pixel.
///
/// Always at least `1`. Construct from a compositor-reported factor with
/// [`Scale::new`] (which clamps), or use [`Scale::ONE`] for the unscaled case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale(u32);

impl Scale {
    /// The identity scale: physical pixels equal logical pixels.
    pub const ONE: Scale = Scale(1);

    /// Build a scale from a compositor-reported factor, clamping to a sane
    /// minimum of `1`.
    ///
    /// Compositors report the buffer scale as a positive integer, but a stray
    /// `0` or negative value (e.g. before the first `enter`) must never collapse
    /// the surface — it falls back to [`Scale::ONE`].
    pub fn new(factor: i32) -> Scale {
        Scale(factor.max(1) as u32)
    }

    /// The integer factor (always `>= 1`).
    pub fn get(self) -> u32 {
        self.0
    }

    /// Convert a logical pixel length to physical device pixels.
    pub fn to_physical(self, logical: u32) -> u32 {
        logical.saturating_mul(self.0)
    }

    /// Convert a logical `width * height` to its physical pixel dimensions.
    ///
    /// This is the buffer/render-target size for a surface that occupies
    /// `(width, height)` logical pixels on an output at this scale.
    pub fn to_physical_size(self, width: u32, height: u32) -> (u32, u32) {
        (self.to_physical(width), self.to_physical(height))
    }

    /// Scale a logical font size (in logical pixels) to physical pixels.
    ///
    /// Text is shaped at the physical size so glyphs are rasterized at full
    /// device resolution; combined with physical layout bounds it renders once,
    /// never twice.
    pub fn scale_font(self, size: f32) -> f32 {
        size * self.0 as f32
    }
}

impl Default for Scale {
    fn default() -> Self {
        Scale::ONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_is_the_identity_scale() {
        assert_eq!(Scale::ONE.get(), 1);
        assert_eq!(Scale::ONE.to_physical(32), 32);
        assert_eq!(Scale::ONE.to_physical_size(1920, 32), (1920, 32));
        assert_eq!(Scale::ONE.scale_font(16.0), 16.0);
        assert_eq!(Scale::default(), Scale::ONE);
    }

    #[test]
    fn new_clamps_non_positive_factors_to_one() {
        // A stray 0 or negative factor (e.g. reported before the surface enters
        // an output) must never collapse the surface to zero pixels.
        assert_eq!(Scale::new(0), Scale::ONE);
        assert_eq!(Scale::new(-3), Scale::ONE);
        assert_eq!(Scale::new(1), Scale::ONE);
    }

    #[test]
    fn to_physical_multiplies_logical_by_the_factor() {
        let scale = Scale::new(2);
        assert_eq!(scale.get(), 2);
        assert_eq!(scale.to_physical(32), 64);
        assert_eq!(scale.to_physical(0), 0);
    }

    #[test]
    fn to_physical_size_scales_both_axes() {
        assert_eq!(Scale::new(2).to_physical_size(1920, 32), (3840, 64));
        assert_eq!(Scale::new(3).to_physical_size(100, 10), (300, 30));
    }

    #[test]
    fn scale_font_multiplies_the_logical_size() {
        assert_eq!(Scale::new(2).scale_font(16.0), 32.0);
        assert_eq!(Scale::new(3).scale_font(10.5), 31.5);
    }

    #[test]
    fn to_physical_saturates_instead_of_overflowing() {
        // Pathological widths must not wrap around; saturation keeps the value
        // pinned at the maximum rather than producing a tiny buffer.
        assert_eq!(Scale::new(4).to_physical(u32::MAX), u32::MAX);
    }
}
