//! Pictures as an agent sees them: the decoder's planes kept as they came
//! (YUV), turned into RGB and a PNG only when someone looks. Also what tells
//! a screen that changed from one that is only blinking a caret.

use std::io::Cursor;

/// A decoded picture, 8-bit planar YUV. Chroma is at half size in both
/// directions (4:2:0) or full size (4:4:4).
#[derive(Clone)]
pub struct Yuv {
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    /// Chroma planes' width and height.
    pub chroma_width: u32,
    pub chroma_height: u32,
    /// Full range (0-255) rather than video range (16-235).
    pub full_range: bool,
}

/// One plane as a decoder hands it out: rows `stride` bytes apart.
pub struct PlaneRef<'a> {
    pub data: &'a [u8],
    pub stride: usize,
}

impl Yuv {
    /// From planar 4:2:0 or 4:4:4 (the chroma planes' size says which).
    pub fn from_planar(
        width: u32,
        height: u32,
        y: PlaneRef,
        u: PlaneRef,
        v: PlaneRef,
        chroma: (u32, u32),
        full_range: bool,
    ) -> Yuv {
        Yuv {
            width,
            height,
            y: copy_plane(&y, width as usize, height as usize),
            u: copy_plane(&u, chroma.0 as usize, chroma.1 as usize),
            v: copy_plane(&v, chroma.0 as usize, chroma.1 as usize),
            chroma_width: chroma.0,
            chroma_height: chroma.1,
            full_range,
        }
    }

    /// From NV12: luma, then interleaved U and V at half size.
    pub fn from_nv12(width: u32, height: u32, y: PlaneRef, uv: PlaneRef, full_range: bool) -> Yuv {
        Self::from_semi_planar(
            width,
            height,
            y,
            uv,
            (width.div_ceil(2), height.div_ceil(2)),
            full_range,
        )
    }

    /// Luma, then interleaved U and V at `chroma` size (NV12 is half size,
    /// the 4:2:2 and 4:4:4 bi-planar formats wider or whole).
    pub fn from_semi_planar(
        width: u32,
        height: u32,
        y: PlaneRef,
        uv: PlaneRef,
        chroma: (u32, u32),
        full_range: bool,
    ) -> Yuv {
        let (cw, ch) = (chroma.0 as usize, chroma.1 as usize);
        let mut u = Vec::with_capacity(cw * ch);
        let mut v = Vec::with_capacity(cw * ch);
        for row in 0..ch {
            let start = row * uv.stride;
            let Some(line) = uv.data.get(start..start + cw * 2) else {
                break;
            };
            for [cb, cr] in line.as_chunks::<2>().0 {
                u.push(*cb);
                v.push(*cr);
            }
        }
        u.resize(cw * ch, 128);
        v.resize(cw * ch, 128);
        Yuv {
            width,
            height,
            y: copy_plane(&y, width as usize, height as usize),
            u,
            v,
            chroma_width: cw as u32,
            chroma_height: ch as u32,
            full_range,
        }
    }

    /// BT.709 to RGB, 3 bytes a pixel. Chroma is taken from the nearest
    /// sample: text is sharp in luma, which is kept whole.
    pub fn to_rgb(&self) -> Rgb {
        let (w, h) = (self.width as usize, self.height as usize);
        let (cw, ch) = (self.chroma_width as usize, self.chroma_height as usize);
        let (sx, sy) = (w.div_ceil(cw.max(1)), h.div_ceil(ch.max(1)));
        // Video range: luma 16-235, chroma 16-240 (224 steps).
        let (y_off, y_scale, c_scale) = if self.full_range {
            (0.0, 1.0, 1.0)
        } else {
            (16.0, 255.0 / 219.0, 255.0 / 224.0)
        };
        let mut out = vec![0u8; w * h * 3];
        for row in 0..h {
            let crow = (row / sy).min(ch.saturating_sub(1));
            for col in 0..w {
                let ccol = (col / sx).min(cw.saturating_sub(1));
                let yy = (self.y[row * w + col] as f32 - y_off) * y_scale;
                let cb = (self.u[crow * cw + ccol] as f32 - 128.0) * c_scale;
                let cr = (self.v[crow * cw + ccol] as f32 - 128.0) * c_scale;
                let r = yy + 1.5748 * cr;
                let g = yy - 0.1873 * cb - 0.4681 * cr;
                let b = yy + 1.8556 * cb;
                let i = (row * w + col) * 3;
                out[i] = r.round().clamp(0.0, 255.0) as u8;
                out[i + 1] = g.round().clamp(0.0, 255.0) as u8;
                out[i + 2] = b.round().clamp(0.0, 255.0) as u8;
            }
        }
        Rgb {
            width: self.width,
            height: self.height,
            data: out,
        }
    }

