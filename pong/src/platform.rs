//! The Windows host's side of a session (see `session`): a SudoVDA virtual
//! display at the client's mode, made the whole desktop (without SudoVDA, the
//! host's main display as it is); Desktop Duplication of it, the pointer
//! drawn in; NVENC, or Media Foundation's H.264 encoder where there is no
//! NVENC; SendInput; WASAPI loopback; ViGEm pads; the app a session opens.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpong_display::windows::WindowsDisplay;
use pingpong_display::{DisplayControl, DisplayError, DisplayMode};
use pingpong_encode::nvenc::CodecCaps;
use pingpong_encode::{Codec, EncoderConfig};
use pingpong_input::SendInputSink;
use pingpong_proto::control::{self, AckStatus, Control, SessionStart};
use pingpong_transport::{Endpoint, Peer};

use crate::audio::{AudioHandle, AudioParams};
use crate::config::HostConfig;
use crate::negotiate::Negotiated;
use crate::session::Shared;
use crate::video::{Backend, VideoParams};

pub const HOST_KIND: u8 = control::host::WINDOWS;

/// Where the client's input goes.
pub type Sink = SendInputSink;

/// After a session ends, the virtual display (and the host's monitors off)
/// stays this long: a client coming back -- a changed setting, Desktop then
/// Steam -- finds it ready at once. Putting the host's desktop back means
/// waking its monitor, which asleep takes seconds (26 s measured), and every
/// display call waits for that. Someone at the host ends it at once.
const LINGER: Duration = Duration::from_secs(60);

/// A session's display, while it streams.
pub struct Display {
    gdi_name: String,
    /// SDR white in its HDR desktop, cd/m² (203, BT.2408's, when SDR).
    sdr_white: u16,
    /// What the client hears of an HDR stream, read once the display is up.
    hdr: control::HdrMetadata,
    /// A virtual display made for the session, rather than the host's own.
    made: bool,
}

pub struct Platform {
    display: WindowsDisplay,
    data_dir: PathBuf,
    /// The app this session opened (`control::app`), kept across a
    /// renegotiation and closed when the session ends.
    app: Option<u8>,
    /// A session ended and its display is kept for the next (see `LINGER`):
    /// since when, and the host's last-input time then.
    linger: Option<(Instant, u32)>,
    /// The NVIDIA driver's settings for streaming, put back when Pong stops.
    _nvidia: Option<crate::nvprefs::Applied>,
    /// The encoder, and what it can do per codec (probed once).
    encoder: std::cell::OnceCell<(Backend, Vec<CodecCaps>)>,
    /// What Windows is asked for while a session streams.
    streaming: Option<crate::tuning::Streaming>,
}

fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("display-state.toml")
}

impl Platform {
    pub fn new(data_dir: &Path) -> Platform {
        let path = state_path(data_dir);
        // A previous run that died mid-session left the host's monitors off.
        WindowsDisplay::restore_stale(&path);
        crate::audio::restore_stale(data_dir);
        let cfg = HostConfig::load_or_default(data_dir);
        Platform {
            display: WindowsDisplay::new(path),
            data_dir: data_dir.to_path_buf(),
            app: None,
            linger: None,
            _nvidia: crate::nvprefs::apply(data_dir, cfg.nvidia_max_power, cfg.nvidia_dxgi_present),
            streaming: None,
            encoder: std::cell::OnceCell::new(),
        }
    }

    pub fn supported_codecs(&self) -> Vec<Codec> {
        self.caps().iter().map(|c| c.codec).collect()
    }

    fn caps(&self) -> &[CodecCaps] {
        &self.encoder().1
    }

