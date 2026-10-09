//! Opus, configured as Sunshine configures it for Moonlight: 48 kHz, CELT-only
//! restricted-low-delay, constant bitrate, 5 ms packets. Surround (5.1, 7.1)
//! is Opus multistream, as Sunshine's is.

// A C-to-Rust translation of libopus; its functions are still `unsafe fn`,
// so the shorter name hides nothing.
#[allow(clippy::unsafe_removed_from_name)]
use unsafe_libopus as ffi;

use pingpong_proto::audio::SAMPLE_RATE;

/// Sunshine's rates, normal and high quality (Moonlight asks for high quality
/// when the stream's bitrate leaves room for it). 7.1's high-quality rate is
/// held under Sunshine's 2048 so a 5 ms packet fits one datagram.
pub const STEREO_KBPS: u32 = 96;
pub const STEREO_HQ_KBPS: u32 = 512;
pub const SURROUND51_KBPS: u32 = 256;
pub const SURROUND51_HQ_KBPS: u32 = 1536;
pub const SURROUND71_KBPS: u32 = 450;
pub const SURROUND71_HQ_KBPS: u32 = 1728;

/// How `channels` split into Opus streams: (streams, coupled streams, mapping).
///
/// Channels travel in Windows' order (WAVEFORMATEXTENSIBLE masks 0x3F and
/// 0x63F: FL FR FC LFE BL BR [SL SR]) from capture to playback; front, back
/// and side pairs are coupled (stereo) streams, centre and LFE mono ones. The
/// mapping gives, for each channel, its place in the decoded streams:
/// coupled stream k is places 2k and 2k+1, then one place per mono stream.
pub fn layout(channels: u8) -> Option<(i32, i32, &'static [u8])> {
    match channels {
        6 => Some((4, 2, &[0, 1, 4, 5, 2, 3])),
        8 => Some((5, 3, &[0, 1, 6, 7, 2, 3, 4, 5])),
        _ => None,
    }
}

enum EncoderState {
    Plain(*mut ffi::OpusEncoder),
    Multi(*mut ffi::OpusMSEncoder),
}

pub struct Encoder {
    st: EncoderState,
    channels: usize,
}

