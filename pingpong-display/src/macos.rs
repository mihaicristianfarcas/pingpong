//! macOS: a virtual display at the client's mode, as SudoVDA gives the
//! Windows host one.
//!
//! macOS has no public API for this. CoreGraphics' `CGVirtualDisplay` classes
//! (private, used by DeskPad and BetterDisplay; present through macOS 26) make
//! a display that the window server, ScreenCaptureKit and System Settings
//! treat like a plugged-in monitor. It lives exactly as long as the object:
//! dropping it unplugs the display.
//!
//! At Retina-class sizes the mode is HiDPI at the client's pixels (points =
//! pixels / 2), so a Retina client sees the host's interface at its own
//! Retina scale rather than at a quarter of the size; smaller sizes are one
//! point per pixel, as a monitor of that size would be. It also gives a Mac without a display of
//! its own (lid closed, a headless mini) something to capture at all.

use std::ffi::CStr;

use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CFType, CGSize};
use objc2_core_graphics::{
    kCGDisplayShowDuplicateLowResolutionModes, CGBeginDisplayConfiguration,
    CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
    CGConfigureDisplayMirrorOfDisplay, CGConfigureDisplayOrigin, CGConfigureOption,
    CGDisplayBounds, CGDisplayConfigRef, CGDisplayCopyAllDisplayModes, CGDisplayCopyDisplayMode,
    CGDisplayIsActive, CGDisplayIsOnline, CGDisplayMirrorsDisplay, CGDisplayMode,
    CGDisplaySetDisplayMode, CGGetOnlineDisplayList, CGRestorePermanentDisplayConfiguration,
};
use objc2_foundation::{NSArray, NSString};

use crate::{DisplayError, DisplayMode};

/// Widths from which the display is HiDPI (Retina-class clients).
const HIDPI_FROM_WIDTH: u32 = 2560;

/// The virtual panel's size in pixels, whatever the session's mode (see
/// `VirtualDisplay::new`); larger sessions get a larger panel.
const PANEL: (u32, u32) = (3840, 2400);

/// How long a display just online gets to offer the session's mode.
const MODE_WITHIN: std::time::Duration = std::time::Duration::from_millis(1500);

/// `CGVirtualDisplayMode`'s transfer function for an HDR display.
const HDR_TRANSFER_FUNCTION: u32 = 1;

/// How long macOS gets to bring a new virtual display online.
const ONLINE_WITHIN: std::time::Duration = std::time::Duration::from_secs(2);

/// A virtual display, for as long as this lives.
pub struct VirtualDisplay {
    display: Option<Retained<AnyObject>>,
    _queue: DispatchRetained<DispatchQueue>,
    pub id: u32,
    pub mode: DisplayMode,
    /// The arrangement was changed for the session: put the user's back.
    rearranged: bool,
}

// SAFETY: the object is only touched through &self/&mut self and dropped
// once; CGVirtualDisplay is not thread-affine (it takes its own queue).
unsafe impl Send for VirtualDisplay {}

/// Put display `id` in the mode of `w`x`h` pixels (at 2x if `hidpi`),
/// nearest `hz`, unless it already is. False if it has no such mode.
fn select_mode(id: u32, w: u32, h: u32, hidpi: bool, hz: f64) -> bool {
    let (pw, ph) = if hidpi { (w / 2, h / 2) } else { (w, h) };
    let fits = |m: &CGDisplayMode| {
        CGDisplayMode::pixel_width(Some(m)) == w as usize
            && CGDisplayMode::pixel_height(Some(m)) == h as usize
            && CGDisplayMode::width(Some(m)) == pw as usize
            && CGDisplayMode::height(Some(m)) == ph as usize
    };
    if CGDisplayCopyDisplayMode(id).is_some_and(|m| fits(&m)) {
        return true;
    }
    let yes: &CFType = CFBoolean::new(true);
    let options = CFDictionary::<CFString, CFType>::from_slices(
        &[unsafe { kCGDisplayShowDuplicateLowResolutionModes }],
        &[yes],
    );
    let Some(modes) = (unsafe { CGDisplayCopyAllDisplayModes(id, Some(options.as_opaque())) })
    else {
        return false;
    };
    let best = (0..modes.count())
        .filter_map(|i| unsafe { (modes.value_at_index(i) as *const CGDisplayMode).as_ref() })
        .filter(|m| fits(m))
        .min_by_key(|m| ((CGDisplayMode::refresh_rate(Some(m)) - hz).abs() * 1000.0) as i64);
    match best {
        Some(m) => unsafe { CGDisplaySetDisplayMode(id, Some(m), None) }.0 == 0,
        None => false,
    }
}

