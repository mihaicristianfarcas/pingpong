//! Starting a stream, the same on every platform: what the user asked for,
//! the host looked up among the paired ones, and the platform's session
//! (window, decoder, presentation, input) opened on it.

use std::path::Path;
use std::sync::Arc;

use pingpong_proto::control::{app, codec, video};

use crate::store::KnownHost;
use crate::stream::{HostTarget, StreamSettings};

#[cfg(target_os = "macos")]
pub use crate::mac::Session;
#[cfg(windows)]
pub use crate::win::Session;

/// Linux: the stream runs in a process of its own (see `linux`).
#[cfg(target_os = "linux")]
pub use crate::linux::Session;

pub struct SessionOptions {
    pub settings: StreamSettings,
    pub fullscreen: bool,
    pub command_is_windows_key: bool,
    pub show_stats: bool,
    /// Silence the stream's audio while its window is in the background
    /// (Moonlight's "mute on focus loss").
    pub mute_in_background: bool,
    /// Say when the network is struggling (Moonlight's connection warnings).
    pub connection_warnings: bool,
    /// Holding a controller's Start turns it into a mouse (Moonlight's
    /// gamepad mouse emulation).
    pub gamepad_mouse: bool,
}

/// Why a session ended: `None` when the user quit. Called on any thread;
/// close the session from the thread that opened it.
pub type EndCallback = Arc<dyn Fn(Option<String>) + Send + Sync>;

/// What a stream asks for, as the app's settings say it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StreamRequest {
    pub width: u16,
    pub height: u16,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// `control::codec` bits the client accepts.
    pub codecs: u8,
    pub vsync: bool,
    pub frame_pacing: bool,
    pub fullscreen: bool,
    pub show_stats: bool,
    pub cmd_is_win: bool,
    /// 0 (no audio), 2 (stereo), 6 (5.1) or 8 (7.1).
    pub audio_channels: u8,
    pub host_audio: bool,
    pub mute_in_background: bool,
    pub connection_warnings: bool,
    pub gamepad_mouse: bool,
    /// `control::app::DESKTOP` or `STEAM_BIG_PICTURE`.
    pub app: u8,
    /// Keep the host's own displays on beside the stream's (tests).
    pub keep_host_displays: bool,
    /// Only what the internet rendezvous finds: proves the remote path.
    pub wan_only: bool,
    /// Only these addresses, nothing else (diagnostics).
    pub via: Vec<std::net::SocketAddr>,
    /// Watch the AI agent working on the host (Ctrl+Alt+Shift+T takes over).
    pub watch: bool,
    /// Share the clipboard with the host: copy on one, paste on the other.
    pub clipboard: bool,
    /// Left and right mouse buttons swapped (Moonlight's option).
    pub swap_mouse_buttons: bool,
    /// The wheel and the trackpad scroll the other way (Moonlight's option).
    pub reverse_scroll: bool,
    /// Stream in HDR when this display and the host can (Moonlight's
    /// "HDR").
    pub hdr: bool,
    /// Full-resolution colour when the host can encode it and this computer
    /// decode it (Moonlight's "YUV 4:4:4").
    pub yuv444: bool,
}

impl Default for StreamRequest {
    fn default() -> Self {
        StreamRequest {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 20_000,
            codecs: codec::H264 | codec::HEVC,
            vsync: true,
            frame_pacing: false,
            fullscreen: true,
            show_stats: false,
            cmd_is_win: false,
            audio_channels: 2,
            host_audio: false,
            mute_in_background: true,
            connection_warnings: true,
            gamepad_mouse: true,
            app: app::DESKTOP,
            keep_host_displays: false,
            wan_only: false,
            via: Vec::new(),
            watch: false,
            clipboard: true,
            swap_mouse_buttons: false,
            reverse_scroll: false,
            hdr: false,
            yuv444: false,
        }
    }
}

