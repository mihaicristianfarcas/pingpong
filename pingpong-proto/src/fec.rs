//! Reed-Solomon parameters and block splitting. See v1 design §6.2 and §6.3.
//!
//! THIS IS THE ONLY FILE THAT TALKS TO THE REED-SOLOMON CRATE.
//!
//! Three crate constraints drive this module (v1 design §6.3):
//!   1. every shard must be the same size -- the caller zero-pads the last
//!   2. shard size must be non-zero and even
//!   3. not every (data, parity) pair is supported -- must be checked

use reed_solomon_simd::engine::DefaultEngine;
use reed_solomon_simd::rate::{DefaultRate, Rate};

/// Data shards per FEC block. Reed-Solomon caps at 255 total shards; 200
/// leaves headroom for parity (v1 design §6.2).
pub const MAX_DATA_SHARDS_PER_BLOCK: usize = 200;

/// Upper bound on the parity search in [`supported_parity`], so a pathological
/// input cannot loop forever.
const PARITY_SEARCH_LIMIT: usize = 255;

/// Data + parity shards in one block: the header counts each in a byte, and
/// the block's shards share one 8-bit index space on the client, which
/// drops a block claiming more.
pub const MAX_SHARDS_PER_BLOCK: usize = 255;

/// How much parity each block carries. The default is Moonlight's 20% with a
/// floor of 2; the host raises both on a link that loses packets whatever
/// the rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FecPolicy {
    /// Parity per data shard, in percent (rounded up).
    pub percent: u8,
    /// Parity shards per block at least.
    ///
    /// The floor matters: a static-scene P-frame can be one packet, where a
    /// pure ratio rounds to zero parity -- leaving the frame unprotected
    /// exactly when protection is cheapest.
    pub min_parity: u8,
}

impl FecPolicy {
    pub const DEFAULT: FecPolicy = FecPolicy {
        percent: 20,
        min_parity: 2,
    };

    /// The most parity a host sends: Pong raises its policy no further on
    /// the lossiest link (`pong/src/bitrate.rs`), and the client's
    /// reassembly budget holds two of the largest frames carrying this much
    /// (`reassemble.rs`).
    pub const MAX: FecPolicy = FecPolicy {
        percent: 60,
        min_parity: 8,
    };

    pub fn parity(self, data_shards: usize) -> usize {
        (data_shards * self.percent as usize)
            .div_ceil(100)
            .max(self.min_parity as usize)
    }

    /// Data shards per block that leave room for this policy's parity.
    pub fn max_data_per_block(self) -> usize {
        let mut n = MAX_DATA_SHARDS_PER_BLOCK;
        while n > 1 && n + self.parity(n) > MAX_SHARDS_PER_BLOCK {
            n -= 1;
        }
        n
    }

    /// Packed into a u16 (for an atomic the sender thread reads).
    pub fn to_bits(self) -> u16 {
        (self.percent as u16) << 8 | self.min_parity as u16
    }

    pub fn from_bits(bits: u16) -> FecPolicy {
        FecPolicy {
            percent: (bits >> 8) as u8,
            min_parity: bits as u8,
        }
    }
}

impl Default for FecPolicy {
    fn default() -> Self {
        FecPolicy::DEFAULT
    }
}

/// 20% parity, rounded up, with a floor of 2 (v1 design §6.2): the default
/// [`FecPolicy`].
pub fn parity_for(data_shards: usize) -> usize {
    FecPolicy::DEFAULT.parity(data_shards)
}

/// Smallest supported parity count >= `wanted`, or `None` if none is found
/// within the search limit.
///
/// Never assume [`parity_for`] yields a supported pair -- this crate is
/// Leopard-RS based and rejects some combinations (v1 design §6.3).
pub fn supported_parity(data_shards: usize, wanted: usize) -> Option<usize> {
    (wanted..=PARITY_SEARCH_LIMIT).find(|&parity| {
        <DefaultRate<DefaultEngine> as Rate<DefaultEngine>>::supports(data_shards, parity)
    })
}

/// Split `total_shards` data shards into balanced blocks, each at most
/// [`MAX_DATA_SHARDS_PER_BLOCK`]. Each block is FEC-encoded and recovered
/// independently and carries its own `fec_block_idx`.
pub fn block_sizes(total_shards: usize) -> Vec<usize> {
    block_sizes_max(total_shards, MAX_DATA_SHARDS_PER_BLOCK)
}

/// [`block_sizes`] with at most `max_data` data shards per block.
pub fn block_sizes_max(total_shards: usize, max_data: usize) -> Vec<usize> {
    if total_shards == 0 {
        return Vec::new();
    }
    let block_count = total_shards.div_ceil(max_data.max(1));
    let base = total_shards / block_count;
    let remainder = total_shards % block_count;
    (0..block_count)
        .map(|i| base + usize::from(i < remainder))
        .collect()
}

