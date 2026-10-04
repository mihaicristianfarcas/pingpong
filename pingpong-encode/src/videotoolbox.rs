//! macOS: VideoToolbox's hardware encoder, set up for streaming the way the
//! Windows host sets up NVENC: real time, no frame reordering (every frame
//! goes out as soon as it is encoded), a keyframe only when asked for, the
//! low-latency rate control where the encoder has it, and BT.709 limited range
//! written into the stream. Frames come out in Annex-B form (start codes,
//! parameter sets in front of every keyframe), as NVENC's do, so the client
//! cannot tell the hosts apart.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMVideoCodecType_H264, kCMVideoCodecType_HEVC, CMFormatDescription, CMSampleBuffer, CMTime,
    CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_2020, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
    kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVImageBufferYCbCrMatrix_ITU_R_709_2, CVPixelBuffer,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_ContentLightLevelInfo,
    kVTCompressionPropertyKey_EnableLTR, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MasteringDisplayColorVolume,
    kVTCompressionPropertyKey_MaxFrameDelayCount, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaximizePowerEfficiency,
    kVTCompressionPropertyKey_MaximumRealTimeFrameRate,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_AcknowledgedLTRTokens, kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTEncodeFrameOptionKey_ForceLTRRefresh, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_HEVC_Main10_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTSampleAttachmentKey_RequireLTRAcknowledgementToken,
    kVTVideoEncoderSpecification_EnableLowLatencyRateControl,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSessionSetProperty,
};

use crate::{Codec, EncodeError, EncodedFrame, EncoderConfig, FrameKind};

const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// HDR10's static metadata as its SEI messages carry it (big-endian):
/// the mastering display's colour volume (BT.2020 primaries in the order
/// green, blue, red, D65, `max_nits` and 0.005 cd/m² in 0.0001 cd/m²), and
/// content light levels left unknown (0).
fn hdr10_sei(max_nits: u32) -> ([u8; 24], [u8; 4]) {
    let mut display = [0u8; 24];
    let xy: [u16; 8] = [8500, 39850, 6550, 2300, 35400, 14600, 15635, 16450];
    for (i, v) in xy.iter().enumerate() {
        display[i * 2..i * 2 + 2].copy_from_slice(&v.to_be_bytes());
    }
    display[16..20].copy_from_slice(&(max_nits * 10_000).to_be_bytes());
    display[20..24].copy_from_slice(&50u32.to_be_bytes());
    (display, [0u8; 4])
}

/// Longest wait for a frame to come out of the encoder before asking it to
/// flush (a dropped frame never comes).
const OUTPUT_TIMEOUT: Duration = Duration::from_millis(50);

/// One finished frame: Annex-B, whether it is a keyframe, and its long-term
/// reference token if the encoder made it one.
struct Out {
    data: Vec<u8>,
    keyframe: bool,
    ltr_token: Option<i64>,
}

/// Frames the encoder has finished, oldest first, with their index. Every
/// submitted frame produces exactly one, a dropped frame an empty one.
#[derive(Default)]
struct Shared {
    out: Mutex<VecDeque<(u64, Result<Out, String>)>>,
    done: Condvar,
}

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    frame_refcon: *mut c_void,
    status: i32,
    _flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: the Box<Shared> handed over at session creation; the session is
    // invalidated before it is freed.
    let shared = unsafe { &*(refcon as *const Shared) };
    let result = match unsafe { sample.as_ref() } {
        Some(sample) if status == 0 => annex_b(sample).map(|(data, keyframe)| Out {
            data,
            keyframe,
            ltr_token: unsafe { ltr_token(sample) },
        }),
        // Success without a sample: the encoder dropped the frame.
        None if status == 0 => Ok(Out {
            data: Vec::new(),
            keyframe: false,
            ltr_token: None,
        }),
        _ => Err(format!("encode failed (OSStatus {status})")),
    };
    // The frame's index rides in its refcon.
    let index = frame_refcon as usize as u64;
    shared
        .out
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back((index, result));
    shared.done.notify_all();
}

