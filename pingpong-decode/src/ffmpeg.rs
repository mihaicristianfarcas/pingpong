//! Decoding on Linux: FFmpeg's decoders, with VA-API when the machine has
//! it (Intel, AMD, NVIDIA through nvidia-vaapi-driver) and in software
//! otherwise. Pictures come out in system memory -- NV12 from VA-API,
//! planar 4:2:0 (or 4:4:4) from software -- for the presenter to upload.

use ffmpeg_sys_next as ff;

use crate::{Codec, DecodeError};

/// A decoded picture's planes, valid only for the callback it is handed to.
pub struct Picture<'a> {
    pub width: u32,
    pub height: u32,
    pub layout: Layout<'a>,
    /// The tag given to [`FfmpegDecoder::decode`].
    pub tag: u32,
    /// When the decoder handed it over, on the shared client clock.
    pub decoded_at_us: u32,
}

pub enum Layout<'a> {
    /// Luma, then interleaved chroma at half size.
    Nv12 { y: Plane<'a>, uv: Plane<'a> },
    /// Luma, then the two chroma planes at half size.
    I420 {
        y: Plane<'a>,
        u: Plane<'a>,
        v: Plane<'a>,
    },
    /// Luma and the two chroma planes, all at full size.
    I444 {
        y: Plane<'a>,
        u: Plane<'a>,
        v: Plane<'a>,
    },
}

pub struct Plane<'a> {
    pub data: &'a [u8],
    /// Bytes from one row to the next.
    pub stride: usize,
}

pub struct FfmpegDecoder {
    ctx: *mut ff::AVCodecContext,
    packet: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    /// Where a hardware picture is copied down to.
    sw_frame: *mut ff::AVFrame,
    buf: Vec<u8>,
    hardware: bool,
}

// SAFETY: the codec context is only ever used by the thread that owns the
// decoder; FFmpeg keeps no thread affinity.
unsafe impl Send for FfmpegDecoder {}

/// AVERROR(EAGAIN) with Linux's EAGAIN.
const EAGAIN: i32 = -11;

fn err(call: &'static str, status: i32) -> DecodeError {
    DecodeError::Os { call, status }
}

unsafe extern "C" fn get_vaapi(
    _ctx: *mut ff::AVCodecContext,
    formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    let mut p = formats;
    let mut first = ff::AVPixelFormat::AV_PIX_FMT_NONE;
    unsafe {
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == ff::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return *p;
            }
            if first == ff::AVPixelFormat::AV_PIX_FMT_NONE {
                first = *p;
            }
            p = p.add(1);
        }
    }
    // No VA-API for this stream: the software format FFmpeg offered first.
    first
}

impl FfmpegDecoder {
    /// A decoder for `codec`: with VA-API unless `software`, or it is not
    /// there.
    pub fn new(codec: Codec, software: bool) -> Result<FfmpegDecoder, DecodeError> {
        let id = match codec {
            Codec::H264 => ff::AVCodecID::AV_CODEC_ID_H264,
            Codec::Hevc => ff::AVCodecID::AV_CODEC_ID_HEVC,
            Codec::Av1 => ff::AVCodecID::AV_CODEC_ID_AV1,
        };
        unsafe {
            let decoder = ff::avcodec_find_decoder(id);
            if decoder.is_null() {
                return Err(DecodeError::Bitstream(format!(
                    "FFmpeg has no {codec:?} decoder"
                )));
            }
            let ctx = ff::avcodec_alloc_context3(decoder);
            if ctx.is_null() {
                return Err(err("avcodec_alloc_context3", -1));
            }
            // A picture out for each one in, at once (no reordering delay).
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            let mut hardware = false;
            if !software {
                let mut device: *mut ff::AVBufferRef = std::ptr::null_mut();
                let status = ff::av_hwdevice_ctx_create(
                    &mut device,
                    ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    0,
                );
                if status >= 0 {
                    (*ctx).hw_device_ctx = ff::av_buffer_ref(device);
                    (*ctx).get_format = Some(get_vaapi);
                    ff::av_buffer_unref(&mut device);
                    hardware = true;
                } else {
                    tracing::info!(status, "no VA-API device; decoding in software");
                }
            }
            // Frame threads add a frame of latency each; a hardware decoder
            // wants one thread, software a few slices' worth.
            (*ctx).thread_count = if hardware { 1 } else { 4 };
            if !hardware {
                (*ctx).thread_type = ff::FF_THREAD_SLICE;
            }
            let status = ff::avcodec_open2(ctx, decoder, std::ptr::null_mut());
            if status < 0 {
                let mut ctx = ctx;
                ff::avcodec_free_context(&mut ctx);
                return Err(err("avcodec_open2", status));
            }
            tracing::info!(?codec, hardware, "FFmpeg decoder ready");
            Ok(FfmpegDecoder {
                ctx,
                packet: ff::av_packet_alloc(),
                frame: ff::av_frame_alloc(),
                sw_frame: ff::av_frame_alloc(),
                buf: Vec::new(),
                hardware,
            })
        }
    }