impl StreamRequest {
    pub fn options(&self) -> SessionOptions {
        SessionOptions {
            settings: StreamSettings {
                width: self.width,
                height: self.height,
                fps: self.fps,
                bitrate_kbps: self.bitrate_kbps,
                codecs: self.codecs,
                audio_channels: self.audio_channels,
                host_audio: self.host_audio,
                vsync: self.vsync,
                frame_pacing: self.frame_pacing,
                keep_host_displays: self.keep_host_displays,
                app: self.app,
                watch: self.watch,
                clipboard: self.clipboard,
                mouse: crate::input::MouseOptions {
                    swap_buttons: self.swap_mouse_buttons,
                    reverse_scroll: self.reverse_scroll,
                },
                // What this computer can show is the platform's to say
                // (`video_caps`), when the session opens.
                video: (if self.hdr { video::HDR } else { 0 })
                    | (if self.yuv444 { video::YUV444 } else { 0 }),
                // A watcher gets no sound (an agent's session has none).
                ..StreamSettings::default()
            },
            fullscreen: self.fullscreen,
            command_is_windows_key: self.cmd_is_win,
            show_stats: self.show_stats,
            mute_in_background: self.mute_in_background,
            connection_warnings: self.connection_warnings,
            gamepad_mouse: self.gamepad_mouse,
        }
    }
}

