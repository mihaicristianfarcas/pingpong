//! What a session will be, from what the client asked for and what the host
//! allows. Shared by the Windows and macOS session managers.

use pingpong_encode::Codec;
use pingpong_proto::control::{AckStatus, SessionStart};

use crate::config::HostConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    pub codec: Codec,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Even, as encoders want.
    pub width: u16,
    pub height: u16,
}

/// The session for `req`, or why there is none.
pub fn negotiate(
    req: &SessionStart,
    cfg: &HostConfig,
    supported: &[Codec],
) -> Result<Negotiated, AckStatus> {
    let codec = codec_for(req.codecs, supported, cfg.allow_hevc, cfg.allow_av1)
        .ok_or(AckStatus::NoCodec)?;
    let mut fps = (req.refresh_mhz / 1000).clamp(10, 240);
    if cfg.max_fps > 0 {
        fps = fps.min(cfg.max_fps);
    }
    let mut bitrate_kbps = req.bitrate_kbps.max(1000);
    if cfg.max_bitrate_kbps > 0 {
        bitrate_kbps = bitrate_kbps.min(cfg.max_bitrate_kbps);
    }
    Ok(Negotiated {
        codec,
        fps,
        bitrate_kbps,
        width: req.width & !1,
        height: req.height & !1,
    })
}

/// The best codec both sides have: AV1, then HEVC, then H.264.
pub fn codec_for(
    requested: u8,
    supported: &[Codec],
    allow_hevc: bool,
    allow_av1: bool,
) -> Option<Codec> {
    use pingpong_proto::control::codec;
    if requested & codec::AV1 != 0 && allow_av1 && supported.contains(&Codec::Av1) {
        return Some(Codec::Av1);
    }
    if requested & codec::HEVC != 0 && allow_hevc && supported.contains(&Codec::Hevc) {
        return Some(Codec::Hevc);
    }
    if requested & codec::H264 != 0 && supported.contains(&Codec::H264) {
        return Some(Codec::H264);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_proto::control::{app, codec};

    fn request(codecs: u8, w: u16, h: u16, hz: u32, kbps: u32) -> SessionStart {
        SessionStart {
            width: w,
            height: h,
            refresh_mhz: hz * 1000,
            bitrate_kbps: kbps,
            codecs,
            audio_channels: 2,
            flags: 0,
            slices: 1,
            nonce: 1,
            app: app::DESKTOP,
        }
    }

    #[test]
    fn the_best_shared_codec_wins() {
        let all = [Codec::H264, Codec::Hevc, Codec::Av1];
        assert_eq!(
            codec_for(codec::H264 | codec::HEVC | codec::AV1, &all, true, true),
            Some(Codec::Av1)
        );
        assert_eq!(
            codec_for(codec::H264 | codec::HEVC | codec::AV1, &all, true, false),
            Some(Codec::Hevc)
        );
        assert_eq!(
            codec_for(codec::H264 | codec::HEVC, &[Codec::H264], true, true),
            Some(Codec::H264)
        );
        assert_eq!(codec_for(codec::HEVC, &[Codec::H264], true, true), None);
    }

    #[test]
    fn the_host_caps_rate_and_bitrate_and_evens_the_size() {
        let cfg = HostConfig {
            max_fps: 60,
            max_bitrate_kbps: 50_000,
            ..HostConfig::default()
        };
        let n = negotiate(
            &request(codec::HEVC, 3025, 1891, 120, 100_000),
            &cfg,
            &[Codec::Hevc],
        )
        .unwrap();
        assert_eq!(
            (n.codec, n.fps, n.bitrate_kbps, n.width, n.height),
            (Codec::Hevc, 60, 50_000, 3024, 1890)
        );
    }

    #[test]
    fn nonsense_is_bounded() {
        let cfg = HostConfig::default();
        let n = negotiate(
            &request(codec::H264, 1280, 720, 1, 10),
            &cfg,
            &[Codec::H264],
        )
        .unwrap();
        assert_eq!((n.fps, n.bitrate_kbps), (10, 1000));
        let n = negotiate(
            &request(codec::H264, 1280, 720, 1000, 10),
            &cfg,
            &[Codec::H264],
        )
        .unwrap();
        assert_eq!(n.fps, 240);
        assert_eq!(
            negotiate(
                &request(codec::AV1, 1280, 720, 60, 10),
                &cfg,
                &[Codec::H264]
            ),
            Err(AckStatus::NoCodec)
        );
    }
}
