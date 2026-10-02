//! A captured device's channels in the order the stream carries them, and at
//! the stream's rate. The stream's order is Windows' (FL FR FC LFE BL BR SL
//! SR, see [`crate::opus::layout`]); a Mac's output device says which
//! speaker each of its channels feeds with Core Audio's `AudioChannelLabel`s,
//! in an `AudioChannelLayout` (its "Configure Speakers" in Audio MIDI
//! Setup). Pure: the Mac host's capture (`tap`) uses it, the tests run
//! anywhere.
//!
//! Two surround pairs are told apart as Windows tells them: a 5.1 device's
//! surround pair (Core Audio's Ls/Rs) is the stream's back pair, as in
//! Windows' 5.1 and Moonlight's; a 7.1 device's rear pair (Rls/Rrs) is the
//! back pair and its Ls/Rs the side pair.

use pingpong_proto::audio::SAMPLE_RATE;

/// Core Audio's speaker labels (`kAudioChannelLabel_*`), the ones that
/// place a channel.
pub mod label {
    pub const LEFT: u32 = 1;
    pub const RIGHT: u32 = 2;
    pub const CENTER: u32 = 3;
    pub const LFE: u32 = 4;
    pub const LEFT_SURROUND: u32 = 5;
    pub const RIGHT_SURROUND: u32 = 6;
    pub const LEFT_SURROUND_DIRECT: u32 = 10;
    pub const RIGHT_SURROUND_DIRECT: u32 = 11;
    pub const REAR_SURROUND_LEFT: u32 = 33;
    pub const REAR_SURROUND_RIGHT: u32 = 34;
    pub const LFE2: u32 = 37;
    /// A channel that says nothing about where it goes.
    pub const UNKNOWN: u32 = 0xFFFF_FFFF;
}

/// The labels a device's channels have when it does not say: Core Audio's
/// usual order for 5.1 and 7.1 (`kAudioChannelLayoutTag_MPEG_7_1_C`).
const BY_POSITION: [u32; 8] = [
    label::LEFT,
    label::RIGHT,
    label::CENTER,
    label::LFE,
    label::LEFT_SURROUND,
    label::RIGHT_SURROUND,
    label::REAR_SURROUND_LEFT,
    label::REAR_SURROUND_RIGHT,
];

/// `kAudioChannelLayoutTag_UseChannelDescriptions`, `_UseChannelBitmap`.
const TAG_DESCRIPTIONS: u32 = 0;
const TAG_BITMAP: u32 = 1 << 16;

/// The labels of a device's `channels` channels, from its preferred
/// `AudioChannelLayout` as Core Audio hands it over (native-endian bytes:
/// tag, bitmap, count, then 20-byte descriptions whose first field is the
/// label). Channels it does not place are [`label::UNKNOWN`].
pub fn labels_from_layout(bytes: &[u8], channels: usize) -> Vec<u32> {
    let word = |i: usize| -> Option<u32> {
        bytes
            .get(i * 4..i * 4 + 4)
            .map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
    };
    let mut labels = vec![label::UNKNOWN; channels];
    let Some(tag) = word(0) else {
        return labels;
    };
    match tag {
        TAG_DESCRIPTIONS => {
            let count = word(2).unwrap_or(0) as usize;
            for (i, slot) in labels.iter_mut().enumerate().take(count) {
                // Each description is five words; its label is the first.
                if let Some(l) = word(3 + i * 5) {
                    *slot = l;
                }
            }
        }
        TAG_BITMAP => {
            // Bit n is label n + 1 for the first eleven speakers, the
            // channels in bit order.
            let bitmap = word(1).unwrap_or(0);
            let placed = (0..11).filter(|b| bitmap & (1 << b) != 0).map(|b| b + 1);
            for (slot, l) in labels.iter_mut().zip(placed) {
                *slot = l;
            }
        }
        _ => {
            if let Some(known) = tag_labels(tag) {
                for (slot, &l) in labels.iter_mut().zip(known) {
                    *slot = l;
                }
            }
        }
    }
    labels
}

/// The labels of the layout tags a device may report for surround.
fn tag_labels(tag: u32) -> Option<&'static [u32]> {
    use label::*;
    const STEREO: u32 = (101 << 16) | 2;
    const MPEG_5_1_A: u32 = (121 << 16) | 6;
    const MPEG_5_1_C: u32 = (123 << 16) | 6;
    const MPEG_7_1_C: u32 = (128 << 16) | 8;
    Some(match tag {
        STEREO => &[LEFT, RIGHT],
        MPEG_5_1_A => &BY_POSITION[..6],
        MPEG_5_1_C => &[LEFT, CENTER, RIGHT, LEFT_SURROUND, RIGHT_SURROUND, LFE],
        MPEG_7_1_C => &BY_POSITION,
        _ => return None,
    })
}

