//! A Linux host's accessibility tree, as screen text for its agent
//! (`pingpong_proto::screen`): AT-SPI over D-Bus, what Orca reads. GTK and
//! Qt apps publish to it when the desktop runs the accessibility bus
//! (at-spi2-core; GNOME and KDE start it themselves).
//!
//! X11 only. Under Wayland a client does not know where its window is, so
//! AT-SPI's screen coordinates are not the screen's: a Wayland host answers
//! that it cannot say, rather than with places that would mislead a click.
//!
//! The front window is the one whose state says active. A window's walk asks
//! each element for its state first and leaves out what is not showing (and
//! all it holds); the element at a point is searched for the same way (see
//! `Read::at`). Every call has `CALL_TIMEOUT`: a hung app costs the read
//! that long, not the click that waits on it.

use std::time::{Duration, Instant};

use pingpong_proto::screen::{flags, Element, Query, Role, ScreenText};
use zbus::blocking::Connection;
use zbus::zvariant::{self, OwnedObjectPath, OwnedValue};

use crate::screen::{Geometry, MIN_SIDE};

const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
const COMPONENT: &str = "org.a11y.atspi.Component";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const ROOT: &str = "/org/a11y/atspi/accessible/root";
const NULL_PATH: &str = "/org/a11y/atspi/null";
/// `ATSPI_COORD_TYPE_SCREEN`.
const SCREEN: u32 = 0;

/// How long one call into an app may take: a hung app answers nothing.
const CALL_TIMEOUT: Duration = Duration::from_millis(250);
/// A walk stops after visiting this many elements, or after this long, and
/// says so. Each element costs five calls here, against one on a Mac.
const MAX_VISITS: usize = 1500;
const MAX_TIME: Duration = Duration::from_millis(600);
/// Levels searched down to the element at a point, at most.
const MAX_LEVELS: usize = 24;

/// `AtspiStateType`s, as bit numbers in `GetState`'s two words.
mod state {
    pub const ACTIVE: u32 = 1;
    pub const CHECKED: u32 = 4;
    pub const EDITABLE: u32 = 7;
    pub const FOCUSED: u32 = 12;
    pub const MULTI_LINE: u32 = 17;
    pub const PRESSED: u32 = 20;
    pub const SELECTED: u32 = 23;
    pub const SENSITIVE: u32 = 24;
    pub const SHOWING: u32 = 25;
}

/// An element: the app's bus name and the object's path.
#[derive(Clone, PartialEq)]
struct Node {
    bus: String,
    path: OwnedObjectPath,
}

#[derive(Clone)]
struct States(Vec<u32>);

impl States {
    fn has(&self, s: u32) -> bool {
        self.0
            .get((s / 32) as usize)
            .is_some_and(|w| w & (1 << (s % 32)) != 0)
    }
}

pub struct ScreenReader {
    geometry: Geometry,
    wayland: bool,
    bus: Option<Connection>,
}

impl ScreenReader {
    /// `picture`: where the screen sits in the stream; `screen`: its size.
    pub fn new(picture: (u32, u32, u32, u32), screen: (u32, u32), wayland: bool) -> ScreenReader {
        ScreenReader {
            geometry: Geometry {
                left: 0.0,
                top: 0.0,
                width: screen.0 as f64,
                height: screen.1 as f64,
                picture,
            },
            wayland,
            bus: None,
        }
    }

    pub fn read(&mut self, query: Query) -> ScreenText {
        if self.wayland {
            return ScreenText::unavailable(
                "Under Wayland the accessibility tree does not say where things are on \
                    the screen.",
            );
        }
        if self.bus.is_none() {
            match connect() {
                Ok(c) => self.bus = Some(c),
                Err(e) => return ScreenText::unavailable(e),
            }
        }
        let mut r = Read {
            bus: self.bus.as_ref().expect("connected above"),
            g: &self.geometry,
            started: Instant::now(),
            visits: 0,
        };
        let out = match query {
            Query::Window => r.window(),
            Query::At { x, y } => r.at(x, y),
        };
        // A bus that went away (the session's accessibility bus restarted)
        // is connected again next time.
        if out.status == pingpong_proto::screen::Status::Unavailable {
            self.bus = None;
        }
        out
    }
}

