//! The session's video pipeline on Windows: Desktop Duplication → BGRA→NV12
//! → NVENC, on one thread and one D3D11 device, feeding the paced sender.
//! The pipeline's shape and timing (Apollo's) are shared with the other
//! hosts: see `pipeline`.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpong_capture::dda::{sync_thread_desktop, DdaCapture};
use pingpong_capture::gpu::Gpu;
use pingpong_capture::Grab;
use pingpong_encode::convert::Converter;
use pingpong_encode::nvenc::NvencEncoder;
use pingpong_encode::EncoderConfig;
use pingpong_proto::clock;
use pingpong_transport::Endpoint;

use crate::pipeline::{Cadence, EncodeThread, FrameOut};
pub use crate::pipeline::{VideoCmd, VideoHandle};
use crate::sender;

#[derive(Debug, Clone)]
pub struct VideoParams {
    pub gdi_name: String,
    pub encoder: EncoderConfig,
    pub pace_mbps: u32,
}

/// Start capturing and encoding `params.gdi_name`. Blocks until the encoder is
/// running (or failed), so the caller can ack the session with the truth.
pub fn start(
    params: VideoParams,
    endpoint: Arc<Endpoint>,
    recipients: sender::Recipients,
) -> Result<VideoHandle, String> {
    let pace_mbps = params.pace_mbps;
    crate::pipeline::start(params, pace_mbps, endpoint, recipients, encode_loop)
}

/// Reconstruct a 64-bit frame index from the 32-bit id the client echoes,
/// relative to the newest index encoded.
fn widen(id: u32, newest: u64) -> u64 {
    newest.wrapping_sub((newest as u32).wrapping_sub(id) as u64)
}

fn encode_loop(thread: EncodeThread<VideoParams>) {
    let EncodeThread {
        params,
        frames,
        cmds,
        stop,
        streaming,
        stats,
        ready,
    } = thread;
    // The thread must be on the input desktop before duplicating it.
    sync_thread_desktop();

    let setup = (|| -> Result<(DdaCapture, Converter, NvencEncoder), String> {
        let gpu = Gpu::for_output(&params.gdi_name).map_err(|e| {
            let seen: Vec<String> = Gpu::list_outputs()
                .into_iter()
                .map(|(n, a)| format!("{n} ({a})"))
                .collect();
            format!("{e}; outputs: {seen:?}")
        })?;
        let (device, context) = (gpu.device.clone(), gpu.context.clone());
        let mut cap = DdaCapture::new(gpu);
        let e = params.encoder;
        let deadline = Instant::now() + Duration::from_secs(1);
        while cap.texture().is_none() {
            if Instant::now() > deadline {
                // A still desktop (the lock screen after sleep) presents
                // nothing: start from black, as Sunshine does, and let the
                // first present replace it.
                tracing::info!("no desktop image yet; starting from a black one");
                cap.blank(e.width, e.height).map_err(|e| e.to_string())?;
                break;
            }
            if let Err(e) = cap.grab(100) {
                tracing::debug!(error = %e, "waiting for the first desktop image");
            }
        }
        let conv =
            Converter::new(&device, &context, e.width, e.height).map_err(|e| e.to_string())?;
        let enc = NvencEncoder::new(&device, conv.output(), e).map_err(|e| e.to_string())?;
        let (cw, ch) = cap.size();
        if (cw, ch) != (e.width, e.height) {
            tracing::warn!(desktop = ?(cw, ch), stream = ?(e.width, e.height), "desktop and stream differ; scaling");
        }
        Ok((cap, conv, enc))
    })();
    let (mut cap, mut conv, mut enc) = match setup {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let Cadence {
        interval,
        repeat_every,
        slack,
    } = Cadence::new(params.encoder.fps);
    let mut out = FrameOut::new(frames, stats.clone(), params.encoder.fps);

    let mut index: u64 = 0;
    let mut force_idr = true;
    let mut last_encode: Option<Instant> = None;
    let mut have_new = false;

    while !stop.load(Ordering::Relaxed) {
        for cmd in cmds.try_iter() {
            match cmd {
                VideoCmd::Idr => force_idr = true,
                VideoCmd::Invalidate { first, last } => {
                    if index == 0 {
                        force_idr = true;
                        continue;
                    }
                    let newest = index - 1;
                    let (f, l) = (widen(first, newest), widen(last, newest));
                    if !enc.invalidate(f, l.min(newest)) {
                        force_idr = true;
                    }
                }
                VideoCmd::Bitrate(bps) => {
                    if let Err(e) = enc.set_bitrate(bps) {
                        tracing::warn!(error = %e, "bitrate change failed");
                    }
                }
            }
        }

        if !streaming.load(Ordering::Acquire) {
            // Keep the capture fresh so the first frame out is current.
            if let Ok(Grab::Frame) = cap.grab(10) {
                have_new = true;
            }
            continue;
        }

        // Not before the next frame slot.
        let now = Instant::now();
        if let Some(last) = last_encode {
            let slot = last + interval - slack;
            if now < slot && !force_idr {
                std::thread::sleep(slot - now);
            }
        }

        // Wait for the desktop, but never past the repeat deadline, and in
        // short slices so a recovery request is acted on promptly.
        let repeat_due = last_encode
            .map(|t| t + repeat_every)
            .unwrap_or_else(Instant::now);
        let wait_ms = repeat_due
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(4) as u32;
        match cap.grab(wait_ms) {
            Ok(Grab::Frame) => {
                have_new = true;
                stats.captured.fetch_add(1, Ordering::Relaxed);
            }
            Ok(Grab::Timeout) => {}
            Err(e) => {
                tracing::debug!(error = %e, "capture");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        let now = Instant::now();
        let repeat = !have_new && now >= repeat_due;
        if !(have_new || force_idr || repeat) {
            continue;
        }
        let captured_at_us = clock::now_us();
        if have_new {
            if let Some(tex) = cap.texture() {
                if let Err(e) = conv.convert(tex) {
                    tracing::warn!(error = %e, "convert");
                }
            }
            have_new = false;
        }
        let encoded = match enc.encode(index, force_idr) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(error = %e, "encode failed; ending the video pipeline");
                break;
            }
        };
        let host_us = clock::since(captured_at_us);
        last_encode = Some(now);
        force_idr = false;
        let frame = out.frame(
            index as u32,
            encoded.kind,
            &encoded.data,
            captured_at_us,
            host_us,
            repeat,
        );
        index += 1;
        out.send(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widen_recovers_indices_across_the_u32_wrap() {
        assert_eq!(widen(5, 10), 5);
        assert_eq!(widen(10, 10), 10);
        let newest = (1u64 << 32) + 3;
        assert_eq!(widen(u32::MAX, newest), (1u64 << 32) - 1);
    }
}
