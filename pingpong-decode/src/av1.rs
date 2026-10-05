//! AV1's open bitstream units (OBUs), as far as a decoder that is not
//! handed a container needs them: VideoToolbox wants a format description
//! carrying an `av1C` record (the AV1 codec configuration of the ISO media
//! file format: profile, level, tier, bit depth and chroma, then the
//! sequence header OBU), and samples as MP4 carries them, without temporal
//! delimiters. The host's encoders send each frame as one temporal unit in
//! the low-overhead format (every OBU with its size).
//!
//! Pure, so it is tested on any machine. Hostile input never panics: a
//! malformed unit ends the walk, a malformed sequence header is `None`.

/// OBU types (AV1 spec, 6.2.2).
pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_PADDING: u8 = 15;

/// One OBU: its type, and where it lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obu<'a> {
    pub kind: u8,
    /// The whole unit, header and size included.
    pub whole: &'a [u8],
    pub payload: &'a [u8],
}

/// A LEB128 number and its length in bytes.
fn leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &b) in data.iter().take(8).enumerate() {
        value |= ((b & 0x7F) as u64) << (i * 7);
        if b & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// The OBUs of a temporal unit in the low-overhead format. A unit without
/// its size is the last one (it runs to the end).
pub fn obus(data: &[u8]) -> impl Iterator<Item = Obu<'_>> {
    let mut rest = data;
    std::iter::from_fn(move || {
        let &header = rest.first()?;
        if header & 0x80 != 0 {
            // The forbidden bit: not an OBU.
            rest = &[];
            return None;
        }
        let kind = (header >> 3) & 0x0F;
        let extension = header & 0x04 != 0;
        let has_size = header & 0x02 != 0;
        let mut at = 1 + extension as usize;
        if at > rest.len() {
            rest = &[];
            return None;
        }
        let size = if has_size {
            let (size, n) = leb128(rest.get(at..)?).or_else(|| {
                rest = &[];
                None
            })?;
            at += n;
            usize::try_from(size).ok()?
        } else {
            rest.len() - at
        };
        let Some(end) = at.checked_add(size).filter(|&e| e <= rest.len()) else {
            rest = &[];
            return None;
        };
        let obu = Obu {
            kind,
            whole: &rest[..end],
            payload: &rest[at..end],
        };
        rest = &rest[end..];
        Some(obu)
    })
}

/// What a sequence header says that a decoder is set up by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SequenceHeader {
    pub profile: u8,
    /// The first operating point's level and tier.
    pub level: u8,
    pub tier: u8,
    pub high_bitdepth: bool,
    pub twelve_bit: bool,
    pub monochrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub chroma_sample_position: u8,
    pub max_width: u32,
    pub max_height: u32,
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            let bit = (byte >> (7 - self.pos % 8)) & 1;
            v = (v << 1) | bit as u32;
            self.pos += 1;
        }
        Some(v)
    }

    fn flag(&mut self) -> Option<bool> {
        self.bits(1).map(|b| b == 1)
    }

    /// `uvlc()` (4.10.3).
    fn uvlc(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while !self.flag()? {
            zeros += 1;
            if zeros >= 32 {
                return Some(u32::MAX);
            }
        }
        Some(self.bits(zeros)? + ((1u64 << zeros) - 1) as u32)
    }
}

