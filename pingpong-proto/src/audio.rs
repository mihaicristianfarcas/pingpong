//! Audio on the wire, the way Sunshine sends it to Moonlight: one Opus packet
//! (5 ms) per datagram, sent the moment it is encoded, and after every block
//! of four, two Reed-Solomon parity datagrams (Sunshine's RTPA_DATA_SHARDS 4,
//! RTPA_FEC_SHARDS 2). Any four of a block's six datagrams recover all four
//! packets; a lost packet is recovered at most one block (20 ms) late, which
//! the client's playout buffer absorbs.
//!
//! Header use (kind = Audio):
//!
//! ```text
//! frame_id       data: the packet's sequence number · parity: the block's first
//! fragment_idx   shard index in the block: 0-3 data, 4-5 parity
//! data/parity    4 / 2
//! frame_len      payload bytes in this datagram
//! capture_ts_us  when the packet's first sample was captured (host clock)
//! ```
//!
//! A data payload is `len: u16 LE` + the Opus packet. Parity is computed over
//! the block's data payloads zero-padded to a common, even length.

use std::collections::VecDeque;

use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};

use crate::header::{Header, Kind};
use crate::HEADER_LEN;

pub const DATA_SHARDS: usize = 4;
pub const PARITY_SHARDS: usize = 2;
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per channel in one packet (5 ms at 48 kHz).
pub const FRAME_SAMPLES: usize = 240;
/// Largest Opus packet we send. 7.1 at Sunshine's high-quality rate
/// (2048 kbit/s) is 1280 bytes per 5 ms; stereo at 96 kbit/s is 60.
pub const MAX_PACKET: usize = 1100;

fn shard_len(max_payload: usize) -> usize {
    (2 + max_payload).next_multiple_of(2)
}

fn datagram(h: Header, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; HEADER_LEN + payload.len()];
    let mut hdr = [0u8; HEADER_LEN];
    Header {
        total_len: out.len() as u16,
        frame_len: payload.len() as u32,
        ..h
    }
    .encode(&mut hdr)
    .expect("audio frame_len fits");
    out[..HEADER_LEN].copy_from_slice(&hdr);
    out[HEADER_LEN..].copy_from_slice(payload);
    out
}

fn header(frame_id: u32, fragment_idx: u16, capture_ts_us: u32) -> Header {
    Header {
        keyframe: false,
        recovery: false,
        lan_shards: false,
        frame_end: false,
        kind: Kind::Audio,
        total_len: 0,
        fragment_idx,
        data_shards: DATA_SHARDS as u8,
        parity_shards: PARITY_SHARDS as u8,
        fec_block_idx: 0,
        frame_len: 0,
        capture_ts_us,
        frame_id,
    }
}

/// Host side: Opus packets in, datagrams out.
pub struct AudioPacketizer {
    seq: u32,
    block: Vec<Vec<u8>>,
    block_ts: u32,
}

impl Default for AudioPacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioPacketizer {
    pub fn new() -> Self {
        AudioPacketizer {
            seq: 0,
            block: Vec::with_capacity(DATA_SHARDS),
            block_ts: 0,
        }
    }

    /// The datagrams to send for one Opus packet: its own, plus the block's
    /// parity when it completes a block.
    pub fn push(&mut self, opus: &[u8], capture_ts_us: u32) -> Vec<Vec<u8>> {
        let opus = &opus[..opus.len().min(MAX_PACKET)];
        let mut payload = Vec::with_capacity(2 + opus.len());
        payload.extend_from_slice(&(opus.len() as u16).to_le_bytes());
        payload.extend_from_slice(opus);

        let index = (self.seq % DATA_SHARDS as u32) as usize;
        if index == 0 {
            self.block.clear();
            self.block_ts = capture_ts_us;
        }
        let mut out = vec![datagram(
            header(self.seq, index as u16, capture_ts_us),
            &payload,
        )];
        self.block.push(payload);
        self.seq = self.seq.wrapping_add(1);

        if self.block.len() == DATA_SHARDS {
            let base = self.seq.wrapping_sub(DATA_SHARDS as u32);
            let len = shard_len(self.block.iter().map(|p| p.len() - 2).max().unwrap_or(0));
            if let Ok(mut enc) = ReedSolomonEncoder::new(DATA_SHARDS, PARITY_SHARDS, len) {
                let mut padded = vec![0u8; len];
                let mut ok = true;
                for p in &self.block {
                    padded.fill(0);
                    padded[..p.len()].copy_from_slice(p);
                    ok &= enc.add_original_shard(&padded).is_ok();
                }
                if ok {
                    if let Ok(result) = enc.encode() {
                        for (j, parity) in result.recovery_iter().enumerate() {
                            let h = header(base, (DATA_SHARDS + j) as u16, self.block_ts);
                            out.push(datagram(h, parity));
                        }
                    }
                }
            }
            self.block.clear();
        }
        out
    }
}

