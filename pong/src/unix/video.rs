//! The session's video pipeline on macOS and Linux: the platform's capture
//! and encoder (`platform::open_video`: ScreenCaptureKit and VideoToolbox;
//! X11 and FFmpeg) on one thread, feeding the same paced sender as on
//! Windows.
//!
//! Timing is Apollo's capture loop, as on Windows (see `pipeline`): never
//! faster than the negotiated frame rate, a still desktop re-encoded at
//! max(fps / 5, 10) frames a second so it keeps sharpening, and recovery
//! requests acted on between frames.
//!
//! The capture has `grab(timeout_ms)` and `image()`; the encoder takes that
//! image in `submit(&image, index, idr)` and hands frames out of `next`, as
//! VideoToolbox does (an encoder that works synchronously has none in
//! flight), with `invalidate` false where it has no reference invalidation.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpong_capture::Grab;
use pingpong_encode::EncoderConfig;
use pingpong_proto::clock;
use pingpong_transport::Endpoint;

use crate::pipeline::{Cadence, EncodeThread, FrameOut};
pub use crate::pipeline::{VideoCmd, VideoHandle};
use crate::sender;

/// Frames in the encoder at once. Two keep more frames a second where the
/// encoder is slower than the frame interval, at the price of latency:
/// measured at 3024x1890 on an M4 Pro, one in flight gave 16 ms and 55 fps,
/// two 27 ms and 63 fps. Latency wins, as in Moonlight.
const MAX_IN_FLIGHT: usize = 1;

/// A frame sent this long ago with no loss reported for it has arrived: the
/// client reports a loss within a round trip and its gate's wait.
const ARRIVED_AFTER: Duration = Duration::from_millis(200);
/// Sent frames remembered for mapping loss reports back (~4 s at 120 fps).
const SENT_REMEMBERED: usize = 512;

#[derive(Debug, Clone)]
pub struct VideoParams {
    /// What to capture: a CoreGraphics display; the X screen or a portal's
    /// screen cast.
    pub source: crate::platform::VideoSource,
    pub encoder: EncoderConfig,
    pub pace_mbps: u32,
}

/// Start capturing and encoding. Blocks until the encoder is running (or
/// failed), so the caller can ack the session with the truth.
pub fn start(
    params: VideoParams,
    endpoint: Arc<Endpoint>,
    recipients: sender::Recipients,
) -> Result<VideoHandle, String> {
    let pace_mbps = params.pace_mbps;
    crate::pipeline::start(params, pace_mbps, endpoint, recipients, encode_loop)
}