/// The long-term reference token the encoder attached to the sample, if it
/// made the frame one.
unsafe fn ltr_token(sample: &CMSampleBuffer) -> Option<i64> {
    let attachments = unsafe { sample.sample_attachments_array(false) }?;
    if attachments.count() == 0 {
        return None;
    }
    let dict = unsafe { (attachments.value_at_index(0) as *const CFDictionary).as_ref() }?;
    let key = unsafe { kVTSampleAttachmentKey_RequireLTRAcknowledgementToken } as *const CFString
        as *const c_void;
    let number = unsafe { (dict.value(key) as *const CFNumber).as_ref() }?;
    number.as_i64()
}

/// The sample as start-code-delimited NAL units, parameter sets first on a
/// keyframe. True if it is one.
fn annex_b(sample: &CMSampleBuffer) -> Result<(Vec<u8>, bool), String> {
    let block = unsafe { sample.data_buffer() }.ok_or("no data in the encoded sample")?;
    let len = unsafe { block.data_length() };
    let mut avcc = vec![0u8; len];
    let status =
        unsafe { block.copy_data_bytes(0, len, NonNull::new(avcc.as_mut_ptr().cast()).unwrap()) };
    if status != 0 {
        return Err(format!("reading the encoded sample (OSStatus {status})"));
    }
    let format = unsafe { sample.format_description() }.ok_or("no format description")?;
    let hevc = unsafe { format.media_sub_type() } == kCMVideoCodecType_HEVC;
    let (params, length_bytes) = parameter_sets(&format)?;
    Ok(to_annex_b(&avcc, length_bytes, &params, hevc))
}

/// Length-prefixed NAL units (`length_bytes` wide, big-endian) as
/// start-code-delimited ones, `params` in front if a NAL is a keyframe's.
/// True if it is one.
fn to_annex_b(avcc: &[u8], length_bytes: usize, params: &[Vec<u8>], hevc: bool) -> (Vec<u8>, bool) {
    let mut nals = Vec::new();
    let mut at = 0;
    while at + length_bytes <= avcc.len() {
        let n = avcc[at..at + length_bytes]
            .iter()
            .fold(0usize, |acc, &b| (acc << 8) | b as usize);
        at += length_bytes;
        let end = (at + n).min(avcc.len());
        nals.push(&avcc[at..end]);
        at = end;
    }
    let keyframe = nals.iter().any(|nal| is_keyframe(nal, hevc));

    let mut out =
        Vec::with_capacity(avcc.len() + 64 + params.iter().map(|p| p.len() + 4).sum::<usize>());
    if keyframe {
        for p in params {
            out.extend_from_slice(&START_CODE);
            out.extend_from_slice(p);
        }
    }
    for nal in nals {
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    (out, keyframe)
}

fn is_keyframe(nal: &[u8], hevc: bool) -> bool {
    let Some(&h) = nal.first() else { return false };
    if hevc {
        // IDR_W_RADL, IDR_N_LP, CRA.
        matches!((h >> 1) & 0x3F, 19..=21)
    } else {
        h & 0x1F == 5
    }
}

/// The format's parameter sets (VPS/SPS/PPS or SPS/PPS) and the width of the
/// length prefix in front of each NAL unit.
fn parameter_sets(format: &CMFormatDescription) -> Result<(Vec<Vec<u8>>, usize), String> {
    let hevc = unsafe { format.media_sub_type() } == kCMVideoCodecType_HEVC;
    let get = |index: usize,
               ptr: &mut *const u8,
               size: &mut usize,
               count: &mut usize,
               header: &mut i32| unsafe {
        if hevc {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                format, index, ptr, size, count, header,
            )
        } else {
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, index, ptr, size, count, header,
            )
        }
    };
    let (mut ptr, mut size, mut count, mut header) = (std::ptr::null(), 0usize, 0usize, 0i32);
    let status = get(0, &mut ptr, &mut size, &mut count, &mut header);
    if status != 0 {
        return Err(format!("reading parameter sets (OSStatus {status})"));
    }
    let mut sets = Vec::with_capacity(count);
    for i in 0..count {
        let mut n = 0usize;
        let status = get(i, &mut ptr, &mut size, &mut n, &mut header);
        if status != 0 || ptr.is_null() {
            return Err(format!("reading parameter set {i} (OSStatus {status})"));
        }
        sets.push(unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec());
    }
    Ok((sets, header.clamp(1, 4) as usize))
}