/// One Opus packet out of the depacketizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub seq: u32,
    pub capture_ts_us: u32,
    /// Came from parity rather than its own datagram.
    pub recovered: bool,
    pub data: Vec<u8>,
}

struct Block {
    base: u32,
    ts: u32,
    shards: [Option<Vec<u8>>; DATA_SHARDS + PARITY_SHARDS],
    delivered: [bool; DATA_SHARDS],
}

/// Client side: datagrams in, Opus packets out as soon as each is available
/// (in arrival order; [`crate::audio::Reorder`] puts them back in sequence).
pub struct AudioDepacketizer {
    blocks: VecDeque<Block>,
    pub recovered: u64,
}

const TRACKED_BLOCKS: usize = 8;
/// A sequence number this far from the current one (5 s of packets) is a new
/// stream, not a straggler.
const RESTART_GAP: i32 = 1000;

impl Default for AudioDepacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioDepacketizer {
    pub fn new() -> Self {
        AudioDepacketizer {
            blocks: VecDeque::with_capacity(TRACKED_BLOCKS),
            recovered: 0,
        }
    }

    pub fn push(&mut self, datagram: &[u8], out: &mut Vec<AudioPacket>) {
        let Ok(h) = Header::decode(datagram) else {
            return;
        };
        if h.kind != Kind::Audio
            || h.data_shards as usize != DATA_SHARDS
            || h.parity_shards as usize != PARITY_SHARDS
            || h.fragment_idx as usize >= DATA_SHARDS + PARITY_SHARDS
        {
            return;
        }
        let payload = &datagram[HEADER_LEN..h.total_len as usize];
        let index = h.fragment_idx as usize;
        let base = if index < DATA_SHARDS {
            h.frame_id.wrapping_sub(index as u32)
        } else {
            h.frame_id
        };

        let block = match self.blocks.iter().position(|b| b.base == base) {
            Some(i) => &mut self.blocks[i],
            None => {
                // Ignore stragglers from blocks we have already let go of;
                // something far behind means the host restarted its count.
                if let Some(newest) = self.blocks.back() {
                    let behind = newest.base.wrapping_sub(base) as i32;
                    if behind > RESTART_GAP {
                        self.blocks.clear();
                    } else if behind > 0 && behind as usize >= TRACKED_BLOCKS * DATA_SHARDS {
                        return;
                    }
                }
                if self.blocks.len() == TRACKED_BLOCKS {
                    self.blocks.pop_front();
                }
                self.blocks.push_back(Block {
                    base,
                    ts: h.capture_ts_us,
                    shards: Default::default(),
                    delivered: [false; DATA_SHARDS],
                });
                self.blocks.back_mut().expect("just pushed")
            }
        };
        if block.shards[index].is_some() {
            return;
        }
        block.shards[index] = Some(payload.to_vec());

        if index < DATA_SHARDS {
            if let Some(p) = parse(payload) {
                block.delivered[index] = true;
                out.push(AudioPacket {
                    seq: base.wrapping_add(index as u32),
                    capture_ts_us: h.capture_ts_us,
                    recovered: false,
                    data: p.to_vec(),
                });
            }
        }
        self.recovered += Self::recover(block, out);
    }

    fn recover(block: &mut Block, out: &mut Vec<AudioPacket>) -> u64 {
        let missing = block.delivered.iter().filter(|d| !**d).count();
        let have = block.shards.iter().filter(|s| s.is_some()).count();
        if missing == 0 || have < DATA_SHARDS {
            return 0;
        }
        let Some(len) = block.shards[DATA_SHARDS..]
            .iter()
            .flatten()
            .map(Vec::len)
            .next()
        else {
            return 0;
        };
        if len == 0 || len % 2 != 0 {
            return 0;
        }
        let Ok(mut dec) = ReedSolomonDecoder::new(DATA_SHARDS, PARITY_SHARDS, len) else {
            return 0;
        };
        let mut padded = vec![0u8; len];
        for (i, s) in block.shards.iter().enumerate() {
            let Some(s) = s else { continue };
            if s.len() > len {
                return 0;
            }
            padded.fill(0);
            padded[..s.len()].copy_from_slice(s);
            let ok = if i < DATA_SHARDS {
                dec.add_original_shard(i, &padded).is_ok()
            } else {
                dec.add_recovery_shard(i - DATA_SHARDS, &padded).is_ok()
            };
            if !ok {
                return 0;
            }
        }
        let Ok(result) = dec.decode() else { return 0 };
        let mut n = 0;
        for (i, shard) in result.restored_original_iter() {
            if i >= DATA_SHARDS || block.delivered[i] {
                continue;
            }
            let Some(p) = parse(shard) else { continue };
            block.delivered[i] = true;
            n += 1;
            out.push(AudioPacket {
                seq: block.base.wrapping_add(i as u32),
                capture_ts_us: block.ts.wrapping_add(i as u32 * 5000),
                recovered: true,
                data: p.to_vec(),
            });
        }
        n
    }
}

