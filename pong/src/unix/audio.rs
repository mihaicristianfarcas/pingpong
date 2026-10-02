//! The session's sound on macOS and Linux: what the platform captures
//! (`platform::audio`: ScreenCaptureKit, or a Core Audio tap for surround;
//! PulseAudio or PipeWire), cut into the same 5 ms Opus frames the Windows
//! host sends, with parity per block of four.
//!
//! Captures hand over interleaved samples in [`Chunks`]: buffers that come
//! back once sent, so a capture callback (Core Audio's real-time thread
//! among them) neither allocates nor waits. A host streams the channels its
//! capture has (stereo, unless a Mac's tap has surround), whatever the
//! client asked for: the ack says how many, and the client plays what the
//! ack says.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use pingpong_audio::opus::Encoder;
use pingpong_proto::audio::{AudioPacketizer, FRAME_SAMPLES};
use pingpong_transport::{Endpoint, Peer};

/// What these hosts stream unless a capture has more.
pub const CHANNELS: u8 = 2;

/// Buffers of sound on their way to the sender, and back.
const IN_FLIGHT: usize = 64;

/// Where a capture puts its sound: interleaved samples, a chunk at a time,
/// in buffers the sender hands back.
#[derive(Clone)]
pub struct Chunks {
    full: Sender<Vec<f32>>,
    free: Receiver<Vec<f32>>,
}

impl Chunks {
    /// Hand over what `fill` puts in a (cleared) buffer. Dropped when the
    /// sender is a few hundred milliseconds behind, rather than waiting.
    pub fn send(&self, fill: impl FnOnce(&mut Vec<f32>)) {
        let mut buf = self.free.try_recv().unwrap_or_default();
        buf.clear();
        fill(&mut buf);
        if !buf.is_empty() {
            let _ = self.full.try_send(buf);
        }
    }
}

pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    /// The platform's capture, running until dropped.
    capture: Option<Box<dyn Send>>,
    thread: Option<JoinHandle<()>>,
    pub packets: Arc<AtomicU64>,
}

impl AudioHandle {
    /// Send what `open` captures: it is handed where to put interleaved
    /// samples, `channels` a frame (any amount at a time), and returns the
    /// capture.
    pub fn start<C: Send + 'static>(
        open: impl FnOnce(Chunks) -> Result<C, String>,
        channels: u8,
        bitrate_bps: u32,
        endpoint: Arc<Endpoint>,
        peer: Arc<Peer>,
    ) -> Result<AudioHandle, String> {
        let mut encoder = Encoder::new(channels, bitrate_bps)?;
        let stop = Arc::new(AtomicBool::new(false));
        let packets = Arc::new(AtomicU64::new(0));
        // A few hundred milliseconds of chunks; a stalled sender drops sound
        // rather than blocking the capture.
        let (tx, rx) = bounded::<Vec<f32>>(IN_FLIGHT);
        let (free_tx, free_rx) = bounded::<Vec<f32>>(IN_FLIGHT);
        let capture: Box<dyn Send> = Box::new(open(Chunks {
            full: tx,
            free: free_rx,
        })?);

        let thread = {
            let (stop, packets) = (stop.clone(), packets.clone());
            std::thread::Builder::new()
                .name("audio".into())
                .spawn(move || {
                    crate::priority::latency_critical();
                    let frame = FRAME_SAMPLES * channels as usize;
                    let mut pending: Vec<f32> = Vec::with_capacity(frame * 8);
                    let mut packetizer = AudioPacketizer::new();
                    let mut buf = [0u8; pingpong_proto::audio::MAX_PACKET];
                    // The loudest sample of the first second, logged: silence
                    // there is worth knowing about.
                    let (mut peak, mut heard) = (0f32, 0usize);
                    while !stop.load(Ordering::Relaxed) {
                        let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) else {
                            continue;
                        };
                        let second =
                            pingpong_proto::audio::SAMPLE_RATE as usize * channels as usize;
                        if heard < second {
                            peak = chunk.iter().fold(peak, |p, s| p.max(s.abs()));
                            heard += chunk.len();
                            if heard >= second {
                                tracing::info!(
                                    peak = format!("{peak:.3}"),
                                    "first second of sound"
                                );
                            }
                        }
                        pending.extend_from_slice(&chunk);
                        let _ = free_tx.try_send(chunk);
                        let mut at = 0;
                        while pending.len() - at >= frame {
                            let n = match encoder.encode(&pending[at..at + frame], &mut buf) {
                                Ok(n) => n,
                                Err(e) => {
                                    tracing::warn!(error = %e, "opus encode failed");
                                    at += frame;
                                    continue;
                                }
                            };
                            at += frame;
                            let ts = pingpong_proto::clock::now_us();
                            for datagram in packetizer.push(&buf[..n], ts) {
                                if let Err(e) = endpoint.send(&peer, &datagram) {
                                    tracing::debug!(error = %e, "audio send failed");
                                }
                            }
                            packets.fetch_add(1, Ordering::Relaxed);
                        }
                        pending.drain(..at);
                    }
                })
                .map_err(|e| e.to_string())?
        };
        tracing::info!(channels, kbps = bitrate_bps / 1000, "audio stream started");
        Ok(AudioHandle {
            stop,
            capture: Some(capture),
            thread: Some(thread),
            packets,
        })
    }

    pub fn stop(mut self) {
        self.capture = None;
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
