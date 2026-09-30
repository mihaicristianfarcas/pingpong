//! Linux: the X11 screen. MIT-SHM puts each image in memory shared with the
//! X server (no copy through the socket), XDamage says when the screen
//! changed, and XFixes gives the pointer, drawn into the picture: the X
//! server keeps it out of the framebuffer.
//!
//! Images are BGRX at the screen's size; the encoder scales them to the
//! stream's. Three segments rotate, so the one being encoded is never the one
//! the next grab writes.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use crate::image::{Frame, PixelOrder, Pixels};
use crate::{CaptureError, Grab};

const SEGMENTS: usize = 3;
/// The pointer is not in the damage X reports: its position is asked for,
/// at most this often.
const POINTER_EVERY: Duration = Duration::from_millis(4);

fn platform(what: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Platform(format!("{what}: {e}"))
}

/// Memory shared with the X server, mapped here.
struct Segment {
    xid: u32,
    ptr: *mut u8,
    len: usize,
}

// SAFETY: plain bytes; written only through `X11Capture` while no `Frame`
// holds the segment (see `free_segment`).
unsafe impl Send for Segment {}
unsafe impl Sync for Segment {}

impl Drop for Segment {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

impl Pixels for Segment {
    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

struct Pointer {
    serial: u32,
    width: usize,
    height: usize,
    hot: (i32, i32),
    /// Premultiplied ARGB, as XFixes gives it.
    pixels: Vec<u32>,
}

pub struct X11Capture {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    segments: Vec<Arc<Segment>>,
    current: Option<Frame>,
    damage: u32,
    /// Damage reported since the last image.
    dirty: bool,
    cursor: bool,
    pointer: Option<Pointer>,
    /// Where the pointer was drawn in the last image.
    drawn_at: (i16, i16),
    pointer_checked: Instant,
}

impl X11Capture {
    /// The screen of `display` (None: `$DISPLAY`), with the pointer drawn in
    /// when `cursor`.
    pub fn new(display: Option<&str>, cursor: bool) -> Result<X11Capture, CaptureError> {
        let (conn, screen) = x11rb::connect(display).map_err(|e| platform("no X display", e))?;
        let s = &conn.setup().roots[screen];
        let (root, width, height) = (s.root, s.width_in_pixels, s.height_in_pixels);
        if s.root_depth != 24 && s.root_depth != 32 {
            return Err(CaptureError::Platform(format!(
                "an X screen {} bits deep (24 or 32 needed)",
                s.root_depth
            )));
        }
        for (name, ext) in [
            ("MIT-SHM", "MIT-SHM"),
            ("DAMAGE", "DAMAGE"),
            ("XFIXES", "XFIXES"),
        ] {
            if conn.extension_information(ext).ok().flatten().is_none() {
                return Err(CaptureError::Platform(format!(
                    "the X server has no {name}"
                )));
            }
        }
        let shm = conn
            .shm_query_version()
            .map_err(|e| platform("MIT-SHM", e))?
            .reply()
            .map_err(|e| platform("MIT-SHM", e))?;
        if (shm.major_version, shm.minor_version) < (1, 2) {
            return Err(CaptureError::Platform(
                "MIT-SHM 1.2 (file descriptors) needed".into(),
            ));
        }
        conn.damage_query_version(1, 1)
            .map_err(|e| platform("DAMAGE", e))?
            .reply()
            .map_err(|e| platform("DAMAGE", e))?;
        conn.xfixes_query_version(4, 0)
            .map_err(|e| platform("XFIXES", e))?
            .reply()
            .map_err(|e| platform("XFIXES", e))?;

        let len = width as usize * height as usize * 4;
        let mut segments = Vec::with_capacity(SEGMENTS);
        for _ in 0..SEGMENTS {
            segments.push(Arc::new(attach(&conn, len)?));
        }
        let damage = conn.generate_id().map_err(|e| platform("DAMAGE", e))?;
        conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY)
            .map_err(|e| platform("DAMAGE", e))?;
        conn.flush().map_err(|e| platform("X", e))?;
        tracing::info!(width, height, "capturing the X screen");
        Ok(X11Capture {
            conn,
            root,
            width,
            height,
            segments,
            current: None,
            damage,
            dirty: true,
            cursor,
            pointer: None,
            drawn_at: (i16::MIN, i16::MIN),
            pointer_checked: Instant::now(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    /// The newest image, if there is one.
    pub fn image(&self) -> Option<Frame> {
        self.current.clone()
    }

    /// Wait up to `timeout_ms` for the screen (or the pointer) to change, and
    /// take a new image if it did.
    pub fn grab(&mut self, timeout_ms: u32) -> Result<Grab, CaptureError> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            self.drain_events()?;
            if self.dirty || self.pointer_moved()? {
                self.take()?;
                return Ok(Grab::Frame);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Grab::Timeout);
            }
            // Until the X server says something, or it is time to look at the
            // pointer again.
            let wait = if self.cursor {
                left.min(POINTER_EVERY)
            } else {
                left
            };
            let mut fds = libc::pollfd {
                fd: self.conn.stream().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe {
                libc::poll(&mut fds, 1, wait.as_millis().max(1) as i32);
            }
        }
    }

    fn drain_events(&mut self) -> Result<(), CaptureError> {
        while let Some(ev) = self
            .conn
            .poll_for_event()
            .map_err(|e| platform("X connection lost", e))?
        {
            if let Event::DamageNotify(_) = ev {
                self.dirty = true;
            }
        }
        Ok(())
    }

    fn pointer_moved(&mut self) -> Result<bool, CaptureError> {
        if !self.cursor || self.pointer_checked.elapsed() < POINTER_EVERY {
            return Ok(false);
        }
        self.pointer_checked = Instant::now();
        let p = self
            .conn
            .query_pointer(self.root)
            .map_err(|e| platform("X", e))?
            .reply()
            .map_err(|e| platform("X", e))?;
        Ok((p.root_x, p.root_y) != self.drawn_at)
    }

    /// A segment no frame holds: the next image goes there.
    fn free_segment(&mut self) -> Option<&mut Arc<Segment>> {
        self.segments.iter_mut().find(|s| Arc::strong_count(s) == 1)
    }

    fn take(&mut self) -> Result<(), CaptureError> {
        // What changes from here on is reported again.
        self.conn
            .damage_subtract(self.damage, x11rb::NONE, x11rb::NONE)
            .map_err(|e| platform("DAMAGE", e))?;
        self.dirty = false;
        let (root, w, h) = (self.root, self.width, self.height);
        let Some(seg) = self.free_segment() else {
            // All three still held (the encoder is far behind): keep the last.
            return Ok(());
        };
        let xid = seg.xid;
        let seg = seg.clone();
        self.conn
            .shm_get_image(root, 0, 0, w, h, !0, ImageFormat::Z_PIXMAP.into(), xid, 0)
            .map_err(|e| platform("GetImage", e))?
            .reply()
            .map_err(|e| platform("GetImage", e))?;
        if self.cursor {
            self.draw_pointer(&seg)?;
        }
        self.current = Some(Frame {
            pixels: seg,
            width: w as u32,
            height: h as u32,
            stride: w as usize * 4,
            order: PixelOrder::Bgrx,
        });
        Ok(())
    }

    fn draw_pointer(&mut self, seg: &Segment) -> Result<(), CaptureError> {
        let c = self
            .conn
            .xfixes_get_cursor_image()
            .map_err(|e| platform("XFIXES", e))?
            .reply()
            .map_err(|e| platform("XFIXES", e))?;
        self.drawn_at = (c.x, c.y);
        if self
            .pointer
            .as_ref()
            .is_none_or(|p| p.serial != c.cursor_serial)
        {
            self.pointer = Some(Pointer {
                serial: c.cursor_serial,
                width: c.width as usize,
                height: c.height as usize,
                hot: (c.xhot as i32, c.yhot as i32),
                pixels: c.cursor_image,
            });
        }
        let p = self.pointer.as_ref().unwrap();
        let (w, h) = (self.width as i32, self.height as i32);
        let (x0, y0) = (c.x as i32 - p.hot.0, c.y as i32 - p.hot.1);
        // SAFETY: no frame holds this segment (it was free for this image).
        let px = unsafe { std::slice::from_raw_parts_mut(seg.ptr.cast::<u32>(), seg.len / 4) };
        for row in 0..p.height as i32 {
            let y = y0 + row;
            if y < 0 || y >= h {
                continue;
            }
            for col in 0..p.width as i32 {
                let x = x0 + col;
                if x < 0 || x >= w {
                    continue;
                }
                let src = p.pixels[row as usize * p.width + col as usize];
                let a = src >> 24;
                if a == 0 {
                    continue;
                }
                let dst = &mut px[(y * w + x) as usize];
                *dst = over(src, *dst, a);
            }
        }
        Ok(())
    }
}

/// Premultiplied `src` over `dst` (both 0xAARRGGBB in memory order BGRA).
fn over(src: u32, dst: u32, a: u32) -> u32 {
    let inv = 255 - a;
    let ch = |shift: u32| {
        let s = (src >> shift) & 0xFF;
        let d = (dst >> shift) & 0xFF;
        ((s + (d * inv + 127) / 255).min(255)) << shift
    };
    ch(0) | ch(8) | ch(16) | 0xFF00_0000
}

impl Drop for X11Capture {
    fn drop(&mut self) {
        let _ = self.conn.damage_destroy(self.damage);
        for s in &self.segments {
            let _ = self.conn.shm_detach(s.xid);
        }
        let _ = self.conn.flush();
    }
}

/// A shared memory segment of `len` bytes, attached to the X server.
fn attach(conn: &RustConnection, len: usize) -> Result<Segment, CaptureError> {
    unsafe {
        let fd = libc::memfd_create(c"pingpong-capture".as_ptr(), libc::MFD_CLOEXEC);
        if fd < 0 {
            return Err(platform("memfd_create", std::io::Error::last_os_error()));
        }
        let fd = OwnedFd::from_raw_fd(fd);
        if libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) != 0 {
            return Err(platform("ftruncate", std::io::Error::last_os_error()));
        }
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(platform("mmap", std::io::Error::last_os_error()));
        }
        // Unmapped again (by its Drop) if attaching fails.
        let mut seg = Segment {
            xid: 0,
            ptr: ptr.cast(),
            len,
        };
        seg.xid = conn.generate_id().map_err(|e| platform("MIT-SHM", e))?;
        // The server maps it from the descriptor, which it is handed.
        conn.shm_attach_fd(seg.xid, fd, false)
            .map_err(|e| platform("MIT-SHM attach", e))?
            .check()
            .map_err(|e| platform("MIT-SHM attach", e))?;
        Ok(seg)
    }
}

#[cfg(test)]
mod tests {
    use super::over;

    #[test]
    fn premultiplied_over() {
        assert_eq!(over(0xFF11_2233, 0xFF44_5566, 0xFF), 0xFF11_2233, "opaque");
        assert_eq!(
            over(0x8000_0000, 0xFFFF_FFFF, 0x80),
            0xFF7F_7F7F,
            "half black over white"
        );
        assert_eq!(over(0x0000_0000, 0xFF12_3456, 0x00), 0xFF12_3456, "clear");
    }
}
