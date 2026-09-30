//! Frame -> datagrams. See v1 design §5, §6.2, §6.3.
//!
//! Layout of the returned Vec: for each FEC block, all data-shard datagrams in
//! fragment order, then all parity-shard datagrams. `fragment_idx` restarts at
//! 0 in every block; blocks are distinguished by `fec_block_idx`.

use crate::fec::{block_sizes_max, supported_parity, FecPolicy};
use crate::header::{Header, Kind};
use crate::{HEADER_LEN, LAN_PAYLOAD_LEN, PAYLOAD_LEN};
use reed_solomon_simd::ReedSolomonEncoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketizeError {
    EmptyFrame,
    /// frame_len is a u24 on the wire (v1 design §5.1).
    FrameTooLarge,
    NoSupportedParity,
    Rs(&'static str),
}

/// A frame's datagrams, laid end to end in one reusable buffer: packetizing
/// a frame allocates nothing once this has grown to the largest frame seen.
#[derive(Default)]
pub struct Datagrams {
    bytes: Vec<u8>,
    /// Where each datagram ends in `bytes`.
    ends: Vec<usize>,
}

impl Datagrams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.bytes.clear();
        self.ends.clear();
    }

    pub fn len(&self) -> usize {
        self.ends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Total bytes of every datagram.
    pub fn bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn get(&self, i: usize) -> &[u8] {
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        &self.bytes[start..self.ends[i]]
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &[u8]> + Clone + '_ {
        (0..self.len()).map(|i| self.get(i))
    }

    /// Datagrams `range`, in order.
    pub fn range(
        &self,
        range: std::ops::Range<usize>,
    ) -> impl ExactSizeIterator<Item = &[u8]> + Clone + '_ {
        range.map(|i| self.get(i))
    }

    fn push(&mut self, header: &Header, payload: &[u8]) {
        let mut hdr = [0u8; HEADER_LEN];
        header
            .encode(&mut hdr)
            .expect("frame_len bounds checked by the packetizer");
        self.bytes.extend_from_slice(&hdr);
        self.bytes.extend_from_slice(payload);
        self.ends.push(self.bytes.len());
    }
}

pub struct Packetizer {
    /// Reused across frames; `reset()` per block avoids per-frame
    /// reallocation (v1 design §6.3).
    encoder: Option<ReedSolomonEncoder>,
    fec: FecPolicy,
    /// Bytes per shard: `PAYLOAD_LEN`, or `LAN_PAYLOAD_LEN` on the LAN.
    shard: usize,
}

impl Default for Packetizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Packetizer {
    pub fn new() -> Self {
        Packetizer {
            encoder: None,
            fec: FecPolicy::DEFAULT,
            shard: PAYLOAD_LEN,
        }
    }

    /// Shards of `LAN_PAYLOAD_LEN` bytes for the frames that follow (a
    /// client on the local network that understands them), or of
    /// `PAYLOAD_LEN`.
    pub fn set_lan_shards(&mut self, lan: bool) {
        self.shard = if lan { LAN_PAYLOAD_LEN } else { PAYLOAD_LEN };
    }

    /// Parity for the frames that follow.
    pub fn set_fec(&mut self, fec: FecPolicy) {
        self.fec = fec;
    }

    /// [`Packetizer::packetize_into`], each datagram in its own `Vec`.
    pub fn packetize(
        &mut self,
        frame: &[u8],
        frame_id: u32,
        capture_ts_us: u32,
        keyframe: bool,
        recovery: bool,
    ) -> Result<Vec<Vec<u8>>, PacketizeError> {
        let mut out = Datagrams::new();
        self.packetize_into(frame, frame_id, capture_ts_us, keyframe, recovery, &mut out)?;
        Ok(out.iter().map(<[u8]>::to_vec).collect())
    }

