//! Conversion from raw X11 `GetImage` data into the ARGB32 byte layout that
//! StatusNotifierItem expects.
//!
//! SNI icon pixmaps are 32-bit ARGB in network byte order — i.e. each pixel is
//! the four bytes `[A, R, G, B]`. X servers hand us pixels in their own visual
//! layout and byte order, so we normalise here.

use x11rb::protocol::xproto::Setup;

/// Everything needed to interpret one pixel from an X image.
#[derive(Clone, Copy, Debug)]
pub struct PixelFormat {
    pub depth: u8,
    pub bits_per_pixel: u8,
    pub byte_order_msb: bool,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
}

impl PixelFormat {
    /// Resolve the pixel format for `visual_id` from the server setup.
    ///
    /// Returns `None` if the visual or a matching pixmap format can't be found.
    pub fn for_visual(setup: &Setup, visual_id: u32) -> Option<Self> {
        let mut found = None;
        for screen in &setup.roots {
            for depth in &screen.allowed_depths {
                for visual in &depth.visuals {
                    if visual.visual_id == visual_id {
                        found = Some((depth.depth, visual));
                    }
                }
            }
        }
        let (depth, visual) = found?;
        let bits_per_pixel = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .map(|f| f.bits_per_pixel)?;
        Some(Self {
            depth,
            bits_per_pixel,
            byte_order_msb: setup.image_byte_order
                == x11rb::protocol::xproto::ImageOrder::MSB_FIRST,
            red_mask: visual.red_mask,
            green_mask: visual.green_mask,
            blue_mask: visual.blue_mask,
        })
    }
}

fn shift(mask: u32) -> u32 {
    if mask == 0 { 0 } else { mask.trailing_zeros() }
}

/// Convert raw `ZPixmap` image `data` for a `width`x`height` region into
/// `width*height*4` bytes of ARGB32 (`[A, R, G, B]` per pixel).
///
/// Rows are assumed to be tightly packed at `width * bytes_per_pixel`, which
/// holds for the 32-bpp visuals tray icons use (and the only case we request).
pub fn to_argb32(width: u16, height: u16, data: &[u8], fmt: PixelFormat) -> Vec<u8> {
    let (w, h) = (width as usize, height as usize);
    let mut out = vec![0u8; w * h * 4];
    let bytes_per_pixel = (fmt.bits_per_pixel as usize) / 8;
    if bytes_per_pixel == 0 {
        return out;
    }

    let alpha_mask: u32 = if fmt.depth >= 32 {
        !(fmt.red_mask | fmt.green_mask | fmt.blue_mask)
    } else {
        0
    };
    let (rs, gs, bs, as_) = (
        shift(fmt.red_mask),
        shift(fmt.green_mask),
        shift(fmt.blue_mask),
        shift(alpha_mask),
    );
    let stride = w * bytes_per_pixel;
    let mut any_alpha = false;

    for y in 0..h {
        for x in 0..w {
            let off = y * stride + x * bytes_per_pixel;
            if off + bytes_per_pixel > data.len() {
                continue;
            }
            let mut px: u32 = 0;
            if fmt.byte_order_msb {
                for &b in &data[off..off + bytes_per_pixel] {
                    px = (px << 8) | b as u32;
                }
            } else {
                for (i, &b) in data[off..off + bytes_per_pixel].iter().enumerate() {
                    px |= (b as u32) << (8 * i);
                }
            }
            let r = ((px & fmt.red_mask) >> rs) as u8;
            let g = ((px & fmt.green_mask) >> gs) as u8;
            let b = ((px & fmt.blue_mask) >> bs) as u8;
            let a = if alpha_mask != 0 {
                ((px & alpha_mask) >> as_) as u8
            } else {
                255
            };
            any_alpha |= a != 0;
            let o = (y * w + x) * 4;
            out[o] = a;
            out[o + 1] = r;
            out[o + 2] = g;
            out[o + 3] = b;
        }
    }

    if alpha_mask != 0 {
        // 32-bit source: if alpha was never set (fully transparent), the icon
        // would be invisible — treat it as opaque instead.
        if !any_alpha {
            for o in (0..out.len()).step_by(4) {
                out[o] = 255;
            }
        }
    } else {
        // Opaque source (depth < 32, e.g. a Wine tray icon on a 24-bit window):
        // knock out a uniform background colour so it doesn't render as a
        // solid block behind the icon.
        chroma_key_background(&mut out, w, h);
    }

    out
}

