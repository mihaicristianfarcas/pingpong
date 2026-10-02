//! The stream window (main thread): a window of its own that goes full
//! screen into a Space of its own, as Moonlight's does (Ping's window stays on
//! the desktop, a swipe or Ctrl+←/→ away), with a layer-hosting view with a
//! CAMetalLayer, and all keyboard and mouse input.
//!
//! Feels like Moonlight's fullscreen stream:
//! - in its Space the menu bar and Dock are hidden outright (not
//!   auto-hidden), so nothing drops down when the pointer reaches the top
//!   edge;
//! - the system pointer is captured and hidden while streaming, so hot corners
//!   (Quick Note, Mission Control) and screen edges never trigger. The host
//!   draws its pointer into the picture and gets relative motion (Moonlight's
//!   default), or positions after +M; an older host gets positions on its
//!   desktop, with the pointer drawn here in the shape it reports, and
//!   relative motion while a game has the mouse;
//! - the picture fills the area below the notch, at the nearest standard
//!   aspect ratio (`aspect`): exactly at the Mac's default scaling, with 5
//!   rows of black above and below in a scaled desktop;
//! - Ctrl+Option+Shift+Q quits, +S toggles statistics, +Z releases/recaptures
//!   the mouse, +M switches mouse mode, +X full screen, +V types the
//!   clipboard, +D minimises (Moonlight's chords, Option standing for Alt).

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationPresentationOptions, NSBackingStoreType, NSColor, NSCursor,
    NSEvent, NSEventModifierFlags, NSResponder, NSScreen, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGAssociateMouseAndMouseCursorPosition, CGWarpMouseCursorPosition};
use objc2_foundation::{NSNotification, NSObject, NSPoint, NSRect, NSSize};
use objc2_quartz_core::CAMetalLayer;
use parking_lot::Mutex;
use pingpong_proto::input::{scancode, Button, InputEvent};

use super::render::{Layout, RenderShared};
use crate::input::InputSender;
use crate::keymap;
pub use crate::pointer::PointerState;

/// What the view needs, owned by the main thread.
pub struct Handler {
    pub input: InputSender,
    pub render: Arc<RenderShared>,
    pub pointer: Arc<Mutex<PointerState>>,
    /// Forward Command keys: as the Windows key when the user asks for it,
    /// and always to a Mac host (set when the session says what it is).
    pub forward_command: Arc<std::sync::atomic::AtomicBool>,
    /// Stream pixels per point of pointer motion.
    pub pixels_per_point: f64,
    pub on_quit: Box<dyn Fn()>,
    /// The stream window gained (true) or lost (false) focus.
    pub on_focus: Option<Box<dyn Fn(bool)>>,
    /// Ctrl+Option+Shift+T, watching an AI agent: take over or hand back.
    pub on_take_over: Option<Box<dyn Fn()>>,
    held_keys: HashSet<u16>,
    held_buttons: HashSet<u8>,
    ctrl: bool,
    alt: bool,
    shift: bool,
    scroll_residue: (f64, f64),
    /// Keys swallowed as the last key of a hotkey chord: their key-up must not
    /// be forwarded either.
    swallowed: HashSet<u16>,
}

impl Handler {
    pub fn new(
        input: InputSender,
        render: Arc<RenderShared>,
        pointer: Arc<Mutex<PointerState>>,
        forward_command: Arc<std::sync::atomic::AtomicBool>,
        on_quit: Box<dyn Fn()>,
    ) -> Handler {
        Handler {
            input,
            render,
            pointer,
            forward_command,
            pixels_per_point: 2.0,
            on_quit,
            on_focus: None,
            on_take_over: None,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            ctrl: false,
            alt: false,
            shift: false,
            scroll_residue: (0.0, 0.0),
            swallowed: HashSet::new(),
        }
    }

    fn captured(&self) -> bool {
        self.pointer.lock().captured
    }

    fn send_key(&mut self, code: u16, down: bool) {
        let forward_command = self
            .forward_command
            .load(std::sync::atomic::Ordering::Relaxed);
        let Some(key) = keymap::key_for(code, forward_command) else {
            return;
        };
        let sc = scancode(key);
        if down {
            if !self.held_keys.insert(sc) {
                return; // already down (auto-repeat is the host's job)
            }
            self.input.send(InputEvent::KeyDown(sc));
        } else if self.held_keys.remove(&sc) {
            self.input.send(InputEvent::KeyUp(sc));
        }
    }

