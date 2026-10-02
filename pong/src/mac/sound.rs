//! A Mac host's sound for a session: in surround when the client asks for
//! 5.1 or 7.1 and the Mac has an output with the speakers for it, else the
//! system's stereo mix from ScreenCaptureKit (what every Mac can give).
//!
//! Surround is a Core Audio tap (`pingpong_audio::tap`) of a multichannel
//! output, its channels put in the stream's order and at 48 kHz on the way
//! (`pingpong_audio::channels`). The output is the default one when it has
//! surround speakers (a receiver on HDMI); otherwise, unless the client
//! wants the sound on the host too, a loopback device with enough channels
//! (BlackHole 16ch, as Sunshine uses on a Mac) made the default output and
//! set to the session's speakers for its length, as the Windows host does
//! with Steam Streaming Speakers. The tap mutes what it hears on a real
//! device, so the host stays quiet as a Windows host does, unless the
//! client asks for the sound there too.

use std::sync::Arc;

use pingpong_audio::channels::{self, Resampler};
use pingpong_audio::tap::{self, OutputDevice, Route, Tap};
use pingpong_proto::control::{self, SessionStart};
use pingpong_transport::{Endpoint, Peer};

use crate::audio::{AudioHandle, Chunks};
use crate::permissions::{self, Consent};

/// The session's sound, and how many channels it has.
pub fn start(
    display: u32,
    req: &SessionStart,
    bitrate_kbps: u32,
    endpoint: Arc<Endpoint>,
    peer: Arc<Peer>,
) -> Option<(AudioHandle, u8)> {
    let wanted = match req.audio_channels {
        0 => return None,
        1..=2 => 2,
        3..=6 => 6,
        _ => 8,
    };
    let host_audio = req.flags & control::flags::HOST_AUDIO != 0;
    if wanted > 2 {
        match surround(wanted, host_audio, bitrate_kbps, &endpoint, &peer) {
            Ok(Some(a)) => return Some(a),
            Ok(None) => {}
            Err(e) => tracing::warn!(error = e, "no sound in surround; streaming stereo"),
        }
    }
    stereo(display, bitrate_kbps, endpoint, peer)
}

/// The output to tap for `wanted` channels, and the route that sends the
/// system's sound there (none when it is the default output already).
fn output_for(wanted: u8, host_audio: bool) -> Option<(OutputDevice, Option<Route>)> {
    let default = tap::default_output();
    if let Some(d) = default.as_ref().filter(|d| !d.is_virtual) {
        let labels = d.labels();
        if channels::speakers_set(&labels) && channels::surround_channels(&labels) > 2 {
            return Some((d.clone(), None));
        }
    }
    if host_audio {
        // The sound is to be heard here: the default output stays.
        return None;
    }
    // A loopback device: the default one if it is, else the smallest that
    // has the channels; its speakers set to the session's either way.
    let device = default
        .filter(|d| d.is_virtual && d.channels >= wanted as usize)
        .or_else(|| {
            tap::output_devices()
                .into_iter()
                .filter(|d| d.is_virtual && d.channels >= wanted as usize)
                .min_by_key(|d| d.channels)
        })?;
    let route = Route::to(&device, wanted);
    // Read again: its speakers may have just been set.
    let device = tap::output_devices()
        .into_iter()
        .find(|d| d.id == device.id)?;
    Some((device, Some(route)))
}