    /// The GPU's encoder, probed on the first output's device; without
    /// NVENC, Media Foundation's H.264 encoder (8-bit 4:2:0 only).
    fn encoder(&self) -> &(Backend, Vec<CodecCaps>) {
        self.encoder.get_or_init(|| {
            let h264 = || {
                vec![CodecCaps {
                    codec: Codec::H264,
                    ten_bit: false,
                    yuv444: false,
                }]
            };
            let Some((name, _)) = pingpong_capture::gpu::Gpu::list_outputs()
                .into_iter()
                .next()
            else {
                return (Backend::Nvenc, h264());
            };
            match pingpong_capture::gpu::Gpu::for_output(&name) {
                Ok(gpu) => match pingpong_encode::nvenc::capabilities(&gpu.device) {
                    Ok(caps) => (Backend::Nvenc, caps),
                    Err(e) if pingpong_encode::mf::h264_available() => {
                        tracing::info!(
                            error = %e,
                            "no NVENC; encoding with Media Foundation's H.264 encoder (software)"
                        );
                        (Backend::MediaFoundation, h264())
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "NVENC unavailable");
                        (Backend::Nvenc, Vec::new())
                    }
                },
                Err(e) => {
                    tracing::error!(error = %e, "no GPU output to probe");
                    (Backend::Nvenc, h264())
                }
            }
        })
    }

    /// What of `asked` (`control::video`) the encoder can stream with `codec`:
    /// HDR where it encodes 10-bit (HEVC, AV1), 4:4:4 where it encodes that.
    /// Not both: NVENC takes 10-bit 4:4:4 from CUDA only, not from a D3D11
    /// texture (as Sunshine's `nvenc_d3d11_native.cpp` says), so HDR wins.
    pub fn video_caps(&self, codec: Codec, asked: u8) -> u8 {
        use control::video::{HDR, YUV444};
        let Some(c) = self.caps().iter().find(|c| c.codec == codec) else {
            return 0;
        };
        let mut can = 0;
        if c.ten_bit {
            can |= HDR;
        }
        if c.yuv444 {
            can |= YUV444;
        }
        let mut video = asked & can;
        if video & HDR != 0 {
            video &= !YUV444;
        }
        video
    }

    /// The HDR stream's metadata: the virtual display's light as Windows
    /// reports it; BT.2020 primaries, as Sunshine reports them for its scRGB
    /// to PQ conversion (`display_base.cpp`).
    pub fn hdr_metadata(&self, d: &Display) -> control::HdrMetadata {
        d.hdr
    }

    /// Anything that stops a session before it starts.
    pub fn preflight(&self) -> Result<(), AckStatus> {
        Ok(())
    }

    /// What to tell a refused client besides the refusal.
    pub fn refusal_note(&self, _status: AckStatus) -> Option<Control> {
        None
    }

    /// A client is starting a session: a lingering display is its.
    pub fn claim(&mut self) {
        if self.linger.take().is_some() {
            tracing::info!("a client is back: the kept virtual display is its");
        }
    }

    /// The display to stream: a virtual one at the client's mode (reused when
    /// it is already up at that mode). Where SudoVDA can be had, sessions only
    /// ever stream a virtual display made for the client, never the host's
    /// own monitors; without it, the host's main display.
    pub fn display(
        &mut self,
        n: &Negotiated,
        _req: &SessionStart,
        keep_host_displays: bool,
        _cfg: &HostConfig,
    ) -> Result<Display, AckStatus> {
        let (width, height, fps_mhz) = (n.width, n.height, n.fps_mhz);
        self.display.set_isolate(!keep_host_displays);
        // SudoVDA takes millihertz: the client's 59.94 is the display's.
        let mode = DisplayMode {
            width,
            height,
            refresh_mhz: fps_mhz,
        };
        match self.display.activate(mode) {
            Ok(active) => {
                // HDR on for a session that streams it, off otherwise (the
                // display's identity keeps what the last session left). The
                // first switch on a new display blocks in Windows for 3.8 s,
                // later ones take 25 ms (Windows 11 25H2, RTX 3070 Ti).
                let hdr = n.video & control::video::HDR != 0;
                match self.display.set_hdr(hdr) {
                    Ok(true) => tracing::info!(hdr, "virtual display switched"),
                    Ok(false) => {}
                    Err(e) => tracing::warn!(error = %e, hdr, "virtual display's HDR not switched"),
                }
                let sdr_white = if hdr {
                    self.display.sdr_white_nits().unwrap_or(203)
                } else {
                    203
                };
                let mut metadata = control::HdrMetadata::bt2020(1000, sdr_white);
                if let Some((max, min, full)) =
                    pingpong_capture::gpu::Gpu::output_luminance(&active.gdi_name)
                {
                    if hdr && max >= 100.0 {
                        metadata.max_luminance = max.min(10_000.0) as u16;
                        metadata.min_luminance =
                            (min * 10_000.0).clamp(0.0, u16::MAX as f32) as u16;
                        metadata.max_fall = full.min(10_000.0) as u16;
                    }
                }
                Ok(Display {
                    gdi_name: active.gdi_name,
                    sdr_white,
                    hdr: metadata,
                    made: true,
                })
            }
            Err(DisplayError::VddUnavailable) => self.host_display(),
            Err(e) => {
                tracing::error!(error = %e, "virtual display unavailable; refusing the session");
                Err(AckStatus::VddUnavailable)
            }
        }
    }

    /// No virtual display driver (a virtual machine, a Windows on Arm PC:
    /// SudoVDA is built for x64 only): stream the host's main display as it
    /// is, scaled to the client's mode, as the macOS host does without a
    /// virtual display and as Sunshine always does. Its monitor stays on, so
    /// someone at the host sees the session (see `isolates_host_displays`).
    fn host_display(&self) -> Result<Display, AckStatus> {
        let name = pingpong_display::windows::primary_id()
            .and_then(pingpong_display::windows::gdi_name_for_target)
            .ok_or(AckStatus::VddUnavailable)?;
        tracing::warn!(
            display = %name,
            "no virtual display driver (SudoVDA); streaming the host's main display"
        );
        Ok(Display {
            gdi_name: name,
            sdr_white: 203,
            hdr: control::HdrMetadata::bt2020(1000, 203),
            made: false,
        })
    }

    pub fn video_params(
        &self,
        d: &Display,
        encoder: EncoderConfig,
        cfg: &HostConfig,
        req: &SessionStart,
    ) -> VideoParams {
        VideoParams {
            gdi_name: d.gdi_name.clone(),
            backend: self.encoder().0,
            encoder: EncoderConfig {
                two_pass: cfg.nvenc_two_pass,
                slices: req.slices.max(1) as u32,
                ..encoder
            },
            pace_mbps: cfg.pace_mbps.max(10),
            sdr_white_nits: d.sdr_white,
        }
    }

    /// The video could not start on `d`: the host's displays come back.
    pub fn abandon(&mut self, _d: Display) {
        let _ = self.display.restore();
    }

    /// Input goes to the display just made primary (with the host monitors
    /// off, the whole desktop), whatever mode a game gives it meanwhile.
    pub fn input_sink(&self, _d: &Display, width: u16, height: u16) -> Sink {
        SendInputSink::new((width as u32, height as u32))
    }

    /// Input for the session is over.
    pub fn input_done(&self, _sink: &Sink) {}

    /// Stereo, 5.1 or 7.1, as asked (Moonlight's audio configuration), with
    /// its channel count.
    pub fn audio(
        &self,
        _d: &Display,
        req: &SessionStart,
        bitrate_kbps: u32,
        endpoint: Arc<Endpoint>,
        peer: Arc<Peer>,
    ) -> Option<(AudioHandle, u8)> {
        let channels = match req.audio_channels {
            0 => return None,
            1..=2 => 2,
            3..=6 => 6,
            _ => 8,
        };
        let params = AudioParams {
            channels,
            bitrate_bps: pingpong_audio::bitrate_bps(channels, bitrate_kbps),
            host_audio: req.flags & control::flags::HOST_AUDIO != 0,
            state_path: crate::audio::state_path(&self.data_dir),
        };
        match AudioHandle::start(params, endpoint, peer) {
            Ok(a) => Some((a, channels)),
            Err(e) => {
                tracing::warn!(error = %e, "audio unavailable; streaming video only");
                None
            }
        }
    }

    /// The session's pipeline is up, just before the client hears so: keep
    /// the host awake, plug in the client's pads.
    pub fn starting(&mut self, peer: &Arc<Peer>, shared: &Shared, endpoint: &Arc<Endpoint>) {
        stay_awake(true);
        if self.streaming.is_none() {
            self.streaming = Some(crate::tuning::Streaming::start());
        }
        let mut pads = shared.pads.lock();
        if pads.as_ref().is_none_or(|(p, _)| *p != peer.id()) {
            *pads = Some((
                peer.id(),
                crate::gamepad::Pads::start(endpoint.clone(), peer.clone()),
            ));
        }
    }

    /// The session runs: the app it asked for, once its display is there to
    /// open on.
    pub fn started(&mut self, req: &SessionStart) {
        if self.app == Some(req.app) {
            return;
        }
        self.close_app();
        if let Some(a) = crate::apps::app(req.app) {
            match crate::apps::open_as_user(a.open) {
                Ok(()) => tracing::info!(app = a.name, "app opened"),
                Err(e) => tracing::warn!(app = a.name, error = %e, "could not open the app"),
            }
        }
        self.app = Some(req.app);
    }

    fn close_app(&mut self) {
        let Some(a) = self.app.take().and_then(crate::apps::app) else {
            return;
        };
        if let Some(close) = a.close {
            match crate::apps::open_as_user(close) {
                Ok(()) => tracing::info!(app = a.name, "app closed"),
                Err(e) => tracing::warn!(app = a.name, error = %e, "could not close the app"),
            }
        }
    }

    /// Every tick while streaming: nothing more to tell the client.
    pub fn tick(&mut self, _d: &mut Display, _now: Instant, _second: bool) -> Vec<Control> {
        Vec::new()
    }

    /// The session is over (`ran`: it had started). The virtual display
    /// lingers for a returning client -- unless the host is shutting down.
    pub fn ended(&mut self, display: Option<Display>, ran: bool, shutdown: bool, shared: &Shared) {
        // The host's own display has nothing to keep or put back.
        let made = display.as_ref().is_none_or(|d| d.made);
        drop(display);
        self.close_app();
        // Unplug the virtual pads (a renegotiation keeps them, so games do
        // not see the controller vanish).
        drop(shared.pads.lock().take());
        if ran {
            stay_awake(false);
            self.streaming = None;
            if !shutdown && made {
                self.linger = Some((Instant::now(), last_input()));
                tracing::info!(
                    secs = LINGER.as_secs(),
                    "keeping the virtual display for a returning client"
                );
            }
        }
        if shutdown {
            self.restore_display();
        }
    }

    /// Between sessions: give a lingering display back once nobody came
    /// back for it, or someone is at the host.
    pub fn idle(&mut self) {
        let Some((since, input)) = self.linger else {
            return;
        };
        if last_input() != input {
            tracing::info!("someone is at the host; putting its desktop back");
            self.restore_display();
        } else if since.elapsed() >= LINGER {
            tracing::info!("no client came back; putting the host's desktop back");
            self.restore_display();
        }
    }

    /// Put the host's desktop back: its monitors, arrangement and primary.
    fn restore_display(&mut self) {
        self.linger = None;
        if let Err(e) = self.display.restore() {
            tracing::warn!(error = %e, "display restore failed");
        }
    }
}

