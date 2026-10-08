//! The console's video from RTP packets to whole H.264 frames, and what is
//! done when a packet is missing.
//!
//! A console sends H.264 over RTP (RFC 6184, packetization mode 1) and
//! sends again what the client's NACKs ask for (RTX, which str0m unwraps
//! into the packet it repairs). A frame is handed on the moment its last
//! packet is in. When one is missing:
//!
//! - **It is waited for**, as long as a retransmission takes on this link:
//!   the frames behind it wait too, since each P-frame needs the one
//!   before. Every missing packet that turns up is timed, from when a later
//!   one showed it missing to its arrival, and the wait is that time as TCP
//!   estimates a retransmission timeout from round trips (RFC 6298: the
//!   smoothed time plus four times its deviation), and room for a second
//!   ask ([`SECOND_ASK`]), at most [`MAX_WAIT`]. Packets that come after
//!   their frames were given up are timed too, so a slow link lengthens
//!   the wait.
//! - **When the wait runs out, the frames are given up**, and nothing is
//!   handed on until a keyframe, which the caller asks for: Moonlight's
//!   rule (`VideoDepacketizer.c`), where decoding past the hole smears
//!   every later frame.
//! - **A whole keyframe further on ends a wait at once**: nothing before it
//!   is needed any more.
//!
//! Chrome, under Microsoft's web client, also waits for retransmissions,
//! with fixed limits of seconds and a jitter buffer on top; Moonlight has
//! no retransmissions (its host adds FEC) and gives a frame up at once.
//! This waits only as long as a retransmission takes here. Looks
//! replaceable by str0m's own frame assembly, is not: its default holds
//! every later frame for up to two seconds behind a packet that never
//! comes (`reordering_timeout_video`), so a retransmission lost too froze
//! the picture for two seconds and then jumped. Over the mock console's
//! link with Wi-Fi's outages and 1% loss (`pingpong-xbox-mock`
//! `examples/link.rs`), the longest stop was 2.7 s with str0m's assembly
//! and 1.5 s with this one.
//!
//! It runs for every video packet, so it allocates nothing per packet: the
//! packets str0m hands over are kept as they are in a ring of slots, and
//! frames are built in one buffer that is reused.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// Packets held, by sequence number: a keyframe and the wait behind it. A
/// 1440p keyframe is some 500 packets of 1,200 bytes; 30 Mb/s over the
/// longest wait is 800 more.
const CAPACITY: usize = 2048;

/// Room in every wait for a missing packet to be asked for a second time:
/// str0m asks again 33 ms after the first NACK (vendor/str0m
/// `register_nack.rs` `NACK_RETRY`), for a retransmission lost too. Without
/// it, once retransmissions came as evenly as a first NACK sent at once
/// makes them, the wait shrank to one round trip and a lost retransmission
/// cost a keyframe: at 1% loss (mock link, 25 ms each way), 1-2 keyframes
/// asked for in 15 s and stops of 0.22 s, against none.
pub const SECOND_ASK: Duration = Duration::from_millis(40);
/// The longest: past this, asking for a keyframe (a round trip and the
/// keyframe's own time on the wire) costs no more than waiting on.
pub const MAX_WAIT: Duration = Duration::from_millis(250);
/// The wait before any retransmission has been timed: a round trip
/// between two cities over Wi-Fi, with room for the first NACK's delay.
const FIRST_WAIT: Duration = Duration::from_millis(120);

/// Keyframe starts remembered, so a wait can end on a whole keyframe
/// without searching every packet held.
const CANDIDATES: usize = 8;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const STAP_A: u8 = 24;
const FU_A: u8 = 28;
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_AUD: u8 = 9;

/// One RTP packet of the video, as str0m hands it over (sequence number
/// and time extended past their 16 and 32 bits).
#[derive(Debug, Clone)]
pub struct Packet {
    pub seq: u64,
    /// 90 kHz.
    pub time: u64,
    pub marker: bool,
    pub payload: Arc<[u8]>,
    pub arrived: Instant,
}

