//! A Mac host's accessibility tree, as screen text for its agent
//! (`pingpong_proto::screen`): AXUIElement, what VoiceOver reads. Pong holds
//! the Accessibility permission already, for the client's keyboard and mouse
//! (`permissions`); the same permission lets it read the tree.
//!
//! The front window is the frontmost ordinary window on the agent's display,
//! as the window server lists them (front to back), and its app the one that
//! owns it; the app's menu bar comes after the window. Asking the
//! system-wide element for its focused application instead fails
//! (`kAXErrorCannotComplete`) from a process that is not the one in front,
//! and would name an app on another display. Each element
//! costs one call into the app (`AXUIElementCopyMultipleAttributeValues`, all
//! the attributes at once) and a hung app costs at most `APP_TIMEOUT` a call.
//!
//! Frames lie, so the walk prunes: a subtree whose frame misses the display
//! (Notes reports rows screens below, Chrome parks scrolled-out nodes above
//! the viewport), and a menu with no size (a closed menu holds every item it
//! would show). A text field's value is never read: its label or placeholder
//! names it, and a password field is marked secret.

use std::ffi::c_void;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFString, CGPoint, CGRect, CGSize};
use pingpong_proto::screen::{flags, Element, Query, Role, ScreenText};

use crate::screen::{Geometry, MIN_SIDE};

type CFTypeRef = *const c_void;

#[repr(C)]
struct CFArrayCallBacks {
    version: isize,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFTypeRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementCopyMultipleAttributeValues(
        element: CFTypeRef,
        attributes: CFTypeRef,
        options: u32,
        values: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementCopyElementAtPosition(
        application: CFTypeRef,
        x: f32,
        y: f32,
        element: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, seconds: f32) -> i32;
    fn AXUIElementGetPid(element: CFTypeRef, pid: *mut i32) -> i32;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetValue(value: CFTypeRef, kind: u32, out: *mut c_void) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    static kCFTypeArrayCallBacks: CFArrayCallBacks;
    fn CFRelease(cf: CFTypeRef);
    fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFBooleanGetTypeID() -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFBooleanGetValue(b: CFTypeRef) -> u8;
    fn CFNumberGetValue(n: CFTypeRef, kind: isize, out: *mut c_void) -> bool;
    fn CFArrayCreate(
        allocator: CFTypeRef,
        values: *const CFTypeRef,
        count: isize,
        callbacks: *const CFArrayCallBacks,
    ) -> CFTypeRef;
    fn CFArrayGetCount(array: CFTypeRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: isize) -> CFTypeRef;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFTypeRef;
    fn CGRectMakeWithDictionaryRepresentation(dict: CFTypeRef, rect: *mut CGRect) -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
}

/// `kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements`.
const ON_SCREEN_WINDOWS: u32 = 1 | 1 << 4;

/// `AXValueType`s.
const AX_POINT: u32 = 1;
const AX_SIZE: u32 = 2;
/// `kCFNumberDoubleType`.
const CF_DOUBLE: isize = 13;

/// How long one call into an app may take: a hung app answers nothing, and
/// the default (6 s) would hold the agent's click that long.
const APP_TIMEOUT: f32 = 0.25;
/// The first message a process sends an app sets up the connection, which
/// takes longer: Finder answered it with `kAXErrorCannotComplete` at
/// `APP_TIMEOUT`, and the same read in the same process then took 6 ms
/// (M-series Mac, macOS 26). Later reads find the connection made.
const FIRST_CONTACT: f32 = 0.5;
/// A walk stops after visiting this many elements, or after this long, and
/// says so: Chrome's tree for a long page runs to tens of thousands, and the
/// agent's click waits on a read.
const MAX_VISITS: usize = 4000;
const MAX_TIME: Duration = Duration::from_millis(600);
/// Parents listed above the element at a point, at most.
const MAX_PARENTS: usize = 12;

/// An owned CoreFoundation reference (released when dropped).
struct Cf(CFTypeRef);

impl Cf {
    /// Take ownership of what a Create or Copy function returned.
    fn own(r: CFTypeRef) -> Option<Cf> {
        (!r.is_null()).then_some(Cf(r))
    }

