//! Annex-B byte streams: finding the NAL units between start codes.
//!
//! This runs over every byte of every frame before it reaches the decoder, so
//! it is on the latency path: a byte-at-a-time loop cost ~180 µs for a 120 KB
//! frame on an M4 Pro. The start-code search is SIMD (`memchr`), which makes it
//! a few microseconds.

use memchr::memmem;

const START_CODE: &[u8] = &[0, 0, 1];

/// The NAL units of an Annex-B stream, split on 3- and 4-byte start codes.
/// A 4-byte start code's leading zero, and any trailing_zero_bytes, belong to
/// no NAL; empty NALs are skipped.
pub fn nal_units(data: &[u8]) -> NalUnits<'_> {
    let mut starts = memmem::find_iter(data, START_CODE);
    let next = starts.next().map(|i| i + START_CODE.len());
    NalUnits { data, starts, next }
}

pub struct NalUnits<'a> {
    data: &'a [u8],
    starts: memmem::FindIter<'a, 'static>,
    /// Where the NAL after the start code found last begins.
    next: Option<usize>,
}

impl<'a> Iterator for NalUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            let start = self.next?;
            let following = self.starts.next();
            let mut end = following.unwrap_or(self.data.len());
            self.next = following.map(|i| i + START_CODE.len());
            while end > start && self.data[end - 1] == 0 {
                end -= 1;
            }
            if end > start {
                return Some(&self.data[start..end]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte-at-a-time splitter this replaced, kept as the reference.
    fn reference(data: &[u8]) -> Vec<&[u8]> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                starts.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut nals = Vec::new();
        for (k, &start) in starts.iter().enumerate() {
            let mut end = if k + 1 < starts.len() {
                starts[k + 1] - 3
            } else {
                data.len()
            };
            while end > start && data[end - 1] == 0 {
                end -= 1;
            }
            if end > start {
                nals.push(&data[start..end]);
            }
        }
        nals
    }

    #[test]
    fn splits_on_both_start_code_lengths() {
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0xCC, 0xDD,
        ];
        let nals: Vec<_> = nal_units(&stream).collect();
        assert_eq!(
            nals,
            vec![&[0x67, 0xAA][..], &[0x68, 0xBB], &[0x65, 0xCC, 0xDD]]
        );
    }

    #[test]
    fn empty_and_startcode_free_input_yield_nothing() {
        assert_eq!(nal_units(&[]).count(), 0);
        assert_eq!(nal_units(&[1, 2, 3, 4]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 1]).count(), 0);
        assert_eq!(nal_units(&[0, 0, 1, 0, 0, 0, 1]).count(), 0);
    }

    #[test]
    fn trailing_zero_bytes_are_dropped() {
        let stream = [0, 0, 1, 0x65, 0x10, 0, 0, 0, 0, 0, 1, 0x41, 0, 0];
        let nals: Vec<_> = nal_units(&stream).collect();
        assert_eq!(nals, vec![&[0x65, 0x10][..], &[0x41]]);
    }

    #[test]
    fn matches_the_reference_on_arbitrary_bytes() {
        // Dense in zeros and ones, so start codes of both lengths, runs of
        // zeros and adjacent codes all occur.
        let mut seed = 0x9E37_79B9u32;
        for len in [0usize, 1, 2, 3, 4, 5, 17, 64, 1000, 4096, 100_000] {
            for _ in 0..20 {
                let data: Vec<u8> = (0..len)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        match seed % 8 {
                            0..=3 => 0,
                            4 | 5 => 1,
                            _ => (seed >> 8) as u8,
                        }
                    })
                    .collect();
                assert_eq!(
                    nal_units(&data).collect::<Vec<_>>(),
                    reference(&data),
                    "len {len}"
                );
            }
        }
    }
}
