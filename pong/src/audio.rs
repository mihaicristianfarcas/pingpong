//! The session's audio stream (Windows): capture on its own thread, encode
//! each 5 ms block to Opus and send it at once, with parity per block of four.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use pingpong_audio::opus::Encoder;
use pingpong_audio::wasapi::{self, CaptureConfig};
use pingpong_proto::audio::AudioPacketizer;
use pingpong_transport::{Endpoint, Peer};

pub struct AudioParams {
    pub channels: u8,
    pub bitrate_bps: u32,
    /// Keep sound on the host's own speakers too (Moonlight's "play audio on
    /// host"); otherwise it goes to a virtual sink for the session.
    pub host_audio: bool,
    pub state_path: std::path::PathBuf,
}

pub struct AudioHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub packets: Arc<AtomicU64>,
}

pub fn state_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("audio-state.txt")
}

/// A previous run died mid-session: give the host its speakers back.
pub fn restore_stale(data_dir: &std::path::Path) {
    wasapi::restore_stale(&state_path(data_dir));
}

impl AudioHandle {
    pub fn start(
        params: AudioParams,
        endpoint: Arc<Endpoint>,
        peer: Arc<Peer>,
    ) -> Result<AudioHandle, String> {
        let mut encoder = Encoder::new(params.channels, params.bitrate_bps)?;
        let stop = Arc::new(AtomicBool::new(false));
        let packets = Arc::new(AtomicU64::new(0));
        let thread = {
            let (stop, packets) = (stop.clone(), packets.clone());
            std::thread::Builder::new()
                .name("audio".into())
                .spawn(move || {
                    let cfg = CaptureConfig {
                        channels: params.channels,
                        virtual_sink: !params.host_audio,
                        state_path: Some(params.state_path),
                    };
                    let mut packetizer = AudioPacketizer::new();
                    let mut buf = [0u8; pingpong_proto::audio::MAX_PACKET];
                    let result = wasapi::run(&cfg, &stop, |pcm| {
                        let n = match encoder.encode(pcm, &mut buf) {
                            Ok(n) => n,
                            Err(e) => {
                                tracing::warn!(error = %e, "opus encode failed");
                                return;
                            }
                        };
                        let ts = pingpong_proto::clock::now_us();
                        for datagram in packetizer.push(&buf[..n], ts) {
                            if let Err(e) = endpoint.send(&peer, &datagram) {
                                tracing::debug!(error = %e, "audio send failed");
                            }
                        }
                        packets.fetch_add(1, Ordering::Relaxed);
                    });
                    if let Err(e) = result {
                        tracing::error!(error = %e, "audio capture failed");
                    }
                })
                .map_err(|e| e.to_string())?
        };
        tracing::info!(
            channels = params.channels,
            kbps = params.bitrate_bps / 1000,
            host_audio = params.host_audio,
            "audio stream started"
        );
        Ok(AudioHandle {
            stop,
            thread: Some(thread),
            packets,
        })
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