    /// Keep something a Get function lent.
    fn retain(r: CFTypeRef) -> Option<Cf> {
        // SAFETY: `r` is a live CF object (checked non-null) lent by its
        // container, which outlives this call.
        (!r.is_null()).then(|| Cf(unsafe { CFRetain(r) }))
    }
}

impl Drop for Cf {
    fn drop(&mut self) {
        // SAFETY: every `Cf` owns exactly one reference (see `own`, `retain`).
        unsafe { CFRelease(self.0) }
    }
}

fn is(r: CFTypeRef, type_id: usize) -> bool {
    // SAFETY: `r` is a live CF object or null (checked).
    !r.is_null() && unsafe { CFGetTypeID(r) } == type_id
}

fn string(r: CFTypeRef) -> Option<String> {
    // SAFETY: the type is checked first; the CFString lives as long as `r`.
    is(r, unsafe { CFStringGetTypeID() })
        .then(|| unsafe { &*(r as *const CFString) }.to_string())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn boolean(r: CFTypeRef) -> Option<bool> {
    // SAFETY: the type is checked first.
    unsafe {
        if is(r, CFBooleanGetTypeID()) {
            Some(CFBooleanGetValue(r) != 0)
        } else if is(r, CFNumberGetTypeID()) {
            let mut v = 0f64;
            CFNumberGetValue(r, CF_DOUBLE, (&mut v as *mut f64).cast()).then_some(v != 0.0)
        } else {
            None
        }
    }
}

fn point(r: CFTypeRef) -> Option<CGPoint> {
    let mut p = CGPoint { x: 0.0, y: 0.0 };
    // SAFETY: the type is checked; AXValueGetValue checks the AXValue's own
    // type against AX_POINT and writes a CGPoint.
    (is(r, unsafe { AXValueGetTypeID() })
        && unsafe { AXValueGetValue(r, AX_POINT, (&mut p as *mut CGPoint).cast()) })
    .then_some(p)
}

fn size(r: CFTypeRef) -> Option<CGSize> {
    let mut s = CGSize {
        width: 0.0,
        height: 0.0,
    };
    // SAFETY: as `point`, for a CGSize.
    (is(r, unsafe { AXValueGetTypeID() })
        && unsafe { AXValueGetValue(r, AX_SIZE, (&mut s as *mut CGSize).cast()) })
    .then_some(s)
}

/// The attributes read from every element, in `Attr` order.
const ATTRS: [&str; 13] = [
    "AXRole",
    "AXSubrole",
    "AXTitle",
    "AXDescription",
    "AXValue",
    "AXPlaceholderValue",
    "AXPosition",
    "AXSize",
    "AXFocused",
    "AXEnabled",
    "AXSelected",
    "AXChildren",
    "AXParent",
];

#[derive(Clone, Copy)]
enum Attr {
    Role,
    Subrole,
    Title,
    Description,
    Value,
    Placeholder,
    Position,
    Size,
    Focused,
    Enabled,
    Selected,
    Children,
    Parent,
}

/// What one call read about one element.
struct Read {
    values: Cf,
}

impl Read {
    fn get(&self, a: Attr) -> CFTypeRef {
        // SAFETY: `values` is the CFArray AX returned, one entry per name
        // in `ATTRS` (an AXValue error where an attribute is missing).
        unsafe {
            if (a as isize) < CFArrayGetCount(self.values.0) {
                CFArrayGetValueAtIndex(self.values.0, a as isize)
            } else {
                std::ptr::null()
            }
        }
    }

    fn text(&self, a: Attr) -> Option<String> {
        string(self.get(a))
    }

    fn role(&self) -> String {
        self.text(Attr::Role).unwrap_or_default()
    }

    /// Its frame, in points on the global display space.
    fn frame(&self) -> Option<(f64, f64, f64, f64)> {
        let p = point(self.get(Attr::Position))?;
        let s = size(self.get(Attr::Size))?;
        Some((p.x, p.y, s.width, s.height))
    }

    fn children(&self) -> Vec<Cf> {
        let list = self.get(Attr::Children);
        // SAFETY: the type is checked; each child is lent by the array,
        // and retained before the array goes.
        unsafe {
            if !is(list, CFArrayGetTypeID()) {
                return Vec::new();
            }
            (0..CFArrayGetCount(list))
                .filter_map(|i| Cf::retain(CFArrayGetValueAtIndex(list, i)))
                .collect()
        }
    }
}

/// One read: the attribute names, as one CFArray for every call, and what
/// the read may still spend.
struct Session {
    _strings: Vec<objc2_core_foundation::CFRetained<CFString>>,
    array: Cf,
    started: Instant,
    visits: usize,
    /// Calls that ran into `APP_TIMEOUT`: an app that answers that slowly
    /// is left with what it gave.
    timeouts: u32,
}

/// Calls into an app that time out before the read gives up on it.
const MAX_TIMEOUTS: u32 = 2;

impl Session {
    fn new() -> Option<Session> {
        let strings: Vec<_> = ATTRS.iter().map(|a| CFString::from_str(a)).collect();
        let ptrs: Vec<CFTypeRef> = strings
            .iter()
            .map(|s| (&**s as *const CFString).cast())
            .collect();
        // SAFETY: `ptrs` holds live CFStrings; the array retains them.
        let array = Cf::own(unsafe {
            CFArrayCreate(
                std::ptr::null(),
                ptrs.as_ptr(),
                ptrs.len() as isize,
                &kCFTypeArrayCallBacks,
            )
        })?;
        Some(Session {
            _strings: strings,
            array,
            started: Instant::now(),
            visits: 0,
            timeouts: 0,
        })
    }

    /// Out of time, visits or patience: stop reading.
    fn spent(&self) -> bool {
        self.visits >= MAX_VISITS
            || self.started.elapsed() >= MAX_TIME
            || self.timeouts >= MAX_TIMEOUTS
    }

    fn read(&mut self, element: &Cf) -> Option<Read> {
        if self.timeouts >= MAX_TIMEOUTS {
            return None;
        }
        let called = Instant::now();
        let mut out: CFTypeRef = std::ptr::null();
        // SAFETY: `element` is a live AXUIElement, `array` a CFArray of
        // CFStrings; on success `out` is a new CFArray we own.
        let err =
            unsafe { AXUIElementCopyMultipleAttributeValues(element.0, self.array.0, 0, &mut out) };
        self.count_time(called);
        if err != 0 {
            return None;
        }
        Cf::own(out).map(|values| Read { values })
    }

    fn attribute(&mut self, element: &Cf, name: &str) -> Option<Cf> {
        if self.timeouts >= MAX_TIMEOUTS {
            return None;
        }
        let called = Instant::now();
        let out = attribute(element, name);
        self.count_time(called);
        out
    }

    fn count_time(&mut self, called: Instant) {
        if called.elapsed().as_secs_f32() >= APP_TIMEOUT * 0.9 {
            self.timeouts += 1;
        }
    }

    /// What to tell the model about a read that stopped short.
    fn note(&self) -> Option<&'static str> {
        if self.timeouts >= MAX_TIMEOUTS {
            Some("The app answers the accessibility tree slowly: this is what it gave in time.")
        } else if self.spent() {
            Some("The window has more than was read in time.")
        } else {
            None
        }
    }
}

fn attribute(element: &Cf, name: &str) -> Option<Cf> {
    let name = CFString::from_str(name);
    let mut out: CFTypeRef = std::ptr::null();
    // SAFETY: `element` is a live AXUIElement; on success `out` is ours.
    let err = unsafe {
        AXUIElementCopyAttributeValue(element.0, (&*name as *const CFString).cast(), &mut out)
    };
    if err != 0 {
        return None;
    }
    Cf::own(out)
}

/// The ordinary windows (layer 0) on the display, front to back: their
/// owners' pids and frames, in points.
fn windows_on(g: &Geometry) -> Vec<(i32, CGRect)> {
    let number = |dict: CFTypeRef, key: &'static str| -> Option<f64> {
        let key = CFString::from_static_str(key);
        // SAFETY: `dict` is a live CFDictionary; the value is lent by it.
        let v = unsafe { CFDictionaryGetValue(dict, (&*key as *const CFString).cast()) };
        let mut n = 0f64;
        // SAFETY: the type is checked; a CFNumber converts to a double.
        (is(v, unsafe { CFNumberGetTypeID() })
            && unsafe { CFNumberGetValue(v, CF_DOUBLE, (&mut n as *mut f64).cast()) })
        .then_some(n)
    };
    // SAFETY: on success, a new CFArray of CFDictionaries we own.
    let Some(list) = Cf::own(unsafe { CGWindowListCopyWindowInfo(ON_SCREEN_WINDOWS, 0) }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // SAFETY: each entry is a CFDictionary lent by the array.
    unsafe {
        for i in 0..CFArrayGetCount(list.0) {
            let w = CFArrayGetValueAtIndex(list.0, i);
            if number(w, "kCGWindowLayer") != Some(0.0) {
                continue;
            }
            let Some(pid) = number(w, "kCGWindowOwnerPID") else {
                continue;
            };
            let key = CFString::from_static_str("kCGWindowBounds");
            let bounds = CFDictionaryGetValue(w, (&*key as *const CFString).cast());
            let mut r = CGRect::default();
            if bounds.is_null() || !CGRectMakeWithDictionaryRepresentation(bounds, &mut r) {
                continue;
            }
            if g.stream_box(r.origin.x, r.origin.y, r.size.width, r.size.height)
                .is_some()
            {
                out.push((pid as i32, r));
            }
        }
    }
    out
}

fn app(pid: i32) -> Option<Cf> {
    // SAFETY: Create returns a new reference we own.
    let app = Cf::own(unsafe { AXUIElementCreateApplication(pid) })?;
    // SAFETY: `app` is a live AXUIElement. One call with room for a first
    // contact, then the short timeout for the read.
    unsafe {
        AXUIElementSetMessagingTimeout(app.0, FIRST_CONTACT);
        let _ = attribute(&app, "AXRole");
        AXUIElementSetMessagingTimeout(app.0, APP_TIMEOUT);
    }
    Some(app)
}

/// The label a person would read off it: its title, its description, its
/// text (static text only), or what an empty field says to type.
fn label(read: &Read, role: Role) -> String {
    let own = read
        .text(Attr::Title)
        .or_else(|| read.text(Attr::Description));
    match role {
        Role::Text | Role::Heading => own.or_else(|| read.text(Attr::Value)),
        Role::TextField | Role::TextArea | Role::ComboBox => {
            own.or_else(|| read.text(Attr::Placeholder))
        }
        _ => own,
    }
    .unwrap_or_default()
}

fn role_of(role: &str, subrole: Option<&str>, parent: &str) -> Role {
    match role {
        "AXButton" | "AXMenuButton" | "AXDisclosureTriangle" | "AXColorWell" => Role::Button,
        "AXLink" => Role::Link,
        "AXMenuItem" | "AXMenuBarItem" => Role::MenuItem,
        "AXPopUpButton" | "AXComboBox" => Role::ComboBox,
        "AXRadioButton" if parent == "AXTabGroup" => Role::Tab,
        "AXRadioButton" => Role::Radio,
        "AXCheckBox" if matches!(subrole, Some("AXSwitch" | "AXToggle")) => Role::Switch,
        "AXCheckBox" => Role::CheckBox,
        "AXTextField" | "AXDateField" => Role::TextField,
        "AXTextArea" => Role::TextArea,
        "AXSlider" | "AXIncrementor" => Role::Slider,
        "AXRow" if subrole == Some("AXOutlineRow") => Role::TreeItem,
        "AXRow" => Role::Row,
        "AXCell" => Role::Cell,
        "AXStaticText" => Role::Text,
        "AXHeading" => Role::Heading,
        "AXImage" => Role::Image,
        "AXGroup" | "AXSplitGroup" | "AXScrollArea" | "AXLayoutArea" | "AXRadioGroup"
        | "AXTabGroup" | "AXWebArea" => Role::Group,
        "AXToolbar" => Role::Toolbar,
        "AXSheet" | "AXDrawer" => Role::Dialog,
        "AXWindow" if matches!(subrole, Some("AXDialog" | "AXSystemDialog")) => Role::Dialog,
        "AXWindow" => Role::Window,
        "AXMenu" => Role::Menu,
        "AXMenuBar" => Role::MenuBar,
        "AXScrollBar" => Role::ScrollBar,
        "AXProgressIndicator" | "AXBusyIndicator" => Role::ProgressBar,
        "AXTable" | "AXOutline" => Role::Table,
        "AXList" | "AXBrowser" => Role::List,
        _ => Role::Other,
    }
}

/// The element `read` describes, if it is on the display and big enough.
fn element(read: &Read, role: Role, subrole: Option<&str>, g: &Geometry) -> Option<Element> {
    let (x, y, w, h) = read.frame()?;
    if w < MIN_SIDE || h < MIN_SIDE {
        return None;
    }
    let (sx, sy, sw, sh) = g.stream_box(x, y, w, h)?;
    let mut f = 0;
    if boolean(read.get(Attr::Focused)) == Some(true) {
        f |= flags::FOCUSED;
    }
    if boolean(read.get(Attr::Enabled)) == Some(false) {
        f |= flags::DISABLED;
    }
    if subrole == Some("AXSecureTextField") {
        f |= flags::SECRET;
    }
    let on = match role {
        Role::CheckBox | Role::Radio | Role::Switch | Role::Tab => boolean(read.get(Attr::Value)),
        _ => boolean(read.get(Attr::Selected)),
    };
    if on == Some(true) {
        f |= flags::ON;
    }
    Some(Element {
        role,
        flags: f,
        label: label(read, role),
        x: sx,
        y: sy,
        w: sw,
        h: sh,
        depth: 0,
    })
}

/// Listed in a window's walk: controls, and what is labelled.
fn listed(e: &Element) -> bool {
    e.role.is_control() || !e.label.is_empty()
}

pub struct ScreenReader {
    display: u32,
    stream: (u32, u32),
}

impl ScreenReader {
    /// The reader for the CoreGraphics display `display`, streamed at
    /// `stream` pixels.
    pub fn new(display: u32, stream: (u32, u32)) -> ScreenReader {
        ScreenReader { display, stream }
    }