/// What [`Reassembler::pop`] has.
#[derive(Debug, PartialEq, Eq)]
pub enum Popped<'a> {
    Frame(Frame<'a>),
    /// Frames were given up: nothing more until a keyframe, which the
    /// caller asks for.
    Lost,
}

/// A whole frame, Annex B.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub data: &'a [u8],
    pub keyframe: bool,
    /// 90 kHz, extended.
    pub time: u64,
    /// When its first packet arrived.
    pub arrived: Instant,
}

/// Counts since the stream began.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub packets: u64,
    pub bytes: u64,
    /// Packets found missing (a later one came first).
    pub missing: u64,
    /// Missing packets that came while their frame was still waited for.
    pub recovered: u64,
    /// Missing packets that came after their frames were given up.
    pub too_late: u64,
    /// Times frames were given up for a keyframe.
    pub given_up: u64,
    pub frames: u64,
    pub keyframes: u64,
    /// Frames passed over: given up, or skipped for a keyframe.
    pub skipped: u64,
}

#[derive(Debug, Clone, Default)]
struct Slot {
    /// The sequence number this slot holds, or waits for.
    seq: u64,
    payload: Option<Arc<[u8]>>,
    time: u64,
    marker: bool,
    arrived: Option<Instant>,
    /// Since when it is missing: a later packet came first.
    missing_since: Option<Instant>,
}

/// Where the frame starting at a packet stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameAt {
    /// All in: these are its first and last packets.
    Complete(u64, u64),
    /// In so far, with more to come.
    Arriving,
    /// A packet is missing, since this moment.
    Gap(Instant),
}

pub struct Reassembler {
    slots: Vec<Slot>,
    /// The first packet of the next frame: the one after the last frame
    /// handed on or given up. Anything before it is behind.
    next: Option<u64>,
    highest: Option<u64>,
    /// Nothing is handed on until a keyframe: at the start, after frames
    /// were given up, after the decoder failed.
    keyframe_only: bool,
    /// Missing packets up to this one belong to frames given up.
    given_up_through: Option<u64>,
    /// A [`Popped::Lost`] waiting to be said.
    lost: bool,
    /// When the frame at `next` began waiting for a missing packet.
    waiting_since: Option<Instant>,
    /// Packets that may start a keyframe.
    candidates: Vec<u64>,
    wait: RetransmitTime,
    frame: Vec<u8>,
    stats: ReassemblyStats,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Reassembler {
        Reassembler {
            slots: vec![Slot::default(); CAPACITY],
            next: None,
            highest: None,
            keyframe_only: true,
            given_up_through: None,
            lost: false,
            waiting_since: None,
            candidates: Vec::with_capacity(CANDIDATES),
            wait: RetransmitTime::default(),
            frame: Vec::with_capacity(256 * 1024),
            stats: ReassemblyStats::default(),
        }
    }

    pub fn stats(&self) -> ReassemblyStats {
        self.stats
    }

    /// How long a missing packet is waited for now.
    pub fn wait(&self) -> Duration {
        self.wait.timeout()
    }

    /// The decoder failed: nothing more until a keyframe.
    pub fn need_keyframe(&mut self) {
        self.keyframe_only = true;
        self.waiting_since = None;
    }

    /// Whether only a keyframe is taken now.
    pub fn awaiting_keyframe(&self) -> bool {
        self.keyframe_only
    }

    /// When the newest keyframe held began to arrive, whole or not: one
    /// asked for may be on its way.
    pub fn keyframe_arriving_since(&self) -> Option<Instant> {
        self.candidates
            .iter()
            .filter_map(|&c| self.held(c)?.arrived)
            .max()
    }

    /// When [`Reassembler::pop`] gives up waiting, if nothing arrives
    /// before.
    pub fn deadline(&self) -> Option<Instant> {
        self.waiting_since.map(|t| t + self.wait.timeout())
    }

    fn index(seq: u64) -> usize {
        (seq % CAPACITY as u64) as usize
    }

