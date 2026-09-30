//! Spike: measure VideoToolbox H.264 decode latency and find out whether
//! `kVTDecompressionPropertyKey_RealTime` is actually honoured.
//!
//! Resolves risk #3 (plan Task 1). The DELIVERABLE IS THE NUMBERS, not this
//! code -- see README.md. Nothing here is meant to be reused; Task 14 rewrites
//! the decoder properly.
//!
//! Two questions, both of which change the client design if answered badly:
//!   1. What is p50/p95/p99 submit -> output-callback latency?
//!   2. Does the callback fire one-per-submit, or in bursts? Bursts mean the
//!      decoder is buffering, which no amount of network tuning can undo.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::time::Instant;

use objc2_core_foundation::{kCFBooleanTrue, CFRetained, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMVideoFormatDescription, CMVideoFormatDescriptionCreateFromH264ParameterSets,
};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    kVTDecompressionPropertyKey_RealTime, VTDecodeFrameFlags, VTDecodeInfoFlags,
    VTDecompressionOutputCallbackRecord, VTDecompressionSession, VTSessionSetProperty,
};

/// Timing state, indexed by frame ordinal.
struct Timing {
    /// When frame i was handed to VTDecompressionSessionDecodeFrame.
    submitted_at: Vec<Instant>,
    /// (frame_ordinal, latency_us) as callbacks land, in callback order.
    completed: Vec<(usize, u64)>,
    /// Non-zero OSStatus values seen in the callback.
    errors: Vec<i32>,
    /// Frames whose callback delivered a null image buffer.
    null_images: usize,
}

static TIMING: Mutex<Option<Timing>> = Mutex::new(None);

/// VTDecompressionOutputCallback. Fires on VideoToolbox's own thread.
unsafe extern "C-unwind" fn output_callback(
    _output_ref_con: *mut c_void,
    source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: VTDecodeInfoFlags,
    image_buffer: *mut CVImageBuffer,
    _pts: CMTime,
    _duration: CMTime,
) {
    let now = Instant::now();
    let frame_ordinal = source_frame_ref_con as usize;

    let mut guard = TIMING.lock().unwrap();
    let t = guard.as_mut().expect("timing initialised before decoding");

    if status != 0 {
        t.errors.push(status);
    }
    if image_buffer.is_null() {
        t.null_images += 1;
    }

    let submitted = t.submitted_at[frame_ordinal];
    let latency_us = now.duration_since(submitted).as_micros() as u64;
    t.completed.push((frame_ordinal, latency_us));
}

/// Split an Annex-B elementary stream into NAL units (payload only, start codes
/// stripped).
fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    // Collect every start code, then slice between them.
    let mut starts: Vec<(usize, usize)> = Vec::new(); // (payload_start, code_len)
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 {
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                starts.push((i + 4, 4));
                i += 4;
                continue;
            }
            if data[i + 2] == 1 {
                starts.push((i + 3, 3));
                i += 3;
                continue;
            }
        }
        i += 1;
    }

    let mut nals = Vec::with_capacity(starts.len());
    for (n, &(payload_start, _)) in starts.iter().enumerate() {
        let end = match starts.get(n + 1) {
            // The next NAL's payload start, minus its start-code length.
            Some(&(next_start, next_code_len)) => next_start - next_code_len,
            None => data.len(),
        };
        if payload_start < end {
            nals.push(&data[payload_start..end]);
        }
    }
    nals
}