fn parse(payload: &[u8]) -> Option<&[u8]> {
    let len = u16::from_le_bytes([*payload.first()?, *payload.get(1)?]) as usize;
    payload.get(2..2 + len).filter(|p| !p.is_empty())
}

/// What the decoder should do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Playout {
    /// Decode this packet.
    Packet(Vec<u8>),
    /// The packet is lost for good: conceal it (Opus PLC).
    Lost,
}

/// Puts packets back in sequence and decides when a missing one is lost.
/// A gap is held open for `wait_us` (long enough for the block's parity to
/// arrive) unless packets pile up behind it.
pub struct Reorder {
    next: Option<u32>,
    pending: std::collections::BTreeMap<u32, Vec<u8>>,
    gap_since: Option<u64>,
    wait_us: u64,
    max_ahead: u32,
    pub lost: u64,
    pub late: u64,
}

impl Reorder {
    /// `wait_us`: how long to hold a gap; `max_ahead`: packets queued behind
    /// a gap that end the wait early.
    pub fn new(wait_us: u64, max_ahead: u32) -> Self {
        Reorder {
            next: None,
            pending: Default::default(),
            gap_since: None,
            wait_us,
            max_ahead,
            lost: 0,
            late: 0,
        }
    }

    pub fn push(&mut self, seq: u32, data: Vec<u8>) {
        let next = *self.next.get_or_insert(seq);
        let ahead = seq.wrapping_sub(next) as i32;
        if ahead < 0 && ahead > -RESTART_GAP {
            self.late += 1;
            return;
        }
        if ahead < 0 || ahead as u32 > 4 * self.max_ahead.max(DATA_SHARDS as u32) {
            // A jump (host restarted its sequence): start over from here.
            self.pending.clear();
            self.next = Some(seq);
            self.gap_since = None;
        }
        self.pending.entry(seq).or_insert(data);
    }

    /// The next decode step at `now_us`, if there is one yet.
    pub fn pop(&mut self, now_us: u64) -> Option<Playout> {
        let next = self.next?;
        if let Some(data) = self.pending.remove(&next) {
            self.next = Some(next.wrapping_add(1));
            self.gap_since = None;
            return Some(Playout::Packet(data));
        }
        // `next` is missing. Is anything queued behind it?
        let newest = self.pending.keys().map(|&k| k.wrapping_sub(next)).max()?;
        let since = *self.gap_since.get_or_insert(now_us);
        if newest >= self.max_ahead || now_us.saturating_sub(since) >= self.wait_us {
            self.next = Some(next.wrapping_add(1));
            self.gap_since = if self.pending.contains_key(&next.wrapping_add(1)) {
                None
            } else {
                Some(now_us)
            };
            self.lost += 1;
            return Some(Playout::Lost);
        }
        None
    }

    /// Packets waiting (for diagnostics).
    pub fn queued(&self) -> usize {
        self.pending.len()
    }