    /// Release everything held on the host: on focus loss, capture release and
    /// quit, so nothing stays stuck down.
    pub fn release_all(&mut self) {
        for sc in self.held_keys.drain() {
            self.input.send(InputEvent::KeyUp(sc));
        }
        for b in self.held_buttons.drain() {
            if let Some(b) = button(b) {
                self.input.send(InputEvent::ButtonUp(b));
            }
        }
        self.ctrl = false;
        self.alt = false;
        self.shift = false;
    }

    fn hotkey(&mut self, code: u16) -> bool {
        if !(self.ctrl && self.alt && self.shift) {
            return false;
        }
        match code {
            0x0C => {
                // Q
                self.release_all();
                (self.on_quit)();
            }
            0x01 => {
                // S
                let on = self.render.toggle_overlay();
                tracing::info!(on, "statistics overlay");
            }
            0x06 => {
                // Z
                let captured = !self.captured();
                set_capture(self, captured);
            }
            0x09 => {
                // V: type the Mac's clipboard on the host (Moonlight's).
                self.paste_clipboard();
            }
            0x2E => {
                // M: mouse mode, relative <-> desktop pointer (Moonlight's).
                let mut p = self.pointer.lock();
                let relative = p.toggle_mode();
                self.render.set_cursor(p.draw());
                tracing::info!(
                    relative,
                    automatic = p.mode_override.is_none(),
                    "mouse mode"
                );
            }
            0x07 => {
                // X: full screen <-> a window. After this event: the change
                // re-enters the view (focus, resize), whose handler is busy.
                dispatch2::DispatchQueue::main().exec_async(toggle_fullscreen);
            }
            0x11 => {
                // T: watching an agent, take over or hand back.
                if self.on_take_over.is_none() {
                    return false;
                }
                self.release_all();
                if let Some(f) = &self.on_take_over {
                    f();
                }
            }
            0x02 => {
                // D
                self.release_all();
                set_capture(self, false);
                if let Some(mtm) = MainThreadMarker::new() {
                    if let Some(w) = NSApplication::sharedApplication(mtm).keyWindow() {
                        w.miniaturize(None);
                    }
                }
            }
            _ => return false,
        }
        true
    }

    /// The Mac's clipboard, typed on the host.
    fn paste_clipboard(&mut self) {
        let Some(text) = clipboard_text() else { return };
        // Ctrl, Alt and Shift are still down on the host: lift them, or the
        // text would type as shortcuts.
        for sc in self.held_keys.drain() {
            self.input.send(InputEvent::KeyUp(sc));
        }
        let chars = self.input.type_text(&text);
        tracing::info!(chars, "clipboard typed on the host");
    }

    fn key(&mut self, event: &NSEvent, down: bool) {
        let code = event.keyCode();
        if down && event.isARepeat() {
            return;
        }
        if down && self.hotkey(code) {
            self.swallowed.insert(code);
            return;
        }
        if !down && self.swallowed.remove(&code) {
            return;
        }
        if !self.captured() {
            return;
        }
        self.send_key(code, down);
    }

    fn flags_changed(&mut self, event: &NSEvent) {
        let code = event.keyCode();
        let flags = event.modifierFlags();
        let Some(mask) = keymap::modifier_mask(code) else {
            return;
        };
        let down = flags.0 as u64 & mask != 0;
        match code {
            0x3B | 0x3E => self.ctrl = flags.contains(NSEventModifierFlags::Control),
            0x3A | 0x3D => self.alt = flags.contains(NSEventModifierFlags::Option),
            0x38 | 0x3C => self.shift = flags.contains(NSEventModifierFlags::Shift),
            _ => {}
        }
        if !self.captured() {
            return;
        }
        if code == 0x39 {
            // Caps Lock reports its latched state, not the key: tap it.
            self.send_key(code, true);
            self.send_key(code, false);
            return;
        }
        self.send_key(code, down);
    }

    fn motion(&mut self, event: &NSEvent) {
        let (dx, dy) = (event.deltaX(), event.deltaY());
        let mut p = self.pointer.lock();
        if !p.captured {
            return;
        }
        // Points scaled to pixels; in a game, raw-ish relative motion with
        // the residue kept by the input thread's batcher.
        let render = &self.render;
        p.nudge(
            dx * self.pixels_per_point,
            dy * self.pixels_per_point,
            &self.input,
            &|d| render.set_cursor(d),
        );
    }

