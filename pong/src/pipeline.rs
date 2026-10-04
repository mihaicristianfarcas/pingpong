//! What a session's video pipeline is on every host: an encode thread (the
//! platform's capture and encoder, in `video`) feeding the paced send thread
//! (`sender`), and the handle the session drives them with.
//!
//! Timing follows Apollo's capture loop (`display_base.cpp`), on every host:
//! - never faster than the negotiated frame rate: after a frame, wait for the
//!   next frame slot before accepting another;
//! - a desktop that presents nothing is re-encoded at a minimum rate of
//!   max(fps / 5, 10) frames per second, so a still image keeps sharpening
//!   (CBR spends the idle budget refining it) instead of freezing at whatever
//!   quality the last motion left it;
//! - loss recovery (IDR / reference invalidation) is applied between frames,
//!   within a few milliseconds of the request arriving.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use pingpong_encode::FrameKind;
use pingpong_proto::fec::FecPolicy;
use pingpong_proto::telemetry::Histogram;
use pingpong_proto::video::{write_prefix, FrameType};
use pingpong_transport::Endpoint;

use crate::sender::{self, OutFrame, SendStats};

/// What the session asks of a running pipeline.
pub enum VideoCmd {
    /// The next frame is a keyframe.
    Idr,
    /// The client lost frames `first..=last` (wire ids): recover without
    /// them, or with a keyframe where the encoder cannot.
    Invalidate { first: u32, last: u32 },
    /// A new target bitrate, in bits per second.
    Bitrate(u32),
}

/// Counters the session's once-a-second report reads (and resets).
#[derive(Default)]
pub struct VideoStats {
    pub encoded: AtomicU64,
    pub captured: AtomicU64,
    pub repeated: AtomicU64,
    pub idrs: AtomicU64,
    pub recoveries: AtomicU64,
    /// Capture to encoded, per frame: handed over once `fps` samples are in.
    pub encode_us: Mutex<Option<Histogram>>,
}

pub struct VideoHandle {
    stop: Arc<AtomicBool>,
    streaming: Arc<AtomicBool>,
    cmds: Sender<VideoCmd>,
    encode: Option<JoinHandle<()>>,
    send: Option<JoinHandle<()>>,
    /// The sender's `FecPolicy`, as bits.
    fec: Arc<AtomicU16>,
    /// The client may be sent LAN-sized shards (see `sender`).
    lan_shards: Arc<AtomicBool>,
    pub stats: Arc<VideoStats>,
    pub send_stats: Arc<SendStats>,
}

impl VideoHandle {
    /// Let frames onto the wire. The session acks first, so the client knows
    /// the session exists before the first frame lands.
    pub fn open(&self) {
        self.streaming.store(true, Ordering::Release);
    }

    pub fn command(&self, cmd: VideoCmd) {
        let _ = self.cmds.try_send(cmd);
    }

    /// Parity for the frames sent from now on.
    pub fn set_fec(&self, fec: FecPolicy) {
        self.fec.store(fec.to_bits(), Ordering::Relaxed);
    }

    /// The client understands LAN-sized shards (and they get through): the
    /// sender uses them while it reaches the client on the local network.
    pub fn set_lan_shards(&self, allowed: bool) {
        self.lan_shards.store(allowed, Ordering::Relaxed);
    }

    pub fn commands(&self) -> Sender<VideoCmd> {
        self.cmds.clone()
    }

    pub fn is_running(&self) -> bool {
        self.encode.as_ref().is_some_and(|h| !h.is_finished())
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.encode.take() {
            let _ = h.join();
        }
        if let Some(h) = self.send.take() {
            let _ = h.join();
        }
    }
}

impl Drop for VideoHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What an encode loop is handed when its thread starts.
pub struct EncodeThread<P> {
    pub params: P,
    pub frames: Sender<OutFrame>,
    pub cmds: Receiver<VideoCmd>,
    pub stop: Arc<AtomicBool>,
    /// Frames may go out (see [`VideoHandle::open`]).
    pub streaming: Arc<AtomicBool>,
    pub stats: Arc<VideoStats>,
    /// Told once whether the capture and encoder came up.
    pub ready: Sender<Result<(), String>>,
}

