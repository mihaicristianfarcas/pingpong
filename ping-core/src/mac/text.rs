//! Rasterise the stats overlay: monospaced white text on a translucent panel,
//! drawn with CoreText into an RGBA bitmap the renderer uploads as a texture.

use objc2_core_foundation::{
    CFAttributedString, CFDictionary, CFRetained, CFString, CFType, CGFloat, CGPoint, CGRect,
    CGSize,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColor, CGColorSpace, CGContext, CGImageAlphaInfo,
};
use objc2_core_text::{kCTFontAttributeName, kCTForegroundColorAttributeName, CTFont, CTLine};

/// Premultiplied RGBA8, top row first.
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub fn rasterize(text: &str, scale: f64) -> Option<Bitmap> {
    let font_size: CGFloat = 13.0 * scale;
    let pad = 10.0 * scale;
    let line_gap = 3.0 * scale;
    unsafe {
        let font = CTFont::with_name(&CFString::from_str("Menlo"), font_size, std::ptr::null());
        let white = CGColor::new_srgb(1.0, 1.0, 1.0, 1.0);
        let keys: [&CFType; 2] = [
            kCTFontAttributeName.as_ref(),
            kCTForegroundColorAttributeName.as_ref(),
        ];
        let values: [&CFType; 2] = [font.as_ref(), white.as_ref()];
        let attrs = CFDictionary::<CFType, CFType>::from_slices(&keys, &values);

        let mut lines: Vec<(CFRetained<CTLine>, f64, f64, f64)> = Vec::new();
        for text in text.lines() {
            let s = CFString::from_str(text);
            let attributed = CFAttributedString::new(None, Some(&s), Some(attrs.as_opaque()))?;
            let line = CTLine::with_attributed_string(&attributed);
            let (mut ascent, mut descent, mut leading) = (0.0, 0.0, 0.0);
            let width = line.typographic_bounds(&mut ascent, &mut descent, &mut leading);
            lines.push((line, width, ascent, descent));
        }
        if lines.is_empty() {
            return None;
        }
        let text_w = lines.iter().map(|l| l.1).fold(0.0f64, f64::max);
        let line_h = lines.iter().map(|l| l.2 + l.3).fold(0.0f64, f64::max) + line_gap;
        let width = (text_w + 2.0 * pad).ceil() as usize;
        let height = (line_h * lines.len() as f64 + 2.0 * pad).ceil() as usize;

        let mut rgba = vec![0u8; width * height * 4];
        let space = CGColorSpace::new_device_rgb()?;
        let ctx: CFRetained<CGContext> = CGBitmapContextCreate(
            rgba.as_mut_ptr() as *mut _,
            width,
            height,
            8,
            width * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )?;
        // Translucent black panel, rounded look is not worth the code.
        CGContext::set_rgb_fill_color(Some(&ctx), 0.0, 0.0, 0.0, 0.62);
        CGContext::fill_rect(
            Some(&ctx),
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: width as f64,
                    height: height as f64,
                },
            },
        );
        // CoreGraphics' origin is bottom-left; lay lines out top-down.
        let mut y = height as f64 - pad;
        for (line, _, ascent, descent) in &lines {
            y -= ascent;
            CGContext::set_text_position(Some(&ctx), pad, y);
            line.draw(&ctx);
            y -= descent + line_gap;
        }
        drop(ctx);
        Some(Bitmap {
            width,
            height,
            rgba,
        })
    }
}
