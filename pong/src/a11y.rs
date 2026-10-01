//! A Windows host's accessibility tree, as screen text for its agent
//! (`pingpong_proto::screen`): UI Automation, what Narrator reads.
//!
//! The front window is `GetForegroundWindow`'s; its elements come from the
//! control view (what UI Automation shows a screen reader), one call per
//! element with every property cached at once, so the walk can stop at its
//! limits -- `FindAll` over a browser's tree would run as long as the tree.
//!
//! Coordinates: the host process is not DPI-aware (`DisplayRect::primary`
//! is in the desktop's scaled units), and what UI Automation reports to such
//! a process depends on the provider. The reading thread makes itself
//! per-monitor aware instead, so every rectangle is in physical pixels, and
//! the display's own rectangle is read the same way (its current mode).
//!
//! A text field's value is never read: UI Automation's name for a field is
//! its label, and a password field is marked secret.

use std::time::{Duration, Instant};

use pingpong_proto::screen::{flags, Element, Query, Role, ScreenText};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, POINT};
use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use crate::screen::{Geometry, MIN_SIDE};

/// A walk stops after visiting this many elements, or after this long, and
/// says so: a browser's tree for a long page runs to tens of thousands, and
/// the agent's click waits on a read.
const MAX_VISITS: usize = 4000;
const MAX_TIME: Duration = Duration::from_millis(600);
/// Parents listed above the element at a point, at most.
const MAX_PARENTS: usize = 12;

thread_local! {
    /// COM and the DPI context are set up once per reading thread.
    static READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The properties read with every element.
const PROPERTIES: [UIA_PROPERTY_ID; 9] = [
    UIA_NamePropertyId,
    UIA_ControlTypePropertyId,
    UIA_BoundingRectanglePropertyId,
    UIA_IsEnabledPropertyId,
    UIA_HasKeyboardFocusPropertyId,
    UIA_IsPasswordPropertyId,
    UIA_ToggleToggleStatePropertyId,
    UIA_SelectionItemIsSelectedPropertyId,
    UIA_ProcessIdPropertyId,
];

pub struct ScreenReader {
    /// The display streamed (`\\.\DISPLAY3`).
    gdi_name: String,
    stream: (u32, u32),
}

/// What a read needs, made on the reading thread (COM objects stay there).
struct Uia {
    automation: IUIAutomation,
    cache: IUIAutomationCacheRequest,
    walker: IUIAutomationTreeWalker,
}

impl Uia {
    fn new() -> windows::core::Result<Uia> {
        // SAFETY: COM is initialised on this thread (`prepare`).
        unsafe {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)?;
            let cache = automation.CreateCacheRequest()?;
            for p in PROPERTIES {
                cache.AddProperty(p)?;
            }
            let walker = automation.ControlViewWalker()?;
            Ok(Uia {
                automation,
                cache,
                walker,
            })
        }
    }
}

fn prepare() {
    READY.with(|ready| {
        if !ready.get() {
            // SAFETY: once per thread; the thread only ever reads.
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            }
            ready.set(true);
        }
    })
}

impl ScreenReader {
    pub fn new(gdi_name: String, stream: (u32, u32)) -> ScreenReader {
        ScreenReader { gdi_name, stream }
    }

