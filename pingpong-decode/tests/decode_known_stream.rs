//! Decode a real H.264 elementary stream end to end.
//!
//! The fixture is the stream the VideoToolbox latency spike measured against
//! (`spikes/vt-latency`): 600 frames of 1920x1080, no B-frames, one IDR then
//! 599 P-frames -- the same shape the host's encoders produce, so what passes
//! here is what the client will be fed.
//!
//! Regenerate it with the ffmpeg command in `spikes/vt-latency/README.md`.

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
};
use pingpong_decode::videotoolbox::VtDecoder;
use pingpong_decode::VideoDecoder;

const STREAM: &[u8] = include_bytes!("fixtures/testsrc.h264");

/// Offsets of every start code, as `(code_start, payload_start)`.
fn nal_spans(data: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                spans.push((i, i + 4));
                i += 4;
                continue;
            }
            if data[i + 2] == 1 {
                spans.push((i, i + 3));
                i += 3;
                continue;
            }
        }
        i += 1;
    }
    spans
}

/// Group NALs into access units: everything up to and including the next VCL
/// slice is one frame.
///
/// This lives in the test rather than the crate because on the wire an access
/// unit boundary is explicit -- the client is handed one reassembled frame at a
/// time by `pingpong-proto` and never has to infer it. Only a file needs this.
fn split_access_units(data: &[u8]) -> Vec<&[u8]> {
    let spans = nal_spans(data);
    let mut aus = Vec::new();
    let mut au_start: Option<usize> = None;

    for (n, &(code_start, payload_start)) in spans.iter().enumerate() {
        let nal_end = spans.get(n + 1).map_or(data.len(), |&(cs, _)| cs);
        if payload_start >= nal_end {
            continue; // start code with no payload behind it
        }
        let start = *au_start.get_or_insert(code_start);
        let nal_type = data[payload_start] & 0x1F;
        if matches!(nal_type, 1 | 5) {
            aus.push(&data[start..nal_end]);
            au_start = None;
        }
    }
    aus
}

#[test]
fn decodes_every_frame_of_a_known_stream() {
    // 600 is what `ffprobe -count_frames` reports for this fixture. Pinning the
    // exact number means a splitter that quietly swallows frames fails here
    // instead of passing on a shorter run.
    let access_units = split_access_units(STREAM);
    assert_eq!(
        access_units.len(),
        600,
        "parsed {} access units from {} bytes; ffprobe counts 600 frames",
        access_units.len(),
        STREAM.len()
    );

    let mut decoder = VtDecoder::new(pingpong_decode::Codec::H264).expect("VtDecoder::new");
    let mut decoded = 0usize;

    for (i, au) in access_units.iter().enumerate() {
        decoder
            .decode(au, i as u32)
            .unwrap_or_else(|e| panic!("decode of access unit {i} failed: {e}"));

        // Wait for this frame's callback before submitting the next. That is
        // both what keeps the run inside the ready queue's bound of 2, and an
        // assertion in its own right: the spike measured one callback per submit,
        // and if that ever stops holding this test hangs to the deadline rather
        // than silently passing on a decoder that has started buffering.
        let deadline = Instant::now() + Duration::from_secs(5);
        let frame = loop {
            if let Some(frame) = decoder.poll() {
                break frame;
            }
            assert!(
                Instant::now() < deadline,
                "access unit {i} produced no frame within 5 s"
            );
            std::thread::yield_now();
        };

        assert!(
            !frame.pixel_buffer.is_null(),
            "access unit {i} decoded to a null pixel buffer"
        );
        assert_eq!(
            (frame.width, frame.height),
            (1920, 1080),
            "access unit {i} decoded at the wrong size"
        );
        assert_eq!(
            frame.capture_ts_us, i as u32,
            "capture timestamp was not carried through access unit {i}"
        );

        // The client's zero-copy present binds one Metal texture per plane, so a
        // single-plane or three-plane output would break it. Checking here means
        // that assumption fails in this crate's tests rather than as a black
        // screen in the client.
        // SAFETY: the frame owns a live retained CVPixelBuffer for as long as it
        // is alive, which is the whole of this block.
        let pixel_buffer = unsafe { &*(frame.pixel_buffer as *const CVPixelBuffer) };
        let planes = CVPixelBufferGetPlaneCount(pixel_buffer);
        let format = CVPixelBufferGetPixelFormatType(pixel_buffer);
        assert_eq!(
            planes,
            2,
            "expected a bi-planar (NV12-family) buffer, got {planes} plane(s), FourCC {:?}",
            String::from_utf8_lossy(&format.to_be_bytes())
        );

        decoded += 1;
    }

    assert_eq!(
        decoded,
        access_units.len(),
        "not every access unit produced a frame"
    );
    assert_eq!(
        decoder.failed_and_dropped(),
        (0, 0),
        "decoder reported failed or dropped frames"
    );
    assert_eq!(
        decoder.skipped_before_parameter_sets(),
        0,
        "the fixture's first access unit should carry SPS and PPS"
    );
}
