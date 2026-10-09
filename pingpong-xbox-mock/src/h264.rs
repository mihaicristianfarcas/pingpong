//! The smallest H.264 encoder that makes a real stream: every macroblock
//! that changed is sent uncompressed (I_PCM), every one that did not is
//! skipped (P_Skip, which copies it from the frame before). No transforms,
//! no prediction to get wrong, so what the decoder shows is exactly what
//! the mock drew -- and the stream is still Constrained Baseline H.264 that
//! VideoToolbox, FFmpeg and D3D11VA decode, with keyframes on request, as
//! a console's encoder makes them.
//!
//! A test picture changes in a few places a frame (a moving bar, a
//! counter), so a P-frame is tens of kilobytes; a keyframe is the whole
//! picture uncompressed (640×368 4:2:0: 353 KB).
//!
//! Syntax from ITU-T H.264 (08/2021): §7.3.2.1 (SPS), §7.3.2.2 (PPS),
//! §7.3.3 (slice header), §7.3.4 and §7.3.5 (slice data, macroblock
//! layer), Annex E (VUI).

/// A picture in planar 4:2:0, at the coded size (whole macroblocks).
#[derive(Clone)]
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
}

impl Picture {
    /// A picture of `width` × `height` (rounded up to whole macroblocks),
    /// filled with one colour.
    pub fn new(width: usize, height: usize, (y, cb, cr): (u8, u8, u8)) -> Picture {
        let (w, h) = (width.div_ceil(16) * 16, height.div_ceil(16) * 16);
        Picture {
            width: w,
            height: h,
            y: vec![y; w * h],
            cb: vec![cb; w * h / 4],
            cr: vec![cr; w * h / 4],
        }
    }

    /// Paint a rectangle (clipped to the picture).
    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, (cy, cb, cr): (u8, u8, u8)) {
        let (x1, y1) = ((x + w).min(self.width), (y + h).min(self.height));
        for row in y.min(y1)..y1 {
            self.y[row * self.width + x.min(x1)..row * self.width + x1].fill(cy);
        }
        let cw = self.width / 2;
        for row in (y / 2)..y1.div_ceil(2) {
            let (a, b) = (row * cw + x / 2, row * cw + x1.div_ceil(2));
            self.cb[a..b].fill(cb);
            self.cr[a..b].fill(cr);
        }
    }

    fn mb_equal(&self, other: &Picture, mx: usize, my: usize) -> bool {
        let w = self.width;
        let luma = (0..16).all(|r| {
            let at = (my * 16 + r) * w + mx * 16;
            self.y[at..at + 16] == other.y[at..at + 16]
        });
        let cw = w / 2;
        luma && (0..8).all(|r| {
            let at = (my * 8 + r) * cw + mx * 8;
            self.cb[at..at + 8] == other.cb[at..at + 8]
                && self.cr[at..at + 8] == other.cr[at..at + 8]
        })
    }
}

/// Writes bits, most significant first.
struct Bits {
    out: Vec<u8>,
    cur: u8,
    n: u8,
}

impl Bits {
    fn new() -> Bits {
        Bits {
            out: Vec::new(),
            cur: 0,
            n: 0,
        }
    }

    fn bit(&mut self, b: bool) {
        self.cur = (self.cur << 1) | b as u8;
        self.n += 1;
        if self.n == 8 {
            self.out.push(self.cur);
            self.cur = 0;
            self.n = 0;
        }
    }

    fn bits(&mut self, v: u32, n: u8) {
        for i in (0..n).rev() {
            self.bit(v >> i & 1 == 1);
        }
    }

    /// Exp-Golomb, unsigned (§9.1).
    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros() as u8;
        self.bits(0, len - 1);
        for i in (0..len).rev() {
            self.bit(x >> i & 1 == 1);
        }
    }

    /// Exp-Golomb, signed (§9.1.1).
    fn se(&mut self, v: i32) {
        self.ue(if v > 0 {
            2 * v as u32 - 1
        } else {
            (-2 * v) as u32
        });
    }

    fn align_zero(&mut self) {
        while self.n != 0 {
            self.bit(false);
        }
    }

    /// Whole bytes, at a byte boundary.
    fn bytes(&mut self, b: &[u8]) {
        debug_assert_eq!(self.n, 0);
        self.out.extend_from_slice(b);
    }

    /// rbsp_trailing_bits (§7.3.2.11).
    fn finish(mut self) -> Vec<u8> {
        self.bit(true);
        self.align_zero();
        self.out
    }
}

/// Append a NAL unit with its start code, escaping the RBSP (§7.4.1:
/// 00 00 0x with x ≤ 3 gets an 03 in between).
fn nal(out: &mut Vec<u8>, ref_idc: u8, kind: u8, rbsp: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1, (ref_idc << 5) | kind]);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

