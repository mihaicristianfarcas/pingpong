//! The Linux stream: a winit window (X11 or Wayland) with all keyboard and
//! mouse input, wgpu presentation, FFmpeg decoding (VA-API or software),
//! gilrs controllers, cpal audio.
//!
//! winit allows one event loop per process, and the app's is the launcher's:
//! so the app streams from a process of its own -- itself, started with
//! `--ping-stream` ([`child_main`]) -- which it watches ([`Session`]). The
//! `ping` CLI runs the stream in its own process directly ([`run`]).

pub mod gamepad;
pub mod keymap;
pub mod render;
mod text;

use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::Mutex;
use pingpong_decode::ffmpeg::FfmpegDecoder;
use pingpong_proto::control::CursorShape;
use pingpong_proto::input::{scancode, Button};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{
    DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::keyboard::PhysicalKey;
use winit::window::{CursorGrabMode, CursorIcon, Fullscreen, Window, WindowId};

use crate::input::InputSender;
use crate::keyboard::{Hotkey, KeyAction, Keyboard};
use crate::pointer::PointerState;
use crate::session::{EndCallback, StreamRequest};
use crate::stats::StatsCollector;
use crate::stream::{Codec, ControlSender, Event, FrameTiming, Source, Stream, VideoOut};
use render::{Gpu, Layout, RenderShared};

// ---------------------------------------------------------------------------
// The app's side: a stream in a process of its own.

/// A stream running in a child process of this program.
pub struct Session {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    watcher: Option<JoinHandle<()>>,
}

/// The line the stream process ends its output with: how the stream ended
/// (`null`: the user quit).
const ENDED: &str = "PING_ENDED ";

/// Stream from `key_or_name` in a process of its own; `on_end` fires when it
/// is over, with why (None: the user quit).
pub fn spawn(
    key_or_name: &str,
    request: &StreamRequest,
    on_end: EndCallback,
) -> Result<Session, String> {
    spawn_with(STREAM_FLAG, key_or_name, request, on_end)
}

/// Stream from an Xbox in a process of its own, as [`spawn`].
pub fn spawn_xbox(
    source: &crate::xbox::XboxSource,
    request: &StreamRequest,
    on_end: EndCallback,
) -> Result<Session, String> {
    let source = serde_json::to_string(source).map_err(|e| e.to_string())?;
    spawn_with(XBOX_STREAM_FLAG, &source, request, on_end)
}

/// What the app is started with to be a stream process: from a Pong host
/// (`KEY_OR_NAME REQUEST_JSON`), or from an Xbox (`SOURCE_JSON
/// REQUEST_JSON`).
pub const STREAM_FLAG: &str = "--ping-stream";
pub const XBOX_STREAM_FLAG: &str = "--ping-stream-xbox";

fn spawn_with(
    flag: &str,
    what: &str,
    request: &StreamRequest,
    on_end: EndCallback,
) -> Result<Session, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let request = serde_json::to_string(request).map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .args([flag, what, &request])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start the stream: {e}"))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().ok_or("no output from the stream")?;
    let child = Arc::new(Mutex::new(child));
    let watcher = {
        let child = child.clone();
        std::thread::Builder::new()
            .name("ping-stream-watch".into())
            .spawn(move || {
                let mut reason: Option<Option<String>> = None;
                for line in std::io::BufReader::new(stdout)
                    .lines()
                    .map_while(Result::ok)
                {
                    if let Some(json) = line.strip_prefix(ENDED) {
                        reason = serde_json::from_str::<Option<String>>(json).ok();
                    }
                }
                let status = child.lock().wait().ok();
                on_end(match reason {
                    Some(r) => r,
                    None => Some(format!(
                        "The stream stopped unexpectedly ({}).",
                        status.map(|s| s.to_string()).unwrap_or_default()
                    )),
                });
            })
            .map_err(|e| e.to_string())?
    };
    Ok(Session {
        child,
        stdin,
        watcher: Some(watcher),
    })
}

impl Session {
    /// Stop streaming: the stream process ends the session with the host
    /// and exits (killed if it does not).
    pub fn close(&mut self) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = writeln!(stdin, "quit");
        }
        for _ in 0..30 {
            if self.child.lock().try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.lock().kill();
        if let Some(t) = self.watcher.take() {
            let _ = t.join();
        }
    }

    /// The stream process looks for the host itself.
    pub fn candidates(&self) -> Option<crate::stream::Candidates> {
        None
    }

    pub fn input(&self) -> Option<InputSender> {
        None
    }

    pub fn controls(&self) -> Option<ControlSender> {
        None
    }

    /// The window is the stream process's: its window manager raises it.
    pub fn show(&self) {}

    /// Closing the stream process's window ends that process, and so the
    /// stream.
    pub fn window_gone(&self) -> bool {
        false
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.close();
    }
}

