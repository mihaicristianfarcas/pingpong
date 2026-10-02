//! The Windows stream: window and input (Win32), Direct3D 11 presentation,
//! FFmpeg D3D11VA decoding, XInput controllers.

pub mod gamepad;
pub mod render;
mod text;
pub mod window;

use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::Mutex;
use pingpong_decode::d3d11va::D3d11Decoder;
use pingpong_transport::Identity;

use crate::pointer::PointerState;
use crate::session::NativeMode;
pub use crate::session::{EndCallback, SessionOptions};
use crate::stats::StatsCollector;
use crate::stream::{Codec, Event, FrameTiming, HostTarget, Stream, VideoOut};
use render::{Gpu, RenderShared};
use window::{Handler, Window};

pub use window::{clipboard_text, toggle_fullscreen};

enum Job {
    Configure(pingpong_decode::Codec),
    Frame(Vec<u8>, FrameTiming),
}

/// Decoding on its own thread: FFmpeg shares the device context with the
/// screen, and a present that waits (a refresh, a sleeping monitor) must
/// never hold up the network thread that feeds us.
struct WinVideo {
    jobs: Option<crossbeam_channel::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    /// The decoder failed since the last frame: ask the host for an IDR.
    failed: Arc<std::sync::atomic::AtomicBool>,
    render: Arc<RenderShared>,
}

