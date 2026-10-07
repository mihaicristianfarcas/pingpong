//! Synthetic frames through the Mac stream window and renderer, with no host
//! and no decoder: how many reach the glass at a given rate, and how late, in
//! whatever state the screen is in (recorded, a window over the stream, in a
//! window). A real stream mixes the presentation path with the network and
//! the decoder; this measures it alone.
//!
//! ```sh
//! cargo run --release -p ping-core --example mac-present -- --fps 120 --secs 10
//! # the same while the screen is recorded, from another terminal:
//! screencapture -x -V 12 /tmp/recording.mov
//! ```
//!
//! Options: `--fps N` (60), `--secs N` (10), `--size WxH` (the main display's
//! pixels), `--windowed`, `--paced` (frame pacing), `--no-vsync`,
//! `--jitter-ms N` (each frame handed over up to N ms late, as a network
//! delivers them), `--overlay` (the statistics overlay), `--cover AT,FOR` (a
//! small window over the stream from AT seconds for FOR, as a recorder's
//! controls or a dialog would be). Prints a line a second and a summary of
//! the seconds after the first two (the window going full screen).

#[cfg(target_os = "macos")]
fn main() {
    mac::main();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac-present runs on macOS");
}

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use dispatch2::{DispatchQueue, DispatchTime};
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor,
        NSStatusWindowLevel, NSWindow, NSWindowCollectionBehavior, NSWindowStyleMask,
    };
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
    use objc2_core_video::{
        kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey,
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferCreate,
        CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferGetHeightOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize};
    use objc2_quartz_core::CAMetalLayer;
    use ping_core::mac::render::{self, RenderShared};
    use ping_core::mac::window::{ScreenInfo, Window};
    use ping_core::stats::{Stats, StatsCollector};
    use pingpong_decode::DecodedFrame;
    use pingpong_proto::clock;

    #[derive(Clone)]
    struct Options {
        fps: f64,
        secs: u64,
        size: Option<(usize, usize)>,
        windowed: bool,
        paced: bool,
        vsync: bool,
        jitter_ms: u64,
        overlay: bool,
        cover: Option<(f64, f64)>,
    }

    fn options() -> Options {
        let mut o = Options {
            fps: 60.0,
            secs: 10,
            size: None,
            windowed: false,
            paced: false,
            vsync: true,
            jitter_ms: 0,
            overlay: false,
            cover: None,
        };
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            let mut value = || args.next().expect("a value after the option");
            match a.as_str() {
                "--fps" => o.fps = value().parse().expect("--fps N"),
                "--secs" => o.secs = value().parse().expect("--secs N"),
                "--size" => {
                    let v = value();
                    let (w, h) = v.split_once('x').expect("--size WxH");
                    o.size = Some((w.parse().expect("width"), h.parse().expect("height")));
                }
                "--windowed" => o.windowed = true,
                "--paced" => o.paced = true,
                "--no-vsync" => o.vsync = false,
                "--jitter-ms" => o.jitter_ms = value().parse().expect("--jitter-ms N"),
                "--overlay" => o.overlay = true,
                "--cover" => {
                    let v = value();
                    let (at, secs) = v.split_once(',').expect("--cover AT,FOR");
                    o.cover = Some((at.parse().expect("AT"), secs.parse().expect("FOR")));
                }
                _ => panic!("unknown option {a}"),
            }
        }
        o
    }

    /// A video-range NV12 picture (VideoToolbox's `420v`), IOSurface-backed
    /// as decoded ones are, with a bar at `bar` (0..1) across.
    fn picture(width: usize, height: usize, bar: f64) -> CFRetained<CVPixelBuffer> {
        let empty = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        let yes: &CFType = CFBoolean::new(true);
        // SAFETY: CoreVideo's constant keys.
        let keys: [&CFString; 2] = unsafe {
            [
                kCVPixelBufferIOSurfacePropertiesKey,
                kCVPixelBufferMetalCompatibilityKey,
            ]
        };
        let values: [&CFType; 2] = [&empty, yes];
        let attributes = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
        let mut raw: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: a valid attributes dictionary and out pointer.
        let status = unsafe {
            CVPixelBufferCreate(
                None,
                width,
                height,
                kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                Some(attributes.as_opaque()),
                NonNull::from(&mut raw),
            )
        };
        assert_eq!(status, 0, "CVPixelBufferCreate");
        // SAFETY: created with +1.
        let buffer = unsafe { CFRetained::from_raw(NonNull::new(raw).expect("a buffer")) };
        // SAFETY: locked for writing; each plane written within its rows.
        unsafe {
            CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
            for (plane, (background, ink)) in [(16u8, 235u8), (128, 128)].into_iter().enumerate() {
                let base = CVPixelBufferGetBaseAddressOfPlane(&buffer, plane) as *mut u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(&buffer, plane);
                let rows = CVPixelBufferGetHeightOfPlane(&buffer, plane);
                let from = (bar * stride as f64) as usize;
                let to = (from + stride / 20).min(stride);
                for y in 0..rows {
                    let row = std::slice::from_raw_parts_mut(base.add(y * stride), stride);
                    row.fill(background);
                    row[from..to].fill(ink);
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
        }
        buffer
    }

    struct SendLayer(Retained<CAMetalLayer>);
    // SAFETY: CAMetalLayer may be drawn into from a rendering thread; only the
    // render thread does.
    unsafe impl Send for SendLayer {}

    /// Hand the renderer a frame every `1 / fps`, some late by up to
    /// `jitter_ms`, until `stop`.
    fn frames(render: Arc<RenderShared>, o: Options, size: (usize, usize), stop: Arc<AtomicBool>) {
        const PICTURES: usize = 8;
        let pictures: Vec<_> = (0..PICTURES)
            .map(|i| picture(size.0, size.1, i as f64 / PICTURES as f64))
            .collect();
        let interval = Duration::from_secs_f64(1.0 / o.fps);
        let jitter_us = o.jitter_ms * 1000;
        let start = Instant::now();
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut n = 0u32;
        while !stop.load(Ordering::Acquire) {
            let mut due = start + interval * n;
            if jitter_us > 0 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                due += Duration::from_micros(seed % jitter_us);
            }
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let picture = pictures[n as usize % PICTURES].clone();
            render.push_frame(DecodedFrame {
                pixel_buffer: CFRetained::into_raw(picture).as_ptr() as *mut c_void,
                capture_ts_us: n,
                decoded_at_us: clock::now_us(),
                width: size.0 as u32,
                height: size.1 as u32,
            });
            n += 1;
        }
    }

    /// Print each one-second window as it completes; after `secs`, the
    /// summary.
    fn report(stats: Arc<StatsCollector>, secs: u64) {
        const WARMUP: u32 = 2;
        let start = Instant::now();
        let mut last = String::new();
        let mut seconds = 0u32;
        let (mut handed, mut shown, mut glass_ms, mut worst_ms, mut counted) =
            (0u32, 0u32, 0f32, 0f32, 0u32);
        while start.elapsed() < Duration::from_secs(secs) {
            std::thread::sleep(Duration::from_millis(20));
            let s: Stats = stats.snapshot();
            // A window not seen yet (the snapshot is the last complete one).
            let key = format!("{s:?}");
            if key == last {
                continue;
            }
            last = key;
            seconds += 1;
            println!(
                "{seconds:>3} s  handed {:>3}  shown {:>3}  decoded to glass {:>5.1} ms avg / {:>5.1} max",
                s.decoded_fps, s.presented_fps, s.render.avg_ms, s.render.max_ms
            );
            if seconds > WARMUP {
                handed += s.decoded_fps;
                shown += s.presented_fps;
                glass_ms += s.render.avg_ms;
                worst_ms = worst_ms.max(s.render.max_ms);
                counted += 1;
            }
        }
        if counted > 0 {
            println!(
                "summary  {counted} s  shown {shown} of {handed} ({:.1}%)  decoded to glass {:.1} ms avg / {:.1} max",
                100.0 * shown as f32 / handed.max(1) as f32,
                glass_ms / counted as f32,
                worst_ms
            );
        }
    }

    thread_local! {
        /// The window over the stream (`--cover`), on the main thread.
        static COVER: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    }

    /// On the main queue `secs` from now.
    fn later(secs: f64, f: impl FnOnce(MainThreadMarker) + Send + 'static) {
        let when = DispatchTime::try_from(Duration::from_secs_f64(secs)).expect("a time");
        let _ = DispatchQueue::main().after(when, move || {
            f(MainThreadMarker::new().expect("the main queue"));
        });
    }

    /// A small window over the full-screen stream from `at` seconds for
    /// `secs`: borderless, above it in its Space, taking no clicks.
    fn cover(at: f64, secs: f64) {
        later(at, |mtm| {
            let rect = NSRect::new(NSPoint::new(160.0, 160.0), NSSize::new(320.0, 64.0));
            // SAFETY: a plain window; the main thread.
            let w = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    NSWindow::alloc(mtm),
                    rect,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            // SAFETY: kept alive in COVER until closed.
            unsafe { w.setReleasedWhenClosed(false) };
            w.setBackgroundColor(Some(&NSColor::systemRedColor()));
            w.setLevel(NSStatusWindowLevel);
            w.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
            w.setIgnoresMouseEvents(true);
            w.orderFrontRegardless();
            println!("      a window over the stream");
            COVER.with(|c| *c.borrow_mut() = Some(w));
        });
        later(at + secs, |_| {
            if let Some(w) = COVER.with(|c| c.borrow_mut().take()) {
                w.close();
                println!("      the window over the stream gone");
            }
        });
    }

    pub fn main() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "warn".into()),
            )
            .init();
        let o = options();
        let mtm = MainThreadMarker::new().expect("the main thread");
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

        let info = ScreenInfo::main(mtm).expect("a display");
        let size = o.size.unwrap_or((
            (info.frame.size.width * info.scale) as usize,
            (info.frame.size.height * info.scale) as usize,
        ));
        println!(
            "{}x{} at {} fps, {}{}{}, jitter up to {} ms",
            size.0,
            size.1,
            o.fps,
            if o.windowed {
                "windowed"
            } else {
                "full screen"
            },
            if !o.vsync {
                ", V-Sync off"
            } else if o.paced {
                ", frame pacing"
            } else {
                ", V-Sync"
            },
            if o.overlay { ", overlay" } else { "" },
            o.jitter_ms
        );
        let stats = Arc::new(StatsCollector::default());
        let shared = RenderShared::new(stats.clone(), o.vsync, o.paced);
        shared.set_overlay(o.overlay);
        let window = Window::open(
            mtm,
            shared.clone(),
            !o.windowed,
            (size.0 as u32, size.1 as u32),
            "Ping — presentation test",
        );
        let scale = window.window.backingScaleFactor();
        let layer = SendLayer(window.layer.clone());
        let renderer = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("ping-render".into())
                .spawn(move || {
                    let layer = layer;
                    ping_core::priority::latency_critical();
                    render::run(layer.0, shared, Vec::new(), scale)
                })
                .expect("the render thread")
        };
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (shared, stop, o) = (shared.clone(), stop.clone(), o.clone());
            std::thread::Builder::new()
                .name("frames".into())
                .spawn(move || {
                    ping_core::priority::latency_critical();
                    frames(shared, o, size, stop)
                })
                .expect("the frame thread");
        }
        if let Some((at, secs)) = o.cover {
            cover(at, secs);
        }
        let secs = o.secs;
        std::thread::Builder::new()
            .name("report".into())
            .spawn(move || {
                report(stats, secs);
                // No more frames, and the last ones' command buffers done,
                // before the renderer goes.
                stop.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(200));
                shared.stop();
                let _ = renderer.join();
                std::process::exit(0);
            })
            .expect("the report thread");
        app.run();
        drop(window);
    }
}