/// If the four corners share one colour, treat it as the background and make
/// every matching pixel transparent. Skips tiny images, and won't blank an icon
/// that is entirely one colour.
fn chroma_key_background(out: &mut [u8], w: usize, h: usize) {
    if w < 4 || h < 4 {
        return;
    }
    let rgb = |x: usize, y: usize| {
        let o = (y * w + x) * 4;
        (out[o + 1], out[o + 2], out[o + 3])
    };
    let bg = rgb(0, 0);
    if rgb(w - 1, 0) != bg || rgb(0, h - 1) != bg || rgb(w - 1, h - 1) != bg {
        return;
    }
    let total = w * h;
    let mut cleared = 0usize;
    for px in out.chunks_exact_mut(4) {
        if (px[1], px[2], px[3]) == bg {
            px[0] = 0;
            cleared += 1;
        }
    }
    // Degenerate case: the whole icon was the background colour — keep it opaque.
    if cleared == total {
        for px in out.chunks_exact_mut(4) {
            px[0] = 255;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Little-endian BGRA (the common depth-24/32 TrueColor layout).
    const FMT: PixelFormat = PixelFormat {
        depth: 32,
        bits_per_pixel: 32,
        byte_order_msb: false,
        red_mask: 0x00ff_0000,
        green_mask: 0x0000_ff00,
        blue_mask: 0x0000_00ff,
    };

    #[test]
    fn converts_bgra_le_to_argb() {
        // One pixel, value 0xAARRGGBB = 0x8012_3456 stored little-endian.
        let data = [0x56u8, 0x34, 0x12, 0x80];
        let out = to_argb32(1, 1, &data, FMT);
        assert_eq!(out, vec![0x80, 0x12, 0x34, 0x56]);
    }

    #[test]
    fn depth24_is_opaque() {
        let fmt = PixelFormat { depth: 24, ..FMT };
        let data = [0x56u8, 0x34, 0x12, 0x00];
        let out = to_argb32(1, 1, &data, fmt);
        assert_eq!(out, vec![0xff, 0x12, 0x34, 0x56]);
    }

    #[test]
    fn all_transparent_becomes_opaque() {
        let data = [0x10u8, 0x20, 0x30, 0x00, 0x40, 0x50, 0x60, 0x00];
        let out = to_argb32(2, 1, &data, FMT);
        assert_eq!(out[0], 0xff);
        assert_eq!(out[4], 0xff);
    }

    #[test]
    fn chroma_keys_uniform_background() {
        // 4x4, depth 24, black everywhere except a blue centre pixel.
        let fmt = PixelFormat { depth: 24, ..FMT };
        let black = [0x00u8, 0x00, 0x00, 0x00]; // B,G,R,x little-endian
        let blue = [0xffu8, 0x00, 0x00, 0x00]; // B=255
        let mut data = Vec::new();
        for i in 0..16 {
            data.extend_from_slice(if i == 5 { &blue } else { &black });
        }
        let out = to_argb32(4, 4, &data, fmt);
        // Corner (0,0) background is now transparent, centre pixel stays opaque.
        assert_eq!(out[0], 0x00, "background should be transparent");
        assert_eq!(out[5 * 4], 0xff, "foreground should stay opaque");
        assert_eq!(&out[5 * 4 + 1..5 * 4 + 4], &[0x00, 0x00, 0xff]);
    }
}
