//! macOS: ScreenCaptureKit, the capture macOS offers for whole displays.
//!
//! The stream is configured the way the rest of the pipeline wants its
//! frames: already in NV12 at video range with the BT.709 matrix (what
//! VideoToolbox encodes without a conversion, and what Ping's renderer
//! assumes), at the stream's size, never faster than the stream's frame rate.
//! ScreenCaptureKit only delivers a frame when the display changed, so, as
//! with Desktop Duplication, `grab` waits for news with a timeout and the
//! session repeats the last image when there is none.

use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_foundation::CFRetained;
use objc2_core_graphics::{kCGDisplayStreamYCbCrMatrix_ITU_R_709_2, CGMainDisplayID};
use objc2_core_media::{
    CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime, CMTimeFlags,
};
use objc2_core_video::{kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType,
};

use crate::{CaptureError, Grab};

/// The newest frame ScreenCaptureKit delivered, and how many it has.
#[derive(Default)]
struct Latest {
    slot: Mutex<(u64, Option<CFRetained<CVPixelBuffer>>)>,
    fresh: Condvar,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "PongFrameOutput"]
    #[ivars = Arc<Latest>]
    struct FrameOutput;

    unsafe impl NSObjectProtocol for FrameOutput {}

    unsafe impl SCStreamOutput for FrameOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            // An unchanged display still reports in ("idle"), without an image.
            let Some(image) = (unsafe { sample.image_buffer() }) else { return };
            let latest = self.ivars();
            let mut slot = latest.slot.lock().unwrap_or_else(|e| e.into_inner());
            slot.0 += 1;
            slot.1 = Some(image);
            latest.fresh.notify_all();
        }
    }
);

impl FrameOutput {
    fn new(latest: Arc<Latest>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(latest);
        unsafe { msg_send![super(this), init] }
    }
}

/// Holds an Objective-C object across the completion handler's thread.
struct Handoff<T>(T);
// SAFETY: the objects handed over are only used again on the receiving
// thread, after the completion handler has let go of them.
unsafe impl<T> Send for Handoff<T> {}

/// Captures one display.
pub struct SckCapture {
    stream: Retained<SCStream>,
    _output: Retained<FrameOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    latest: Arc<Latest>,
    seen: u64,
    size: (u32, u32),
}

// SAFETY: SCStream is used only through &mut self; ScreenCaptureKit objects
// are not thread-affine.
unsafe impl Send for SckCapture {}

impl SckCapture {
    /// The display macOS calls main (the one with the menu bar).
    pub fn main_display() -> u32 {
        CGMainDisplayID()
    }

    /// Capture `display_id` scaled to `width`x`height`, at most `fps` frames a
    /// second. `cursor`: draw the pointer into the image.
    pub fn new(
        display_id: u32,
        width: u32,
        height: u32,
        fps: u32,
        cursor: bool,
    ) -> Result<SckCapture, CaptureError> {
        let display = find_display(display_id)?;
        unsafe {
            let config = SCStreamConfiguration::new();
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: fps.max(1) as i32,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            config.setPixelFormat(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
            config.setColorMatrix(kCGDisplayStreamYCbCrMatrix_ITU_R_709_2);
            config.setShowsCursor(cursor);
            // Enough in flight that the encoder holding one never stalls capture.
            config.setQueueDepth(5);

            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            );
            // Shared with the capture queue's callback. The pixel buffers in it
            // are CoreFoundation objects, which are safe to retain and release
            // from any thread; the mutex orders the rest.
            #[allow(clippy::arc_with_non_send_sync)]
            let latest = Arc::new(Latest::default());
            let output = FrameOutput::new(latest.clone());
            let queue = DispatchQueue::new("pong.capture", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| {
                    CaptureError::Platform(format!("adding the stream output: {}", describe(&e)))
                })?;
            wait(|done| stream.startCaptureWithCompletionHandler(Some(&done)))
                .map_err(|e| CaptureError::Platform(format!("starting capture: {e}")))?;
            Ok(SckCapture {
                stream,
                _output: output,
                _queue: queue,
                latest,
                seen: 0,
                size: (width, height),
            })
        }
    }

    /// Wait up to `timeout_ms` for a frame newer than the last one grabbed.
    pub fn grab(&mut self, timeout_ms: u32) -> Result<Grab, CaptureError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let mut slot = self.latest.slot.lock().unwrap_or_else(|e| e.into_inner());
        while slot.0 == self.seen {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Grab::Timeout);
            }
            slot = self
                .latest
                .fresh
                .wait_timeout(slot, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        self.seen = slot.0;
        Ok(Grab::Frame)
    }

    /// The newest image, if any has arrived.
    pub fn image(&self) -> Option<CFRetained<CVPixelBuffer>> {
        self.latest
            .slot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .1
            .clone()
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}

impl Drop for SckCapture {
    fn drop(&mut self) {
        let _ = wait(|done| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) });
    }
}

