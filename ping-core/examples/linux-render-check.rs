//! Decode the known stream with FFmpeg and draw its last frame offscreen with
//! the Linux presenter: the colour conversion and layout, checked without a
//! window or a host. `cargo run -p ping-core --example linux-render-check OUT.png`

#[cfg(target_os = "linux")]
fn main() {
    use std::sync::Arc;

    use ping_core::linux::render::{self, Gpu, Layout, RenderShared};
    use pingpong_decode::ffmpeg::FfmpegDecoder;

    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "render-check.png".into());
    let stream = std::fs::read("pingpong-decode/tests/fixtures/testsrc.h264")
        .expect("the fixture (run from the repository)");
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .expect("adapter");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .expect("device");
    let gpu = Arc::new(Gpu::new(device, queue, wgpu::TextureFormat::Rgba8Unorm));
    let shared = RenderShared::new(
        gpu,
        Arc::new(ping_core::stats::StatsCollector::default()),
        true,
    );
    shared.set_layout(Layout {
        width: 1280,
        height: 800,
        scale: 1.0,
    });
    shared.set_notice(Some("Offscreen check".into()));
    let mut decoder = FfmpegDecoder::new(pingpong_decode::Codec::H264, true).expect("decoder");
    // Feed it frame by frame: split at slice NALs as the fixture test does.
    let mut start = 0;
    let mut i = 0;
    let mut n = 0u32;
    while i + 4 < stream.len() {
        if stream[i..i + 4] == [0, 0, 0, 1] || stream[i..i + 3] == [0, 0, 1] {
            let payload = if stream[i + 2] == 1 { i + 3 } else { i + 4 };
            if i > start && matches!(stream[payload] & 0x1F, 1 | 5 | 7 | 8) && {
                // A new access unit begins at an SPS or a slice after a slice.
                let prev = &stream[start..i];
                prev.windows(4)
                    .any(|w| w[..3] == [0, 0, 1] && matches!(w[3] & 0x1F, 1 | 5))
            } {
                let _ = decoder.decode(&stream[start..i], n, |p| shared.push_picture(&p));
                n += 1;
                start = i;
            }
            i = payload;
        } else {
            i += 1;
        }
    }
    let _ = decoder.decode(&stream[start..], n, |p| shared.push_picture(&p));
    render::offscreen_png(shared, 1280, 800, std::path::Path::new(&out)).expect("render");
    println!("decoded {} frames; wrote {out}", n + 1);
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("Linux only");
}