/// The accessibility bus, through the session bus's pointer to it.
fn connect() -> Result<Connection, String> {
    let session = Connection::session().map_err(|e| format!("No session bus: {e}"))?;
    let address: String = session
        .call_method(
            Some("org.a11y.Bus"),
            "/org/a11y/bus",
            Some("org.a11y.Bus"),
            "GetAddress",
            &(),
        )
        .and_then(|m| m.body().deserialize())
        .map_err(|e| {
            format!("The desktop's accessibility bus is not running (at-spi2-core): {e}")
        })?;
    zbus::blocking::connection::Builder::address(address.as_str())
        .and_then(|b| b.method_timeout(CALL_TIMEOUT).build())
        .map_err(|e| format!("Cannot reach the accessibility bus: {e}"))
}

struct Read<'a> {
    bus: &'a Connection,
    g: &'a Geometry,
    started: Instant,
    visits: usize,
}

impl Read<'_> {
    fn spent(&self) -> bool {
        self.visits >= MAX_VISITS || self.started.elapsed() >= MAX_TIME
    }

    fn call<B, R>(&self, n: &Node, iface: &str, method: &str, body: &B) -> Option<R>
    where
        B: serde::Serialize + zvariant::DynamicType,
        R: for<'d> serde::Deserialize<'d> + zvariant::Type,
    {
        self.bus
            .call_method(
                Some(n.bus.as_str()),
                n.path.as_str(),
                Some(iface),
                method,
                body,
            )
            .ok()?
            .body()
            .deserialize()
            .ok()
    }

    fn property(&self, n: &Node, name: &str) -> Option<OwnedValue> {
        self.call(
            n,
            "org.freedesktop.DBus.Properties",
            "Get",
            &(ACCESSIBLE, name),
        )
    }

    fn name(&self, n: &Node) -> String {
        self.property(n, "Name")
            .and_then(|v| String::try_from(v).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    fn children(&self, n: &Node) -> Vec<Node> {
        self.call::<_, Vec<(String, OwnedObjectPath)>>(n, ACCESSIBLE, "GetChildren", &())
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, p)| p.as_str() != NULL_PATH)
            .map(|(bus, path)| Node { bus, path })
            .collect()
    }

    fn states(&self, n: &Node) -> States {
        States(
            self.call(n, ACCESSIBLE, "GetState", &())
                .unwrap_or_default(),
        )
    }

    fn role(&self, n: &Node) -> u32 {
        self.call(n, ACCESSIBLE, "GetRole", &()).unwrap_or(0)
    }

    fn extents(&self, n: &Node) -> Option<(f64, f64, f64, f64)> {
        let (x, y, w, h): (i32, i32, i32, i32) =
            self.call(n, COMPONENT, "GetExtents", &(SCREEN,))?;
        Some((x as f64, y as f64, w as f64, h as f64))
    }

    /// The active window, and its app's name.
    fn front(&self) -> Option<(Node, String)> {
        let registry = Node {
            bus: REGISTRY.into(),
            path: OwnedObjectPath::try_from(ROOT).ok()?,
        };
        for app in self.children(&registry) {
            for w in self.children(&app) {
                if self.states(&w).has(state::ACTIVE) {
                    return Some((w, self.name(&app)));
                }
            }
            if self.started.elapsed() >= MAX_TIME {
                break;
            }
        }
        None
    }

    fn element(&self, n: &Node, s: &States) -> Option<Element> {
        let raw = self.role(n);
        let role = role_of(raw, s);
        let (x, y, w, h) = self.extents(n)?;
        if w < MIN_SIDE || h < MIN_SIDE {
            return None;
        }
        let (sx, sy, sw, sh) = self.g.stream_box(x, y, w, h)?;
        let mut f = 0;
        if s.has(state::FOCUSED) {
            f |= flags::FOCUSED;
        }
        if !s.has(state::SENSITIVE) {
            f |= flags::DISABLED;
        }
        if raw == PASSWORD_TEXT {
            f |= flags::SECRET;
        }
        if s.has(state::CHECKED) || s.has(state::PRESSED) || s.has(state::SELECTED) {
            f |= flags::ON;
        }
        Some(Element {
            role,
            flags: f,
            label: self.name(n),
            x: sx,
            y: sy,
            w: sw,
            h: sh,
            depth: 0,
        })
    }

    fn window(&mut self) -> ScreenText {
        let Some((w, app)) = self.front() else {
            return ScreenText::unavailable("No window is active, or none publishes its tree.");
        };
        let mut out = ScreenText {
            app,
            window: self.name(&w),
            ..ScreenText::default()
        };
        self.walk(&w, 0, &mut out);
        if self.spent() {
            out.truncated = true;
            out.note = "The window has more than was read in time.".into();
        }
        out
    }

    fn walk(&mut self, parent: &Node, depth: u8, out: &mut ScreenText) {
        for child in self.children(parent) {
            if self.spent() || out.elements.len() >= pingpong_proto::screen::MAX_ELEMENTS {
                return;
            }
            self.visits += 1;
            let s = self.states(&child);
            // Not showing: nor is anything in it.
            if !s.has(state::SHOWING) {
                continue;
            }
            if let Some(mut e) = self.element(&child, &s) {
                if e.role.is_control() || !e.label.is_empty() {
                    e.depth = depth;
                    out.elements.push(e);
                }
            }
            self.walk(&child, depth.saturating_add(1), out);
        }
    }

    /// The innermost showing element at the point, then the ones that hold
    /// it out to the window. Searched rather than asked for
    /// (`GetAccessibleAtPoint`): GTK keeps an open menu's items under the
    /// menu bar, inside boxes that do not hold the point, and asking the
    /// window from the top down finds the text under the menu. So the search
    /// walks everything showing, as a window's walk does (40 ms for
    /// Mousepad's window with a menu open, in the container desktop of
    /// `tools/linux/agent-desktop`), and an open menu's item wins over what
    /// it covers, a deeper element over what holds it, a later one over an
    /// earlier (drawn over it).
    fn at(&mut self, x: u16, y: u16) -> ScreenText {
        let Some((w, app)) = self.front() else {
            return ScreenText::unavailable("No window is active, or none publishes its tree.");
        };
        let (gx, gy) = self.g.host_point(x, y);
        let mut out = ScreenText {
            app,
            window: self.name(&w),
            ..ScreenText::default()
        };
        let mut path = Vec::new();
        let mut best = None;
        self.hunt(&w, (gx, gy), &mut path, &mut best);
        let Some(Found { chain, .. }) = best else {
            out.note = "The window publishes nothing at that point.".into();
            return out;
        };
        for (n, s, role) in chain.iter().rev() {
            let label = self.name(n);
            if out.elements.is_empty() || !label.is_empty() || role.is_control() {
                let e = self.element(n, s).unwrap_or(Element {
                    role: *role,
                    flags: 0,
                    label,
                    x,
                    y,
                    w: 0,
                    h: 0,
                    depth: 0,
                });
                out.elements.push(Element {
                    depth: out.elements.len().min(255) as u8,
                    ..e
                });
            }
        }
        if self.spent() {
            out.note = "The window has more than was searched in time.".into();
        }
        out
    }

    fn hunt(
        &mut self,
        parent: &Node,
        (gx, gy): (f64, f64),
        path: &mut Vec<(Node, States, Role)>,
        best: &mut Option<Found>,
    ) {
        for child in self.children(parent) {
            if self.spent() || path.len() >= MAX_LEVELS {
                return;
            }
            self.visits += 1;
            let s = self.states(&child);
            if !s.has(state::SHOWING) {
                continue;
            }
            let role = role_of(self.role(&child), &s);
            let ext = self.extents(&child);
            let holds =
                ext.is_some_and(|(x, y, w, h)| gx >= x && gy >= y && gx < x + w && gy < y + h);
            path.push((child.clone(), s, role));
            if holds {
                let rank = (
                    path.iter().any(|(_, _, r)| *r == Role::Menu) as u8,
                    path.len(),
                );
                if best.as_ref().is_none_or(|b| rank >= b.rank) {
                    *best = Some(Found {
                        rank,
                        chain: path.clone(),
                    });
                }
            }
            self.hunt(&child, (gx, gy), path, best);
            path.pop();
        }
    }
}

