//! Linux: the sound the machine plays -- the monitor of its default output --
//! through PulseAudio's API, which PipeWire serves as well. 48 kHz stereo,
//! handed on a frame at a time from a thread of its own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use libpulse_binding::def::BufferAttr;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::Direction;
use libpulse_simple_binding::Simple;

use crate::CaptureError;

/// Interleaved stereo samples, a frame's worth.
pub type PulseSink = Box<dyn FnMut(&[f32]) + Send>;

pub struct PulseCapture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PulseCapture {
    /// Capture in frames of `frame` samples a channel, each handed to `sink`.
    pub fn new(frame: usize, mut sink: PulseSink) -> Result<PulseCapture, CaptureError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || {
                    let spec = Spec {
                        format: Format::FLOAT32NE,
                        channels: 2,
                        rate: 48_000,
                    };
                    let bytes = frame * 2 * 4;
                    // Handed over a frame at a time, not in PulseAudio's
                    // default two-second pieces.
                    let attr = BufferAttr {
                        maxlength: u32::MAX,
                        tlength: u32::MAX,
                        prebuf: u32::MAX,
                        minreq: u32::MAX,
                        fragsize: bytes as u32,
                    };
                    let pa = match Simple::new(
                        None,
                        "Pong",
                        Direction::Record,
                        Some("@DEFAULT_MONITOR@"),
                        "the stream's sound",
                        &spec,
                        None,
                        Some(&attr),
                    ) {
                        Ok(pa) => {
                            let _ = ready_tx.send(Ok(()));
                            pa
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("{e:?}")));
                            return;
                        }
                    };
                    let mut raw = vec![0u8; bytes];
                    let mut samples = vec![0f32; frame * 2];
                    while !stop.load(Ordering::Relaxed) {
                        if let Err(e) = pa.read(&mut raw) {
                            tracing::warn!(error = ?e, "sound capture stopped");
                            break;
                        }
                        for (s, b) in samples.iter_mut().zip(raw.as_chunks::<4>().0) {
                            *s = f32::from_ne_bytes(*b);
                        }
                        sink(&samples);
                    }
                })
                .map_err(|e| CaptureError::Platform(e.to_string()))?
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(PulseCapture {
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(CaptureError::Platform(format!(
                "no sound server (PulseAudio or PipeWire): {e}"
            ))),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err(CaptureError::Platform(
                    "the sound server did not answer within 5 s".into(),
                ))
            }
        }
    }
}

impl Drop for PulseCapture {
    fn drop(&mut self) {
        // A read returns within a frame.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
