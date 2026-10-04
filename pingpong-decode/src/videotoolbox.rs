//! H.264 and HEVC decode through VideoToolbox, in real-time mode: 8-bit
//! and 10-bit (HDR), 4:2:0 and 4:4:4 (Apple silicon decodes HEVC 4:4:4 in
//! hardware: measured on an M4 Pro, 8- and 10-bit).
//!
//! Measured: 0.96 ms p50 / 1.12 ms p95 at 1080p60, one callback per
//! submit, no buffering or reordering -- which is why the client has no jitter
//! buffer and why frames are handed on as soon as they are decoded.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use objc2_core_foundation::{
    kCFBooleanTrue, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMBlockBufferAssureMemoryNowFlag, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMSampleTimingInfo, CMTime, CMTimeFlags, CMVideoFormatDescription,
    CMVideoFormatDescriptionCreateFromH264ParameterSets,
    CMVideoFormatDescriptionCreateFromHEVCParameterSets,
};
use objc2_core_video::{
    kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey, CVImageBuffer,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth,
};
use objc2_video_toolbox::{
    kVTDecompressionPropertyKey_RealTime, VTDecodeFrameFlags, VTDecodeInfoFlags,
    VTDecompressionOutputCallbackRecord, VTDecompressionSession, VTSessionSetProperty,
};

use crate::{annexb, Codec, DecodeError, DecodedFrame, VideoDecoder};

/// Decoded frames waiting for the presenter. A display shows one frame; a
/// second is only a frame shown late, so the oldest is discarded.
const MAX_READY: usize = 2;

/// Length-prefix width every NAL is rewritten with (AVCC/HVCC framing).
const LENGTH_BYTES: i32 = 4;

/// Bi-planar Y'CbCr, video range: 10-bit in 16-bit samples, 4:2:0 and
/// 4:4:4 (`x420`, `x444`), and 8-bit 4:4:4 (`444v`).
const X420: u32 = u32::from_be_bytes(*b"x420");
const X444: u32 = u32::from_be_bytes(*b"x444");
const V444: u32 = u32::from_be_bytes(*b"444v");

/// What a stream's pictures are: their bit depth and chroma.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PictureFormat {
    pub ten_bit: bool,
    pub yuv444: bool,
}

impl PictureFormat {
    /// The pixel format to ask VideoToolbox for: one Metal can sample plane
    /// by plane. VideoToolbox's own for 10-bit is packed (`p420`, `p444`:
    /// three samples to 32 bits), which `CVMetalTextureCache` cannot map.
    /// `None` for 8-bit 4:2:0, whose own (`420v`) is.
    fn pixel_format(self) -> Option<u32> {
        match (self.ten_bit, self.yuv444) {
            (false, false) => None,
            (false, true) => Some(V444),
            (true, false) => Some(X420),
            (true, true) => Some(X444),
        }
    }
}

/// Shared with VideoToolbox's callback through a raw pointer, so it lives in a
/// Box that outlives the session.
/// Where decoded frames go when the caller wants them pushed rather than
/// polled: called on VideoToolbox's callback thread.
pub type FrameSink = std::sync::Arc<dyn Fn(DecodedFrame) + Send + Sync>;

struct Shared {
    sink: Option<FrameSink>,
    ready: Mutex<VecDeque<DecodedFrame>>,
    failed: AtomicU64,
    dropped: AtomicU64,
}