    fn mouse_button(&mut self, number: isize, down: bool) {
        if !self.captured() {
            if down {
                // First click into an uncaptured window recaptures (Moonlight).
                set_capture(self, true);
            }
            return;
        }
        let n = number as u8;
        let Some(b) = button(n) else { return };
        if down {
            if self.held_buttons.insert(n) {
                self.input.send(InputEvent::ButtonDown(b));
            }
        } else if self.held_buttons.remove(&n) {
            self.input.send(InputEvent::ButtonUp(b));
        }
    }

    fn scroll(&mut self, event: &NSEvent) {
        if !self.captured() {
            return;
        }
        // Windows counts 120 per wheel notch. A wheel reports lines; a
        // trackpad reports points, of which ~40 make a notch's worth.
        let (per_unit_y, per_unit_x) = if event.hasPreciseScrollingDeltas() {
            (3.0, 3.0)
        } else {
            (120.0, 120.0)
        };
        let (rx, ry) = self.scroll_residue;
        let fy = ry + event.scrollingDeltaY() * per_unit_y;
        let fx = rx + event.scrollingDeltaX() * per_unit_x;
        let (dv, dh) = (fy.trunc(), fx.trunc());
        self.scroll_residue = (fx - dh, fy - dv);
        if dv != 0.0 || dh != 0.0 {
            self.input.send(InputEvent::Wheel {
                dv: dv.clamp(i16::MIN as f64, i16::MAX as f64) as i16,
                // Windows' horizontal wheel runs the other way.
                dh: (-dh).clamp(i16::MIN as f64, i16::MAX as f64) as i16,
            });
        }
    }
}

/// The text on the Mac's clipboard, if any.
pub fn clipboard_text() -> Option<String> {
    let text = unsafe {
        objc2_app_kit::NSPasteboard::generalPasteboard()
            .stringForType(objc2_app_kit::NSPasteboardTypeString)
    };
    text.map(|t| t.to_string())
}

fn button(n: u8) -> Option<Button> {
    Some(match n {
        0 => Button::Left,
        1 => Button::Right,
        2 => Button::Middle,
        3 => Button::X1,
        4 => Button::X2,
        _ => return None,
    })
}

/// Capture or release the system pointer.
pub fn set_capture(h: &mut Handler, captured: bool) {
    let mut p = h.pointer.lock();
    if p.captured == captured {
        return;
    }
    p.captured = captured;
    let draw = p.draw();
    drop(p);
    if captured {
        let _ = CGAssociateMouseAndMouseCursorPosition(false);
        NSCursor::hide();
    } else {
        h.release_all();
        let _ = CGAssociateMouseAndMouseCursorPosition(true);
        NSCursor::unhide();
    }
    h.render.set_cursor(draw);
    tracing::info!(captured, "pointer capture");
}

pub struct ViewIvars {
    pub handler: RefCell<Option<Handler>>,
    pub render: Arc<RenderShared>,
    top_inset_points: std::cell::Cell<f64>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PingStreamView"]
    #[ivars = ViewIvars]
    pub struct StreamView;

    unsafe impl NSObjectProtocol for StreamView {}

    impl StreamView {
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            self.with(|h| h.key(event, true));
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            self.with(|h| h.key(event, false));
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            self.with(|h| h.flags_changed(event));
        }

        // Cmd-combinations would otherwise go to the menu (Cmd+Q, Cmd+W...).
        // While streaming they belong to the host.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> bool {
            self.with(|h| h.key(event, true));
            true
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            self.with(|h| h.motion(event));
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            self.with(|h| h.motion(event));
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            self.with(|h| h.motion(event));
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            self.with(|h| h.motion(event));
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            self.with(|h| h.mouse_button(event.buttonNumber(), true));
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            self.with(|h| h.mouse_button(event.buttonNumber(), false));
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            self.with(|h| h.mouse_button(event.buttonNumber(), true));
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            self.with(|h| h.mouse_button(event.buttonNumber(), false));
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            self.with(|h| h.mouse_button(event.buttonNumber(), true));
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            self.with(|h| h.mouse_button(event.buttonNumber(), false));
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            self.with(|h| h.scroll(event));
        }

        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.publish_layout();
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            self.publish_layout();
        }
    }
);

