//! Streaming statistics, in one-second windows: what the overlay shows and what
//! the logs record. Moonlight's overlay is the model -- frame rates at each
//! stage, network and host latency, loss, and where the time goes.

use std::collections::VecDeque;

use parking_lot::Mutex;
use pingpong_proto::control::{LossReport, SessionAck};
use pingpong_proto::video::GateStats;

#[derive(Debug, Clone, Copy, Default)]
pub struct Latency {
    pub avg_ms: f32,
    pub max_ms: f32,
}

#[derive(Default)]
struct Acc {
    sum_us: u64,
    max_us: u32,
    n: u32,
}

impl Acc {
    fn add(&mut self, us: u32) {
        self.sum_us += us as u64;
        self.max_us = self.max_us.max(us);
        self.n += 1;
    }
    fn take(&mut self) -> Latency {
        let l = if self.n == 0 {
            Latency::default()
        } else {
            Latency {
                avg_ms: self.sum_us as f32 / self.n as f32 / 1000.0,
                max_ms: self.max_us as f32 / 1000.0,
            }
        };
        *self = Acc::default();
        l
    }
}

/// One second of streaming, as the overlay shows it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Stats {
    pub width: u16,
    pub height: u16,
    pub target_fps: u32,
    pub codec: u8,
    pub bitrate_kbps: u32,
    /// Frames completed by the network / handed to the decoder / shown.
    pub received_fps: u32,
    pub decoded_fps: u32,
    pub presented_fps: u32,
    pub mbps: f32,
    pub packet_loss_pct: f32,
    /// Frames withheld from the decoder while recovering from a loss.
    pub dropped_frames: u32,
    pub lost_frames: u64,
    pub recoveries: u64,
    pub idr_requests: u64,
    pub rtt_ms: f32,
    pub host: Latency,
    /// First packet of a frame → frame complete.
    pub network: Latency,
    /// Complete → decoded.
    pub decode: Latency,
    /// Decoded → on screen (display-link wait included).
    pub render: Latency,
    /// The host picking up a captured frame → on this screen, across the two
    /// machines' clocks (see [`ClockSync`]).
    pub end_to_end: Latency,
    pub decode_errors: u32,
}

#[derive(Default)]
struct Window {
    bytes: u64,
    frames_received: u32,
    frames_decodable: u32,
    decoded: u32,
    presented: u32,
    host: Acc,
    network: Acc,
    decode: Acc,
    render: Acc,
    end_to_end: Acc,
    rtt: Acc,
    decode_errors: u32,
    started: Option<std::time::Instant>,
}

#[derive(Default)]
pub struct StatsCollector {
    window: Mutex<Window>,
    last: Mutex<Stats>,
    session: Mutex<Option<SessionAck>>,
    gate: Mutex<GateStats>,
    loss: Mutex<LossReport>,
    clock: Mutex<ClockSync>,
}

/// The host's clock against ours, from the Ping/Pong exchange: the Pong's
/// header carries the host's clock when it replied, which on a symmetric path
/// is halfway through the round trip. As NTP does, the sample with the
/// shortest round trip wins, since it has the least queueing to be lopsided.
#[derive(Default)]
struct ClockSync {
    /// (round trip, host minus ours), both µs, wrapping.
    samples: VecDeque<(u32, u32)>,
}

/// About 8 s of pings.
const CLOCK_SAMPLES: usize = 16;

impl StatsCollector {
    /// A Ping sent at `sent_us` (ours) came back at `now_us` (ours) with the
    /// host's clock at `host_us`.
    pub fn clock_sample(&self, sent_us: u32, host_us: u32, now_us: u32) {
        let rtt = now_us.wrapping_sub(sent_us);
        let offset = host_us.wrapping_sub(sent_us.wrapping_add(rtt / 2));
        let mut c = self.clock.lock();
        if c.samples.len() == CLOCK_SAMPLES {
            c.samples.pop_front();
        }
        c.samples.push_back((rtt, offset));
    }

