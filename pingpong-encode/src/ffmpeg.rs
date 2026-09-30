//! Encoding on Linux: FFmpeg's encoders -- VA-API (Intel, AMD), NVENC
//! (NVIDIA), else x264 in software -- configured for streaming the way
//! Sunshine configures them: no B-frames, an endless GOP, a VBV of about one
//! frame, SPS/PPS with every IDR.
//!
//! Images come in as BGRX or RGBX at the screen's size; swscale turns them into the
//! encoder's 4:2:0 (BT.709, limited range) at the stream's size, scaled to
//! fit and letterboxed ([`fit`]). Encoding is synchronous: a frame is out by
//! the time `submit` returns, so nothing is ever in flight, and there is no
//! reference invalidation (a loss costs an IDR).

use std::collections::VecDeque;
use std::ffi::{CStr, CString};
use std::time::Duration;

use ffmpeg_sys_next as ff;

use crate::{Codec, EncodeError, EncodedFrame, EncoderConfig, FrameKind};

/// An image to encode: 4 bytes a pixel, `stride` bytes a row.
pub struct Image<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    /// Red first (RGBX) rather than blue (BGRX).
    pub rgb: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vaapi,
    Nvenc,
    Software,
}

impl Backend {
    fn encoder_name(self, codec: Codec) -> &'static CStr {
        match (self, codec) {
            (Backend::Vaapi, Codec::H264) => c"h264_vaapi",
            (Backend::Vaapi, Codec::Hevc) => c"hevc_vaapi",
            (Backend::Vaapi, Codec::Av1) => c"av1_vaapi",
            (Backend::Nvenc, Codec::H264) => c"h264_nvenc",
            (Backend::Nvenc, Codec::Hevc) => c"hevc_nvenc",
            (Backend::Nvenc, Codec::Av1) => c"av1_nvenc",
            (Backend::Software, Codec::H264) => c"libx264",
            (Backend::Software, Codec::Hevc) => c"libx265",
            (Backend::Software, Codec::Av1) => c"libsvtav1",
        }
    }
}

/// Where the picture goes in a `dw`x`dh` frame: `sw`x`sh` scaled to fit,
/// centred, on even pixels (4:2:0). (x, y, w, h).
pub fn fit(sw: u32, sh: u32, dw: u32, dh: u32) -> (u32, u32, u32, u32) {
    if sw == 0 || sh == 0 {
        return (0, 0, dw, dh);
    }
    let (w, h) = if sw as u64 * dh as u64 > sh as u64 * dw as u64 {
        (dw, (sh as u64 * dw as u64 / sw as u64) as u32)
    } else {
        ((sw as u64 * dh as u64 / sh as u64) as u32, dh)
    };
    let (w, h) = ((w & !1).max(2), (h & !1).max(2));
    (((dw - w) / 2) & !1, ((dh - h) / 2) & !1, w, h)
}

fn err(what: &str, status: i32) -> EncodeError {
    let mut buf = [0 as std::ffi::c_char; 128];
    unsafe {
        ff::av_strerror(status, buf.as_mut_ptr(), buf.len());
    }
    let msg = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    EncodeError::Unsupported(format!("{what}: {msg} ({status})"))
}

/// AVERROR(EAGAIN) with Linux's EAGAIN.
const EAGAIN: i32 = -11;

pub struct FfmpegEncoder {
    ctx: *mut ff::AVCodecContext,
    backend: Backend,
    config: EncoderConfig,
    /// The encoder's input in system memory (NV12 or YUV420P), letterboxed.
    frame: *mut ff::AVFrame,
    sw_format: ff::AVPixelFormat,
    /// VA-API: the surface it is uploaded to.
    hw_frame: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    sws: *mut ff::SwsContext,
    /// The source size and order `sws` was made for.
    sws_for: (u32, u32, bool),
    out: VecDeque<EncodedFrame>,
    /// A bitrate change this backend cannot make while encoding, told once.
    told_fixed_rate: bool,
}

// SAFETY: used by one thread at a time; FFmpeg keeps no thread affinity.
unsafe impl Send for FfmpegEncoder {}

