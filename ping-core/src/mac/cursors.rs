//! The host application's cursor shapes, drawn with the Mac's own cursors so
//! the client-drawn pointer looks native: an I-beam over text, a hand over a
//! link.

use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::NSCursor;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo};
use pingpong_proto::control::CursorShape;

use super::render::CursorImage;

fn image_of(cursor: &Retained<NSCursor>, scale: f64) -> Option<CursorImage> {
    let image = cursor.image();
    let size = image.size();
    let hot = cursor.hotSpot();
    let (w, h) = (
        (size.width * scale).round() as usize,
        (size.height * scale).round() as usize,
    );
    if w == 0 || h == 0 {
        return None;
    }
    let mut rect = objc2_foundation::NSRect::new(objc2_foundation::NSPoint::new(0.0, 0.0), size);
    let cg = unsafe { image.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;
    let mut rgba = vec![0u8; w * h * 4];
    let space = CGColorSpace::new_device_rgb()?;
    unsafe {
        let ctx = CGBitmapContextCreate(
            rgba.as_mut_ptr() as *mut _,
            w,
            h,
            8,
            w * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )?;
        CGContext::draw_image(
            Some(&ctx),
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: w as f64,
                    height: h as f64,
                },
            },
            Some(&cg),
        );
    }
    Some(CursorImage {
        width: w,
        height: h,
        hot_x: (hot.x * scale) as f32,
        hot_y: (hot.y * scale) as f32,
        rgba,
    })
}

/// Load every shape the protocol names. Main thread only (AppKit).
pub fn load(_mtm: MainThreadMarker, scale: f64) -> Vec<(CursorShape, CursorImage)> {
    #[allow(deprecated)]
    let table: Vec<(CursorShape, Retained<NSCursor>)> = vec![
        (CursorShape::Arrow, NSCursor::arrowCursor()),
        (CursorShape::IBeam, NSCursor::IBeamCursor()),
        (CursorShape::Hand, NSCursor::pointingHandCursor()),
        (CursorShape::Cross, NSCursor::crosshairCursor()),
        (CursorShape::No, NSCursor::operationNotAllowedCursor()),
        (CursorShape::SizeWE, NSCursor::resizeLeftRightCursor()),
        (CursorShape::SizeNS, NSCursor::resizeUpDownCursor()),
        (CursorShape::SizeAll, NSCursor::openHandCursor()),
        (CursorShape::AppStarting, NSCursor::arrowCursor()),
        (CursorShape::Wait, NSCursor::arrowCursor()),
        (CursorShape::Help, NSCursor::arrowCursor()),
        (CursorShape::SizeNWSE, NSCursor::crosshairCursor()),
        (CursorShape::SizeNESW, NSCursor::crosshairCursor()),
    ];
    table
        .into_iter()
        .filter_map(|(shape, c)| image_of(&c, scale).map(|i| (shape, i)))
        .collect()
}
