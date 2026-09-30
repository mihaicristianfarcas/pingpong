//! Desktop capture, and the host's sound where the platform captures it
//! beside the picture.
//!
//! - Windows (`dda`, `gpu`): DXGI Desktop Duplication.
//! - macOS (`sck`): ScreenCaptureKit, the picture and the system's sound.
//! - Linux: X11 through MIT-SHM (`x11`), Wayland through the desktop portal
//!   and PipeWire (`pipewire`), and the sound through PulseAudio (`pulse`).
//!
//! On Windows, Desktop Duplication is the capture Sunshine and Apollo use by
//! default. Two properties decided it over Windows Graphics Capture, which
//! pingpong's first versions used:
//!
//! - **It follows the input desktop.** Run as SYSTEM and re-attached with
//!   `OpenInputDesktop` + `SetThreadDesktop` whenever duplication is lost, it
//!   captures the secure desktop -- UAC prompts, the lock screen, Ctrl+Alt+Del.
//!   WGC cannot see any of them, so a UAC prompt left the client staring at a
//!   frozen desktop with input going nowhere.
//! - **Frame timing is ours.** `grab` takes a timeout, so the session paces
//!   capture to the client's frame rate instead of WGC's callback cadence.

#[derive(Debug)]
pub enum CaptureError {
    /// No display with that GDI name is attached to any adapter.
    NoSuchOutput(String),
    /// The desktop is not accessible right now (secure desktop while not
    /// running as SYSTEM, a mode change in flight). Transient: keep grabbing.
    Unavailable(String),
    Platform(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::NoSuchOutput(n) => write!(f, "no output named {n}"),
            CaptureError::Unavailable(m) => write!(f, "desktop unavailable: {m}"),
            CaptureError::Platform(m) => write!(f, "capture error: {m}"),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Outcome of one [`dda::DdaCapture::grab`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grab {
    /// A new desktop image is in the capture texture.
    Frame,
    /// Nothing new was presented within the timeout; the capture texture still
    /// holds the previous image.
    Timeout,
}

#[cfg(windows)]
pub mod dda;
#[cfg(windows)]
pub mod gpu;
#[cfg(target_os = "linux")]
mod image;
#[cfg(target_os = "macos")]
pub mod sck;
#[cfg(target_os = "linux")]
pub use image::{Frame, PixelOrder};
#[cfg(target_os = "linux")]
pub mod pipewire;
#[cfg(target_os = "linux")]
pub mod pulse;
#[cfg(target_os = "linux")]
pub mod x11;