fn nal_type(nal: &[u8]) -> u8 {
    nal[0] & 0x1F
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // Nearest-rank on a 0-indexed sorted slice.
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn main() {
    let stream = std::fs::read("testsrc.h264").expect(
        "testsrc.h264 not found -- generate it with the ffmpeg command in README.md \
         (run from spikes/vt-latency/)",
    );
    let nals = split_annexb(&stream);
    println!(
        "parsed {} NAL units from {} bytes",
        nals.len(),
        stream.len()
    );

    let sps = nals
        .iter()
        .find(|n| nal_type(n) == 7)
        .expect("no SPS (NAL type 7) in stream");
    let pps = nals
        .iter()
        .find(|n| nal_type(n) == 8)
        .expect("no PPS (NAL type 8) in stream");
    println!("SPS {} bytes, PPS {} bytes", sps.len(), pps.len());

    // --- format description from the parameter sets ---
    // nal_unit_header_length = 4: every Annex-B start code is rewritten to a
    // 4-byte big-endian length below, because VideoToolbox does not accept
    // Annex-B.
    let param_ptrs: [NonNull<u8>; 2] = [
        NonNull::new(sps.as_ptr() as *mut u8).unwrap(),
        NonNull::new(pps.as_ptr() as *mut u8).unwrap(),
    ];
    let param_sizes: [usize; 2] = [sps.len(), pps.len()];

    let mut fmt: *const CMFormatDescription = std::ptr::null();
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            None,
            2,
            NonNull::new(param_ptrs.as_ptr() as *mut NonNull<u8>).unwrap(),
            NonNull::new(param_sizes.as_ptr() as *mut usize).unwrap(),
            4,
            NonNull::new(&mut fmt as *mut *const CMFormatDescription).unwrap(),
        )
    };
    assert_eq!(
        status, 0,
        "CMVideoFormatDescriptionCreateFromH264ParameterSets"
    );
    assert!(!fmt.is_null());
    let fmt: CFRetained<CMVideoFormatDescription> = unsafe {
        CFRetained::from_raw(NonNull::new(fmt as *mut CMVideoFormatDescription).unwrap())
    };

    // --- build AVCC frames up front so timing measures decode, not parsing ---
    // Each buffer stays alive for the whole run, so CMBlockBuffer can borrow it
    // with kCFAllocatorNull and never copy.
    let frames: Vec<Vec<u8>> = nals
        .iter()
        .filter(|n| matches!(nal_type(n), 1 | 5)) // non-IDR and IDR slices
        .map(|n| {
            let mut avcc = Vec::with_capacity(4 + n.len());
            avcc.extend_from_slice(&(n.len() as u32).to_be_bytes());
            avcc.extend_from_slice(n);
            avcc
        })
        .collect();
    println!("{} coded frames to decode", frames.len());
    assert!(!frames.is_empty(), "no VCL NALs found");

    // Two runs. Feeding 600 frames as fast as the CPU can build sample buffers
    // is NOT the production case and inflates the in-flight depth all by
    // itself, so the burst run cannot answer "is the decoder buffering?".
    // The paced run submits at 60 fps, exactly as the client will.
    decode_run(&fmt, &frames, None);
    decode_run(
        &fmt,
        &frames,
        Some(std::time::Duration::from_micros(16_667)),
    );
}

