//! The macOS stream: window, input, Metal presentation, VideoToolbox.

pub mod cursors;
pub mod gamepad;
mod glass;
mod hid;
pub mod render;
pub mod text;
pub mod window;

use std::sync::Arc;
use std::thread::JoinHandle;

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::MainThreadMarker;
use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
use objc2_quartz_core::CAMetalLayer;
use parking_lot::Mutex;
use pingpong_decode::videotoolbox::{PictureFormat, VtDecoder};
use pingpong_decode::VideoDecoder;

use crate::pointer::PointerState;
pub use crate::session::{EndCallback, SessionOptions};
use crate::stats::StatsCollector;
use crate::stream::{Codec, Event, FrameTiming, Source, Stream, VideoOut};
use render::RenderShared;
use window::{Handler, Window};

pub use window::{clipboard_text, toggle_fullscreen, ScreenInfo};

/// Whether the main display shows HDR: headroom above SDR white (EDR), as an
/// XDR panel or an HDR monitor has. Main thread.
pub fn display_has_hdr() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    objc2_app_kit::NSScreen::mainScreen(mtm)
        .is_some_and(|s| s.maximumPotentialExtendedDynamicRangeColorComponentValue() > 1.0)
}

/// The main display's refresh rate to the millihertz (59.94 Hz external
/// panels are common), when macOS knows it: some built-in panels say 0.
/// Any thread.
pub fn display_refresh_mhz() -> Option<u32> {
    use objc2_core_graphics::{CGDisplayCopyDisplayMode, CGDisplayMode, CGMainDisplayID};
    let mode = CGDisplayCopyDisplayMode(CGMainDisplayID())?;
    let hz = CGDisplayMode::refresh_rate(Some(&mode));
    (hz > 1.0).then(|| (hz * 1000.0).round() as u32)
}

struct SendLayer(Retained<CAMetalLayer>);
// SAFETY: CAMetalLayer is documented as usable from a rendering thread; the
// render thread is the only one that draws into it.
unsafe impl Send for SendLayer {}

struct MacVideo {
    decoder: Option<VtDecoder>,
    render: Arc<RenderShared>,
}

impl VideoOut for MacVideo {
    fn configure(
        &mut self,
        codec: Codec,
        _width: u32,
        _height: u32,
        video: u8,
    ) -> Result<(), String> {
        let codec = match codec {
            Codec::H264 => pingpong_decode::Codec::H264,
            Codec::Hevc => pingpong_decode::Codec::Hevc,
            Codec::Av1 => pingpong_decode::Codec::Av1,
        };
        let render = self.render.clone();
        let mut decoder = VtDecoder::with_sink(codec, Arc::new(move |f| render.push_frame(f)))
            .map_err(|e| e.to_string())?;
        let hdr = video & pingpong_proto::control::video::HDR != 0;
        decoder.set_format(PictureFormat {
            ten_bit: hdr,
            yuv444: video & pingpong_proto::control::video::YUV444 != 0,
        });
        self.render.set_hdr(hdr);
        self.decoder = Some(decoder);
        Ok(())
    }

    fn hdr_metadata(&mut self, metadata: pingpong_proto::control::HdrMetadata) {
        self.render.set_hdr_metadata(metadata);
    }

    fn decode(&mut self, bitstream: &[u8], timing: FrameTiming) -> Result<(), String> {
        let Some(d) = self.decoder.as_mut() else {
            return Ok(());
        };
        self.render.expect(timing);
        // The frame id rides through VideoToolbox as the per-frame tag.
        d.decode(bitstream, timing.frame_id)
            .map_err(|e| e.to_string())
    }
}