    pub fn is_hardware(&self) -> bool {
        self.hardware
    }

    /// Decode one access unit (Annex B). `tag` rides along to the picture;
    /// `on_picture` gets every picture that comes out.
    pub fn decode(
        &mut self,
        annexb: &[u8],
        tag: u32,
        mut on_picture: impl FnMut(Picture<'_>),
    ) -> Result<(), DecodeError> {
        self.buf.clear();
        self.buf.extend_from_slice(annexb);
        self.buf
            .resize(annexb.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
        unsafe {
            (*self.packet).data = self.buf.as_mut_ptr();
            (*self.packet).size = annexb.len() as i32;
            (*self.packet).pts = tag as i64;
            let status = ff::avcodec_send_packet(self.ctx, self.packet);
            (*self.packet).data = std::ptr::null_mut();
            (*self.packet).size = 0;
            if status < 0 && status != EAGAIN {
                return Err(err("avcodec_send_packet", status));
            }
            loop {
                let status = ff::avcodec_receive_frame(self.ctx, self.frame);
                if status == EAGAIN || status == ff::AVERROR_EOF {
                    return Ok(());
                }
                if status < 0 {
                    return Err(err("avcodec_receive_frame", status));
                }
                let tag = (*self.frame).pts as u32;
                let mut picture = self.frame;
                if (*self.frame).format == ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
                    // Down to system memory (NV12) for the presenter.
                    let status = ff::av_hwframe_transfer_data(self.sw_frame, self.frame, 0);
                    if status < 0 {
                        ff::av_frame_unref(self.frame);
                        return Err(err("av_hwframe_transfer_data", status));
                    }
                    picture = self.sw_frame;
                }
                let decoded_at_us = pingpong_proto::clock::now_us();
                let f = &*picture;
                let (w, h) = (f.width as u32, f.height as u32);
                let plane = |i: usize, rows: u32| -> Plane<'_> {
                    let stride = f.linesize[i] as usize;
                    Plane {
                        data: std::slice::from_raw_parts(f.data[i], stride * rows as usize),
                        stride,
                    }
                };
                let layout = match f.format {
                    x if x == ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32 => Some(Layout::Nv12 {
                        y: plane(0, h),
                        uv: plane(1, h.div_ceil(2)),
                    }),
                    x if x == ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32
                        || x == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32 =>
                    {
                        Some(Layout::I420 {
                            y: plane(0, h),
                            u: plane(1, h.div_ceil(2)),
                            v: plane(2, h.div_ceil(2)),
                        })
                    }
                    x if x == ff::AVPixelFormat::AV_PIX_FMT_YUV444P as i32
                        || x == ff::AVPixelFormat::AV_PIX_FMT_YUVJ444P as i32 =>
                    {
                        Some(Layout::I444 {
                            y: plane(0, h),
                            u: plane(1, h),
                            v: plane(2, h),
                        })
                    }
                    other => {
                        tracing::warn!(
                            format = other,
                            "a picture in a format the presenter cannot show; dropped"
                        );
                        None
                    }
                };
                if let Some(layout) = layout {
                    on_picture(Picture {
                        width: w,
                        height: h,
                        layout,
                        tag,
                        decoded_at_us,
                    });
                }
                ff::av_frame_unref(self.sw_frame);
                ff::av_frame_unref(self.frame);
            }
        }
    }
}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.sw_frame);
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.packet);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}
