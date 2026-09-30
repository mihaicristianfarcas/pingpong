//! The session's sound on macOS and Linux: what the platform captures
//! (`platform::audio`: ScreenCaptureKit; PulseAudio or PipeWire), in stereo,
//! cut into the same 5 ms Opus frames the Windows host sends, with parity per
//! block of four.
//!
//! Both capture the system mix in stereo, so these hosts stream stereo
//! whatever the client asked for (the ack says so, and the client plays what
//! the ack says).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, Sender};
use pingpong_audio::opus::Encoder;
use pingpong_proto::audio::{AudioPacketizer, FRAME_SAMPLES};
use pingpong_transport::{Endpoint, Peer};

/// What these hosts stream.
pub const CHANNELS: u8 = 2;

pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    /// The platform's capture, running until dropped.
    capture: Option<Box<dyn Send>>,
    thread: Option<JoinHandle<()>>,
    pub packets: Arc<AtomicU64>,
}

impl AudioHandle {
    /// Send what `open` captures: it is handed where to put interleaved
    /// stereo samples (any amount at a time) and returns the capture.
    pub fn start<C: Send + 'static>(
        open: impl FnOnce(Sender<Vec<f32>>) -> Result<C, String>,
        bitrate_bps: u32,
        endpoint: Arc<Endpoint>,
        peer: Arc<Peer>,
    ) -> Result<AudioHandle, String> {
        let mut encoder = Encoder::new(CHANNELS, bitrate_bps)?;
        let stop = Arc::new(AtomicBool::new(false));
        let packets = Arc::new(AtomicU64::new(0));
        // A few hundred milliseconds of chunks; a stalled sender drops sound
        // rather than blocking the capture.
        let (tx, rx) = bounded::<Vec<f32>>(64);
        let capture: Box<dyn Send> = Box::new(open(tx)?);

        let thread = {
            let (stop, packets) = (stop.clone(), packets.clone());
            std::thread::Builder::new()
                .name("audio".into())
                .spawn(move || {
                    crate::priority::latency_critical();
                    let frame = FRAME_SAMPLES * CHANNELS as usize;
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
                        if heard < pingpong_proto::audio::SAMPLE_RATE as usize * CHANNELS as usize {
                            peak = chunk.iter().fold(peak, |p, s| p.max(s.abs()));
                            heard += chunk.len();
                            if heard
                                >= pingpong_proto::audio::SAMPLE_RATE as usize * CHANNELS as usize
                            {
                                tracing::info!(
                                    peak = format!("{peak:.3}"),
                                    "first second of sound"
                                );
                            }
                        }
                        pending.extend_from_slice(&chunk);
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
        tracing::info!(
            channels = CHANNELS,
            kbps = bitrate_bps / 1000,
            "audio stream started"
        );
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
