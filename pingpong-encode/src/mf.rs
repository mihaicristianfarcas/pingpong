//! Encoding on Windows without NVENC: Media Foundation's H.264 encoder
//! (`CLSID_MSH264EncoderMFT`, in software), so a host with no NVIDIA GPU -- a
//! virtual machine, a Windows on Arm PC, an AMD or Intel GPU -- still streams.
//! Sunshine's answer for such a host is libx264, tuned for zero latency
//! (`video.cpp`); this encoder comes with Windows, so Pong ships nothing more.
//!
//! It reads the converter's NV12 texture (`convert.rs`, the colour math of the
//! NVENC path) through a CPU-readable copy, and is set up the way Sunshine
//! sets up its encoders for streaming: the fastest speed, no B-frames, CBR
//! with a single-frame buffer, low-latency mode (each frame comes out of the
//! call that put it in), IDR only on request, SPS and PPS with every IDR.
//!
//! What it cannot do: invalidate reference frames (a loss costs an IDR, as on
//! the Linux host's FFmpeg path), HEVC, AV1, 10-bit and 4:4:4. A host on this
//! encoder offers H.264 4:2:0 only. The GPU vendors' hardware encoders under
//! Media Foundation (AMD, Intel, Qualcomm) are asynchronous transforms, driven
//! by events; they are not used here.

use std::mem::ManuallyDrop;

use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::{VARIANT, VT_BOOL, VT_UI4};

use crate::h264::{parameter_sets, Units};
use crate::{Codec, EncodeError, EncodedFrame, EncoderConfig, FrameKind};

/// The quality-for-speed trade (0 fastest .. 100 best). The fastest, as
/// Sunshine picks NVENC's fastest preset (P1) by default: a software encoder
/// on a host without a GPU's is short of time before it is short of bits.
const QUALITY_VS_SPEED: u32 = 0;

/// Frames between IDRs the encoder would make on its own. Its largest: IDRs
/// come when the client asks (a loss, a new session), as with NVENC's
/// endless GOP.
const GOP_FRAMES: u32 = u32::MAX;

fn mf(what: &'static str) -> impl Fn(windows::core::Error) -> EncodeError {
    move |e| EncodeError::MediaFoundation(format!("{what}: {e}"))
}