impl StreamView {
    pub fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        render: Arc<RenderShared>,
        top_inset_points: f64,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ViewIvars {
            handler: RefCell::new(None),
            render,
            top_inset_points: std::cell::Cell::new(top_inset_points),
        });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    pub fn set_handler(&self, h: Handler) {
        *self.ivars().handler.borrow_mut() = Some(h);
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut Handler) -> R) -> Option<R> {
        let mut h = self.ivars().handler.try_borrow_mut().ok()?;
        h.as_mut().map(f)
    }

    fn scale(&self) -> f64 {
        self.window().map(|w| w.backingScaleFactor()).unwrap_or(2.0)
    }

    pub fn set_top_inset(&self, points: f64) {
        self.ivars().top_inset_points.set(points);
        self.publish_layout();
    }

    /// Tell the renderer the drawable's pixel size and the notch inset, and
    /// keep the layer's contents scale honest.
    pub fn publish_layout(&self) {
        let scale = self.scale();
        let size = self.bounds().size;
        if let Some(layer) = self.layer() {
            layer.setContentsScale(scale);
        }
        // Below the notch: a full-screen window normally sits below it
        // already, and says so here if it does not.
        let inset = self
            .ivars()
            .top_inset_points
            .get()
            .max(self.safeAreaInsets().top);
        self.ivars().render.set_layout(Layout {
            drawable_w: (size.width * scale).round(),
            drawable_h: (size.height * scale).round(),
            top_inset: (inset * scale).round(),
            // Full screen, the layer goes straight to the display; in a
            // window it is composited.
            windowed: self.window().is_some_and(|w| {
                let style = w.styleMask();
                style.contains(NSWindowStyleMask::Titled)
                    && !style.contains(NSWindowStyleMask::FullScreen)
            }),
        });
    }
}

// ---------------------------------------------------------------------------

pub struct WindowIvars {
    pub view: Retained<StreamView>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PingWindowDelegate"]
    #[ivars = WindowIvars]
    pub struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowDidBecomeKey:))]
        fn window_did_become_key(&self, _n: &NSNotification) {
            self.ivars().view.with(|h| {
                set_capture(h, true);
                if let Some(f) = &h.on_focus {
                    f(true);
                }
            });
        }

        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _n: &NSNotification) {
            // Cmd+Tab away: never leave keys held on the host or the pointer
            // trapped in a window the user has left.
            self.ivars().view.with(|h| {
                set_capture(h, false);
                if let Some(f) = &h.on_focus {
                    f(false);
                }
            });
        }

        // In its full-screen Space, nothing drops down from the top edge.
        #[unsafe(method(window:willUseFullScreenPresentationOptions:))]
        fn window_will_use_full_screen_presentation_options(
            &self,
            _window: &NSWindow,
            _proposed: NSApplicationPresentationOptions,
        ) -> NSApplicationPresentationOptions {
            NSApplicationPresentationOptions::FullScreen
                | NSApplicationPresentationOptions::HideDock
                | NSApplicationPresentationOptions::HideMenuBar
        }

        // The request for full screen was taken: no retry needed.
        #[unsafe(method(windowWillEnterFullScreen:))]
        fn window_will_enter_full_screen(&self, _n: &NSNotification) {
            FULLSCREEN_WANTED.store(false, Ordering::Relaxed);
        }

        // Refused mid-way (another full-screen Space still leaving): again.
        #[unsafe(method(windowDidFailToEnterFullScreen:))]
        fn window_did_fail_to_enter_full_screen(&self, _w: &NSWindow) {
            tracing::info!("the window could not go full screen yet");
            FULLSCREEN_WANTED.store(true, Ordering::Relaxed);
            retry_fullscreen(FULLSCREEN_TRIES);
        }

        // The layout follows the window into and out of full screen.
        #[unsafe(method(windowDidEnterFullScreen:))]
        fn window_did_enter_full_screen(&self, _n: &NSNotification) {
            self.ivars().view.publish_layout();
            tracing::info!(fullscreen = true, "window mode");
        }

        #[unsafe(method(windowDidExitFullScreen:))]
        fn window_did_exit_full_screen(&self, _n: &NSNotification) {
            self.ivars().view.publish_layout();
            tracing::info!(fullscreen = false, "window mode");
        }

        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            self.ivars().view.with(|h| {
                h.release_all();
                (h.on_quit)();
            });
            false
        }

        // Closed some other way (the close button asks first, above): the
        // stream must not go on without its window. After the app closed it
        // itself this says so again, which it ignores.
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            self.ivars().view.with(|h| (h.on_quit)());
        }
    }
);

impl WindowDelegate {
    pub fn new(mtm: MainThreadMarker, view: Retained<StreamView>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(WindowIvars { view });
        unsafe { msg_send![super(this), init] }
    }
}

