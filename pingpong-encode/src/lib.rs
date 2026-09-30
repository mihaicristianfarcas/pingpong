//! Video encoding: on Windows a D3D11 colour converter feeding NVENC,
//! configured the way Apollo configures it (see "The video path" in
//! docs/architecture.md); VideoToolbox on macOS; FFmpeg on Linux.

#[derive(Debug)]
pub enum EncodeError {
    /// The NVENC runtime could not be loaded or rejected a call.
    Nvenc(String),
    /// A D3D11 call failed (shader compile, texture, view).
    D3d(String),
    /// The GPU cannot do what was asked (codec, size, feature).
    Unsupported(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Nvenc(m) => write!(f, "NVENC: {m}"),
            EncodeError::D3d(m) => write!(f, "D3D11: {m}"),
            EncodeError::Unsupported(m) => write!(f, "unsupported: {m}"),
        }
    }
}

impl std::error::Error for EncodeError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    /// NVENC preset P1 (fastest) .. P7 (best). Apollo defaults to P1.
    pub preset: u8,
    /// Quarter-resolution two-pass rate control (Apollo's default).
    pub two_pass: bool,
    /// Slices per frame. More slices decode in parallel and localise damage.
    pub slices: u32,
}

/// What the client must know about an encoded frame to decide whether it can
/// be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Decodable on its own; resets any loss state.
    Idr,
    /// Predicts only from frames the client has.
    P,
    /// The first frame encoded after reference invalidation: it predicts only
    /// from frames older than the loss the client reported, so it ends the
    /// client's wait without an IDR.
    Recovery,
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub kind: FrameKind,
    /// The frame index passed to `encode`.
    pub index: u64,
}

#[cfg(windows)]
pub mod convert;
#[cfg(target_os = "linux")]
pub mod ffmpeg;
#[cfg(windows)]
pub mod nvenc;
#[cfg(windows)]
mod nvenc_sys;
#[cfg(target_os = "macos")]
pub mod videotoolbox;
