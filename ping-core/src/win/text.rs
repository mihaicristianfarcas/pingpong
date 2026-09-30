//! Rasterise overlay text (statistics, status): monospaced white text on a
//! translucent panel, drawn with GDI into a bitmap the renderer uploads.

use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor, TextOutW, ANTIALIASED_QUALITY,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS,
    FF_MODERN, FIXED_PITCH, FW_NORMAL, HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT,
};

/// Premultiplied RGBA8, top row first.
pub struct Bitmap {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// The panel's opacity behind the text.
const PANEL_ALPHA: u32 = 158;

pub fn rasterize(text: &str, scale: f64) -> Option<Bitmap> {
    let lines: Vec<Vec<u16>> = text.lines().map(|l| l.encode_utf16().collect()).collect();
    if lines.is_empty() {
        return None;
    }
    let font_px = (13.0 * scale).round() as i32;
    let pad = (10.0 * scale).round() as i32;
    let gap = (3.0 * scale).round() as i32;
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let face: Vec<u16> = "Consolas".encode_utf16().chain(Some(0)).collect();
        let font = CreateFontW(
            -font_px,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            (FIXED_PITCH.0 | FF_MODERN.0) as u32,
            PCWSTR(face.as_ptr()),
        );
        let old_font = SelectObject(dc, HGDIOBJ(font.0));
        let mut sizes = Vec::with_capacity(lines.len());
        for l in &lines {
            let mut size = SIZE::default();
            let _ = GetTextExtentPoint32W(dc, l, &mut size);
            sizes.push(size);
        }
        let line_h = sizes.iter().map(|s| s.cy).max().unwrap_or(font_px) + gap;
        let width = (sizes.iter().map(|s| s.cx).max().unwrap_or(0) + 2 * pad).max(1) as usize;
        let height = (line_h * lines.len() as i32 + 2 * pad).max(1) as usize;

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                // Top-down.
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            SelectObject(dc, old_font);
            let _ = DeleteObject(HGDIOBJ(font.0));
            let _ = DeleteDC(dc);
            return None;
        };
        let old_bitmap = SelectObject(dc, HGDIOBJ(bitmap.0));
        SetTextColor(dc, windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
        SetBkMode(dc, TRANSPARENT);
        for (i, l) in lines.iter().enumerate() {
            let _ = TextOutW(dc, pad, pad + i as i32 * line_h, l);
        }
        let _ = windows::Win32::Graphics::Gdi::GdiFlush();
        // White on black: any channel is the coverage.
        let src = std::slice::from_raw_parts(bits as *const u8, width * height * 4);
        let mut rgba = vec![0u8; width * height * 4];
        let (out_pixels, _) = rgba.as_chunks_mut::<4>();
        let (in_pixels, _) = src.as_chunks::<4>();
        for (o, i) in out_pixels.iter_mut().zip(in_pixels) {
            let cov = i[0].max(i[1]).max(i[2]) as u32;
            let a = cov + (255 - cov) * PANEL_ALPHA / 255;
            *o = [cov as u8, cov as u8, cov as u8, a as u8];
        }
        SelectObject(dc, old_bitmap);
        SelectObject(dc, old_font);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteObject(HGDIOBJ(font.0));
        let _ = DeleteDC(dc);
        Some(Bitmap {
            width,
            height,
            rgba,
        })
    }
}
