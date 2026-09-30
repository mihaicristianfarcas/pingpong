//! Decode the known H.264 stream end to end with FFmpeg, as the Linux client
//! does (in software here: VA-API needs a GPU, and a test must not).
//! See decode_known_stream.rs for the fixture.

#![cfg(target_os = "linux")]

use pingpong_decode::ffmpeg::{FfmpegDecoder, Layout};

const STREAM: &[u8] = include_bytes!("fixtures/testsrc.h264");

/// Access units: everything up to and including the next slice (a file has
/// no frame boundaries; the wire does).
fn split_access_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0
            && data[i + 1] == 0
            && (data[i + 2] == 1 || (i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1))
        {
            let payload = if data[i + 2] == 1 { i + 3 } else { i + 4 };
            starts.push((i, payload));
            i = payload;
        } else {
            i += 1;
        }
    }
    let mut aus = Vec::new();
    let mut au_start = None;
    for (n, &(code, payload)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(data.len(), |&(c, _)| c);
        if payload >= end {
            continue;
        }
        let start = *au_start.get_or_insert(code);
        if matches!(data[payload] & 0x1F, 1 | 5) {
            aus.push(&data[start..end]);
            au_start = None;
        }
    }
    aus
}

#[test]
fn decodes_every_frame_of_a_known_stream() {
    let aus = split_access_units(STREAM);
    assert_eq!(aus.len(), 600);
    let mut decoder = FfmpegDecoder::new(pingpong_decode::Codec::H264, true).expect("decoder");
    let mut pictures = 0;
    let mut last_tag = None;
    for (i, au) in aus.iter().enumerate() {
        decoder
            .decode(au, i as u32, |pic| {
                assert_eq!((pic.width, pic.height), (1920, 1080));
                // The fixture is 4:4:4 (the host's streams are 4:2:0).
                match pic.layout {
                    Layout::I444 { y, u, v } => {
                        assert!(y.stride >= 1920 && u.stride >= 1920 && v.stride >= 1920);
                        assert!(y.data.len() >= y.stride * 1080 && v.data.len() >= v.stride * 1080);
                    }
                    _ => panic!("software decoding of this fixture gives planar 4:4:4"),
                }
                last_tag = Some(pic.tag);
                pictures += 1;
            })
            .unwrap_or_else(|e| panic!("access unit {i}: {e}"));
    }
    // Low delay: a picture out for every frame in, in order, at once.
    assert_eq!(pictures, 600);
    assert_eq!(last_tag, Some(599));
}