// The encoder state is plain memory owned by this value.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(channels: u8, bitrate_bps: u32) -> Result<Encoder, String> {
        let mut err = 0;
        let st = match layout(channels) {
            None => {
                let st = unsafe {
                    ffi::opus_encoder_create(
                        SAMPLE_RATE as i32,
                        channels as i32,
                        ffi::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                        &mut err,
                    )
                };
                if st.is_null() || err != ffi::OPUS_OK {
                    return Err(format!("opus_encoder_create failed ({err})"));
                }
                unsafe {
                    ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_BITRATE_REQUEST, bitrate_bps as i32);
                    ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_VBR_REQUEST, 0);
                }
                EncoderState::Plain(st)
            }
            Some((streams, coupled, mapping)) => {
                let st = unsafe {
                    ffi::opus_multistream_encoder_create(
                        SAMPLE_RATE as i32,
                        channels as i32,
                        streams,
                        coupled,
                        mapping.as_ptr(),
                        ffi::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                        &mut err,
                    )
                };
                if st.is_null() || err != ffi::OPUS_OK {
                    return Err(format!("opus_multistream_encoder_create failed ({err})"));
                }
                unsafe {
                    ffi::opus_multistream_encoder_ctl!(
                        st,
                        ffi::OPUS_SET_BITRATE_REQUEST,
                        bitrate_bps as i32
                    );
                    ffi::opus_multistream_encoder_ctl!(st, ffi::OPUS_SET_VBR_REQUEST, 0);
                }
                EncoderState::Multi(st)
            }
        };
        Ok(Encoder {
            st,
            channels: channels as usize,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Encode one frame, channels interleaved: 5 ms
    /// ([`FRAME_SAMPLES`](pingpong_proto::audio::FRAME_SAMPLES)) as
    /// the host sends, or any length Opus takes in this mode (2.5, 5, 10 or
    /// 20 ms).
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, String> {
        let frame = pcm.len() / self.channels;
        if !pcm.len().is_multiple_of(self.channels) || ![120, 240, 480, 960].contains(&frame) {
            return Err(format!("{frame} samples is not an Opus frame"));
        }
        let (frame, cap) = (frame as i32, out.len() as i32);
        let n = match self.st {
            EncoderState::Plain(st) => unsafe {
                ffi::opus_encode_float(st, pcm.as_ptr(), frame, out.as_mut_ptr(), cap)
            },
            EncoderState::Multi(st) => unsafe {
                ffi::opus_multistream_encode_float(st, pcm.as_ptr(), frame, out.as_mut_ptr(), cap)
            },
        };
        if n < 0 {
            return Err(format!("opus encode failed ({n})"));
        }
        Ok(n as usize)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        match self.st {
            EncoderState::Plain(st) => unsafe { ffi::opus_encoder_destroy(st) },
            EncoderState::Multi(st) => unsafe { ffi::opus_multistream_encoder_destroy(st) },
        }
    }
}

enum DecoderState {
    Plain(*mut ffi::OpusDecoder),
    Multi(*mut ffi::OpusMSDecoder),
}

pub struct Decoder {
    st: DecoderState,
    channels: usize,
}

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(channels: u8) -> Result<Decoder, String> {
        let mut err = 0;
        let st = match layout(channels) {
            None => {
                let st = unsafe {
                    ffi::opus_decoder_create(SAMPLE_RATE as i32, channels as i32, &mut err)
                };
                if st.is_null() || err != ffi::OPUS_OK {
                    return Err(format!("opus_decoder_create failed ({err})"));
                }
                DecoderState::Plain(st)
            }
            Some((streams, coupled, mapping)) => {
                let st = unsafe {
                    ffi::opus_multistream_decoder_create(
                        SAMPLE_RATE as i32,
                        channels as i32,
                        streams,
                        coupled,
                        mapping.as_ptr(),
                        &mut err,
                    )
                };
                if st.is_null() || err != ffi::OPUS_OK {
                    return Err(format!("opus_multistream_decoder_create failed ({err})"));
                }
                DecoderState::Multi(st)
            }
        };
        Ok(Decoder {
            st,
            channels: channels as usize,
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decode `packet` into `out` (interleaved), or conceal a lost packet
    /// when `packet` is `None`. Returns samples per channel.
    pub fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<usize, String> {
        let frame = (out.len() / self.channels) as i32;
        let (ptr, len) = match packet {
            Some(p) => (p.as_ptr(), p.len() as i32),
            None => (std::ptr::null(), 0),
        };
        let n = match self.st {
            DecoderState::Plain(st) => unsafe {
                ffi::opus_decode_float(st, ptr, len, out.as_mut_ptr(), frame, 0)
            },
            DecoderState::Multi(st) => unsafe {
                ffi::opus_multistream_decode_float(st, ptr, len, out.as_mut_ptr(), frame, 0)
            },
        };
        if n < 0 {
            return Err(format!("opus decode failed ({n})"));
        }
        Ok(n as usize)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        match self.st {
            DecoderState::Plain(st) => unsafe { ffi::opus_decoder_destroy(st) },
            DecoderState::Multi(st) => unsafe { ffi::opus_multistream_decoder_destroy(st) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_proto::audio::FRAME_SAMPLES;

    fn tone(n: usize, offset: usize) -> Vec<f32> {
        (0..n)
            .flat_map(|i| {
                let v = ((i + offset) as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32)
                    .sin()
                    * 0.5;
                [v, v]
            })
            .collect()
    }

    #[test]
    fn a_tone_survives_the_round_trip() {
        let mut enc = Encoder::new(2, STEREO_KBPS * 1000).unwrap();
        let mut dec = Decoder::new(2).unwrap();
        let mut packet = [0u8; 1500];
        let mut pcm = vec![0f32; FRAME_SAMPLES * 2];
        let mut energy_in = 0.0;
        let mut energy_out = 0.0;
        for f in 0..40 {
            let input = tone(FRAME_SAMPLES, f * FRAME_SAMPLES);
            let n = enc.encode(&input, &mut packet).unwrap();
            // CBR: every packet is the same size (96 kbit/s × 5 ms = 60 bytes).
            assert_eq!(n, 60);
            assert_eq!(
                dec.decode(Some(&packet[..n]), &mut pcm).unwrap(),
                FRAME_SAMPLES
            );
            if f > 4 {
                energy_in += input.iter().map(|v| v * v).sum::<f32>();
                energy_out += pcm.iter().map(|v| v * v).sum::<f32>();
            }
        }
        let ratio = energy_out / energy_in;
        assert!((0.7..1.3).contains(&ratio), "energy ratio {ratio}");
        // Concealment produces a full frame too.
        assert_eq!(dec.decode(None, &mut pcm).unwrap(), FRAME_SAMPLES);
    }

    /// Each surround channel comes back where it went in: a different tone
    /// per channel, and each channel's output correlates with its own input
    /// far more than with any other's.
    #[test]
    fn surround_channels_keep_their_places() {
        for (channels, kbps) in [(6u8, SURROUND51_HQ_KBPS), (8u8, SURROUND71_HQ_KBPS)] {
            let ch = channels as usize;
            let mut enc = Encoder::new(channels, kbps * 1000).unwrap();
            let mut dec = Decoder::new(channels).unwrap();
            let freq = |c: usize| 200.0 + 170.0 * c as f32;
            let mut packet = [0u8; pingpong_proto::audio::MAX_PACKET];
            let mut pcm = vec![0f32; FRAME_SAMPLES * ch];
            let (mut inputs, mut outputs) = (vec![], vec![]);
            for f in 0..60 {
                let input: Vec<f32> = (0..FRAME_SAMPLES)
                    .flat_map(|i| {
                        let t = (f * FRAME_SAMPLES + i) as f32 / SAMPLE_RATE as f32;
                        (0..ch).map(move |c| (t * freq(c) * std::f32::consts::TAU).sin() * 0.3)
                    })
                    .collect();
                let n = enc.encode(&input, &mut packet).unwrap();
                assert!(n <= pingpong_proto::audio::MAX_PACKET);
                dec.decode(Some(&packet[..n]), &mut pcm).unwrap();
                inputs.extend(input);
                outputs.extend(pcm.iter().copied());
            }
            // Opus delays the signal by a few ms: compare against the input
            // shifted by the best lag.
            let at = |v: &[f32], c: usize, i: usize| v[i * ch + c];
            let frames = inputs.len() / ch;
            for c in 0..ch {
                let corr = |src: usize| -> f32 {
                    (0..400)
                        .map(|lag| {
                            (2000..frames - 400)
                                .map(|i| at(&outputs, c, i + lag) * at(&inputs, src, i))
                                .sum::<f32>()
                        })
                        .fold(f32::MIN, f32::max)
                };
                let own = corr(c);
                for other in (0..ch).filter(|&o| o != c) {
                    assert!(
                        own > 4.0 * corr(other).abs(),
                        "{channels} ch: channel {c} looks like {other}"
                    );
                }
            }
        }
    }
}