/// Decode every frame through a fresh session. `pace` is the interval between
/// submissions; `None` submits as fast as possible.
fn decode_run(
    fmt: &CFRetained<CMVideoFormatDescription>,
    frames: &[Vec<u8>],
    pace: Option<std::time::Duration>,
) {
    println!();
    println!("================================================================");
    match pace {
        None => println!("RUN: burst (submit as fast as possible -- not the production case)"),
        Some(d) => println!(
            "RUN: paced at {:.2} ms between submits ({} fps -- the production case)",
            d.as_secs_f64() * 1000.0,
            (1.0 / d.as_secs_f64()).round() as u32
        ),
    }
    println!("================================================================");

    // --- decompression session ---
    let callback_record = VTDecompressionOutputCallbackRecord {
        decompressionOutputCallback: Some(output_callback),
        decompressionOutputRefCon: std::ptr::null_mut(),
    };
    let mut session: *mut VTDecompressionSession = std::ptr::null_mut();
    let status = unsafe {
        VTDecompressionSession::create(
            None,
            fmt,
            None,
            None,
            &callback_record,
            NonNull::new(&mut session as *mut *mut VTDecompressionSession).unwrap(),
        )
    };
    assert_eq!(status, 0, "VTDecompressionSessionCreate failed: {status}");
    let session: CFRetained<VTDecompressionSession> =
        unsafe { CFRetained::from_raw(NonNull::new(session).unwrap()) };

    // --- THE POINT OF THE SPIKE: does the real-time flag stick? ---
    let realtime_status = unsafe {
        let key = kVTDecompressionPropertyKey_RealTime;
        let true_value: &CFType = kCFBooleanTrue.expect("kCFBooleanTrue");
        VTSessionSetProperty(&session, key, Some(true_value))
    };
    if realtime_status == 0 {
        println!("kVTDecompressionPropertyKey_RealTime: ACCEPTED (noErr)");
    } else {
        println!(
            "kVTDecompressionPropertyKey_RealTime: REJECTED, OSStatus {realtime_status} \
             -- flag unsupported, relying on the no-B-frame stream alone"
        );
    }

    *TIMING.lock().unwrap() = Some(Timing {
        submitted_at: Vec::with_capacity(frames.len()),
        completed: Vec::with_capacity(frames.len()),
        errors: Vec::new(),
        null_images: 0,
    });

    // How far ahead of completions submission gets. If the decoder honours
    // real-time and does not buffer, this stays tiny under pacing. A number
    // that climbs toward frames.len() is the "bursty" failure mode.
    let mut max_in_flight = 0usize;
    let mut next_submit = Instant::now();

    for (ordinal, avcc) in frames.iter().enumerate() {
        if let Some(interval) = pace {
            // Busy-wait rather than sleep: sleep granularity would add its own
            // jitter to a measurement in the low milliseconds.
            while Instant::now() < next_submit {
                std::hint::spin_loop();
            }
            next_submit += interval;
        }

        // CMBlockBuffer borrowing `avcc` -- kCFAllocatorNull means "do not free".
        let mut block: *mut CMBlockBuffer = std::ptr::null_mut();
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                avcc.as_ptr() as *mut c_void,
                avcc.len(),
                objc2_core_foundation::kCFAllocatorNull,
                std::ptr::null(),
                0,
                avcc.len(),
                0,
                NonNull::new(&mut block as *mut *mut CMBlockBuffer).unwrap(),
            )
        };
        assert_eq!(status, 0, "CMBlockBufferCreateWithMemoryBlock");
        let block: CFRetained<CMBlockBuffer> =
            unsafe { CFRetained::from_raw(NonNull::new(block).unwrap()) };

        // 60 fps timing. VideoToolbox is happier with valid timestamps even
        // though this spike never reads them back.
        let timing = CMSampleTimingInfo {
            duration: CMTime {
                value: 1,
                timescale: 60,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            },
            presentationTimeStamp: CMTime {
                value: ordinal as i64,
                timescale: 60,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            },
            decodeTimeStamp: CMTime {
                value: ordinal as i64,
                timescale: 60,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            },
        };
        let sizes = [avcc.len()];

        let mut sample: *mut CMSampleBuffer = std::ptr::null_mut();
        let status = unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&fmt),
                1,
                1,
                &timing,
                1,
                sizes.as_ptr(),
                NonNull::new(&mut sample as *mut *mut CMSampleBuffer).unwrap(),
            )
        };
        assert_eq!(status, 0, "CMSampleBufferCreateReady");
        let sample: CFRetained<CMSampleBuffer> =
            unsafe { CFRetained::from_raw(NonNull::new(sample).unwrap()) };

        // Record the submit instant and in-flight depth under one lock, then
        // release it -- the callback needs the same lock and may fire
        // synchronously from inside decode_frame.
        let in_flight = {
            let mut guard = TIMING.lock().unwrap();
            let t = guard.as_mut().unwrap();
            t.submitted_at.push(Instant::now());
            t.submitted_at.len() - t.completed.len()
        };
        max_in_flight = max_in_flight.max(in_flight);

        let status = unsafe {
            session.decode_frame(
                &sample,
                VTDecodeFrameFlags::Frame_EnableAsynchronousDecompression,
                ordinal as *mut c_void,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(
            status, 0,
            "VTDecompressionSessionDecodeFrame frame {ordinal}"
        );
    }

    let status = unsafe { session.wait_for_asynchronous_frames() };
    assert_eq!(status, 0, "VTDecompressionSessionWaitForAsynchronousFrames");

    // --- report ---
    let guard = TIMING.lock().unwrap();
    let t = guard.as_ref().unwrap();

    let mut latencies: Vec<u64> = t.completed.iter().map(|&(_, us)| us).collect();
    latencies.sort_unstable();

    let p50 = percentile(&latencies, 50.0);
    let p95 = percentile(&latencies, 95.0);
    let p99 = percentile(&latencies, 99.0);

    // Out-of-order completions would mean the decoder is reordering, which a
    // no-B-frame stream should never trigger.
    let out_of_order = t.completed.windows(2).filter(|w| w[1].0 < w[0].0).count();

    println!();
    println!("--- results ---");
    println!("submitted        : {}", t.submitted_at.len());
    println!("completed        : {}", t.completed.len());
    println!(
        "decode errors    : {} {:?}",
        t.errors.len(),
        &t.errors[..t.errors.len().min(5)]
    );
    println!("null image bufs  : {}", t.null_images);
    println!("out-of-order     : {out_of_order}");
    println!(
        "max in flight    : {max_in_flight}  (1-2 = one-per-submit, large = BURSTY/buffering)"
    );
    println!(
        "latency p50/p95/p99 : {:.2} / {:.2} / {:.2} ms",
        p50 as f64 / 1000.0,
        p95 as f64 / 1000.0,
        p99 as f64 / 1000.0
    );
    println!(
        "latency min/max     : {:.2} / {:.2} ms",
        latencies.first().copied().unwrap_or(0) as f64 / 1000.0,
        latencies.last().copied().unwrap_or(0) as f64 / 1000.0
    );

    println!();
    println!("--- verdict against the 8 ms p95 threshold (plan Task 1 Step 4) ---");
    if p95 <= 8_000 {
        println!(
            "PASS: p95 {:.2} ms <= 8 ms. Spec section 12's budget holds.",
            p95 as f64 / 1000.0
        );
    } else if p95 <= 16_000 {
        println!(
            "MARGINAL: p95 {:.2} ms is above 8 ms but below 16 ms. Budget needs revising.",
            p95 as f64 / 1000.0
        );
    } else {
        println!(
            "FAIL: p95 {:.2} ms > 16 ms. STOP -- client design needs revisiting.",
            p95 as f64 / 1000.0
        );
    }
    if max_in_flight > 4 {
        println!(
            "WARNING: max in flight {max_in_flight} suggests the decoder is buffering, \
             not decoding one-per-submit."
        );
    }
}
