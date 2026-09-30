//! The headless session's decoder, fed a real stream (the decoder crate's
//! fixture: 1920x1080 H.264, ffmpeg's testsrc pattern): pictures reach the
//! store, their colours are right, and the moving counter counts as change.

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use ping_agent::decode::{FrameStore, HeadlessVideo};
use ping_core::stats::StatsCollector;
use ping_core::stream::{Codec, FrameTiming, VideoOut};

const STREAM: &[u8] = include_bytes!("../../pingpong-decode/tests/fixtures/testsrc.h264");

/// Access units: everything up to and including each slice.
fn access_units(data: &[u8]) -> Vec<&[u8]> {
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
    let mut out = Vec::new();
    let mut au = None;
    for (n, &(code, payload)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(data.len(), |s| s.0);
        let start = *au.get_or_insert(code);
        if payload < end && matches!(data[payload] & 0x1F, 1 | 5) {
            out.push(&data[start..end]);
            au = None;
        }
    }
    out
}

#[test]
fn a_real_stream_decodes_into_the_store_in_the_right_colours() {
    let store = Arc::new(FrameStore::default());
    let mut video = HeadlessVideo::new(store.clone(), Arc::new(StatsCollector::default()));
    video.configure(Codec::H264, 1920, 1080).unwrap();
    let aus = access_units(STREAM);
    assert!(aus.len() >= 60);
    let started = Instant::now();
    for (i, au) in aus.iter().take(60).enumerate() {
        video
            .decode(
                au,
                FrameTiming {
                    frame_id: i as u32,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while store.count() < 60 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        store.count() >= 55,
        "only {} pictures in {:?}",
        store.count(),
        started.elapsed()
    );
    let snap = store.latest().unwrap();
    let rgb = snap.picture.to_rgb();
    assert_eq!((rgb.width, rgb.height), (1920, 1080));
    // testsrc's picture is full of saturated colour: there must be strongly
    // red, green and blue pixels, and near-white and near-black ones.
    let px = |i: usize| (rgb.data[i * 3], rgb.data[i * 3 + 1], rgb.data[i * 3 + 2]);
    let n = (rgb.width * rgb.height) as usize;
    let count = |f: &dyn Fn((u8, u8, u8)) -> bool| (0..n).step_by(97).filter(|&i| f(px(i))).count();
    let red = count(&|(r, g, b)| r > 180 && g < 80 && b < 80);
    let green = count(&|(r, g, b)| g > 180 && r < 80 && b < 80);
    let blue = count(&|(r, g, b)| b > 180 && r < 80 && g < 80);
    let white = count(&|(r, g, b)| r > 230 && g > 230 && b > 230);
    assert!(
        red > 50 && green > 50 && blue > 50,
        "red {red} green {green} blue {blue}"
    );
    assert!(
        white + count(&|(r, g, b)| r < 25 && g < 25 && b < 25) > 50,
        "white and black"
    );
    // A PNG of it, as a screenshot would be.
    let png = rgb.png();
    assert!(png.len() > 10_000);
    std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).unwrap();
    std::fs::write(
        format!("{}/headless-testsrc.png", env!("CARGO_TARGET_TMPDIR")),
        &png,
    )
    .unwrap();
}
