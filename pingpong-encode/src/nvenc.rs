//! NVENC on a D3D11 device, configured as Apollo configures it.
//!
//! The runtime comes from `nvEncodeAPI64.dll`, installed with the driver, and is
//! loaded on first use -- as Apollo does -- so the build needs no SDK.
//!
//! Settings, each Apollo's (`src/nvenc/nvenc_base.cpp`):
//! - P1 preset (configurable), ultra-low-latency tuning, no B-frames, no
//!   lookahead, zero reorder delay.
//! - CBR with a **single-frame VBV** (`bitrate / fps`): no frame may be larger
//!   than one frame's share of the bitrate, so an IDR never becomes a burst the
//!   network has to absorb.
//! - Two-pass at quarter resolution for better bit allocation at no latency.
//! - Infinite GOP, IDR only on request, SPS/PPS repeated on every IDR.
//! - Five frames in the DPB with one active reference: after a loss the encoder
//!   can be told to stop referencing the lost frames and predict from an older
//!   one the client still has (reference-frame invalidation), which recovers
//!   with a P-frame instead of an IDR.
//! - A VUI colour description matching `convert.rs`: BT.709, limited range,
//!   chroma sited left; for HDR, BT.2020 primaries and matrix and the PQ
//!   curve (HEVC Main10, AV1 10-bit, from P010), as Sunshine sends HDR.
//! - 4:4:4 from a packed AYUV texture (`chromaFormatIDC` 3), as Sunshine's
//!   D3D11 path feeds it; 10-bit 4:4:4 is not possible from D3D11.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{s, Interface};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

use crate::nvenc_sys::*;
use crate::{Codec, EncodeError, EncodedFrame, EncoderConfig, FrameKind};

/// Frames kept in the DPB. Apollo's default for H.264/HEVC.
const DPB_FRAMES: u32 = 5;

struct Api(NV_ENCODE_API_FUNCTION_LIST);
// SAFETY: a table of function pointers into a DLL that is never unloaded.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static NV_ENCODE_API_FUNCTION_LIST, EncodeError> {
    let loaded = API.get_or_init(|| unsafe {
        let lib = LoadLibraryA(s!("nvEncodeAPI64.dll"))
            .map_err(|e| format!("nvEncodeAPI64.dll not loadable (no NVIDIA driver?): {e}"))?;
        let max_version = GetProcAddress(lib, s!("NvEncodeAPIGetMaxSupportedVersion"))
            .ok_or("NvEncodeAPIGetMaxSupportedVersion missing")?;
        let create = GetProcAddress(lib, s!("NvEncodeAPICreateInstance"))
            .ok_or("NvEncodeAPICreateInstance missing")?;
        let max_version: unsafe extern "C" fn(*mut u32) -> NVENCSTATUS =
            std::mem::transmute(max_version);
        let create: unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS =
            std::mem::transmute(create);

        let mut supported = 0u32;
        let status = max_version(&mut supported);
        let wanted = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
        if status != NVENCSTATUS::NV_ENC_SUCCESS || supported < wanted {
            return Err(format!(
                "driver supports NVENC API {}.{}, need {}.{} -- update the NVIDIA driver",
                supported >> 4,
                supported & 0xF,
                NVENCAPI_MAJOR_VERSION,
                NVENCAPI_MINOR_VERSION
            ));
        }

        let mut list = NV_ENCODE_API_FUNCTION_LIST {
            version: NV_ENCODE_API_FUNCTION_LIST_VER,
            ..Default::default()
        };
        let status = create(&mut list);
        if status != NVENCSTATUS::NV_ENC_SUCCESS {
            return Err(format!("NvEncodeAPICreateInstance: {status:?}"));
        }
        Ok(Api(list))
    });
    match loaded {
        Ok(api) => Ok(&api.0),
        Err(e) => Err(EncodeError::Nvenc(e.clone())),
    }
}

