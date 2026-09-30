//! Datagrams -> frames, with Reed-Solomon recovery. See v1 design §5, §6.3, §13.1.
//!
//! Invariant under test: a frame either reconstructs BYTE-EXACTLY or is not
//! reported at all. Silent corruption is the failure mode to prevent.
//!
//! This module parses hostile network input. Every malformed datagram must be
//! dropped without panicking -- see the fuzz target.
//!
//! It runs for every video datagram, so it allocates nothing per datagram:
//! a fixed set of frame slots is reused, each FEC block's shards are copied
//! once, straight to their place in the block's buffer, and a frame of one
//! block (anything up to ~230 KB) is handed out as that buffer itself. Give
//! finished frames' buffers back with [`Reassembler::recycle`] and a steady
//! stream allocates nothing at all.

use crate::header::Header;
use crate::HEADER_LEN;
use reed_solomon_simd::ReedSolomonDecoder;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedFrame {
    pub frame_id: u32,
    pub capture_ts_us: u32,
    pub keyframe: bool,
    pub recovery: bool,
    pub data: Vec<u8>,
}

/// Frames already emitted, so late duplicates cannot re-emit them.
const COMPLETED_MEMORY: usize = 32;
/// Spare frame buffers kept for reuse.
const SPARES: usize = 4;

/// One FEC block's shards. `buf` holds the data shards, padded to the
/// frame's shard size, in fragment order, then the parity shards.
#[derive(Default)]
struct Block {
    /// This block's datagrams have been seen (its shape is known).
    known: bool,
    data_shards: usize,
    parity_shards: usize,
    /// Which fragment indices have arrived (data then parity), one bit each.
    present: [u64; 8],
    data_count: usize,
    parity_count: usize,
    done: bool,
    buf: Vec<u8>,
}

impl Block {
    fn has(&self, idx: usize) -> bool {
        self.present[idx / 64] & (1 << (idx % 64)) != 0
    }

    fn mark(&mut self, idx: usize) {
        self.present[idx / 64] |= 1 << (idx % 64);
    }
}

#[derive(Default)]
struct Slot {
    in_use: bool,
    frame_id: u32,
    capture_ts_us: u32,
    keyframe: bool,
    recovery: bool,
    frame_len: usize,
    /// Bytes per shard (`Header::shard_len`).
    shard: usize,
    /// Data shards the whole frame has (from `frame_len`).
    total_shards: usize,
    /// Data shards of the blocks seen so far, and how many of those are done.
    known_shards: usize,
    done_shards: usize,
    /// Monotonic counter for eviction order.
    seq: u64,
    /// Indexed by `fec_block_idx`; only the first `used` are this frame's.
    blocks: Vec<Block>,
    used: usize,
}

pub struct Reassembler {
    slots: Vec<Slot>,
    seq: u64,
    completed: [u32; COMPLETED_MEMORY],
    completed_len: usize,
    completed_next: usize,
    spares: Vec<Vec<u8>>,
    decoder: Option<ReedSolomonDecoder>,
}

impl Reassembler {
    pub fn new(max_tracked_frames: usize) -> Self {
        Reassembler {
            slots: (0..max_tracked_frames.max(1))
                .map(|_| Slot::default())
                .collect(),
            seq: 0,
            completed: [0; COMPLETED_MEMORY],
            completed_len: 0,
            completed_next: 0,
            spares: Vec::new(),
            decoder: None,
        }
    }

    pub fn tracked_frames(&self) -> usize {
        self.slots.iter().filter(|s| s.in_use).count()
    }

    /// Give a finished frame's buffer back, to be filled by a later frame.
    pub fn recycle(&mut self, mut buf: Vec<u8>) {
        if self.spares.len() < SPARES && buf.capacity() > 0 {
            buf.clear();
            self.spares.push(buf);
        }
    }

    fn was_completed(&self, frame_id: u32) -> bool {
        self.completed[..self.completed_len].contains(&frame_id)
    }

