//! What an encoder's H.264 output holds, read from its Annex-B bytes: whether
//! a frame is an IDR, and its SPS and PPS. Encoders that report neither
//! reliably (Media Foundation's, `mf.rs`) are checked by their output, which
//! is what the client's decoder reads too.

use pingpong_proto::annexb::nal_units;

/// What an access unit holds that matters to the host.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Units {
    /// An IDR slice (NAL type 5).
    pub idr: bool,
    /// A sequence parameter set (NAL type 7).
    pub sps: bool,
}

impl Units {
    pub fn summary(data: &[u8]) -> Units {
        let mut u = Units::default();
        for nal in nal_units(data) {
            match nal[0] & 0x1f {
                5 => u.idr = true,
                7 => u.sps = true,
                _ => {}
            }
        }
        u
    }
}

/// The SPS and PPS units of an access unit, each behind a four-byte start
/// code: what an IDR that comes without them needs in front of it.
pub fn parameter_sets(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nal_units(data) {
        if matches!(nal[0] & 0x1f, 7 | 8) {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPS: [u8; 4] = [0x67, 0x64, 0x00, 0x1f];
    const PPS: [u8; 3] = [0x68, 0xee, 0x3c];
    const IDR: [u8; 3] = [0x65, 0x88, 0x84];
    const P: [u8; 3] = [0x41, 0x9a, 0x02];

    /// `units` behind start codes of four and three bytes in turn, as
    /// encoders write them.
    fn stream(units: &[&[u8]]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, u) in units.iter().enumerate() {
            if i % 2 == 0 {
                out.push(0);
            }
            out.extend_from_slice(&[0, 0, 1]);
            out.extend_from_slice(u);
        }
        out
    }

    #[test]
    fn a_keyframe_is_an_idr_with_or_without_its_parameter_sets() {
        let with = Units::summary(&stream(&[&SPS, &PPS, &IDR]));
        assert_eq!(
            with,
            Units {
                idr: true,
                sps: true
            }
        );
        let without = Units::summary(&stream(&[&IDR]));
        assert_eq!(
            without,
            Units {
                idr: true,
                sps: false
            }
        );
    }

    #[test]
    fn a_predicted_frame_is_neither_an_idr_nor_a_sequence() {
        assert_eq!(Units::summary(&stream(&[&P])), Units::default());
    }

    #[test]
    fn parameter_sets_come_out_behind_four_byte_start_codes() {
        let mut want = vec![0, 0, 0, 1];
        want.extend_from_slice(&SPS);
        want.extend_from_slice(&[0, 0, 0, 1]);
        want.extend_from_slice(&PPS);
        assert_eq!(parameter_sets(&stream(&[&SPS, &PPS, &IDR])), want);
    }

    #[test]
    fn a_frame_without_parameter_sets_gives_none() {
        assert!(parameter_sets(&stream(&[&P])).is_empty());
        assert!(parameter_sets(&[]).is_empty());
    }
}