impl FfmpegEncoder {
    /// The first of VA-API, NVENC and software that opens for `config`.
    pub fn new(config: EncoderConfig) -> Result<FfmpegEncoder, EncodeError> {
        // FFmpeg's own messages: none while backends are tried (the ones
        // missing say so on stderr), then warnings and errors only (not
        // x264's statistics at the end of each session).
        unsafe { ff::av_log_set_level(ff::AV_LOG_QUIET) };
        let found = FfmpegEncoder::first(config);
        unsafe { ff::av_log_set_level(ff::AV_LOG_WARNING) };
        found
    }

    fn first(config: EncoderConfig) -> Result<FfmpegEncoder, EncodeError> {
        let mut last = EncodeError::Unsupported(format!("no {} encoder", config.codec.name()));
        for backend in [Backend::Vaapi, Backend::Nvenc, Backend::Software] {
            if backend == Backend::Software && config.codec != Codec::H264 {
                // x265 and SVT-AV1 cannot keep up in real time on most
                // machines; H.264 is offered instead.
                continue;
            }
            match FfmpegEncoder::with(backend, config) {
                Ok(e) => return Ok(e),
                Err(e) => {
                    tracing::debug!(?backend, codec = config.codec.name(), error = %e, "encoder not available");
                    last = e;
                }
            }
        }
        Err(last)
    }