    /// The packet `seq`, if it is in.
    fn held(&self, seq: u64) -> Option<&Slot> {
        let slot = &self.slots[Self::index(seq)];
        (slot.seq == seq && slot.payload.is_some()).then_some(slot)
    }

    pub fn push(&mut self, p: Packet, now: Instant) {
        let seq = p.seq;
        if let Some(highest) = self.highest {
            if seq.saturating_add(CAPACITY as u64) <= highest {
                // Older than anything held: a stray.
                return;
            }
            if seq > highest + CAPACITY as u64 {
                // A jump past everything held: what is held is of no use.
                self.restart();
            }
        }
        if let Some(next) = self.next {
            if seq >= next + CAPACITY as u64 {
                // The frame waited for would be overwritten: give it up.
                self.give_up(seq + 1 - CAPACITY as u64);
            }
        }
        match self.highest {
            Some(highest) if seq > highest => {
                for s in highest + 1..seq {
                    self.slots[Self::index(s)] = Slot {
                        seq: s,
                        missing_since: Some(now),
                        ..Slot::default()
                    };
                    self.stats.missing += 1;
                }
                self.highest = Some(seq);
            }
            None => self.highest = Some(seq),
            Some(_) => {}
        }
        let slot = &mut self.slots[Self::index(seq)];
        if slot.seq == seq && slot.payload.is_some() {
            return; // a duplicate
        }
        if slot.seq == seq {
            if let Some(since) = slot.missing_since.take() {
                self.wait.sample(now.saturating_duration_since(since));
                if self.given_up_through.is_some_and(|g| seq <= g) {
                    self.stats.too_late += 1;
                } else {
                    self.stats.recovered += 1;
                }
            }
        }
        if self.next.is_some_and(|next| seq < next) {
            return; // behind: its frame was handed on or given up
        }
        self.stats.packets += 1;
        self.stats.bytes += p.payload.len() as u64;
        if starts_keyframe(&p.payload) {
            if self.candidates.len() == CANDIDATES {
                self.candidates.remove(0);
            }
            self.candidates.push(seq);
        }
        self.slots[Self::index(seq)] = Slot {
            seq,
            payload: Some(p.payload),
            time: p.time,
            marker: p.marker,
            arrived: Some(p.arrived),
            missing_since: None,
        };
    }

    /// Everything held is dropped; a keyframe starts the stream again.
    fn restart(&mut self) {
        for slot in &mut self.slots {
            *slot = Slot::default();
        }
        self.candidates.clear();
        if !self.keyframe_only {
            self.stats.given_up += 1;
            self.lost = true;
        }
        self.next = None;
        self.highest = None;
        self.keyframe_only = true;
        self.waiting_since = None;
    }

    /// Frames before `from` are given up: a keyframe is needed.
    fn give_up(&mut self, from: u64) {
        if !self.keyframe_only {
            self.stats.given_up += 1;
            self.stats.skipped += 1;
            self.lost = true;
        }
        self.keyframe_only = true;
        self.given_up_through = self.highest;
        self.waiting_since = None;
        if self.next.is_some_and(|n| n < from) {
            self.next = Some(from);
        }
    }