    fn geometry(&self) -> Geometry {
        // SAFETY: a plain query; an unknown display gives an empty rect.
        let b = unsafe { CGDisplayBounds(self.display) };
        Geometry {
            left: b.origin.x,
            top: b.origin.y,
            width: b.size.width,
            height: b.size.height,
            picture: (0, 0, self.stream.0, self.stream.1),
        }
    }

    pub fn read(&mut self, query: Query) -> ScreenText {
        // SAFETY: a plain query.
        if !unsafe { AXIsProcessTrusted() } {
            return ScreenText::unavailable(
                "Pong lacks the Accessibility permission (System Settings > Privacy & \
                    Security > Accessibility).",
            );
        }
        if crate::platform::secure_screen() {
            return ScreenText::unavailable("The screen is locked.");
        }
        let Some(mut session) = Session::new() else {
            return ScreenText::unavailable("Could not set up the accessibility query.");
        };
        let g = self.geometry();
        match query {
            Query::Window => window(&mut session, &g),
            Query::At { x, y } => at(&mut session, &g, x, y),
        }
    }
}

/// The front window's elements, then its app's menu bar.
fn window(s: &mut Session, g: &Geometry) -> ScreenText {
    let Some(&(pid, _)) = windows_on(g).first() else {
        return ScreenText::unavailable("No window is open on the agent's display.");
    };
    window_of(s, g, pid)
}

/// App `pid`'s focused window, then its menu bar.
fn window_of(s: &mut Session, g: &Geometry, pid: i32) -> ScreenText {
    let Some(app) = app(pid) else {
        return ScreenText::unavailable("The app in front is not readable.");
    };
    let mut out = ScreenText {
        app: app_name(s, &app),
        ..ScreenText::default()
    };
    // The app's focused window is the front one on the display: the window
    // server's front window belongs to it.
    let window = s
        .attribute(&app, "AXFocusedWindow")
        .or_else(|| s.attribute(&app, "AXMainWindow"));
    if let Some(r) = window.as_ref().and_then(|w| s.read(w)) {
        out.window = r.text(Attr::Title).unwrap_or_default();
        walk(s, g, &r, "AXWindow", 0, &mut out);
    }
    if let Some(r) = s.attribute(&app, "AXMenuBar").and_then(|bar| s.read(&bar)) {
        walk(s, g, &r, "AXMenuBar", 0, &mut out);
    }
    if let Some(note) = s.note() {
        out.truncated = true;
        out.note = note.into();
    } else if window.is_none() {
        out.note = "The app in front has no window the accessibility tree knows.".into();
    }
    out
}

fn app_name(s: &mut Session, app: &Cf) -> String {
    s.attribute(app, "AXTitle")
        .and_then(|t| string(t.0))
        .unwrap_or_default()
}

/// Depth-first through `parent`'s children, in order: what a screen reader
/// would read, top to bottom.
fn walk(
    s: &mut Session,
    g: &Geometry,
    parent: &Read,
    parent_role: &str,
    depth: u8,
    out: &mut ScreenText,
) {
    for child in parent.children() {
        if s.spent() || out.elements.len() >= pingpong_proto::screen::MAX_ELEMENTS {
            return;
        }
        s.visits += 1;
        let Some(r) = s.read(&child) else {
            continue;
        };
        let ax_role = r.role();
        let subrole = r.text(Attr::Subrole);
        if let Some((x, y, w, h)) = r.frame() {
            let empty = w < 1.0 || h < 1.0;
            // A closed menu: no size, every item it would show inside.
            if ax_role == "AXMenu" && empty {
                continue;
            }
            // Wholly off the display (scrolled away, another display): so
            // is everything in it, as far as a click is concerned.
            if !empty && g.stream_box(x, y, w, h).is_none() {
                continue;
            }
        }
        let role = role_of(&ax_role, subrole.as_deref(), parent_role);
        if let Some(mut e) = element(&r, role, subrole.as_deref(), g) {
            if listed(&e) {
                e.depth = depth;
                out.elements.push(e);
            }
        }
        walk(s, g, &r, &ax_role, depth.saturating_add(1), out);
    }
}

/// The element at a stream pixel, then its parents up to its window.
fn at(s: &mut Session, g: &Geometry, x: u16, y: u16) -> ScreenText {
    let (gx, gy) = g.host_point(x, y);
    // The app whose window is topmost there; off every window (the menu
    // bar, the Dock), the front app's, and then the system's.
    let windows = windows_on(g);
    let owner = windows
        .iter()
        .find(|(_, r)| {
            gx >= r.origin.x
                && gy >= r.origin.y
                && gx < r.origin.x + r.size.width
                && gy < r.origin.y + r.size.height
        })
        .or(windows.first())
        .and_then(|(pid, _)| app(*pid));
    // SAFETY: Create returns a new reference we own.
    let system = Cf::own(unsafe { AXUIElementCreateSystemWide() });
    let mut found = None;
    for el in owner.iter().chain(system.iter()) {
        if s.spent() {
            break;
        }
        let called = Instant::now();
        let mut hit: CFTypeRef = std::ptr::null();
        // SAFETY: a live AXUIElement; on success `hit` is a new one we own.
        let err = unsafe { AXUIElementCopyElementAtPosition(el.0, gx as f32, gy as f32, &mut hit) };
        s.count_time(called);
        if err == 0 {
            found = Cf::own(hit);
            break;
        }
    }
    let Some(hit) = found else {
        return ScreenText::unavailable("Nothing the accessibility tree knows is there.");
    };
    let mut out = ScreenText::default();
    let mut pid = 0;
    // SAFETY: a live AXUIElement; `pid` is written on success.
    if unsafe { AXUIElementGetPid(hit.0, &mut pid) } == 0 {
        if let Some(app) = app(pid) {
            out.app = app_name(s, &app);
        }
    }
    let mut current = Some(hit);
    let mut depth = 0u8;
    while let Some(el) = current.take() {
        if s.spent() {
            break;
        }
        let Some(r) = s.read(&el) else { break };
        let ax_role = r.role();
        if ax_role == "AXApplication" {
            break;
        }
        let subrole = r.text(Attr::Subrole);
        let role = role_of(&ax_role, subrole.as_deref(), "");
        // Every link of the chain is kept, framed or not: a click's
        // meaning is in its parents ("Delete" in the "Trash" toolbar).
        let e = element(&r, role, subrole.as_deref(), g).unwrap_or(Element {
            role,
            flags: 0,
            label: label(&r, role),
            x,
            y,
            w: 0,
            h: 0,
            depth: 0,
        });
        if ax_role == "AXWindow" {
            out.window = e.label.clone();
            // A click on a window's own background: the window is what is
            // there.
            if depth == 0 {
                out.elements.push(e);
            }
            break;
        }
        if depth == 0 || !e.label.is_empty() || e.role.is_control() {
            out.elements.push(Element { depth, ..e });
            depth = depth.saturating_add(1);
        }
        if out.elements.len() > MAX_PARENTS {
            break;
        }
        current = Cf::retain(r.get(Attr::Parent));
    }
    if let Some(note) = s.note() {
        out.note = note.into();
    }
    out
}

#[cfg(test)]
mod tests {
    //! The roles are tested anywhere; reading a real tree needs a Mac with
    //! a window in front and the Accessibility permission for the test
    //! runner: `cargo test -p pong a11y -- --ignored --nocapture`.
    use super::*;

