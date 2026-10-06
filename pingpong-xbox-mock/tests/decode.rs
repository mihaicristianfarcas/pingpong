//! The mock console's H.264 is a stream the client's own decoders take,
//! and it decodes to exactly the picture the mock drew (its macroblocks
//! are uncompressed): VideoToolbox on macOS, FFmpeg in software on Linux
//! and Windows. So what a client shows of the mock is the client's doing.

use pingpong_xbox::input::xbutton;
use pingpong_xbox_mock::{test_picture, Encoder, InputView, Picture, HEIGHT, WIDTH};

/// Forty frames: a keyframe, motion, a button lit half way, a keyframe on
/// request.
fn frames() -> Vec<(Picture, Vec<u8>, bool)> {
    let mut enc = Encoder::new(WIDTH, HEIGHT);
    (0..40u64)
        .map(|i| {
            let view = InputView {
                buttons: if i >= 20 { xbutton::A } else { 0 },
                ..Default::default()
            };
            if i == 30 {
                enc.request_keyframe();
            }
            let pic = test_picture(i, &view);
            let (au, key) = enc.encode(&pic);
            assert_eq!(key, i == 0 || i == 30);
            (pic, au, key)
        })
        .collect()
}

/// The visible luma of `pic`, row by row.
fn luma(pic: &Picture) -> Vec<&[u8]> {
    (0..HEIGHT)
        .map(|r| &pic.y[r * pic.width..r * pic.width + WIDTH])
        .collect()
}

#[cfg(target_os = "macos")]
#[test]
fn the_mocks_stream_decodes_exactly_on_videotoolbox() {
    use std::time::{Duration, Instant};

    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    use pingpong_decode::videotoolbox::VtDecoder;
    use pingpong_decode::VideoDecoder;

    let mut decoder = VtDecoder::new(pingpong_decode::Codec::H264).unwrap();
    for (i, (pic, au, _)) in frames().iter().enumerate() {
        decoder.decode(au, i as u32).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let frame = loop {
            if let Some(f) = decoder.poll() {
                break f;
            }
            assert!(Instant::now() < deadline, "frame {i} did not decode");
            std::thread::yield_now();
        };
        assert_eq!((frame.width, frame.height), (WIDTH as u32, HEIGHT as u32));
        // SAFETY: the frame holds a retained pixel buffer for this block;
        // its base address is read only between lock and unlock.
        unsafe {
            let pb = &*(frame.pixel_buffer as *const CVPixelBuffer);
            CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
            let base = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *const u8;
            let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
            for (r, want) in luma(pic).iter().enumerate() {
                let got = std::slice::from_raw_parts(base.add(r * stride), WIDTH);
                assert_eq!(got, *want, "frame {i}, row {r}");
            }
            CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
#[test]
fn the_mocks_stream_decodes_exactly_on_ffmpeg() {
    use pingpong_decode::ffmpeg::{FfmpegDecoder, Layout};

    let mut decoder = FfmpegDecoder::new(pingpong_decode::Codec::H264, true).unwrap();
    let mut decoded = 0;
    for (i, (pic, au, _)) in frames().iter().enumerate() {
        decoder
            .decode(au, i as u32, |p| {
                assert_eq!((p.width, p.height), (WIDTH as u32, HEIGHT as u32));
                let y = match &p.layout {
                    Layout::Nv12 { y, .. } | Layout::I420 { y, .. } | Layout::I444 { y, .. } => y,
                };
                for (r, want) in luma(pic).iter().enumerate() {
                    assert_eq!(
                        &y.data[r * y.stride..r * y.stride + WIDTH],
                        *want,
                        "frame {i}, row {r}"
                    );
                }
                decoded += 1;
            })
            .unwrap();
    }
    assert_eq!(decoded, 40, "every frame came out at once (no reordering)");
}
