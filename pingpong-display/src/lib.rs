//! Display control: putting a display into a requested mode and putting it
//! back (v2 design §6).
//!
//! Display control is one of the three things that differ per host platform
//! (with capture and input injection), so it sits behind a trait; everything
//! that uses it is portable.
//!
//! Section references in this crate (`v2 design §6.1`) are to the original
//! design documents in `docs/design/`.

use serde::{Deserialize, Serialize};

pub mod state;

/// Raw SudoVDA ioctl transport.
#[cfg(windows)]
pub mod sudovda;

/// Display policy over `sudovda`: activate, set primary, keepalive, restore.
#[cfg(windows)]
pub mod windows;

/// HDR on a Windows display.
#[cfg(windows)]
pub mod hdr;

/// A virtual display on macOS (CoreGraphics' private CGVirtualDisplay).
#[cfg(target_os = "macos")]
pub mod macos;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayMode {
    pub width: u16,
    pub height: u16,
    /// Millihertz. See v2 design §4.4.
    pub refresh_mhz: u32,
}

/// Which display the pipeline should capture, and what it is set to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveDisplay {
    /// GDI device name (`\\.\DISPLAYn`): what DXGI calls the output, so
    /// Desktop Duplication captures exactly this display.
    pub gdi_name: String,
    pub mode: DisplayMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayError {
    NoSuchDisplay,
    ModeRejected,
    VddUnavailable,
    Os(String),
}

impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayError::NoSuchDisplay => write!(f, "no such display"),
            DisplayError::ModeRejected => write!(f, "the display rejected the requested mode"),
            DisplayError::VddUnavailable => write!(f, "no virtual display driver available"),
            DisplayError::Os(e) => write!(f, "os error: {e}"),
        }
    }
}

impl std::error::Error for DisplayError {}

pub trait DisplayControl {
    /// Put a display into `mode` and return which one to capture.
    ///
    /// MUST be idempotent: activating a mode already active is a no-op that
    /// still returns the active display (v2 design §4.4 -- a duplicated
    /// SessionStart must cost nothing).
    fn activate(&mut self, mode: DisplayMode) -> Result<ActiveDisplay, DisplayError>;

    /// Put the display back the way it was found.
    fn restore(&mut self) -> Result<(), DisplayError>;
}