fn encode_loop(thread: EncodeThread<VideoParams>) {
    let EncodeThread {
        params,
        frames,
        cmds,
        stop,
        streaming,
        stats,
        ready,
    } = thread;
    let e = params.encoder;
    let setup = crate::platform::open_video(&params).and_then(|(mut cap, enc)| {
        // The first image, so the first frame out is the desktop.
        let deadline = Instant::now() + Duration::from_secs(5);
        while cap.image().is_none() {
            if Instant::now() > deadline {
                return Err("no desktop image within 5 s".to_string());
            }
            let _ = cap.grab(100);
        }
        Ok((cap, enc))
    });
    let (mut cap, mut enc) = match setup {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let Cadence {
        interval,
        repeat_every,
        slack,
    } = Cadence::new(e.fps_mhz);
    let mut out = FrameOut::new(frames, stats.clone(), e.fps());

    let mut index: u64 = 0;
    // Frame ids on the wire count only frames sent: one the encoder dropped
    // leaves no gap for the client to take for a loss.
    let mut wire_id: u32 = 0;
    let mut force_idr = true;
    let mut last_submit: Option<Instant> = None;
    let mut have_new = false;
    // Submitted frames: (index, captured at, a repeat).
    let mut pending: VecDeque<(u64, u32, bool)> = VecDeque::new();
    // Frames sent: (wire id, index, when). A loss report names wire ids; a
    // frame nobody reported lost in time has arrived.
    let mut sent: VecDeque<(u32, u64, Instant)> = VecDeque::new();
    let mut acknowledged_through: Option<u64> = None;

    'run: while !stop.load(Ordering::Relaxed) {
        for cmd in cmds.try_iter() {
            match cmd {
                VideoCmd::Idr => force_idr = true,
                // From a long-term reference the client has, else a keyframe.
                VideoCmd::Invalidate { first, last } => {
                    let index_of = |id: u32| sent.iter().find(|s| s.0 == id).map(|s| s.1);
                    match (index_of(first), index_of(last)) {
                        (Some(f), Some(l)) if enc.invalidate(f, l) => {}
                        _ => force_idr = true,
                    }
                }
                VideoCmd::Bitrate(bps) => {
                    if let Err(e) = enc.set_bitrate(bps) {
                        tracing::warn!(error = %e, "bitrate change failed");
                    }
                }
            }
        }

        // Out with whatever the encoder has finished; with the pipeline full,
        // wait (briefly) for the oldest.
        let wait = if enc.in_flight() >= MAX_IN_FLIGHT {
            Duration::from_millis(4)
        } else {
            Duration::ZERO
        };
        while let Some(done) = enc.next(if pending.len() >= MAX_IN_FLIGHT {
            wait
        } else {
            Duration::ZERO
        }) {
            let frame = match done {
                Ok(f) => f,
                Err(e) => {
                    tracing::error!(error = %e, "encode failed; ending the video pipeline");
                    break 'run;
                }
            };
            let Some(pos) = pending.iter().position(|p| p.0 == frame.index) else {
                continue;
            };
            let (_, captured_at_us, repeat) = pending.remove(pos).unwrap();
            if frame.data.is_empty() {
                continue; // dropped by rate control
            }
            let host_us = clock::since(captured_at_us);
            let wire_frame = out.frame(
                wire_id,
                frame.kind,
                &frame.data,
                captured_at_us,
                host_us,
                repeat,
            );
            sent.push_back((wire_id, frame.index, Instant::now()));
            while sent.len() > SENT_REMEMBERED {
                sent.pop_front();
            }
            wire_id = wire_id.wrapping_add(1);
            out.send(wire_frame);
        }
        // Frames out long enough that a loss would have been reported.
        if let Some(&(_, index, _)) = sent.iter().rev().find(|s| s.2.elapsed() >= ARRIVED_AFTER) {
            if acknowledged_through.is_none_or(|a| index > a) {
                enc.acknowledge_through(index);
                acknowledged_through = Some(index);
            }
        }
        if enc.in_flight() >= MAX_IN_FLIGHT {
            continue;
        }

        if !streaming.load(Ordering::Acquire) {
            if let Ok(Grab::Frame) = cap.grab(10) {
                have_new = true;
            }
            continue;
        }

        // Not before the next frame slot.
        let now = Instant::now();
        if let Some(last) = last_submit {
            let slot = last + interval - slack;
            if now < slot && !force_idr {
                std::thread::sleep((slot - now).min(Duration::from_millis(2)));
                continue;
            }
        }

        let repeat_due = last_submit
            .map(|t| t + repeat_every)
            .unwrap_or_else(Instant::now);
        let wait_ms = repeat_due
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(2) as u32;
        match cap.grab(wait_ms) {
            Ok(Grab::Frame) => {
                have_new = true;
                stats.captured.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Grab::Timeout) => {}
            Err(e) => {
                tracing::debug!(error = %e, "capture");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        let now = Instant::now();
        let repeat = !have_new && now >= repeat_due;
        if !(have_new || force_idr || repeat) {
            continue;
        }
        let Some(image) = cap.image() else { continue };
        let captured_at_us = clock::now_us();
        have_new = false;
        if let Err(e) = enc.submit(&image, index, force_idr) {
            tracing::error!(error = %e, "encode failed; ending the video pipeline");
            break;
        }
        pending.push_back((index, captured_at_us, repeat));
        index += 1;
        last_submit = Some(now);
        force_idr = false;
    }
}
