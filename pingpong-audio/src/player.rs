//! Client playback, the way Moonlight plays Sunshine's audio: packets from the
//! network are put back in order (waiting briefly for FEC to fill a gap),
//! decoded -- or concealed with Opus PLC when lost -- into a lock-free ring,
//! and the platform's output callback drains the ring.
//!
//! Latency adapts to the network, as WebRTC's jitter buffer does: after an
//! underrun the output waits for the target amount of audio before playing
//! again, and each underrun raises the target (a Wi-Fi scan stalls delivery for
//! tens of milliseconds); a long quiet stretch lowers it again. Whenever the
//! lowest fill over half a second stays well above the target, the excess is
//! dropped. That also absorbs the drift between the host's and this device's
//! sample clocks.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use pingpong_proto::audio::{AudioPacket, Playout, Reorder, FRAME_SAMPLES, SAMPLE_RATE};

use crate::opus::Decoder;

/// Audio buffered before playback (re)starts: the target's floor, ceiling,
/// and the step an underrun raises it by.
const TARGET_MIN_MS: u64 = 20;
const TARGET_MAX_MS: u64 = 80;
const TARGET_STEP_MS: u64 = 10;
/// Underrun-free time after which the target comes down a step.
const TARGET_RELAX: Duration = Duration::from_secs(15);
/// How far below the target the steady margin may sit before trimming.
const MARGIN_BELOW_TARGET_MS: u64 = 5;
/// Below this margin, a concealment frame is inserted instead of risking an
/// underrun (the device is consuming faster than the host produces).
const MIN_MARGIN_MS: usize = 2;
/// How long a gap waits for its FEC block (the parity for a block's first
/// packet trails it by three packets).
const GAP_WAIT_US: u64 = 17_000;
/// Packets queued behind a gap that end the wait early.
const GAP_MAX_AHEAD: u32 = 8;
const RING_MS: usize = 250;
/// Fill is sampled over windows this long...
const WINDOW: Duration = Duration::from_millis(500);
/// Longest the decode loop sleeps with nothing to wait for.
const IDLE_WAIT: Duration = Duration::from_millis(20);
/// ...and trimming acts on the lowest fill over this many of them (2 s), so
/// the burst that follows a stall is not cut just before the next stall.
const TRIM_WINDOWS: usize = 4;

fn samples_for_ms(ms: usize, channels: usize) -> usize {
    ms * SAMPLE_RATE as usize / 1000 * channels
}

#[derive(Default)]
pub struct Counters {
    pub decoded: AtomicU64,
    pub concealed: AtomicU64,
    pub trimmed: AtomicU64,
    pub inserted: AtomicU64,
    pub underruns: AtomicU64,
    pub late: AtomicU64,
    /// Fill in microseconds of audio, sampled when the output last ran.
    pub fill_us: AtomicU64,
    /// Peak sample level (×10000) since the last [`Player::stats`].
    pub peak: AtomicU64,
    /// The same per channel (up to 7.1), to see where sound lands.
    pub channel_peaks: [AtomicU64; 8],
    /// Current buffering target, ms.
    pub target_ms: AtomicU64,
    /// Play silence (the stream keeps flowing, so unmuting is instant).
    pub muted: AtomicBool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlayerStats {
    pub decoded: u64,
    pub concealed: u64,
    pub trimmed: u64,
    pub inserted: u64,
    pub underruns: u64,
    pub late: u64,
    pub buffered_ms: f64,
    /// Loudest sample since the previous call, 0..1.
    pub peak: f32,
    /// The same per channel, in stream order.
    pub channel_peaks: [f32; 8],
    pub target_ms: u64,
}

/// The output side of the ring: platform callbacks call [`Feeder::fill`].
pub struct Feeder {
    consumer: rtrb::Consumer<f32>,
    channels: usize,
    primed: bool,
    counters: Arc<Counters>,
}

impl Feeder {
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Fill `out` (interleaved) from the ring; silence when starved.
    pub fn fill(&mut self, out: &mut [f32]) {
        let avail = self.consumer.slots();
        self.counters.fill_us.store(
            (avail / self.channels) as u64 * 1_000_000 / SAMPLE_RATE as u64,
            Ordering::Relaxed,
        );
        if !self.primed {
            let prime = samples_for_ms(
                self.counters.target_ms.load(Ordering::Relaxed) as usize,
                self.channels,
            );
            if avail < prime {
                out.fill(0.0);
                return;
            }
            self.primed = true;
        }
        let n = avail.min(out.len());
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out[..a.len()].copy_from_slice(a);
            out[a.len()..a.len() + b.len()].copy_from_slice(b);
            chunk.commit_all();
        }
        if n < out.len() {
            out[n..].fill(0.0);
            self.primed = false;
            self.counters.underruns.fetch_add(1, Ordering::Relaxed);
        }
        if self.counters.muted.load(Ordering::Relaxed) {
            out.fill(0.0);
        }
    }
}

/// Something playing a [`Feeder`]; playback stops when it is dropped.
pub trait Output: Send {}

