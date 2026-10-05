//! Stream preferences, Moonlight's settings: `settings.toml` beside the
//! paired hosts. The first launch takes over what the SwiftUI app kept in
//! UserDefaults on a Mac; "Import Settings from Moonlight" reads Moonlight's.

use std::path::{Path, PathBuf};

use ping_core::aspect::fit_standard_ratio;
use ping_core::session::{NativeMode, StreamRequest};
use pingpong_proto::control::{app, codec};
use pingpong_update::{Build, Channel};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// "native", "desktop" (a Mac's scaled desktop) or "WxH".
    pub resolution: String,
    /// 0: the display's maximum.
    pub fps: u32,
    pub auto_bitrate: bool,
    pub bitrate_kbps: u32,
    /// "auto", "av1" (AV1 when the host has it), "hevc" or "h264".
    pub codec: String,
    pub vsync: bool,
    pub frame_pacing: bool,
    pub fullscreen: bool,
    pub show_stats: bool,
    /// Forward Command (a Mac) as the Windows key.
    pub cmd_is_win: bool,
    pub audio: bool,
    pub host_audio: bool,
    /// 2, 6 (5.1) or 8 (7.1).
    pub audio_channels: u8,
    pub mute_in_background: bool,
    pub connection_warnings: bool,
    pub gamepad_mouse: bool,
    /// Moonlight's "swap left and right mouse buttons".
    pub swap_mouse_buttons: bool,
    /// Moonlight's "reverse mouse scrolling direction".
    pub reverse_scroll: bool,
    /// Moonlight's "HDR": asked for when this display shows it.
    pub hdr: bool,
    /// Moonlight's "YUV 4:4:4".
    pub yuv444: bool,
    /// Share the clipboard with the host: copy on one, paste on the other.
    pub share_clipboard: bool,
    /// What the update check follows, once the user has chosen (until
    /// then, what suits the build that runs: see [`Prefs::update_channel`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updates: Option<Channel>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            resolution: "native".into(),
            fps: 0,
            auto_bitrate: true,
            bitrate_kbps: 20_000,
            codec: "auto".into(),
            vsync: true,
            frame_pacing: false,
            fullscreen: true,
            show_stats: false,
            cmd_is_win: false,
            audio: true,
            host_audio: false,
            audio_channels: 2,
            mute_in_background: true,
            connection_warnings: true,
            gamepad_mouse: true,
            swap_mouse_buttons: false,
            reverse_scroll: false,
            hdr: false,
            yuv444: false,
            share_clipboard: true,
            updates: None,
        }
    }
}

/// The resolutions the picker lists by name.
pub const RESOLUTIONS: [(&str, &str); 4] = [
    ("1280x720", "720p"),
    ("1920x1080", "1080p"),
    ("2560x1440", "1440p"),
    ("3840x2160", "4K"),
];

pub const FRAME_RATES: [u32; 5] = [30, 60, 90, 120, 144];

fn path(dir: &Path) -> PathBuf {
    dir.join("settings.toml")
}

