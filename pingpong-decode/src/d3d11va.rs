//! Hardware decoding on Windows the way Moonlight does it: FFmpeg's H.264,
//! HEVC and AV1 decoders with the D3D11VA hwaccel, on the Direct3D 11 device
//! the client presents with. Each decoded picture is a slice of the
//! decoder's texture array, handed to the caller while it is still valid.
//!
//! The device's immediate context is shared: FFmpeg takes [`ContextLock`]
//! around its calls on it, and so must everyone else who uses it.

use std::ffi::c_void;
use std::sync::Arc;

use ffmpeg_sys_next as ff;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};

use crate::{Codec, DecodeError};

/// FFmpeg's AVD3D11VADeviceContext (libavutil/hwcontext_d3d11va.h, which
/// the bindings leave out).
#[repr(C)]
struct AvD3d11vaDeviceContext {
    device: *mut c_void,
    device_context: *mut c_void,
    video_device: *mut c_void,
    video_context: *mut c_void,
    lock: Option<unsafe extern "C" fn(*mut c_void)>,
    unlock: Option<unsafe extern "C" fn(*mut c_void)>,
    lock_ctx: *mut c_void,
}

/// The lock around the device's immediate context.
pub struct ContextLock(parking_lot::Mutex<()>);

impl Default for ContextLock {
    fn default() -> Self {
        ContextLock(parking_lot::Mutex::new(()))
    }
}

impl ContextLock {
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.0.lock()
    }
}

unsafe extern "C" fn lock(ctx: *mut c_void) {
    let l = unsafe { &*(ctx as *const ContextLock) };
    std::mem::forget(l.0.lock());
}

unsafe extern "C" fn unlock(ctx: *mut c_void) {
    let l = unsafe { &*(ctx as *const ContextLock) };
    unsafe { l.0.force_unlock() };
}

/// A decoded picture: slice `index` of `texture` (NV12 for 8-bit streams),
/// valid only for the callback it is handed to.
pub struct DecodedTexture<'a> {
    pub texture: &'a ID3D11Texture2D,
    pub index: u32,
    pub width: u32,
    pub height: u32,
    /// The tag given to [`D3d11Decoder::decode`].
    pub tag: u32,
    /// When the decoder handed it over, on the shared client clock.
    pub decoded_at_us: u32,
}

pub struct D3d11Decoder {
    ctx: *mut ff::AVCodecContext,
    packet: *mut ff::AVPacket,
    frame: *mut ff::AVFrame,
    /// The bitstream with the zeroed tail FFmpeg's readers may overrun into.
    buf: Vec<u8>,
    /// FFmpeg holds a raw pointer to it (the device's lock callbacks).
    _lock: Arc<ContextLock>,
    codec: Codec,
}

// SAFETY: the codec context is only ever used by the thread that owns the
// decoder; FFmpeg keeps no thread affinity.
unsafe impl Send for D3d11Decoder {}

unsafe extern "C" fn get_format(
    _ctx: *mut ff::AVCodecContext,
    formats: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    let mut p = formats;
    unsafe {
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == ff::AVPixelFormat::AV_PIX_FMT_D3D11 {
                return *p;
            }
            p = p.add(1);
        }
    }
    tracing::error!("the decoder offers no D3D11 output for this stream");
    ff::AVPixelFormat::AV_PIX_FMT_NONE
}

fn err(call: &'static str, status: i32) -> DecodeError {
    DecodeError::Os { call, status }
}

/// AVERROR(EAGAIN) with the Microsoft C runtime's EAGAIN.
const EAGAIN: i32 = -11;

impl D3d11Decoder {
    pub fn new(
        codec: Codec,
        device: &ID3D11Device,
        lock: Arc<ContextLock>,
    ) -> Result<D3d11Decoder, DecodeError> {
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
            let mut device_ref =
                ff::av_hwdevice_ctx_alloc(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA);
            if device_ref.is_null() {
                return Err(err("av_hwdevice_ctx_alloc", -1));
            }
            let hw = (*device_ref).data as *mut ff::AVHWDeviceContext;
            let d3d = (*hw).hwctx as *mut AvD3d11vaDeviceContext;
            // FFmpeg releases the device when the context goes: give it its
            // own reference.
            (*d3d).device = device.clone().into_raw();
            (*d3d).lock = Some(self::lock);
            (*d3d).unlock = Some(self::unlock);
            (*d3d).lock_ctx = Arc::as_ptr(&lock) as *mut c_void;
            let status = ff::av_hwdevice_ctx_init(device_ref);
            if status < 0 {
                ff::av_buffer_unref(&mut device_ref);
                return Err(err("av_hwdevice_ctx_init", status));
            }

            let ctx = ff::avcodec_alloc_context3(decoder);
            if ctx.is_null() {
                ff::av_buffer_unref(&mut device_ref);
                return Err(err("avcodec_alloc_context3", -1));
            }
            // A picture out for each one in, at once (no reordering delay),
            // on one thread: frame threads only add latency to a hwaccel.
            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            (*ctx).thread_count = 1;
            (*ctx).get_format = Some(get_format);
            (*ctx).hw_device_ctx = ff::av_buffer_ref(device_ref);
            ff::av_buffer_unref(&mut device_ref);
            let status = ff::avcodec_open2(ctx, decoder, std::ptr::null_mut());
            if status < 0 {
                let mut ctx = ctx;
                ff::avcodec_free_context(&mut ctx);
                return Err(err("avcodec_open2", status));
            }
            let packet = ff::av_packet_alloc();
            let frame = ff::av_frame_alloc();
            tracing::info!(?codec, "D3D11VA decoder ready");
            Ok(D3d11Decoder {
                ctx,
                packet,
                frame,
                buf: Vec::new(),
                _lock: lock,
                codec,
            })
        }
    }

    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Decode one access unit (Annex B). `tag` rides along to the picture;
    /// `on_frame` gets every picture that comes out.
    pub fn decode(
        &mut self,
        annexb: &[u8],
        tag: u32,
        mut on_frame: impl FnMut(DecodedTexture<'_>),
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
            // Not ours: the decoder keeps a copy.
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
                let f = &*self.frame;
                if f.format == ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 && !f.data[0].is_null() {
                    let raw = f.data[0] as *mut c_void;
                    if let Some(texture) = ID3D11Texture2D::from_raw_borrowed(&raw) {
                        on_frame(DecodedTexture {
                            texture,
                            index: f.data[1] as usize as u32,
                            width: f.width as u32,
                            height: f.height as u32,
                            tag: f.pts as u32,
                            decoded_at_us: pingpong_proto::clock::now_us(),
                        });
                    }
                } else {
                    tracing::warn!(format = f.format, "a picture not on the GPU; dropped");
                }
                ff::av_frame_unref(self.frame);
            }
        }
    }
}

impl Drop for D3d11Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_frame_free(&mut self.frame);
            ff::av_packet_free(&mut self.packet);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}