    fn remember_completed(&mut self, frame_id: u32) {
        self.completed[self.completed_next] = frame_id;
        self.completed_next = (self.completed_next + 1) % COMPLETED_MEMORY;
        self.completed_len = (self.completed_len + 1).min(COMPLETED_MEMORY);
    }

    /// The slot for `h`'s frame: its own, or a new one (evicting the oldest
    /// partial frame when all are taken). A partial frame whose shards never
    /// arrive is never completed, so nothing is lost by evicting it.
    fn slot_for(&mut self, h: &Header) -> Option<usize> {
        let frame_len = h.frame_len as usize;
        let shard = h.shard_len();
        if let Some(i) = self
            .slots
            .iter()
            .position(|s| s.in_use && s.frame_id == h.frame_id)
        {
            // A mismatched frame_len (or shard size) across packets means a
            // corrupt or spoofed datagram; ignore it rather than trusting
            // either value.
            let s = &self.slots[i];
            return (s.frame_len == frame_len && s.shard == shard).then_some(i);
        }
        let total_shards = frame_len.div_ceil(shard);
        if total_shards == 0 {
            return None;
        }
        let i = match self.slots.iter().position(|s| !s.in_use) {
            Some(i) => i,
            None => (0..self.slots.len()).min_by_key(|&i| self.slots[i].seq)?,
        };
        self.seq += 1;
        let s = &mut self.slots[i];
        s.in_use = true;
        s.frame_id = h.frame_id;
        s.capture_ts_us = h.capture_ts_us;
        s.keyframe = h.keyframe;
        s.recovery = h.recovery;
        s.frame_len = frame_len;
        s.shard = shard;
        s.total_shards = total_shards;
        s.known_shards = 0;
        s.done_shards = 0;
        s.seq = self.seq;
        for b in &mut s.blocks[..s.used] {
            b.known = false;
        }
        s.used = 0;
        Some(i)
    }

    pub fn push(&mut self, datagram: &[u8]) -> Option<CompletedFrame> {
        let h = Header::decode(datagram).ok()?;
        let payload = datagram.get(HEADER_LEN..h.total_len as usize)?;

        let shard = h.shard_len();
        if h.data_shards == 0 || payload.len() > shard {
            return None;
        }
        if self.was_completed(h.frame_id) {
            return None;
        }
        let si = self.slot_for(&h)?;

        let (data_shards, parity_shards) = (h.data_shards as usize, h.parity_shards as usize);
        let idx = h.fragment_idx as usize;
        if idx >= data_shards + parity_shards {
            return None;
        }
        let bi = h.fec_block_idx as usize;
        let slot = &mut self.slots[si];
        if slot.blocks.len() <= bi {
            slot.blocks.resize_with(bi + 1, Block::default);
        }
        slot.used = slot.used.max(bi + 1);
        let block = &mut slot.blocks[bi];
        if !block.known {
            // More data shards than the frame has left: not one of its blocks.
            if slot.known_shards + data_shards > slot.total_shards {
                return None;
            }
            slot.known_shards += data_shards;
            block.known = true;
            block.data_shards = data_shards;
            block.parity_shards = parity_shards;
            block.present = [0; 8];
            block.data_count = 0;
            block.parity_count = 0;
            block.done = false;
            let len = (data_shards + parity_shards) * shard;
            if block.buf.capacity() == 0 {
                if let Some(spare) = self.spares.pop() {
                    block.buf = spare;
                }
            }
            // Only growth is zeroed: every shard read later is written first.
            block.buf.resize(len, 0);
        } else if block.data_shards != data_shards || block.parity_shards != parity_shards {
            return None;
        }
        if block.done || block.has(idx) {
            return None;
        }

        // Shards are stored PADDED, because Reed-Solomon requires equal sizes.
        let at = idx * shard;
        block.buf[at..at + payload.len()].copy_from_slice(payload);
        block.buf[at + payload.len()..at + shard].fill(0);
        block.mark(idx);
        if idx < data_shards {
            block.data_count += 1;
        } else {
            block.parity_count += 1;
        }

        if block.data_count == data_shards {
            block.done = true;
        } else if block.data_count + block.parity_count >= data_shards {
            // Enough shards in total -- run recovery now.
            if !recover(&mut self.decoder, block, shard) {
                return None;
            }
            block.done = true;
        } else {
            return None;
        }
        slot.done_shards += data_shards;
        if slot.done_shards < slot.total_shards {
            return None;
        }
        self.finish(si)
    }

