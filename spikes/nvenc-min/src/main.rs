//! Spike: minimal NVENC H.264 encode to file, P1 + ultra-low-latency.
//!
//! Resolves risk #5 (plan Task 3). Confirms the crate's init path and the
//! latency-critical config from §8.3 before any of it is entangled with capture.
//!
//! Deliverables:
//!   1. a working Encoder init + config sequence Task 12 reuses
//!   2. the EXACT override field paths that took effect (struct layouts in this
//!      crate are version-sensitive, so Task 12 needs them verbatim)
//!   3. measured submit -> bitstream-available latency
//!   4. proof of ZERO B-frames (checked with ffprobe, see README)

use std::time::Instant;

use cudarc::driver::{CudaContext, DevicePtr};
use nvidia_video_codec_sdk::sys::nvEncodeAPI::{
    NVENC_INFINITE_GOPLENGTH, NV_ENC_BUFFER_FORMAT, NV_ENC_CODEC_H264_GUID,
    NV_ENC_INPUT_RESOURCE_TYPE, NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_TYPE, NV_ENC_PRESET_P1_GUID,
    NV_ENC_TUNING_INFO,
};
use nvidia_video_codec_sdk::{EncodePictureParams, Encoder, EncoderInitParams};

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FRAMES: usize = 120;
const BITRATE: u32 = 20_000_000;

