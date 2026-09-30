//! Draws Ping's icon: a dark rounded square, a ball in flight and its trail.
//!
//!   cargo run -p ping-app --example make-icon -- OUT_DIR
//!
//! writes OUT_DIR/AppIcon.iconset (for macOS's `iconutil`), OUT_DIR/Ping.ico
//! (Windows) and OUT_DIR/icon-256.png (the window icon).

use std::path::Path;

use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LinearGradient, Mask, Paint, Path as SkPath,
    PathBuilder, Pixmap, PixmapPaint, Point, RadialGradient, Shader, SpreadMode, Stroke,
    StrokeDash, Transform,
};

const SIZE: u32 = 1024;

fn rgba(r: f32, g: f32, b: f32, a: f32) -> Color {
    Color::from_rgba(r, g, b, a).unwrap()
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> SkPath {
    // A circle's quarter as a cubic.
    let k = r * 0.552_284_7;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish().unwrap()
}

/// A separable box blur, three passes (close to a Gaussian of `sigma`), on
/// premultiplied pixels.
fn blur(p: &mut Pixmap, sigma: f32) {
    let radius = ((sigma * 3.0_f32.sqrt()).round() as usize).max(1);
    let (w, h) = (p.width() as usize, p.height() as usize);
    let data = p.data_mut();
    let mut tmp = vec![0f32; w * h * 4];
    let mut buf: Vec<f32> = data.iter().map(|&v| v as f32).collect();
    for _ in 0..3 {
        for pass in 0..2 {
            let (len, lines, step, stride) = if pass == 0 {
                (w, h, 4, w * 4)
            } else {
                (h, w, w * 4, 4)
            };
            for line in 0..lines {
                let base = line * stride;
                for c in 0..4 {
                    let mut acc = 0f32;
                    let at = |i: isize| -> f32 {
                        let i = i.clamp(0, len as isize - 1) as usize;
                        buf[base + i * step + c]
                    };
                    for i in -(radius as isize)..=(radius as isize) {
                        acc += at(i);
                    }
                    for i in 0..len {
                        tmp[base + i * step + c] = acc / (2 * radius + 1) as f32;
                        acc +=
                            at(i as isize + radius as isize + 1) - at(i as isize - radius as isize);
                    }
                }
            }
            std::mem::swap(&mut buf, &mut tmp);
        }
    }
    for (d, v) in data.iter_mut().zip(buf) {
        *d = v.round().clamp(0.0, 255.0) as u8;
    }
}

/// `path` filled with `color`, blurred: a shadow or a glow.
fn soft(path: &SkPath, color: Color, sigma: f32, dx: f32, dy: f32) -> Pixmap {
    let mut layer = Pixmap::new(SIZE, SIZE).unwrap();
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    layer.fill_path(
        path,
        &paint,
        FillRule::Winding,
        Transform::from_translate(dx, dy),
        None,
    );
    blur(&mut layer, sigma);
    layer
}

fn draw() -> Pixmap {
    let mut canvas = Pixmap::new(SIZE, SIZE).unwrap();
    // macOS icon grid: an 824 px body, 100 px margins.
    let body = rounded_rect(100.0, 100.0, 824.0, 824.0, 185.0);
    let mut clip = Mask::new(SIZE, SIZE).unwrap();
    clip.fill_path(&body, FillRule::Winding, true, Transform::identity());

    canvas.draw_pixmap(
        0,
        0,
        soft(&body, rgba(0.0, 0.0, 0.0, 0.35), 12.0, 0.0, 10.0).as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );

    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.shader = LinearGradient::new(
        Point::from_xy(512.0, 100.0),
        Point::from_xy(512.0, 924.0),
        vec![
            GradientStop::new(0.0, rgba(0.10, 0.12, 0.22, 1.0)),
            GradientStop::new(1.0, rgba(0.04, 0.05, 0.10, 1.0)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    canvas.fill_path(
        &body,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // The table's centre line.
    let mut line = PathBuilder::new();
    line.move_to(512.0, 150.0);
    line.line_to(512.0, 874.0);
    let line = line.finish().unwrap();
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(rgba(1.0, 1.0, 1.0, 0.10));
    let stroke = Stroke {
        width: 10.0,
        dash: StrokeDash::new(vec![34.0, 26.0], 0.0),
        ..Stroke::default()
    };
    canvas.stroke_path(&line, &paint, &stroke, Transform::identity(), Some(&clip));

    // The trail: an arc from lower left to the ball, fading in.
    let mut trail = PathBuilder::new();
    trail.move_to(210.0, 724.0);
    trail.quad_to(380.0, 324.0, 640.0, 414.0);
    let trail = trail.finish().unwrap();
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.shader = LinearGradient::new(
        Point::from_xy(210.0, 724.0),
        Point::from_xy(640.0, 414.0),
        vec![
            GradientStop::new(0.0, rgba(1.0, 0.48, 0.27, 0.0)),
            GradientStop::new(1.0, rgba(1.0, 0.48, 0.27, 0.85)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    let stroke = Stroke {
        width: 44.0,
        line_cap: LineCap::Round,
        ..Stroke::default()
    };
    canvas.stroke_path(&trail, &paint, &stroke, Transform::identity(), Some(&clip));

    // The ball, with a soft glow and a shine.
    let (cx, cy, r) = (690.0, 404.0, 92.0);
    let ball = PathBuilder::from_circle(cx, cy, r).unwrap();
    let glow = soft(&ball, rgba(1.0, 0.45, 0.2, 0.9), 35.0, 0.0, 0.0);
    canvas.draw_pixmap(
        0,
        0,
        glow.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        Some(&clip),
    );
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.set_color(rgba(1.0, 0.50, 0.27, 1.0));
    canvas.fill_path(
        &ball,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        Some(&clip),
    );
    let shine: Shader = RadialGradient::new(
        Point::from_xy(cx - 34.0, cy - 36.0),
        0.0,
        Point::from_xy(cx - 34.0, cy - 36.0),
        80.0,
        vec![
            GradientStop::new(0.0, rgba(1.0, 1.0, 1.0, 0.9)),
            GradientStop::new(1.0, rgba(1.0, 1.0, 1.0, 0.0)),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .unwrap();
    let paint = Paint {
        shader: shine,
        anti_alias: true,
        ..Paint::default()
    };
    canvas.fill_path(
        &ball,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        Some(&clip),
    );
    canvas
}

/// Half the size, each pixel the mean of four.
fn halve(p: &Pixmap) -> Pixmap {
    let (w, h) = (p.width() / 2, p.height() / 2);
    let mut out = Pixmap::new(w, h).unwrap();
    let src = p.data();
    let sw = p.width() as usize;
    let dst = out.data_mut();
    for y in 0..h as usize {
        for x in 0..w as usize {
            for c in 0..4 {
                let at =
                    |dx: usize, dy: usize| src[((2 * y + dy) * sw + 2 * x + dx) * 4 + c] as u32;
                dst[(y * w as usize + x) * 4 + c] =
                    ((at(0, 0) + at(1, 0) + at(0, 1) + at(1, 1) + 2) / 4) as u8;
            }
        }
    }
    out
}

fn main() {
    let out = std::env::args().nth(1).expect("usage: make-icon OUT_DIR");
    let out = Path::new(&out);
    let iconset = out.join("AppIcon.iconset");
    std::fs::create_dir_all(&iconset).unwrap();
    let mut sizes = vec![(SIZE, draw())];
    while sizes.last().unwrap().0 > 16 {
        let next = halve(&sizes.last().unwrap().1);
        sizes.push((next.width(), next));
    }
    let png = |size: u32| {
        sizes
            .iter()
            .find(|s| s.0 == size)
            .unwrap()
            .1
            .encode_png()
            .unwrap()
    };
    for size in [16, 32, 128, 256, 512] {
        std::fs::write(iconset.join(format!("icon_{size}x{size}.png")), png(size)).unwrap();
        std::fs::write(
            iconset.join(format!("icon_{size}x{size}@2x.png")),
            png(size * 2),
        )
        .unwrap();
    }
    std::fs::write(out.join("icon-256.png"), png(256)).unwrap();

    // An .ico of PNG images: 16, 32, 64 and 256 pixels.
    let images: Vec<(u32, Vec<u8>)> = [16, 32, 64, 256].iter().map(|&s| (s, png(s))).collect();
    let mut ico = vec![0u8, 0, 1, 0];
    ico.extend((images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in &images {
        let dim = if *size >= 256 { 0 } else { *size as u8 };
        ico.extend([dim, dim, 0, 0]);
        ico.extend(1u16.to_le_bytes());
        ico.extend(32u16.to_le_bytes());
        ico.extend((data.len() as u32).to_le_bytes());
        ico.extend(offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        ico.extend(data);
    }
    std::fs::write(out.join("Ping.ico"), ico).unwrap();
    println!("wrote {}", out.display());
}