    /// Every block of the frame in slot `si` is done: assemble it in block
    /// order, then strip padding using frame_len -- the reason that field
    /// exists (v1 design §5.1).
    fn finish(&mut self, si: usize) -> Option<CompletedFrame> {
        let slot = &mut self.slots[si];
        slot.in_use = false;
        let blocks = &mut slot.blocks[..slot.used];
        if blocks.iter().any(|b| !b.known) {
            // A gap in the block indices: the shard count adds up only by
            // accident of a spoofed header.
            return None;
        }
        let mut data = if blocks.len() == 1 {
            std::mem::take(&mut blocks[0].buf)
        } else {
            let mut data = self.spares.pop().unwrap_or_default();
            data.clear();
            data.reserve(slot.total_shards * slot.shard);
            for b in blocks.iter() {
                data.extend_from_slice(&b.buf[..b.data_shards * slot.shard]);
            }
            data
        };
        if data.len() < slot.frame_len {
            return None;
        }
        data.truncate(slot.frame_len);
        let out = CompletedFrame {
            frame_id: slot.frame_id,
            capture_ts_us: slot.capture_ts_us,
            keyframe: slot.keyframe,
            recovery: slot.recovery,
            data,
        };
        self.remember_completed(out.frame_id);
        Some(out)
    }
}

