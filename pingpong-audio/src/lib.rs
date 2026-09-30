//! Audio for pingpong, as Sunshine and Moonlight do it: the host captures what
//! it plays (WASAPI loopback), encodes 5 ms Opus packets, and sends each at
//! once with Reed-Solomon parity per block of four (wire format in
//! `pingpong_proto::audio`); the client reorders, recovers or conceals losses,
//! and plays through a small, self-trimming buffer.

pub mod opus;
pub mod player;

#[cfg(target_os = "macos")]
pub mod coreaudio;
#[cfg(target_os = "linux")]
pub mod cpal_out;
#[cfg(windows)]
pub mod wasapi;

/// Opus bitrate for `channels` (2, 6 or 8), following Moonlight: high
/// quality when the video bitrate leaves room for it.
pub fn bitrate_bps(channels: u8, video_kbps: u32) -> u32 {
    let hq = video_kbps >= 15_000;
    let kbps = match (channels, hq) {
        (6, false) => opus::SURROUND51_KBPS,
        (6, true) => opus::SURROUND51_HQ_KBPS,
        (8, false) => opus::SURROUND71_KBPS,
        (8, true) => opus::SURROUND71_HQ_KBPS,
        (_, false) => opus::STEREO_KBPS,
        (_, true) => opus::STEREO_HQ_KBPS,
    };
    kbps * 1000
}

/// Opus bitrate for a stereo stream.
pub fn stereo_bitrate_bps(video_kbps: u32) -> u32 {
    bitrate_bps(2, video_kbps)
}