macro_rules! call {
    ($api:expr, $f:ident ( $($arg:expr),* )) => {{
        let f = $api.$f.ok_or_else(|| EncodeError::Nvenc(concat!(stringify!($f), " missing").into()))?;
        unsafe { f($($arg),*) }
    }};
}

fn codec_guid(codec: Codec) -> GUID {
    match codec {
        Codec::H264 => NV_ENC_CODEC_H264_GUID,
        Codec::Hevc => NV_ENC_CODEC_HEVC_GUID,
        Codec::Av1 => NV_ENC_CODEC_AV1_GUID,
    }
}

fn preset_guid(preset: u8) -> GUID {
    match preset {
        0 | 1 => NV_ENC_PRESET_P1_GUID,
        2 => NV_ENC_PRESET_P2_GUID,
        3 => NV_ENC_PRESET_P3_GUID,
        4 => NV_ENC_PRESET_P4_GUID,
        5 => NV_ENC_PRESET_P5_GUID,
        6 => NV_ENC_PRESET_P6_GUID,
        _ => NV_ENC_PRESET_P7_GUID,
    }
}

fn guid_eq(a: &GUID, b: &GUID) -> bool {
    a.Data1 == b.Data1 && a.Data2 == b.Data2 && a.Data3 == b.Data3 && a.Data4 == b.Data4
}

/// What this GPU's encoder can do with a codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecCaps {
    pub codec: Codec,
    /// 10-bit (Main10 for HEVC): HDR.
    pub ten_bit: bool,
    pub yuv444: bool,
}

/// Which codecs this GPU can encode. Opens and closes a throwaway session.
pub fn supported_codecs(device: &ID3D11Device) -> Result<Vec<Codec>, EncodeError> {
    Ok(capabilities(device)?.into_iter().map(|c| c.codec).collect())
}

/// Which codecs this GPU can encode, and how. Opens and closes a throwaway
/// session.
pub fn capabilities(device: &ID3D11Device) -> Result<Vec<CodecCaps>, EncodeError> {
    let api = api()?;
    let enc = open_session(api, device)?;
    let result = (|| {
        let mut count = 0u32;
        check(
            api,
            enc,
            call!(api, nvEncGetEncodeGUIDCount(enc, &mut count)),
            "nvEncGetEncodeGUIDCount",
        )?;
        let mut guids = vec![GUID::default(); count as usize];
        let mut got = 0u32;
        check(
            api,
            enc,
            call!(
                api,
                nvEncGetEncodeGUIDs(enc, guids.as_mut_ptr(), count, &mut got)
            ),
            "nvEncGetEncodeGUIDs",
        )?;
        guids.truncate(got as usize);
        Ok([Codec::H264, Codec::Hevc, Codec::Av1]
            .into_iter()
            .filter(|c| guids.iter().any(|g| guid_eq(g, &codec_guid(*c))))
            .map(|codec| CodecCaps {
                codec,
                ten_bit: codec != Codec::H264
                    && cap_of(
                        api,
                        enc,
                        codec,
                        NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE,
                    ) != 0,
                yuv444: cap_of(
                    api,
                    enc,
                    codec,
                    NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
                ) != 0,
            })
            .collect())
    })();
    let _ = call!(api, nvEncDestroyEncoder(enc));
    result
}

fn cap_of(
    api: &NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    codec: Codec,
    cap: NV_ENC_CAPS,
) -> i32 {
    let mut param = NV_ENC_CAPS_PARAM {
        version: NV_ENC_CAPS_PARAM_VER,
        capsToQuery: cap,
        ..Default::default()
    };
    let mut value = 0i32;
    let Some(f) = api.nvEncGetEncodeCaps else {
        return 0;
    };
    // SAFETY: a live session; the out-pointers are locals.
    let status = unsafe { f(enc, codec_guid(codec), &mut param, &mut value) };
    if status == NVENCSTATUS::NV_ENC_SUCCESS {
        value
    } else {
        0
    }
}

