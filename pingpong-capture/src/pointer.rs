//! The host's pointer, drawn into the picture: the platform-free half.
//!
//! Desktop Duplication leaves the pointer out of the desktop image and hands
//! it over beside each frame -- where it is, whether it shows, and, when it
//! changes, its shape. Drawing it in is the capturer's job, and Sunshine does
//! it (`display_vram.cpp`), so the pointer a client sees is the one the
//! host's screen shows: hidden when a game hides it, wherever a game, the
//! Steam overlay or a controller puts it. An earlier pingpong drew the
//! pointer on the client instead, from what the host's foreground
//! application said it was doing with it; that disagreed with the screen
//! (a pointer left over after the Steam overlay closed, a drawn pointer away
//! from the one clicking), and reading it meant joining the application's
//! input queue, which games noticed.
//!
//! A shape comes in one of three kinds (`ShapeKind`). Each is decoded here
//! into two layers of the same size: one blended onto the desktop by its
//! alpha, and one XORed into the result -- the inverting part of a
//! monochrome pointer, like the I-beam's. No pixel uses both, which is what
//! lets one rule (`composite`) draw all three kinds. The GPU applies that
//! rule in `overlay` (Windows); it lives here so it is tested on any
//! machine.

/// A pointer shape's kind, numbered as `DXGI_OUTDUPL_POINTER_SHAPE_TYPE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    /// One bit a pixel: an AND mask, then an XOR mask below it (the shape's
    /// reported height counts both).
    Monochrome,
    /// BGRA, blended by its alpha.
    Color,
    /// BGRA whose alpha is a mask: 0 puts the colour on the screen, 0xFF
    /// XORs it into what is there.
    MaskedColor,
}

impl ShapeKind {
    pub fn from_dxgi(kind: u32) -> Option<ShapeKind> {
        match kind {
            1 => Some(ShapeKind::Monochrome),
            2 => Some(ShapeKind::Color),
            4 => Some(ShapeKind::MaskedColor),
            _ => None,
        }
    }
}

/// A pointer shape decoded for drawing: `width` x `height` BGRA pixels of
/// the blend layer, then as many of the XOR layer (its alpha unused).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointerImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// The largest side a pointer shape is taken with. Windows' own top out at
/// 256 (the largest accessibility size); anything past this is a corrupt
/// report, not a pointer.
const MAX_SIDE: u32 = 1024;

impl PointerImage {
    /// Decode a shape as Desktop Duplication reports it: `height` as
    /// reported (twice the pointer's for a monochrome one), `pitch` bytes
    /// from one row to the next. `None` for a shape the data cannot hold.
    pub fn decode(
        kind: ShapeKind,
        width: u32,
        height: u32,
        pitch: u32,
        data: &[u8],
    ) -> Option<PointerImage> {
        let height = match kind {
            ShapeKind::Monochrome => height / 2,
            ShapeKind::Color | ShapeKind::MaskedColor => height,
        };
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
            return None;
        }
        let (w, h, pitch) = (width as usize, height as usize, pitch as usize);
        let rows = match kind {
            ShapeKind::Monochrome => 2 * h,
            ShapeKind::Color | ShapeKind::MaskedColor => h,
        };
        let row_bytes = match kind {
            ShapeKind::Monochrome => w.div_ceil(8),
            ShapeKind::Color | ShapeKind::MaskedColor => w * 4,
        };
        if pitch < row_bytes || data.len() < (rows - 1) * pitch + row_bytes {
            return None;
        }

        let layer = w * h * 4;
        let mut pixels = vec![0u8; 2 * layer];
        let (blend, xor) = pixels.split_at_mut(layer);
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                match kind {
                    ShapeKind::Monochrome => {
                        let bit = |row: usize| data[row * pitch + x / 8] >> (7 - x % 8) & 1;
                        // AND 0 paints black over the screen (then XOR 1
                        // turns it white); AND 1 leaves the screen (then
                        // XOR 1 inverts it).
                        if bit(y) == 0 {
                            blend[i + 3] = 0xFF;
                        }
                        if bit(h + y) == 1 {
                            xor[i..i + 3].fill(0xFF);
                        }
                    }
                    ShapeKind::Color => {
                        let p = y * pitch + x * 4;
                        blend[i..i + 4].copy_from_slice(&data[p..p + 4]);
                    }
                    ShapeKind::MaskedColor => {
                        let p = y * pitch + x * 4;
                        if data[p + 3] == 0 {
                            blend[i..i + 3].copy_from_slice(&data[p..p + 3]);
                            blend[i + 3] = 0xFF;
                        } else {
                            xor[i..i + 3].copy_from_slice(&data[p..p + 3]);
                        }
                    }
                }
            }
        }
        Some(PointerImage {
            width,
            height,
            pixels,
        })
    }

    /// The blend layer's pixel at (`x`, `y`), BGRA.
    pub fn blend_at(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        self.pixels[i..i + 4].try_into().expect("four bytes")
    }

    /// The XOR layer's pixel at (`x`, `y`), BGRA.
    pub fn xor_at(&self, x: u32, y: u32) -> [u8; 4] {
        self.blend_at(x, y + self.height)
    }
}

/// One screen pixel under one pointer pixel, BGRA: the blend layer over the
/// screen by its alpha, then the XOR layer's colour XORed in. The overlay's
/// pixel shader computes the same, rounding the blend to the nearest step.
pub fn composite(screen: [u8; 4], blend: [u8; 4], xor: [u8; 4]) -> [u8; 4] {
    let a = blend[3] as u32;
    let mut out = [0u8, 0, 0, 0xFF];
    for c in 0..3 {
        let mixed = (screen[c] as u32 * (255 - a) + blend[c] as u32 * a + 127) / 255;
        out[c] = mixed as u8 ^ xor[c];
    }
    out
}

