//! Property tests over a simulated lossy link (v1 design §13.1).
//!
//! THE invariant: a frame either reconstructs byte-exactly, or is not
//! reported. Never silently corrupt.

use pingpong_proto::packetize::Packetizer;
use pingpong_proto::reassemble::Reassembler;
use pingpong_proto::PAYLOAD_LEN;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    /// Drop, reorder, and duplicate arbitrarily. Anything reported must be
    /// byte-exact.
    #[test]
    fn never_reports_a_corrupt_frame(
        frame_len in 1usize..(PAYLOAD_LEN * 6),
        drop_seed: u64,
        reorder_seed: u64,
        dup_count in 0usize..5,
        lan in any::<bool>(),
    ) {
        let original: Vec<u8> = (0..frame_len).map(|i| (i % 251) as u8).collect();
        let mut packetizer = Packetizer::new();
        packetizer.set_lan_shards(lan);
        let pkts = packetizer
            .packetize(&original, 42, 7, false, false)
            .unwrap();

        // Deterministic pseudo-random shuffle and drop.
        let mut idx: Vec<usize> = (0..pkts.len()).collect();
        let mut s = reorder_seed | 1;
        for i in (1..idx.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            idx.swap(i, (s >> 33) as usize % (i + 1));
        }
        let mut d = drop_seed | 1;
        let mut wire: Vec<&Vec<u8>> = Vec::new();
        for &i in &idx {
            d = d.wrapping_mul(6364136223846793005).wrapping_add(1);
            if (d >> 60) % 4 != 0 {   // ~75% delivery
                wire.push(&pkts[i]);
            }
        }
        for k in 0..dup_count {
            if !wire.is_empty() {
                wire.push(wire[k % wire.len()]);
            }
        }

        let mut r = Reassembler::new(8);
        for pkt in wire {
            if let Some(f) = r.push(pkt) {
                prop_assert_eq!(&f.data, &original, "reported frame must be exact");
                prop_assert_eq!(f.frame_id, 42);
            }
        }
    }

    /// All data shards present, in any order, always completes exactly.
    #[test]
    fn all_data_shards_always_completes(
        frame_len in 1usize..(PAYLOAD_LEN * 6),
        reorder_seed: u64,
    ) {
        let original: Vec<u8> = (0..frame_len).map(|i| (i % 251) as u8).collect();
        let pkts = Packetizer::new()
            .packetize(&original, 1, 0, false, false)
            .unwrap();
        let data_count = pkts
            .iter()
            .filter(|p| {
                let h = pingpong_proto::header::Header::decode(p).unwrap();
                (h.fragment_idx as usize) < h.data_shards as usize
            })
            .count();

        let mut order: Vec<usize> = (0..data_count).collect();
        let mut s = reorder_seed | 1;
        for i in (1..order.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
            order.swap(i, (s >> 33) as usize % (i + 1));
        }

        let mut r = Reassembler::new(8);
        let mut got = None;
        for &i in &order {
            if let Some(f) = r.push(&pkts[i]) {
                got = Some(f);
            }
        }
        prop_assert_eq!(got.map(|f| f.data), Some(original));
    }

    /// Arbitrary bytes must never panic the reassembler.
    #[test]
    fn arbitrary_input_never_panics(
        datagrams in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..1300), 0..40)
    ) {
        let mut r = Reassembler::new(8);
        for d in &datagrams {
            let _ = r.push(d);
        }
    }
}

/// A raised FEC policy rebuilds a frame that lost as many shards per block as
/// it has parity, in any pattern, including one burst at the start.
#[test]
fn raised_fec_rebuilds_a_burst() {
    use pingpong_proto::fec::FecPolicy;
    use pingpong_proto::header::Header;

    let policy = FecPolicy {
        percent: 50,
        min_parity: 6,
    };
    for frame_len in [PAYLOAD_LEN * 4, PAYLOAD_LEN * 12 + 17, PAYLOAD_LEN * 400] {
        let original: Vec<u8> = (0..frame_len).map(|i| (i * 7 % 251) as u8).collect();
        let mut p = Packetizer::new();
        p.set_fec(policy);
        let pkts = p.packetize(&original, 9, 1, false, false).unwrap();

        // Drop the first `parity` datagrams of every block: a burst as long
        // as the block can bear.
        let mut dropped_in_block = std::collections::HashMap::new();
        let wire: Vec<&Vec<u8>> = pkts
            .iter()
            .filter(|pkt| {
                let h = Header::decode(pkt).unwrap();
                assert_eq!(
                    h.parity_shards as usize,
                    policy.parity(h.data_shards as usize).max(6)
                );
                let n = dropped_in_block.entry(h.fec_block_idx).or_insert(0usize);
                if *n < h.parity_shards as usize {
                    *n += 1;
                    false
                } else {
                    true
                }
            })
            .collect();
        let mut r = Reassembler::new(8);
        let done = wire
            .into_iter()
            .find_map(|pkt| r.push(pkt))
            .expect("the frame is rebuilt");
        assert_eq!(done.data, original, "{frame_len} bytes");
    }
}