pub struct VtEncoder {
    session: CFRetained<VTCompressionSession>,
    shared: Box<Shared>,
    config: EncoderConfig,
    /// Whether the encoder took the low-latency rate control.
    pub low_latency: bool,
    in_flight: usize,
    /// Long-term references (recovering from loss without a keyframe).
    ltr: Ltr,
}

/// The encoder's long-term references, as the receiver has them.
#[derive(Default)]
struct Ltr {
    /// The encoder takes them (low-latency mode, this codec).
    enabled: bool,
    /// Made, not yet known to have arrived: (frame index, token).
    unconfirmed: VecDeque<(u64, i64)>,
    /// Arrived: frame indices, newest last.
    confirmed: VecDeque<u64>,
    /// Tokens to report as arrived with the next frame.
    to_acknowledge: Vec<i64>,
    /// The next frame predicts from a confirmed reference only.
    refresh: bool,
    /// The frame submitted as such a refresh.
    refresh_index: Option<u64>,
}

// SAFETY: the session is only used through &mut self; VideoToolbox sessions
// are not thread-affine.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub fn new(config: EncoderConfig) -> Result<VtEncoder, EncodeError> {
        let codec_type = match config.codec {
            Codec::H264 => kCMVideoCodecType_H264,
            Codec::Hevc => kCMVideoCodecType_HEVC,
            Codec::Av1 => return Err(EncodeError::Unsupported("AV1 encoding on macOS".into())),
        };
        let shared = Box::<Shared>::default();
        // Low-latency rate control first; an encoder without it for this
        // codec still streams, only with the ordinary rate control.
        let (session, low_latency) = match Self::create(&config, codec_type, &shared, true) {
            Ok(s) => (s, true),
            Err(e) => {
                tracing::info!(error = %e, "no low-latency rate control for this codec; \
                    using the ordinary one");
                (Self::create(&config, codec_type, &shared, false)?, false)
            }
        };
        let mut enc = VtEncoder {
            session,
            shared,
            config,
            low_latency,
            in_flight: 0,
            ltr: Ltr::default(),
        };
        enc.configure()?;
        Ok(enc)
    }

    fn create(
        config: &EncoderConfig,
        codec_type: u32,
        shared: &Shared,
        low_latency: bool,
    ) -> Result<CFRetained<VTCompressionSession>, EncodeError> {
        let yes: &CFType = CFBoolean::new(true);
        let mut keys: Vec<&CFString> =
            vec![unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder }];
        let mut values: Vec<&CFType> = vec![yes];
        if low_latency {
            keys.push(unsafe { kVTVideoEncoderSpecification_EnableLowLatencyRateControl });
            values.push(yes);
        }
        let spec = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
        let mut session: *mut VTCompressionSession = std::ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                config.width as i32,
                config.height as i32,
                codec_type,
                Some(spec.as_opaque()),
                None,
                None,
                Some(output_callback),
                shared as *const Shared as *mut c_void,
                NonNull::from(&mut session),
            )
        };
        let session = NonNull::new(session)
            .filter(|_| status == 0)
            .ok_or_else(|| {
                EncodeError::Unsupported(format!("VTCompressionSessionCreate returned {status}"))
            })?;
        // SAFETY: created with a +1 retain count.
        Ok(unsafe { CFRetained::from_raw(session) })
    }

    fn configure(&mut self) -> Result<(), EncodeError> {
        let c = self.config;
        let yes: &CFType = CFBoolean::new(true);
        let no: &CFType = CFBoolean::new(false);
        let profile: &CFType = match (c.codec, c.hdr) {
            (Codec::Hevc, true) => unsafe { kVTProfileLevel_HEVC_Main10_AutoLevel },
            (Codec::Hevc, false) => unsafe { kVTProfileLevel_HEVC_Main_AutoLevel },
            _ => unsafe { kVTProfileLevel_H264_High_AutoLevel },
        };
        // HDR10 (BT.2020, PQ) as ScreenCaptureKit's HDR10 capture delivers
        // it; else BT.709, as the SDR capture is converted.
        let (primaries, transfer, matrix): (&CFType, &CFType, &CFType) = unsafe {
            if c.hdr {
                (
                    kCVImageBufferColorPrimaries_ITU_R_2020,
                    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
                    kCVImageBufferYCbCrMatrix_ITU_R_2020,
                )
            } else {
                (
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                )
            }
        };
        let fps = CFNumber::new_f64(c.fps_mhz as f64 / 1000.0);
        // A keyframe only when the client asks: set the interval out of reach.
        let never = CFNumber::new_i32(i32::MAX);
        let zero_delay = CFNumber::new_i32(0);
        let props: [(&CFString, &CFType, &str); 13] = unsafe {
            [
                // Clocked for the stream's rate, not for battery life.
                (
                    kVTCompressionPropertyKey_MaximumRealTimeFrameRate,
                    &fps,
                    "real-time frame rate",
                ),
                (
                    kVTCompressionPropertyKey_MaximizePowerEfficiency,
                    no,
                    "power efficiency off",
                ),
                (kVTCompressionPropertyKey_RealTime, yes, "real time"),
                (
                    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                    yes,
                    "speed over quality",
                ),
                (
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    no,
                    "no frame reordering",
                ),
                (kVTCompressionPropertyKey_ProfileLevel, profile, "profile"),
                (
                    kVTCompressionPropertyKey_ExpectedFrameRate,
                    &fps,
                    "frame rate",
                ),
                (
                    kVTCompressionPropertyKey_MaxKeyFrameInterval,
                    &never,
                    "keyframe interval",
                ),
                (
                    kVTCompressionPropertyKey_MaxFrameDelayCount,
                    &zero_delay,
                    "frame delay",
                ),
                (
                    kVTCompressionPropertyKey_ColorPrimaries,
                    primaries,
                    "primaries",
                ),
                (
                    kVTCompressionPropertyKey_TransferFunction,
                    transfer,
                    "transfer",
                ),
                (kVTCompressionPropertyKey_YCbCrMatrix, matrix, "matrix"),
                (
                    kVTCompressionPropertyKey_AverageBitRate,
                    &CFNumber::new_i32(c.bitrate_bps as i32),
                    "bitrate",
                ),
            ]
        };
        for (key, value, what) in props {
            let status = unsafe { VTSessionSetProperty(&self.session, key, Some(value)) };
            if status != 0 {
                // Not every encoder takes every property; the stream still works.
                tracing::debug!(what, status, "encoder property not taken");
            }
        }
        if c.hdr {
            // The SEI a decoder tone-maps by: a BT.2020 display of 1000
            // cd/m², the capture's canonical HDR10 (the client also hears
            // it in `HdrMetadata`).
            let (display, content) = hdr10_sei(1000);
            for (key, value, what) in unsafe {
                [
                    (
                        kVTCompressionPropertyKey_MasteringDisplayColorVolume,
                        CFData::from_bytes(&display),
                        "mastering display",
                    ),
                    (
                        kVTCompressionPropertyKey_ContentLightLevelInfo,
                        CFData::from_bytes(&content),
                        "content light level",
                    ),
                ]
            } {
                let status = unsafe { VTSessionSetProperty(&self.session, key, Some(&value)) };
                if status != 0 {
                    tracing::debug!(what, status, "HDR metadata not taken");
                }
            }
        }
        // Long-term references: the way back from a lost frame without a
        // keyframe (NVENC's reference invalidation, done VideoToolbox's way).
        if self.low_latency {
            let status = unsafe {
                VTSessionSetProperty(
                    &self.session,
                    kVTCompressionPropertyKey_EnableLTR,
                    Some(yes),
                )
            };
            self.ltr.enabled = status == 0;
            tracing::debug!(enabled = self.ltr.enabled, status, "long-term references");
        }
        Ok(())
    }

    /// Encode `image` as frame `index` and wait for it; a keyframe if
    /// `force_idr`. An empty frame: the encoder dropped it.
    pub fn encode(
        &mut self,
        image: &CVPixelBuffer,
        index: u64,
        force_idr: bool,
    ) -> Result<EncodedFrame, EncodeError> {
        self.submit(image, index, force_idr)?;
        loop {
            match self.next(OUTPUT_TIMEOUT) {
                Some(Ok(f)) if f.index == index => return Ok(f),
                Some(Ok(_)) => continue, // an earlier frame's
                Some(Err(e)) => return Err(e),
                None => {
                    // Late: collect it the slow way.
                    let pts = self.pts(index);
                    let _ = unsafe { self.session.complete_frames(pts) };
                    return self.next(Duration::ZERO).unwrap_or(Ok(EncodedFrame {
                        data: Vec::new(),
                        kind: FrameKind::P,
                        index,
                    }));
                }
            }
        }
    }

    /// Start encoding `image` as frame `index` without waiting for it: a
    /// frame larger than the encoder does in one frame interval (3024x1890
    /// takes ~15 ms) keeps its pace only with more than one in flight.
    pub fn submit(
        &mut self,
        image: &CVPixelBuffer,
        index: u64,
        force_idr: bool,
    ) -> Result<(), EncodeError> {
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        };
        let yes: &CFType = CFBoolean::new(true);
        let acknowledged: Vec<CFRetained<CFNumber>> = self
            .ltr
            .to_acknowledge
            .drain(..)
            .map(CFNumber::new_i64)
            .collect();
        let acknowledged =
            (!acknowledged.is_empty()).then(|| CFArray::from_retained_objects(&acknowledged));
        let refresh = self.ltr.refresh && !force_idr;
        let mut keys: Vec<&CFString> = Vec::new();
        let mut values: Vec<&CFType> = Vec::new();
        if force_idr {
            keys.push(unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame });
            values.push(yes);
        }
        if let Some(a) = &acknowledged {
            keys.push(unsafe { kVTEncodeFrameOptionKey_AcknowledgedLTRTokens });
            values.push(a.as_ref());
        }
        if refresh {
            keys.push(unsafe { kVTEncodeFrameOptionKey_ForceLTRRefresh });
            values.push(yes);
        }
        let options = (!keys.is_empty())
            .then(|| CFDictionary::<CFString, CFType>::from_slices(&keys, &values));
        let status = unsafe {
            self.session.encode_frame(
                image,
                self.pts(index),
                invalid,
                options.as_deref().map(|d| d.as_opaque()),
                index as usize as *mut c_void,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(EncodeError::Unsupported(format!(
                "VTCompressionSessionEncodeFrame returned {status}"
            )));
        }
        self.in_flight += 1;
        if refresh || force_idr {
            self.ltr.refresh = false;
        }
        if refresh {
            self.ltr.refresh_index = Some(index);
        }
        Ok(())
    }

    /// The oldest finished frame, waiting up to `timeout` for one. None: no
    /// frame finished in time.
    pub fn next(&mut self, timeout: Duration) -> Option<Result<EncodedFrame, EncodeError>> {
        let mut out = self.shared.out.lock().unwrap_or_else(|e| e.into_inner());
        if out.is_empty() && !timeout.is_zero() {
            out = self
                .shared
                .done
                .wait_timeout(out, timeout)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        let (index, result) = out.pop_front()?;
        drop(out);
        self.in_flight = self.in_flight.saturating_sub(1);
        Some(match result {
            Ok(o) => {
                if let Some(token) = o.ltr_token {
                    self.ltr.unconfirmed.push_back((index, token));
                    while self.ltr.unconfirmed.len() > 64 {
                        self.ltr.unconfirmed.pop_front();
                    }
                }
                let kind = if o.keyframe {
                    // A keyframe starts over: older references are gone.
                    self.ltr.confirmed.clear();
                    FrameKind::Idr
                } else if self.ltr.refresh_index == Some(index) && !o.data.is_empty() {
                    FrameKind::Recovery
                } else {
                    FrameKind::P
                };
                Ok(EncodedFrame {
                    data: o.data,
                    kind,
                    index,
                })
            }
            Err(e) => Err(EncodeError::Unsupported(e)),
        })
    }

    /// Long-term references: whether the encoder takes them, and how many
    /// are waiting for / have had the receiver's confirmation.
    pub fn ltr_state(&self) -> (bool, usize, usize) {
        (
            self.ltr.enabled,
            self.ltr.unconfirmed.len(),
            self.ltr.confirmed.len(),
        )
    }

    /// Frames submitted and not yet taken with `next`.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    fn pts(&self, index: u64) -> CMTime {
        CMTime {
            value: index as i64 * 1000,
            timescale: self.config.fps_mhz.max(1) as i32,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        }
    }

    /// The receiver has every frame up to `index` (no loss reported for
    /// them in time): the long-term references among them may be used.
    pub fn acknowledge_through(&mut self, index: u64) {
        while let Some(&(i, token)) = self.ltr.unconfirmed.front() {
            if i > index {
                break;
            }
            self.ltr.unconfirmed.pop_front();
            self.ltr.to_acknowledge.push(token);
            self.ltr.confirmed.push_back(i);
            while self.ltr.confirmed.len() > 16 {
                self.ltr.confirmed.pop_front();
            }
        }
    }

    /// Frames `first..=last` never arrived. True if the next frame can
    /// predict from a reference the receiver has (a recovery frame); false
    /// if only a keyframe will do.
    pub fn invalidate(&mut self, first: u64, _last: u64) -> bool {
        if !self.ltr.enabled {
            return false;
        }
        // Nothing from the loss on can be relied on.
        self.ltr.unconfirmed.retain(|&(i, _)| i < first);
        self.ltr.confirmed.retain(|&i| i < first);
        if self.ltr.confirmed.is_empty() {
            return false;
        }
        self.ltr.refresh = true;
        true
    }

    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        let rate = CFNumber::new_i32(bitrate_bps as i32);
        let status = unsafe {
            VTSessionSetProperty(
                &self.session,
                kVTCompressionPropertyKey_AverageBitRate,
                Some(&rate),
            )
        };
        if status != 0 {
            return Err(EncodeError::Unsupported(format!(
                "setting the bitrate returned {status}"
            )));
        }
        self.config.bitrate_bps = bitrate_bps;
        Ok(())
    }
}