fn surround(
    wanted: u8,
    host_audio: bool,
    bitrate_kbps: u32,
    endpoint: &Arc<Endpoint>,
    peer: &Arc<Peer>,
) -> Result<Option<(AudioHandle, u8)>, String> {
    let has_surround = tap::output_devices()
        .iter()
        .any(|d| d.channels > 2 && (d.is_virtual || d.surround_channels() > 2));
    if !has_surround {
        tracing::info!(
            wanted,
            "no output with surround speakers (a receiver, or a loopback device such as \
                BlackHole 16ch); streaming stereo"
        );
        return Ok(None);
    }
    match permissions::audio_capture() {
        Consent::Granted => {}
        Consent::NotAsked => {
            permissions::ask_audio_capture();
            return Ok(None);
        }
        Consent::Denied => {
            tracing::warn!(
                "sound in surround needs System Audio Recording, which was turned down \
                    (System Settings > Privacy & Security > Screen & System Audio Recording)"
            );
            return Ok(None);
        }
    }
    let Some((device, route)) = output_for(wanted, host_audio) else {
        return Ok(None);
    };
    let labels = device.labels();
    let channels = wanted.min(channels::surround_channels(&labels));
    if channels <= 2 {
        tracing::info!(
            device = device.name,
            "its speakers are not surround; streaming stereo"
        );
        return Ok(None);
    }
    let plan = channels::plan(&labels, channels);
    let mute = !host_audio && !device.is_virtual;
    let bitrate = pingpong_audio::bitrate_bps(channels, bitrate_kbps);
    let open = move |chunks: Chunks| -> Result<(Tap, Option<Route>), String> {
        // Used on Core Audio's real-time thread: made here, reused there.
        let mut wired: Vec<f32> = Vec::with_capacity(4096 * channels as usize);
        let mut resampler = Resampler::new(device.rate, channels as usize);
        let in_channels = device.channels;
        let tap = Tap::start(
            &device,
            mute,
            &test_only(),
            Box::new(move |pcm| {
                wired.clear();
                channels::remap(pcm, in_channels, &plan, channels as usize, &mut wired);
                chunks.send(|buf| resampler.push(&wired, buf));
            }),
        )?;
        if tap.channels != in_channels {
            return Err(format!(
                "the tap has {} channels, the device {in_channels}",
                tap.channels
            ));
        }
        if (tap.rate - 48_000.0).abs() > 1.0 {
            tracing::info!(
                rate = tap.rate,
                "the output runs at another rate than the stream's 48 kHz: resampled (set it \
                    to 48 kHz in Audio MIDI Setup for the best sound)"
            );
        }
        // The tap stops before the route puts the old output back.
        Ok((tap, route))
    };
    let handle = AudioHandle::start(open, channels, bitrate, endpoint.clone(), peer.clone())?;
    tracing::info!(channels, muted = mute, "sound in surround");
    Ok(Some((handle, channels)))
}

/// PONG_TEST_TAP_ONLY=PID,...: the processes the surround capture hears,
/// and no others. A client streaming this Mac to itself plays the session's
/// sound into the device the tap hears, which would send it round again;
/// a test names its signal's player instead.
fn test_only() -> Vec<i32> {
    std::env::var("PONG_TEST_TAP_ONLY")
        .map(|v| v.split(',').filter_map(|p| p.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// The system's mix, from ScreenCaptureKit, in stereo.
fn stereo(
    display: u32,
    bitrate_kbps: u32,
    endpoint: Arc<Endpoint>,
    peer: Arc<Peer>,
) -> Option<(AudioHandle, u8)> {
    let channels = crate::audio::CHANNELS;
    let bitrate = pingpong_audio::bitrate_bps(channels, bitrate_kbps);
    let open = move |chunks: Chunks| {
        pingpong_capture::sck::SckAudio::new(
            display,
            Box::new(move |pcm, n| {
                chunks.send(|buf| match n {
                    2 => buf.extend_from_slice(pcm),
                    1 => buf.extend(pcm.iter().flat_map(|&s| [s, s])),
                    n => buf.extend(pcm.chunks_exact(n).flat_map(|f| [f[0], f[1]])),
                })
            }),
        )
        .map_err(|e| e.to_string())
    };
    match AudioHandle::start(open, channels, bitrate, endpoint, peer) {
        Ok(a) => Some((a, channels)),
        Err(e) => {
            tracing::warn!(error = %e, "audio unavailable; streaming video only");
            None
        }
    }
}