/// Whether this computer decodes AV1 (on a Mac, in hardware: an M3 or
/// later).
pub fn decodes_av1() -> bool {
    #[cfg(target_os = "macos")]
    {
        pingpong_decode::videotoolbox::av1_in_hardware()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// What of `asked` (`control::video::*`) this computer can show, with these
/// codecs: HDR needs HEVC or AV1, and a display with headroom above SDR
/// white; 4:4:4 a decoder for it. On a Mac, call on the main thread.
pub fn video_caps(asked: u8, codecs: u8) -> u8 {
    let mut can = 0;
    #[cfg(target_os = "macos")]
    {
        // Apple silicon decodes HEVC Main10 and 4:4:4 in hardware.
        if cfg!(target_arch = "aarch64") {
            can |= video::YUV444;
            if crate::mac::display_has_hdr() {
                can |= video::HDR;
            }
        }
    }
    // FFmpeg decodes 4:4:4 in software where VA-API does not, and the
    // presenter draws its full-size chroma planes.
    #[cfg(target_os = "linux")]
    {
        can |= video::YUV444;
    }
    if codecs & (codec::HEVC | codec::AV1) == 0 {
        can &= !video::HDR;
    }
    asked & can
}

/// The display a stream would fill, as Moonlight's "native" resolution, at
/// a standard aspect ratio (`aspect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeMode {
    /// The panel's pixels (on a Mac, below the notch).
    pub width: u16,
    pub height: u16,
    pub max_fps: u32,
    /// The scaled desktop's pixels, when that differs from the panel's
    /// (a Mac in "More Space").
    pub desktop: Option<(u16, u16)>,
}

impl Default for NativeMode {
    fn default() -> Self {
        NativeMode {
            width: 1920,
            height: 1080,
            max_fps: 60,
            desktop: None,
        }
    }
}

impl NativeMode {
    /// The sizes fitted to the standard aspect ratios games are made for.
    fn fitted(self) -> NativeMode {
        let (width, height) = crate::aspect::fit_standard_ratio(self.width, self.height);
        let desktop = self
            .desktop
            .map(|(w, h)| crate::aspect::fit_standard_ratio(w, h))
            .filter(|d| *d != (width, height));
        NativeMode {
            width,
            height,
            desktop,
            ..self
        }
    }
}

/// The main display's native mode, at a standard aspect ratio. On a Mac,
/// call on the main thread.
pub fn native_mode() -> NativeMode {
    display_mode().fitted()
}

/// The main display's own mode, whatever its aspect ratio.
fn display_mode() -> NativeMode {
    #[cfg(target_os = "macos")]
    {
        let Some(mtm) = objc2::MainThreadMarker::new() else {
            return NativeMode::default();
        };
        let Some(screen) = crate::mac::ScreenInfo::main(mtm) else {
            return NativeMode::default();
        };
        let (width, height) = screen.native_mode();
        let desktop = screen.desktop_mode();
        // 0 at times (seen from an app not yet frontmost): take 60 then.
        let max_fps = objc2_app_kit::NSScreen::mainScreen(mtm)
            .map(|s| s.maximumFramesPerSecond())
            .unwrap_or(0);
        let max_fps = if max_fps > 0 { max_fps as u32 } else { 60 };
        NativeMode {
            width,
            height,
            max_fps,
            desktop: (desktop != (width, height)).then_some(desktop),
        }
    }
    #[cfg(windows)]
    {
        crate::win::native_mode()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        NativeMode::default()
    }
}

/// Where to find `known`: its local and other addresses, and the internet
/// rendezvous when pairing set one up.
pub fn host_target(dir: &Path, known: &KnownHost, wan_only: bool) -> Result<HostTarget, String> {
    let public = known
        .public()
        .ok_or("The host's keys are damaged; pair it again.")?;
    let (mut local, mut remote) = known.candidates();
    if wan_only {
        local.clear();
        remote.clear();
    }
    let wan = crate::wan::Wan::for_host(dir, known);
    if wan_only && wan.is_none() {
        return Err(format!(
            "{} was paired before internet access existed; pair it again.",
            known.name
        ));
    }
    if local.is_empty() && remote.is_empty() && wan.is_none() {
        return Err(format!("Cannot resolve {}.", known.address));
    }
    Ok(HostTarget {
        name: known.name.clone(),
        local,
        remote,
        public,
        wan,
        data_dir: Some(dir.to_path_buf()),
    })
}

/// Stream from the paired host `key_or_name` (its X25519 key or its name).
/// On a Mac, call on the main thread. `on_end` fires once when the stream
/// ends (see [`EndCallback`]); the caller then closes the session.
#[cfg(target_os = "linux")]
pub fn start(
    key_or_name: &str,
    request: &StreamRequest,
    on_end: EndCallback,
) -> Result<Session, String> {
    crate::linux::spawn(key_or_name, request, on_end)
}

/// Stream from the paired host `key_or_name` (its X25519 key or its name).
/// On a Mac, call on the main thread. `on_end` fires once when the stream
/// ends (see [`EndCallback`]); the caller then closes the session.
#[cfg(any(target_os = "macos", windows))]
pub fn start(
    key_or_name: &str,
    request: &StreamRequest,
    on_end: EndCallback,
) -> Result<Session, String> {
    let dir = crate::store::data_dir();
    let hosts = crate::store::Hosts::load(&dir);
    let known = hosts
        .list()
        .iter()
        .find(|h| h.x25519 == key_or_name || h.name.eq_ignore_ascii_case(key_or_name))
        .cloned()
        .ok_or("That host is not paired.")?;
    let mut host = host_target(&dir, &known, request.wan_only)?;
    if !request.via.is_empty() {
        host.local.clear();
        host.remote = request.via.clone();
        host.wan = None;
    }
    let identity = Arc::new(crate::store::identity(&dir)?);
    let host_id = host.public.short_id();
    let session = Session::open(identity, host, request.options(), on_end)?;
    // Look for the host on the local network meanwhile, as Moonlight's host
    // list does, and race that path too.
    if let Some(candidates) = session
        .candidates()
        .filter(|_| !request.wan_only && request.via.is_empty())
    {
        std::thread::spawn(move || {
            let found = crate::pair::discover(&dir, std::time::Duration::from_millis(1500))
                .unwrap_or_default();
            if let Some(f) = found.iter().find(|f| f.id == host_id) {
                candidates.add_local(std::net::SocketAddr::new(f.address, f.port));
            }
        });
    }
    Ok(session)
}