    /// Split `frame` into datagrams, with parity, replacing what `out` held.
    pub fn packetize_into(
        &mut self,
        frame: &[u8],
        frame_id: u32,
        capture_ts_us: u32,
        keyframe: bool,
        recovery: bool,
        out: &mut Datagrams,
    ) -> Result<(), PacketizeError> {
        out.clear();
        if frame.is_empty() {
            return Err(PacketizeError::EmptyFrame);
        }
        if frame.len() > 0x00FF_FFFF {
            return Err(PacketizeError::FrameTooLarge);
        }

        let shard = self.shard;
        let total_shards = frame.len().div_ceil(shard);
        let mut shard_cursor = 0usize;

        let blocks = block_sizes_max(total_shards, self.fec.max_data_per_block());
        out.bytes.reserve(
            blocks
                .iter()
                .map(|&d| d + self.fec.parity(d))
                .sum::<usize>()
                * (HEADER_LEN + shard),
        );
        for (block_idx, &data_shards) in blocks.iter().enumerate() {
            let parity_shards = supported_parity(data_shards, self.fec.parity(data_shards))
                .ok_or(PacketizeError::NoSupportedParity)?;

            let encoder = match self.encoder.as_mut() {
                Some(e) => {
                    e.reset(data_shards, parity_shards, shard)
                        .map_err(|_| PacketizeError::Rs("reset"))?;
                    e
                }
                None => self.encoder.insert(
                    ReedSolomonEncoder::new(data_shards, parity_shards, shard)
                        .map_err(|_| PacketizeError::Rs("new"))?,
                ),
            };

            let header = |fragment_idx: usize, frame_end: bool, len: usize| Header {
                keyframe,
                recovery,
                lan_shards: shard == LAN_PAYLOAD_LEN,
                frame_end,
                kind: Kind::Video,
                total_len: (HEADER_LEN + len) as u16,
                fragment_idx: fragment_idx as u16,
                data_shards: data_shards as u8,
                parity_shards: parity_shards as u8,
                fec_block_idx: block_idx as u8,
                frame_len: frame.len() as u32,
                capture_ts_us,
                frame_id,
            };

            // Data shards go out at TRUE length -- padding is never
            // transmitted -- but the encoder is fed them padded.
            let last_data_of_frame = shard_cursor + data_shards == total_shards;
            for i in 0..data_shards {
                let start = (shard_cursor + i) * shard;
                let payload = &frame[start..(start + shard).min(frame.len())];
                let frame_end = last_data_of_frame && i + 1 == data_shards;
                out.push(&header(i, frame_end, payload.len()), payload);
                let added = if payload.len() == shard {
                    encoder.add_original_shard(payload)
                } else {
                    let mut padded = [0u8; LAN_PAYLOAD_LEN];
                    padded[..payload.len()].copy_from_slice(payload);
                    encoder.add_original_shard(&padded[..shard])
                };
                added.map_err(|_| PacketizeError::Rs("add_original_shard"))?;
            }

            // Parity shards, always full length.
            let result = encoder.encode().map_err(|_| PacketizeError::Rs("encode"))?;
            for (j, shard) in result.recovery_iter().enumerate() {
                out.push(&header(data_shards + j, false, shard.len()), shard);
            }

            shard_cursor += data_shards;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::Header;

    #[test]
    fn single_packet_frame() {
        let mut p = Packetizer::new();
        let frame = vec![0xABu8; 500];
        let pkts = p.packetize(&frame, 1, 12345, true, false).unwrap();

        // 1 data shard + parity floor of 2 = 3 datagrams.
        assert_eq!(pkts.len(), 3);
        let h = Header::decode(&pkts[0]).unwrap();
        assert_eq!(h.data_shards, 1);
        assert_eq!(h.parity_shards, 2);
        assert_eq!(h.frame_len, 500);
        assert_eq!(h.frame_id, 1);
        assert_eq!(h.capture_ts_us, 12345);
        assert!(h.keyframe);
        // The single data shard is sent at its TRUE length, not padded.
        assert_eq!(pkts[0].len(), HEADER_LEN + 500);
        assert_eq!(h.total_len as usize, HEADER_LEN + 500);
    }

    #[test]
    fn parity_shards_are_full_length() {
        // Parity is computed over PADDED shards, so parity is always full size.
        let mut p = Packetizer::new();
        let frame = vec![0xABu8; 500];
        let pkts = p.packetize(&frame, 1, 0, false, false).unwrap();
        for pkt in &pkts[1..] {
            assert_eq!(pkt.len(), HEADER_LEN + PAYLOAD_LEN);
        }
    }

    #[test]
    fn multi_shard_frame_indices_are_sequential() {
        let mut p = Packetizer::new();
        let frame = vec![0x5Au8; PAYLOAD_LEN * 3 + 17];
        let pkts = p.packetize(&frame, 7, 0, false, false).unwrap();

        assert_eq!(pkts.len(), 4 + 2); // 4 data shards, parity_for(4) == 2
        for (i, pkt) in pkts.iter().enumerate() {
            let h = Header::decode(pkt).unwrap();
            assert_eq!(h.fragment_idx as usize, i);
            assert_eq!(h.data_shards, 4);
            assert_eq!(h.frame_len as usize, PAYLOAD_LEN * 3 + 17);
        }
    }

    #[test]
    fn only_the_last_data_shard_sets_frame_end() {
        let mut p = Packetizer::new();
        let frame = vec![0u8; PAYLOAD_LEN * 3];
        let pkts = p.packetize(&frame, 1, 0, false, false).unwrap();
        let ends: Vec<bool> = pkts
            .iter()
            .map(|p| Header::decode(p).unwrap().frame_end)
            .collect();
        assert_eq!(ends[..3], [false, false, true]);
    }

    #[test]
    fn payload_bytes_are_preserved_in_order() {
        let mut p = Packetizer::new();
        let frame: Vec<u8> = (0..PAYLOAD_LEN * 2).map(|i| (i % 251) as u8).collect();
        let pkts = p.packetize(&frame, 1, 0, false, false).unwrap();
        assert_eq!(&pkts[0][HEADER_LEN..], &frame[..PAYLOAD_LEN]);
        assert_eq!(&pkts[1][HEADER_LEN..], &frame[PAYLOAD_LEN..]);
    }

    #[test]
    fn large_frame_splits_into_blocks() {
        let mut p = Packetizer::new();
        // 250 data shards -> 2 blocks of 125.
        let frame = vec![0u8; PAYLOAD_LEN * 250];
        let pkts = p.packetize(&frame, 1, 0, true, false).unwrap();
        let blocks: std::collections::HashSet<u8> = pkts
            .iter()
            .map(|p| Header::decode(p).unwrap().fec_block_idx)
            .collect();
        assert_eq!(blocks.len(), 2);
        // Within each block, fragment_idx restarts at 0.
        let block1_first = pkts
            .iter()
            .find(|p| Header::decode(p).unwrap().fec_block_idx == 1)
            .map(|p| Header::decode(p).unwrap().fragment_idx);
        assert_eq!(block1_first, Some(0));
    }

    #[test]
    fn rejects_empty_frame() {
        let mut p = Packetizer::new();
        assert_eq!(
            p.packetize(&[], 1, 0, false, false),
            Err(PacketizeError::EmptyFrame)
        );
    }

    #[test]
    fn rejects_frame_over_u24() {
        let mut p = Packetizer::new();
        let frame = vec![0u8; 0x0100_0000];
        assert_eq!(
            p.packetize(&frame, 1, 0, false, false),
            Err(PacketizeError::FrameTooLarge)
        );
    }
}