    /// A host timestamp on our clock, once there is a sample.
    pub fn host_to_local(&self, host_us: u32) -> Option<u32> {
        let c = self.clock.lock();
        c.samples
            .iter()
            .min_by_key(|s| s.0)
            .map(|&(_, offset)| host_us.wrapping_sub(offset))
    }

    pub fn end_to_end(&self, us: u32) {
        self.window.lock().end_to_end.add(us);
    }

    pub fn set_ack(&self, ack: SessionAck) {
        *self.session.lock() = Some(ack);
    }

    pub fn packet(&self, len: usize) {
        let mut w = self.window.lock();
        w.bytes += len as u64;
        self.maybe_roll(&mut w);
    }

    pub fn frame_received(&self, decodable: bool) {
        let mut w = self.window.lock();
        w.frames_received += 1;
        if decodable {
            w.frames_decodable += 1;
        }
    }

    pub fn host_latency(&self, us: u32) {
        self.window.lock().host.add(us);
    }

    pub fn network_latency(&self, us: u32) {
        self.window.lock().network.add(us);
    }

    pub fn decoded(&self, decode_us: u32) {
        let mut w = self.window.lock();
        w.decoded += 1;
        w.decode.add(decode_us);
    }

    pub fn presented(&self, render_us: u32) {
        let mut w = self.window.lock();
        w.presented += 1;
        w.render.add(render_us);
        self.maybe_roll(&mut w);
    }

    pub fn rtt(&self, us: u32) {
        self.window.lock().rtt.add(us);
    }

    pub fn decode_error(&self) {
        self.window.lock().decode_errors += 1;
    }

    pub fn loss(&self, r: LossReport) {
        *self.loss.lock() = r;
    }

    pub fn gate(&self, g: GateStats) {
        *self.gate.lock() = g;
    }

    fn maybe_roll(&self, w: &mut Window) {
        let now = std::time::Instant::now();
        let started = *w.started.get_or_insert(now);
        let elapsed = now.duration_since(started).as_secs_f32();
        if elapsed < 1.0 {
            return;
        }
        let scale = 1.0 / elapsed;
        let ack = *self.session.lock();
        let loss = *self.loss.lock();
        let gate = *self.gate.lock();
        let mut prev = self.last.lock();
        let s = Stats {
            width: ack.map(|a| a.width).unwrap_or(0),
            height: ack.map(|a| a.height).unwrap_or(0),
            target_fps: ack.map(|a| a.refresh_mhz / 1000).unwrap_or(0),
            codec: ack.map(|a| a.codec).unwrap_or(0),
            bitrate_kbps: ack.map(|a| a.bitrate_kbps).unwrap_or(0),
            received_fps: (w.frames_received as f32 * scale).round() as u32,
            decoded_fps: (w.decoded as f32 * scale).round() as u32,
            presented_fps: (w.presented as f32 * scale).round() as u32,
            mbps: w.bytes as f32 * 8.0 / 1e6 * scale,
            packet_loss_pct: if loss.expected > 0 {
                100.0 * loss.expected.saturating_sub(loss.received) as f32 / loss.expected as f32
            } else {
                0.0
            },
            dropped_frames: w.frames_received.saturating_sub(w.frames_decodable),
            lost_frames: gate.lost,
            recoveries: gate.losses,
            idr_requests: gate.idr_requests,
            rtt_ms: {
                let r = w.rtt.take();
                if r.avg_ms > 0.0 {
                    r.avg_ms
                } else {
                    prev.rtt_ms
                }
            },
            host: w.host.take(),
            network: w.network.take(),
            decode: w.decode.take(),
            render: w.render.take(),
            end_to_end: w.end_to_end.take(),
            decode_errors: w.decode_errors,
        };
        tracing::debug!(
            target: "ping_core::stats",
            received_fps = s.received_fps,
            decoded_fps = s.decoded_fps,
            presented_fps = s.presented_fps,
            mbps = format_args!("{:.1}", s.mbps),
            loss_pct = format_args!("{:.2}", s.packet_loss_pct),
            lost_frames = s.lost_frames,
            recoveries = s.recoveries,
            rtt_ms = format_args!("{:.1}", s.rtt_ms),
            host_ms = format_args!("{:.1}", s.host.avg_ms),
            network_ms = format_args!("{:.1}", s.network.avg_ms),
            decode_ms = format_args!("{:.2}", s.decode.avg_ms),
            render_ms = format_args!("{:.1}", s.render.avg_ms),
            end_to_end_ms = format_args!("{:.1}", s.end_to_end.avg_ms),
            "second"
        );
        *prev = s;
        *w = Window {
            started: Some(now),
            ..Window::default()
        };
    }

