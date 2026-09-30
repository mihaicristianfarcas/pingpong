//! The 20-byte pingpong header. It IS the inner IPv4 header: only three of
//! its twenty bytes are validated by boringtun on receive, so the rest carry
//! pingpong data at zero overhead. See v1 design §5 for the full layout table and
//! §4.1 for the exact constraints.
//!
//! THIS IS THE ONLY FILE THAT KNOWS BYTE OFFSETS. Do not lay out header bytes
//! anywhere else.
//!
//! ```text
//! byte  0     0x45            version=4 REQUIRED (IHL=5 cosmetic, never read)
//! byte  1     flags           bit0 keyframe · bit1 frame_end · bits2-3 kind
//!                             bit4 recovery (video: first frame after RFI)
//!                             bit5 lan_shards (video: LAN_PAYLOAD_LEN shards)
//! bytes 2-3   total_len  BE   REQUIRED: true total length, <= datagram length
//! bytes 4-5   fragment_idx    u16 LE
//! byte  6     data_shards     u8
//! byte  7     parity_shards   u8
//! byte  8     fec_block_idx   u8
//! bytes 9-11  frame_len       u24 LE
//! bytes 12-15 capture_ts_us   u32 LE  (also surfaces as a bogus "src IP")
//! bytes 16-19 frame_id        u32 LE
//! ```

use crate::{HEADER_LEN, LAN_PAYLOAD_LEN, PAYLOAD_LEN};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Video = 0,
    Audio = 1,
    Input = 2,
    Control = 3,
}