const NAL_SLICE: u8 = 1;
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
/// I_PCM's mb_type: 25 in an I slice, 5 + 25 in a P slice (Tables 7-11, 7-13).
const MB_I_PCM_IN_I: u32 = 25;
const MB_I_PCM_IN_P: u32 = 30;
/// frame_num is 8 bits (log2_max_frame_num_minus4 = 4).
const FRAME_NUM_BITS: u8 = 8;

pub struct Encoder {
    /// The visible height (the coded height cropped).
    height: usize,
    mbs_x: usize,
    mbs_y: usize,
    last: Option<Picture>,
    frame_num: u32,
    idr_id: u32,
    keyframe_wanted: bool,
}

impl Encoder {
    /// An encoder for pictures of `width` × `height`; `width` a multiple of
    /// 16, `height` even (the coded height is rounded up and cropped).
    pub fn new(width: usize, height: usize) -> Encoder {
        assert!(width.is_multiple_of(16) && height.is_multiple_of(2));
        Encoder {
            height,
            mbs_x: width / 16,
            mbs_y: height.div_ceil(16),
            last: None,
            frame_num: 0,
            idr_id: 0,
            keyframe_wanted: true,
        }
    }

    /// The next frame is a keyframe.
    pub fn request_keyframe(&mut self) {
        self.keyframe_wanted = true;
    }

    /// Encode `pic`: one access unit, Annex B, and whether it is a keyframe.
    pub fn encode(&mut self, pic: &Picture) -> (Vec<u8>, bool) {
        assert_eq!((pic.width, pic.height), (self.mbs_x * 16, self.mbs_y * 16));
        let key = self.keyframe_wanted || self.last.is_none();
        self.keyframe_wanted = false;
        let mut out = Vec::new();
        if key {
            nal(&mut out, 3, NAL_SPS, &self.sps());
            nal(&mut out, 3, NAL_PPS, &pps());
            self.frame_num = 0;
            let slice = self.slice(pic, true);
            nal(&mut out, 3, NAL_IDR, &slice);
            self.idr_id ^= 1;
        } else {
            self.frame_num = (self.frame_num + 1) % (1 << FRAME_NUM_BITS);
            let slice = self.slice(pic, false);
            nal(&mut out, 2, NAL_SLICE, &slice);
        }
        self.last = Some(pic.clone());
        (out, key)
    }

    fn sps(&self) -> Vec<u8> {
        let mut b = Bits::new();
        b.bits(66, 8); // profile_idc: Baseline
        b.bits(0b1100_0000, 8); // constraint_set0 and 1: Constrained Baseline
        b.bits(31, 8); // level_idc 3.1
        b.ue(0); // seq_parameter_set_id
        b.ue(FRAME_NUM_BITS as u32 - 4); // log2_max_frame_num_minus4
        b.ue(2); // pic_order_cnt_type 2: output order is decode order
        b.ue(1); // max_num_ref_frames
        b.bit(false); // gaps_in_frame_num_value_allowed_flag
        b.ue(self.mbs_x as u32 - 1);
        b.ue(self.mbs_y as u32 - 1);
        b.bit(true); // frame_mbs_only_flag
        b.bit(true); // direct_8x8_inference_flag
        let crop = (self.mbs_y * 16 - self.height) / 2;
        b.bit(crop > 0); // frame_cropping_flag
        if crop > 0 {
            b.ue(0);
            b.ue(0);
            b.ue(0);
            b.ue(crop as u32); // in units of two rows (4:2:0, frames)
        }
        b.bit(true); // vui_parameters_present_flag
        b.bit(false); // aspect_ratio_info_present_flag
        b.bit(false); // overscan_info_present_flag
        b.bit(false); // video_signal_type_present_flag
        b.bit(false); // chroma_loc_info_present_flag
        b.bit(false); // timing_info_present_flag
        b.bit(false); // nal_hrd_parameters_present_flag
        b.bit(false); // vcl_hrd_parameters_present_flag
        b.bit(false); // pic_struct_present_flag
                      // No reordering and one frame of buffering: a decoder shows each
                      // frame as soon as it is decoded, as a console's stream says too.
        b.bit(true); // bitstream_restriction_flag
        b.bit(true); // motion_vectors_over_pic_boundaries_flag
        b.ue(0); // max_bytes_per_pic_denom
        b.ue(0); // max_bits_per_mb_denom
        b.ue(16); // log2_max_mv_length_horizontal
        b.ue(16); // log2_max_mv_length_vertical
        b.ue(0); // max_num_reorder_frames
        b.ue(1); // max_dec_frame_buffering
        b.finish()
    }

