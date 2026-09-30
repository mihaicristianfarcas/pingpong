//! An icon where the system keeps the apps that run in the background: the
//! menu bar's status items on a Mac, the taskbar's notification area on
//! Windows. Pong's window lives behind one, so the host is at hand without a
//! window open or a place in the Dock or the taskbar.
//!
//! The icon has a menu, rebuilt whenever what it says changes. On a Mac a
//! click opens the menu, as status items do; on Windows a click is
//! [`TrayEvent::Activate`] (open the window) and the menu is on the right
//! button, as notification icons do.
//!
//! Linux has no equivalent: there is no one tray there (the StatusNotifier
//! protocol needs a D-Bus service and a menu protocol of its own, and GNOME
//! shows its items only with an extension), so [`Tray::new`] returns `None`
//! and the app stays an ordinary window with a launcher entry.

#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod win;

use gpui::App;

/// What the user did with the icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Clicked it (Windows), or started the app a second time: show the
    /// window.
    Activate,
    /// Chose the menu item with this id.
    Item(u32),
}

/// A line of the icon's menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayItem {
    Separator,
    /// A line that only says something (the host's state).
    Label(String),
    Action {
        id: u32,
        title: String,
    },
}

impl TrayItem {
    pub fn action(id: u32, title: impl Into<String>) -> TrayItem {
        TrayItem::Action {
            id,
            title: title.into(),
        }
    }
}

/// The icon's picture. A Mac draws a template image: the mark in black and
/// alpha, at one and two pixels to the point, tinted by the menu bar. Windows
/// takes the app's own icon from the executable's resources, so there is
/// nothing to pass.
#[derive(Debug, Clone, Copy)]
pub struct TrayImage {
    pub template_png: &'static [u8],
    pub template_png_2x: &'static [u8],
}

/// Where the platform's callbacks leave what the user did.
#[cfg(any(target_os = "macos", windows))]
type Events = futures::channel::mpsc::UnboundedSender<TrayEvent>;

/// The icon, for as long as this lives.
pub struct Tray {
    #[cfg(target_os = "macos")]
    inner: mac::Tray,
    #[cfg(windows)]
    inner: win::Tray,
    /// The menu as last set: it is only rebuilt when it changes (a rebuild
    /// under an open menu flickers).
    items: Vec<TrayItem>,
    tooltip: String,
}

impl Tray {
    /// Put the icon up. `app_id` names the app to the system (the status
    /// item's saved place on a Mac, the window class on Windows); `on_event`
    /// runs on the main thread, outside any of the system's callbacks.
    /// `None` where there is no tray, or the system refused.
    #[cfg(any(target_os = "macos", windows))]
    pub fn new(
        app_id: &str,
        tooltip: &str,
        image: TrayImage,
        cx: &mut App,
        mut on_event: impl FnMut(TrayEvent, &mut App) + 'static,
    ) -> Option<Tray> {
        use futures::StreamExt;
        let (events, mut taken) = futures::channel::mpsc::unbounded::<TrayEvent>();
        #[cfg(target_os = "macos")]
        let inner = mac::Tray::new(app_id, tooltip, image, events)?;
        #[cfg(windows)]
        let inner = {
            let _ = image;
            win::Tray::new(app_id, tooltip, events)?
        };
        // The system calls back inside its own menu tracking and window
        // procedures, where GPUI may be in the middle of something: what
        // the user did is queued, and handled from GPUI's own loop.
        cx.spawn(async move |cx| {
            while let Some(event) = taken.next().await {
                cx.update(|cx| on_event(event, cx));
            }
        })
        .detach();
        Some(Tray {
            inner,
            items: Vec::new(),
            tooltip: tooltip.to_string(),
        })
    }

    /// No tray on this system (see the module's words on Linux).
    #[cfg(not(any(target_os = "macos", windows)))]
    pub fn new(
        app_id: &str,
        tooltip: &str,
        image: TrayImage,
        cx: &mut App,
        on_event: impl FnMut(TrayEvent, &mut App) + 'static,
    ) -> Option<Tray> {
        let _ = (app_id, tooltip, image, cx, on_event);
        None
    }

    /// What the menu holds from now on.
    pub fn set_menu(&mut self, items: Vec<TrayItem>) {
        if items == self.items {
            return;
        }
        #[cfg(any(target_os = "macos", windows))]
        self.inner.set_menu(&items);
        self.items = items;
    }

    /// What hovering over the icon says.
    pub fn set_tooltip(&mut self, tooltip: &str) {
        if tooltip == self.tooltip {
            return;
        }
        #[cfg(any(target_os = "macos", windows))]
        self.inner.set_tooltip(tooltip);
        self.tooltip = tooltip.to_string();
    }
}

/// Whether this system has a tray at all (what [`Tray::new`] can return).
pub const fn supported() -> bool {
    cfg!(any(target_os = "macos", windows))
}