fn open_session(
    api: &NV_ENCODE_API_FUNCTION_LIST,
    device: &ID3D11Device,
) -> Result<*mut c_void, EncodeError> {
    let mut params = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
        version: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
        deviceType: NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
        device: device.as_raw(),
        apiVersion: NVENCAPI_VERSION,
        ..Default::default()
    };
    let mut enc: *mut c_void = std::ptr::null_mut();
    let status = call!(api, nvEncOpenEncodeSessionEx(&mut params, &mut enc));
    if status != NVENCSTATUS::NV_ENC_SUCCESS {
        if !enc.is_null() {
            let _ = call!(api, nvEncDestroyEncoder(enc));
        }
        return Err(EncodeError::Nvenc(format!(
            "nvEncOpenEncodeSessionEx: {status:?}"
        )));
    }
    Ok(enc)
}

fn last_error(api: &NV_ENCODE_API_FUNCTION_LIST, enc: *mut c_void) -> String {
    let Some(f) = api.nvEncGetLastErrorString else {
        return String::new();
    };
    let p = unsafe { f(enc) };
    if p.is_null() {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

fn check(
    api: &NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    status: NVENCSTATUS,
    what: &str,
) -> Result<(), EncodeError> {
    if status == NVENCSTATUS::NV_ENC_SUCCESS {
        Ok(())
    } else {
        Err(EncodeError::Nvenc(format!(
            "{what}: {status:?} {}",
            last_error(api, enc)
        )))
    }
}

pub struct NvencEncoder {
    api: &'static NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
    bitstream: *mut c_void,
    registered: *mut c_void,
    /// Kept alive and pointed to by `init.encodeConfig` for reconfiguration.
    config: Box<NV_ENC_CONFIG>,
    init: Box<NV_ENC_INITIALIZE_PARAMS>,
    settings: EncoderConfig,
    rfi: bool,
    last_encoded: Option<u64>,
    last_rfi: Option<(u64, u64)>,
    rfi_pending: bool,
}

// SAFETY: the session and its buffers are owned here and only used through
// `&mut self` on one thread at a time; NVENC sessions have no thread affinity.
unsafe impl Send for NvencEncoder {}

impl NvencEncoder {
    /// Open an encoder on `device` that reads `input`, an NV12 texture of
    /// exactly `settings.width` × `settings.height` (see [`crate::convert`]).
    pub fn new(
        device: &ID3D11Device,
        input: &ID3D11Texture2D,
        settings: EncoderConfig,
    ) -> Result<NvencEncoder, EncodeError> {
        let api = api()?;
        let enc = open_session(api, device)?;
        // From here on, Drop cleans up whatever has been created.
        let mut this = NvencEncoder {
            api,
            enc,
            bitstream: std::ptr::null_mut(),
            registered: std::ptr::null_mut(),
            config: Box::new(NV_ENC_CONFIG::default()),
            init: Box::new(NV_ENC_INITIALIZE_PARAMS::default()),
            settings,
            rfi: false,
            last_encoded: None,
            last_rfi: None,
            rfi_pending: false,
        };
        this.initialize(input)?;
        Ok(this)
    }

    fn cap(&self, cap: NV_ENC_CAPS) -> i32 {
        cap_of(self.api, self.enc, self.settings.codec, cap)
    }

    /// The input texture's format: P010 for HDR, packed AYUV for 4:4:4,
    /// else NV12 (what `convert::Output::for_stream` writes).
    fn buffer_format(&self) -> NV_ENC_BUFFER_FORMAT {
        match (self.settings.hdr, self.settings.yuv444) {
            (true, _) => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
            (false, true) => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_AYUV,
            (false, false) => NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
        }
    }

    fn initialize(&mut self, input: &ID3D11Texture2D) -> Result<(), EncodeError> {
        let (api, enc) = (self.api, self.enc);
        let s = self.settings;
        let codec = codec_guid(s.codec);
        let preset = preset_guid(s.preset);

        let supported = supported_on(api, enc)?;
        if !supported.iter().any(|g| guid_eq(g, &codec)) {
            return Err(EncodeError::Unsupported(format!(
                "{} encoding on this GPU",
                s.codec.name()
            )));
        }
        if s.hdr
            && (s.codec == Codec::H264
                || self.cap(NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE) == 0)
        {
            return Err(EncodeError::Unsupported(format!(
                "10-bit {} encoding on this GPU",
                s.codec.name()
            )));
        }
        if s.yuv444 && self.cap(NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE) == 0 {
            return Err(EncodeError::Unsupported(format!(
                "4:4:4 {} encoding on this GPU",
                s.codec.name()
            )));
        }
        // Packed AYUV is 8-bit only: 10-bit 4:4:4 would need CUDA.
        let yuv444 = s.yuv444 && !s.hdr;

        let mut preset_config = NV_ENC_PRESET_CONFIG {
            version: NV_ENC_PRESET_CONFIG_VER,
            presetCfg: NV_ENC_CONFIG {
                version: NV_ENC_CONFIG_VER,
                ..Default::default()
            },
            ..Default::default()
        };
        check(
            api,
            enc,
            call!(
                api,
                nvEncGetEncodePresetConfigEx(
                    enc,
                    codec,
                    preset,
                    NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                    &mut preset_config
                )
            ),
            "nvEncGetEncodePresetConfigEx",
        )?;

        let mut cfg = preset_config.presetCfg;
        cfg.version = NV_ENC_CONFIG_VER;
        cfg.profileGUID = NV_ENC_CODEC_PROFILE_AUTOSELECT_GUID;
        cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
        cfg.frameIntervalP = 1;
        cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        cfg.rcParams.set_zeroReorderDelay(1);
        cfg.rcParams.set_enableLookahead(0);
        cfg.rcParams.lowDelayKeyFrameScale = 1;
        cfg.rcParams.multiPass = if s.two_pass {
            NV_ENC_MULTI_PASS::NV_ENC_TWO_PASS_QUARTER_RESOLUTION
        } else {
            NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED
        };
        cfg.rcParams.set_enableAQ(0);
        cfg.rcParams.averageBitRate = s.bitrate_bps;
        cfg.rcParams.maxBitRate = s.bitrate_bps;
        if self.cap(NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE) != 0 {
            cfg.rcParams.vbvBufferSize = s.frame_bits(s.bitrate_bps);
            cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
        }

        let multi_ref = self.cap(NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES) != 0;
        let dpb = if multi_ref { DPB_FRAMES } else { 1 };
        self.rfi =
            multi_ref && self.cap(NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION) != 0;
        let slices = s.slices.max(1);

        // SAFETY (all three arms): writing a union variant; the preset was
        // fetched for this codec, so this is also the initialised variant.
        unsafe {
            match s.codec {
                Codec::H264 => {
                    cfg.profileGUID = if yuv444 {
                        NV_ENC_H264_PROFILE_HIGH_444_GUID
                    } else {
                        NV_ENC_H264_PROFILE_HIGH_GUID
                    };
                    let h = &mut cfg.encodeCodecConfig.h264Config;
                    if yuv444 {
                        h.chromaFormatIDC = 3;
                    }
                    h.set_repeatSPSPPS(1);
                    h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    h.sliceMode = 3;
                    h.sliceModeData = slices;
                    h.entropyCodingMode =
                        NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
                    h.maxNumRefFrames = dpb;
                    h.numRefL0 = NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
                    fill_vui(&mut h.h264VUIParameters, false, yuv444);
                }
                Codec::Hevc => {
                    let h = &mut cfg.encodeCodecConfig.hevcConfig;
                    h.set_repeatSPSPPS(1);
                    h.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    h.sliceMode = 3;
                    h.sliceModeData = slices;
                    h.maxNumRefFramesInDPB = dpb;
                    h.numRefL0 = NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
                    if yuv444 {
                        h.set_chromaFormatIDC(3);
                    }
                    if s.hdr {
                        h.set_pixelBitDepthMinus8(2);
                    }
                    fill_vui(&mut h.hevcVUIParameters, s.hdr, yuv444);
                }
                Codec::Av1 => {
                    let a = &mut cfg.encodeCodecConfig.av1Config;
                    a.set_repeatSeqHdr(1);
                    a.idrPeriod = NVENC_INFINITE_GOPLENGTH;
                    let (primaries, transfer, matrix) = colour(s.hdr);
                    a.colorPrimaries = primaries;
                    a.transferCharacteristics = transfer;
                    a.matrixCoefficients = matrix;
                    a.colorRange = 0;
                    a.chromaSamplePosition = if yuv444 { 0 } else { 1 };
                    if yuv444 {
                        a.set_chromaFormatIDC(3);
                    }
                    if s.hdr {
                        a.set_inputPixelBitDepthMinus8(2);
                        a.set_pixelBitDepthMinus8(2);
                    }
                    a.maxNumRefFramesInDPB = if multi_ref { 8 } else { 1 };
                    a.numFwdRefs = NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;
                }
            }
        }
        *self.config = cfg;

        *self.init = NV_ENC_INITIALIZE_PARAMS {
            version: NV_ENC_INITIALIZE_PARAMS_VER,
            encodeGUID: codec,
            presetGUID: preset,
            encodeWidth: s.width,
            encodeHeight: s.height,
            darWidth: s.width,
            darHeight: s.height,
            maxEncodeWidth: s.width,
            maxEncodeHeight: s.height,
            frameRateNum: s.fps_mhz.max(1),
            frameRateDen: 1000,
            // Picture-type decision on: NVENC picks I/P itself and a keyframe is
            // requested per picture with FORCEIDR, which also keeps it marking
            // references correctly (with PTD off, refPicFlag had to be set by
            // hand).
            enablePTD: 1,
            tuningInfo: NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
            encodeConfig: &mut *self.config,
            ..Default::default()
        };
        check(
            api,
            enc,
            call!(api, nvEncInitializeEncoder(enc, &mut *self.init)),
            "nvEncInitializeEncoder",
        )?;

        let mut create = NV_ENC_CREATE_BITSTREAM_BUFFER {
            version: NV_ENC_CREATE_BITSTREAM_BUFFER_VER,
            ..Default::default()
        };
        check(
            api,
            enc,
            call!(api, nvEncCreateBitstreamBuffer(enc, &mut create)),
            "nvEncCreateBitstreamBuffer",
        )?;
        self.bitstream = create.bitstreamBuffer;

        let mut reg = NV_ENC_REGISTER_RESOURCE {
            version: NV_ENC_REGISTER_RESOURCE_VER,
            resourceType: NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
            width: s.width,
            height: s.height,
            pitch: 0,
            subResourceIndex: 0,
            resourceToRegister: input.as_raw(),
            bufferFormat: self.buffer_format(),
            bufferUsage: NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
            ..Default::default()
        };
        check(
            api,
            enc,
            call!(api, nvEncRegisterResource(enc, &mut reg)),
            "nvEncRegisterResource",
        )?;
        self.registered = reg.registeredResource;

        tracing::info!(
            codec = s.codec.name(),
            width = s.width,
            height = s.height,
            fps = s.fps_mhz as f64 / 1000.0,
            bitrate_mbps = s.bitrate_bps as f64 / 1e6,
            preset = s.preset.max(1),
            two_pass = s.two_pass,
            hdr = s.hdr,
            yuv444,
            slices,
            rfi = self.rfi,
            dpb,
            "NVENC encoder ready"
        );
        Ok(())
    }

    pub fn settings(&self) -> EncoderConfig {
        self.settings
    }

    /// Whether this encoder can recover from loss with a P-frame.
    pub fn supports_rfi(&self) -> bool {
        self.rfi
    }

    /// Encode whatever is in the input texture now, as frame `index`.
    /// Indices must increase by one per call.
    pub fn encode(&mut self, index: u64, force_idr: bool) -> Result<EncodedFrame, EncodeError> {
        let (api, enc) = (self.api, self.enc);

        let mut map = NV_ENC_MAP_INPUT_RESOURCE {
            version: NV_ENC_MAP_INPUT_RESOURCE_VER,
            registeredResource: self.registered,
            ..Default::default()
        };
        check(
            api,
            enc,
            call!(api, nvEncMapInputResource(enc, &mut map)),
            "nvEncMapInputResource",
        )?;

        let result = (|| {
            let mut pic = NV_ENC_PIC_PARAMS {
                version: NV_ENC_PIC_PARAMS_VER,
                inputWidth: self.settings.width,
                inputHeight: self.settings.height,
                encodePicFlags: if force_idr {
                    NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32
                        | NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32
                } else {
                    0
                },
                inputTimeStamp: index,
                pictureStruct: NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME,
                inputBuffer: map.mappedResource,
                bufferFmt: map.mappedBufferFmt,
                outputBitstream: self.bitstream,
                ..Default::default()
            };
            check(
                api,
                enc,
                call!(api, nvEncEncodePicture(enc, &mut pic)),
                "nvEncEncodePicture",
            )?;

            let mut lock = NV_ENC_LOCK_BITSTREAM {
                version: NV_ENC_LOCK_BITSTREAM_VER,
                outputBitstream: self.bitstream,
                ..Default::default()
            };
            check(
                api,
                enc,
                call!(api, nvEncLockBitstream(enc, &mut lock)),
                "nvEncLockBitstream",
            )?;
            // SAFETY: valid between lock and unlock.
            let data = unsafe {
                std::slice::from_raw_parts(
                    lock.bitstreamBufferPtr as *const u8,
                    lock.bitstreamSizeInBytes as usize,
                )
            }
            .to_vec();
            let idr = matches!(
                lock.pictureType,
                NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
            );
            let _ = call!(api, nvEncUnlockBitstream(enc, self.bitstream));
            Ok((data, idr))
        })();

        let _ = call!(api, nvEncUnmapInputResource(enc, map.mappedResource));
        let (data, idr) = result?;

        let kind = if idr {
            FrameKind::Idr
        } else if self.rfi_pending {
            FrameKind::Recovery
        } else {
            FrameKind::P
        };
        self.rfi_pending = false;
        if idr {
            self.last_rfi = None;
        }
        self.last_encoded = Some(index);
        Ok(EncodedFrame { data, kind, index })
    }

    /// Stop predicting from frames `first..=last` (as reported lost by the
    /// client). Returns `false` when that cannot recover the stream -- the loss
    /// reaches past the DPB, or RFI is unsupported -- and an IDR is needed.
    ///
    /// Apollo's rules: the range is extended to the last encoded frame (the
    /// client could not decode anything after the loss either), a range
    /// already invalidated is not redone, and the next frame out is marked as
    /// the recovery point.
    pub fn invalidate(&mut self, first: u64, last: u64) -> bool {
        if !self.rfi || last < first {
            return false;
        }
        let Some(newest) = self.last_encoded else {
            return false;
        };
        if let Some((a, b)) = self.last_rfi {
            if first >= a && last <= b {
                return true;
            }
        }
        let last = newest.max(last);
        if last - first + 1 >= DPB_FRAMES as u64 {
            return false;
        }
        for ts in first..=last {
            let Some(f) = self.api.nvEncInvalidateRefFrames else {
                return false;
            };
            if unsafe { f(self.enc, ts) } != NVENCSTATUS::NV_ENC_SUCCESS {
                return false;
            }
        }
        self.last_rfi = Some((first, last));
        self.rfi_pending = true;
        true
    }

    /// Change the bitrate without an IDR (adaptive bitrate).
    pub fn set_bitrate(&mut self, bitrate_bps: u32) -> Result<(), EncodeError> {
        if bitrate_bps == self.settings.bitrate_bps {
            return Ok(());
        }
        self.config.rcParams.averageBitRate = bitrate_bps;
        self.config.rcParams.maxBitRate = bitrate_bps;
        if self.config.rcParams.vbvBufferSize != 0 {
            self.config.rcParams.vbvBufferSize = self.settings.frame_bits(bitrate_bps);
            self.config.rcParams.vbvInitialDelay = self.config.rcParams.vbvBufferSize;
        }
        self.init.encodeConfig = &mut *self.config;
        let mut params = NV_ENC_RECONFIGURE_PARAMS {
            version: NV_ENC_RECONFIGURE_PARAMS_VER,
            reInitEncodeParams: *self.init,
            ..Default::default()
        };
        params.set_resetEncoder(0);
        params.set_forceIDR(0);
        check(
            self.api,
            self.enc,
            call!(self.api, nvEncReconfigureEncoder(self.enc, &mut params)),
            "nvEncReconfigureEncoder",
        )?;
        self.settings.bitrate_bps = bitrate_bps;
        Ok(())
    }
}

fn supported_on(
    api: &NV_ENCODE_API_FUNCTION_LIST,
    enc: *mut c_void,
) -> Result<Vec<GUID>, EncodeError> {
    let mut count = 0u32;
    check(
        api,
        enc,
        call!(api, nvEncGetEncodeGUIDCount(enc, &mut count)),
        "nvEncGetEncodeGUIDCount",
    )?;
    let mut guids = vec![GUID::default(); count as usize];
    let mut got = 0u32;
    check(
        api,
        enc,
        call!(
            api,
            nvEncGetEncodeGUIDs(enc, guids.as_mut_ptr(), count, &mut got)
        ),
        "nvEncGetEncodeGUIDs",
    )?;
    guids.truncate(got as usize);
    Ok(guids)
}

/// The colour description: BT.709 for SDR; BT.2020 and PQ for HDR.
fn colour(
    hdr: bool,
) -> (
    NV_ENC_VUI_COLOR_PRIMARIES,
    NV_ENC_VUI_TRANSFER_CHARACTERISTIC,
    NV_ENC_VUI_MATRIX_COEFFS,
) {
    if hdr {
        (
            NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT2020,
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084,
            NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL,
        )
    } else {
        (
            NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709,
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709,
            NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709,
        )
    }
}

fn fill_vui(vui: &mut NV_ENC_CONFIG_H264_VUI_PARAMETERS, hdr: bool, yuv444: bool) {
    let (primaries, transfer, matrix) = colour(hdr);
    vui.videoSignalTypePresentFlag = 1;
    vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    vui.videoFullRangeFlag = 0;
    vui.colourDescriptionPresentFlag = 1;
    vui.colourPrimaries = primaries;
    vui.transferCharacteristics = transfer;
    vui.colourMatrix = matrix;
    // Chroma sample location type 0 (left), matching the converter; none
    // to say at 4:4:4.
    vui.chromaSampleLocationFlag = if yuv444 { 0 } else { 1 };
    vui.chromaSampleLocationTop = 0;
    vui.chromaSampleLocationBot = 0;
}

impl Drop for NvencEncoder {
    fn drop(&mut self) {
        let api = self.api;
        unsafe {
            if !self.registered.is_null() {
                if let Some(f) = api.nvEncUnregisterResource {
                    let _ = f(self.enc, self.registered);
                }
            }
            if !self.bitstream.is_null() {
                if let Some(f) = api.nvEncDestroyBitstreamBuffer {
                    let _ = f(self.enc, self.bitstream);
                }
            }
            if let Some(f) = api.nvEncDestroyEncoder {
                let _ = f(self.enc);
            }
        }
    }
}
