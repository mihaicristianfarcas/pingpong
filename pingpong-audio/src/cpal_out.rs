//! Linux playback through cpal (ALSA, which PipeWire and PulseAudio serve):
//! the default output device, 48 kHz float at the stream's channel count,
//! fed from the player's ring.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use pingpong_proto::audio::SAMPLE_RATE;

use crate::player::{Feeder, Output};

struct Playback {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Output for Playback {}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

/// Play `feeder` on the default output device until the returned handle is
/// dropped. The stream lives on a thread of its own (cpal's is not `Send`).
pub fn open(feeder: Feeder) -> Result<Box<dyn Output>, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("ping-audio-out".into())
            .spawn(move || {
                let stream = match build(feeder) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(()));
                while !stop.load(Ordering::Relaxed) {
                    std::thread::park_timeout(Duration::from_millis(200));
                }
                drop(stream);
            })
            .map_err(|e| e.to_string())?
    };
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(Box::new(Playback {
            stop,
            thread: Some(thread),
        })),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the audio device did not start".into()),
    }
}

fn build(mut feeder: Feeder) -> Result<cpal::Stream, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or("no audio output device")?;
    let config = cpal::StreamConfig {
        channels: feeder.channels() as u16,
        sample_rate: SAMPLE_RATE,
        buffer_size: cpal::BufferSize::Default,
    };
    let channels = config.channels;
    let stream = device
        .build_output_stream(
            config,
            move |out: &mut [f32], _| feeder.fill(out),
            |e| tracing::warn!(error = %e, "audio output"),
            None,
        )
        .map_err(|e| format!("audio output: {e}"))?;
    stream.play().map_err(|e| format!("audio output: {e}"))?;
    tracing::info!(
        device = device
            .description()
            .map(|d| d.to_string())
            .unwrap_or_default(),
        channels,
        "audio playback started"
    );
    Ok(stream)
}