unsafe extern "C-unwind" fn output_callback(
    output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: VTDecodeInfoFlags,
    image_buffer: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    // SAFETY: the Box<Shared> handed over at session creation; the session is
    // drained and invalidated before it is freed.
    let shared = unsafe { &*(output_ref_con as *const Shared) };
    let decoded_at_us = pingpong_proto::clock::now_us();
    let Some(image) = NonNull::new(image_buffer) else {
        shared.failed.fetch_add(1, Ordering::Relaxed);
        return;
    };
    if status != 0 {
        shared.failed.fetch_add(1, Ordering::Relaxed);
        return;
    }
    // SAFETY: valid for the callback; retained to outlive it.
    let pixel_buffer = unsafe { CFRetained::retain(image) };
    let width = CVPixelBufferGetWidth(&pixel_buffer) as u32;
    let height = CVPixelBufferGetHeight(&pixel_buffer) as u32;
    let frame = DecodedFrame {
        pixel_buffer: CFRetained::into_raw(pixel_buffer).as_ptr() as *mut c_void,
        // The frame's tag rides in the per-frame refcon (a u32 fits a pointer).
        capture_ts_us: source_frame_ref_con as usize as u32,
        decoded_at_us,
        width,
        height,
    };
    if let Some(sink) = &shared.sink {
        sink(frame);
        return;
    }
    let mut ready = shared.ready.lock().unwrap_or_else(|e| e.into_inner());
    while ready.len() >= MAX_READY {
        ready.pop_front();
        shared.dropped.fetch_add(1, Ordering::Relaxed);
    }
    ready.push_back(frame);
}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        if let Some(ptr) = NonNull::new(self.pixel_buffer as *mut CVImageBuffer) {
            // SAFETY: the +1 taken in the callback; never cloned.
            drop(unsafe { CFRetained::from_raw(ptr) });
        }
    }
}

impl DecodedFrame {
    /// Another owning handle to the same pixel buffer, for keeping it alive
    /// while the GPU still reads it.
    pub fn clone_ref(&self) -> DecodedFrame {
        if let Some(ptr) = NonNull::new(self.pixel_buffer as *mut CVImageBuffer) {
            // SAFETY: a live CVPixelBuffer; this takes a second +1.
            std::mem::forget(unsafe { CFRetained::retain(ptr) });
        }
        DecodedFrame { ..*self }
    }
}

pub struct VtDecoder {
    shared: Box<Shared>,
    session: Option<CFRetained<VTDecompressionSession>>,
    format: Option<CFRetained<CMVideoFormatDescription>>,
    codec: Codec,
    /// Parameter sets the session was built from (VPS/SPS/PPS or SPS/PPS).
    params: Vec<Vec<u8>>,
    /// The current frame's slice NALs, as (offset, length) in its Annex-B
    /// bytes: written straight into the sample's block buffer, length-prefixed.
    slices: Vec<(usize, usize)>,
    frame_index: i64,
    skipped_before_parameter_sets: u64,
    picture: PictureFormat,
}

// SAFETY: the session is only used through &mut self; VideoToolbox sessions
// are not thread-affine.
unsafe impl Send for VtDecoder {}

impl VtDecoder {
    pub fn new(codec: Codec) -> Result<VtDecoder, DecodeError> {
        Self::build(codec, None)
    }

    /// A decoder that hands every frame to `sink` as soon as it is decoded.
    pub fn with_sink(codec: Codec, sink: FrameSink) -> Result<VtDecoder, DecodeError> {
        Self::build(codec, Some(sink))
    }

    fn build(codec: Codec, sink: Option<FrameSink>) -> Result<VtDecoder, DecodeError> {
        if codec == Codec::Av1 {
            return Err(DecodeError::Bitstream(
                "AV1 decode is not implemented".into(),
            ));
        }
        Ok(VtDecoder {
            shared: Box::new(Shared {
                sink,
                ready: Mutex::new(VecDeque::with_capacity(MAX_READY)),
                failed: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
            }),
            session: None,
            format: None,
            codec,
            params: Vec::new(),
            slices: Vec::new(),
            frame_index: 0,
            skipped_before_parameter_sets: 0,
            picture: PictureFormat::default(),
        })
    }