/// The stream process: `PROGRAM --ping-stream KEY REQUEST_JSON`, or
/// `--ping-stream-xbox SOURCE_JSON REQUEST_JSON`. Runs the
/// stream, says how it ended on stdout, and exits. "quit" on stdin ends it.
pub fn child_main() -> ! {
    crate::logging::init();
    let args: Vec<String> = std::env::args().collect();
    let (Some(flag), Some(what), Some(request)) = (args.get(1), args.get(2), args.get(3)) else {
        eprintln!(
            "usage: --ping-stream KEY REQUEST_JSON | --ping-stream-xbox SOURCE_JSON REQUEST_JSON"
        );
        std::process::exit(2)
    };
    let hooks = Hooks {
        stdin_quit: true,
        ..Default::default()
    };
    let outcome = match serde_json::from_str::<StreamRequest>(request) {
        Ok(request) if flag == XBOX_STREAM_FLAG => {
            match serde_json::from_str::<crate::xbox::XboxSource>(what) {
                Ok(source) => run_source(Source::Xbox(source), &request, hooks),
                Err(e) => Err(format!("bad Xbox source: {e}")),
            }
        }
        Ok(request) => run(what, &request, hooks),
        Err(e) => Err(format!("bad stream request: {e}")),
    };
    let reason = match outcome {
        Ok(r) => r,
        Err(e) => Some(e),
    };
    println!(
        "{ENDED}{}",
        serde_json::to_string(&reason).unwrap_or_else(|_| "null".into())
    );
    let _ = std::io::stdout().flush();
    std::process::exit(0)
}

// ---------------------------------------------------------------------------
// The stream process's side.

/// Handed the stream's input, its controls and a way to end it, once it runs.
pub type StartedHook = Box<dyn FnOnce(InputSender, ControlSender, EndCallback)>;

/// What the caller of [`run`] can plug in.
#[derive(Default)]
pub struct Hooks {
    /// End the stream on "quit" on stdin (the app's child).
    pub stdin_quit: bool,
    /// Called once the stream is up (tests' scripted input).
    pub started: Option<StartedHook>,
}

enum Job {
    Configure(pingpong_decode::Codec),
    Frame(Vec<u8>, FrameTiming),
}

/// Decoding on its own thread: an upload can wait on the GPU, and the
/// network thread that feeds us must never.
struct LinuxVideo {
    jobs: Option<crossbeam_channel::Sender<Job>>,
    thread: Option<JoinHandle<()>>,
    failed: Arc<AtomicBool>,
    render: Arc<RenderShared>,
}