    /// The display's rectangle in physical pixels (this thread is
    /// per-monitor aware).
    fn geometry(&self) -> Option<Geometry> {
        let name: Vec<u16> = self.gdi_name.encode_utf16().chain([0]).collect();
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: `name` is NUL-terminated and outlives the call; `mode` is
        // sized as the call expects.
        let ok = unsafe {
            EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode)
        };
        if !ok.as_bool() || mode.dmPelsWidth == 0 {
            return None;
        }
        // SAFETY: for a display, the union holds the position.
        let pos = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
        Some(Geometry {
            left: pos.x as f64,
            top: pos.y as f64,
            width: mode.dmPelsWidth as f64,
            height: mode.dmPelsHeight as f64,
            picture: (0, 0, self.stream.0, self.stream.1),
        })
    }

    pub fn read(&mut self, query: Query) -> ScreenText {
        if crate::platform::secure_screen() {
            return ScreenText::unavailable(
                "A secure screen is up (sign-in, lock or administrator prompt).",
            );
        }
        prepare();
        let Some(g) = self.geometry() else {
            return ScreenText::unavailable("The agent's display is not there.");
        };
        let uia = match Uia::new() {
            Ok(u) => u,
            Err(e) => return ScreenText::unavailable(format!("UI Automation is unavailable: {e}")),
        };
        match query {
            Query::Window => window(&uia, &g),
            Query::At { x, y } => at(&uia, &g, x, y),
        }
    }
}

// The Windows SDK's names for the control types, matched as they are.
#[allow(non_upper_case_globals)]
fn role_of(t: UIA_CONTROLTYPE_ID) -> Role {
    match t {
        UIA_ButtonControlTypeId | UIA_SplitButtonControlTypeId | UIA_HeaderItemControlTypeId => {
            Role::Button
        }
        UIA_HyperlinkControlTypeId => Role::Link,
        UIA_MenuItemControlTypeId => Role::MenuItem,
        UIA_TabItemControlTypeId => Role::Tab,
        UIA_CheckBoxControlTypeId => Role::CheckBox,
        UIA_RadioButtonControlTypeId => Role::Radio,
        UIA_EditControlTypeId => Role::TextField,
        UIA_ComboBoxControlTypeId => Role::ComboBox,
        UIA_SliderControlTypeId | UIA_SpinnerControlTypeId => Role::Slider,
        UIA_ListItemControlTypeId => Role::ListItem,
        UIA_DataItemControlTypeId => Role::Row,
        UIA_TreeItemControlTypeId => Role::TreeItem,
        UIA_TextControlTypeId | UIA_ToolTipControlTypeId => Role::Text,
        UIA_ImageControlTypeId => Role::Image,
        UIA_GroupControlTypeId
        | UIA_PaneControlTypeId
        | UIA_DocumentControlTypeId
        | UIA_TabControlTypeId
        | UIA_StatusBarControlTypeId
        | UIA_TitleBarControlTypeId
        | UIA_HeaderControlTypeId
        | UIA_CalendarControlTypeId
        | UIA_SemanticZoomControlTypeId => Role::Group,
        UIA_ToolBarControlTypeId | UIA_AppBarControlTypeId => Role::Toolbar,
        UIA_WindowControlTypeId => Role::Window,
        UIA_MenuControlTypeId => Role::Menu,
        UIA_MenuBarControlTypeId => Role::MenuBar,
        UIA_ScrollBarControlTypeId => Role::ScrollBar,
        UIA_ProgressBarControlTypeId => Role::ProgressBar,
        UIA_TableControlTypeId | UIA_DataGridControlTypeId => Role::Table,
        UIA_ListControlTypeId | UIA_TreeControlTypeId => Role::List,
        _ => Role::Other,
    }
}

/// What one element's cache says.
struct Read {
    role: Role,
    name: String,
    rect: Option<(f64, f64, f64, f64)>,
    flags: u8,
    pid: i32,
}

/// A cached property's VARIANT as a number (VT_I4) or a flag (VT_BOOL).
fn variant_on(el: &IUIAutomationElement, p: UIA_PROPERTY_ID) -> bool {
    // SAFETY: the property is in the cache request; the VARIANT's tag is
    // read before its value, and neither tag holds anything to free.
    unsafe {
        let Ok(v) = el.GetCachedPropertyValue(p) else {
            return false;
        };
        let inner = &v.Anonymous.Anonymous;
        match inner.vt {
            windows::Win32::System::Variant::VT_I4 => inner.Anonymous.lVal == 1,
            windows::Win32::System::Variant::VT_BOOL => inner.Anonymous.boolVal.as_bool(),
            _ => false,
        }
    }
}