    /// The codecs something here can encode, fastest backend first found.
    pub fn probe() -> Vec<(Codec, Backend)> {
        let mut found = Vec::new();
        for codec in [Codec::Hevc, Codec::H264, Codec::Av1] {
            let config = EncoderConfig {
                codec,
                width: 1280,
                height: 720,
                fps: 60,
                bitrate_bps: 10_000_000,
                preset: 1,
                two_pass: false,
                slices: 1,
            };
            if let Ok(e) = FfmpegEncoder::new(config) {
                found.push((codec, e.backend));
            }
        }
        found
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn with(backend: Backend, config: EncoderConfig) -> Result<FfmpegEncoder, EncodeError> {
        unsafe {
            let codec =
                ff::avcodec_find_encoder_by_name(backend.encoder_name(config.codec).as_ptr());
            if codec.is_null() {
                return Err(EncodeError::Unsupported(format!(
                    "FFmpeg has no {:?}",
                    backend.encoder_name(config.codec)
                )));
            }
            let mut e = FfmpegEncoder {
                ctx: ff::avcodec_alloc_context3(codec),
                backend,
                config,
                frame: ff::av_frame_alloc(),
                sw_format: ff::AVPixelFormat::AV_PIX_FMT_NV12,
                hw_frame: ff::av_frame_alloc(),
                packet: ff::av_packet_alloc(),
                sws: std::ptr::null_mut(),
                sws_for: (0, 0, false),
                out: VecDeque::new(),
                told_fixed_rate: false,
            };
            if e.ctx.is_null() || e.frame.is_null() || e.hw_frame.is_null() || e.packet.is_null() {
                return Err(EncodeError::Unsupported("out of memory".into()));
            }
            let c = &mut *e.ctx;
            c.width = config.width as i32;
            c.height = config.height as i32;
            c.time_base = ff::AVRational {
                num: 1,
                den: config.fps.max(1) as i32,
            };
            c.framerate = ff::AVRational {
                num: config.fps.max(1) as i32,
                den: 1,
            };
            // Keyframes only when asked for (a loss, a new client).
            c.gop_size = i32::MAX;
            c.max_b_frames = 0;
            c.flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            c.color_primaries = ff::AVColorPrimaries::AVCOL_PRI_BT709;
            c.color_trc = ff::AVColorTransferCharacteristic::AVCOL_TRC_BT709;
            c.colorspace = ff::AVColorSpace::AVCOL_SPC_BT709;
            c.color_range = ff::AVColorRange::AVCOL_RANGE_MPEG;
            if config.slices > 1 {
                c.slices = config.slices as i32;
            }
            e.apply_rate(config.bitrate_bps);
            e.sw_format = match backend {
                Backend::Vaapi => {
                    e.attach_vaapi()?;
                    ff::AVPixelFormat::AV_PIX_FMT_NV12
                }
                Backend::Nvenc => {
                    (*e.ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_NV12;
                    ff::AVPixelFormat::AV_PIX_FMT_NV12
                }
                Backend::Software => {
                    (*e.ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_YUV420P;
                    (*e.ctx).thread_count = 0;
                    ff::AVPixelFormat::AV_PIX_FMT_YUV420P
                }
            };
            e.set_options();
            let status = ff::avcodec_open2(e.ctx, codec, std::ptr::null_mut());
            if status < 0 {
                return Err(err("avcodec_open2", status));
            }
            let f = &mut *e.frame;
            f.format = e.sw_format as i32;
            f.width = config.width as i32;
            f.height = config.height as i32;
            let status = ff::av_frame_get_buffer(e.frame, 0);
            if status < 0 {
                return Err(err("av_frame_get_buffer", status));
            }
            e.fill_black();
            tracing::info!(
                ?backend,
                codec = config.codec.name(),
                width = config.width,
                height = config.height,
                fps = config.fps,
                mbps = config.bitrate_bps / 1_000_000,
                "encoder ready"
            );
            Ok(e)
        }
    }

    /// Bitrate and a VBV of a frame and a half.
    unsafe fn apply_rate(&mut self, bps: u32) {
        let c = unsafe { &mut *self.ctx };
        c.bit_rate = bps as i64;
        c.rc_max_rate = bps as i64;
        c.rc_buffer_size = (bps as u64 * 3 / 2 / self.config.fps.max(1) as u64) as i32;
    }

    unsafe fn opt(&self, name: &CStr, value: &str) {
        let value = CString::new(value).unwrap();
        let status =
            unsafe { ff::av_opt_set((*self.ctx).priv_data, name.as_ptr(), value.as_ptr(), 0) };
        if status < 0 {
            tracing::debug!(option = ?name, value = ?value, status, "encoder option not taken");
        }
    }

    unsafe fn set_options(&self) {
        unsafe {
            match self.backend {
                Backend::Vaapi => {
                    self.opt(c"rc_mode", "CBR");
                    self.opt(c"async_depth", "1");
                    self.opt(c"idr_interval", "0");
                }
                Backend::Nvenc => {
                    let preset = format!("p{}", self.config.preset.clamp(1, 7));
                    self.opt(c"preset", &preset);
                    self.opt(c"tune", "ull");
                    self.opt(c"rc", "cbr");
                    self.opt(c"zerolatency", "1");
                    self.opt(c"delay", "0");
                    self.opt(c"forced-idr", "1");
                }
                Backend::Software => {
                    self.opt(c"preset", "superfast");
                    self.opt(c"tune", "zerolatency");
                    self.opt(c"forced-idr", "1");
                }
            }
        }
    }

    /// VA-API: the device, and a pool of surfaces at the stream's size.
    unsafe fn attach_vaapi(&mut self) -> Result<(), EncodeError> {
        unsafe {
            let mut device: *mut ff::AVBufferRef = std::ptr::null_mut();
            let status = ff::av_hwdevice_ctx_create(
                &mut device,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            );
            if status < 0 {
                return Err(err("no VA-API device", status));
            }
            let mut frames = ff::av_hwframe_ctx_alloc(device);
            ff::av_buffer_unref(&mut device);
            if frames.is_null() {
                return Err(EncodeError::Unsupported("av_hwframe_ctx_alloc".into()));
            }
            let fc = &mut *((*frames).data as *mut ff::AVHWFramesContext);
            fc.format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            fc.sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            fc.width = self.config.width as i32;
            fc.height = self.config.height as i32;
            fc.initial_pool_size = 4;
            let status = ff::av_hwframe_ctx_init(frames);
            if status < 0 {
                ff::av_buffer_unref(&mut frames);
                return Err(err("VA-API surfaces", status));
            }
            (*self.ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            (*self.ctx).hw_frames_ctx = frames;
            Ok(())
        }
    }

    /// The letterbox bars: black (16, 128, 128).
    unsafe fn fill_black(&mut self) {
        unsafe {
            let f = &*self.frame;
            let h = f.height as usize;
            std::ptr::write_bytes(f.data[0], 16, f.linesize[0] as usize * h);
            let planes = if self.sw_format == ff::AVPixelFormat::AV_PIX_FMT_NV12 {
                1
            } else {
                2
            };
            for p in 1..=planes {
                std::ptr::write_bytes(f.data[p], 128, f.linesize[p] as usize * h.div_ceil(2));
            }
        }
    }

    /// Scale and convert `image` into the encoder's frame.
    unsafe fn convert(&mut self, image: &Image) -> Result<(), EncodeError> {
        unsafe {
            let (dw, dh) = (self.config.width, self.config.height);
            let (x, y, w, h) = fit(image.width, image.height, dw, dh);
            if self.sws.is_null() || self.sws_for != (image.width, image.height, image.rgb) {
                ff::sws_freeContext(self.sws);
                self.sws = ff::sws_getContext(
                    image.width as i32,
                    image.height as i32,
                    if image.rgb {
                        ff::AVPixelFormat::AV_PIX_FMT_RGB0
                    } else {
                        ff::AVPixelFormat::AV_PIX_FMT_BGR0
                    },
                    w as i32,
                    h as i32,
                    self.sw_format,
                    ff::SWS_BILINEAR,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                if self.sws.is_null() {
                    return Err(EncodeError::Unsupported(
                        "swscale cannot convert this".into(),
                    ));
                }
                // Full-range RGB in, BT.709 limited range out.
                let bt709 = ff::sws_getCoefficients(ff::SWS_CS_ITU709);
                ff::sws_setColorspaceDetails(self.sws, bt709, 1, bt709, 0, 0, 1 << 16, 1 << 16);
                self.sws_for = (image.width, image.height, image.rgb);
                if (x, y, w, h) != (0, 0, dw, dh) {
                    tracing::info!(from = ?(image.width, image.height), to = ?(w, h), at = ?(x, y), "picture letterboxed");
                }
            }
            // The encoder may still hold the last one: a fresh buffer then
            // (with the bars copied along).
            let status = ff::av_frame_make_writable(self.frame);
            if status < 0 {
                return Err(err("av_frame_make_writable", status));
            }
            let f = &*self.frame;
            let nv12 = self.sw_format == ff::AVPixelFormat::AV_PIX_FMT_NV12;
            let mut dst: [*mut u8; 4] = [std::ptr::null_mut(); 4];
            let mut strides = [0i32; 4];
            dst[0] = f.data[0].add(y as usize * f.linesize[0] as usize + x as usize);
            strides[0] = f.linesize[0];
            if nv12 {
                dst[1] = f.data[1].add((y / 2) as usize * f.linesize[1] as usize + x as usize);
                strides[1] = f.linesize[1];
            } else {
                for p in 1..3 {
                    dst[p] =
                        f.data[p].add((y / 2) as usize * f.linesize[p] as usize + (x / 2) as usize);
                    strides[p] = f.linesize[p];
                }
            }
            let src = [
                image.data.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
            ];
            let src_strides = [image.stride as i32, 0, 0, 0];
            ff::sws_scale(
                self.sws,
                src.as_ptr(),
                src_strides.as_ptr(),
                0,
                image.height as i32,
                dst.as_ptr(),
                strides.as_ptr(),
            );
            Ok(())
        }
    }

    /// Encode `image` as frame `index`; what comes out waits in `next`.
    pub fn submit(
        &mut self,
        image: &Image,
        index: u64,
        force_idr: bool,
    ) -> Result<(), EncodeError> {
        unsafe {
            self.convert(image)?;
            let mut input = self.frame;
            if self.backend == Backend::Vaapi {
                ff::av_frame_unref(self.hw_frame);
                let status = ff::av_hwframe_get_buffer((*self.ctx).hw_frames_ctx, self.hw_frame, 0);
                if status < 0 {
                    return Err(err("VA-API surface", status));
                }
                let status = ff::av_hwframe_transfer_data(self.hw_frame, self.frame, 0);
                if status < 0 {
                    return Err(err("VA-API upload", status));
                }
                input = self.hw_frame;
            }
            (*input).pts = index as i64;
            (*input).pict_type = if force_idr {
                ff::AVPictureType::AV_PICTURE_TYPE_I
            } else {
                ff::AVPictureType::AV_PICTURE_TYPE_NONE
            };
            let status = ff::avcodec_send_frame(self.ctx, input);
            if status < 0 {
                return Err(err("avcodec_send_frame", status));
            }
            loop {
                let status = ff::avcodec_receive_packet(self.ctx, self.packet);
                if status == EAGAIN || status == ff::AVERROR_EOF {
                    break;
                }
                if status < 0 {
                    return Err(err("avcodec_receive_packet", status));
                }
                let p = &*self.packet;
                let data = std::slice::from_raw_parts(p.data, p.size as usize).to_vec();
                let kind = if p.flags & ff::AV_PKT_FLAG_KEY != 0 {
                    FrameKind::Idr
                } else {
                    FrameKind::P
                };
                self.out.push_back(EncodedFrame {
                    data,
                    kind,
                    index: p.pts as u64,
                });
                ff::av_packet_unref(self.packet);
            }
            Ok(())
        }
    }

    /// The next frame encoded. Never waits: `submit` has done the work.
    pub fn next(&mut self, _timeout: Duration) -> Option<Result<EncodedFrame, EncodeError>> {
        self.out.pop_front().map(Ok)
    }

    pub fn in_flight(&self) -> usize {
        0
    }

    /// No long-term references here: nothing to do.
    pub fn acknowledge_through(&mut self, _index: u64) {}

    /// No reference invalidation: the caller sends an IDR.
    pub fn invalidate(&mut self, _first: u64, _last: u64) -> bool {
        false
    }

    /// x264 and NVENC take a new rate between frames; VA-API keeps the one
    /// it opened with.
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        if self.backend == Backend::Vaapi {
            if !self.told_fixed_rate {
                tracing::info!(
                    mbps = bitrate_bps / 1_000_000,
                    "VA-API keeps its bitrate for the session"
                );
                self.told_fixed_rate = true;
            }
            return Ok(());
        }
        unsafe {
            self.apply_rate(bitrate_bps);
        }
        Ok(())
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        unsafe {
            ff::sws_freeContext(self.sws);
            ff::av_packet_free(&mut self.packet);
            ff::av_frame_free(&mut self.hw_frame);
            ff::av_frame_free(&mut self.frame);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letterbox() {
        assert_eq!(fit(1920, 1080, 1280, 720), (0, 0, 1280, 720));
        assert_eq!(fit(1920, 1080, 1280, 800), (0, 40, 1280, 720));
        assert_eq!(fit(1280, 1024, 1920, 1080), (284, 0, 1350, 1080));
        assert_eq!(fit(2560, 1600, 1920, 1080), (96, 0, 1728, 1080));
    }

    /// x264 turns a picture into an IDR, then P-frames, then an IDR on demand.
    #[test]
    fn software_h264_idr_on_demand() {
        let config = EncoderConfig {
            codec: Codec::H264,
            width: 640,
            height: 360,
            fps: 60,
            bitrate_bps: 4_000_000,
            preset: 1,
            two_pass: false,
            slices: 1,
        };
        let Ok(mut e) = FfmpegEncoder::with(Backend::Software, config) else {
            eprintln!("no libx264 here; skipped");
            return;
        };
        let (w, h) = (800u32, 600u32);
        let mut kinds = Vec::new();
        for i in 0..6u64 {
            let px: Vec<u8> = (0..w * h)
                .flat_map(|p| [(p % 256) as u8, (i * 40) as u8, 128, 0])
                .collect();
            e.submit(
                &Image {
                    data: &px,
                    width: w,
                    height: h,
                    stride: w as usize * 4,
                    rgb: false,
                },
                i,
                i == 0 || i == 4,
            )
            .unwrap();
            let f = e
                .next(Duration::ZERO)
                .expect("a frame for each picture")
                .unwrap();
            assert_eq!(f.index, i);
            assert!(
                f.data.starts_with(&[0, 0, 0, 1]) || f.data.starts_with(&[0, 0, 1]),
                "Annex B"
            );
            kinds.push(f.kind);
        }
        use FrameKind::*;
        assert_eq!(kinds, [Idr, P, P, P, Idr, P]);
    }
}