    /// The stream's pictures are `format` (from the session's ack): decoded
    /// into a pixel format Metal samples as it is.
    pub fn set_format(&mut self, format: PictureFormat) {
        if format != self.picture {
            self.picture = format;
            // Built again, for the new output, at the next parameter sets.
            self.teardown_session();
            self.params.clear();
        }
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    pub fn failed_and_dropped(&self) -> (u64, u64) {
        (
            self.shared.failed.load(Ordering::Relaxed),
            self.shared.dropped.load(Ordering::Relaxed),
        )
    }

    pub fn skipped_before_parameter_sets(&self) -> u64 {
        self.skipped_before_parameter_sets
    }

    fn rebuild(&mut self, params: &[&[u8]]) -> Result<(), DecodeError> {
        self.teardown_session();

        let ptrs: Vec<NonNull<u8>> = params
            .iter()
            .map(|p| NonNull::new(p.as_ptr() as *mut u8).expect("slices are never null"))
            .collect();
        let sizes: Vec<usize> = params.iter().map(|p| p.len()).collect();
        let mut fmt: *const CMFormatDescription = std::ptr::null();
        // SAFETY: `ptrs`/`sizes` describe `params.len()` live slices, and `fmt`
        // is a valid out-param.
        let (status, call) = unsafe {
            match self.codec {
                Codec::H264 => (
                    CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        None,
                        params.len(),
                        NonNull::new(ptrs.as_ptr() as *mut NonNull<u8>).unwrap(),
                        NonNull::new(sizes.as_ptr() as *mut usize).unwrap(),
                        LENGTH_BYTES,
                        NonNull::new(&mut fmt as *mut *const CMFormatDescription).unwrap(),
                    ),
                    "CMVideoFormatDescriptionCreateFromH264ParameterSets",
                ),
                _ => (
                    CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        None,
                        params.len(),
                        NonNull::new(ptrs.as_ptr() as *mut NonNull<u8>).unwrap(),
                        NonNull::new(sizes.as_ptr() as *mut usize).unwrap(),
                        LENGTH_BYTES,
                        None,
                        NonNull::new(&mut fmt as *mut *const CMFormatDescription).unwrap(),
                    ),
                    "CMVideoFormatDescriptionCreateFromHEVCParameterSets",
                ),
            }
        };
        if status != 0 {
            return Err(DecodeError::Os { call, status });
        }
        let fmt = NonNull::new(fmt as *mut CMVideoFormatDescription)
            .ok_or_else(|| DecodeError::Bitstream("no format description".into()))?;
        // SAFETY: created with +1.
        let fmt: CFRetained<CMVideoFormatDescription> = unsafe { CFRetained::from_raw(fmt) };