fn read(el: &IUIAutomationElement) -> Read {
    // SAFETY: the element was fetched with `PROPERTIES` cached.
    unsafe {
        let role = role_of(el.CachedControlType().unwrap_or_default());
        let name = el
            .CachedName()
            .map(|b| b.to_string().trim().to_string())
            .unwrap_or_default();
        let rect = el.CachedBoundingRectangle().ok().map(|r| {
            (
                r.left as f64,
                r.top as f64,
                (r.right - r.left) as f64,
                (r.bottom - r.top) as f64,
            )
        });
        let mut f = 0;
        if el.CachedHasKeyboardFocus().is_ok_and(|b| b.as_bool()) {
            f |= flags::FOCUSED;
        }
        if el.CachedIsEnabled().is_ok_and(|b| !b.as_bool()) {
            f |= flags::DISABLED;
        }
        if el.CachedIsPassword().is_ok_and(|b| b.as_bool()) {
            f |= flags::SECRET;
        }
        if variant_on(el, UIA_ToggleToggleStatePropertyId)
            || variant_on(el, UIA_SelectionItemIsSelectedPropertyId)
        {
            f |= flags::ON;
        }
        Read {
            role,
            name,
            rect,
            flags: f,
            pid: el.CachedProcessId().unwrap_or(0),
        }
    }
}

fn element(r: &Read, g: &Geometry) -> Option<Element> {
    let (x, y, w, h) = r.rect?;
    if w < MIN_SIDE || h < MIN_SIDE {
        return None;
    }
    let (sx, sy, sw, sh) = g.stream_box(x, y, w, h)?;
    Some(Element {
        role: r.role,
        flags: r.flags,
        label: r.name.clone(),
        x: sx,
        y: sy,
        w: sw,
        h: sh,
        depth: 0,
    })
}

/// The program's name ("msedge"), for the model to know the app.
fn app_name(pid: i32) -> String {
    if pid <= 0 {
        return String::new();
    }
    // SAFETY: a query-only handle, closed below; the buffer's length is
    // passed and updated by the call.
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) else {
            return String::new();
        };
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let name =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
                .ok()
                .map(|()| String::from_utf16_lossy(&buf[..len as usize]));
        let _ = CloseHandle(h);
        name.and_then(|p| {
            std::path::Path::new(&p)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
    }
}

fn window(uia: &Uia, g: &Geometry) -> ScreenText {
    // SAFETY: plain calls on objects made on this thread.
    let root = unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_invalid() {
            return ScreenText::unavailable("No window is in front.");
        }
        match uia.automation.ElementFromHandleBuildCache(hwnd, &uia.cache) {
            Ok(e) => e,
            Err(e) => {
                return ScreenText::unavailable(format!("The front window is unreadable: {e}"))
            }
        }
    };
    let top = read(&root);
    let mut out = ScreenText {
        app: app_name(top.pid),
        window: top.name.clone(),
        ..ScreenText::default()
    };
    let started = Instant::now();
    let mut visits = 0;
    let mut seen = Seen::new();
    walk(uia, g, &root, 0, &mut out, &mut visits, &mut seen, started);
    if visits >= MAX_VISITS || started.elapsed() >= MAX_TIME {
        out.truncated = true;
        out.note = "The window has more than was read in time.".into();
    }
    out
}

/// An element already listed, by what it is and where: WebView2 apps
/// (WhatsApp) show the same web content's tree under several hosts, three
/// times over in WhatsApp's window. A repeat is not listed again, but what
/// it holds is still walked (and its own repeats left out the same way):
/// there the content itself sits under a group of the same name and place
/// as its host, so leaving out a repeat's whole subtree leaves out the
/// content.
type Seen = std::collections::HashSet<(u8, String, u16, u16, u16, u16)>;