    /// Nearly all black (a display coming up, a mode being set).
    pub fn is_black(&self) -> bool {
        let limit = if self.full_range { 8 } else { 24 };
        let (mut dark, mut n) = (0usize, 0usize);
        for v in self.y.iter().step_by(97) {
            n += 1;
            if *v <= limit {
                dark += 1;
            }
        }
        n > 0 && dark * 100 >= n * 98
    }

    /// Mean luma of each 16x16 block, row by row: what `Change` compares.
    pub fn signature(&self) -> Signature {
        let (w, h) = (self.width as usize, self.height as usize);
        let (bw, bh) = (w.div_ceil(BLOCK), h.div_ceil(BLOCK));
        let mut sums = vec![0u32; bw * bh];
        let mut counts = vec![0u32; bw * bh];
        // Every other row and column: plenty to see a change, a quarter the work.
        for row in (0..h).step_by(2) {
            let b_row = (row / BLOCK) * bw;
            let line = &self.y[row * w..row * w + w];
            for col in (0..w).step_by(2) {
                let b = b_row + col / BLOCK;
                sums[b] += line[col] as u32;
                counts[b] += 1;
            }
        }
        Signature {
            columns: bw,
            means: sums
                .iter()
                .zip(&counts)
                .map(|(s, c)| (*s / (*c).max(1)) as u8)
                .collect(),
        }
    }
}

fn copy_plane(p: &PlaneRef, width: usize, height: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height);
    for row in 0..height {
        let start = row * p.stride;
        match p.data.get(start..start + width) {
            Some(line) => out.extend_from_slice(line),
            None => break,
        }
    }
    out.resize(width * height, 0);
    out
}

const BLOCK: usize = 16;
/// A block whose mean luma moved this much changed (below it: encoder noise
/// as a still picture sharpens).
const BLOCK_THRESHOLD: u8 = 6;
/// Changes this small are a blinking caret or cursor, not the screen
/// changing.
pub const MINOR_BLOCKS: usize = 3;

/// A picture's coarse fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    columns: usize,
    means: Vec<u8>,
}

impl Signature {
    /// Blocks that differ from `other` (a different size: all of them).
    pub fn changed_blocks(&self, other: &Signature) -> usize {
        if self.columns != other.columns || self.means.len() != other.means.len() {
            return self.means.len().max(1);
        }
        self.means
            .iter()
            .zip(&other.means)
            .filter(|(a, b)| a.abs_diff(**b) > BLOCK_THRESHOLD)
            .count()
    }
}