/// The element a search found at a point, outermost first, and how it
/// ranks: inside an open menu, then deeper.
struct Found {
    rank: (u8, usize),
    chain: Vec<(Node, States, Role)>,
}

/// `AtspiRole`s used by name.
const PASSWORD_TEXT: u32 = 40;
const TEXT: u32 = 61;

fn role_of(r: u32, s: &States) -> Role {
    match r {
        43 | 62 | 129 => Role::Button, // push button, toggle button, push button menu
        88 => Role::Link,
        8 | 35 | 45 | 59 => Role::MenuItem, // check, plain, radio and tear-off menu items
        37 => Role::Tab,
        7 => Role::CheckBox,
        44 => Role::Radio,
        130 => Role::Switch,
        12 | PASSWORD_TEXT | 79 => Role::TextField, // date editor, password, entry
        TEXT if s.has(state::EDITABLE) && s.has(state::MULTI_LINE) => Role::TextArea,
        TEXT if s.has(state::EDITABLE) => Role::TextField,
        TEXT | 29 | 64 | 73 | 81 | 116 => Role::Text, // label, tooltip, paragraph, caption, static
        11 => Role::ComboBox,
        15 | 51 | 52 => Role::Slider, // dial, slider, spin button
        32 => Role::ListItem,
        90 => Role::Row,
        56 => Role::Cell,
        91 => Role::TreeItem,
        83 => Role::Heading,
        26 | 27 => Role::Image,
        20 | 25 | 30 | 38 | 39 | 46 | 49 | 53 | 54 | 68 | 82 | 85 | 87 | 94 | 95 | 99 => {
            Role::Group
        }
        63 => Role::Toolbar,
        2 | 16 | 19 => Role::Dialog, // alert, dialog, file chooser
        23 | 69 => Role::Window,     // frame, window
        33 | 41 => Role::Menu,
        34 => Role::MenuBar,
        48 => Role::ScrollBar,
        42 | 103 => Role::ProgressBar,
        55 | 66 => Role::Table,
        31 | 65 | 98 => Role::List,
        _ => Role::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_are_bits_across_two_words() {
        let s = States(vec![1 << state::ACTIVE | 1 << state::SHOWING, 0]);
        assert!(s.has(state::ACTIVE) && s.has(state::SHOWING));
        assert!(!s.has(state::FOCUSED));
        assert!(!States(vec![]).has(state::SHOWING));
        assert!(States(vec![0, 1]).has(32));
    }

    #[test]
    fn text_roles_follow_whether_they_can_be_edited() {
        let editable = States(vec![1 << state::EDITABLE]);
        let area = States(vec![1 << state::EDITABLE | 1 << state::MULTI_LINE]);
        assert_eq!(role_of(TEXT, &States(vec![])), Role::Text);
        assert_eq!(role_of(TEXT, &editable), Role::TextField);
        assert_eq!(role_of(TEXT, &area), Role::TextArea);
        assert_eq!(role_of(43, &States(vec![])), Role::Button);
        assert_eq!(role_of(999, &States(vec![])), Role::Other);
    }
}
