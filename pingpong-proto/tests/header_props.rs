use pingpong_proto::header::{Header, Kind};
use pingpong_proto::HEADER_LEN;
use proptest::prelude::*;

fn kind_of(n: u8) -> Kind {
    match n % 4 {
        0 => Kind::Video,
        1 => Kind::Audio,
        2 => Kind::Input,
        _ => Kind::Control,
    }
}

proptest! {
    #[test]
    fn round_trip_any_header(
        keyframe: bool,
        frame_end: bool,
        kind_n: u8,
        total_len in (HEADER_LEN as u16)..=1200u16,
        fragment_idx: u16,
        data_shards: u8,
        parity_shards: u8,
        fec_block_idx: u8,
        frame_len in 0u32..=0x00FF_FFFFu32,
        capture_ts_us: u32,
        frame_id: u32,
    ) {
        let h = Header {
            keyframe, recovery: frame_end && keyframe, lan_shards: keyframe && !frame_end, frame_end, kind: kind_of(kind_n),
            total_len, fragment_idx, data_shards, parity_shards,
            fec_block_idx, frame_len, capture_ts_us, frame_id,
        };
        let mut buf = [0u8; HEADER_LEN];
        h.encode(&mut buf).unwrap();
        // Decode needs a buffer at least total_len long.
        let mut wire = vec![0u8; total_len as usize];
        wire[..HEADER_LEN].copy_from_slice(&buf);
        prop_assert_eq!(Header::decode(&wire).unwrap(), h);
    }

    /// Decode must never panic on arbitrary bytes -- it parses hostile
    /// network input (v1 design §13.1).
    #[test]
    fn decode_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..2000)) {
        let _ = Header::decode(&bytes);
    }
}