impl Prefs {
    /// The saved preferences; on the first launch, the SwiftUI app's.
    pub fn load(dir: &Path) -> Prefs {
        match std::fs::read_to_string(path(dir)) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "settings.toml unreadable; using defaults");
                Prefs::default()
            }),
            Err(_) => {
                let prefs = Prefs::from_swift_app().unwrap_or_default();
                prefs.save(dir);
                prefs
            }
        }
    }

    pub fn save(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(path(dir), text) {
                    tracing::warn!(error = %e, "settings not saved");
                }
            }
            Err(e) => tracing::warn!(error = %e, "settings not saved"),
        }
    }

    /// What the update check follows: the user's choice, else this build's
    /// default. The default is not saved, so a checkout's (main) does not
    /// follow the settings to a packaged release, whose default is releases.
    pub fn update_channel(&self) -> Channel {
        self.updates
            .unwrap_or_else(|| Channel::default_for(&Build::this()))
    }

    pub fn is_listed_resolution(&self) -> bool {
        self.resolution == "native"
            || self.resolution == "desktop"
            || RESOLUTIONS.iter().any(|r| r.0 == self.resolution)
    }

    /// The mode a stream will ask for: always at a standard aspect ratio
    /// (`ping_core::aspect`), a custom size included.
    pub fn mode(&self, native: &NativeMode) -> (u16, u16, u32) {
        let (mut w, mut h) = (native.width, native.height);
        if self.resolution == "desktop" {
            if let Some(d) = native.desktop {
                (w, h) = d;
            }
        } else if let Some((rw, rh)) = parse_size(&self.resolution) {
            (w, h) = fit_standard_ratio(rw, rh);
        }
        (
            w,
            h,
            if self.fps > 0 {
                self.fps
            } else {
                native.max_fps
            },
        )
    }

    pub fn default_bitrate_kbps(&self, native: &NativeMode) -> u32 {
        let (w, h, fps) = self.mode(native);
        ping_core::stream::default_bitrate_kbps(w as u32, h as u32, fps)
    }

    pub fn bitrate(&self, native: &NativeMode) -> u32 {
        if self.auto_bitrate {
            self.default_bitrate_kbps(native)
        } else {
            self.bitrate_kbps.max(500)
        }
    }

    /// What a stream of `app` ("desktop" or "steam") asks for.
    pub fn request(&self, native: &NativeMode, steam: bool) -> StreamRequest {
        let (width, height, fps) = self.mode(native);
        StreamRequest {
            width,
            height,
            fps,
            bitrate_kbps: self.bitrate(native),
            codecs: match self.codec.as_str() {
                "h264" => codec::H264,
                "hevc" => codec::HEVC,
                // The host takes AV1 first, else HEVC, else H.264.
                "av1" => codec::AV1 | codec::HEVC | codec::H264,
                _ => codec::H264 | codec::HEVC,
            },
            vsync: self.vsync,
            frame_pacing: self.frame_pacing,
            fullscreen: self.fullscreen,
            show_stats: self.show_stats,
            cmd_is_win: self.cmd_is_win,
            audio_channels: if self.audio {
                self.audio_channels.clamp(2, 8)
            } else {
                0
            },
            host_audio: self.host_audio,
            mute_in_background: self.mute_in_background,
            connection_warnings: self.connection_warnings,
            gamepad_mouse: self.gamepad_mouse,
            app: if steam {
                app::STEAM_BIG_PICTURE
            } else {
                app::DESKTOP
            },
            keep_host_displays: false,
            wan_only: false,
            via: Vec::new(),
            watch: false,
            clipboard: self.share_clipboard,
            swap_mouse_buttons: self.swap_mouse_buttons,
            reverse_scroll: self.reverse_scroll,
            hdr: self.hdr,
            yuv444: self.yuv444,
        }
    }

    /// What the SwiftUI Ping.app kept in UserDefaults, if it ever ran here.
    fn from_swift_app() -> Option<Prefs> {
        let d = crate::platform::Defaults::open("dev.pingpong.Ping")?;
        // Nothing the user set: nothing to carry over.
        let keys = [
            "resolution",
            "fps",
            "autoBitrate",
            "bitrateKbps",
            "codec",
            "vsync",
            "framePacing",
            "fullscreen",
            "showStats",
            "cmdIsWin",
            "audio",
            "hostAudio",
            "audioChannels",
            "muteInBackground",
            "connectionWarnings",
            "gamepadMouse",
        ];
        if !keys.iter().any(|k| d.has(k)) {
            return None;
        }
        let mut p = Prefs::default();
        if let Some(v) = d.string("resolution") {
            p.resolution = v;
        }
        if let Some(v) = d.int("fps") {
            p.fps = v.max(0) as u32;
        }
        if let Some(v) = d.int("bitrateKbps") {
            p.bitrate_kbps = v.max(500) as u32;
        }
        if let Some(v) = d.string("codec") {
            p.codec = v;
        }
        let flags: [(&str, &mut bool); 11] = [
            ("vsync", &mut p.vsync),
            ("framePacing", &mut p.frame_pacing),
            ("fullscreen", &mut p.fullscreen),
            ("showStats", &mut p.show_stats),
            ("cmdIsWin", &mut p.cmd_is_win),
            ("audio", &mut p.audio),
            ("hostAudio", &mut p.host_audio),
            ("muteInBackground", &mut p.mute_in_background),
            ("connectionWarnings", &mut p.connection_warnings),
            ("gamepadMouse", &mut p.gamepad_mouse),
            ("autoBitrate", &mut p.auto_bitrate),
        ];
        for (key, value) in flags {
            if let Some(v) = d.bool(key) {
                *value = v;
            }
        }
        if let Some(v) = d.int("audioChannels") {
            p.audio_channels = if matches!(v, 2 | 6 | 8) { v as u8 } else { 2 };
        }
        tracing::info!("settings carried over from the SwiftUI app");
        Some(p)
    }

    /// Take over Moonlight's settings from its preferences on this computer,
    /// so a Moonlight setup carries over as it is. Returns what was
    /// imported, or None when Moonlight has never been configured here.
    pub fn import_moonlight(&mut self, native: &NativeMode) -> Option<Vec<String>> {
        let m = crate::platform::Defaults::moonlight()?;
        if !m.has("width") {
            return None;
        }
        let mut done = Vec::new();
        let (w, h) = (m.int("width").unwrap_or(0), m.int("height").unwrap_or(0));
        if w > 0 && h > 0 {
            // Moonlight's native size was taken in its own display mode: on a
            // Mac, the same panel below a menu bar a few pixels taller or
            // shorter.
            if w == native.width as i64 && (h - native.height as i64).abs() <= 16 {
                self.resolution = "native".into();
            } else if native
                .desktop
                .is_some_and(|(dw, dh)| (dw as i64, dh as i64) == (w, h))
            {
                self.resolution = "desktop".into();
            } else {
                self.resolution = format!("{w}x{h}");
            }
            done.push(format!("{w} × {h}"));
        }
        if let Some(f) = m.int("fps").filter(|f| *f > 0) {
            self.fps = if f as u32 == native.max_fps {
                0
            } else {
                f as u32
            };
            done.push(format!("{f} FPS"));
        }
        if let Some(kbps) = m.int("bitrate").filter(|k| *k > 0) {
            self.auto_bitrate = false;
            self.bitrate_kbps = kbps as u32;
            done.push(format!("{} Mbps", kbps / 1000));
        }
        // Moonlight's video codec choice: 0 automatic, 1 H.264, 2 HEVC,
        // 3 HEVC with HDR (an older setting), 4 AV1.
        self.codec = match m.int("videocfg") {
            Some(1) => "h264",
            Some(2) | Some(3) => "hevc",
            Some(4) => "av1",
            _ => "auto",
        }
        .into();
        if m.int("videocfg") == Some(3) {
            self.hdr = true;
        }
        if let Some(v) = m.bool("vsync") {
            self.vsync = v;
        }
        if let Some(v) = m.bool("framepacing") {
            self.frame_pacing = v;
            if v {
                done.push("frame pacing".into());
            }
        }
        // 0 full screen, 1 borderless full screen, 2 windowed.
        if let Some(v) = m.int("windowmode") {
            self.fullscreen = v != 2;
        }
        if let Some(v) = m.bool("showperfoverlay") {
            self.show_stats = v;
        }
        // 0 stereo, 1 5.1, 2 7.1.
        if let Some(v) = m.int("audiocfg") {
            let ch = [2u8, 6, 8][v.clamp(0, 2) as usize];
            self.audio_channels = ch;
            done.push(match ch {
                2 => "stereo".into(),
                6 => "5.1 audio".into(),
                _ => "7.1 audio".into(),
            });
        }
        if let Some(v) = m.bool("hostaudio") {
            self.host_audio = v;
        }
        if let Some(v) = m.bool("muteonfocusloss") {
            self.mute_in_background = v;
        }
        if let Some(v) = m.bool("connwarnings") {
            self.connection_warnings = v;
        }
        if let Some(v) = m.bool("gamepadmouse") {
            self.gamepad_mouse = v;
        }
        if let Some(v) = m.bool("swapmousebuttons") {
            self.swap_mouse_buttons = v;
            if v {
                done.push("swapped mouse buttons".into());
            }
        }
        if let Some(v) = m.bool("hdr") {
            self.hdr = v;
            if v {
                done.push("HDR".into());
            }
        }
        if let Some(v) = m.bool("yuv444") {
            self.yuv444 = v;
            if v {
                done.push("YUV 4:4:4".into());
            }
        }
        if let Some(v) = m.bool("reversescroll") {
            self.reverse_scroll = v;
            if v {
                done.push("reversed scrolling".into());
            }
        }
        Some(done)
    }
}