/// A moving gradient in NV12 at an arbitrary row stride. Y is a diagonal ramp
/// offset by the frame number; chroma sweeps slowly so the output is obviously
/// animated by eye.
///
/// `pitch` is the row stride in bytes, which may exceed WIDTH -- see the
/// alignment note where the device buffer is allocated.
fn synth_nv12(frame: usize, pitch: usize) -> Vec<u8> {
    let w = WIDTH as usize;
    let h = HEIGHT as usize;
    let mut buf = vec![0u8; pitch * h + pitch * h / 2];

    for y in 0..h {
        for x in 0..w {
            buf[y * pitch + x] = ((x + y + frame * 4) % 256) as u8;
        }
    }
    // Interleaved U,V at half resolution, starting after the full luma plane.
    let uv_off = pitch * h;
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let o = uv_off + cy * pitch + cx * 2;
            buf[o] = ((cx * 2 + frame * 2) % 256) as u8;
            buf[o + 1] = ((cy * 2 + frame) % 256) as u8;
        }
    }
    buf
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ctx = CudaContext::new(0)?;
    let stream = ctx.default_stream();
    println!("CUDA context created on device 0");

    let encoder = Encoder::initialize_with_cuda(ctx.clone())?;
    println!("NVENC encoder initialized with the CUDA context");

    // --- assert support before configuring anything ---
    let codecs = encoder.get_encode_guids()?;
    assert!(
        codecs.contains(&NV_ENC_CODEC_H264_GUID),
        "H.264 encoding not supported by this GPU"
    );
    let presets = encoder.get_preset_guids(NV_ENC_CODEC_H264_GUID)?;
    assert!(
        presets.contains(&NV_ENC_PRESET_P1_GUID),
        "P1 preset not available"
    );
    let formats = encoder.get_supported_input_formats(NV_ENC_CODEC_H264_GUID)?;
    assert!(
        formats.contains(&NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12),
        "NV12 input format not supported"
    );
    println!("H.264 + P1 + NV12 all supported");

    // --- preset config, then the four latency-critical overrides (§8.3) ---
    let mut preset = encoder.get_preset_config(
        NV_ENC_CODEC_H264_GUID,
        NV_ENC_PRESET_P1_GUID,
        NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
    )?;
    let cfg = &mut preset.presetCfg;

    // 1. No B-frames. frameIntervalP == 1 means IPPP...; anything larger
    //    introduces reorder delay, which is encode latency we cannot recover.
    cfg.frameIntervalP = 1;
    // 2. Effectively infinite GOP: keyframes on demand only, never periodic.
    cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
    // 3. CBR, so bitrate does not spike into the network's queue.
    cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
    cfg.rcParams.averageBitRate = BITRATE;
    println!(
        "config overrides: frameIntervalP={} gopLength={:#x} rc=CBR bitrate={}",
        cfg.frameIntervalP, cfg.gopLength, cfg.rcParams.averageBitRate
    );

    let mut init = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, WIDTH, HEIGHT);
    init.preset_guid(NV_ENC_PRESET_P1_GUID)
        .tuning_info(NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY)
        .framerate(60, 1)
        // Required. Without PTD, NVENC demands an explicit picture type on every
        // submit and rejects NV_ENC_PIC_TYPE_UNKNOWN (which is what
        // EncodePictureParams::default() carries) with
        // `InvalidParam: "Invalid value for picture type."`.
        // With PTD the encoder picks I/P itself, which is what we want anyway:
        // an infinite GOP with keyframes only on demand.
        .enable_picture_type_decision()
        .encode_config(cfg);

    let session = encoder.start_session(NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12, init)?;
    println!("session started at {WIDTH}x{HEIGHT}");

    // NVENC registers a CUDA device pointer only if the CUDA context is current
    // on the calling thread. cudarc binds lazily, so make it explicit.
    ctx.bind_to_thread()?;

    // --- find a row stride NVENC will accept for a registered CUDA pointer ---
    //
    // Task 12 NEEDS this path: NV12 comes straight out of the conversion kernel,
    // and a host round trip would undo the whole point of §8. So probe rather
    // than assume, and probe several strides -- a failure at pitch == WIDTH says
    // nothing about whether the path works at all.
    //
    // NVENC's docs only require the pitch be a multiple of 4, but its own
    // samples allocate with cuMemAllocPitch, which pads to the device's texture
    // alignment (typically 256 or 512 B). 1920 is a multiple of 4 but not of
    // 256, which makes alignment the obvious suspect.
    let candidate_pitches: [u32; 4] = [WIDTH, 2048, 2560, 4096];
    let mut chosen: Option<(u32, cudarc::driver::CudaSlice<u8>)> = None;

    for pitch in candidate_pitches {
        let frame_len = (pitch * HEIGHT + pitch * HEIGHT / 2) as usize;
        let buf = stream.alloc_zeros::<u8>(frame_len)?;
        // Scoped so the SyncOnDrop guard is released before `buf` is moved.
        let ok = {
            let (raw_ptr, _sync) = buf.device_ptr(&stream);
            match session.register_generic_resource(
                (),
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                raw_ptr as *mut std::ffi::c_void,
                pitch,
            ) {
                Ok(r) => {
                    drop(r);
                    true
                }
                Err(e) => {
                    println!("[A] pitch {pitch:>5}: FAILED {:?}", e.kind());
                    false
                }
            }
        };
        if ok {
            println!("[A] pitch {pitch:>5}: SUCCESS");
            chosen = Some((pitch, buf));
            break;
        }
    }

    // --- [A2] the same thing, but allocated with plain cuMemAlloc ---
    //
    // THIS IS THE FINDING. cudarc's `alloc`/`alloc_zeros` call cuMemAllocAsync
    // whenever the device supports it (`CudaContext::has_async_alloc`, which is
    // pub(crate) with no opt-out). That returns STREAM-ORDERED memory from a
    // memory pool, and NVENC cannot register pool memory -- hence
    // ResourceRegisterFailed at every pitch above, which is nothing to do with
    // alignment.
    //
    // Allocating with cuMemAlloc (result::malloc_sync) instead gives ordinary
    // device memory that registers fine.
    let mut sync_alloc: Option<(u32, cudarc::driver::sys::CUdeviceptr)> = None;
    if chosen.is_none() {
        for pitch in candidate_pitches {
            let bytes = (pitch * HEIGHT + pitch * HEIGHT / 2) as usize;
            let dptr = unsafe { cudarc::driver::result::malloc_sync(bytes)? };
            let ok = match session.register_generic_resource(
                (),
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                dptr as *mut std::ffi::c_void,
                pitch,
            ) {
                Ok(r) => {
                    drop(r);
                    true
                }
                Err(e) => {
                    println!("[A2] cuMemAlloc pitch {pitch:>5}: FAILED {:?}", e.kind());
                    false
                }
            };
            if ok {
                println!("[A2] cuMemAlloc pitch {pitch:>5}: SUCCESS");
                sync_alloc = Some((pitch, dptr));
                break;
            }
            unsafe { cudarc::driver::result::free_sync(dptr)? };
        }
    }

    let cuda_path_works = chosen.is_some() || sync_alloc.is_some();
    if !cuda_path_works {
        println!("[A] no candidate registered on either allocator.");
        println!("    -> falling back to [B] a mapped NVENC input buffer (host upload)");
    }
    let (nv12_pitch, mut device_nv12, raw_nv12) = match (chosen, sync_alloc) {
        (Some((p, b)), _) => (p, Some(b), None),
        (None, Some((p, d))) => (p, None, Some(d)),
        (None, None) => (WIDTH, None, None),
    };

    let mut out = Vec::new();
    let mut latencies_us: Vec<u128> = Vec::with_capacity(FRAMES);
    let mut keyframes = 0usize;

    for i in 0..FRAMES {
        let host = synth_nv12(i, nv12_pitch as usize);
        let mut bitstream = session.create_output_bitstream()?;

        let (elapsed_us, pic_type, data) = if cuda_path_works {
            // Upload, then register and submit. The device pointer comes either
            // from cudarc (if its allocator happened to be registrable) or from
            // our own cuMemAlloc.
            let dptr = if let Some(buf) = device_nv12.as_mut() {
                stream.memcpy_htod(&host, buf)?;
                stream.synchronize()?;
                let (p, _sync) = buf.device_ptr(&stream);
                p
            } else {
                let d = raw_nv12.expect("one allocator path must be present");
                unsafe { cudarc::driver::result::memcpy_htod_sync(d, &host)? };
                d
            };

            let mut registered = session.register_generic_resource(
                (),
                NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                dptr as *mut std::ffi::c_void,
                nv12_pitch,
            )?;

            let t0 = Instant::now();
            session.encode_picture(
                &mut registered,
                &mut bitstream,
                EncodePictureParams {
                    input_timestamp: i as u64,
                    ..Default::default()
                },
            )?;
            let locked = bitstream.lock()?;
            (
                t0.elapsed().as_micros(),
                locked.picture_type(),
                locked.data().to_vec(),
            )
        } else {
            let mut input = session.create_input_buffer()?;
            unsafe { input.lock()?.write(&host) };

            let t0 = Instant::now();
            session.encode_picture(
                &mut input,
                &mut bitstream,
                EncodePictureParams {
                    input_timestamp: i as u64,
                    ..Default::default()
                },
            )?;
            let locked = bitstream.lock()?;
            (
                t0.elapsed().as_micros(),
                locked.picture_type(),
                locked.data().to_vec(),
            )
        };

        // lock() blocks until the bitstream is available, so the elapsed time is
        // submit -> available.
        latencies_us.push(elapsed_us);
        if matches!(
            pic_type,
            NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR | NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_I
        ) {
            keyframes += 1;
        }
        out.extend_from_slice(&data);
    }

    session.end_of_stream()?;
    std::fs::write("out.h264", &out)?;

    let mut sorted = latencies_us.clone();
    sorted.sort_unstable();

    println!();
    println!("--- results ---");
    println!(
        "input path     : {}",
        if !cuda_path_works {
            "mapped NVENC input buffer (host upload)".to_string()
        } else if device_nv12.is_some() {
            format!("registered CUDA device pointer (cudarc alloc), pitch {nv12_pitch}")
        } else {
            format!("registered CUDA device pointer (cuMemAlloc), pitch {nv12_pitch}")
        }
    );
    println!("frames encoded : {FRAMES}");
    println!("keyframes      : {keyframes} (expect 1: infinite GOP, no periodic IDR)");
    println!("out.h264       : {} bytes", out.len());
    println!(
        "avg bitrate    : {:.1} Mbps at 60fps",
        (out.len() as f64 * 8.0) / (FRAMES as f64 / 60.0) / 1_000_000.0
    );
    println!(
        "submit->available p50/p95/p99 : {:.3} / {:.3} / {:.3} ms",
        percentile(&sorted, 50.0) as f64 / 1000.0,
        percentile(&sorted, 95.0) as f64 / 1000.0,
        percentile(&sorted, 99.0) as f64 / 1000.0
    );
    println!(
        "cold first frame : {:.3} ms",
        latencies_us[0] as f64 / 1000.0
    );
    println!(
        "min / max        : {:.3} / {:.3} ms",
        sorted[0] as f64 / 1000.0,
        sorted[sorted.len() - 1] as f64 / 1000.0
    );
    println!();
    println!("now verify with:  ffprobe -show_frames -select_streams v out.h264 | Select-String \"pict_type=B\"");
    println!("expected: no matches (zero B-frames)");

    Ok(())
}
