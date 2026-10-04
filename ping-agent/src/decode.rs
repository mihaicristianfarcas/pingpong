//! The stream decoded into memory for an agent (no window): VideoToolbox on
//! a Mac, FFmpeg in software elsewhere. Every picture goes to a
//! [`FrameStore`], which keeps the newest and knows when the screen last
//! really changed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use ping_core::stream::{Codec, FrameTiming, VideoOut};

use crate::frame::{Signature, Yuv, MINOR_BLOCKS};

/// The newest picture, and the screen's recent history.
#[derive(Default)]
pub struct FrameStore {
    inner: Mutex<Inner>,
    cond: Condvar,
}

#[derive(Default)]
struct Inner {
    latest: Option<Arc<Yuv>>,
    /// Pictures decoded so far.
    count: u64,
    signature: Option<Signature>,
    /// When a picture last differed from the one before by more than a
    /// caret's blink.
    last_change: Option<Instant>,
    last_frame: Option<Instant>,
}

/// A picture, and what the store knew when it was handed out.
#[derive(Clone)]
pub struct Snapshot {
    pub picture: Arc<Yuv>,
    /// Its number among the pictures decoded.
    pub index: u64,
    /// Nothing but a blinking caret changed for this long before it.
    pub settled: bool,
}

impl FrameStore {
    pub fn push(&self, picture: Yuv) {
        let signature = picture.signature();
        let now = Instant::now();
        let mut g = self.inner.lock();
        let changed = match &g.signature {
            Some(prev) => prev.changed_blocks(&signature) > MINOR_BLOCKS,
            None => true,
        };
        if changed {
            g.last_change = Some(now);
        }
        g.signature = Some(signature);
        g.latest = Some(Arc::new(picture));
        g.count += 1;
        g.last_frame = Some(now);
        drop(g);
        self.cond.notify_all();
    }

    pub fn latest(&self) -> Option<Snapshot> {
        let g = self.inner.lock();
        g.latest.clone().map(|picture| Snapshot {
            picture,
            index: g.count,
            settled: true,
        })
    }

    pub fn count(&self) -> u64 {
        self.inner.lock().count
    }

    /// Wait for a first picture.
    pub fn wait_first(&self, timeout: Duration) -> Option<Snapshot> {
        let deadline = Instant::now() + timeout;
        let mut g = self.inner.lock();
        while g.latest.is_none() {
            if self.cond.wait_until(&mut g, deadline).timed_out() {
                break;
            }
        }
        g.latest.clone().map(|picture| Snapshot {
            picture,
            index: g.count,
            settled: true,
        })
    }

    /// The screen after an action at `since`: wait at least `min` for it
    /// to react, then until nothing but a caret has changed for `quiet`,
    /// giving up after `max` (the picture then says `settled: false`).
    pub fn wait_settled(
        &self,
        since: Instant,
        min: Duration,
        quiet: Duration,
        max: Duration,
    ) -> Option<Snapshot> {
        let deadline = since + max;
        let earliest = since + min;
        let mut g = self.inner.lock();
        loop {
            let now = Instant::now();
            let settled_at = g
                .last_change
                .map_or(earliest, |t| (t + quiet).max(earliest));
            // A picture must have arrived since the action, or we would
            // hand back the screen from before it.
            let fresh = g.last_frame.is_some_and(|t| t >= earliest.min(now));
            if now >= settled_at && fresh {
                return g.latest.clone().map(|picture| Snapshot {
                    picture,
                    index: g.count,
                    settled: true,
                });
            }
            if now >= deadline {
                return g.latest.clone().map(|picture| Snapshot {
                    picture,
                    index: g.count,
                    settled: false,
                });
            }
            let until = if now < settled_at {
                settled_at.min(deadline)
            } else {
                deadline
            };
            // Woken by each picture; otherwise at the time things settle.
            let _ = self
                .cond
                .wait_until(&mut g, until.max(now + Duration::from_millis(5)));
        }
    }

    /// Wait until the screen changes after `since` (or `max` passes). True
    /// if it did.
    pub fn wait_change(&self, since: Instant, max: Duration) -> bool {
        let deadline = Instant::now() + max;
        let mut g = self.inner.lock();
        loop {
            if g.last_change.is_some_and(|t| t > since) {
                return true;
            }
            if self.cond.wait_until(&mut g, deadline).timed_out() {
                return g.last_change.is_some_and(|t| t > since);
            }
        }
    }
}

