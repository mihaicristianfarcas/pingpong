//! What a session will be, from what the client asked for and what the host
//! allows. Shared by the Windows and macOS session managers.

use pingpong_encode::Codec;
use pingpong_proto::control::{video, AckStatus, SessionStart};

use crate::config::HostConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    pub codec: Codec,
    /// Frames a second, in millihertz: the client's display's own rate,
    /// 59.94 included (Moonlight sends it as `clientRefreshRateX100`, and
    /// Sunshine captures at it).
    pub fps_mhz: u32,
    pub bitrate_kbps: u32,
    /// `control::video::*`: HDR and 4:4:4, where the client asked, the
    /// host's encoder can, and the settings allow.
    pub video: u8,
    /// Even, as encoders want.
    pub width: u16,
    pub height: u16,
}

/// The session for `req`, or why there is none. `caps`: what the host can
/// encode of these `control::video` bits with a codec, together.
pub fn negotiate(
    req: &SessionStart,
    cfg: &HostConfig,
    supported: &[Codec],
    caps: impl Fn(Codec, u8) -> u8,
) -> Result<Negotiated, AckStatus> {
    let codec = codec_for(req.codecs, supported, cfg.allow_hevc, cfg.allow_av1)
        .ok_or(AckStatus::NoCodec)?;
    let mut fps_mhz = requested_mhz(req).clamp(10_000, 240_000);
    if cfg.max_fps > 0 {
        fps_mhz = fps_mhz.min(cfg.max_fps.saturating_mul(1000));
    }
    let mut bitrate_kbps = req.bitrate_kbps.max(1000);
    if cfg.max_bitrate_kbps > 0 {
        bitrate_kbps = bitrate_kbps.min(cfg.max_bitrate_kbps);
    }
    let mut allowed = 0;
    if cfg.allow_hdr {
        allowed |= video::HDR;
    }
    if cfg.allow_yuv444 {
        allowed |= video::YUV444;
    }
    Ok(Negotiated {
        video: caps(codec, req.video & allowed),
        codec,
        fps_mhz,
        bitrate_kbps,
        width: req.width & !1,
        height: req.height & !1,
    })
}

/// The rate the client asked for: its display's own when it sent one within
/// 1% of the whole rate (the client's own rule; anything else is not taken
/// from the network), else the whole rate.
fn requested_mhz(req: &SessionStart) -> u32 {
    let (whole, exact) = (req.refresh_mhz, req.exact_refresh_mhz);
    if exact != 0 && exact.abs_diff(whole) as u64 * 100 <= whole as u64 {
        exact
    } else {
        whole
    }
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
            repeat_delay_ms: 0,
            repeat_interval_ms: 0,
            video: 0,
            exact_refresh_mhz: 0,
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
            |_, _| 0,
        )
        .unwrap();
        assert_eq!(
            (n.codec, n.fps_mhz, n.bitrate_kbps, n.width, n.height),
            (Codec::Hevc, 60_000, 50_000, 3024, 1890)
        );
    }

    #[test]
    fn a_fractional_rate_is_kept_to_the_millihertz() {
        let mut req = request(codec::HEVC, 1920, 1080, 60, 20_000);
        req.exact_refresh_mhz = 59_940;
        let n = negotiate(&req, &HostConfig::default(), &[Codec::Hevc], |_, _| 0).unwrap();
        assert_eq!(n.fps_mhz, 59_940);
    }

    #[test]
    fn an_exact_rate_far_from_the_whole_one_is_not_taken() {
        let mut req = request(codec::HEVC, 1920, 1080, 60, 20_000);
        req.exact_refresh_mhz = 143_972;
        let n = negotiate(&req, &HostConfig::default(), &[Codec::Hevc], |_, _| 0).unwrap();
        assert_eq!(n.fps_mhz, 60_000);
    }

    #[test]
    fn an_older_clients_whole_rate_is_the_rate() {
        let req = request(codec::HEVC, 1920, 1080, 144, 20_000);
        let n = negotiate(&req, &HostConfig::default(), &[Codec::Hevc], |_, _| 0).unwrap();
        assert_eq!(n.fps_mhz, 144_000);
    }

    #[test]
    fn hdr_and_444_need_the_client_the_encoder_and_the_settings() {
        use pingpong_proto::control::video::{HDR, YUV444};
        let mut req = request(codec::HEVC, 1920, 1080, 60, 20_000);
        req.video = HDR | YUV444;
        let caps = |c: Codec, asked: u8| if c == Codec::Hevc { asked & HDR } else { 0 };
        let cfg = HostConfig::default();
        assert_eq!(
            negotiate(&req, &cfg, &[Codec::Hevc], caps).unwrap().video,
            HDR
        );
        let off = HostConfig {
            allow_hdr: false,
            ..HostConfig::default()
        };
        assert_eq!(
            negotiate(&req, &off, &[Codec::Hevc], caps).unwrap().video,
            0
        );
        req.video = YUV444;
        assert_eq!(
            negotiate(&req, &cfg, &[Codec::Hevc], |_, asked| asked)
                .unwrap()
                .video,
            YUV444
        );
    }

    #[test]
    fn nonsense_is_bounded() {
        let cfg = HostConfig::default();
        let n = negotiate(
            &request(codec::H264, 1280, 720, 1, 10),
            &cfg,
            &[Codec::H264],
            |_, _| 0,
        )
        .unwrap();
        assert_eq!((n.fps_mhz, n.bitrate_kbps), (10_000, 1000));
        let n = negotiate(
            &request(codec::H264, 1280, 720, 1000, 10),
            &cfg,
            &[Codec::H264],
            |_, _| 0,
        )
        .unwrap();
        assert_eq!(n.fps_mhz, 240_000);
        assert_eq!(
            negotiate(
                &request(codec::AV1, 1280, 720, 60, 10),
                &cfg,
                &[Codec::H264],
                |_, _| 0
            ),
            Err(AckStatus::NoCodec)
        );
    }
}