    /// The last complete one-second window.
    pub fn snapshot(&self) -> Stats {
        let mut w = self.window.lock();
        self.maybe_roll(&mut w);
        drop(w);
        *self.last.lock()
    }
}

impl Stats {
    pub fn codec_name(&self) -> &'static str {
        match self.codec {
            1 => "H.264",
            2 => "HEVC",
            4 => "AV1",
            _ => "?",
        }
    }

    /// The overlay text, Moonlight-style.
    pub fn overlay_text(&self) -> String {
        format!(
            "Video stream: {}x{} {:.2} FPS ({})\n\
                Incoming frame rate from network: {} FPS\n\
                Decoding frame rate: {} FPS\n\
                Rendering frame rate: {} FPS\n\
                Bitrate: {:.1} Mbps (target {:.0})\n\
                Frames dropped by network: {:.2}% packets, {} frames lost, {} recoveries\n\
                Average network latency: {:.1} ms (RTT)\n\
                Host processing latency: {:.1} ms avg / {:.1} ms max\n\
                Network + reassembly: {:.1} ms avg\n\
                Average decoding time: {:.2} ms\n\
                Average rendering time (incl. V-sync): {:.2} ms\n\
                Host capture to screen: {:.1} ms avg / {:.1} ms max",
            self.width,
            self.height,
            self.target_fps as f32,
            self.codec_name(),
            self.received_fps,
            self.decoded_fps,
            self.presented_fps,
            self.mbps,
            self.bitrate_kbps as f32 / 1000.0,
            self.packet_loss_pct,
            self.lost_frames,
            self.recoveries,
            self.rtt_ms,
            self.host.avg_ms,
            self.host.max_ms,
            self.network.avg_ms,
            self.decode.avg_ms,
            self.render.avg_ms,
            self.end_to_end.avg_ms,
            self.end_to_end.max_ms,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::StatsCollector;

    #[test]
    fn the_host_clock_comes_from_the_quickest_round_trip() {
        let s = StatsCollector::default();
        assert_eq!(s.host_to_local(5), None);
        // The host's clock runs 500 ms ahead. A quick exchange: sent at 1000,
        // back at 2000, the host replied at its 501_500 (our 1500).
        s.clock_sample(1_000, 501_500, 2_000);
        // A slow one, queued on the way back: it would say 480 ms.
        s.clock_sample(10_000, 510_500, 50_000);
        assert_eq!(s.host_to_local(600_000), Some(100_000));
    }

    #[test]
    fn the_clocks_may_wrap() {
        let s = StatsCollector::default();
        let sent = u32::MAX - 400;
        // Ours wraps during the exchange; the host's is behind ours.
        s.clock_sample(
            sent,
            sent.wrapping_sub(7_000).wrapping_add(500),
            sent.wrapping_add(1_000),
        );
        let captured_host = sent.wrapping_sub(7_000).wrapping_add(600);
        assert_eq!(s.host_to_local(captured_host), Some(sent.wrapping_add(600)));
    }
}