/// Rebuild `block`'s missing data shards from what arrived. False if the
/// shards do not decode (a corrupt block).
fn recover(decoder: &mut Option<ReedSolomonDecoder>, block: &mut Block, shard: usize) -> bool {
    let (d, p) = (block.data_shards, block.parity_shards);
    let dec = match decoder {
        Some(dec) => {
            if dec.reset(d, p, shard).is_err() {
                return false;
            }
            dec
        }
        None => match ReedSolomonDecoder::new(d, p, shard) {
            Ok(dec) => decoder.insert(dec),
            Err(_) => return false,
        },
    };
    for i in 0..d + p {
        if !block.has(i) {
            continue;
        }
        let bytes = &block.buf[i * shard..(i + 1) * shard];
        let added = if i < d {
            dec.add_original_shard(i, bytes)
        } else {
            dec.add_recovery_shard(i - d, bytes)
        };
        if added.is_err() {
            return false;
        }
    }
    let Ok(restored) = dec.decode() else {
        return false;
    };
    let mut rebuilt = 0;
    for (i, s) in restored.restored_original_iter() {
        if i < d && s.len() == shard {
            block.buf[i * shard..(i + 1) * shard].copy_from_slice(s);
            rebuilt += 1;
        }
    }
    rebuilt + block.data_count == d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packetize::Packetizer;
    use crate::PAYLOAD_LEN;

    fn frame(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn lossless_reassembly_is_byte_exact() {
        let original = frame(PAYLOAD_LEN * 3 + 17);
        let pkts = Packetizer::new()
            .packetize(&original, 1, 999, true, false)
            .unwrap();

        let mut r = Reassembler::new(8);
        let mut done = None;
        for pkt in &pkts {
            if let Some(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        let f = done.expect("frame must complete");
        assert_eq!(f.data, original);
        assert_eq!(f.frame_id, 1);
        assert_eq!(f.capture_ts_us, 999);
        assert!(f.keyframe);
    }

    #[test]
    fn completes_without_parity_when_all_data_arrives() {
        let original = frame(PAYLOAD_LEN * 3);
        let pkts = Packetizer::new()
            .packetize(&original, 1, 0, false, false)
            .unwrap();
        let mut r = Reassembler::new(8);
        // Only the 3 data shards; drop all parity.
        let mut done = None;
        for pkt in pkts.iter().take(3) {
            if let Some(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        assert_eq!(done.expect("should complete on data alone").data, original);
    }

    #[test]
    fn recovers_a_lost_middle_data_shard() {
        let original = frame(PAYLOAD_LEN * 4);
        let pkts = Packetizer::new()
            .packetize(&original, 1, 0, false, false)
            .unwrap();
        let mut r = Reassembler::new(8);
        let mut done = None;
        for (i, pkt) in pkts.iter().enumerate() {
            if i == 1 {
                continue; // drop data shard 1
            }
            if let Some(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        assert_eq!(done.expect("parity must recover it").data, original);
    }

    #[test]
    fn recovers_a_lost_final_shard_using_frame_len() {
        // The regression test for the reason frame_len exists (v1 design §5.1):
        // the final shard is padded, so recovery yields PAYLOAD_LEN bytes and
        // only frame_len says how many are real.
        let original = frame(PAYLOAD_LEN * 2 + 3);
        let pkts = Packetizer::new()
            .packetize(&original, 1, 0, false, false)
            .unwrap();
        let mut r = Reassembler::new(8);
        let mut done = None;
        for (i, pkt) in pkts.iter().enumerate() {
            if i == 2 {
                continue; // drop the LAST data shard (the padded one)
            }
            if let Some(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        let f = done.expect("must recover the padded tail shard");
        assert_eq!(f.data.len(), original.len(), "padding must be stripped");
        assert_eq!(f.data, original);
    }

    #[test]
    fn returns_none_when_too_many_shards_are_lost() {
        let original = frame(PAYLOAD_LEN * 4);
        let pkts = Packetizer::new()
            .packetize(&original, 1, 0, false, false)
            .unwrap();
        // 4 data + 2 parity; dropping 3 makes recovery impossible.
        let mut r = Reassembler::new(8);
        for (i, pkt) in pkts.iter().enumerate() {
            if i < 3 {
                continue;
            }
            assert!(r.push(pkt).is_none(), "must not claim completion");
        }
    }

    #[test]
    fn duplicate_shards_are_ignored() {
        let original = frame(PAYLOAD_LEN * 2);
        let pkts = Packetizer::new()
            .packetize(&original, 1, 0, false, false)
            .unwrap();
        let mut r = Reassembler::new(8);
        let mut completions = 0;
        for pkt in pkts.iter().chain(pkts.iter()) {
            if r.push(pkt).is_some() {
                completions += 1;
            }
        }
        assert_eq!(completions, 1, "a frame must complete exactly once");
    }

    #[test]
    fn malformed_datagrams_are_dropped_without_panic() {
        let mut r = Reassembler::new(8);
        assert!(r.push(&[]).is_none());
        assert!(r.push(&[0x45]).is_none());
        assert!(r.push(&[0x65; 40]).is_none()); // wrong version
        assert!(r.push(&[0xFF; 1200]).is_none());
    }

    #[test]
    fn eviction_bounds_memory() {
        let mut r = Reassembler::new(2);
        // Start 3 frames, each with one shard so none completes.
        for frame_id in 0..3u32 {
            let original = frame(PAYLOAD_LEN * 4);
            let pkts = Packetizer::new()
                .packetize(&original, frame_id, 0, false, false)
                .unwrap();
            r.push(&pkts[0]);
        }
        assert!(r.tracked_frames() <= 2, "must evict oldest partial frames");
    }

    #[test]
    fn the_oldest_partial_frame_is_the_one_evicted() {
        let mut r = Reassembler::new(2);
        let original = frame(PAYLOAD_LEN * 2);
        let pkts: Vec<_> = (0..3u32)
            .map(|id| {
                Packetizer::new()
                    .packetize(&original, id, 0, false, false)
                    .unwrap()
            })
            .collect();
        r.push(&pkts[0][0]);
        r.push(&pkts[1][0]);
        r.push(&pkts[2][0]); // evicts frame 0
        assert!(r.push(&pkts[1][1]).is_some(), "frame 1 survived");
        assert!(r.push(&pkts[0][1]).is_none(), "frame 0 starts over");
    }

    #[test]
    fn recycled_buffers_carry_no_stale_bytes() {
        let mut r = Reassembler::new(4);
        let big: Vec<u8> = vec![0xEE; PAYLOAD_LEN * 5];
        let pkts = Packetizer::new()
            .packetize(&big, 1, 0, false, false)
            .unwrap();
        let f = pkts.iter().find_map(|p| r.push(p)).unwrap();
        r.recycle(f.data);
        // A shorter frame with its padded last shard lost: recovery must see
        // zero padding, not the previous frame's bytes.
        let small = frame(PAYLOAD_LEN * 2 + 5);
        let pkts = Packetizer::new()
            .packetize(&small, 2, 0, false, false)
            .unwrap();
        let f = pkts
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != 2)
            .find_map(|(_, p)| r.push(p))
            .unwrap();
        assert_eq!(f.data, small);
    }

    #[test]
    fn lan_shards_round_trip_and_recover() {
        use crate::LAN_PAYLOAD_LEN;
        let mut p = Packetizer::new();
        p.set_lan_shards(true);
        let mut r = Reassembler::new(8);
        for (id, len) in [
            (1u32, 17usize),
            (2, LAN_PAYLOAD_LEN * 3 + 5),
            (3, LAN_PAYLOAD_LEN * 260),
        ] {
            let original = frame(len);
            let pkts = p.packetize(&original, id, 0, false, false).unwrap();
            assert!(pkts.iter().all(|d| Header::decode(d).unwrap().lan_shards));
            assert!(pkts.iter().all(|d| d.len() <= HEADER_LEN + LAN_PAYLOAD_LEN));
            // Each block's first datagram lost: parity rebuilds it.
            let f = pkts
                .iter()
                .filter(|d| Header::decode(d).unwrap().fragment_idx != 0)
                .find_map(|d| r.push(d))
                .expect("recovers");
            assert_eq!(f.data, original, "{len} bytes");
        }
        // Frames of both sizes interleaved on one reassembler.
        let small = frame(PAYLOAD_LEN * 2 + 1);
        let large = frame(LAN_PAYLOAD_LEN * 2 + 1);
        let a = Packetizer::new()
            .packetize(&small, 10, 0, false, false)
            .unwrap();
        let b = p.packetize(&large, 11, 0, false, false).unwrap();
        let mut done = Vec::new();
        for (x, y) in a.iter().zip(b.iter()) {
            done.extend(r.push(x));
            done.extend(r.push(y));
        }
        let got: Vec<_> = done.iter().map(|f| (f.frame_id, f.data.clone())).collect();
        assert!(got.contains(&(10, small)) && got.contains(&(11, large)));
    }

    #[test]
    fn a_frame_of_several_blocks_is_assembled_in_order() {
        let original = frame(PAYLOAD_LEN * 450 + 99);
        let pkts = Packetizer::new()
            .packetize(&original, 5, 0, true, false)
            .unwrap();
        let mut r = Reassembler::new(8);
        // Blocks arrive back to front, each missing its first shard.
        let mut done = None;
        for pkt in pkts.iter().rev() {
            if Header::decode(pkt).unwrap().fragment_idx == 0 {
                continue;
            }
            if let Some(f) = r.push(pkt) {
                done = Some(f);
            }
        }
        assert_eq!(done.expect("every block recovers").data, original);
    }
}
