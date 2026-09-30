//! Linux under Wayland: a screen cast the desktop portal set up, received
//! from PipeWire. The portal hands over a PipeWire connection (a file
//! descriptor) and the stream's node; frames arrive in shared memory, the
//! pointer drawn in by the compositor, and are copied out on PipeWire's
//! thread into buffers the encoder reads.
//!
//! Compositors send a frame when the screen changes, so `grab` waits for
//! the next one, as it does on X11.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::VideoFormat;
use pw::spa::pod::Pod;

use crate::image::{Frame, PixelOrder, Pixels};
use crate::{CaptureError, Grab};

/// Buffers the frames are copied into, taken in turn.
const BUFFERS: usize = 3;

#[derive(Default)]
struct Latest {
    frame: Option<Frame>,
    /// Frames delivered so far.
    count: u64,
    failed: Option<String>,
}

struct Shared {
    latest: Mutex<Latest>,
    arrived: Condvar,
}

pub struct PipeWireCapture {
    shared: Arc<Shared>,
    /// The count `grab` last handed out.
    seen: u64,
    quit: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl PipeWireCapture {
    /// Receive stream `node` over the PipeWire connection `fd`.
    pub fn new(fd: OwnedFd, node: u32) -> Result<PipeWireCapture, CaptureError> {
        let shared = Arc::new(Shared {
            latest: Mutex::new(Latest::default()),
            arrived: Condvar::new(),
        });
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let thread = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("pipewire".into())
                .spawn(move || {
                    if let Err(e) = run(fd, node, shared.clone(), quit_rx, &ready_tx) {
                        tracing::warn!(error = %e, "screen cast stopped");
                        let _ = ready_tx.try_send(Err(e.clone()));
                        shared.latest.lock().failed = Some(e);
                        shared.arrived.notify_all();
                    }
                })
                .map_err(|e| CaptureError::Platform(e.to_string()))?
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(PipeWireCapture {
                shared,
                seen: 0,
                quit: Some(quit_tx),
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(CaptureError::Platform(format!("PipeWire: {e}"))),
            Err(_) => Err(CaptureError::Platform(
                "PipeWire did not connect within 5 s".into(),
            )),
        }
    }

    pub fn image(&self) -> Option<Frame> {
        self.shared.latest.lock().frame.clone()
    }

    /// Wait up to `timeout_ms` for a frame newer than the last one grabbed.
    pub fn grab(&mut self, timeout_ms: u32) -> Result<Grab, CaptureError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let mut latest = self.shared.latest.lock();
        loop {
            if let Some(e) = &latest.failed {
                return Err(CaptureError::Platform(e.clone()));
            }
            if latest.count != self.seen {
                self.seen = latest.count;
                return Ok(Grab::Frame);
            }
            if self
                .shared
                .arrived
                .wait_until(&mut latest, deadline)
                .timed_out()
            {
                return Ok(Grab::Timeout);
            }
        }
    }
}

impl Drop for PipeWireCapture {
    fn drop(&mut self) {
        if let Some(q) = self.quit.take() {
            let _ = q.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Stream {
    shared: Arc<Shared>,
    format: spa::param::video::VideoInfoRaw,
    buffers: Vec<Arc<Vec<u8>>>,
    next: usize,
}

impl Stream {
    fn frame(&mut self, data: &[u8], stride: usize) {
        let (w, h) = (self.format.size().width, self.format.size().height);
        let order = match self.format.format() {
            VideoFormat::RGBx | VideoFormat::RGBA => PixelOrder::Rgbx,
            _ => PixelOrder::Bgrx,
        };
        let row = w as usize * 4;
        if w == 0 || h == 0 || stride < row || data.len() < stride * (h as usize - 1) + row {
            return;
        }
        // A buffer no frame holds any more (the encoder is done with it).
        let Some(i) = (0..BUFFERS)
            .map(|k| (self.next + k) % BUFFERS)
            .find(|&k| Arc::get_mut(&mut self.buffers[k]).is_some())
        else {
            return; // all three still held: drop this frame
        };
        self.next = (i + 1) % BUFFERS;
        let buf = Arc::get_mut(&mut self.buffers[i]).unwrap();
        buf.resize(row * h as usize, 0);
        for y in 0..h as usize {
            buf[y * row..(y + 1) * row].copy_from_slice(&data[y * stride..y * stride + row]);
        }
        let pixels: Arc<dyn Pixels> = self.buffers[i].clone();
        let mut latest = self.shared.latest.lock();
        latest.frame = Some(Frame {
            pixels,
            width: w,
            height: h,
            stride: row,
            order,
        });
        latest.count += 1;
        drop(latest);
        self.shared.arrived.notify_all();
    }
}

fn run(
    fd: OwnedFd,
    node: u32,
    shared: Arc<Shared>,
    quit: pw::channel::Receiver<()>,
    ready: &std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_fd_rc(fd, None).map_err(|e| e.to_string())?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "pong-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;
    let data = Stream {
        shared,
        format: Default::default(),
        buffers: (0..BUFFERS).map(|_| Arc::new(Vec::new())).collect(),
        next: 0,
    };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _, old, new| tracing::debug!(?old, ?new, "screen cast stream"))
        .param_changed(|_, s, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            if s.format.parse(param).is_ok() {
                tracing::info!(
                    format = ?s.format.format(),
                    width = s.format.size().width,
                    height = s.format.size().height,
                    "screen cast format"
                );
            }
        })
        .process(|stream, s| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(d) = datas.first_mut() else { return };
            let (offset, size, stride) = (
                d.chunk().offset() as usize,
                d.chunk().size() as usize,
                d.chunk().stride(),
            );
            if size == 0 || stride <= 0 {
                return; // nothing new in this one (a cursor-only update)
            }
            if let Some(bytes) = d.data() {
                let end = (offset + size).min(bytes.len());
                if offset < end {
                    s.frame(&bytes[offset..end], stride as usize);
                }
            }
        })
        .register()
        .map_err(|e| e.to_string())?;

    // 4 bytes a pixel, whichever way round; any size and rate the desktop has.
    let format = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 8192,
                height: 8192
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(format),
    )
    .map_err(|e| format!("{e:?}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&bytes).ok_or("the format did not serialise")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| e.to_string())?;

    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}
