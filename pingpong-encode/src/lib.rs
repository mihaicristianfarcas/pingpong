//! Video encoding: on Windows a D3D11 colour converter feeding NVENC,
//! configured the way Sunshine configures it (see "The video path" in
//! docs/architecture.md), or Media Foundation's H.264 encoder where there is
//! no NVENC; VideoToolbox on macOS; FFmpeg on Linux.

#[derive(Debug)]
pub enum EncodeError {
    /// The NVENC runtime could not be loaded or rejected a call.
    Nvenc(String),
    /// A D3D11 call failed (shader compile, texture, view).
    D3d(String),
    /// Media Foundation's encoder could not be made or rejected a call.
    MediaFoundation(String),
    /// The GPU cannot do what was asked (codec, size, feature).
    Unsupported(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EncodeError::Nvenc(m) => write!(f, "NVENC: {m}"),
            EncodeError::D3d(m) => write!(f, "D3D11: {m}"),
            EncodeError::MediaFoundation(m) => write!(f, "Media Foundation: {m}"),
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
    /// Frames a second, in millihertz: the client's display may refresh at
    /// 59.94 Hz, and a stream at 60 would show a frame twice every 17 s.
    pub fps_mhz: u32,
    pub bitrate_bps: u32,
    /// NVENC preset P1 (fastest) .. P7 (best). Sunshine defaults to P1.
    pub preset: u8,
    /// Quarter-resolution two-pass rate control (Sunshine's default).
    pub two_pass: bool,
    /// Slices per frame. More slices decode in parallel and localise damage.
    pub slices: u32,
    /// HDR10: 10-bit, BT.2020 primaries, the PQ curve (the capture is
    /// already so, or is converted to it).
    pub hdr: bool,
    /// Chroma at full resolution (4:4:4).
    pub yuv444: bool,
}

impl EncoderConfig {
    /// Whole frames a second, rounded.
    pub fn fps(&self) -> u32 {
        (self.fps_mhz.saturating_add(500) / 1000).max(1)
    }

    /// One frame's share of `bitrate_bps`, in bits: the single-frame VBV.
    pub fn frame_bits(&self, bitrate_bps: u32) -> u32 {
        (bitrate_bps as u64 * 1000 / self.fps_mhz.max(1) as u64).min(u32::MAX as u64) as u32
    }
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
pub mod h264;
#[cfg(windows)]
pub mod mf;
#[cfg(windows)]
pub mod nvenc;
#[cfg(windows)]
mod nvenc_sys;
#[cfg(target_os = "macos")]
pub mod videotoolbox;
