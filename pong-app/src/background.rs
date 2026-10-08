//! The app behind the window: Pong's icon in the menu bar (a Mac) or the
//! taskbar's notification area (Windows), which stays when the window is
//! closed. Its menu says what the host is doing and opens the window; a
//! device asking to pair is announced with a notification.
//!
//! On a Mac the app keeps out of the Dock and the app switcher altogether:
//! it is a menu bar item that can show a window, as such apps are. Where
//! there is no tray (Linux), none of this exists and the app is its window:
//! closing it quits, as before.
//!
//! Quitting from the menu quits this app, not the host: the host is a
//! service (or Pong.app) of its own and keeps running, which the menu's
//! first line goes on saying for as long as the icon is there.

use std::collections::HashSet;
use std::rc::Rc;

use futures::StreamExt;
use gpui::{App, AppContext, Entity, Global, SystemNotification, WindowHandle};
use pingpong_ui::tray::{Tray, TrayEvent, TrayImage, TrayItem};
use pingpong_update::{Checker, Install, Status as UpdateStatus};

use crate::app::{Page, PongApp};
use crate::link::Link;
use crate::worker::{Cmd, Conn, Snapshot};

/// The app's name to the system: the status item's saved place, the tray
/// window's class, the notifications' sender.
pub const APP_ID: &str = "dev.pingpong.PongControl";

/// The menu's items.
mod item {
    pub const OPEN: u32 = 1;
    pub const PAIR: u32 = 2;
    pub const UPDATES: u32 = 3;
    pub const QUIT: u32 = 4;
}

/// What opening the window is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    /// Just the window, where it was.
    Window,
    /// The pairing requests.
    Devices,
    /// The update sheet.
    Updates,
}

pub struct Background {
    link: Entity<Link>,
    updates: Rc<Checker>,
    window: Option<WindowHandle<PongApp>>,
    tray: Option<Tray>,
    /// The pairing requests a notification has gone out for.
    announced: HashSet<u32>,
    /// The window has been opened at an update that did not take.
    failure_shown: bool,
}

impl Global for Background {}

/// A notification's tag for a pairing request, and back.
fn pairing_tag(id: u32) -> String {
    format!("pong-pair-{id}")
}

/// The menu's first line, and what hovering over the icon says.
pub fn state_line(snap: &Snapshot) -> String {
    match &snap.conn {
        Conn::Connecting => "Connecting to Pong…".into(),
        Conn::NotRunning => "Pong is not running".into(),
        Conn::SignIn { .. } => "Signed out of Pong".into(),
        Conn::Ready => match snap.status.as_ref().and_then(|s| s.session.as_ref()) {
            Some(session) if session.agent.is_some() => {
                format!("An AI agent is at work ({})", session.client)
            }
            Some(session) => format!("Streaming to {}", session.client),
            None => "Pong is running".into(),
        },
    }
}

/// The icon's menu for the host's state and the update check's.
pub fn menu(snap: &Snapshot, updates: &UpdateStatus) -> Vec<TrayItem> {
    let mut items = vec![
        TrayItem::Label(state_line(snap)),
        TrayItem::Separator,
        TrayItem::action(item::OPEN, "Open Pong"),
    ];
    let pending = snap
        .status
        .as_ref()
        .map(|s| s.pending.as_slice())
        .unwrap_or_default();
    match pending {
        [] => {}
        [one] => items.push(TrayItem::action(
            item::PAIR,
            format!("{} Wants to Pair…", one.client_name),
        )),
        many => items.push(TrayItem::action(
            item::PAIR,
            format!("{} Devices Want to Pair…", many.len()),
        )),
    }
    items.push(TrayItem::Separator);
    items.push(TrayItem::action(
        item::UPDATES,
        match &updates.update {
            Some(update) => format!("{}…", title_case(&update.headline("Pong"))),
            None => "Check for Updates…".into(),
        },
    ));
    items.push(TrayItem::Separator);
    // The app's own name: it is this that quits, and the host that stays.
    items.push(TrayItem::action(item::QUIT, "Quit Pong Control"));
    items
}