impl LinuxVideo {
    fn start(render: Arc<RenderShared>) -> Result<LinuxVideo, String> {
        let (tx, rx) = crossbeam_channel::unbounded::<Job>();
        let failed = Arc::new(AtomicBool::new(false));
        // PING_SOFTWARE_DECODE=1: no VA-API even where there is one.
        let software = std::env::var_os("PING_SOFTWARE_DECODE").is_some();
        let thread = {
            let (render, failed) = (render.clone(), failed.clone());
            std::thread::Builder::new()
                .name("ping-decode".into())
                .spawn(move || {
                    let mut decoder: Option<FfmpegDecoder> = None;
                    for job in rx {
                        match job {
                            Job::Configure(codec) => {
                                decoder = None;
                                match FfmpegDecoder::new(codec, software).or_else(|e| {
                                    tracing::warn!(error = %e, "hardware decoder failed; \
                                        trying software");
                                    FfmpegDecoder::new(codec, true)
                                }) {
                                    Ok(d) => decoder = Some(d),
                                    Err(e) => {
                                        tracing::error!(error = %e, "no decoder");
                                        failed.store(true, Ordering::Release);
                                    }
                                }
                            }
                            Job::Frame(bitstream, timing) => {
                                let Some(d) = decoder.as_mut() else { continue };
                                if let Err(e) = d.decode(&bitstream, timing.frame_id, |pic| {
                                    render.push_picture(&pic)
                                }) {
                                    tracing::warn!(error = %e, "decode failed");
                                    failed.store(true, Ordering::Release);
                                }
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?
        };
        Ok(LinuxVideo {
            jobs: Some(tx),
            thread: Some(thread),
            failed,
            render,
        })
    }
}

impl VideoOut for LinuxVideo {
    fn configure(
        &mut self,
        codec: Codec,
        _width: u32,
        _height: u32,
        _video: u8,
    ) -> Result<(), String> {
        let codec = match codec {
            Codec::H264 => pingpong_decode::Codec::H264,
            Codec::Hevc => pingpong_decode::Codec::Hevc,
            Codec::Av1 => pingpong_decode::Codec::Av1,
        };
        self.failed.store(false, Ordering::Release);
        if let Some(j) = &self.jobs {
            j.send(Job::Configure(codec))
                .map_err(|_| "the decoder is gone".to_string())?;
        }
        Ok(())
    }

    fn decode(&mut self, bitstream: &[u8], timing: FrameTiming) -> Result<(), String> {
        self.render.expect(timing);
        if let Some(j) = &self.jobs {
            let _ = j.send(Job::Frame(bitstream.to_vec(), timing));
        }
        if self.failed.swap(false, Ordering::AcqRel) {
            return Err("the decoder failed".into());
        }
        Ok(())
    }
}

impl Drop for LinuxVideo {
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

enum UserEvent {
    /// The stream ended: why (None: the user quit).
    Ended(Option<String>),
    /// The host's pointer changed shape or mode.
    Cursor,
    /// "quit" on stdin.
    Quit,
}

struct Running {
    window: Arc<Window>,
    render: Arc<RenderShared>,
    render_thread: Option<JoinHandle<()>>,
    stream: Stream,
    gamepads: Option<gamepad::Gamepads>,
    pointer: Arc<Mutex<PointerState>>,
    keyboard: Keyboard,
    held_buttons: Vec<Button>,
    scroll_residue: (f64, f64),
}

struct StreamApp {
    request: StreamRequest,
    source: Option<Source>,
    dir: std::path::PathBuf,
    proxy: EventLoopProxy<UserEvent>,
    hooks: Option<StartedHook>,
    running: Option<Running>,
    /// How it ended; Err: it could not start.
    outcome: Option<Result<Option<String>, String>>,
}

fn cursor_icon(shape: CursorShape) -> CursorIcon {
    match shape {
        CursorShape::Arrow => CursorIcon::Default,
        CursorShape::IBeam => CursorIcon::Text,
        CursorShape::Wait => CursorIcon::Wait,
        CursorShape::Cross => CursorIcon::Crosshair,
        CursorShape::SizeAll => CursorIcon::Move,
        CursorShape::SizeNS => CursorIcon::NsResize,
        CursorShape::SizeWE => CursorIcon::EwResize,
        CursorShape::SizeNWSE => CursorIcon::NwseResize,
        CursorShape::SizeNESW => CursorIcon::NeswResize,
        CursorShape::Hand => CursorIcon::Pointer,
        CursorShape::No => CursorIcon::NotAllowed,
        CursorShape::Help => CursorIcon::Help,
        CursorShape::AppStarting => CursorIcon::Progress,
    }
}

impl StreamApp {
    fn open(&mut self, el: &ActiveEventLoop) -> Result<Running, String> {
        let r = &self.request;
        let source = self.source.take().ok_or("nothing to stream")?;
        let mut attributes = Window::default_attributes()
            .with_title(format!("Ping — {}", source.name()))
            .with_inner_size(PhysicalSize::new(r.width as u32, r.height as u32));
        if r.fullscreen {
            attributes = attributes.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        let window = Arc::new(
            el.create_window(attributes)
                .map_err(|e| format!("no stream window: {e}"))?,
        );

        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("no surface: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("no GPU: {e}"))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| format!("no GPU device: {e}"))?;
        let size = window.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .ok_or("the surface has no configuration")?;
        // Our shader writes display values: a surface that does not encode.
        if let Some(f) = caps.formats.iter().find(|f| !f.is_srgb()) {
            config.format = *f;
        }
        config.present_mode = render::present_mode(&caps, r.vsync);
        config.desired_maximum_frame_latency = 1;
        tracing::info!(adapter = adapter.get_info().name, backend = ?adapter.get_info().backend, "GPU");

        let stats = Arc::new(StatsCollector::default());
        let render = RenderShared::new(
            Arc::new(Gpu::new(device, queue, config.format)),
            stats.clone(),
            r.vsync,
        );
        render.set_overlay(r.show_stats);
        render.set_status(Some(format!("Connecting to {}…", source.name())));
        render.set_layout(Layout {
            width: size.width,
            height: size.height,
            scale: window.scale_factor(),
        });
        let render_thread = {
            let render = render.clone();
            std::thread::Builder::new()
                .name("ping-render".into())
                .spawn(move || render::run(render, surface, config))
                .map_err(|e| e.to_string())?
        };

        let mut settings = r.options().settings;
        settings.video = crate::session::video_caps(settings.video, settings.codecs);
        let pointer = Arc::new(Mutex::new(PointerState::new(
            settings.width as u32,
            settings.height as u32,
        )));
        let (rumble_tx, rumble_rx) = crate::pad::rumble_channel();
        let events: crate::stream::EventSink = {
            let (pointer, render, proxy) = (pointer.clone(), render.clone(), self.proxy.clone());
            let connection_warnings = r.connection_warnings;
            Arc::new(move |ev| match ev {
                Event::Rumble { index, low, high } => {
                    let _ = rumble_tx.try_send((index, low, high));
                }
                Event::Cursor(c) => {
                    pointer.lock().apply_host(&c);
                    let _ = proxy.send_event(UserEvent::Cursor);
                }
                Event::Started(ack) => {
                    render.set_notice(None);
                    pointer.lock().started(&ack);
                    let _ = proxy.send_event(UserEvent::Cursor);
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
                    let _ = proxy.send_event(UserEvent::Ended(Some(reason)));
                }
            })
        };
        let host_id = source.host_id();
        let video = Box::new(LinuxVideo::start(render.clone())?);
        let stream = Stream::open(source, settings, video, events, stats)?;

        // Look for a Pong host on the local network meanwhile, and race
        // that path too.
        if let (Some(candidates), Some(host_id), true) = (
            stream.candidates(),
            host_id,
            !r.wan_only && r.via.is_empty(),
        ) {
            let dir = self.dir.clone();
            std::thread::spawn(move || {
                let found =
                    crate::pair::discover(&dir, Duration::from_millis(1500)).unwrap_or_default();
                if let Some(f) = found.iter().find(|f| f.id == host_id) {
                    candidates.add_local(SocketAddr::new(f.address, f.port));
                }
            });
        }

        let gamepads = {
            let controls = stream.controls();
            let mouse = r.gamepad_mouse.then(|| {
                let (render, window) = (render.clone(), window.clone());
                crate::pad::PadMouse {
                    pointer: pointer.clone(),
                    show: Arc::new(move |d| {
                        if let (true, Some(((vx, vy, vw, vh), (sw, sh)))) =
                            (d.absolute, render.video_rect())
                        {
                            let p = PhysicalPosition::new(
                                vx + d.x as f64 * vw / sw,
                                vy + d.y as f64 * vh / sh,
                            );
                            let _ = window.set_cursor_position(p);
                        }
                    }),
                    input: stream.input().clone(),
                }
            });
            gamepad::Gamepads::start(move |msg| controls.send(msg), rumble_rx, mouse)
        };

        if let Some(started) = self.hooks.take() {
            let proxy = self.proxy.clone();
            let end: EndCallback = Arc::new(move |reason| {
                let _ = proxy.send_event(UserEvent::Ended(reason));
            });
            started(stream.input().clone(), stream.controls(), end);
        }

        Ok(Running {
            window,
            render,
            render_thread: Some(render_thread),
            stream,
            gamepads: Some(gamepads),
            pointer,
            keyboard: Keyboard::default(),
            held_buttons: Vec::new(),
            scroll_residue: (0.0, 0.0),
        })
    }

    fn finish(&mut self, el: &ActiveEventLoop, reason: Option<String>) {
        if let Some(mut r) = self.running.take() {
            let input = r.stream.input().clone();
            r.keyboard.release_all(&input);
            drop(r.gamepads.take());
            r.stream.stop();
            r.render.stop();
            if let Some(t) = r.render_thread.take() {
                let _ = t.join();
            }
        }
        if self.outcome.is_none() {
            self.outcome = Some(Ok(reason));
        }
        el.exit();
    }
}

/// Capture or release the pointer (and so the keyboard's keys) for the
/// stream: confined to the window, hidden while a game has the mouse.
fn apply_capture(r: &mut Running, captured: bool) {
    let was = {
        let mut p = r.pointer.lock();
        let was = p.captured;
        p.captured = captured;
        was
    };
    if was && !captured {
        let input = r.stream.input().clone();
        r.keyboard.release_all(&input);
        for b in r.held_buttons.drain(..) {
            input.mouse_button(b, false);
        }
    }
    apply_pointer(r);
    if was != captured {
        tracing::info!(captured, "pointer capture");
    }
}

fn apply_pointer(r: &Running) {
    let p = *r.pointer.lock();
    let w = &r.window;
    if p.captured {
        let relative = p.is_relative();
        // Locked where the system allows (Wayland), confined otherwise (X11).
        let grab = if relative {
            CursorGrabMode::Locked
        } else {
            CursorGrabMode::Confined
        };
        if w.set_cursor_grab(grab).is_err() {
            let _ = w.set_cursor_grab(CursorGrabMode::Confined);
        }
        // The host draws its pointer into the picture; an older host leaves
        // it to us on its desktop.
        w.set_cursor_visible(!relative && !p.host_draws);
        w.set_cursor(cursor_icon(p.shape));
    } else {
        let _ = w.set_cursor_grab(CursorGrabMode::None);
        w.set_cursor_visible(true);
        w.set_cursor(CursorIcon::Default);
    }
}

fn button(b: MouseButton) -> Option<Button> {
    Some(match b {
        MouseButton::Left => Button::Left,
        MouseButton::Right => Button::Right,
        MouseButton::Middle => Button::Middle,
        MouseButton::Back => Button::X1,
        MouseButton::Forward => Button::X2,
        _ => return None,
    })
}

impl ApplicationHandler<UserEvent> for StreamApp {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.running.is_some() || self.outcome.is_some() {
            return;
        }
        match self.open(el) {
            Ok(mut r) => {
                apply_capture(&mut r, true);
                self.running = Some(r);
            }
            Err(e) => {
                self.outcome = Some(Err(e));
                el.exit();
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Ended(reason) => self.finish(el, reason),
            UserEvent::Quit => self.finish(el, None),
            UserEvent::Cursor => {
                if let Some(r) = &self.running {
                    apply_pointer(r);
                }
            }
        }
    }

    fn device_event(&mut self, _el: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        let Some(r) = &self.running else { return };
        if let DeviceEvent::MouseMotion { delta: (dx, dy) } = event {
            let p = r.pointer.lock();
            if p.captured && p.is_relative() {
                r.stream.input().motion(dx, dy);
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(r) = self.running.as_mut() else {
            return;
        };
        let input = r.stream.input().clone();
        match event {
            WindowEvent::CloseRequested => self.finish(el, None),
            WindowEvent::Focused(focused) => {
                if !focused {
                    r.keyboard.forget_modifiers();
                }
                apply_capture(r, focused);
                if self.request.mute_in_background {
                    r.stream.controls().set_audio_muted(!focused);
                }
            }
            WindowEvent::Resized(size) => {
                r.render.set_layout(Layout {
                    width: size.width,
                    height: size.height,
                    scale: r.window.scale_factor(),
                });
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = r.window.inner_size();
                r.render.set_layout(Layout {
                    width: size.width,
                    height: size.height,
                    scale: scale_factor,
                });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let Some(key) = keymap::key_for(code) else {
                    return;
                };
                let captured = r.pointer.lock().captured;
                let down = event.state == ElementState::Pressed;
                if let KeyAction::Hotkey(h) = r.keyboard.key(scancode(key), down, captured, &input)
                {
                    match h {
                        Hotkey::Quit => self.finish(el, None),
                        Hotkey::Stats => {
                            let on = r.render.toggle_overlay();
                            tracing::info!(on, "statistics overlay");
                        }
                        Hotkey::Capture => {
                            let captured = !r.pointer.lock().captured;
                            apply_capture(r, captured);
                        }
                        Hotkey::Paste => {
                            if let Some(text) = arboard::Clipboard::new()
                                .ok()
                                .and_then(|mut c| c.get_text().ok())
                            {
                                r.keyboard.release_all(&input);
                                let chars = input.type_text(&text);
                                tracing::info!(chars, "clipboard typed on the host");
                            }
                        }
                        Hotkey::MouseMode => {
                            let relative = r.pointer.lock().toggle_mode();
                            tracing::info!(relative, "mouse mode");
                            apply_pointer(r);
                        }
                        Hotkey::Fullscreen => {
                            let to_window = r.window.fullscreen().is_some();
                            r.window.set_fullscreen(if to_window {
                                None
                            } else {
                                Some(Fullscreen::Borderless(None))
                            });
                            tracing::info!(fullscreen = !to_window, "window mode");
                        }
                        Hotkey::Minimize => {
                            apply_capture(r, false);
                            r.window.set_minimized(true);
                        }
                        Hotkey::TakeOver => {
                            r.keyboard.release_all(&input);
                            r.stream.controls().toggle_take_over();
                        }
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let mut p = r.pointer.lock();
                if !p.captured || p.is_relative() {
                    return;
                }
                if let Some(((vx, vy, vw, vh), (sw, sh))) = r.render.video_rect() {
                    if vw > 0.0 && vh > 0.0 {
                        let x = ((position.x - vx) * sw / vw) as f32;
                        let y = ((position.y - vy) * sh / vh) as f32;
                        p.place(x, y, &input);
                    }
                }
            }
            WindowEvent::MouseInput {
                state, button: b, ..
            } => {
                let captured = r.pointer.lock().captured;
                let down = state == ElementState::Pressed;
                if !captured {
                    // The first click into an uncaptured window recaptures.
                    if down {
                        apply_capture(r, true);
                    }
                    return;
                }
                let Some(b) = button(b) else { return };
                if down {
                    if !r.held_buttons.contains(&b) {
                        r.held_buttons.push(b);
                        input.mouse_button(b, true);
                    }
                } else if let Some(i) = r.held_buttons.iter().position(|x| *x == b) {
                    r.held_buttons.remove(i);
                    input.mouse_button(b, false);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if !r.pointer.lock().captured {
                    return;
                }
                // Windows counts 120 per wheel notch; a touchpad's pixels are
                // finer (about 40 to a notch).
                let (x, y) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64 * 120.0, y as f64 * 120.0),
                    MouseScrollDelta::PixelDelta(p) => (p.x * 3.0, p.y * 3.0),
                };
                let (rx, ry) = r.scroll_residue;
                let (fx, fy) = (rx + x, ry + y);
                let (dh, dv) = (fx.trunc(), fy.trunc());
                r.scroll_residue = (fx - dh, fy - dv);
                if dv != 0.0 || dh != 0.0 {
                    input.wheel(
                        dv.clamp(i16::MIN as f64, i16::MAX as f64) as i16,
                        dh.clamp(i16::MIN as f64, i16::MAX as f64) as i16,
                    );
                }
            }
            _ => {}
        }
    }
}

/// Run a stream from `key_or_name` on this thread (the process's main
/// thread: winit's event loop). Returns how it ended: None when the user
/// quit, else why.
pub fn run(
    key_or_name: &str,
    request: &StreamRequest,
    hooks: Hooks,
) -> Result<Option<String>, String> {
    let dir = crate::store::data_dir();
    let hosts = crate::store::Hosts::load(&dir);
    let known = hosts
        .list()
        .iter()
        .find(|h| h.x25519 == key_or_name || h.name.eq_ignore_ascii_case(key_or_name))
        .cloned()
        .ok_or("That host is not paired.")?;
    let mut host = crate::session::host_target(&dir, &known, request.wan_only)?;
    if !request.via.is_empty() {
        host.local.clear();
        host.remote = request.via.clone();
        host.wan = None;
    }
    let identity = Arc::new(crate::store::identity(&dir)?);
    run_source(Source::Pong { identity, host }, request, hooks)
}

/// Run a stream from `source` on this thread, as [`run`].
pub fn run_source(
    source: Source,
    request: &StreamRequest,
    hooks: Hooks,
) -> Result<Option<String>, String> {
    let dir = crate::store::data_dir();
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .map_err(|e| format!("no display: {e}"))?;
    let proxy = event_loop.create_proxy();
    if hooks.stdin_quit {
        let proxy = proxy.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if line.trim() == "quit" {
                    let _ = proxy.send_event(UserEvent::Quit);
                    return;
                }
            }
            // The app is gone: so is the stream.
            let _ = proxy.send_event(UserEvent::Quit);
        });
    }
    let mut app = StreamApp {
        request: request.clone(),
        source: Some(source),
        dir,
        proxy,
        hooks: hooks.started,
        running: None,
        outcome: None,
    };
    event_loop.run_app(&mut app).map_err(|e| e.to_string())?;
    app.outcome.unwrap_or(Ok(None))
}
