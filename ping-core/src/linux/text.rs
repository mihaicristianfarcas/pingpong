//! Rasterise overlay text (statistics, status): monospaced white text on a
//! translucent panel, drawn with ab_glyph (the Hack font egui ships) into a
//! bitmap the renderer uploads.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};

/// Premultiplied RGBA8, top row first.
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// The panel's opacity behind the text.
const PANEL_ALPHA: f32 = 0.62;

pub fn rasterize(text: &str, scale: f64) -> Option<Bitmap> {
    let font = FontRef::try_from_slice(epaint_default_fonts::HACK_REGULAR).ok()?;
    let px = PxScale::from((13.0 * scale) as f32 * 1.25);
    let scaled = font.as_scaled(px);
    let pad = (10.0 * scale).round() as usize;
    let gap = (3.0 * scale).round() as usize;
    let line_h = (scaled.ascent() - scaled.descent()).ceil() as usize + gap;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let advance = |s: &str| {
        s.chars()
            .map(|c| scaled.h_advance(font.glyph_id(c)))
            .sum::<f32>()
    };
    let text_w = lines
        .iter()
        .map(|l| advance(l))
        .fold(0.0f32, f32::max)
        .ceil() as usize;
    let width = text_w + 2 * pad;
    let height = line_h * lines.len() + 2 * pad;
    let mut coverage = vec![0f32; width * height];
    for (i, line) in lines.iter().enumerate() {
        let mut x = pad as f32;
        let baseline = (pad + i * line_h) as f32 + scaled.ascent();
        for c in line.chars() {
            let id = font.glyph_id(c);
            let glyph = id.with_scale_and_position(px, ab_glyph::point(x, baseline));
            x += scaled.h_advance(id);
            if let Some(outline) = font.outline_glyph(glyph) {
                let b = outline.px_bounds();
                outline.draw(|gx, gy, v| {
                    let (px_, py_) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                    if px_ >= 0 && py_ >= 0 && (px_ as usize) < width && (py_ as usize) < height {
                        let c = &mut coverage[py_ as usize * width + px_ as usize];
                        *c = (*c + v).min(1.0);
                    }
                });
            }
        }
    }
    let mut rgba = vec![0u8; width * height * 4];
    for (o, &c) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(&coverage) {
        let a = c + (1.0 - c) * PANEL_ALPHA;
        let v = (c * 255.0).round() as u8;
        o.copy_from_slice(&[v, v, v, (a * 255.0).round() as u8]);
    }
    Some(Bitmap {
        width,
        height,
        rgba,
    })
}