/// Depth-first through `parent`'s children in the control view, in order.
#[allow(clippy::too_many_arguments)]
fn walk(
    uia: &Uia,
    g: &Geometry,
    parent: &IUIAutomationElement,
    depth: u8,
    out: &mut ScreenText,
    visits: &mut usize,
    seen: &mut Seen,
    started: Instant,
) {
    // SAFETY: plain calls on objects made on this thread.
    let mut next = unsafe {
        uia.walker
            .GetFirstChildElementBuildCache(parent, &uia.cache)
    }
    .ok();
    while let Some(child) = next.take() {
        if *visits >= MAX_VISITS
            || started.elapsed() >= MAX_TIME
            || out.elements.len() >= pingpong_proto::screen::MAX_ELEMENTS
        {
            return;
        }
        *visits += 1;
        let r = read(&child);
        // Wholly off the display (scrolled away): so is what is in it.
        let off = r
            .rect
            .is_some_and(|(x, y, w, h)| w >= 1.0 && h >= 1.0 && g.stream_box(x, y, w, h).is_none());
        let e = element(&r, g);
        let repeat = e
            .as_ref()
            .is_some_and(|e| !seen.insert((e.role as u8, e.label.clone(), e.x, e.y, e.w, e.h)));
        if !off {
            if let Some(mut e) = e.filter(|_| !repeat) {
                if e.role.is_control() || !e.label.is_empty() {
                    e.depth = depth;
                    out.elements.push(e);
                }
            }
            walk(
                uia,
                g,
                &child,
                depth.saturating_add(1),
                out,
                visits,
                seen,
                started,
            );
        }
        // SAFETY: as above.
        next = unsafe {
            uia.walker
                .GetNextSiblingElementBuildCache(&child, &uia.cache)
        }
        .ok();
    }
}

fn at(uia: &Uia, g: &Geometry, x: u16, y: u16) -> ScreenText {
    let (gx, gy) = g.host_point(x, y);
    let pt = POINT {
        x: gx as i32,
        y: gy as i32,
    };
    // SAFETY: plain calls on objects made on this thread.
    let Ok(hit) = (unsafe { uia.automation.ElementFromPointBuildCache(pt, &uia.cache) }) else {
        return ScreenText::unavailable("Nothing the accessibility tree knows is there.");
    };
    let first = read(&hit);
    let mut out = ScreenText {
        app: app_name(first.pid),
        ..ScreenText::default()
    };
    let mut current = Some(hit);
    let mut depth = 0u8;
    while let Some(el) = current.take() {
        let r = read(&el);
        if r.role == Role::Window {
            out.window = r.name;
            break;
        }
        // Every link of the chain is kept, framed or not: a click's meaning
        // is in its parents ("Delete" in the "Recycle Bin" toolbar).
        let e = element(&r, g).unwrap_or(Element {
            role: r.role,
            flags: r.flags,
            label: r.name.clone(),
            x,
            y,
            w: 0,
            h: 0,
            depth: 0,
        });
        if depth == 0 || !e.label.is_empty() || e.role.is_control() {
            out.elements.push(Element { depth, ..e });
            depth = depth.saturating_add(1);
        }
        if out.elements.len() > MAX_PARENTS {
            break;
        }
        // SAFETY: as above. The desktop's root has no parent: the chain ends
        // there when it meets no window (the taskbar).
        current = unsafe { uia.walker.GetParentElementBuildCache(&el, &uia.cache) }.ok();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_types_map_to_roles() {
        assert_eq!(role_of(UIA_ButtonControlTypeId), Role::Button);
        assert_eq!(role_of(UIA_EditControlTypeId), Role::TextField);
        assert_eq!(role_of(UIA_TabItemControlTypeId), Role::Tab);
        assert_eq!(role_of(UIA_CONTROLTYPE_ID(1)), Role::Other);
    }
}
