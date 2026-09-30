//! Sending input: unreliable, healed by redundancy (every packet carries the
//! last eight events) and a backed-off trailing repeat, so a lost key release
//! is re-delivered without acks or retransmission timers. Relative motion is
//! batched for 1 ms, as Moonlight does; keys and buttons go out immediately.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use pingpong_proto::clock;
use pingpong_proto::input::{
    InputEvent, InputRing, MotionBatcher, RepeatSchedule, BATCH_WINDOW, MAX_EVENTS_PER_PACKET,
    MAX_INPUT_LEN,
};
use pingpong_transport::{Endpoint, Peer};

enum Msg {
    Event(InputEvent),
    /// Relative motion in (fractional) pixels: the batcher carries the residue
    /// so slow movements are not truncated away.
    Motion(f64, f64),
}

/// Gap between the packets of a burst of keys; see `run`.
const BURST_SPACING: Duration = Duration::from_millis(2);

#[derive(Clone)]
pub struct InputSender {
    tx: Sender<(Msg, u32)>,
}

impl InputSender {
    /// A sender whose events come out of the receiver (motion dropped).
    #[cfg(test)]
    pub(crate) fn for_test() -> (InputSender, crossbeam_channel::Receiver<InputEvent>) {
        let (tx, rx) = crossbeam_channel::unbounded::<(Msg, u32)>();
        let (out_tx, out_rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            for (msg, _) in rx {
                if let Msg::Event(ev) = msg {
                    let _ = out_tx.send(ev);
                }
            }
        });
        // The forwarding thread runs behind: tests read after a moment.
        (InputSender { tx }, out_rx)
    }

    pub fn send(&self, ev: InputEvent) {
        let _ = self.tx.send((Msg::Event(ev), clock::now_us()));
    }

    pub fn motion(&self, dx: f64, dy: f64) {
        let _ = self.tx.send((Msg::Motion(dx, dy), clock::now_us()));
    }

    /// Type `text` on the host (Moonlight's paste): characters as text, right
    /// whatever the host's layout; line breaks and tabs as their keys. Types
    /// at most `MAX_TYPED` characters and returns how many.
    pub fn type_text(&self, text: &str) -> usize {
        /// More is probably not something to type.
        const MAX_TYPED: usize = 4096;
        const ENTER: u16 = 0x1C;
        const TAB: u16 = 0x0F;
        let mut typed = 0;
        let mut after_cr = false;
        for c in text.chars().take(MAX_TYPED) {
            let key = match c {
                '\r' => Some(ENTER),
                '\n' if after_cr => None,
                '\n' => Some(ENTER),
                '\t' => Some(TAB),
                _ => {
                    self.send(InputEvent::Text(c));
                    None
                }
            };
            if let Some(sc) = key {
                self.send(InputEvent::KeyDown(sc));
                self.send(InputEvent::KeyUp(sc));
            }
            after_cr = c == '\r';
            typed += 1;
        }
        typed
    }
}

pub struct InputThread {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl InputThread {
    pub fn spawn(endpoint: Arc<Endpoint>, peer: Arc<Peer>) -> (InputThread, InputSender) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("ping-input".into())
                .spawn(move || run(endpoint, peer, rx, stop))
                .expect("spawning the input thread")
        };
        (InputThread { stop, handle }, InputSender { tx })
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.handle.join();
    }
}

fn run(endpoint: Arc<Endpoint>, peer: Arc<Peer>, rx: Receiver<(Msg, u32)>, stop: Arc<AtomicBool>) {
    crate::priority::latency_critical();
    let mut ring = InputRing::new();
    let mut batcher = MotionBatcher::new();
    let mut schedule = RepeatSchedule::new();
    let mut pending: Vec<InputEvent> = Vec::with_capacity(MAX_EVENTS_PER_PACKET);
    let mut wire = [0u8; MAX_INPUT_LEN];
    // Trailing repeats reuse the newest real event's timestamp: the event
    // happened when it happened.
    let mut newest_ts = 0u32;

    let send = |ring: &InputRing, ts: u32, wire: &mut [u8; MAX_INPUT_LEN]| {
        if let Some(n) = ring.encode_into(ts, wire) {
            if let Err(e) = endpoint.send(&peer, &wire[..n]) {
                tracing::debug!(error = %e, "input send");
            }
        }
    };

    while !stop.load(Ordering::Relaxed) {
        let timeout = schedule.next_delay().unwrap_or(Duration::from_millis(100));
        match rx.recv_timeout(timeout) {
            Ok((msg, ts)) => {
                newest_ts = ts;
                pending.clear();
                let ev = match msg {
                    Msg::Event(ev) => ev,
                    Msg::Motion(dx, dy) => {
                        batcher.accumulate(dx, dy);
                        InputEvent::MouseMoveRel { dx: 0, dy: 0 }
                    }
                };
                if let InputEvent::MouseMoveRel { dx, dy } = ev {
                    batcher.accumulate(dx as f64, dy as f64);
                    let deadline = Instant::now() + BATCH_WINDOW;
                    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                        match rx.recv_timeout(remaining) {
                            Ok((Msg::Motion(dx, dy), ts)) => {
                                batcher.accumulate(dx, dy);
                                newest_ts = ts;
                            }
                            Ok((Msg::Event(InputEvent::MouseMoveRel { dx, dy }), ts)) => {
                                batcher.accumulate(dx as f64, dy as f64);
                                newest_ts = ts;
                            }
                            Ok((Msg::Event(other), ts)) => {
                                batcher.drain(&mut pending);
                                pending.push(other);
                                newest_ts = ts;
                                break;
                            }
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    if pending.is_empty() {
                        batcher.drain(&mut pending);
                    }
                } else if let InputEvent::MouseMoveAbs { .. } = ev {
                    // Absolute positions supersede each other: coalesce a burst
                    // to its latest position.
                    let mut last = ev;
                    while let Ok((Msg::Event(next), ts)) = rx.try_recv() {
                        match next {
                            InputEvent::MouseMoveAbs { .. } => {
                                last = next;
                                newest_ts = ts;
                            }
                            other => {
                                pending.push(last);
                                last = other;
                                newest_ts = ts;
                            }
                        }
                    }
                    pending.push(last);
                } else {
                    pending.push(ev);
                }
                if pending.is_empty() {
                    continue;
                }
                // The ring holds eight; a burst larger than that is sent in
                // ring-sized packets so nothing is skipped.
                for chunk in pending.chunks(MAX_EVENTS_PER_PACKET) {
                    for &ev in chunk {
                        tracing::trace!(?ev, "input");
                        ring.push(ev);
                    }
                    send(&ring, newest_ts, &mut wire);
                }
                schedule.reset();
                // A burst of keys (a paste) goes out spaced: every event rides
                // in eight packets, which only helps if they are not all in
                // the one Wi-Fi frame that gets lost. Measured: 13 packets sent
                // back to back lost together, six pasted characters with them.
                let keys = pending.iter().any(|e| {
                    matches!(
                        e,
                        InputEvent::Text(_) | InputEvent::KeyDown(_) | InputEvent::KeyUp(_)
                    )
                });
                if keys && !rx.is_empty() {
                    std::thread::sleep(BURST_SPACING);
                }
            }
            // The queue drained: a trailing repeat is due. Once the backoff is
            // exhausted this keeps re-sending the ring every 100 ms, which
            // costs nothing (the host's sequence gate discards repeats) and
            // eventually heals any loss, however long.
            Err(RecvTimeoutError::Timeout) => send(&ring, newest_ts, &mut wire),
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