/// Where Desktop Duplication last put the pointer, and whether it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PointerPlace {
    pub visible: bool,
    /// The shape's top-left on the captured display, in its pixels (hot spot
    /// already applied; negative past the left or top edge).
    pub x: i32,
    pub y: i32,
}

impl PointerPlace {
    /// Whether going from `self` to `next` changes the picture: a pointer
    /// that shows and moves, appears or goes. A hidden one moving does not.
    pub fn redraw_for(&self, next: PointerPlace) -> bool {
        match (self.visible, next.visible) {
            (false, false) => false,
            (true, true) => (self.x, self.y) != (next.x, next.y),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GREY: [u8; 4] = [0x40, 0x80, 0xC0, 0xFF];

    /// A 2x1 monochrome shape: one pixel per (AND, XOR) pair, `pitch` 4.
    fn monochrome(pairs: [(u8, u8); 2]) -> PointerImage {
        let and = pairs[0].0 << 7 | pairs[1].0 << 6;
        let xor = pairs[0].1 << 7 | pairs[1].1 << 6;
        let data = [and, 0, 0, 0, xor, 0, 0, 0];
        PointerImage::decode(ShapeKind::Monochrome, 2, 2, 4, &data).expect("decodes")
    }

    fn draw(image: &PointerImage, x: u32, screen: [u8; 4]) -> [u8; 4] {
        composite(screen, image.blend_at(x, 0), image.xor_at(x, 0))
    }

    #[test]
    fn a_monochrome_pointer_keeps_paints_and_inverts_the_screen() {
        let clear_and_black = monochrome([(1, 0), (0, 0)]);
        assert_eq!((clear_and_black.width, clear_and_black.height), (2, 1));
        assert_eq!(
            draw(&clear_and_black, 0, GREY),
            GREY,
            "AND 1, XOR 0: the screen"
        );
        assert_eq!(
            draw(&clear_and_black, 1, GREY),
            [0, 0, 0, 0xFF],
            "AND 0, XOR 0: black"
        );

        let white_and_invert = monochrome([(0, 1), (1, 1)]);
        assert_eq!(
            draw(&white_and_invert, 0, GREY),
            [0xFF, 0xFF, 0xFF, 0xFF],
            "AND 0, XOR 1: white"
        );
        assert_eq!(
            draw(&white_and_invert, 1, GREY),
            [0xBF, 0x7F, 0x3F, 0xFF],
            "AND 1, XOR 1: the screen inverted (the I-beam over text)"
        );
    }

    #[test]
    fn a_colour_pointer_blends_by_its_alpha() {
        let half_red = [0, 0, 0xFF, 0x80];
        let data = [half_red, [0, 0, 0, 0]].concat();
        let image = PointerImage::decode(ShapeKind::Color, 2, 1, 8, &data).expect("decodes");
        assert_eq!(draw(&image, 0, [0, 0, 0, 0xFF]), [0, 0, 0x80, 0xFF]);
        assert_eq!(draw(&image, 1, GREY), GREY, "transparent");
    }

    #[test]
    fn a_masked_colour_pointer_replaces_or_xors_by_its_mask() {
        let data = [[0x10, 0x20, 0x30, 0x00], [0xFF, 0x00, 0xFF, 0xFF]].concat();
        let image = PointerImage::decode(ShapeKind::MaskedColor, 2, 1, 8, &data).expect("decodes");
        assert_eq!(
            draw(&image, 0, GREY),
            [0x10, 0x20, 0x30, 0xFF],
            "mask 0: replaced"
        );
        assert_eq!(
            draw(&image, 1, GREY),
            [0xBF, 0x80, 0x3F, 0xFF],
            "mask 0xFF: XORed"
        );
    }

    #[test]
    fn rows_are_read_at_the_pitch_not_the_width() {
        // A 1x2 colour shape whose rows are 8 bytes apart (padding between).
        let data = [[1, 2, 3, 0xFF], [9, 9, 9, 9], [4, 5, 6, 0xFF]].concat();
        let image = PointerImage::decode(ShapeKind::Color, 1, 2, 8, &data).expect("decodes");
        assert_eq!(image.blend_at(0, 1), [4, 5, 6, 0xFF]);
    }

    #[test]
    fn a_shape_the_data_cannot_hold_is_refused() {
        assert_eq!(
            PointerImage::decode(ShapeKind::Color, 2, 2, 8, &[0; 15]),
            None
        );
        assert_eq!(
            PointerImage::decode(ShapeKind::Color, 2, 1, 4, &[0; 8]),
            None,
            "pitch below a row"
        );
        assert_eq!(
            PointerImage::decode(ShapeKind::Monochrome, 8, 1, 1, &[0; 2]),
            None,
            "no XOR rows"
        );
        assert_eq!(PointerImage::decode(ShapeKind::Color, 0, 1, 0, &[]), None);
        let huge = MAX_SIDE + 1;
        assert_eq!(
            PointerImage::decode(ShapeKind::Color, huge, 1, huge * 4, &[]),
            None
        );
        assert_eq!(ShapeKind::from_dxgi(3), None);
    }

    #[test]
    fn only_a_pointer_that_shows_redraws_the_picture() {
        let at = |visible, x| PointerPlace { visible, x, y: 0 };
        assert!(!at(false, 0).redraw_for(at(false, 50)), "hidden, moving");
        assert!(at(true, 0).redraw_for(at(true, 1)));
        assert!(!at(true, 3).redraw_for(at(true, 3)));
        assert!(at(true, 0).redraw_for(at(false, 0)), "goes");
        assert!(at(false, 0).redraw_for(at(true, 0)), "comes back");
    }
}