/// `VideoOut` for a session without a window.
pub struct HeadlessVideo {
    store: Arc<FrameStore>,
    stats: Arc<ping_core::stats::StatsCollector>,
    #[cfg(target_os = "macos")]
    decoder: Option<pingpong_decode::videotoolbox::VtDecoder>,
    #[cfg(not(target_os = "macos"))]
    decoder: Option<pingpong_decode::ffmpeg::FfmpegDecoder>,
}

impl HeadlessVideo {
    pub fn new(
        store: Arc<FrameStore>,
        stats: Arc<ping_core::stats::StatsCollector>,
    ) -> HeadlessVideo {
        HeadlessVideo {
            store,
            stats,
            decoder: None,
        }
    }
}

fn codec(c: Codec) -> pingpong_decode::Codec {
    match c {
        Codec::H264 => pingpong_decode::Codec::H264,
        Codec::Hevc => pingpong_decode::Codec::Hevc,
        Codec::Av1 => pingpong_decode::Codec::Av1,
    }
}

#[cfg(target_os = "macos")]
impl VideoOut for HeadlessVideo {
    fn configure(&mut self, c: Codec, _width: u32, _height: u32, _video: u8) -> Result<(), String> {
        let (store, stats) = (self.store.clone(), self.stats.clone());
        let sink: pingpong_decode::videotoolbox::FrameSink =
            Arc::new(move |f: pingpong_decode::DecodedFrame| {
                let started = pingpong_proto::clock::now_us();
                if let Some(picture) = mac::copy_out(&f) {
                    store.push(picture);
                    stats.decoded(pingpong_proto::clock::now_us().wrapping_sub(started));
                }
            });
        self.decoder = Some(
            pingpong_decode::videotoolbox::VtDecoder::with_sink(codec(c), sink)
                .map_err(|e| e.to_string())?,
        );
        Ok(())
    }