/// `kAudioChannelLabel_Unused`: a channel no speaker plays.
const UNUSED: u32 = 0;

/// An `AudioChannelLayout` naming the speakers of 5.1 (6) or 7.1 (8), in
/// Core Audio's order, on a device with `device_channels` channels, to set
/// as its speaker configuration (native-endian, as Core Audio takes it).
/// Core Audio takes a layout only when it describes every channel the
/// device has (BlackHole 16ch: a layout of 8 is refused as the wrong size,
/// one of 16 taken); the ones past the session's are unused.
pub fn surround_layout(channels: u8, device_channels: usize) -> Vec<u8> {
    let labels = &BY_POSITION[..(channels as usize).min(8)];
    let count = device_channels.max(labels.len());
    let mut out = Vec::with_capacity(12 + count * 20);
    for w in [TAG_DESCRIPTIONS, 0, count as u32] {
        out.extend_from_slice(&w.to_ne_bytes());
    }
    for i in 0..count {
        out.extend_from_slice(&labels.get(i).copied().unwrap_or(UNUSED).to_ne_bytes());
        // Flags and coordinates: none.
        out.extend_from_slice(&[0u8; 16]);
    }
    out
}

/// For each of the stream's channels, the device channel it is taken from
/// (`None`: silence).
pub type Plan = [Option<usize>; 8];

/// Where each of the stream's `wire` channels (2, 6 or 8) comes from on a
/// device whose channels have `labels`. A device that labels nothing is
/// read in Core Audio's usual order.
pub fn plan(labels: &[u32], wire: u8) -> Plan {
    let labels: Vec<u32> = if speakers_set(labels) {
        labels.to_vec()
    } else {
        (0..labels.len())
            .map(|i| BY_POSITION.get(i).copied().unwrap_or(label::UNKNOWN))
            .collect()
    };
    let at = |l: u32| labels.iter().position(|&x| x == l);
    let pair = |a: u32, b: u32| at(a).zip(at(b));
    let surround = pair(label::LEFT_SURROUND, label::RIGHT_SURROUND);
    let rear = pair(label::REAR_SURROUND_LEFT, label::REAR_SURROUND_RIGHT);
    let direct = pair(label::LEFT_SURROUND_DIRECT, label::RIGHT_SURROUND_DIRECT);
    let mut plan: Plan = [None; 8];
    plan[0] = at(label::LEFT);
    plan[1] = at(label::RIGHT);
    if wire <= 2 {
        return plan;
    }
    plan[2] = at(label::CENTER);
    plan[3] = at(label::LFE).or_else(|| at(label::LFE2));
    let (back, side) = if wire >= 8 {
        match (rear, surround, direct) {
            (Some(r), s, d) => (Some(r), s.or(d)),
            (None, Some(s), Some(d)) => (Some(s), Some(d)),
            // One surround pair: the sides, where a 5.1 room has it.
            (None, s, d) => (None, s.or(d)),
        }
    } else {
        (surround.or(rear).or(direct), None)
    };
    if let Some((l, r)) = back {
        plan[4] = Some(l);
        plan[5] = Some(r);
    }
    if let Some((l, r)) = side {
        plan[6] = Some(l);
        plan[7] = Some(r);
    }
    plan
}

/// Whether a device's speakers are those [`surround_layout`] sets for
/// `channels`: those, in that order, and no other.
pub fn is_layout(labels: &[u32], channels: u8) -> bool {
    let n = (channels as usize).min(8);
    labels.len() >= n
        && labels[..n] == BY_POSITION[..n]
        && labels[n..]
            .iter()
            .all(|&l| l == UNUSED || l == label::UNKNOWN)
}

/// Whether the device says where its speakers are (its speakers were set
/// up), rather than leaving it to Core Audio's usual order.
pub fn speakers_set(labels: &[u32]) -> bool {
    labels
        .iter()
        .any(|&l| l != label::UNKNOWN && BY_POSITION.contains(&l))
}

/// The most channels the stream can carry from a device with `labels`: 8
/// with two surround pairs, 6 with one, else 2.
pub fn surround_channels(labels: &[u32]) -> u8 {
    let full = plan(labels, 8);
    match (full[4].is_some(), full[6].is_some()) {
        (true, true) => 8,
        (true, false) | (false, true) => 6,
        (false, false) => 2,
    }
}

