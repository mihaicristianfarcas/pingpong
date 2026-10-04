//! VideoToolbox hands HDR (10-bit) and 4:4:4 pictures over in the formats
//! the client's Metal renderer samples plane by plane: `x420`, `x444`,
//! `444v`, with chroma at full size for 4:4:4.
//!
//! The fixtures are two frames of FFmpeg's `testsrc2` at 320x180:
//!
//! ```sh
//! ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -frames:v 2 \
//!     -pix_fmt yuv444p -c:v libx265 -x265-params bframes=0 hevc444-8.hevc
//! ```
//!
//! and the same with `yuv420p10le` (`hevc420-10.hevc`), `yuv444p10le`
//! (`hevc444-10.hevc`), and `-pix_fmt yuv444p -c:v libx264 -bf 0`
//! (`h264-444.h264`).

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidthOfPlane,
};
use pingpong_decode::videotoolbox::{PictureFormat, VtDecoder};
use pingpong_decode::{Codec, VideoDecoder};

/// The first access unit: everything up to the end of the first picture's
/// slice (parameter sets included).
fn first_frame(codec: Codec, data: &[u8]) -> &[u8] {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (n, &s) in starts.iter().enumerate() {
        let slice = match codec {
            Codec::H264 => matches!(data[s] & 0x1F, 1 | 5),
            _ => (data[s] >> 1) & 0x3F < 32,
        };
        if slice {
            let end = starts.get(n + 1).map_or(data.len(), |&next| {
                // Back over the next start code and any zero before it.
                let mut e = next - 3;
                while e > s && data[e - 1] == 0 {
                    e -= 1;
                }
                e
            });
            return &data[..end];
        }
    }
    panic!("no picture in the fixture");
}

/// Decode the fixture's first picture as `format`: its pixel format and its
/// planes' widths.
fn decode(codec: Codec, data: &[u8], format: PictureFormat) -> ([u8; 4], usize, usize) {
    let mut decoder = VtDecoder::new(codec).expect("VtDecoder::new");
    decoder.set_format(format);
    decoder.decode(first_frame(codec, data), 0).expect("decode");
    let deadline = Instant::now() + Duration::from_secs(5);
    let frame = loop {
        if let Some(f) = decoder.poll() {
            break f;
        }
        assert!(Instant::now() < deadline, "no picture within 5 s");
        std::thread::sleep(Duration::from_millis(5));
    };
    // SAFETY: a live CVPixelBuffer, held by `frame` for this scope.
    let image: &CVPixelBuffer = unsafe { &*(frame.pixel_buffer as *const CVPixelBuffer) };
    (
        CVPixelBufferGetPixelFormatType(image).to_be_bytes(),
        CVPixelBufferGetWidthOfPlane(image, 0),
        CVPixelBufferGetWidthOfPlane(image, 1),
    )
}

#[test]
fn eight_bit_444_comes_as_444v_with_full_width_chroma() {
    let format = PictureFormat {
        ten_bit: false,
        yuv444: true,
    };
    let got = decode(
        Codec::Hevc,
        include_bytes!("fixtures/hevc444-8.hevc"),
        format,
    );
    assert_eq!(got, (*b"444v", 320, 320));
    let got = decode(
        Codec::H264,
        include_bytes!("fixtures/h264-444.h264"),
        format,
    );
    assert_eq!(got, (*b"444v", 320, 320));
}

#[test]
fn ten_bit_420_comes_as_x420_with_half_width_chroma() {
    let got = decode(
        Codec::Hevc,
        include_bytes!("fixtures/hevc420-10.hevc"),
        PictureFormat {
            ten_bit: true,
            yuv444: false,
        },
    );
    assert_eq!(got, (*b"x420", 320, 160));
}

#[test]
fn ten_bit_444_comes_as_x444() {
    let got = decode(
        Codec::Hevc,
        include_bytes!("fixtures/hevc444-10.hevc"),
        PictureFormat {
            ten_bit: true,
            yuv444: true,
        },
    );
    assert_eq!(got, (*b"x444", 320, 320));
}