        let callback_record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
            decompressionOutputRefCon: &*self.shared as *const Shared as *mut c_void,
        };
        let attributes = self.picture.pixel_format().map(|fourcc| {
            let pixel_format = CFNumber::new_i32(fourcc as i32);
            let yes: &CFType = CFBoolean::new(true);
            // SAFETY: CoreVideo's constant keys.
            let keys: [&CFString; 2] = unsafe {
                [
                    kCVPixelBufferPixelFormatTypeKey,
                    kCVPixelBufferMetalCompatibilityKey,
                ]
            };
            let values: [&CFType; 2] = [&pixel_format, yes];
            CFDictionary::<CFString, CFType>::from_slices(&keys, &values)
        });
        let mut session: *mut VTDecompressionSession = std::ptr::null_mut();
        // SAFETY: valid format and callback; the refcon outlives the session.
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &fmt,
                None,
                attributes.as_deref().map(|d| d.as_opaque()),
                &callback_record,
                NonNull::new(&mut session as *mut *mut VTDecompressionSession).unwrap(),
            )
        };
        if status != 0 {
            return Err(DecodeError::Os {
                call: "VTDecompressionSessionCreate",
                status,
            });
        }
        let session = NonNull::new(session).ok_or(DecodeError::Os {
            call: "VTDecompressionSessionCreate",
            status: 0,
        })?;
        // SAFETY: created with +1.
        let session: CFRetained<VTDecompressionSession> = unsafe { CFRetained::from_raw(session) };

        // Real-time: decode for display now, don't optimise for throughput.
        // SAFETY: a live session and a static CFBoolean.
        let status = unsafe {
            let yes: &CFType = kCFBooleanTrue.expect("kCFBooleanTrue");
            VTSessionSetProperty(&session, kVTDecompressionPropertyKey_RealTime, Some(yes))
        };
        if status != 0 {
            tracing::warn!(status, "VideoToolbox refused real-time mode");
        }

        self.params = params.iter().map(|p| p.to_vec()).collect();
        self.format = Some(fmt);
        self.session = Some(session);
        tracing::info!(codec = ?self.codec, format = ?self.picture, "decoder (re)built from new parameter sets");
        Ok(())
    }

    fn teardown_session(&mut self) {
        if let Some(session) = self.session.take() {
            // SAFETY: a live session; draining before invalidating means no
            // callback can run after the shared state is gone.
            unsafe {
                session.wait_for_asynchronous_frames();
                session.invalidate();
            }
        }
        self.format = None;
    }

    /// Decode the slices in `self.slices` (of `annexb`) as one sample. The
    /// AVCC form (each NAL behind its 4-byte length) is written directly into
    /// the block buffer CoreMedia allocates: one copy of the frame, not two.
    fn submit(
        &self,
        annexb: &[u8],
        capture_ts_us: u32,
        frame_index: i64,
    ) -> Result<(), DecodeError> {
        let session = self.session.as_ref().expect("checked by caller");
        let fmt = self.format.as_ref().expect("checked by caller");
        let len: usize = self
            .slices
            .iter()
            .map(|&(_, n)| LENGTH_BYTES as usize + n)
            .sum();

        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        // SAFETY: CoreMedia allocates `len` bytes; `block` is a valid out-param.
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                std::ptr::null_mut(),
                len,
                None,
                std::ptr::null(),
                0,
                len,
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::new(&mut block as *mut *mut CMBlockBuffer).unwrap(),
            )
        };
        if status != 0 {
            return Err(DecodeError::Os {
                call: "CMBlockBufferCreateWithMemoryBlock",
                status,
            });
        }
        let block = NonNull::new(block).ok_or(DecodeError::Os {
            call: "CMBlockBufferCreateWithMemoryBlock",
            status: 0,
        })?;
        // SAFETY: created with +1.
        let block: CFRetained<CMBlockBuffer> = unsafe { CFRetained::from_raw(block) };
        let mut data: *mut std::ffi::c_char = std::ptr::null_mut();
        let mut contiguous = 0usize;
        // SAFETY: a live block buffer; the out-params are valid.
        let status =
            unsafe { block.data_pointer(0, &mut contiguous, std::ptr::null_mut(), &mut data) };
        if status != 0 || data.is_null() || contiguous < len {
            return Err(DecodeError::Os {
                call: "CMBlockBufferGetDataPointer",
                status,
            });
        }
        // SAFETY: one memory block of `len` bytes, allocated just now (the
        // AssureMemoryNow flag) and not yet shared with anyone.
        let out = unsafe { std::slice::from_raw_parts_mut(data as *mut u8, len) };
        let mut at = 0;
        for &(start, n) in &self.slices {
            out[at..at + 4].copy_from_slice(&(n as u32).to_be_bytes());
            out[at + 4..at + 4 + n].copy_from_slice(&annexb[start..start + n]);
            at += 4 + n;
        }

        let t = |value| CMTime {
            value,
            timescale: 1000,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let timing = CMSampleTimingInfo {
            duration: t(1),
            presentationTimeStamp: t(frame_index),
            decodeTimeStamp: t(frame_index),
        };
        let sizes = [len];
        let mut sample: *mut CMSampleBuffer = std::ptr::null_mut();
        // SAFETY: one sample of `len` bytes backed by `block`.
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(fmt),
                1,
                1,
                &timing,
                1,
                sizes.as_ptr(),
                NonNull::new(&mut sample as *mut *mut CMSampleBuffer).unwrap(),
            )
        };
        if status != 0 {
            return Err(DecodeError::Os {
                call: "CMSampleBufferCreateReady",
                status,
            });
        }
        let sample = NonNull::new(sample).ok_or(DecodeError::Os {
            call: "CMSampleBufferCreateReady",
            status: 0,
        })?;
        // SAFETY: created with +1.
        let sample: CFRetained<CMSampleBuffer> = unsafe { CFRetained::from_raw(sample) };

        // SAFETY: live session and sample; the refcon is a plain integer.
        let status = unsafe {
            session.decode_frame(
                &sample,
                VTDecodeFrameFlags::Frame_EnableAsynchronousDecompression,
                capture_ts_us as usize as *mut c_void,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(DecodeError::Os {
                call: "VTDecompressionSessionDecodeFrame",
                status,
            });
        }
        Ok(())
    }
}