    /// When the gap being held open gives up (`pop` then conceals it), if
    /// one is: until then there is nothing to do without a new packet.
    pub fn deadline_us(&self) -> Option<u64> {
        if self.pending.is_empty() {
            return None;
        }
        self.gap_since.map(|since| since + self.wait_us)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_gap_has_a_deadline_and_nothing_else_does() {
        let mut r = Reorder::new(20_000, 8);
        assert_eq!(r.deadline_us(), None);
        r.push(0, vec![0]);
        assert_eq!(r.pop(1_000), Some(Playout::Packet(vec![0])));
        assert_eq!(r.deadline_us(), None, "in order: nothing to wait for");
        r.push(2, vec![2]); // 1 is missing
        assert_eq!(r.pop(5_000), None);
        assert_eq!(r.deadline_us(), Some(25_000));
        assert_eq!(r.pop(25_000), Some(Playout::Lost));
        assert_eq!(r.pop(25_000), Some(Playout::Packet(vec![2])));
        assert_eq!(r.deadline_us(), None);
    }

    use super::*;

    fn opus(i: u32) -> Vec<u8> {
        (0..(40 + i % 7)).map(|b| (b * 7 + i) as u8).collect()
    }

    fn all_datagrams(n: u32) -> Vec<(u32, Vec<u8>)> {
        let mut p = AudioPacketizer::new();
        let mut out = Vec::new();
        for i in 0..n {
            for d in p.push(&opus(i), i * 5000) {
                out.push((i, d));
            }
        }
        out
    }

    #[test]
    fn every_block_gets_two_parity_datagrams() {
        let d = all_datagrams(8);
        assert_eq!(d.len(), 8 + 4);
        let parity = d
            .iter()
            .filter(|(_, g)| Header::decode(g).unwrap().fragment_idx >= 4)
            .count();
        assert_eq!(parity, 4);
    }

    #[test]
    fn lossless_delivery_is_immediate_and_exact() {
        let mut dep = AudioDepacketizer::new();
        let mut out = Vec::new();
        for (_, g) in all_datagrams(12) {
            dep.push(&g, &mut out);
        }
        assert_eq!(out.len(), 12);
        for (i, p) in out.iter().enumerate() {
            assert_eq!(p.seq, i as u32);
            assert_eq!(p.data, opus(i as u32));
            assert!(!p.recovered);
        }
    }

    #[test]
    fn any_two_losses_per_block_are_recovered() {
        for a in 0..6 {
            for b in (a + 1)..6 {
                let mut dep = AudioDepacketizer::new();
                let mut out = Vec::new();
                for (_, g) in all_datagrams(4) {
                    let idx = Header::decode(&g).unwrap().fragment_idx as usize;
                    if idx != a && idx != b {
                        dep.push(&g, &mut out);
                    }
                }
                let mut seqs: Vec<u32> = out.iter().map(|p| p.seq).collect();
                seqs.sort();
                assert_eq!(seqs, vec![0, 1, 2, 3], "lost {a} and {b}");
                for p in &out {
                    assert_eq!(p.data, opus(p.seq));
                }
            }
        }
    }

    #[test]
    fn three_losses_deliver_what_arrived() {
        let mut dep = AudioDepacketizer::new();
        let mut out = Vec::new();
        for (_, g) in all_datagrams(4) {
            let idx = Header::decode(&g).unwrap().fragment_idx;
            if ![0, 1, 4].contains(&idx) {
                dep.push(&g, &mut out);
            }
        }
        let seqs: Vec<u32> = out.iter().map(|p| p.seq).collect();
        assert_eq!(seqs, vec![2, 3]);
    }

    #[test]
    fn duplicates_are_ignored() {
        let mut dep = AudioDepacketizer::new();
        let mut out = Vec::new();
        for (_, g) in all_datagrams(4) {
            dep.push(&g, &mut out);
            dep.push(&g, &mut out);
        }
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn reorder_plays_in_sequence_and_conceals_after_the_wait() {
        let mut r = Reorder::new(10_000, 6);
        r.push(10, vec![10]);
        assert_eq!(r.pop(0), Some(Playout::Packet(vec![10])));
        r.push(12, vec![12]);
        assert_eq!(r.pop(1_000), None, "11 may still arrive");
        r.push(11, vec![11]);
        assert_eq!(r.pop(2_000), Some(Playout::Packet(vec![11])));
        assert_eq!(r.pop(2_000), Some(Playout::Packet(vec![12])));
        r.push(14, vec![14]);
        assert_eq!(r.pop(3_000), None);
        assert_eq!(r.pop(13_000), Some(Playout::Lost));
        assert_eq!(r.pop(13_000), Some(Playout::Packet(vec![14])));
        assert_eq!(r.lost, 1);
        r.push(13, vec![13]);
        assert_eq!(r.late, 1);
    }

    #[test]
    fn reorder_gives_up_early_when_packets_pile_up() {
        let mut r = Reorder::new(1_000_000, 3);
        r.push(0, vec![0]);
        assert!(r.pop(0).is_some());
        r.push(2, vec![2]);
        r.push(3, vec![3]);
        assert_eq!(r.pop(0), None);
        r.push(4, vec![4]);
        assert_eq!(r.pop(0), Some(Playout::Lost));
        assert_eq!(r.pop(0), Some(Playout::Packet(vec![2])));
    }

    #[test]
    fn reorder_restarts_on_a_sequence_jump() {
        let mut r = Reorder::new(10_000, 6);
        r.push(5, vec![5]);
        assert!(r.pop(0).is_some());
        r.push(100_000, vec![1]);
        assert_eq!(r.pop(0), Some(Playout::Packet(vec![1])));
        // ...and back to zero, as when the host restarts its encoder.
        r.push(0, vec![0]);
        assert_eq!(r.pop(0), Some(Playout::Packet(vec![0])));
    }

    #[test]
    fn depacketizer_follows_a_restarted_sequence() {
        let mut dep = AudioDepacketizer::new();
        let mut out = Vec::new();
        let mut p = AudioPacketizer::new();
        for i in 0..5000u32 {
            for d in p.push(&opus(i), 0) {
                dep.push(&d, &mut out);
            }
        }
        out.clear();
        for (_, g) in all_datagrams(4) {
            dep.push(&g, &mut out);
        }
        assert_eq!(out.len(), 4);
    }
}