/// Start the send thread, and `encode_loop` on a thread of its own. Blocks
/// until the encoder is running (or failed), so the caller can ack the
/// session with the truth.
pub fn start<P: Send + 'static>(
    params: P,
    pace_mbps: u32,
    endpoint: Arc<Endpoint>,
    recipients: sender::Recipients,
    encode_loop: fn(EncodeThread<P>),
) -> Result<VideoHandle, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let streaming = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(VideoStats::default());
    let send_stats = Arc::new(SendStats::default());
    let fec = Arc::new(AtomicU16::new(FecPolicy::DEFAULT.to_bits()));
    let lan_shards = Arc::new(AtomicBool::new(false));
    let (cmd_tx, cmd_rx) = bounded(64);
    // Small and blocking: an encoded frame is never dropped (the next one
    // predicts from it), so if the network cannot keep up the encoder waits.
    let (frame_tx, frame_rx) = bounded::<OutFrame>(3);
    let (ready_tx, ready_rx) = bounded(1);

    let send = {
        let (recipients, stop, stats, fec, lan) = (
            recipients,
            stop.clone(),
            send_stats.clone(),
            fec.clone(),
            lan_shards.clone(),
        );
        std::thread::Builder::new()
            .name("video-send".into())
            .spawn(move || {
                crate::priority::latency_critical();
                sender::run(sender::SendLoop {
                    endpoint,
                    recipients,
                    frames: frame_rx,
                    pace_mbps,
                    fec,
                    lan_shards: lan,
                    stop,
                    stats,
                })
            })
            .map_err(|e| e.to_string())?
    };
    let encode = {
        let thread = EncodeThread {
            params,
            frames: frame_tx,
            cmds: cmd_rx,
            stop: stop.clone(),
            streaming: streaming.clone(),
            stats: stats.clone(),
            ready: ready_tx,
        };
        std::thread::Builder::new()
            .name("video-encode".into())
            .spawn(move || {
                crate::priority::latency_critical();
                encode_loop(thread)
            })
            .map_err(|e| e.to_string())?
    };

    let mut handle = VideoHandle {
        stop,
        streaming,
        cmds: cmd_tx,
        encode: Some(encode),
        send: Some(send),
        fec,
        lan_shards,
        stats,
        send_stats,
    };
    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(e)) => {
            handle.shutdown();
            Err(e)
        }
        Err(_) => {
            handle.shutdown();
            Err("video pipeline did not start within 10 s".into())
        }
    }
}

/// The capture loop's clock, from the negotiated frame rate.
pub struct Cadence {
    /// One frame slot.
    pub interval: Duration,
    /// A still desktop is re-encoded this often: max(fps / 5, 10) a second.
    pub repeat_every: Duration,
    /// Tolerated jitter in the desktop's own present cadence, so a 120 Hz
    /// display feeding a 120 fps stream is never skipped by a hair.
    pub slack: Duration,
}

impl Cadence {
    /// For `fps_mhz` frames a second, in millihertz (59.94 Hz is 59_940).
    pub fn new(fps_mhz: u32) -> Cadence {
        let fps_mhz = fps_mhz.max(1000) as u64;
        let interval = Duration::from_nanos(1_000_000_000_000 / fps_mhz);
        let repeat_mhz = (fps_mhz / 5).max(10_000);
        Cadence {
            interval,
            repeat_every: Duration::from_nanos(1_000_000_000_000 / repeat_mhz),
            slack: interval / 8,
        }
    }
}

/// Hands encoded frames to the send thread, and counts them.
pub struct FrameOut {
    frames: Sender<OutFrame>,
    stats: Arc<VideoStats>,
    fps: u32,
    hist: Histogram,
}

impl FrameOut {
    pub fn new(frames: Sender<OutFrame>, stats: Arc<VideoStats>, fps: u32) -> FrameOut {
        FrameOut {
            frames,
            stats,
            fps: fps.max(1),
            hist: Histogram::new("capture->encoded"),
        }
    }

    /// An encoded frame, as the sender takes it: `bitstream` behind the
    /// pingpong frame prefix, with wire id `id`. `host_us` is capture to
    /// encoded; `repeat`, a still desktop re-encoded.
    pub fn frame(
        &mut self,
        id: u32,
        kind: FrameKind,
        bitstream: &[u8],
        captured_at_us: u32,
        host_us: u32,
        repeat: bool,
    ) -> OutFrame {
        self.hist.record(host_us);
        let stats = &self.stats;
        stats.encoded.fetch_add(1, Ordering::Relaxed);
        if repeat {
            stats.repeated.fetch_add(1, Ordering::Relaxed);
        }
        let kind = match kind {
            FrameKind::Idr => {
                stats.idrs.fetch_add(1, Ordering::Relaxed);
                FrameType::Idr
            }
            FrameKind::Recovery => {
                stats.recoveries.fetch_add(1, Ordering::Relaxed);
                FrameType::Recovery
            }
            FrameKind::P => FrameType::P,
        };
        let mut data = Vec::with_capacity(bitstream.len() + 4);
        data.extend_from_slice(&write_prefix(host_us));
        data.extend_from_slice(bitstream);
        OutFrame {
            id,
            kind,
            capture_ts_us: captured_at_us,
            data,
        }
    }

    /// Queue `out` for the sender, and hand over a second's latency samples.
    pub fn send(&mut self, out: OutFrame) {
        if self
            .frames
            .send_timeout(out, Duration::from_millis(500))
            .is_err()
        {
            // The sender is wedged. The client will see a gap and recover.
            tracing::warn!("send queue stalled; frame discarded");
        }
        if self.hist.count() >= self.fps as usize {
            *self.stats.encode_us.lock() = Some(std::mem::replace(
                &mut self.hist,
                Histogram::new("capture->encoded"),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fractional_rate_paces_to_the_nanosecond() {
        let c = Cadence::new(59_940);
        assert_eq!(c.interval, Duration::from_nanos(16_683_350));
        assert_eq!(
            Cadence::new(120_000).interval,
            Duration::from_nanos(8_333_333)
        );
    }

    #[test]
    fn a_still_desktop_repeats_at_a_fifth_of_the_rate_and_never_below_ten() {
        assert_eq!(
            Cadence::new(120_000).repeat_every,
            Duration::from_nanos(41_666_666)
        );
        assert_eq!(
            Cadence::new(30_000).repeat_every,
            Duration::from_millis(100)
        );
    }
}