impl Drop for VtEncoder {
    fn drop(&mut self) {
        unsafe { self.session.invalidate() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VPS: [u8; 3] = [0x40, 0x01, 0xAA];
    const SPS: [u8; 3] = [0x42, 0x01, 0xBB];
    const PPS: [u8; 3] = [0x44, 0x01, 0xCC];

    fn prefixed(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter()
            .flat_map(|n| {
                (n.len() as u32)
                    .to_be_bytes()
                    .into_iter()
                    .chain(n.iter().copied())
            })
            .collect()
    }

    #[test]
    fn an_hevc_keyframe_gets_its_parameter_sets_in_front() {
        let idr: &[u8] = &[19 << 1, 0x01, 0x11, 0x22];
        let (out, key) = to_annex_b(
            &prefixed(&[idr]),
            4,
            &[VPS.to_vec(), SPS.to_vec(), PPS.to_vec()],
            true,
        );
        assert!(key);
        let want: Vec<u8> = [&VPS[..], &SPS, &PPS, idr]
            .iter()
            .flat_map(|n| START_CODE.iter().chain(n.iter()).copied())
            .collect();
        assert_eq!(out, want);
    }

    #[test]
    fn a_p_frame_goes_out_alone_and_every_nal_is_kept() {
        let trail: &[u8] = &[1 << 1, 0x01, 0x33];
        let sei: &[u8] = &[39 << 1, 0x01, 0x44, 0x55];
        let (out, key) = to_annex_b(
            &prefixed(&[sei, trail]),
            4,
            &[VPS.to_vec(), SPS.to_vec(), PPS.to_vec()],
            true,
        );
        assert!(!key);
        let want: Vec<u8> = [sei, trail]
            .iter()
            .flat_map(|n| START_CODE.iter().chain(n.iter()).copied())
            .collect();
        assert_eq!(out, want);
    }

    #[test]
    fn h264_keyframes_are_nal_type_5_whatever_the_parameter_set_count() {
        let idr: &[u8] = &[0x65, 0x88];
        let extra_pps = vec![0x68, 0x02];
        let (out, key) = to_annex_b(
            &prefixed(&[idr]),
            4,
            &[vec![0x67, 0x01], vec![0x68, 0x01], extra_pps],
            false,
        );
        assert!(key);
        assert!(out.starts_with(&[0, 0, 0, 1, 0x67, 0x01]));
        assert!(out.ends_with(&[0, 0, 0, 1, 0x65, 0x88]));
    }

    #[test]
    fn a_truncated_last_nal_is_cut_short_not_overrun() {
        let mut avcc = prefixed(&[&[0x02, 0x01, 0x10]]);
        avcc.extend_from_slice(&[0, 0, 0, 9, 0x02, 0x01]); // claims 9, has 2
        let (out, _) = to_annex_b(&avcc, 4, &[], true);
        assert!(out.ends_with(&[0, 0, 0, 1, 0x02, 0x01]));
    }
}