    /// The next frame, or that frames were given up. Call until `None`,
    /// after each [`Reassembler::push`] and at the [`deadline`].
    ///
    /// [`deadline`]: Reassembler::deadline
    pub fn pop(&mut self, now: Instant) -> Option<Popped<'_>> {
        if std::mem::take(&mut self.lost) {
            return Some(Popped::Lost);
        }
        if self.keyframe_only {
            let (start, end) = self.find_keyframe()?;
            self.keyframe_only = false;
            return Some(self.emit(start, end));
        }
        let mut start = self.next?;
        // Padding between frames (a sender's bandwidth probes) is no
        // frame's.
        while self.held(start).is_some_and(|s| s.payload_is_empty()) {
            start += 1;
        }
        self.next = Some(start);
        match self.frame_at(start) {
            FrameAt::Complete(first, last) => {
                self.waiting_since = None;
                Some(self.emit(first, last))
            }
            FrameAt::Arriving => {
                self.waiting_since = None;
                None
            }
            FrameAt::Gap(since) => {
                if let Some((first, last)) = self.find_keyframe() {
                    // Nothing before a whole keyframe is needed.
                    self.stats.skipped += 1;
                    self.waiting_since = None;
                    return Some(self.emit(first, last));
                }
                if now.saturating_duration_since(since) >= self.wait.timeout() {
                    // From here on only a keyframe is taken, wherever it
                    // starts: it may be partly in already.
                    self.give_up(start);
                    self.lost = false;
                    return Some(Popped::Lost);
                }
                self.waiting_since = Some(since);
                None
            }
        }
    }

    /// Where the frame whose first packet is `start` stands.
    fn frame_at(&self, start: u64) -> FrameAt {
        let Some(highest) = self.highest.filter(|&h| h >= start) else {
            return FrameAt::Arriving;
        };
        let Some(first) = self.held(start) else {
            return FrameAt::Gap(self.missing_since(start));
        };
        let time = first.time;
        let mut seq = start;
        loop {
            match self.held(seq) {
                // A new time without a marker before it: the frame ended.
                Some(slot) if slot.time != time => return FrameAt::Complete(start, seq - 1),
                Some(slot) if slot.marker => return FrameAt::Complete(start, seq),
                Some(_) if seq == highest => return FrameAt::Arriving,
                Some(_) => seq += 1,
                None => return FrameAt::Gap(self.missing_since(seq)),
            }
        }
    }

    fn missing_since(&self, seq: u64) -> Instant {
        let slot = &self.slots[Self::index(seq)];
        let since = (slot.seq == seq).then_some(slot.missing_since).flatten();
        // Every packet up to the highest is either in or marked missing.
        // A slot reused meanwhile lost its packet for good: waited for
        // since long enough ago that the wait is over at once (a moving
        // "now" would keep the wait from ever ending).
        since.unwrap_or_else(|| {
            let now = Instant::now();
            now.checked_sub(MAX_WAIT).unwrap_or(now)
        })
    }

    /// The first whole keyframe held at or after `next`.
    fn find_keyframe(&mut self) -> Option<(u64, u64)> {
        let next = self.next;
        let slots = &self.slots;
        self.candidates.retain(|&c| {
            let slot = &slots[Self::index(c)];
            slot.seq == c && slot.payload.is_some() && next.is_none_or(|n| c >= n)
        });
        self.candidates.sort_unstable();
        self.candidates
            .iter()
            .find_map(|&c| match self.frame_at(c) {
                FrameAt::Complete(first, last) if self.has_idr(first, last) => Some((first, last)),
                _ => None,
            })
    }

    fn has_idr(&self, first: u64, last: u64) -> bool {
        (first..=last).any(|s| {
            self.held(s)
                .and_then(|slot| slot.payload.as_deref())
                .is_some_and(has_idr)
        })
    }

    /// Build the frame `first..=last` and move past it.
    fn emit(&mut self, first: u64, last: u64) -> Popped<'_> {
        self.frame.clear();
        let mut keyframe = false;
        let mut arrived: Option<Instant> = None;
        let mut time = 0;
        let mut whole = true;
        for s in first..=last {
            let slot = &self.slots[Self::index(s)];
            let (Some(payload), true) = (slot.payload.as_deref(), slot.seq == s) else {
                whole = false;
                break;
            };
            time = slot.time;
            if payload.is_empty() {
                continue; // padding
            }
            arrived = match (arrived, slot.arrived) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            match append_annexb(payload, &mut self.frame) {
                Some(idr) => keyframe |= idr,
                None => {
                    whole = false;
                    break;
                }
            }
        }
        self.next = Some(last + 1);
        self.candidates.retain(|&c| c > last);
        if !whole {
            // A packet that is not H.264 as offered: as if lost.
            self.give_up(last + 1);
            self.lost = false;
            return Popped::Lost;
        }
        self.stats.frames += 1;
        self.stats.keyframes += keyframe as u64;
        Popped::Frame(Frame {
            data: &self.frame,
            keyframe,
            time,
            arrived: arrived.unwrap_or_else(Instant::now),
        })
    }
}

