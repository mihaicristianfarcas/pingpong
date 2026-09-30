//! Hardware video decoding behind a platform-neutral trait (v1 design §9;
//! the design documents are in `docs/design/`).
//!
//! VideoToolbox on macOS; on Windows, FFmpeg with D3D11VA (`d3d11va`), which
//! hands out GPU textures rather than going through the trait; on Linux,
//! FFmpeg with VA-API or in software (`ffmpeg`), pictures in system memory.

use std::ffi::c_void;

#[derive(Debug)]
pub enum DecodeError {
    /// A VideoToolbox or CoreMedia call returned a non-zero `OSStatus`.
    ///
    /// `call` is the C function name, which is what Apple's error tables and
    /// every search result are keyed by -- much more use than a wrapped
    /// message.
    Os { call: &'static str, status: i32 },
    /// The bitstream is not something a decoder can be started from.
    Bitstream(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Os { call, status } => {
                write!(f, "{call} failed with OSStatus {status}")
            }
            DecodeError::Bitstream(m) => write!(f, "bad bitstream: {m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// One decoded frame, as a `CVPixelBuffer` the GPU can sample directly.
///
/// The pointer is a **retained** `CVPixelBuffer`; dropping this struct releases
/// it. See the deviation note on the `pixel_buffer` field.
/// Video codecs the client can decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

pub struct DecodedFrame {
    /// A retained `CVPixelBuffer`, in whatever pixel format the decoder chose
    /// (NV12 for the streams this project produces).
    ///
    /// Released by [`DecodedFrame`]'s `Drop`, not by the consumer. The
    /// consumer still owns the frame and controls when it goes away -- it just
    /// cannot forget, and forgetting here leaks an IOSurface-backed 1080p NV12
    /// buffer sixty times a second. Borrow the pointer for as long as the
    /// `DecodedFrame` lives; do not release it yourself.
    pub pixel_buffer: *mut c_void,
    /// Carried through from the [`CapturedFrame`][cf] this started life as, on
    /// the *host's* clock. Only ever compare it to other host timestamps.
    ///
    /// [cf]: https://docs.rs/pingpong-capture
    pub capture_ts_us: u32,
    /// When the decode callback fired, on this process's shared clock
    /// ([`pingpong_proto::clock`]). Compare it to other client-side timestamps,
    /// never to `capture_ts_us` -- those two clocks are on different machines.
    pub decoded_at_us: u32,
    /// Convenience copies of the pixel buffer's dimensions, so a consumer sizing
    /// a texture does not have to reach back into CoreVideo for them.
    pub width: u32,
    pub height: u32,
}

// SAFETY: a CVPixelBuffer is reference counted and may be used from any thread;
// nothing in `DecodedFrame` is tied to the thread that created it. This is
// needed because the decode callback fires on VideoToolbox's thread and the
// frame is handed to the caller's thread by `poll`.
unsafe impl Send for DecodedFrame {}

pub trait VideoDecoder {
    /// Submit one access unit, in Annex-B byte-stream form.
    ///
    /// Decoding is asynchronous: this returns as soon as the frame is handed to
    /// the decoder. Finished frames come back from [`VideoDecoder::poll`].
    ///
    /// `capture_ts_us` is carried through untouched to the resulting
    /// [`DecodedFrame`].
    fn decode(&mut self, annexb: &[u8], capture_ts_us: u32) -> Result<(), DecodeError>;

    /// Take the oldest finished frame, if any. Never blocks.
    fn poll(&mut self) -> Option<DecodedFrame>;
}

pub mod annexb;

#[cfg(target_os = "macos")]
pub mod videotoolbox;

#[cfg(windows)]
pub mod d3d11va;

/// Linux's decoder; on Windows, the software path for pictures in memory
/// (the headless agent session), beside D3D11VA for the screen.
#[cfg(any(target_os = "linux", windows))]
pub mod ffmpeg;