/// A sentence as a menu item writes it: "Pong 0.7.0 Is Available".
fn title_case(text: &str) -> String {
    text.split(' ')
        .map(|word| match word {
            // The small words menus leave alone.
            "on" | "to" | "of" | "a" | "the" => word.to_string(),
            _ => {
                let mut chars = word.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

impl Background {
    /// Whether the app outlives its window (there is an icon to come back
    /// by).
    pub fn has_tray(cx: &App) -> bool {
        cx.try_global::<Background>()
            .is_some_and(|b| b.tray.is_some())
    }

    /// The icon's menu as it is now, a line each (checks).
    pub fn menu_lines(cx: &App) -> Vec<String> {
        let Some(this) = cx.try_global::<Background>() else {
            return Vec::new();
        };
        if this.tray.is_none() {
            return Vec::new();
        }
        menu(&this.link.read(cx).snap, &this.updates.status())
            .into_iter()
            .map(|item| match item {
                TrayItem::Separator => "-".to_string(),
                TrayItem::Label(text) => format!("({text})"),
                TrayItem::Action { title, .. } => title,
            })
            .collect()
    }

    /// An update being installed: once it is in place (or its installer
    /// waits for this app), the app quits; one that did not take opens the
    /// window at the sheet that says why, once.
    fn follow_install(cx: &mut App) {
        let install = cx.global::<Background>().updates.status().install;
        match install {
            Some(Install::Restarting) => cx.quit(),
            Some(Install::Failed(_)) => {
                if !std::mem::replace(&mut cx.global_mut::<Background>().failure_shown, true) {
                    open(Show::Updates, cx);
                }
            }
            _ => cx.global_mut::<Background>().failure_shown = false,
        }
    }

    /// The host said something new, or the update check did: the icon and
    /// its notifications follow.
    fn refresh(cx: &mut App) {
        let (snap, updates) = {
            let this = cx.global::<Background>();
            (this.link.read(cx).snap.clone(), this.updates.status())
        };
        let window_in_front = cx.global::<Background>().window.is_some_and(|w| {
            w.update(cx, |_, window, _| window.is_window_active())
                .unwrap_or(false)
        });
        let pending: Vec<(u32, String)> = snap
            .status
            .as_ref()
            .map(|s| {
                s.pending
                    .iter()
                    .map(|p| (p.id, p.client_name.clone()))
                    .collect()
            })
            .unwrap_or_default();
        // Requests that went (paired, declined, given up) take their
        // notifications along.
        let gone: Vec<u32> = cx
            .global::<Background>()
            .announced
            .iter()
            .copied()
            .filter(|id| !pending.iter().any(|(p, _)| p == id))
            .collect();
        for id in gone {
            cx.dismiss_system_notification(&pairing_tag(id));
            cx.global_mut::<Background>().announced.remove(&id);
        }
        for (id, name) in pending {
            if !cx.global_mut::<Background>().announced.insert(id) {
                continue;
            }
            // The window in front shows the request itself.
            if window_in_front {
                continue;
            }
            tracing::info!(request = id, "announcing a pairing request");
            cx.show_system_notification(SystemNotification {
                tag: pairing_tag(id).into(),
                title: format!("{name} wants to pair").into(),
                body: "Open Pong and enter the PIN it shows to let it connect.".into(),
                actions: Vec::new(),
            });
        }
        let this = cx.global_mut::<Background>();
        if let Some(tray) = &mut this.tray {
            tray.set_tooltip(&format!("Pong: {}", state_line(&snap)));
            tray.set_menu(menu(&snap, &updates));
        }
    }

    fn on_tray(event: TrayEvent, cx: &mut App) {
        tracing::debug!(?event, "tray");
        match event {
            TrayEvent::Activate | TrayEvent::Item(item::OPEN) => open(Show::Window, cx),
            TrayEvent::Item(item::PAIR) => open(Show::Devices, cx),
            TrayEvent::Item(item::UPDATES) => open(Show::Updates, cx),
            TrayEvent::Item(item::QUIT) => cx.quit(),
            TrayEvent::Item(_) => {}
        }
    }
}

/// Start the app's background half: the icon (where there is a tray), the
/// host's state and the update check feeding it, the notifications. Then
/// the window, unless the app was started to sit in the tray (`background`,
/// at login) and there is a tray to sit in.
pub fn start(
    link: Entity<Link>,
    updates: Rc<Checker>,
    mut update_news: futures::channel::mpsc::UnboundedReceiver<()>,
    mut shows: futures::channel::mpsc::UnboundedReceiver<()>,
    background: bool,
    cx: &mut App,
) {
    let tray = Tray::new(
        APP_ID,
        "Pong",
        TrayImage {
            template_png: include_bytes!("../assets/tray.png"),
            template_png_2x: include_bytes!("../assets/tray@2x.png"),
        },
        cx,
        Background::on_tray,
    );
    let has_tray = tray.is_some();
    tracing::info!(tray = has_tray, background, "Pong's icon");
    if has_tray {
        // The icon is the app: closing the window leaves it, and a Mac
        // shows it nowhere else.
        cx.set_quit_mode(gpui::QuitMode::Explicit);
        pingpong_ui::desktop::hide_from_dock();
    }
    cx.set_global(Background {
        link: link.clone(),
        updates,
        window: None,
        tray,
        announced: HashSet::new(),
        failure_shown: false,
    });
    cx.observe(&link, |_, cx| Background::refresh(cx)).detach();
    cx.on_window_closed(|cx, _| {
        if !cx.windows().is_empty() {
            return;
        }
        tracing::info!(stays = Background::has_tray(cx), "window closed");
        let link = {
            let this = cx.global_mut::<Background>();
            this.window = None;
            this.link.clone()
        };
        link.read(cx).send(Cmd::Watched(false));
        if !Background::has_tray(cx) {
            cx.quit();
        }
    })
    .detach();
    // A click on a pairing request's notification.
    cx.on_system_notification_response(|response, cx| {
        if response.tag.starts_with("pong-pair-") {
            open(Show::Devices, cx);
        }
    });
    // The update check's news, and a second copy of the app asking this
    // one to show itself: both arrive from other threads.
    cx.spawn(async move |cx| {
        while update_news.next().await.is_some() {
            cx.update(|cx| {
                Background::refresh(cx);
                cx.refresh_windows();
                Background::follow_install(cx);
            });
        }
    })
    .detach();
    cx.spawn(async move |cx| {
        while shows.next().await.is_some() {
            cx.update(|cx| open(Show::Window, cx));
        }
    })
    .detach();
    Background::refresh(cx);
    if !(background && has_tray) {
        open(Show::Window, cx);
    }
}

/// Bring the window up (opening it if it is closed), at what it was asked
/// for.
pub fn open(show: Show, cx: &mut App) {
    let existing = cx.global::<Background>().window;
    let handle = match existing {
        Some(handle) if handle.update(cx, |_, _, _| ()).is_ok() => handle,
        _ => {
            let options = pingpong_ui::window_options(
                "Pong",
                "dev.pingpong.PongApp",
                940.0,
                640.0,
                (700.0, 460.0),
                cx,
            );
            let (link, updates) = {
                let this = cx.global::<Background>();
                (this.link.clone(), this.updates.clone())
            };
            let opened = cx.open_window(options, |window, cx| {
                cx.new(|cx| PongApp::new(link.clone(), updates, window, cx))
            });
            let handle = match opened {
                Ok(handle) => handle,
                Err(e) => {
                    tracing::error!(error = %e, "Pong's window did not open");
                    return;
                }
            };
            tracing::info!(?show, "window opened");
            link.read(cx).send(Cmd::Watched(true));
            cx.global_mut::<Background>().window = Some(handle);
            handle
        }
    };
    let _ = handle.update(cx, |app, window, cx| {
        match show {
            Show::Window => {}
            Show::Devices => app.go(Page::Devices, cx),
            Show::Updates => app.show_updates(cx),
        }
        window.activate_window();
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{AgentStatus, Pending, Session, Status};
    use pingpong_update::Update;

    fn ready(status: Status) -> Snapshot {
        Snapshot {
            conn: Conn::Ready,
            status: Some(status),
            ..Snapshot::default()
        }
    }

    fn titles(items: &[TrayItem]) -> Vec<&str> {
        items
            .iter()
            .map(|item| match item {
                TrayItem::Separator => "-",
                TrayItem::Label(text) | TrayItem::Action { title: text, .. } => text,
            })
            .collect()
    }

    #[test]
    fn the_menu_says_what_the_host_is_doing() {
        let idle = ready(Status::default());
        assert_eq!(state_line(&idle), "Pong is running");
        let streaming = ready(Status {
            session: Some(Session {
                client: "macbook".into(),
                ..Session::default()
            }),
            ..Status::default()
        });
        assert_eq!(state_line(&streaming), "Streaming to macbook");
        let agent = ready(Status {
            session: Some(Session {
                client: "macbook agent".into(),
                agent: Some(AgentStatus::default()),
                ..Session::default()
            }),
            ..Status::default()
        });
        assert_eq!(state_line(&agent), "An AI agent is at work (macbook agent)");
        let stopped = Snapshot {
            conn: Conn::NotRunning,
            ..Snapshot::default()
        };
        assert_eq!(state_line(&stopped), "Pong is not running");
        assert_eq!(
            titles(&menu(&idle, &UpdateStatus::default())),
            [
                "Pong is running",
                "-",
                "Open Pong",
                "-",
                "Check for Updates…",
                "-",
                "Quit Pong Control"
            ]
        );
    }

    #[test]
    fn a_device_asking_to_pair_is_in_the_menu_by_name() {
        let request = |id: u32, name: &str| Pending {
            id,
            client_name: name.into(),
            ..Pending::default()
        };
        let one = ready(Status {
            pending: vec![request(7, "MacBook Air")],
            ..Status::default()
        });
        assert!(
            titles(&menu(&one, &UpdateStatus::default())).contains(&"MacBook Air Wants to Pair…")
        );
        let two = ready(Status {
            pending: vec![request(7, "MacBook Air"), request(8, "living-room")],
            ..Status::default()
        });
        assert!(titles(&menu(&two, &UpdateStatus::default())).contains(&"2 Devices Want to Pair…"));
    }

    #[test]
    fn an_update_takes_the_place_of_the_check_in_the_menu() {
        let updates = UpdateStatus {
            update: Some(Update::Release {
                version: "0.7.0".into(),
                url: String::new(),
            }),
            ..UpdateStatus::default()
        };
        let items = menu(&ready(Status::default()), &updates);
        assert!(titles(&items).contains(&"Pong 0.7.0 Is Available…"));
        assert!(!titles(&items).contains(&"Check for Updates…"));
        assert_eq!(
            title_case("4 newer commits on main"),
            "4 Newer Commits on Main"
        );
    }
}
