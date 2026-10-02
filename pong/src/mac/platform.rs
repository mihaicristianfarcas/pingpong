//! The Mac host's side of a session (see `session`): a CGVirtualDisplay at
//! the client's mode, made the desktop (the Mac's own displays mirroring it
//! unless the client keeps them); ScreenCaptureKit of it; CoreGraphics
//! events for input; the system's sound, in surround where the Mac has it
//! (`sound`). No controllers yet: macOS makes virtual ones only for a
//! program signed with an entitlement Apple grants on request.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpong_capture::sck::SckCapture;
use pingpong_encode::videotoolbox::VtEncoder;
use pingpong_encode::{Codec, EncoderConfig};
use pingpong_input::macos::CgEventSink;
use pingpong_input::{InputError, InputSink};
use pingpong_proto::control::{self, AckStatus, Control, SessionStart};
use pingpong_proto::input::InputEvent;
use pingpong_transport::{Endpoint, Peer};

use crate::audio::AudioHandle;
use crate::config::HostConfig;
use crate::session::Shared;
use crate::video::VideoParams;

pub const HOST_KIND: u8 = control::host::MACOS;

/// What the video pipeline captures: a CoreGraphics display.
pub type VideoSource = u32;

/// Where the client's input goes: CoreGraphics events on the streamed
/// display, counted so the log shows input arriving even where macOS drops
/// it (no Accessibility permission).
pub struct Sink {
    inner: CgEventSink,
    received: u64,
}

impl InputSink for Sink {
    fn inject(&mut self, events: &[InputEvent]) -> Result<(), InputError> {
        self.received += events.len() as u64;
        self.inner.inject(events)
    }

    fn release_all(&mut self) -> Result<(), InputError> {
        self.inner.release_all()
    }
}

/// A session's display, while it streams (unplugged when dropped).
pub struct Display {
    /// The CoreGraphics display streamed.
    id: u32,
    /// None: the main display, when no virtual one could be made.
    _virtual: Option<pingpong_display::macos::VirtualDisplay>,
    /// The display awake for as long as the session runs.
    _awake: crate::power::Awake,
    /// The input warning told so many times: control messages are not
    /// retransmitted, so a few times over.
    told: u32,
    input_blocked: bool,
    /// The pointer's shape and place, for the client to draw.
    cursor: pingpong_input::cursor::CursorWatcher,
}

pub struct Platform;

impl Platform {
    pub fn new(_data_dir: &Path) -> Platform {
        Platform
    }

    /// VideoToolbox on Apple silicon encodes both; AV1 encoding it does not
    /// have.
    pub fn supported_codecs(&self) -> Vec<Codec> {
        vec![Codec::Hevc, Codec::H264]
    }

    /// Anything that stops a session before it starts.
    pub fn preflight(&self) -> Result<(), AckStatus> {
        if !crate::permissions::screen_capture_allowed() {
            tracing::error!("no Screen Recording permission; refusing the session");
            return Err(AckStatus::Failed);
        }
        Ok(())
    }

    /// Say why, where the client can show it (older clients ignore it).
    pub fn refusal_note(&self, status: AckStatus) -> Option<Control> {
        (status == AckStatus::Failed && !crate::permissions::screen_capture_allowed())
            .then_some(Control::HostWarning(control::host_warning::CAPTURE_BLOCKED))
    }

    /// A client is starting a session.
    pub fn claim(&mut self) {}

    /// The display to stream: a virtual one at the client's mode, made the
    /// desktop; the main display when none can be made.
    pub fn display(
        &mut self,
        width: u16,
        height: u16,
        fps: u32,
        _req: &SessionStart,
        keep_host_displays: bool,
        cfg: &HostConfig,
    ) -> Result<Display, AckStatus> {
        // An asleep display cannot be captured at all: wake it first.
        let awake = crate::power::Awake::hold();
        let mode = pingpong_display::DisplayMode {
            width,
            height,
            refresh_mhz: fps * 1000,
        };
        let virtual_display = match pingpong_display::macos::VirtualDisplay::new(mode, &cfg.name) {
            Ok(mut d) => {
                if let Err(e) = d.make_main(!keep_host_displays) {
                    tracing::warn!(error = %e, "could not make the virtual display the desktop");
                }
                tracing::info!(
                    id = d.id,
                    width,
                    height,
                    fps,
                    mirrored = !keep_host_displays,
                    "virtual display plugged in"
                );
                Some(d)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no virtual display; streaming the main display");
                None
            }
        };
        let input_blocked = !pingpong_input::macos::trusted();
        if input_blocked {
            tracing::warn!(
                "input from the client will be dropped: macOS has not given this process the Accessibility \
                    permission (System Settings > Privacy & Security > Accessibility)"
            );
        }
        let id = virtual_display
            .as_ref()
            .map_or_else(pingpong_capture::sck::SckCapture::main_display, |d| d.id);
        Ok(Display {
            id,
            _virtual: virtual_display,
            _awake: awake,
            told: 0,
            input_blocked,
            cursor: pingpong_input::cursor::CursorWatcher::new(id, (width as u32, height as u32)),
        })
    }