// A borderless window refuses key status by default; this one must take it.
define_class!(
    #[unsafe(super(NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PingStreamWindow"]
    pub struct StreamNSWindow;

    impl StreamNSWindow {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            true
        }
    }
);

/// Screen geometry that decides the stream mode.
#[derive(Debug, Clone, Copy)]
pub struct ScreenInfo {
    pub frame: NSRect,
    pub scale: f64,
    /// The notch: points at the top of the screen no picture should go under.
    pub top_inset: f64,
    /// The panel's own pixels (its native display mode). A scaled mode
    /// ("More Space") renders the desktop larger than this and shrinks it.
    pub panel: Option<(f64, f64)>,
}

impl ScreenInfo {
    pub fn main(mtm: MainThreadMarker) -> Option<ScreenInfo> {
        let screen = NSScreen::mainScreen(mtm)?;
        Some(ScreenInfo {
            frame: screen.frame(),
            scale: screen.backingScaleFactor(),
            top_inset: screen.safeAreaInsets().top,
            panel: panel_pixels(&screen),
        })
    }

    /// The panel's pixels below the notch: 3024x1890 on a 14" MacBook Pro
    /// at the default scaling, 3024x1900 in "More Space" (its menu bar is
    /// taller in panel pixels). `session::native_mode` fits it to a standard
    /// aspect ratio, which is Moonlight's 3024x1890 in both.
    pub fn native_mode(&self) -> (u16, u16) {
        let Some((pw, ph)) = self.panel else {
            return self.desktop_mode();
        };
        let notch = (self.top_inset * pw / self.frame.size.width).ceil();
        ((pw as u16) & !1, ((ph - notch) as u16) & !1)
    }

    /// The desktop's own pixels below the notch: what macOS renders before
    /// scaling to the panel (3600x2262 on that MacBook in "More Space").
    /// Sharper there than the panel size, at more pixels to stream.
    pub fn desktop_mode(&self) -> (u16, u16) {
        let w = (self.frame.size.width * self.scale).round() as u16;
        let h = ((self.frame.size.height - self.top_inset) * self.scale).round() as u16;
        (w & !1, h & !1)
    }
}

/// The pixel size of `screen`'s native display mode.
fn panel_pixels(screen: &NSScreen) -> Option<(f64, f64)> {
    use objc2_core_graphics::{CGDisplayCopyAllDisplayModes, CGDisplayMode};
    use objc2_foundation::{ns_string, NSNumber};
    /// kDisplayModeNativeFlag (IOGraphicsTypes.h).
    const NATIVE: u32 = 0x0200_0000;
    let number = screen
        .deviceDescription()
        .objectForKey(ns_string!("NSScreenNumber"))?;
    let id = number.downcast::<NSNumber>().ok()?.unsignedIntValue();
    let modes = unsafe { CGDisplayCopyAllDisplayModes(id, None) }?;
    (0..modes.count())
        .filter_map(|i| unsafe { (modes.value_at_index(i) as *const CGDisplayMode).as_ref() })
        .find(|m| CGDisplayMode::io_flags(Some(m)) & NATIVE != 0)
        .map(|m| {
            (
                CGDisplayMode::pixel_width(Some(m)) as f64,
                CGDisplayMode::pixel_height(Some(m)) as f64,
            )
        })
}

pub struct Window {
    pub window: Retained<NSWindow>,
    pub view: Retained<StreamView>,
    pub layer: Retained<CAMetalLayer>,
    _delegate: Retained<WindowDelegate>,
}

impl Window {
    pub fn open(
        mtm: MainThreadMarker,
        render: Arc<RenderShared>,
        fullscreen: bool,
        stream: (u32, u32),
        title: &str,
    ) -> Window {
        let info = ScreenInfo::main(mtm).unwrap_or(ScreenInfo {
            frame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(1512.0, 982.0)),
            scale: 2.0,
            top_inset: 0.0,
            panel: None,
        });
        // A window either way: full screen, it then goes into a Space of its
        // own (below).
        let w = (stream.0 as f64 / info.scale).min(info.frame.size.width * 0.9);
        let h = (stream.1 as f64 / info.scale).min(info.frame.size.height * 0.9);
        let rect = NSRect::new(
            NSPoint::new(
                info.frame.origin.x + (info.frame.size.width - w) / 2.0,
                info.frame.origin.y + (info.frame.size.height - h) / 2.0,
            ),
            NSSize::new(w, h),
        );
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let inset = 0.0;
        let window: Retained<StreamNSWindow> = unsafe {
            msg_send![StreamNSWindow::alloc(mtm), initWithContentRect: rect, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
        };
        let window: Retained<NSWindow> = window.into_super();
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&objc2_foundation::NSString::from_str(title));
        window.setBackgroundColor(Some(&NSColor::blackColor()));
        window.setOpaque(true);
        window.setAcceptsMouseMovedEvents(true);