/// Pay reed-solomon-simd's one-time table setup (about 6 ms) now, not on the
/// first keyframe of a session.
pub fn warm_up() {
    use reed_solomon_simd::{ReedSolomonDecoder, ReedSolomonEncoder};
    let shard = [0u8; 64];
    let Ok(mut enc) = ReedSolomonEncoder::new(4, 2, shard.len()) else {
        return;
    };
    for _ in 0..4 {
        let _ = enc.add_original_shard(shard);
    }
    let recovery: Vec<Vec<u8>> = match enc.encode() {
        Ok(r) => r.recovery_iter().map(<[u8]>::to_vec).collect(),
        Err(_) => return,
    };
    if let Ok(mut dec) = ReedSolomonDecoder::new(4, 2, shard.len()) {
        for i in 1..4 {
            let _ = dec.add_original_shard(i, shard);
        }
        let _ = dec.add_recovery_shard(0, &recovery[0]);
        let _ = dec.decode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_is_twenty_percent_rounded_up() {
        assert_eq!(parity_for(36), 8); // v1 design §6.1: 1080p60 average frame
        assert_eq!(parity_for(31), 7); // v1 design §6.1: 1080p120
        assert_eq!(parity_for(30), 6);
        assert_eq!(parity_for(45), 9);
        assert_eq!(parity_for(106), 22);
    }

    #[test]
    fn parity_never_below_two() {
        // The floor is load-bearing: a static-scene P-frame can be a single
        // packet, where 20% rounds to zero parity (v1 design §6.2).
        assert_eq!(parity_for(1), 2);
        assert_eq!(parity_for(2), 2);
        assert_eq!(parity_for(5), 2);
        assert_eq!(parity_for(6), 2);
    }

    #[test]
    fn supported_parity_returns_a_supported_pair() {
        use reed_solomon_simd::engine::DefaultEngine;
        use reed_solomon_simd::rate::{DefaultRate, Rate};
        for data in 1..=250usize {
            let wanted = parity_for(data);
            let got = supported_parity(data, wanted).expect("some parity is supported");
            assert!(got >= wanted, "must not reduce protection below the target");
            assert!(
                <DefaultRate<DefaultEngine> as Rate<DefaultEngine>>::supports(data, got),
                "data={data} parity={got} must be a supported combination"
            );
        }
    }

    #[test]
    fn a_raised_policy_still_fits_the_header() {
        for policy in [
            FecPolicy::DEFAULT,
            FecPolicy {
                percent: 35,
                min_parity: 4,
            },
            FecPolicy {
                percent: 60,
                min_parity: 8,
            },
        ] {
            let max = policy.max_data_per_block();
            for total in [1, 7, 150, 400, 1000] {
                for data in block_sizes_max(total, max) {
                    let parity = supported_parity(data, policy.parity(data)).unwrap();
                    assert!(data + parity <= 255, "{policy:?}: {data} + {parity}");
                }
            }
        }
        assert_eq!(
            FecPolicy::DEFAULT.max_data_per_block(),
            MAX_DATA_SHARDS_PER_BLOCK
        );
        assert_eq!(FecPolicy::MAX.max_data_per_block(), 159);
        assert_eq!(
            FecPolicy {
                percent: 50,
                min_parity: 6
            }
            .parity(4),
            6
        );
    }

    #[test]
    fn a_policy_survives_the_trip_through_bits() {
        let p = FecPolicy {
            percent: 47,
            min_parity: 5,
        };
        assert_eq!(FecPolicy::from_bits(p.to_bits()), p);
    }

    #[test]
    fn small_frames_use_a_single_block() {
        assert_eq!(block_sizes(1), vec![1]);
        assert_eq!(block_sizes(36), vec![36]);
        assert_eq!(block_sizes(200), vec![200]);
    }

    #[test]
    fn large_frames_split_into_balanced_blocks() {
        // 340 shards (a 4K keyframe) exceeds one block; RS caps at 255 total
        // shards and we cap data at 200 to leave parity headroom (v1 design §6.2).
        let blocks = block_sizes(340);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks.iter().sum::<usize>(), 340);
        assert!(blocks.iter().all(|&b| b <= MAX_DATA_SHARDS_PER_BLOCK));
        assert!(blocks[0] - blocks[1] <= 1, "blocks should be balanced");
    }

    #[test]
    fn block_count_scales() {
        assert_eq!(block_sizes(201).len(), 2);
        assert_eq!(block_sizes(600).len(), 3);
        for total in [1usize, 55, 200, 201, 340, 600, 1000] {
            let blocks = block_sizes(total);
            assert_eq!(blocks.iter().sum::<usize>(), total);
            assert!(blocks
                .iter()
                .all(|&b| b > 0 && b <= MAX_DATA_SHARDS_PER_BLOCK));
        }
    }
}