    fn decode(&mut self, bitstream: &[u8], timing: FrameTiming) -> Result<(), String> {
        use pingpong_decode::VideoDecoder;
        let Some(d) = self.decoder.as_mut() else {
            return Ok(());
        };
        d.decode(bitstream, timing.frame_id)
            .map_err(|e| e.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
impl VideoOut for HeadlessVideo {
    fn configure(&mut self, c: Codec, _width: u32, _height: u32, _video: u8) -> Result<(), String> {
        // Software: a few frames a second of a desktop cost little, and the
        // pictures are wanted in memory anyway.
        self.decoder = Some(
            pingpong_decode::ffmpeg::FfmpegDecoder::new(codec(c), true)
                .map_err(|e| e.to_string())?,
        );
        Ok(())
    }

    fn decode(&mut self, bitstream: &[u8], timing: FrameTiming) -> Result<(), String> {
        use crate::frame::PlaneRef;
        use pingpong_decode::ffmpeg::Layout;
        let Some(d) = self.decoder.as_mut() else {
            return Ok(());
        };
        let started = pingpong_proto::clock::now_us();
        let store = &self.store;
        fn plane<'a>(pl: &pingpong_decode::ffmpeg::Plane<'a>) -> PlaneRef<'a> {
            PlaneRef {
                data: pl.data,
                stride: pl.stride,
            }
        }
        d.decode(bitstream, timing.frame_id, |p| {
            let yuv = match &p.layout {
                Layout::Nv12 { y, uv } => {
                    Yuv::from_nv12(p.width, p.height, plane(y), plane(uv), false)
                }
                Layout::I420 { y, u, v } => Yuv::from_planar(
                    p.width,
                    p.height,
                    plane(y),
                    plane(u),
                    plane(v),
                    (p.width.div_ceil(2), p.height.div_ceil(2)),
                    false,
                ),
                Layout::I444 { y, u, v } => Yuv::from_planar(
                    p.width,
                    p.height,
                    plane(y),
                    plane(u),
                    plane(v),
                    (p.width, p.height),
                    false,
                ),
            };
            store.push(yuv);
        })
        .map_err(|e| e.to_string())?;
        self.stats
            .decoded(pingpong_proto::clock::now_us().wrapping_sub(started));
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType,
        CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };

    use crate::frame::{PlaneRef, Yuv};

    /// The bi-planar formats (luma, then interleaved chroma), video and full
    /// range: NV12 from the hosts' 4:2:0 streams, and 4:2:2 and 4:4:4.
    const BIPLANAR: [(&[u8; 4], bool); 6] = [
        (b"420v", false),
        (b"420f", true),
        (b"422v", false),
        (b"422f", true),
        (b"444v", false),
        (b"444f", true),
    ];

    /// The decoded pixel buffer's planes, copied into memory.
    pub fn copy_out(f: &pingpong_decode::DecodedFrame) -> Option<Yuv> {
        if f.pixel_buffer.is_null() {
            return None;
        }
        // SAFETY: a live, retained CVPixelBuffer for as long as `f` is.
        let buf: &CVPixelBuffer = unsafe { &*(f.pixel_buffer as *const CVPixelBuffer) };
        let format = CVPixelBufferGetPixelFormatType(buf);
        let Some(&(_, full_range)) = BIPLANAR
            .iter()
            .find(|(fourcc, _)| u32::from_be_bytes(**fourcc) == format)
        else {
            tracing::warn!(format = %String::from_utf8_lossy(&format.to_be_bytes()), "a decoded format the agent cannot read");
            return None;
        };
        // SAFETY: locked for reading, unlocked below; the plane pointers are
        // only used in between.
        unsafe {
            if CVPixelBufferLockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly) != 0 {
                return None;
            }
            let plane = |i: usize| {
                let base = CVPixelBufferGetBaseAddressOfPlane(buf, i) as *const u8;
                let stride = CVPixelBufferGetBytesPerRowOfPlane(buf, i);
                let rows = CVPixelBufferGetHeightOfPlane(buf, i);
                (base, stride, rows, CVPixelBufferGetWidthOfPlane(buf, i))
            };
            let (y, ys, yr, yw) = plane(0);
            let (uv, uvs, uvr, uvw) = plane(1);
            let out = if y.is_null() || uv.is_null() {
                None
            } else {
                let y = std::slice::from_raw_parts(y, ys * yr);
                let uv = std::slice::from_raw_parts(uv, uvs * uvr);
                Some(Yuv::from_semi_planar(
                    yw as u32,
                    yr as u32,
                    PlaneRef {
                        data: y,
                        stride: ys,
                    },
                    PlaneRef {
                        data: uv,
                        stride: uvs,
                    },
                    (uvw as u32, uvr as u32),
                    full_range,
                ))
            };
            CVPixelBufferUnlockBaseAddress(buf, CVPixelBufferLockFlags::ReadOnly);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(luma: u8) -> Yuv {
        Yuv {
            width: 64,
            height: 64,
            y: vec![luma; 64 * 64],
            u: vec![128; 32 * 32],
            v: vec![128; 32 * 32],
            chroma_width: 32,
            chroma_height: 32,
            full_range: false,
        }
    }

    #[test]
    fn settles_once_the_screen_stops_changing() {
        let store = Arc::new(FrameStore::default());
        store.push(picture(10));
        let action = Instant::now();
        let s = store.clone();
        let feeder = std::thread::spawn(move || {
            // The screen changes for 150 ms after the action, then holds.
            for i in 0..6 {
                std::thread::sleep(Duration::from_millis(25));
                s.push(picture(40 + i * 30));
            }
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(25));
                s.push(picture(220));
            }
        });
        let got = store
            .wait_settled(
                action,
                Duration::from_millis(50),
                Duration::from_millis(200),
                Duration::from_secs(3),
            )
            .unwrap();
        let waited = action.elapsed();
        assert!(got.settled);
        assert_eq!(got.picture.y[0], 220);
        // At least the 150 ms of change and the 200 ms of quiet; at most
        // well short of the 3 s limit (it settled, it did not give up). The
        // sleeps run long on a loaded machine: 1.08 s on a CI runner.
        assert!(
            waited >= Duration::from_millis(340) && waited < Duration::from_millis(2500),
            "{waited:?}"
        );
        feeder.join().unwrap();
    }

    #[test]
    fn a_screen_that_never_stops_is_returned_unsettled() {
        let store = Arc::new(FrameStore::default());
        let action = Instant::now();
        let s = store.clone();
        let feeder = std::thread::spawn(move || {
            for i in 0..40u32 {
                std::thread::sleep(Duration::from_millis(15));
                s.push(picture(if i % 2 == 0 { 20 } else { 200 }));
            }
        });
        let got = store
            .wait_settled(
                action,
                Duration::ZERO,
                Duration::from_millis(200),
                Duration::from_millis(400),
            )
            .unwrap();
        assert!(!got.settled);
        feeder.join().unwrap();
    }
}