        let content = NSRect::new(NSPoint::new(0.0, 0.0), rect.size);
        let view = StreamView::new(mtm, content, render.clone(), inset);
        let layer = CAMetalLayer::new();
        layer.setFrame(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(rect.size.width, rect.size.height),
        ));
        layer.setContentsScale(info.scale);
        // Opaque, and nothing else in the window: lets the compositor send the
        // layer straight to the display ("direct to display"), which saves a
        // whole refresh of latency against a composited layer.
        layer.setOpaque(true);
        // Layer-hosting: set the layer before wantsLayer.
        view.setLayer(Some(&layer));
        view.setWantsLayer(true);
        window.setContentView(Some(&view));

        let delegate = WindowDelegate::new(mtm, view.clone());
        window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

        window.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenPrimary);

        let app = NSApplication::sharedApplication(mtm);
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&view));
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
        if fullscreen {
            FULLSCREEN_WANTED.store(true, Ordering::Relaxed);
            window.toggleFullScreen(None);
            retry_fullscreen(FULLSCREEN_TRIES);
        }
        view.publish_layout();
        Window {
            window,
            view,
            layer,
            _delegate: delegate,
        }
    }

    pub fn close(&self) {
        FULLSCREEN_WANTED.store(false, Ordering::Relaxed);
        self.view.with(|h| set_capture(h, false));
        // Either way: Ctrl+Alt+Shift+X may have made a window full screen.
        if let Some(mtm) = MainThreadMarker::new() {
            NSApplication::sharedApplication(mtm)
                .setPresentationOptions(NSApplicationPresentationOptions::Default);
        }
        self.window.orderOut(None);
        self.window.close();
    }
}

/// A stream window asked for full screen and has not started into it (or
/// was refused on the way).
/// macOS ignores the request while another full-screen Space is still on its
/// way out (a stream started just as one ended), and the stream would then
/// run in a window: composited, a refresh or two later to the glass.
static FULLSCREEN_WANTED: AtomicBool = AtomicBool::new(false);
/// How often, 400 ms apart, the request is repeated.
const FULLSCREEN_TRIES: u32 = 10;

fn retry_fullscreen(tries: u32) {
    let Ok(when) = dispatch2::DispatchTime::try_from(Duration::from_millis(400)) else {
        return;
    };
    let _ = dispatch2::DispatchQueue::main().after(when, move || {
        if !FULLSCREEN_WANTED.load(Ordering::Relaxed) {
            return;
        }
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        for window in NSApplication::sharedApplication(mtm).windows().iter() {
            let stream = window
                .contentView()
                .and_then(|v| v.downcast::<StreamView>().ok())
                .is_some();
            if stream
                && window.isVisible()
                && !window.styleMask().contains(NSWindowStyleMask::FullScreen)
            {
                tracing::info!("asking for full screen again");
                window.toggleFullScreen(None);
            }
        }
        if tries > 1 {
            retry_fullscreen(tries - 1);
        } else {
            FULLSCREEN_WANTED.store(false, Ordering::Relaxed);
        }
    });
}

/// Moonlight's Ctrl+Alt+Shift+X: the key stream window into or out of full
/// screen (a Space of its own).
pub fn toggle_fullscreen() {
    // The user's choice from here on.
    FULLSCREEN_WANTED.store(false, Ordering::Relaxed);
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(window) = app.keyWindow() else {
        return;
    };
    if window
        .contentView()
        .and_then(|v| v.downcast::<StreamView>().ok())
        .is_none()
    {
        return;
    }
    window.toggleFullScreen(None);
}

/// Warp the (captured, hidden) system pointer to the window's centre, so it is
/// somewhere sensible when released.
pub fn centre_pointer(window: &NSWindow) {
    let f = window.frame();
    let _ = CGWarpMouseCursorPosition(CGPoint::new(
        f.origin.x + f.size.width / 2.0,
        f.origin.y + f.size.height / 2.0,
    ));
}