/// How long since the host last had keyboard or mouse input of any kind
/// (its own, or injected): the session tells someone at the host from its
/// agent by when it last injected.
pub fn host_input_idle() -> Option<Duration> {
    use windows::Win32::System::SystemInformation::GetTickCount;
    let last = last_input();
    let now = unsafe { GetTickCount() };
    Some(Duration::from_millis(now.wrapping_sub(last) as u64))
}

/// The session's virtual display is the whole desktop and the host's own
/// monitors are off (Sunshine's `ensure_only_display`, unless the client or
/// the config keeps them): someone at the host sees nothing. Without SudoVDA
/// the session streams the host's own display, which stays on.
pub fn isolates_host_displays(req: &SessionStart, cfg: &HostConfig) -> bool {
    !cfg.keep_host_displays
        && req.flags & pingpong_proto::control::flags::KEEP_HOST_DISPLAYS == 0
        && pingpong_display::windows::virtual_display_present()
}

/// The input desktop is not the user's (sign-in, lock screen, UAC,
/// Ctrl+Alt+Del): a person's to answer, never an agent's.
pub fn secure_screen() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_ACCESS_FLAGS,
        DESKTOP_CONTROL_FLAGS, UOI_NAME,
    };
    unsafe {
        let Ok(desk) = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x0001),
        ) else {
            // Not even readable: the secure desktop, to a process that is
            // not SYSTEM.
            return true;
        };
        let mut name = [0u16; 64];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            (name.len() * 2) as u32,
            Some(&mut needed),
        )
        .is_ok();
        let _ = CloseDesktop(desk);
        if !ok {
            return false;
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        !String::from_utf16_lossy(&name[..len]).eq_ignore_ascii_case("Default")
    }
}

/// When the host last had keyboard or mouse input (GetLastInputInfo's tick).
fn last_input() -> u32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    let mut info = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    unsafe {
        let _ = GetLastInputInfo(&mut info);
    }
    info.dwTime
}

/// Keep the display and system awake for the session's duration: a remote
/// user resets no idle timer, and Windows powering the display off ends the
/// capture (measured: frames stopped after exactly 15 minutes).
fn stay_awake(on: bool) {
    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    };
    unsafe {
        if on {
            SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED);
            let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
        } else {
            SetThreadExecutionState(ES_CONTINUOUS);
            let _ = SetPriorityClass(GetCurrentProcess(), NORMAL_PRIORITY_CLASS);
        }
    }
}