/// `sequence_header_obu()` (5.5), as far as its colour config.
pub fn parse_sequence_header(payload: &[u8]) -> Option<SequenceHeader> {
    let mut b = Bits {
        data: payload,
        pos: 0,
    };
    let mut h = SequenceHeader {
        profile: b.bits(3)? as u8,
        ..Default::default()
    };
    let _still_picture = b.flag()?;
    let reduced = b.flag()?;
    if reduced {
        h.level = b.bits(5)? as u8;
    } else {
        let timing_info = b.flag()?;
        let mut decoder_model = false;
        let mut buffer_delay_bits = 0;
        if timing_info {
            b.bits(32)?;
            b.bits(32)?;
            if b.flag()? {
                b.uvlc()?;
            }
            decoder_model = b.flag()?;
            if decoder_model {
                buffer_delay_bits = b.bits(5)? + 1;
                b.bits(32)?;
                b.bits(5)?;
                b.bits(5)?;
            }
        }
        let initial_display_delay = b.flag()?;
        let points = b.bits(5)? + 1;
        for i in 0..points {
            b.bits(12)?;
            let level = b.bits(5)? as u8;
            let tier = if level > 7 { b.bits(1)? as u8 } else { 0 };
            if i == 0 {
                h.level = level;
                h.tier = tier;
            }
            if decoder_model && b.flag()? {
                b.bits(buffer_delay_bits)?;
                b.bits(buffer_delay_bits)?;
                b.bits(1)?;
            }
            if initial_display_delay && b.flag()? {
                b.bits(4)?;
            }
        }
    }
    let width_bits = b.bits(4)? + 1;
    let height_bits = b.bits(4)? + 1;
    h.max_width = b.bits(width_bits)? + 1;
    h.max_height = b.bits(height_bits)? + 1;
    if !reduced && b.flag()? {
        b.bits(4)?;
        b.bits(3)?;
    }
    b.bits(3)?; // 128x128 superblocks, filter intra, intra edge filter
    if !reduced {
        b.bits(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = b.flag()?;
        if order_hint {
            b.bits(2)?; // jnt_comp, ref_frame_mvs
        }
        let screen_content = if b.flag()? { 2 } else { b.bits(1)? };
        if screen_content > 0 && !b.flag()? {
            b.bits(1)?; // seq_force_integer_mv
        }
        if order_hint {
            b.bits(3)?;
        }
    }
    b.bits(3)?; // superres, cdef, restoration
                // color_config()
    h.high_bitdepth = b.flag()?;
    if h.profile == 2 && h.high_bitdepth {
        h.twelve_bit = b.flag()?;
    }
    h.monochrome = h.profile != 1 && b.flag()?;
    let (mut primaries, mut transfer, mut matrix) = (2, 2, 2);
    if b.flag()? {
        primaries = b.bits(8)?;
        transfer = b.bits(8)?;
        matrix = b.bits(8)?;
    }
    if h.monochrome {
        h.subsampling_x = true;
        h.subsampling_y = true;
        return Some(h);
    }
    if primaries == 1 && transfer == 13 && matrix == 0 {
        // sRGB: 4:4:4, full range.
        return Some(h);
    }
    b.bits(1)?; // colour range
    match h.profile {
        0 => {
            h.subsampling_x = true;
            h.subsampling_y = true;
        }
        1 => {}
        _ if h.twelve_bit => {
            h.subsampling_x = b.flag()?;
            if h.subsampling_x {
                h.subsampling_y = b.flag()?;
            }
        }
        _ => h.subsampling_x = true,
    }
    if h.subsampling_x && h.subsampling_y {
        h.chroma_sample_position = b.bits(2)? as u8;
    }
    Some(h)
}

/// The `av1C` record for a stream with this sequence header OBU (`whole`,
/// as it came) and what it says.
pub fn av1c(sequence_header: &[u8], h: &SequenceHeader) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + sequence_header.len());
    // marker 1, version 1
    out.push(0x81);
    out.push((h.profile & 0x07) << 5 | (h.level & 0x1F));
    out.push(
        (h.tier & 1) << 7
            | (h.high_bitdepth as u8) << 6
            | (h.twelve_bit as u8) << 5
            | (h.monochrome as u8) << 4
            | (h.subsampling_x as u8) << 3
            | (h.subsampling_y as u8) << 2
            | (h.chroma_sample_position & 0x03),
    );
    // No initial presentation delay.
    out.push(0);
    out.extend_from_slice(sequence_header);
    out
}

/// Whether an OBU belongs in a sample as MP4 stores it: temporal
/// delimiters and padding do not.
pub fn in_sample(kind: u8) -> bool {
    kind != OBU_TEMPORAL_DELIMITER && kind != OBU_PADDING
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three frames of FFmpeg's testsrc2 at 320x180 from SVT-AV1, in the
    /// low-overhead format (see `tests/decode_formats.rs`).
    const STREAM: &[u8] = include_bytes!("../tests/fixtures/av1-420-8.obu");

    #[test]
    fn a_temporal_unit_is_walked_obu_by_obu() {
        let units: Vec<_> = obus(STREAM).collect();
        assert_eq!(units[0].kind, OBU_TEMPORAL_DELIMITER);
        assert_eq!(units[1].kind, OBU_SEQUENCE_HEADER);
        // The walk covers the whole stream, unit after unit.
        let total: usize = units.iter().map(|u| u.whole.len()).sum();
        assert_eq!(total, STREAM.len());
        assert_eq!(
            units
                .iter()
                .filter(|u| u.kind == OBU_TEMPORAL_DELIMITER)
                .count(),
            3
        );
    }

    #[test]
    fn the_sequence_header_says_main_profile_8_bit_420() {
        let seq = obus(STREAM)
            .find(|u| u.kind == OBU_SEQUENCE_HEADER)
            .unwrap();
        let h = parse_sequence_header(seq.payload).unwrap();
        assert_eq!(h.profile, 0);
        assert!(!h.high_bitdepth && !h.monochrome);
        assert!(h.subsampling_x && h.subsampling_y);
        assert_eq!((h.max_width, h.max_height), (320, 180));
        let record = av1c(seq.whole, &h);
        assert_eq!(record[0], 0x81);
        assert_eq!(record[2] & 0x0C, 0x0C, "4:2:0 in av1C");
        assert_eq!(&record[4..], seq.whole);
    }

    #[test]
    fn hostile_input_ends_the_walk_without_a_panic() {
        assert_eq!(obus(&[]).count(), 0);
        assert_eq!(obus(&[0x80, 1, 2]).count(), 0);
        // A size past the end.
        assert_eq!(obus(&[0x0A, 0x7F, 0]).count(), 0);
        // A size that never ends.
        assert_eq!(obus(&[0x0A, 0xFF, 0xFF, 0xFF]).count(), 0);
        assert_eq!(parse_sequence_header(&[]), None);
        assert_eq!(parse_sequence_header(&[0xFF; 3]), None);
        for n in 0..64 {
            let _ = parse_sequence_header(&STREAM[2..(2 + n).min(STREAM.len())]);
        }
    }
}