    #[test]
    fn roles_follow_their_parents_and_subroles() {
        assert_eq!(role_of("AXRadioButton", None, "AXTabGroup"), Role::Tab);
        assert_eq!(role_of("AXRadioButton", None, "AXRadioGroup"), Role::Radio);
        assert_eq!(role_of("AXCheckBox", Some("AXSwitch"), ""), Role::Switch);
        assert_eq!(role_of("AXRow", Some("AXOutlineRow"), ""), Role::TreeItem);
        assert_eq!(role_of("AXWindow", Some("AXDialog"), ""), Role::Dialog);
        assert_eq!(role_of("AXSomethingNew", None, ""), Role::Other);
    }

    #[test]
    #[ignore = "requires a Mac desktop and the Accessibility permission"]
    fn the_front_window_and_the_element_under_its_middle_read() {
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGMainDisplayID() -> u32;
        }
        // The main display, at its own size in points.
        let id = unsafe { CGMainDisplayID() };
        let b = unsafe { CGDisplayBounds(id) };
        let stream = (b.size.width as u32, b.size.height as u32);
        let mut r = ScreenReader::new(id, stream);
        // A11Y_TEST_PID reads that app's window instead of the front one
        // (Finder's, say), so the test needs no window brought forward.
        let pid: Option<i32> = std::env::var("A11Y_TEST_PID")
            .ok()
            .and_then(|p| p.parse().ok());
        let mut read = || match pid {
            Some(pid) => window_of(&mut Session::new().unwrap(), &r.geometry(), pid),
            None => r.read(Query::Window),
        };
        // Twice, timing the second: the first read in a process connects to
        // the window server and to the app, which a running Pong has done.
        let first = Instant::now();
        read();
        println!("first read {} ms", first.elapsed().as_millis());
        let started = Instant::now();
        let text = read();
        println!(
            "{} elements in {} ms, truncated {}, note {:?}; {} / {}",
            text.elements.len(),
            started.elapsed().as_millis(),
            text.truncated,
            text.note,
            text.app,
            text.window,
        );
        for e in text.elements.iter().take(40) {
            println!(
                "{:indent$}{:?} {:?} {:?}",
                "",
                e.role,
                e.label,
                (e.x, e.y, e.w, e.h),
                indent = e.depth as usize * 2
            );
        }
        assert_eq!(text.status, pingpong_proto::screen::Status::Ok);
        let (x, y) = (stream.0 as u16 / 2, stream.1 as u16 / 2);
        let started = Instant::now();
        let hit = r.read(Query::At { x, y });
        println!(
            "at ({x}, {y}): {} elements in {} ms; {} / {}",
            hit.elements.len(),
            started.elapsed().as_millis(),
            hit.app,
            hit.window
        );
        println!("status {:?}, note {:?}", hit.status, hit.note);
        for e in &hit.elements {
            println!("  {:?} {:?}", e.role, e.label);
        }
    }
}