    pub fn video_params(
        &self,
        d: &Display,
        encoder: EncoderConfig,
        cfg: &HostConfig,
        _req: &SessionStart,
    ) -> VideoParams {
        VideoParams {
            source: d.id,
            encoder: EncoderConfig {
                two_pass: false,
                slices: 1,
                ..encoder
            },
            pace_mbps: cfg.pace_mbps.max(10),
            // The client draws the pointer (`CursorWatcher`): none in the picture.
            cursor: false,
        }
    }

    /// The video could not start on `d`: unplug it.
    pub fn abandon(&mut self, d: Display) {
        drop(d);
    }

    pub fn input_sink(&self, d: &Display, width: u16, height: u16) -> Sink {
        Sink {
            inner: CgEventSink::new(d.id, width as u32, height as u32),
            received: 0,
        }
    }

    /// Input for the session is over.
    pub fn input_done(&self, sink: &Sink) {
        if sink.received > 0 {
            tracing::info!(events = sink.received, "input received");
        }
    }

    /// The system's sound, and its channel count: in surround when the
    /// client asks and the Mac can (see `sound`), else in stereo.
    pub fn audio(
        &self,
        d: &Display,
        req: &SessionStart,
        bitrate_kbps: u32,
        endpoint: Arc<Endpoint>,
        peer: Arc<Peer>,
    ) -> Option<(AudioHandle, u8)> {
        crate::sound::start(d.id, req, bitrate_kbps, endpoint, peer)
    }

    /// The session's pipeline is up, just before the client hears so.
    pub fn starting(&mut self, _peer: &Arc<Peer>, _shared: &Shared, _endpoint: &Arc<Endpoint>) {}

    /// The session runs.
    pub fn started(&mut self, _req: &SessionStart) {}

    /// Once a second while streaming, the first few seconds: the pointer is
    /// in the picture, so the client must not draw one; and whether input
    /// reaches anything.
    pub fn tick(&mut self, d: &mut Display, now: Instant, second: bool) -> Vec<Control> {
        let mut out: Vec<Control> = d
            .cursor
            .poll(now)
            .map(Control::CursorState)
            .into_iter()
            .collect();
        if second && d.told < 3 {
            d.told += 1;
            if d.input_blocked {
                out.push(Control::HostWarning(control::host_warning::INPUT_BLOCKED));
            }
        }
        out
    }

    /// The session is over: its display is unplugged (dropped).
    pub fn ended(
        &mut self,
        display: Option<Display>,
        _ran: bool,
        _shutdown: bool,
        _shared: &Shared,
    ) {
        drop(display);
    }

    /// Between sessions.
    pub fn idle(&mut self) {}
}

/// The session's capture of its display, and an encoder at its size
/// (ScreenCaptureKit scales to it and hands over NV12: no conversion step).
pub fn open_video(params: &VideoParams) -> Result<(SckCapture, VtEncoder), String> {
    let e = params.encoder;
    // A display just woken takes a moment to be offered for capture.
    let woken_by = Instant::now() + Duration::from_secs(4);
    let cap = loop {
        match SckCapture::new(params.source, e.width, e.height, e.fps, params.cursor) {
            Ok(c) => break c,
            Err(pingpong_capture::CaptureError::NoSuchOutput(_)) if Instant::now() < woken_by => {
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let enc = VtEncoder::new(e).map_err(|e| e.to_string())?;
    Ok((cap, enc))
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    fn CGSessionCopyCurrentDictionary() -> *const std::ffi::c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDictionaryGetValue(
        dict: *const std::ffi::c_void,
        key: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CFBooleanGetValue(b: *const std::ffi::c_void) -> u8;
    fn CFRelease(cf: *const std::ffi::c_void);
}

/// The Mac's own panel mirrors the session's display: someone at the Mac
/// sees what the agent does.
pub fn isolates_host_displays(_req: &SessionStart, _cfg: &HostConfig) -> bool {
    false
}

/// How long since the Mac last had keyboard or mouse input (the HID
/// system's count, posted events included): the session tells someone at
/// the Mac from its agent by when it last injected.
pub fn host_input_idle() -> Option<Duration> {
    // kCGEventSourceStateHIDSystemState, kCGAnyInputEventType.
    let secs = unsafe { CGEventSourceSecondsSinceLastEventType(1, u32::MAX) };
    (secs.is_finite() && secs >= 0.0).then(|| Duration::from_secs_f64(secs))
}

/// The screen is locked (or the login window is up): a person's to answer.
pub fn secure_screen() -> bool {
    use objc2_core_foundation::CFString;
    unsafe {
        let dict = CGSessionCopyCurrentDictionary();
        if dict.is_null() {
            // No window server session for this process: nobody signed in.
            return true;
        }
        let key = CFString::from_static_str("CGSSessionScreenIsLocked");
        let value = CFDictionaryGetValue(dict, (&*key as *const CFString).cast());
        let locked = !value.is_null() && CFBooleanGetValue(value) != 0;
        CFRelease(dict);
        locked
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_session_can_read_idle_time_and_the_lock() {
        // Whatever the Mac is doing, both answer (and neither crashes).
        let idle = super::host_input_idle().expect("the HID system's idle time");
        assert!(idle < std::time::Duration::from_secs(365 * 24 * 3600));
        let locked = super::secure_screen();
        println!("idle {idle:?}, locked {locked}");
    }
}