/// What a NAL unit is, independent of codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NalKind {
    /// Parameter set, with its position in the set the format description
    /// takes (H.264: SPS=0, PPS=1; HEVC: VPS=0, SPS=1, PPS=2).
    Param(usize),
    Slice,
    Other,
}

pub fn classify(codec: Codec, nal: &[u8]) -> NalKind {
    let Some(&b0) = nal.first() else {
        return NalKind::Other;
    };
    match codec {
        Codec::H264 => match b0 & 0x1F {
            7 => NalKind::Param(0),
            8 => NalKind::Param(1),
            1..=5 => NalKind::Slice,
            _ => NalKind::Other,
        },
        _ => match (b0 >> 1) & 0x3F {
            32 => NalKind::Param(0),
            33 => NalKind::Param(1),
            34 => NalKind::Param(2),
            0..=31 => NalKind::Slice,
            _ => NalKind::Other,
        },
    }
}

fn param_count(codec: Codec) -> usize {
    match codec {
        Codec::H264 => 2,
        _ => 3,
    }
}

impl VideoDecoder for VtDecoder {
    fn decode(&mut self, annexb: &[u8], capture_ts_us: u32) -> Result<(), DecodeError> {
        let n = param_count(self.codec);
        let mut params: [Option<&[u8]>; 3] = [None; 3];
        self.slices.clear();
        for nal in annexb::nal_units(annexb) {
            match classify(self.codec, nal) {
                NalKind::Param(i) if i < n => {
                    // First of each kind: some encoders emit several SPS/PPS.
                    if params[i].is_none() {
                        params[i] = Some(nal);
                    }
                }
                NalKind::Slice => {
                    // `nal` is a subslice of `annexb`.
                    let start = nal.as_ptr() as usize - annexb.as_ptr() as usize;
                    self.slices.push((start, nal.len()));
                }
                _ => {}
            }
        }
        let params = &params[..n];
        if params.iter().all(Option::is_some) {
            let changed = self.params.len() != params.len()
                || self
                    .params
                    .iter()
                    .zip(params)
                    .any(|(a, b)| Some(a.as_slice()) != *b);
            if changed || self.session.is_none() {
                let params: Vec<&[u8]> = params.iter().map(|p| p.unwrap()).collect();
                self.rebuild(&params)?;
            }
        }
        if self.session.is_none() {
            self.skipped_before_parameter_sets += 1;
            return Ok(());
        }
        if self.slices.is_empty() {
            return Ok(());
        }
        let index = self.frame_index;
        self.frame_index += 1;
        self.submit(annexb, capture_ts_us, index)
    }

    fn poll(&mut self) -> Option<DecodedFrame> {
        self.shared
            .ready
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }
}

impl Drop for VtDecoder {
    fn drop(&mut self) {
        self.teardown_session();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_both_start_code_lengths() {
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0, 0, 0, 1, 0x65, 0xCC, 0xDD,
        ];
        let nals: Vec<_> = annexb::nal_units(&stream).collect();
        assert_eq!(
            nals,
            vec![&[0x67, 0xAA][..], &[0x68, 0xBB], &[0x65, 0xCC, 0xDD]]
        );
        assert_eq!(classify(Codec::H264, nals[0]), NalKind::Param(0));
        assert_eq!(classify(Codec::H264, nals[1]), NalKind::Param(1));
        assert_eq!(classify(Codec::H264, nals[2]), NalKind::Slice);
    }

    #[test]
    fn hevc_nal_types() {
        assert_eq!(classify(Codec::Hevc, &[0x40, 0x01]), NalKind::Param(0)); // VPS
        assert_eq!(classify(Codec::Hevc, &[0x42, 0x01]), NalKind::Param(1)); // SPS
        assert_eq!(classify(Codec::Hevc, &[0x44, 0x01]), NalKind::Param(2)); // PPS
        assert_eq!(classify(Codec::Hevc, &[0x26, 0x01]), NalKind::Slice); // IDR_W_RADL
        assert_eq!(classify(Codec::Hevc, &[0x02, 0x01]), NalKind::Slice); // TRAIL_R
        assert_eq!(classify(Codec::Hevc, &[0x4E, 0x01]), NalKind::Other); // SEI
    }
}