/// Append `frames` of `input` (interleaved, `in_channels` a frame) to `out`
/// in the stream's order, `wire` channels a frame.
pub fn remap(input: &[f32], in_channels: usize, plan: &Plan, wire: usize, out: &mut Vec<f32>) {
    if in_channels == 0 {
        return;
    }
    for frame in input.chunks_exact(in_channels) {
        for take in &plan[..wire] {
            out.push(take.and_then(|c| frame.get(c).copied()).unwrap_or(0.0));
        }
    }
}

/// From a device's rate to the stream's 48 kHz, by linear interpolation
/// between neighbouring frames, carried across calls. A device at 48 kHz
/// (most: Core Audio's own default) passes through untouched.
pub struct Resampler {
    channels: usize,
    /// Input frames per output frame.
    step: f64,
    /// Where the next output frame falls, in input frames from `last`.
    at: f64,
    /// The previous call's last frame (interpolation reaches back to it).
    last: Vec<f32>,
}

impl Resampler {
    pub fn new(from_rate: f64, channels: usize) -> Resampler {
        Resampler {
            channels,
            step: from_rate / SAMPLE_RATE as f64,
            at: 1.0,
            last: vec![0.0; channels],
        }
    }

    pub fn passes_through(&self) -> bool {
        (self.step - 1.0).abs() < 1e-9
    }