fn class(name: &CStr) -> Result<&'static AnyClass, DisplayError> {
    AnyClass::get(name).ok_or(DisplayError::VddUnavailable)
}

impl VirtualDisplay {
    /// Plug in a display of `mode` (pixels; HiDPI), named `name`; an HDR one
    /// with `hdr`.
    pub fn new(mode: DisplayMode, name: &str, hdr: bool) -> Result<VirtualDisplay, DisplayError> {
        let (w, h) = (mode.width as u32 & !1, mode.height as u32 & !1);
        let hz = mode.refresh_mhz as f64 / 1000.0;
        let queue = DispatchQueue::new("pong.virtual-display", None);
        unsafe {
            let descriptor: Retained<AnyObject> =
                msg_send![class(c"CGVirtualDisplayDescriptor")?, new];
            let _: () = msg_send![&*descriptor, setQueue: &*queue];
            let _: () = msg_send![&*descriptor, setName: &*NSString::from_str(name)];
            // The same panel every session, whatever mode it is then put in:
            // macOS holds a display it has not seen before offline until
            // someone answers "What do you want to show on Pong?", which on
            // a host nobody sits at never happens. Sized to the session, each
            // new size was such a display (1280x800 waited 35 s for the
            // answer); one fixed panel is asked about at most once.
            let (max_w, max_h) = (w.max(PANEL.0), h.max(PANEL.1));
            let _: () = msg_send![&*descriptor, setMaxPixelsWide: max_w];
            let _: () = msg_send![&*descriptor, setMaxPixelsHigh: max_h];
            // ~220 pixels per inch, a Retina panel's density.
            let mm = CGSize::new(max_w as f64 * 25.4 / 220.0, max_h as f64 * 25.4 / 220.0);
            let _: () = msg_send![&*descriptor, setSizeInMillimeters: mm];
            // A stable identity, so macOS remembers the display's arrangement.
            let _: () = msg_send![&*descriptor, setVendorID: 0x5050u32];
            let _: () = msg_send![&*descriptor, setProductID: 0x0001u32];
            let _: () = msg_send![&*descriptor, setSerialNum: 0x504F_4E47u32];

            let alloc: Allocated<AnyObject> = msg_send![class(c"CGVirtualDisplay")?, alloc];
            let display: Option<Retained<AnyObject>> =
                msg_send![alloc, initWithDescriptor: &*descriptor];
            let display = display.ok_or_else(|| {
                DisplayError::Os("CGVirtualDisplay refused the descriptor".into())
            })?;

            // Retina-class sizes at twice the pixels per point, as the
            // client's own Mac shows them; smaller ones one to one, as a
            // 1080p monitor would (at 2x, 1920x1080 is a 960x540 desktop).
            let hidpi = w >= HIDPI_FROM_WIDTH;
            let (mw, mh) = if hidpi { (w / 2, h / 2) } else { (w, h) };
            let mode_alloc: Allocated<AnyObject> =
                msg_send![class(c"CGVirtualDisplayMode")?, alloc];
            // A mode's transfer function makes the display HDR: 1 (measured
            // on macOS 26: the screen then has EDR headroom, 5x SDR white;
            // 0 is SDR, 2 and 3 SDR too). The HDR display's pictures are
            // what an HDR capture of it carries.
            let vmode: Retained<AnyObject> = if hdr {
                msg_send![mode_alloc, initWithWidth: mw, height: mh, refreshRate: hz,
                    transferFunction: HDR_TRANSFER_FUNCTION]
            } else {
                msg_send![mode_alloc, initWithWidth: mw, height: mh, refreshRate: hz]
            };
            let settings: Retained<AnyObject> = msg_send![class(c"CGVirtualDisplaySettings")?, new];
            let _: () = msg_send![&*settings, setHiDPI: hidpi as u32];
            let modes = NSArray::from_retained_slice(&[vmode]);
            let _: () = msg_send![&*settings, setModes: &*modes];
            let applied: Bool = msg_send![&*display, applySettings: &*settings];
            let id: u32 = msg_send![&*display, displayID];
            if !applied.as_bool() || id == 0 {
                return Err(DisplayError::ModeRejected);
            }
            // macOS does not always bring it up: seen with the lid closed and
            // the built-in panel kept awake, the display stayed offline (and
            // uncapturable) whatever the queue or run loop. The caller then
            // streams another display rather than none.
            let deadline = std::time::Instant::now() + ONLINE_WITHIN;
            while !(CGDisplayIsOnline(id) && CGDisplayIsActive(id)) {
                if std::time::Instant::now() > deadline {
                    return Err(DisplayError::Os(format!(
                        "macOS did not bring virtual display {id} online (online={} \
                            active={} mirrors={})",
                        CGDisplayIsOnline(id),
                        CGDisplayIsActive(id),
                        CGDisplayMirrorsDisplay(id)
                    )));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            // macOS remembers a mode per display identity (ours is the same
            // every session) and applies that one: a 1920x1080 display came up
            // at 960x540 points because an earlier session had it at 2x.
            let vd = VirtualDisplay {
                display: Some(display),
                _queue: queue,
                id,
                mode: DisplayMode {
                    width: w as u16,
                    height: h as u16,
                    ..mode
                },
                rearranged: false,
            };
            // Briefly: arranging it (`make_main`) is when the modes show up
            // on a panel macOS has seen before, and that settles it again.
            vd.settle_mode(std::time::Duration::from_millis(300));
            Ok(vd)
        }
    }
}

impl VirtualDisplay {
    /// Make this the main display (menu bar, Dock, new windows) for the
    /// session: the virtual display is the desktop, as Sunshine's "deactivate other
    /// displays" makes the streamed one (`ensure_only_display`). With
    /// `mirror`, the Mac's own displays show it too, so no window is left
    /// where the client cannot see it; without, they extend it to the right.
    /// Undone when the display is unplugged.
    pub fn make_main(&mut self, mirror: bool) -> Result<(), DisplayError> {
        // Just plugged in, the display is sometimes not ready to be arranged
        // (CGCompleteDisplayConfiguration fails with 1001): try again shortly.
        let mut last = Err(DisplayError::NoSuchDisplay);
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            last = self.arrange(mirror);
            if last.is_ok() {
                self.rearranged = true;
                break;
            }
        }
        // Arranged, macOS may have applied the mode it remembers for this
        // panel (the last session's).
        if !self.settle_mode(MODE_WITHIN) {
            let DisplayMode { width, height, .. } = self.mode;
            tracing::warn!(
                id = self.id,
                width,
                height,
                "could not put the virtual display in the stream's mode"
            );
        }
        last
    }

    /// Put the display in the session's mode, waiting up to `within` for
    /// macOS to offer it: a display just online, or just rearranged, lists
    /// no modes for a while (seen for over 1.5 s).
    fn settle_mode(&self, within: std::time::Duration) -> bool {
        let (w, h) = (self.mode.width as u32, self.mode.height as u32);
        let hidpi = w >= HIDPI_FROM_WIDTH;
        let hz = self.mode.refresh_mhz as f64 / 1000.0;
        let deadline = std::time::Instant::now() + within;
        while !select_mode(self.id, w, h, hidpi, hz) {
            if std::time::Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        true
    }

    fn arrange(&self, mirror: bool) -> Result<(), DisplayError> {
        let mut ids = [0u32; 16];
        let mut n = 0u32;
        let others: Vec<u32> = unsafe {
            if CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut n).0 != 0 {
                return Err(DisplayError::Os("listing displays failed".into()));
            }
            ids[..n as usize]
                .iter()
                .copied()
                .filter(|&d| d != self.id)
                .collect()
        };
        let mut config: CGDisplayConfigRef = std::ptr::null_mut();
        unsafe {
            if CGBeginDisplayConfiguration(&mut config).0 != 0 {
                return Err(DisplayError::Os(
                    "CGBeginDisplayConfiguration failed".into(),
                ));
            }
            let mut ok = CGConfigureDisplayOrigin(config, self.id, 0, 0).0 == 0;
            let mut x = CGDisplayBounds(self.id).size.width as i32;
            for &d in &others {
                ok &= if mirror {
                    CGConfigureDisplayMirrorOfDisplay(config, d, self.id).0 == 0
                } else {
                    let placed = CGConfigureDisplayOrigin(config, d, x, 0).0 == 0;
                    x += CGDisplayBounds(d).size.width as i32;
                    placed
                };
            }
            if !ok {
                CGCancelDisplayConfiguration(config);
                return Err(DisplayError::Os("arranging the displays failed".into()));
            }
            let rc = CGCompleteDisplayConfiguration(config, CGConfigureOption::ForSession);
            if rc.0 != 0 {
                return Err(DisplayError::Os(format!(
                    "CGCompleteDisplayConfiguration returned {}",
                    rc.0
                )));
            }
        }
        Ok(())
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        if self.rearranged {
            // The arrangement saved in System Settings, as it was.
            CGRestorePermanentDisplayConfiguration();
        }
        // Releasing the object unplugs the display.
        self.display = None;
    }
}