impl WinVideo {
    fn start(gpu: Arc<Gpu>, render: Arc<RenderShared>) -> Result<WinVideo, String> {
        let (tx, rx) = crossbeam_channel::unbounded::<Job>();
        let failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread = {
            let (render, failed) = (render.clone(), failed.clone());
            std::thread::Builder::new()
                .name("ping-decode".into())
                .spawn(move || {
                    let mut decoder: Option<D3d11Decoder> = None;
                    for job in rx {
                        match job {
                            Job::Configure(codec) => {
                                decoder = None;
                                match D3d11Decoder::new(codec, &gpu.device, gpu.lock.clone()) {
                                    Ok(d) => decoder = Some(d),
                                    Err(e) => {
                                        tracing::error!(error = %e, "no decoder");
                                        failed.store(true, std::sync::atomic::Ordering::Release);
                                    }
                                }
                            }
                            Job::Frame(bitstream, timing) => {
                                let Some(d) = decoder.as_mut() else { continue };
                                // The frame id rides through FFmpeg as the picture's timestamp.
                                if let Err(e) = d.decode(&bitstream, timing.frame_id, |pic| {
                                    render.push_picture(&pic)
                                }) {
                                    tracing::warn!(error = %e, "decode failed");
                                    failed.store(true, std::sync::atomic::Ordering::Release);
                                }
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        Ok(WinVideo {
            jobs: Some(tx),
            thread: Some(thread),
            failed,
            render,
        })
    }
}

impl VideoOut for WinVideo {
    fn configure(&mut self, codec: Codec, _width: u32, _height: u32) -> Result<(), String> {
        let codec = match codec {
            Codec::H264 => pingpong_decode::Codec::H264,
            Codec::Hevc => pingpong_decode::Codec::Hevc,
            Codec::Av1 => pingpong_decode::Codec::Av1,
        };
        self.failed
            .store(false, std::sync::atomic::Ordering::Release);
        self.jobs
            .as_ref()
            .map(|j| j.send(Job::Configure(codec)))
            .transpose()
            .map_err(|_| "the decoder is gone".to_string())?;
        Ok(())
    }

    fn decode(&mut self, bitstream: &[u8], timing: FrameTiming) -> Result<(), String> {
        self.render.expect(timing);
        if let Some(j) = &self.jobs {
            let _ = j.send(Job::Frame(bitstream.to_vec(), timing));
        }
        if self.failed.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return Err("the decoder failed".into());
        }
        Ok(())
    }
}

impl Drop for WinVideo {
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Millisecond timers while streaming: the input thread batches motion for
/// 1 ms and the default 15.6 ms tick would stretch that sixteenfold.
struct TimerResolution;

impl TimerResolution {
    fn fine() -> TimerResolution {
        unsafe { windows::Win32::Media::timeBeginPeriod(1) };
        TimerResolution
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe { windows::Win32::Media::timeEndPeriod(1) };
    }
}

/// A running stream with its window.
pub struct Session {
    window: Option<Window>,
    gamepads: Option<gamepad::Gamepads>,
    stream: Option<Stream>,
    render: Arc<RenderShared>,
    render_thread: Option<JoinHandle<()>>,
    _timer: TimerResolution,
}

impl Session {
    pub fn open(
        identity: Arc<Identity>,
        host: HostTarget,
        opts: SessionOptions,
        on_end: EndCallback,
    ) -> Result<Session, String> {
        // Pixels, not points (the app has said so already; the CLI has not).
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        let timer = TimerResolution::fine();
        let s = opts.settings;
        let stats = Arc::new(StatsCollector::default());
        let gpu = Gpu::new()?;
        let render = RenderShared::new(gpu.clone(), stats.clone(), s.vsync, s.frame_pacing);
        render.set_overlay(opts.show_stats);
        render.set_status(Some(format!("Connecting to {}…", host.name)));

        let pointer = Arc::new(Mutex::new(PointerState::new(
            s.width as u32,
            s.height as u32,
        )));
        // The window, once it is up: the network thread tells it when the
        // host's pointer changes.
        let window_hwnd = Arc::new(std::sync::atomic::AtomicIsize::new(0));
        let (rumble_tx, rumble_rx) = crate::pad::rumble_channel();
        let events: crate::stream::EventSink = {
            let (pointer, render, on_end, window_hwnd) = (
                pointer.clone(),
                render.clone(),
                on_end.clone(),
                window_hwnd.clone(),
            );
            let connection_warnings = opts.connection_warnings;
            let cursor_changed = {
                let window_hwnd = window_hwnd.clone();
                move || {
                    let hwnd = window_hwnd.load(std::sync::atomic::Ordering::Acquire);
                    if hwnd != 0 {
                        window::post_cursor_changed(hwnd);
                    }
                }
            };
            Arc::new(move |ev| match ev {
                Event::Rumble { index, low, high } => {
                    let _ = rumble_tx.try_send((index, low, high));
                }
                Event::Cursor(c) => {
                    let mut p = pointer.lock();
                    let was_relative = p.is_relative();
                    p.apply_host(&c);
                    let (now_relative, draw) = (p.is_relative(), p.draw());
                    drop(p);
                    // Leaving a game: our pointer goes where the host's is.
                    if was_relative && !now_relative && draw.visible {
                        window::place_pointer(
                            window_hwnd.load(std::sync::atomic::Ordering::Acquire),
                            &render,
                            draw.x,
                            draw.y,
                        );
                    }
                    cursor_changed();
                }
                Event::Started(ack) => {
                    // Renegotiated after an interruption: the notice is over.
                    render.set_notice(None);
                    pointer.lock().started(&ack);
                    cursor_changed();
                }
                Event::Status(text) => {
                    tracing::info!("{text}");
                    render.set_status(Some(text));
                }
                Event::Notice(text) => render.set_notice(text),
                Event::Agent(_) => {}
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

        let video = Box::new(WinVideo::start(gpu.clone(), render.clone())?);
        let stream = Stream::start(identity, host.clone(), s, video, events, stats)?;

        let quit = on_end.clone();
        let mut handler = Handler::new(
            stream.input().clone(),
            render.clone(),
            pointer.clone(),
            Box::new(move || quit(None)),
        );
        if opts.mute_in_background {
            let controls = stream.controls();
            handler.on_focus = Some(Box::new(move |focused| controls.set_audio_muted(!focused)));
        }
        if s.watch {
            let controls = stream.controls();
            handler.on_take_over = Some(Box::new(move || controls.toggle_take_over()));
        }
        let window = match Window::open(
            handler,
            opts.fullscreen,
            (s.width as u32, s.height as u32),
            &format!("Ping — {}", host.name),
        ) {
            Ok(w) => w,
            Err(e) => {
                let mut stream = stream;
                stream.stop();
                return Err(e);
            }
        };
        window_hwnd.store(window.hwnd(), std::sync::atomic::Ordering::Release);

        let render_thread = {
            let (render, hwnd) = (render.clone(), window.hwnd());
            std::thread::Builder::new()
                .name("ping-render".into())
                .spawn(move || render::run(hwnd, render))
                .map_err(|e| e.to_string())?
        };

        let gamepads = {
            let controls = stream.controls();
            let mouse = opts.gamepad_mouse.then(|| {
                let (render, hwnd) = (render.clone(), window.hwnd());
                crate::pad::PadMouse {
                    pointer: pointer.clone(),
                    show: Arc::new(move |d| {
                        if d.absolute {
                            window::place_pointer(hwnd, &render, d.x, d.y);
                        }
                    }),
                    input: stream.input().clone(),
                }
            });
            gamepad::Gamepads::start(move |msg| controls.send(msg), rumble_rx, mouse)
        };

        Ok(Session {
            window: Some(window),
            gamepads: Some(gamepads),
            stream: Some(stream),
            render,
            render_thread: Some(render_thread),
            _timer: timer,
        })
    }

    /// For sending control messages from another thread (tests).
    pub fn controls(&self) -> Option<crate::stream::ControlSender> {
        self.stream.as_ref().map(|s| s.controls())
    }

    /// For adding paths to the host while connecting (discovery).
    pub fn candidates(&self) -> Option<crate::stream::Candidates> {
        self.stream.as_ref().map(|s| s.candidates())
    }

    /// The input channel, for scripted input (tests).
    pub fn input(&self) -> Option<crate::input::InputSender> {
        self.stream.as_ref().map(|s| s.input().clone())
    }

    pub fn stats(&self) -> Option<crate::stats::Stats> {
        self.stream.as_ref().map(|s| s.stats())
    }

    pub fn toggle_stats(&self) -> bool {
        self.render.toggle_overlay()
    }

    /// Bring the window to the front, restored if minimised.
    pub fn show(&self) {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{
            IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
        };
        let Some(w) = &self.window else { return };
        let hwnd = HWND(w.hwnd() as *mut core::ffi::c_void);
        unsafe {
            let _ = ShowWindow(
                hwnd,
                if IsIconic(hwnd).as_bool() {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            let _ = SetForegroundWindow(hwnd);
        }
    }

    /// The window is gone (closed some way that did not end the stream).
    pub fn window_gone(&self) -> bool {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        match &self.window {
            Some(w) => {
                !unsafe { IsWindow(Some(HWND(w.hwnd() as *mut core::ffi::c_void))) }.as_bool()
            }
            None => false,
        }
    }

    /// Stop streaming and close the window.
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
        if let Some(mut w) = self.window.take() {
            w.close();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// The display a stream would fill: the monitor the pointer is on, at its
/// current mode (Moonlight's "native" resolution and its refresh rate).
pub fn native_mode() -> NativeMode {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromPoint, DEVMODEW, ENUM_CURRENT_SETTINGS,
        MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTOPRIMARY,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut p = POINT::default();
        let _ = GetCursorPos(&mut p);
        let monitor = MonitorFromPoint(p, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool() {
            return NativeMode::default();
        }
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !EnumDisplaySettingsW(
            windows::core::PCWSTR(info.szDevice.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
        .as_bool()
        {
            return NativeMode::default();
        }
        NativeMode {
            width: (mode.dmPelsWidth as u16) & !1,
            height: (mode.dmPelsHeight as u16) & !1,
            max_fps: if mode.dmDisplayFrequency > 1 {
                mode.dmDisplayFrequency
            } else {
                60
            },
            desktop: None,
        }
    }
}