/// The ScreenCaptureKit display with this CoreGraphics id.
fn find_display(display_id: u32) -> Result<Retained<SCDisplay>, CaptureError> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            let found = match unsafe { content.as_ref() } {
                Some(content) => {
                    let displays = unsafe { content.displays() };
                    let ids: Vec<u32> = displays.iter().map(|d| unsafe { d.displayID() }).collect();
                    let display = displays
                        .iter()
                        .find(|d| unsafe { d.displayID() } == display_id);
                    Ok((display.map(Handoff), ids))
                }
                None => Err(unsafe { error.as_ref() }
                    .map(describe)
                    .unwrap_or_else(|| "no shareable content".into())),
            };
            let _ = tx.send(found);
        },
    );
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok((Some(Handoff(display)), _))) => Ok(display),
        // Asleep displays (and a locked screen's) are not offered for capture.
        Ok(Ok((None, ids))) => Err(CaptureError::NoSuchOutput(format!(
            "display {display_id} (capturable: {ids:?})"
        ))),
        // Most often: this process (or the terminal it runs in) lacks the
        // Screen Recording permission.
        Ok(Err(e)) => Err(CaptureError::Platform(format!("listing displays: {e}"))),
        Err(_) => Err(CaptureError::Platform("listing displays timed out".into())),
    }
}

/// Call `start` with a completion handler and wait for it.
fn wait(start: impl FnOnce(RcBlock<dyn Fn(*mut NSError)>)) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let done = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(unsafe { error.as_ref() }.map(describe));
    });
    start(done);
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(None) => Ok(()),
        Ok(Some(e)) => Err(e),
        Err(_) => Err("timed out".into()),
    }
}

fn describe(e: &NSError) -> String {
    e.localizedDescription().to_string()
}

/// Where captured sound goes: interleaved 32-bit float frames, and how many
/// channels they have. Called on ScreenCaptureKit's queue.
pub type AudioSink = Box<dyn Fn(&[f32], usize) + Send + Sync>;

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "PongAudioOutput"]
    #[ivars = AudioSink]
    struct AudioOutput;

    unsafe impl NSObjectProtocol for AudioOutput {}

    unsafe impl SCStreamOutput for AudioOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Audio {
                return;
            }
            if let Some((pcm, channels)) = unsafe { interleaved(sample) } {
                (self.ivars())(&pcm, channels);
            }
        }
    }
);

impl AudioOutput {
    fn new(sink: AudioSink) -> Retained<Self> {
        let this = Self::alloc().set_ivars(sink);
        unsafe { msg_send![super(this), init] }
    }
}

/// The sample's float PCM, interleaved (ScreenCaptureKit delivers one plane
/// per channel), and its channel count.
unsafe fn interleaved(sample: &CMSampleBuffer) -> Option<(Vec<f32>, usize)> {
    const FLOAT: u32 = 1; // kAudioFormatFlagIsFloat
    const NON_INTERLEAVED: u32 = 1 << 5; // kAudioFormatFlagIsNonInterleaved
    let format = unsafe { sample.format_description() }?;
    let asbd = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&format).as_ref() }?;
    if asbd.mFormatFlags & FLOAT == 0 || asbd.mBitsPerChannel != 32 {
        return None;
    }
    let channels = asbd.mChannelsPerFrame.max(1) as usize;
    let frames = unsafe { sample.num_samples() }.max(0) as usize;
    let block = unsafe { sample.data_buffer() }?;
    let len = unsafe { block.data_length() };
    let mut raw = vec![0f32; len / 4];
    let status =
        unsafe { block.copy_data_bytes(0, raw.len() * 4, NonNull::new(raw.as_mut_ptr().cast())?) };
    if status != 0 || raw.len() < frames * channels {
        return None;
    }
    if asbd.mFormatFlags & NON_INTERLEAVED == 0 {
        raw.truncate(frames * channels);
        return Some((raw, channels));
    }
    let mut out = vec![0f32; frames * channels];
    for c in 0..channels {
        let plane = &raw[c * frames..(c + 1) * frames];
        for (i, &v) in plane.iter().enumerate() {
            out[i * channels + c] = v;
        }
    }
    Some((out, channels))
}

/// Captures what the Mac plays (every app but this one), 48 kHz stereo.
pub struct SckAudio {
    stream: Retained<SCStream>,
    _output: Retained<AudioOutput>,
    _queue: DispatchRetained<DispatchQueue>,
}

// SAFETY: as SckCapture.
unsafe impl Send for SckAudio {}

impl SckAudio {
    /// ScreenCaptureKit captures sound alongside a display; any capturable
    /// one will do (`display_id`).
    pub fn new(display_id: u32, sink: AudioSink) -> Result<SckAudio, CaptureError> {
        let display = find_display(display_id)?;
        unsafe {
            let config = SCStreamConfiguration::new();
            // The picture is not wanted: as small and as rare as allowed.
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: 1,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            });
            config.setCapturesAudio(true);
            config.setSampleRate(48_000);
            config.setChannelCount(2);
            config.setExcludesCurrentProcessAudio(true);
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            );
            let output = AudioOutput::new(sink);
            let queue = DispatchQueue::new("pong.audio-capture", None);
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*output),
                    SCStreamOutputType::Audio,
                    Some(&queue),
                )
                .map_err(|e| {
                    CaptureError::Platform(format!("adding the audio output: {}", describe(&e)))
                })?;
            wait(|done| stream.startCaptureWithCompletionHandler(Some(&done)))
                .map_err(|e| CaptureError::Platform(format!("starting audio capture: {e}")))?;
            Ok(SckAudio {
                stream,
                _output: output,
                _queue: queue,
            })
        }
    }
}

impl Drop for SckAudio {
    fn drop(&mut self) {
        let _ = wait(|done| unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&done)) });
    }
}