impl Slot {
    fn payload_is_empty(&self) -> bool {
        self.payload.as_deref().is_none_or(<[u8]>::is_empty)
    }
}

/// How long a missing packet takes to arrive, estimated as TCP estimates a
/// round trip (RFC 6298, §2): a smoothed time and its mean deviation.
#[derive(Debug, Clone, Copy, Default)]
struct RetransmitTime {
    smoothed: Option<Duration>,
    deviation: Duration,
}

impl RetransmitTime {
    fn sample(&mut self, d: Duration) {
        match self.smoothed {
            None => {
                self.smoothed = Some(d);
                self.deviation = d / 2;
            }
            Some(s) => {
                let diff = s.abs_diff(d);
                self.deviation = self.deviation * 3 / 4 + diff / 4;
                self.smoothed = Some(s * 7 / 8 + d / 8);
            }
        }
    }

    fn timeout(&self) -> Duration {
        match self.smoothed {
            None => FIRST_WAIT,
            Some(s) => (s + self.deviation * 4 + SECOND_ASK).min(MAX_WAIT),
        }
    }
}

/// Append a packet's NAL units to `out`, Annex B; whether one is an IDR
/// slice. `None` for a packet that is not H.264 in packetization mode 1.
fn append_annexb(payload: &[u8], out: &mut Vec<u8>) -> Option<bool> {
    let (&header, rest) = payload.split_first()?;
    match header & 0x1f {
        t @ 1..=23 => {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(payload);
            Some(t == NAL_IDR)
        }
        STAP_A => {
            let mut idr = false;
            let mut p = rest;
            while !p.is_empty() {
                let len = u16::from_be_bytes([*p.first()?, *p.get(1)?]) as usize;
                let nal = p.get(2..2 + len).filter(|n| !n.is_empty())?;
                idr |= nal[0] & 0x1f == NAL_IDR;
                out.extend_from_slice(&START_CODE);
                out.extend_from_slice(nal);
                p = &p[2 + len..];
            }
            Some(idr)
        }
        FU_A => {
            let (&fu, data) = rest.split_first()?;
            let t = fu & 0x1f;
            if fu & 0x80 != 0 {
                out.extend_from_slice(&START_CODE);
                out.push((header & 0xe0) | t);
            }
            out.extend_from_slice(data);
            Some(t == NAL_IDR)
        }
        // STAP-B, MTAP and FU-B belong to the interleaved mode, which is
        // not offered.
        _ => None,
    }
}

/// Whether a packet carries (part of) an IDR slice.
fn has_idr(payload: &[u8]) -> bool {
    any_nal(payload, |t| t == NAL_IDR)
}

/// Whether a packet can be a keyframe's first: it begins with the
/// parameter sets or an access unit delimiter, or begins an IDR slice.
fn starts_keyframe(payload: &[u8]) -> bool {
    let first = match payload.first().map(|h| h & 0x1f) {
        Some(t @ 1..=23) => Some(t),
        Some(STAP_A) => payload.get(3).map(|b| b & 0x1f),
        Some(FU_A) => payload
            .get(1)
            .filter(|fu| *fu & 0x80 != 0)
            .map(|fu| fu & 0x1f),
        _ => None,
    };
    matches!(first, Some(NAL_SPS | NAL_AUD | NAL_IDR))
}