pub fn parse_size(s: &str) -> Option<(u16, u16)> {
    let (w, h) = s.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac() -> NativeMode {
        NativeMode {
            width: 3024,
            height: 1890,
            max_fps: 120,
            desktop: Some((3600, 2262)),
        }
    }

    #[test]
    fn modes_follow_the_display_unless_told_otherwise() {
        let mut p = Prefs::default();
        assert_eq!(p.mode(&mac()), (3024, 1890, 120));
        p.resolution = "desktop".into();
        assert_eq!(p.mode(&mac()), (3600, 2262, 120));
        p.resolution = "1920x1080".into();
        p.fps = 60;
        assert_eq!(p.mode(&mac()), (1920, 1080, 60));
        assert!(p.is_listed_resolution());
        p.resolution = "2000x1000".into();
        assert!(!p.is_listed_resolution());
    }

    #[test]
    fn a_stream_request_carries_the_settings() {
        let p = Prefs {
            codec: "hevc".into(),
            audio: false,
            auto_bitrate: false,
            bitrate_kbps: 100_000,
            ..Prefs::default()
        };
        let r = p.request(&mac(), true);
        assert_eq!(r.codecs, codec::HEVC);
        assert_eq!(r.audio_channels, 0);
        assert_eq!(r.bitrate_kbps, 100_000);
        assert_eq!(r.app, app::STEAM_BIG_PICTURE);
    }

    #[test]
    fn settings_survive_a_round_trip_and_fill_in_missing_keys() {
        let p = Prefs {
            frame_pacing: true,
            fps: 120,
            ..Prefs::default()
        };
        let text = toml::to_string_pretty(&p).unwrap();
        assert_eq!(toml::from_str::<Prefs>(&text).unwrap(), p);
        let partial: Prefs = toml::from_str("frame_pacing = true").unwrap();
        assert!(partial.frame_pacing && partial.vsync);
    }
}