    /// Append `input` (interleaved, `channels` a frame) at 48 kHz to `out`.
    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let c = self.channels;
        if c == 0 {
            return;
        }
        if self.passes_through() {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / c;
        // Frame k of the virtual sequence `last, input...` (k = 0 is last).
        let frame = |k: usize, ch: usize| -> f32 {
            if k == 0 {
                self.last[ch]
            } else {
                input[(k - 1) * c + ch]
            }
        };
        while self.at <= frames as f64 {
            let k = self.at.floor() as usize;
            let t = (self.at - k as f64) as f32;
            for ch in 0..c {
                let a = frame(k, ch);
                let b = if t > 0.0 { frame(k + 1, ch) } else { a };
                out.push(a + (b - a) * t);
            }
            self.at += self.step;
        }
        self.at -= frames as f64;
        if frames > 0 {
            self.last
                .copy_from_slice(&input[(frames - 1) * c..frames * c]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::label::*;
    use super::*;

    fn descriptions(labels: &[u32]) -> Vec<u8> {
        let mut b = Vec::new();
        for w in [TAG_DESCRIPTIONS, 0, labels.len() as u32] {
            b.extend_from_slice(&w.to_ne_bytes());
        }
        for l in labels {
            b.extend_from_slice(&l.to_ne_bytes());
            b.extend_from_slice(&[0; 16]);
        }
        b
    }

    #[test]
    fn a_layout_is_read_by_descriptions_bitmap_or_tag() {
        assert_eq!(
            labels_from_layout(&descriptions(&[LEFT, RIGHT, CENTER]), 4),
            [LEFT, RIGHT, CENTER, UNKNOWN]
        );
        let mut bitmap = Vec::new();
        for w in [TAG_BITMAP, 0b11_1011, 0] {
            bitmap.extend_from_slice(&u32::to_ne_bytes(w));
        }
        assert_eq!(
            labels_from_layout(&bitmap, 5),
            [LEFT, RIGHT, LFE, LEFT_SURROUND, RIGHT_SURROUND]
        );
        let tag = ((123u32 << 16) | 6).to_ne_bytes();
        assert_eq!(
            labels_from_layout(&tag, 6),
            [LEFT, CENTER, RIGHT, LEFT_SURROUND, RIGHT_SURROUND, LFE]
        );
        assert_eq!(labels_from_layout(&[1, 2], 2), [UNKNOWN, UNKNOWN]);
    }

    #[test]
    fn the_layout_set_on_a_device_reads_back_as_itself() {
        assert_eq!(
            labels_from_layout(&surround_layout(6, 6), 6),
            BY_POSITION[..6]
        );
        assert_eq!(labels_from_layout(&surround_layout(8, 8), 8), BY_POSITION);
        // A bigger device: every channel described, the rest unused.
        let sixteen = labels_from_layout(&surround_layout(8, 16), 16);
        assert_eq!(&sixteen[..8], &BY_POSITION);
        assert_eq!(&sixteen[8..], &[UNUSED; 8]);
    }

    #[test]
    fn five_one_goes_to_the_streams_back_pair() {
        let labels = [LEFT, RIGHT, CENTER, LFE, LEFT_SURROUND, RIGHT_SURROUND];
        let p = plan(&labels, 6);
        assert_eq!(
            &p[..6],
            &[Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)]
        );
        assert_eq!(surround_channels(&labels), 6);
    }

    #[test]
    fn seven_one_puts_rear_at_the_back_and_surround_at_the_sides() {
        let labels = [
            LEFT,
            RIGHT,
            CENTER,
            LFE,
            LEFT_SURROUND,
            RIGHT_SURROUND,
            REAR_SURROUND_LEFT,
            REAR_SURROUND_RIGHT,
        ];
        assert_eq!(
            plan(&labels, 8),
            [
                Some(0),
                Some(1),
                Some(2),
                Some(3),
                Some(6),
                Some(7),
                Some(4),
                Some(5)
            ]
        );
        assert_eq!(surround_channels(&labels), 8);
        // Asked for 5.1 of it: the rear pair is the back pair.
        assert_eq!(plan(&labels, 6)[4..6], [Some(4), Some(5)]);
    }

    #[test]
    fn a_device_in_another_order_is_put_in_the_streams() {
        // L C R Ls Rs LFE (MPEG 5.1 C).
        let labels = [LEFT, CENTER, RIGHT, LEFT_SURROUND, RIGHT_SURROUND, LFE];
        assert_eq!(
            &plan(&labels, 6)[..6],
            &[Some(0), Some(2), Some(1), Some(5), Some(3), Some(4)]
        );
    }

    #[test]
    fn unlabelled_channels_are_read_in_core_audios_usual_order() {
        let sixteen = [UNKNOWN; 16];
        assert_eq!(surround_channels(&sixteen), 8);
        assert_eq!(
            plan(&sixteen, 6)[..6],
            [Some(0), Some(1), Some(2), Some(3), Some(4), Some(5)]
        );
        assert_eq!(surround_channels(&[UNKNOWN; 2]), 2);
    }

    #[test]
    fn a_loopback_devices_own_numbering_is_not_the_sessions_layout() {
        // BlackHole 16ch out of the box: speakers 1..16, no rear pair.
        let blackhole: Vec<u32> = (1..=16).collect();
        assert!(!is_layout(&blackhole, 8));
        assert!(
            !is_layout(&blackhole, 6),
            "its first six are 5.1's, but its other speakers would be heard too"
        );
        assert!(is_layout(
            &labels_from_layout(&surround_layout(8, 16), 16),
            8
        ));
        assert!(is_layout(
            &labels_from_layout(&surround_layout(6, 16), 16),
            6
        ));
    }

    #[test]
    fn a_device_whose_speakers_were_never_set_says_so() {
        assert!(!speakers_set(&[UNKNOWN; 16]));
        assert!(speakers_set(&labels_from_layout(
            &surround_layout(6, 16),
            16
        )));
    }

    #[test]
    fn a_stereo_device_has_nothing_for_surround() {
        let p = plan(&[LEFT, RIGHT], 6);
        assert_eq!(p[..6], [Some(0), Some(1), None, None, None, None]);
        assert_eq!(surround_channels(&[LEFT, RIGHT]), 2);
    }

    #[test]
    fn remapping_fills_missing_speakers_with_silence() {
        let plan = plan(&[CENTER, LEFT, RIGHT], 6);
        let mut out = Vec::new();
        remap(&[0.3, 0.1, 0.2, 0.6, 0.4, 0.5], 3, &plan, 6, &mut out);
        assert_eq!(
            out,
            [0.1, 0.2, 0.3, 0.0, 0.0, 0.0, 0.4, 0.5, 0.6, 0.0, 0.0, 0.0]
        );
    }

    #[test]
    fn forty_eight_khz_passes_through() {
        let mut r = Resampler::new(48_000.0, 2);
        let mut out = Vec::new();
        r.push(&[0.1, 0.2, 0.3, 0.4], &mut out);
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn resampling_keeps_time_and_continuity_across_calls() {
        // 96 kHz -> 48 kHz: every other frame, with no seam between calls.
        let mut r = Resampler::new(96_000.0, 1);
        let input: Vec<f32> = (0..96).map(|i| i as f32).collect();
        let mut out = Vec::new();
        for chunk in input.chunks(7) {
            r.push(chunk, &mut out);
        }
        assert_eq!(out.len(), 48);
        for w in out.windows(2) {
            assert!((w[1] - w[0] - 2.0).abs() < 1e-4, "{out:?}");
        }
        // 44.1 kHz -> 48 kHz: a second in is a second out, give or take a frame.
        let mut r = Resampler::new(44_100.0, 2);
        let mut out = Vec::new();
        for _ in 0..100 {
            r.push(&vec![0.5; 441 * 2], &mut out);
        }
        assert!(
            (out.len() as i64 / 2 - 48_000).abs() <= 1,
            "{}",
            out.len() / 2
        );
        assert!(out.iter().skip(4).all(|&s| (s - 0.5).abs() < 1e-6));
    }
}