/// Whether Media Foundation's H.264 encoder can be created here (not on the
/// N editions of Windows without the Media Feature Pack).
pub fn h264_available() -> bool {
    let _com = Com::init();
    // SAFETY: COM is initialised on this thread for the call.
    let encoder = unsafe {
        CoCreateInstance::<_, IMFTransform>(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
    };
    // A local, not the tail expression: a temporary there would outlive
    // `_com`, and the encoder would be released after CoUninitialize, which
    // crashed the host.
    encoder.is_ok()
}

/// COM on the calling thread, uninitialised again when dropped (if it was
/// this that initialised it).
struct Com(bool);

impl Com {
    fn init() -> Com {
        // SAFETY: plain initialisation; S_FALSE (already initialised) is
        // balanced by CoUninitialize too, RPC_E_CHANGED_MODE is not.
        Com(unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
    }
}

pub struct MfEncoder {
    mft: IMFTransform,
    codec_api: Option<ICodecAPI>,
    context: ID3D11DeviceContext,
    /// The converter's output, NV12 at the stream's size.
    input: ID3D11Texture2D,
    /// Where the GPU copies `input` for the CPU to read.
    staging: ID3D11Texture2D,
    /// The input sample, kept while the encoder is known to be done with it.
    sample_in: Option<IMFSample>,
    /// The sample the encoder writes into (Microsoft's encoder does not
    /// allocate its own); `None` when it does.
    sample_out: Option<(IMFSample, IMFMediaBuffer)>,
    settings: EncoderConfig,
    /// The last SPS and PPS seen, with start codes: put in front of an IDR
    /// that comes without them.
    parameter_sets: Option<Vec<u8>>,
    _mf: MfStarted,
    _com: Com,
}

/// Media Foundation started (its start-up is counted), shut down on drop.
struct MfStarted;

impl MfStarted {
    fn start() -> Result<MfStarted, EncodeError> {
        // SAFETY: plain call; balanced by MFShutdown in Drop.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) }.map_err(mf("MFStartup"))?;
        Ok(MfStarted)
    }
}

impl Drop for MfStarted {
    fn drop(&mut self) {
        // SAFETY: balances MFStartup.
        let _ = unsafe { MFShutdown() };
    }
}

impl MfEncoder {
    /// An encoder reading `input`, an NV12 texture of exactly
    /// `settings.width` × `settings.height` (see [`crate::convert`]), on the
    /// calling thread, which must be the one that encodes.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        input: &ID3D11Texture2D,
        settings: EncoderConfig,
    ) -> Result<MfEncoder, EncodeError> {
        if settings.codec != Codec::H264 || settings.hdr || settings.yuv444 {
            return Err(EncodeError::Unsupported(format!(
                "Media Foundation encodes 8-bit 4:2:0 H.264 only, not {}{}{}",
                settings.codec.name(),
                if settings.hdr { " HDR" } else { "" },
                if settings.yuv444 { " 4:4:4" } else { "" },
            )));
        }
        let com = Com::init();
        let started = MfStarted::start()?;
        // SAFETY: COM is initialised on this thread.
        let mft: IMFTransform =
            unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }
                .map_err(mf("the H.264 encoder"))?;
        let codec_api = mft.cast::<ICodecAPI>().ok();
        if let Some(api) = &codec_api {
            // Before the media types: the encoder fixes some of these then.
            set(
                api,
                "rate control",
                &CODECAPI_AVEncCommonRateControlMode,
                ui4(eAVEncCommonRateControlMode_CBR.0 as u32),
            );
            set(
                api,
                "bitrate",
                &CODECAPI_AVEncCommonMeanBitRate,
                ui4(settings.bitrate_bps),
            );
            set(
                api,
                "buffer size",
                &CODECAPI_AVEncCommonBufferSize,
                ui4(settings.frame_bits(settings.bitrate_bps)),
            );
            set(
                api,
                "low latency",
                &CODECAPI_AVLowLatencyMode,
                boolean(true),
            );
            set(
                api,
                "B-frames",
                &CODECAPI_AVEncMPVDefaultBPictureCount,
                ui4(0),
            );
            set(api, "GOP", &CODECAPI_AVEncMPVGOPSize, ui4(GOP_FRAMES));
            set(
                api,
                "speed",
                &CODECAPI_AVEncCommonQualityVsSpeed,
                ui4(QUALITY_VS_SPEED),
            );
        } else {
            tracing::warn!("the H.264 encoder has no ICodecAPI: its own rate control and GOP");
        }

        let (w, h) = (settings.width, settings.height);
        let out_type = video_type(&MFVideoFormat_H264, &settings)?;
        // SAFETY: a live media type and transform throughout.
        unsafe {
            out_type
                .SetUINT32(&MF_MT_AVG_BITRATE, settings.bitrate_bps)
                .map_err(mf("bitrate"))?;
            out_type
                .SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)
                .map_err(mf("profile"))?;
            mft.SetOutputType(0, &out_type, 0)
                .map_err(mf("SetOutputType"))?;
            let in_type = video_type(&MFVideoFormat_NV12, &settings)?;
            in_type
                .SetUINT32(&MF_MT_DEFAULT_STRIDE, w)
                .map_err(mf("stride"))?;
            mft.SetInputType(0, &in_type, 0)
                .map_err(mf("SetInputType"))?;
        }

        // SAFETY: as above.
        let info = unsafe { mft.GetOutputStreamInfo(0) }.map_err(mf("GetOutputStreamInfo"))?;
        let sample_out = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0 {
            None
        } else {
            // An IDR at the start can exceed one frame's share: room for a
            // whole uncompressed frame and then some.
            Some(new_sample(info.cbSize.max(w * h * 2))?)
        };

        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging = None;
        // SAFETY: a valid descriptor; the out-pointer is a local.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .map_err(|e| EncodeError::D3d(format!("CreateTexture2D(staging): {e}")))?;
        let staging = staging.expect("CreateTexture2D succeeded");

        let parameter_sets = sequence_header(&mft);
        // SAFETY: types are set; the transform is ready to stream.
        unsafe {
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(mf("begin streaming"))?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(mf("start of stream"))?;
        }
        tracing::info!(
            width = w,
            height = h,
            fps = settings.fps(),
            mbps = settings.bitrate_bps / 1_000_000,
            "Media Foundation H.264 encoder (software)"
        );
        Ok(MfEncoder {
            mft,
            codec_api,
            context: context.clone(),
            input: input.clone(),
            staging,
            sample_in: None,
            sample_out,
            settings,
            parameter_sets,
            _mf: started,
            _com: com,
        })
    }

    /// Encode whatever is in the input texture now, as frame `index`.
    /// `None` when the encoder gave nothing back for it.
    pub fn encode(
        &mut self,
        index: u64,
        force_idr: bool,
    ) -> Result<Option<EncodedFrame>, EncodeError> {
        let sample = self.read_input(index)?;
        if force_idr {
            if let Some(api) = &self.codec_api {
                set(api, "key frame", &CODECAPI_AVEncVideoForceKeyFrame, ui4(1));
            }
        }
        // SAFETY: a live transform and sample.
        let mut fed = unsafe { self.mft.ProcessInput(0, &sample, 0) };
        if matches!(&fed, Err(e) if e.code() == MF_E_NOTACCEPTING) {
            // Output still waiting from before: take it, then feed again.
            self.drain()?;
            // SAFETY: as above.
            fed = unsafe { self.mft.ProcessInput(0, &sample, 0) };
        }
        fed.map_err(mf("ProcessInput"))?;
        let data = self.drain()?;
        // The encoder is done with the input once its frame is out.
        self.sample_in = data.as_ref().map(|_| sample);
        let Some(mut data) = data else {
            return Ok(None);
        };

        let units = Units::summary(&data);
        if units.sps {
            self.parameter_sets = Some(parameter_sets(&data));
        } else if units.idr {
            if let Some(ps) = &self.parameter_sets {
                data.splice(0..0, ps.iter().copied());
            }
        }
        let kind = if units.idr {
            FrameKind::Idr
        } else {
            FrameKind::P
        };
        Ok(Some(EncodedFrame { data, kind, index }))
    }

    /// Reference-frame invalidation: not something this encoder does, so a
    /// loss always takes an IDR.
    pub fn invalidate(&mut self, _first: u64, _last: u64) -> bool {
        false
    }

    /// Change the bitrate without an IDR (adaptive bitrate).
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        if bitrate_bps == self.settings.bitrate_bps {
            return Ok(());
        }
        let Some(api) = &self.codec_api else {
            return Err(EncodeError::Unsupported("bitrate change".into()));
        };
        // SAFETY: a live interface; the VARIANT is a plain UI4.
        unsafe { api.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &ui4(bitrate_bps)) }
            .map_err(mf("bitrate"))?;
        set(
            api,
            "buffer size",
            &CODECAPI_AVEncCommonBufferSize,
            ui4(self.settings.frame_bits(bitrate_bps)),
        );
        self.settings.bitrate_bps = bitrate_bps;
        Ok(())
    }

    /// The input texture, copied to the CPU, as a sample stamped `index`.
    fn read_input(&mut self, index: u64) -> Result<IMFSample, EncodeError> {
        let (w, h) = (self.settings.width as usize, self.settings.height as usize);
        let size = w * h * 3 / 2;
        let sample = match self.sample_in.take() {
            Some(s) => s,
            // A fresh sample when the encoder may still hold the last one
            // (it gave no frame back for it). Microsoft's encoder in
            // low-latency mode always does, so this is the first frame's.
            None => new_sample(size as u32)?.0,
        };
        // SAFETY: the sample holds the one buffer made in `new_sample`.
        let buffer = unsafe { sample.GetBufferByIndex(0) }.map_err(mf("GetBufferByIndex"))?;

        // SAFETY: both textures are NV12 at the same size on this context's
        // device.
        unsafe { self.context.CopyResource(&self.staging, &self.input) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: a staging texture with CPU read access, unmapped below.
        unsafe {
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }
        .map_err(|e| EncodeError::D3d(format!("Map(staging): {e}")))?;
        let mut dst: *mut u8 = std::ptr::null_mut();
        // SAFETY: the buffer was made `size` bytes long.
        let locked = unsafe { buffer.Lock(&mut dst, None, None) };
        if locked.is_ok() {
            let pitch = mapped.RowPitch as usize;
            // SAFETY: a mapped NV12 texture is `h` rows of luma, then `h / 2`
            // rows of interleaved chroma, each `pitch` bytes apart; `dst`
            // holds `w * h * 3 / 2` bytes. The rows are copied tightly.
            unsafe {
                let src = mapped.pData as *const u8;
                for row in 0..h + h / 2 {
                    std::ptr::copy_nonoverlapping(src.add(row * pitch), dst.add(row * w), w);
                }
                let _ = buffer.Unlock();
            }
        }
        // SAFETY: mapped above.
        unsafe { self.context.Unmap(&self.staging, 0) };
        locked.map_err(mf("Lock(input)"))?;

        let frame_100ns = 10_000_000_000i128 / self.settings.fps_mhz.max(1) as i128;
        // SAFETY: a live sample and buffer.
        unsafe {
            buffer
                .SetCurrentLength(size as u32)
                .map_err(mf("SetCurrentLength"))?;
            sample
                .SetSampleTime((index as i128 * frame_100ns) as i64)
                .map_err(mf("SetSampleTime"))?;
            sample
                .SetSampleDuration(frame_100ns as i64)
                .map_err(mf("SetSampleDuration"))?;
        }
        Ok(sample)
    }

    /// Everything the encoder has to give, until it wants input again.
    fn drain(&mut self) -> Result<Option<Vec<u8>>, EncodeError> {
        let mut out: Option<Vec<u8>> = None;
        loop {
            if let Some((_, buffer)) = &self.sample_out {
                // SAFETY: a live buffer.
                unsafe { buffer.SetCurrentLength(0) }.map_err(mf("SetCurrentLength(output)"))?;
            }
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(self.sample_out.as_ref().map(|(s, _)| s.clone())),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            // SAFETY: one output buffer for the one stream.
            let result = unsafe { self.mft.ProcessOutput(0, &mut buffers, &mut status) };
            // SAFETY: each is taken once and not touched again.
            let (sample, _events) = unsafe {
                (
                    ManuallyDrop::take(&mut buffers[0].pSample),
                    ManuallyDrop::take(&mut buffers[0].pEvents),
                )
            };
            match result {
                Ok(()) => {
                    let sample = sample.ok_or_else(|| {
                        EncodeError::MediaFoundation("ProcessOutput gave no sample".into())
                    })?;
                    append(&sample, out.get_or_insert_with(Vec::new))?;
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(out),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // SAFETY: a live transform; the type it offers is its own.
                    unsafe {
                        let t = self
                            .mft
                            .GetOutputAvailableType(0, 0)
                            .map_err(mf("GetOutputAvailableType"))?;
                        self.mft
                            .SetOutputType(0, &t, 0)
                            .map_err(mf("SetOutputType (stream change)"))?;
                    }
                }
                Err(e) => return Err(mf("ProcessOutput")(e)),
            }
        }
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // SAFETY: a live transform.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

/// A video media type of `subtype` at the stream's size and rate,
/// progressive, square pixels, BT.709 limited range (the converter's).
fn video_type(subtype: &GUID, s: &EncoderConfig) -> Result<IMFMediaType, EncodeError> {
    // SAFETY: a fresh media type, set attribute by attribute.
    unsafe {
        let t = MFCreateMediaType().map_err(mf("MFCreateMediaType"))?;
        let attrs: [(&GUID, u64, bool); 8] = [
            (
                &MF_MT_FRAME_SIZE,
                (s.width as u64) << 32 | s.height as u64,
                true,
            ),
            (&MF_MT_FRAME_RATE, (s.fps_mhz as u64) << 32 | 1000, true),
            (&MF_MT_PIXEL_ASPECT_RATIO, 1 << 32 | 1, true),
            (
                &MF_MT_INTERLACE_MODE,
                MFVideoInterlace_Progressive.0 as u64,
                false,
            ),
            (
                &MF_MT_YUV_MATRIX,
                MFVideoTransferMatrix_BT709.0 as u64,
                false,
            ),
            (
                &MF_MT_VIDEO_NOMINAL_RANGE,
                MFNominalRange_16_235.0 as u64,
                false,
            ),
            (
                &MF_MT_VIDEO_PRIMARIES,
                MFVideoPrimaries_BT709.0 as u64,
                false,
            ),
            (
                &MF_MT_TRANSFER_FUNCTION,
                MFVideoTransFunc_709.0 as u64,
                false,
            ),
        ];
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(mf("major type"))?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype).map_err(mf("subtype"))?;
        for (key, value, wide) in attrs {
            if wide {
                t.SetUINT64(key, value)
            } else {
                t.SetUINT32(key, value as u32)
            }
            .map_err(mf("media type"))?;
        }
        Ok(t)
    }
}

/// A sample holding one memory buffer of `size` bytes.
fn new_sample(size: u32) -> Result<(IMFSample, IMFMediaBuffer), EncodeError> {
    // SAFETY: fresh objects.
    unsafe {
        let sample = MFCreateSample().map_err(mf("MFCreateSample"))?;
        let buffer = MFCreateMemoryBuffer(size).map_err(mf("MFCreateMemoryBuffer"))?;
        sample.AddBuffer(&buffer).map_err(mf("AddBuffer"))?;
        Ok((sample, buffer))
    }
}

/// Append the bytes of `sample` to `out`.
fn append(sample: &IMFSample, out: &mut Vec<u8>) -> Result<(), EncodeError> {
    // SAFETY: the buffer is locked for the copy and unlocked after.
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(mf("ConvertToContiguousBuffer"))?;
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut data, None, Some(&mut len))
            .map_err(mf("Lock(output)"))?;
        out.extend_from_slice(std::slice::from_raw_parts(data, len as usize));
        let _ = buffer.Unlock();
    }
    Ok(())
}

/// The SPS and PPS the encoder put in its output type, if any.
fn sequence_header(mft: &IMFTransform) -> Option<Vec<u8>> {
    // SAFETY: a live transform; the blob is copied into a buffer of its size.
    unsafe {
        let t = mft.GetOutputCurrentType(0).ok()?;
        let size = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER).ok()?;
        let mut blob = vec![0u8; size as usize];
        t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None)
            .ok()?;
        let ps = parameter_sets(&blob);
        (!ps.is_empty()).then_some(ps)
    }
}

fn set(api: &ICodecAPI, what: &'static str, key: &GUID, value: VARIANT) {
    // SAFETY: a live interface; the VARIANT is a plain UI4 or BOOL.
    if let Err(e) = unsafe { api.SetValue(key, &value) } {
        tracing::warn!(error = %e, what, "the H.264 encoder did not take a setting");
    }
}

fn ui4(v: u32) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: the tag and the member it names are written together.
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = v;
    }
    var
}

fn boolean(v: bool) -> VARIANT {
    let mut var = VARIANT::default();
    // SAFETY: as in `ui4`.
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_BOOL;
        inner.Anonymous.boolVal = if v { VARIANT_TRUE } else { VARIANT_FALSE };
    }
    var
}