impl Kind {
    fn from_bits(bits: u8) -> Kind {
        match bits & 0b11 {
            0 => Kind::Video,
            1 => Kind::Audio,
            2 => Kind::Input,
            _ => Kind::Control,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub keyframe: bool,
    /// Video only: the first frame encoded after reference invalidation. It
    /// predicts only from frames the client reported having, so it ends the
    /// client's post-loss wait without an IDR. Flags bit 4.
    pub recovery: bool,
    /// Video only: the frame's shards are `LAN_PAYLOAD_LEN` bytes, not
    /// `PAYLOAD_LEN`. Flags bit 5.
    pub lan_shards: bool,
    pub frame_end: bool,
    pub kind: Kind,
    /// Header + payload length of THIS datagram. Must be exact: boringtun
    /// returns only `packet[..total_len]` to us (v1 design §5.1).
    pub total_len: u16,
    pub fragment_idx: u16,
    pub data_shards: u8,
    pub parity_shards: u8,
    pub fec_block_idx: u8,
    /// True payload length of the whole frame. Required because Reed-Solomon
    /// pads the final shard (v1 design §5.1, §6.3). Max 2^24 - 1.
    pub frame_len: u32,
    pub capture_ts_us: u32,
    pub frame_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    TooShort,
    BadVersion,
    LengthMismatch,
    FrameLenOverflow,
}

impl Header {
    /// Bytes per FEC shard of this datagram's frame.
    pub fn shard_len(&self) -> usize {
        if self.lan_shards {
            LAN_PAYLOAD_LEN
        } else {
            PAYLOAD_LEN
        }
    }

    pub fn encode(&self, out: &mut [u8; HEADER_LEN]) -> Result<(), HeaderError> {
        if self.frame_len > 0x00FF_FFFF {
            return Err(HeaderError::FrameLenOverflow);
        }
        out[0] = 0x45;
        let mut flags = 0u8;
        if self.keyframe {
            flags |= 0b0000_0001;
        }
        if self.frame_end {
            flags |= 0b0000_0010;
        }
        if self.recovery {
            flags |= 0b0001_0000;
        }
        if self.lan_shards {
            flags |= 0b0010_0000;
        }
        flags |= (self.kind as u8) << 2;
        out[1] = flags;
        out[2..4].copy_from_slice(&self.total_len.to_be_bytes());
        out[4..6].copy_from_slice(&self.fragment_idx.to_le_bytes());
        out[6] = self.data_shards;
        out[7] = self.parity_shards;
        out[8] = self.fec_block_idx;
        out[9..12].copy_from_slice(&self.frame_len.to_le_bytes()[..3]);
        out[12..16].copy_from_slice(&self.capture_ts_us.to_le_bytes());
        out[16..20].copy_from_slice(&self.frame_id.to_le_bytes());
        Ok(())
    }

    pub fn decode(buf: &[u8]) -> Result<Header, HeaderError> {
        if buf.len() < HEADER_LEN {
            return Err(HeaderError::TooShort);
        }
        if buf[0] >> 4 != 4 {
            return Err(HeaderError::BadVersion);
        }
        let total_len = u16::from_be_bytes([buf[2], buf[3]]);
        if (total_len as usize) < HEADER_LEN || total_len as usize > buf.len() {
            return Err(HeaderError::LengthMismatch);
        }
        Ok(Header {
            keyframe: buf[1] & 0b0000_0001 != 0,
            recovery: buf[1] & 0b0001_0000 != 0,
            lan_shards: buf[1] & 0b0010_0000 != 0,
            frame_end: buf[1] & 0b0000_0010 != 0,
            kind: Kind::from_bits(buf[1] >> 2),
            total_len,
            fragment_idx: u16::from_le_bytes([buf[4], buf[5]]),
            data_shards: buf[6],
            parity_shards: buf[7],
            fec_block_idx: buf[8],
            frame_len: u32::from_le_bytes([buf[9], buf[10], buf[11], 0]),
            capture_ts_us: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
            frame_id: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Header {
        Header {
            keyframe: true,
            recovery: false,
            lan_shards: false,
            frame_end: false,
            kind: Kind::Video,
            total_len: 1200,
            fragment_idx: 17,
            data_shards: 36,
            parity_shards: 8,
            fec_block_idx: 0,
            frame_len: 41_667,
            capture_ts_us: 0xDEAD_BEEF,
            frame_id: 0x0102_0304,
        }
    }

    /// Encode into a full datagram of `total_len` bytes, the way the wire
    /// carries it.
    ///
    /// `decode` requires `total_len <= buf.len()`, so a header claiming a
    /// 1200-byte datagram cannot be decoded out of a bare 20-byte array. That
    /// check is deliberate -- it is what stops a hostile `total_len` from
    /// causing an over-read in `Reassembler::push` -- and
    /// `rejects_total_len_exceeding_buffer` below pins the same behaviour.
    fn to_wire(h: &Header) -> Vec<u8> {
        let mut hdr = [0u8; HEADER_LEN];
        h.encode(&mut hdr).unwrap();
        let mut wire = vec![0u8; h.total_len as usize];
        wire[..HEADER_LEN].copy_from_slice(&hdr);
        wire
    }

    #[test]
    fn round_trip() {
        let h = sample();
        assert_eq!(Header::decode(&to_wire(&h)).unwrap(), h);
    }

    #[test]
    fn version_nibble_is_four() {
        // boringtun's validate_decapsulated_packet requires packet[0] >> 4 == 4.
        let mut buf = [0u8; HEADER_LEN];
        sample().encode(&mut buf).unwrap();
        assert_eq!(
            buf[0] >> 4,
            4,
            "inner packet must look like IPv4 (v1 design §4.1)"
        );
        assert_eq!(buf[0], 0x45);
    }

    #[test]
    fn total_len_is_big_endian_at_offset_two() {
        // boringtun reads bytes 2..4 as a big-endian u16.
        let mut buf = [0u8; HEADER_LEN];
        sample().encode(&mut buf).unwrap();
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 1200);
    }

    #[test]
    fn rejects_short_buffer() {
        assert_eq!(Header::decode(&[0x45; 19]), Err(HeaderError::TooShort));
    }

    #[test]
    fn rejects_bad_version() {
        let mut buf = [0u8; HEADER_LEN];
        sample().encode(&mut buf).unwrap();
        buf[0] = 0x65; // version 6
        assert_eq!(Header::decode(&buf), Err(HeaderError::BadVersion));
    }

    #[test]
    fn rejects_total_len_exceeding_buffer() {
        let mut buf = [0u8; HEADER_LEN];
        let mut h = sample();
        h.total_len = 9999;
        h.encode(&mut buf).unwrap();
        assert_eq!(Header::decode(&buf), Err(HeaderError::LengthMismatch));
    }

    #[test]
    fn rejects_frame_len_over_u24() {
        let mut buf = [0u8; HEADER_LEN];
        let mut h = sample();
        h.frame_len = 0x0100_0000;
        assert_eq!(h.encode(&mut buf), Err(HeaderError::FrameLenOverflow));
    }

    #[test]
    fn all_kinds_round_trip() {
        for kind in [Kind::Video, Kind::Audio, Kind::Input, Kind::Control] {
            let mut h = sample();
            h.kind = kind;
            assert_eq!(Header::decode(&to_wire(&h)).unwrap().kind, kind);
        }
    }
}