/// A running stream with its window. Create and drop on the main thread.
pub struct Session {
    window: Window,
    gamepads: Option<gamepad::Gamepads>,
    stream: Option<Stream>,
    render: Arc<RenderShared>,
    render_thread: Option<JoinHandle<()>>,
    /// Keeps the display awake (and the app out of App Nap) while streaming:
    /// someone playing with a controller touches neither keyboard nor mouse.
    awake: Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Session {
    /// Main thread.
    pub fn open(
        source: Source,
        opts: SessionOptions,
        on_end: EndCallback,
    ) -> Result<Session, String> {
        let mtm = MainThreadMarker::new().ok_or("a stream opens on the main thread")?;
        let mut s = opts.settings;
        if !pingpong_decode::videotoolbox::av1_in_hardware() {
            // No AV1 decoder here: the host picks among the rest.
            s.codecs &= !pingpong_proto::control::codec::AV1;
        }
        s.video = crate::session::video_caps(s.video, s.codecs);
        let stats = Arc::new(StatsCollector::default());
        let render = RenderShared::new(stats.clone(), s.vsync, s.frame_pacing);
        render.set_overlay(opts.show_stats);
        render.set_status(Some(format!("Connecting to {}…", source.name())));
        let window = Window::open(
            mtm,
            render.clone(),
            opts.fullscreen,
            (s.width as u32, s.height as u32),
            &format!("Ping — {}", source.name()),
        );
        let scale = window.window.backingScaleFactor();

        let cursors = cursors::load(mtm, scale);
        let layer = SendLayer(window.layer.clone());
        let render_thread = {
            let render = render.clone();
            std::thread::Builder::new()
                .name("ping-render".into())
                .spawn(move || {
                    let layer = layer;
                    crate::priority::latency_critical();
                    render::run(layer.0, render, cursors, scale)
                })
                .map_err(|e| e.to_string())?
        };

        let pointer = Arc::new(Mutex::new(PointerState::new(
            s.width as u32,
            s.height as u32,
        )));
        let forward_command = Arc::new(std::sync::atomic::AtomicBool::new(
            opts.command_is_windows_key,
        ));
        let (rumble_tx, rumble_rx) = gamepad::Gamepads::rumble_channel();
        let events: crate::stream::EventSink = {
            let (pointer, render, on_end, forward_command) = (
                pointer.clone(),
                render.clone(),
                on_end.clone(),
                forward_command.clone(),
            );
            let connection_warnings = opts.connection_warnings;
            Arc::new(move |ev| match ev {
                Event::Rumble { index, low, high } => {
                    let _ = rumble_tx.try_send((index, low, high));
                }
                Event::Cursor(c) => {
                    let mut p = pointer.lock();
                    p.apply_host(&c);
                    render.set_cursor(p.draw());
                }
                Event::Started(ack) => {
                    // Renegotiated after an interruption: the notice is over.
                    render.set_notice(None);
                    // On a Mac host, Command is Command.
                    if ack.host == pingpong_proto::control::host::MACOS {
                        forward_command.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    let mut p = pointer.lock();
                    p.started(&ack);
                    render.set_cursor(p.draw());
                }
                Event::Status(text) => {
                    tracing::info!("{text}");
                    render.set_status(Some(text));
                }
                Event::Notice(text) => render.set_notice(text),
                Event::Agent(_) | Event::Permissions(_) => {}
                Event::Warning(text) => {
                    if connection_warnings {
                        render.set_warning(text);
                    }
                }
                Event::Ended { reason, error } => {
                    if error {
                        tracing::warn!("{reason}");
                    } else {
                        tracing::info!("{reason}");
                    }
                    on_end(Some(reason));
                }
            })
        };

        let video = Box::new(MacVideo {
            decoder: None,
            render: render.clone(),
        });
        let stream = Stream::open(source, s, video, events, stats)?;

        let quit = on_end.clone();
        let mut handler = Handler::new(
            stream.input().clone(),
            render.clone(),
            pointer.clone(),
            forward_command.clone(),
            Box::new(move || quit(None)),
        );
        handler.pixels_per_point = scale;
        if opts.mute_in_background {
            let controls = stream.controls();
            handler.on_focus = Some(Box::new(move |focused| controls.set_audio_muted(!focused)));
        }
        if s.watch {
            let controls = stream.controls();
            handler.on_take_over = Some(Box::new(move || controls.toggle_take_over()));
        }
        window.view.set_handler(handler);
        window.view.with(|h| window::set_capture(h, true));
        window::centre_pointer(&window.window);

        let gamepads = {
            let controls = stream.controls();
            let mouse = opts.gamepad_mouse.then(|| {
                let render = render.clone();
                crate::pad::PadMouse {
                    pointer: pointer.clone(),
                    show: std::sync::Arc::new(move |d| render.set_cursor(d)),
                    input: stream.input().clone(),
                }
            });
            gamepad::Gamepads::start(move |msg| controls.send(msg), rumble_rx, mouse)
        };

        let awake = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
            NSActivityOptions::IdleDisplaySleepDisabled
                | NSActivityOptions::UserInitiated
                | NSActivityOptions::LatencyCritical,
            &NSString::from_str("Streaming"),
        );

        Ok(Session {
            window,
            gamepads: Some(gamepads),
            stream: Some(stream),
            render,
            render_thread: Some(render_thread),
            awake: Some(awake),
        })
    }

    /// For sending control messages from another thread (tests).
    pub fn controls(&self) -> Option<crate::stream::ControlSender> {
        self.stream.as_ref().map(|s| s.controls())
    }

    /// For adding paths to the host while connecting (discovery).
    pub fn candidates(&self) -> Option<crate::stream::Candidates> {
        self.stream.as_ref().and_then(|s| s.candidates())
    }

    /// The input channel, for scripted input (tests) and the app's own
    /// shortcuts.
    pub fn input(&self) -> Option<crate::input::InputSender> {
        self.stream.as_ref().map(|s| s.input().clone())
    }

    pub fn stats(&self) -> Option<crate::stats::Stats> {
        self.stream.as_ref().map(|s| s.stats())
    }

    pub fn toggle_stats(&self) -> bool {
        self.render.toggle_overlay()
    }

    /// Close the window as its close button does (tests).
    pub fn perform_close(&self) {
        self.window.window.performClose(None);
    }

    /// Take the window off the screen without closing it (tests: a window
    /// gone some way that did not end the stream).
    pub fn order_out(&self) {
        self.window.window.orderOut(None);
    }

    /// Bring the window to the front, out of the Dock if it is there. Main
    /// thread.
    pub fn show(&self) {
        let w = &self.window.window;
        if w.isMiniaturized() {
            w.deminiaturize(None);
        }
        w.makeKeyAndOrderFront(None);
        if let Some(mtm) = MainThreadMarker::new() {
            #[allow(deprecated)]
            objc2_app_kit::NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        }
    }

    /// The window is gone (closed some way that did not end the stream):
    /// not on screen, not in the Dock, and not because the app is hidden.
    /// Main thread.
    pub fn window_gone(&self) -> bool {
        let w = &self.window.window;
        let hidden = MainThreadMarker::new()
            .is_some_and(|mtm| objc2_app_kit::NSApplication::sharedApplication(mtm).isHidden());
        !hidden && !w.isVisible() && !w.isMiniaturized()
    }

    /// Stop streaming and close the window. Main thread.
    pub fn close(&mut self) {
        // Unplug the host's pads before the stream goes.
        drop(self.gamepads.take());
        if let Some(mut s) = self.stream.take() {
            s.stop();
        }
        self.render.stop();
        if let Some(t) = self.render_thread.take() {
            let _ = t.join();
        }
        if let Some(a) = self.awake.take() {
            unsafe { NSProcessInfo::processInfo().endActivity(&a) };
        }
        self.window.close();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}