pub struct Player {
    tx: Sender<AudioPacket>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
}

impl Player {
    /// Start playing `channels`-channel audio through the output `open`
    /// makes from a [`Feeder`].
    pub fn start(
        channels: u8,
        open: impl FnOnce(Feeder) -> Result<Box<dyn Output>, String>,
    ) -> Result<Player, String> {
        let ch = channels as usize;
        let decoder = Decoder::new(channels)?;
        let (producer, consumer) = rtrb::RingBuffer::new(samples_for_ms(RING_MS, ch));
        let counters = Arc::new(Counters::default());
        counters.target_ms.store(TARGET_MIN_MS, Ordering::Relaxed);
        let feeder = Feeder {
            consumer,
            channels: ch,
            primed: false,
            counters: counters.clone(),
        };
        let output = open(feeder)?;
        let (tx, rx) = crossbeam_channel::bounded::<AudioPacket>(256);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, counters) = (stop.clone(), counters.clone());
            std::thread::Builder::new()
                .name("ping-audio".into())
                .spawn(move || decode_loop(rx, decoder, producer, output, stop, counters))
                .map_err(|e| e.to_string())?
        };
        Ok(Player {
            tx,
            stop,
            thread: Some(thread),
            counters,
        })
    }

    pub fn set_muted(&self, muted: bool) {
        self.counters.muted.store(muted, Ordering::Relaxed);
    }

    /// Hand over a packet from the network (never blocks).
    pub fn push(&self, packet: AudioPacket) {
        let _ = self.tx.try_send(packet);
    }

    pub fn stats(&self) -> PlayerStats {
        let c = &self.counters;
        PlayerStats {
            decoded: c.decoded.load(Ordering::Relaxed),
            concealed: c.concealed.load(Ordering::Relaxed),
            trimmed: c.trimmed.load(Ordering::Relaxed),
            inserted: c.inserted.load(Ordering::Relaxed),
            underruns: c.underruns.load(Ordering::Relaxed),
            late: c.late.load(Ordering::Relaxed),
            buffered_ms: c.fill_us.load(Ordering::Relaxed) as f64 / 1000.0,
            peak: c.peak.swap(0, Ordering::Relaxed) as f32 / 10_000.0,
            channel_peaks: std::array::from_fn(|i| {
                c.channel_peaks[i].swap(0, Ordering::Relaxed) as f32 / 10_000.0
            }),
            target_ms: c.target_ms.load(Ordering::Relaxed),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn decode_loop(
    rx: Receiver<AudioPacket>,
    mut decoder: Decoder,
    mut producer: rtrb::Producer<f32>,
    output: Box<dyn Output>,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
) {
    let ch = decoder.channels();
    let capacity = producer.buffer().capacity();
    let epoch = Instant::now();
    let mut reorder = Reorder::new(GAP_WAIT_US, GAP_MAX_AHEAD);
    let mut pcm = vec![0f32; FRAME_SAMPLES * ch];
    let mut window_start = Instant::now();
    let mut window_min = usize::MAX;
    let mut trim = 0u32;
    let mut underruns_seen = 0;
    let mut warmed_up = false;
    let mut last_underrun = Instant::now();
    let mut history: std::collections::VecDeque<usize> =
        std::collections::VecDeque::with_capacity(TRIM_WINDOWS);

    let write = |producer: &mut rtrb::Producer<f32>, pcm: &[f32]| {
        if let Ok(mut chunk) = producer.write_chunk_uninit(pcm.len()) {
            let (a, b) = chunk.as_mut_slices();
            for (d, s) in a.iter_mut().chain(b.iter_mut()).zip(pcm) {
                d.write(*s);
            }
            // SAFETY: every slot of the chunk was written above.
            unsafe { chunk.commit_all() };
        }
    };

    while !stop.load(Ordering::Relaxed) {
        // Asleep until a packet comes or a gap is due to be concealed: a
        // fixed 2 ms poll cost ~6% of a core in wakeups and channel spinning
        // (packets come every 5 ms).
        let now_us = epoch.elapsed().as_micros() as u64;
        let wait = reorder.deadline_us().map_or(IDLE_WAIT, |d| {
            Duration::from_micros(d.saturating_sub(now_us))
                .clamp(Duration::from_micros(500), IDLE_WAIT)
        });
        match rx.recv_timeout(wait) {
            Ok(p) => {
                reorder.push(p.seq, p.data);
                while let Ok(p) = rx.try_recv() {
                    reorder.push(p.seq, p.data);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let now_us = epoch.elapsed().as_micros() as u64;
        while let Some(step) = reorder.pop(now_us) {
            let packet = match &step {
                Playout::Packet(d) => Some(d.as_slice()),
                Playout::Lost => None,
            };
            if decoder.decode(packet, &mut pcm).is_err() {
                pcm.fill(0.0);
            }
            let peak = pcm.iter().fold(0f32, |p, v| p.max(v.abs()));
            counters
                .peak
                .fetch_max((peak.min(1.0) * 10_000.0) as u64, Ordering::Relaxed);
            let ch = decoder.channels();
            for (c, slot) in counters.channel_peaks.iter().enumerate().take(ch) {
                let p = pcm
                    .iter()
                    .skip(c)
                    .step_by(ch)
                    .fold(0f32, |p, v| p.max(v.abs()));
                slot.fetch_max((p.min(1.0) * 10_000.0) as u64, Ordering::Relaxed);
            }
            match step {
                Playout::Packet(_) => counters.decoded.fetch_add(1, Ordering::Relaxed),
                Playout::Lost => counters.concealed.fetch_add(1, Ordering::Relaxed),
            };
            let fill = capacity - producer.slots();
            window_min = window_min.min(fill);
            if trim > 0 {
                trim -= 1;
                counters.trimmed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            write(&mut producer, &pcm);
        }
        counters.late.store(reorder.late, Ordering::Relaxed);

        if window_start.elapsed() >= WINDOW {
            let underruns = counters.underruns.load(Ordering::Relaxed);
            let starved = underruns != underruns_seen;
            underruns_seen = underruns;
            let target = counters.target_ms.load(Ordering::Relaxed);
            if starved && warmed_up {
                last_underrun = Instant::now();
                let raised = (target + TARGET_STEP_MS).min(TARGET_MAX_MS);
                if raised != target {
                    tracing::debug!(target_ms = raised, "audio underrun; buffering more");
                    counters.target_ms.store(raised, Ordering::Relaxed);
                }
            } else if target > TARGET_MIN_MS && last_underrun.elapsed() >= TARGET_RELAX {
                last_underrun = Instant::now();
                counters
                    .target_ms
                    .store(target - TARGET_STEP_MS / 2, Ordering::Relaxed);
            }
            if starved {
                history.clear();
            } else if window_min != usize::MAX {
                if history.len() == TRIM_WINDOWS {
                    history.pop_front();
                }
                history.push_back(window_min);
            }
            let lowest = if history.len() == TRIM_WINDOWS {
                history.iter().copied().min()
            } else {
                None
            };
            if let (Some(lowest), true) = (lowest, !starved && warmed_up) {
                let target = counters.target_ms.load(Ordering::Relaxed);
                let max =
                    samples_for_ms(target.saturating_sub(MARGIN_BELOW_TARGET_MS) as usize, ch);
                if lowest > max {
                    // Drop the excess, a few packets at a time.
                    trim = ((lowest - max) / pcm.len()).clamp(1, 4) as u32;
                    history.clear();
                } else if window_min < samples_for_ms(MIN_MARGIN_MS, ch)
                    && reorder.queued() == 0
                    && decoder.decode(None, &mut pcm).is_ok()
                {
                    write(&mut producer, &pcm);
                    counters.inserted.fetch_add(1, Ordering::Relaxed);
                }
            }
            warmed_up = counters.decoded.load(Ordering::Relaxed) > 0;
            window_start = Instant::now();
            window_min = usize::MAX;
        }
    }
    drop(output);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::opus::Encoder;
    use pingpong_proto::audio::{AudioDepacketizer, AudioPacketizer};
    use std::sync::Mutex;

    struct Null;
    impl Output for Null {}

    #[test]
    fn packets_become_samples_after_priming() {
        let feeder: Arc<Mutex<Option<Feeder>>> = Arc::default();
        let slot = feeder.clone();
        let player = Player::start(2, move |f| {
            *slot.lock().unwrap() = Some(f);
            Ok(Box::new(Null))
        })
        .unwrap();
        let mut enc = Encoder::new(2, 96_000).unwrap();
        let mut pack = AudioPacketizer::new();
        let mut depack = AudioDepacketizer::new();
        let mut out = Vec::new();
        let tone: Vec<f32> = (0..FRAME_SAMPLES * 2)
            .map(|i| ((i / 2) as f32 * 0.06).sin() * 0.4)
            .collect();
        let mut buf = [0u8; 1500];
        for i in 0..10 {
            let n = enc.encode(&tone, &mut buf).unwrap();
            for d in pack.push(&buf[..n], i * 5000) {
                // Lose the second packet of each block; FEC brings it back.
                if pingpong_proto::header::Header::decode(&d)
                    .unwrap()
                    .fragment_idx
                    == 1
                {
                    continue;
                }
                depack.push(&d, &mut out);
            }
        }
        for p in out.drain(..) {
            player.push(p);
        }
        std::thread::sleep(Duration::from_millis(100));
        let s = player.stats();
        // 0-8: packet 9 was lost with no parity to follow it (block incomplete).
        assert_eq!(s.decoded, 9, "{s:?}");
        let mut guard = feeder.lock().unwrap();
        let f = guard.as_mut().unwrap();
        let mut pcm = vec![1f32; 480];
        f.fill(&mut pcm);
        assert!(
            pcm.iter().any(|v| v.abs() > 0.01),
            "played audio after priming"
        );
    }
}