/// An RGB picture, 3 bytes a pixel, rows packed.
#[derive(Clone)]
pub struct Rgb {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl Rgb {
    pub fn png(&self) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(Cursor::new(&mut out), self.width, self.height);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut w = enc.write_header().expect("PNG header into memory");
            w.write_image_data(&self.data)
                .expect("PNG data into memory");
        }
        out
    }

    /// The region `[x0, y0, x1, y1]` (clamped to the picture), scaled so its
    /// longer side is about `target` pixels (never down, at most 4x up).
    pub fn zoom(&self, region: [u32; 4], target: u32) -> Option<Rgb> {
        let [x0, y0, x1, y1] = region;
        let (x0, x1) = (x0.min(x1).min(self.width), x0.max(x1).min(self.width));
        let (y0, y1) = (y0.min(y1).min(self.height), y0.max(y1).min(self.height));
        let (w, h) = (x1 - x0, y1 - y0);
        if w == 0 || h == 0 {
            return None;
        }
        let scale = (target as f32 / w.max(h) as f32).clamp(1.0, 4.0);
        let (ow, oh) = (
            ((w as f32) * scale).round() as u32,
            ((h as f32) * scale).round() as u32,
        );
        let mut out = vec![0u8; (ow * oh * 3) as usize];
        // Bilinear: an enlarged crop reads better than blocky pixels.
        for oy in 0..oh {
            let fy = ((oy as f32 + 0.5) / scale - 0.5).max(0.0);
            let (ya, t) = (fy.floor() as u32, fy.fract());
            let yb = (ya + 1).min(h - 1);
            for ox in 0..ow {
                let fx = ((ox as f32 + 0.5) / scale - 0.5).max(0.0);
                let (xa, s) = (fx.floor() as u32, fx.fract());
                let xb = (xa + 1).min(w - 1);
                let px = |x: u32, y: u32, c: usize| {
                    self.data[(((y0 + y) * self.width + x0 + x) * 3) as usize + c] as f32
                };
                for c in 0..3 {
                    let top = px(xa.min(w - 1), ya.min(h - 1), c) * (1.0 - s)
                        + px(xb, ya.min(h - 1), c) * s;
                    let bottom = px(xa.min(w - 1), yb, c) * (1.0 - s) + px(xb, yb, c) * s;
                    out[((oy * ow + ox) * 3) as usize + c] =
                        (top * (1.0 - t) + bottom * t).round() as u8;
                }
            }
        }
        Some(Rgb {
            width: ow,
            height: oh,
            data: out,
        })
    }

    /// Scaled down so neither side exceeds `max` (thumbnails for a UI).
    pub fn fit(&self, max: u32) -> Rgb {
        let scale = (max as f32 / self.width.max(self.height) as f32).min(1.0);
        if scale >= 1.0 {
            return self.clone();
        }
        let (ow, oh) = (
            ((self.width as f32) * scale).round().max(1.0) as u32,
            ((self.height as f32) * scale).round().max(1.0) as u32,
        );
        let mut out = Vec::with_capacity((ow * oh * 3) as usize);
        for oy in 0..oh {
            // Box filter over the source pixels each output pixel covers.
            let (sy0, sy1) = (
                (oy as f32 / scale) as u32,
                (((oy + 1) as f32 / scale) as u32).clamp(1, self.height),
            );
            for ox in 0..ow {
                let (sx0, sx1) = (
                    (ox as f32 / scale) as u32,
                    (((ox + 1) as f32 / scale) as u32).clamp(1, self.width),
                );
                let mut acc = [0u32; 3];
                let mut n = 0;
                for sy in sy0..sy1.max(sy0 + 1) {
                    for sx in sx0..sx1.max(sx0 + 1) {
                        let i = ((sy.min(self.height - 1) * self.width + sx.min(self.width - 1))
                            * 3) as usize;
                        for (c, a) in acc.iter_mut().enumerate() {
                            *a += self.data[i + c] as u32;
                        }
                        n += 1;
                    }
                }
                for a in acc {
                    out.push((a / n.max(1)) as u8);
                }
            }
        }
        Rgb {
            width: ow,
            height: oh,
            data: out,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, y: u8, u: u8, v: u8, full: bool) -> Yuv {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        Yuv {
            width: w,
            height: h,
            y: vec![y; (w * h) as usize],
            u: vec![u; (cw * ch) as usize],
            v: vec![v; (cw * ch) as usize],
            chroma_width: cw,
            chroma_height: ch,
            full_range: full,
        }
    }

    #[test]
    fn video_range_black_white_and_bt709_red() {
        assert_eq!(
            &solid(4, 2, 16, 128, 128, false).to_rgb().data[..3],
            &[0, 0, 0]
        );
        assert_eq!(
            &solid(4, 2, 235, 128, 128, false).to_rgb().data[..3],
            &[255, 255, 255]
        );
        // BT.709 red (255, 0, 0) in video range is Y 63, Cb 102, Cr 240.
        let red = solid(4, 2, 63, 102, 240, false).to_rgb();
        assert!(
            red.data[0] >= 253 && red.data[1] <= 2 && red.data[2] <= 2,
            "{:?}",
            &red.data[..3]
        );
        // Full range white.
        assert_eq!(
            &solid(4, 2, 255, 128, 128, true).to_rgb().data[..3],
            &[255, 255, 255]
        );
    }

    #[test]
    fn a_black_screen_is_black() {
        assert!(solid(64, 64, 16, 128, 128, false).is_black());
        assert!(!solid(64, 64, 120, 128, 128, false).is_black());
        let mut mostly = solid(64, 64, 16, 128, 128, false);
        for v in mostly.y.iter_mut().take(1000) {
            *v = 200;
        }
        assert!(!mostly.is_black());
    }

    #[test]
    fn nv12_is_deinterleaved() {
        let (w, h) = (4u32, 2u32);
        let y = vec![100u8; 8];
        let uv = vec![10u8, 20, 30, 40]; // one chroma row: (10,20) (30,40)
        let f = Yuv::from_nv12(
            w,
            h,
            PlaneRef {
                data: &y,
                stride: 4,
            },
            PlaneRef {
                data: &uv,
                stride: 4,
            },
            false,
        );
        assert_eq!(f.u, vec![10, 30]);
        assert_eq!(f.v, vec![20, 40]);
    }

    #[test]
    fn strided_planes_are_packed() {
        // A stride wider than the picture (decoders pad rows).
        let y: Vec<u8> = (0..16).collect();
        let f = Yuv::from_planar(
            2,
            2,
            PlaneRef {
                data: &y,
                stride: 8,
            },
            PlaneRef {
                data: &[1, 0, 0, 0],
                stride: 4,
            },
            PlaneRef {
                data: &[2, 0, 0, 0],
                stride: 4,
            },
            (1, 1),
            false,
        );
        assert_eq!(f.y, vec![0, 1, 8, 9]);
    }

    #[test]
    fn a_blinking_caret_is_a_minor_change_and_a_new_window_is_not() {
        let a = solid(640, 480, 200, 128, 128, false);
        let mut caret = a.clone();
        for row in 100..116 {
            caret.y[row * 640 + 50] = 16; // a one-pixel-wide caret
            caret.y[row * 640 + 51] = 16;
        }
        let changed = a.signature().changed_blocks(&caret.signature());
        assert!(changed <= MINOR_BLOCKS, "{changed}");
        let mut window = a.clone();
        for row in 100..300 {
            for col in 100..400 {
                window.y[row * 640 + col] = 60;
            }
        }
        assert!(a.signature().changed_blocks(&window.signature()) > 100);
        assert_eq!(a.signature().changed_blocks(&a.signature()), 0);
    }

    #[test]
    fn zoom_crops_and_enlarges() {
        let mut img = Rgb {
            width: 100,
            height: 50,
            data: vec![0; 100 * 50 * 3],
        };
        // A white square at (10..20, 10..20).
        for y in 10..20 {
            for x in 10..20 {
                let i = (y * 100 + x) * 3;
                img.data[i..i + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let z = img.zoom([10, 10, 20, 20], 40).unwrap();
        assert_eq!((z.width, z.height), (40, 40));
        assert!(
            z.data.iter().all(|&b| b > 200),
            "all white inside the square"
        );
        assert!(img.zoom([5, 5, 5, 9], 40).is_none());
        // Clamped to the picture.
        let edge = img.zoom([90, 40, 400, 400], 20).unwrap();
        assert_eq!((edge.width, edge.height), (20, 20));
    }

    #[test]
    fn png_round_trips_its_size() {
        let img = Rgb {
            width: 3,
            height: 2,
            data: vec![7; 18],
        };
        let png = img.png();
        assert_eq!(&png[1..4], b"PNG");
        let decoder = png::Decoder::new(Cursor::new(png));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (3, 2));
        let small = Rgb {
            width: 1000,
            height: 500,
            data: vec![9; 1000 * 500 * 3],
        }
        .fit(100);
        assert_eq!((small.width, small.height), (100, 50));
        assert!(small.data.iter().all(|&b| b == 9));
    }
}