/// Whether any NAL unit a packet carries or begins has a type `f` accepts.
fn any_nal(payload: &[u8], f: impl Fn(u8) -> bool) -> bool {
    let Some((&header, rest)) = payload.split_first() else {
        return false;
    };
    match header & 0x1f {
        t @ 1..=23 => f(t),
        STAP_A => {
            let mut p = rest;
            while p.len() > 2 {
                let len = u16::from_be_bytes([p[0], p[1]]) as usize;
                let Some(nal) = p.get(2..2 + len).filter(|n| !n.is_empty()) else {
                    return false;
                };
                if f(nal[0] & 0x1f) {
                    return true;
                }
                p = &p[2 + len..];
            }
            false
        }
        FU_A => rest.first().is_some_and(|fu| f(fu & 0x1f)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: &[u8] = &[0x67, 0x42, 0xe0, 0x1f];
    const PPS: &[u8] = &[0x68, 0xce, 0x3c, 0x80];

    /// Packets for one frame from `seq` on: a keyframe is the parameter
    /// sets in a STAP-A, then an IDR slice in three FU-A fragments; a
    /// P-frame one slice in two.
    fn frame(seq: u64, n: u64, key: bool) -> Vec<Packet> {
        let time = n * 1500;
        let at = Instant::now();
        let mut payloads: Vec<Vec<u8>> = Vec::new();
        let (nal, pieces): (u8, usize) = if key { (0x65, 3) } else { (0x41, 2) };
        if key {
            let mut stap = vec![STAP_A | 0x60];
            for nal in [SPS, PPS] {
                stap.extend_from_slice(&(nal.len() as u16).to_be_bytes());
                stap.extend_from_slice(nal);
            }
            payloads.push(stap);
        }
        for i in 0..pieces {
            let s = if i == 0 { 0x80 } else { 0 };
            let e = if i == pieces - 1 { 0x40 } else { 0 };
            payloads.push(vec![
                FU_A | (nal & 0xe0),
                s | e | (nal & 0x1f),
                n as u8,
                i as u8,
            ]);
        }
        let last = payloads.len() - 1;
        payloads
            .into_iter()
            .enumerate()
            .map(|(i, p)| Packet {
                seq: seq + i as u64,
                time,
                marker: i == last,
                payload: Arc::from(p),
                arrived: at,
            })
            .collect()
    }

    /// A stream: a keyframe, then P-frames (`key` says which others are
    /// keyframes), packet by packet.
    fn stream(frames: u64, key: impl Fn(u64) -> bool) -> Vec<Packet> {
        let mut seq = 1000;
        let mut out = Vec::new();
        for n in 0..frames {
            let f = frame(seq, n, n == 0 || key(n));
            seq += f.len() as u64;
            out.extend(f);
        }
        out
    }

    /// Everything popped: frame numbers (from the RTP time) and losses.
    fn drain(r: &mut Reassembler, now: Instant, out: &mut Vec<Option<u64>>) {
        while let Some(p) = r.pop(now) {
            out.push(match p {
                Popped::Frame(f) => {
                    assert!(f.data.starts_with(&START_CODE));
                    Some(f.time / 1500)
                }
                Popped::Lost => None,
            });
        }
    }

    #[test]
    fn frames_come_out_whole_and_in_order() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut out = Vec::new();
        for p in stream(5, |_| false) {
            r.push(p, now);
            drain(&mut r, now, &mut out);
        }
        assert_eq!(out, [Some(0), Some(1), Some(2), Some(3), Some(4)]);
        assert_eq!(r.stats().keyframes, 1);
        assert_eq!(r.stats().missing, 0);
    }

    #[test]
    fn a_keyframe_is_annex_b_with_its_parameter_sets() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        for p in frame(7, 0, true) {
            r.push(p, now);
        }
        let Some(Popped::Frame(f)) = r.pop(now) else {
            panic!("no frame");
        };
        assert!(f.keyframe);
        let mut want = Vec::new();
        for nal in [SPS, PPS] {
            want.extend_from_slice(&START_CODE);
            want.extend_from_slice(nal);
        }
        want.extend_from_slice(&START_CODE);
        want.extend_from_slice(&[0x65, 0, 0, 0, 1, 0, 2]);
        assert_eq!(f.data, want);
    }

    #[test]
    fn nothing_comes_out_before_the_first_keyframe() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut out = Vec::new();
        // Joined mid-stream: P-frames, then a keyframe.
        for p in stream(6, |n| n == 3).into_iter().skip(5) {
            r.push(p, now);
            drain(&mut r, now, &mut out);
        }
        assert_eq!(out, [Some(3), Some(4), Some(5)]);
    }

    #[test]
    fn a_reordered_packet_is_waited_for_and_nothing_is_lost() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut out = Vec::new();
        let mut packets = stream(4, |_| false);
        // The second P-frame's first packet comes after its second.
        packets.swap(6, 7);
        for p in packets {
            r.push(p, now);
            drain(&mut r, now, &mut out);
        }
        assert_eq!(out, [Some(0), Some(1), Some(2), Some(3)]);
        assert_eq!(r.stats().recovered, 1);
    }

    #[test]
    fn a_retransmission_in_time_completes_the_frames_waiting_behind_it() {
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        let mut out = Vec::new();
        let packets = stream(6, |_| false);
        let lost = packets[6].clone();
        for p in packets.into_iter().filter(|p| p.seq != lost.seq) {
            r.push(p, t0);
            drain(&mut r, t0, &mut out);
        }
        // Frames 2..5 wait behind frame 2's missing packet.
        assert_eq!(out, [Some(0), Some(1)]);
        let deadline = r.deadline().expect("waiting");
        assert_eq!(deadline, t0 + FIRST_WAIT);
        let back = t0 + Duration::from_millis(40);
        r.push(lost, back);
        drain(&mut r, back, &mut out);
        assert_eq!(out, [Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)]);
        assert_eq!(r.deadline(), None);
        assert_eq!(r.stats().recovered, 1);
        assert_eq!(r.stats().given_up, 0);
    }

    #[test]
    fn a_packet_that_never_comes_costs_the_wait_then_a_keyframe() {
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        let mut out = Vec::new();
        let packets = stream(8, |n| n == 6);
        let lost = packets[6].seq;
        for p in packets.iter().filter(|p| p.seq != lost).take(10) {
            r.push(p.clone(), t0);
            drain(&mut r, t0, &mut out);
        }
        assert_eq!(out, [Some(0), Some(1)]);
        // Still waiting just before the deadline; given up at it.
        let deadline = r.deadline().unwrap();
        drain(&mut r, deadline - Duration::from_millis(1), &mut out);
        assert_eq!(out, [Some(0), Some(1)]);
        drain(&mut r, deadline, &mut out);
        assert_eq!(out, [Some(0), Some(1), None]);
        assert!(r.awaiting_keyframe());
        // P-frames are not handed on; the keyframe is, and what follows.
        for p in packets.iter().filter(|p| p.seq != lost).skip(10) {
            r.push(p.clone(), deadline);
            drain(&mut r, deadline, &mut out);
        }
        assert_eq!(out, [Some(0), Some(1), None, Some(6), Some(7)]);
        assert_eq!(r.stats().given_up, 1);
    }

    #[test]
    fn a_whole_keyframe_ends_a_wait_at_once() {
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        let mut out = Vec::new();
        let packets = stream(5, |n| n == 3);
        let lost = packets[6].seq;
        for p in packets.into_iter().filter(|p| p.seq != lost) {
            r.push(p, t0);
            drain(&mut r, t0, &mut out);
        }
        // No time passed: frame 2 is skipped for keyframe 3 at once.
        assert_eq!(out, [Some(0), Some(1), Some(3), Some(4)]);
        assert_eq!(r.stats().given_up, 0);
        assert_eq!(r.stats().skipped, 1);
    }

    #[test]
    fn the_wait_follows_how_long_retransmissions_take() {
        let mut w = RetransmitTime::default();
        assert_eq!(w.timeout(), FIRST_WAIT);
        for _ in 0..50 {
            w.sample(Duration::from_millis(30));
        }
        // A round trip, and a second ask's.
        let steady = w.timeout();
        assert!(steady >= Duration::from_millis(70) && steady < Duration::from_millis(80));
        // Slow ones lengthen it, up to the limit.
        for _ in 0..50 {
            w.sample(Duration::from_millis(400));
        }
        assert_eq!(w.timeout(), MAX_WAIT);
        // A LAN's fast ones shorten it, to a second ask's time.
        for _ in 0..200 {
            w.sample(Duration::from_millis(1));
        }
        let lan = w.timeout();
        assert!(lan >= SECOND_ASK && lan < SECOND_ASK + Duration::from_millis(3));
    }

    #[test]
    fn a_retransmission_after_the_frames_were_given_up_is_counted_late() {
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        let mut out = Vec::new();
        let packets = stream(4, |_| false);
        let lost = packets[6].clone();
        for p in packets.into_iter().filter(|p| p.seq != lost.seq) {
            r.push(p, t0);
            drain(&mut r, t0, &mut out);
        }
        let late = t0 + Duration::from_millis(500);
        drain(&mut r, late, &mut out);
        assert_eq!(out.last(), Some(&None));
        r.push(lost, late);
        drain(&mut r, late, &mut out);
        assert_eq!(r.stats().too_late, 1);
        assert_eq!(r.stats().recovered, 0);
        // The late one taught the wait that this link is slow.
        assert!(r.wait() > FIRST_WAIT);
    }

    #[test]
    fn a_keyframe_on_its_way_is_seen_before_it_is_whole() {
        let mut r = Reassembler::new();
        let t0 = Instant::now();
        assert_eq!(r.keyframe_arriving_since(), None);
        let mut packets = frame(10, 0, true);
        packets[0].arrived = t0 + Duration::from_millis(5);
        r.push(packets.remove(0), t0);
        assert_eq!(
            r.keyframe_arriving_since(),
            Some(t0 + Duration::from_millis(5))
        );
        assert!(r.pop(t0).is_none());
    }

    #[test]
    fn padding_between_frames_is_passed_over() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut out = Vec::new();
        let mut packets = frame(10, 0, true);
        packets.push(Packet {
            seq: 14,
            time: 0,
            marker: false,
            payload: Arc::from(Vec::new()),
            arrived: now,
        });
        packets.extend(frame(15, 1, false));
        for p in packets {
            r.push(p, now);
            drain(&mut r, now, &mut out);
        }
        assert_eq!(out, [Some(0), Some(1)]);
        assert_eq!(r.stats().missing, 0);
    }

    #[test]
    fn a_frame_without_a_marker_ends_where_the_next_begins() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut out = Vec::new();
        let mut packets = stream(3, |_| false);
        for p in &mut packets {
            p.marker = false;
        }
        for p in packets {
            r.push(p, now);
            drain(&mut r, now, &mut out);
        }
        // The last frame waits for what follows it.
        assert_eq!(out, [Some(0), Some(1)]);
    }

    #[test]
    fn a_jump_in_sequence_numbers_starts_again_at_a_keyframe() {
        let mut r = Reassembler::new();
        let now = Instant::now();
        let mut out = Vec::new();
        for p in frame(10, 0, true) {
            r.push(p, now);
        }
        drain(&mut r, now, &mut out);
        for p in frame(10 + 10 * CAPACITY as u64, 1, false) {
            r.push(p, now);
            drain(&mut r, now, &mut out);
        }
        for p in frame(20 + 10 * CAPACITY as u64, 2, true) {
            r.push(p, now);
            drain(&mut r, now, &mut out);
        }
        assert_eq!(out, [Some(0), None, Some(2)]);
    }

    #[test]
    fn hostile_payloads_do_not_panic() {
        let mut out = Vec::new();
        for p in [
            &[][..],
            &[STAP_A],
            &[STAP_A, 0],
            &[STAP_A, 0, 9, 1],
            &[STAP_A, 0, 0],
            &[FU_A],
            &[25, 1, 2],
            &[29],
            &[0x7c, 0x85],
        ] {
            let _ = append_annexb(p, &mut out);
            let _ = has_idr(p);
            let _ = starts_keyframe(p);
        }
        let mut r = Reassembler::new();
        let now = Instant::now();
        for (i, p) in [&[STAP_A, 0, 9][..], &[0x65], &[FU_A, 0x45]]
            .iter()
            .enumerate()
        {
            r.push(
                Packet {
                    seq: i as u64,
                    time: 0,
                    marker: i == 2,
                    payload: Arc::from(p.to_vec()),
                    arrived: now,
                },
                now,
            );
            while r.pop(now).is_some() {}
        }
    }
}