    fn slice(&self, pic: &Picture, idr: bool) -> Vec<u8> {
        let mut b = Bits::new();
        b.ue(0); // first_mb_in_slice
        b.ue(if idr { 7 } else { 5 }); // slice_type: all I, or all P
        b.ue(0); // pic_parameter_set_id
        b.bits(self.frame_num, FRAME_NUM_BITS);
        if idr {
            b.ue(self.idr_id);
        } else {
            b.bit(false); // num_ref_idx_active_override_flag
            b.bit(false); // ref_pic_list_modification_flag_l0
        }
        // dec_ref_pic_marking
        if idr {
            b.bit(false); // no_output_of_prior_pics_flag
            b.bit(false); // long_term_reference_flag
        } else {
            b.bit(false); // adaptive_ref_pic_marking_mode_flag: sliding window
        }
        b.se(0); // slice_qp_delta
                 // Deblocking off: an uncompressed macroblock beside a skipped one
                 // must stay exactly as drawn.
        b.ue(1); // disable_deblocking_filter_idc
        let last = self.last.as_ref().filter(|_| !idr);
        let mut skip = 0u32;
        for my in 0..self.mbs_y {
            for mx in 0..self.mbs_x {
                if last.is_some_and(|l| pic.mb_equal(l, mx, my)) {
                    skip += 1;
                    continue;
                }
                if idr {
                    b.ue(MB_I_PCM_IN_I);
                } else {
                    b.ue(skip);
                    skip = 0;
                    b.ue(MB_I_PCM_IN_P);
                }
                b.align_zero(); // pcm_alignment_zero_bit
                let w = pic.width;
                for r in 0..16 {
                    let at = (my * 16 + r) * w + mx * 16;
                    b.bytes(&pic.y[at..at + 16]);
                }
                let cw = w / 2;
                for plane in [&pic.cb, &pic.cr] {
                    for r in 0..8 {
                        let at = (my * 8 + r) * cw + mx * 8;
                        b.bytes(&plane[at..at + 8]);
                    }
                }
            }
        }
        if skip > 0 {
            b.ue(skip);
        }
        b.finish()
    }
}

fn pps() -> Vec<u8> {
    let mut b = Bits::new();
    b.ue(0); // pic_parameter_set_id
    b.ue(0); // seq_parameter_set_id
    b.bit(false); // entropy_coding_mode_flag: CAVLC
    b.bit(false); // bottom_field_pic_order_in_frame_present_flag
    b.ue(0); // num_slice_groups_minus1
    b.ue(0); // num_ref_idx_l0_default_active_minus1
    b.ue(0); // num_ref_idx_l1_default_active_minus1
    b.bit(false); // weighted_pred_flag
    b.bits(0, 2); // weighted_bipred_idc
    b.se(0); // pic_init_qp_minus26
    b.se(0); // pic_init_qs_minus26
    b.se(0); // chroma_qp_index_offset
    b.bit(true); // deblocking_filter_control_present_flag
    b.bit(false); // constrained_intra_pred_flag
    b.bit(false); // redundant_pic_cnt_present_flag
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_codes_are_the_standards() {
        let mut b = Bits::new();
        for v in [0, 1, 2, 3, 7] {
            b.ue(v);
        }
        // 1 010 011 00100 0001000, then the stop bit and padding.
        assert_eq!(b.finish(), vec![0b1010_0110, 0b0100_0001, 0b0001_0000]);
        let mut b = Bits::new();
        b.se(1);
        b.se(-1);
        b.se(0);
        // 010 011 1, then the stop bit: a whole byte.
        assert_eq!(b.finish(), vec![0b0100_1111]);
    }

    #[test]
    fn three_zero_bytes_are_escaped() {
        let mut out = Vec::new();
        nal(&mut out, 3, NAL_SLICE, &[0, 0, 0, 0, 0, 1, 7]);
        assert_eq!(&out[5..], &[0, 0, 3, 0, 0, 3, 0, 1, 7]);
    }

    #[test]
    fn an_unchanged_frame_is_all_skipped_and_tiny() {
        let pic = Picture::new(640, 360, (40, 128, 128));
        let mut enc = Encoder::new(640, 360);
        let (key, is_key) = enc.encode(&pic);
        assert!(is_key);
        // Every macroblock uncompressed: 40 × 23 × 384 bytes and change.
        assert!(key.len() > 40 * 23 * 384);
        let (p, is_key) = enc.encode(&pic);
        assert!(!is_key);
        assert!(p.len() < 16, "{} bytes", p.len());
        let mut moved = pic.clone();
        moved.fill(100, 100, 16, 16, (235, 128, 128));
        let (p, _) = enc.encode(&moved);
        // The square straddles four macroblocks.
        assert!(p.len() > 4 * 384 && p.len() < 5 * 384, "{} bytes", p.len());
        enc.request_keyframe();
        assert!(enc.encode(&moved).1);
    }
}
