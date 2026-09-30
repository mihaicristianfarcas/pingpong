//! One line-icon family (24×24, 1.75 pt round strokes), embedded in the
//! binary and tinted by the text colour.

use std::borrow::Cow;

use gpui::{prelude::*, px, svg, App, AssetSource, Rgba, SharedString, Window};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IconName {
    Activity,
    Agent,
    ArrowRight,
    ArrowUp,
    Bolt,
    ChartBar,
    Check,
    CheckCircle,
    ChevronDown,
    ChevronLeft,
    ChevronRight,
    ChevronUpDown,
    Clock,
    Close,
    CloseCircle,
    Copy,
    CursorClick,
    Devices,
    Display,
    Download,
    ExternalLink,
    Eye,
    Film,
    Gamepad,
    Globe,
    Hand,
    Key,
    Keyboard,
    Laptop,
    Link,
    List,
    Lock,
    Message,
    Monitor,
    More,
    Network,
    Pause,
    Pencil,
    Play,
    Plus,
    Pointer,
    Power,
    Refresh,
    Return,
    Search,
    Server,
    Settings,
    Shield,
    Sidebar,
    Sparkle,
    Speaker,
    Steam,
    Stop,
    Trash,
    Type,
    User,
    Warning,
    WifiOff,
}

impl IconName {
    pub const ALL: [IconName; 58] = [
        IconName::Activity,
        IconName::Agent,
        IconName::ArrowRight,
        IconName::ArrowUp,
        IconName::Bolt,
        IconName::ChartBar,
        IconName::Check,
        IconName::CheckCircle,
        IconName::ChevronDown,
        IconName::ChevronLeft,
        IconName::ChevronRight,
        IconName::ChevronUpDown,
        IconName::Clock,
        IconName::Close,
        IconName::CloseCircle,
        IconName::Copy,
        IconName::CursorClick,
        IconName::Devices,
        IconName::Display,
        IconName::Download,
        IconName::ExternalLink,
        IconName::Eye,
        IconName::Film,
        IconName::Gamepad,
        IconName::Globe,
        IconName::Hand,
        IconName::Key,
        IconName::Keyboard,
        IconName::Laptop,
        IconName::Link,
        IconName::List,
        IconName::Lock,
        IconName::Message,
        IconName::Monitor,
        IconName::More,
        IconName::Network,
        IconName::Pause,
        IconName::Pencil,
        IconName::Play,
        IconName::Plus,
        IconName::Pointer,
        IconName::Power,
        IconName::Refresh,
        IconName::Return,
        IconName::Search,
        IconName::Server,
        IconName::Settings,
        IconName::Shield,
        IconName::Sidebar,
        IconName::Sparkle,
        IconName::Speaker,
        IconName::Steam,
        IconName::Stop,
        IconName::Trash,
        IconName::Type,
        IconName::User,
        IconName::Warning,
        IconName::WifiOff,
    ];

    pub const fn path(self) -> &'static str {
        match self {
            IconName::Activity => "icons/activity.svg",
            IconName::Agent => "icons/agent.svg",
            IconName::ArrowRight => "icons/arrow-right.svg",
            IconName::ArrowUp => "icons/arrow-up.svg",
            IconName::Bolt => "icons/bolt.svg",
            IconName::ChartBar => "icons/chart-bar.svg",
            IconName::Check => "icons/check.svg",
            IconName::CheckCircle => "icons/check-circle.svg",
            IconName::ChevronDown => "icons/chevron-down.svg",
            IconName::ChevronLeft => "icons/chevron-left.svg",
            IconName::ChevronRight => "icons/chevron-right.svg",
            IconName::ChevronUpDown => "icons/chevron-up-down.svg",
            IconName::Clock => "icons/clock.svg",
            IconName::Close => "icons/close.svg",
            IconName::CloseCircle => "icons/close-circle.svg",
            IconName::Copy => "icons/copy.svg",
            IconName::CursorClick => "icons/cursor-click.svg",
            IconName::Devices => "icons/devices.svg",
            IconName::Display => "icons/display.svg",
            IconName::Download => "icons/download.svg",
            IconName::ExternalLink => "icons/external-link.svg",
            IconName::Eye => "icons/eye.svg",
            IconName::Film => "icons/film.svg",
            IconName::Gamepad => "icons/gamepad.svg",
            IconName::Globe => "icons/globe.svg",
            IconName::Hand => "icons/hand.svg",
            IconName::Key => "icons/key.svg",
            IconName::Keyboard => "icons/keyboard.svg",
            IconName::Laptop => "icons/laptop.svg",
            IconName::Link => "icons/link.svg",
            IconName::List => "icons/list.svg",
            IconName::Lock => "icons/lock.svg",
            IconName::Message => "icons/message.svg",
            IconName::Monitor => "icons/monitor.svg",
            IconName::More => "icons/more.svg",
            IconName::Network => "icons/network.svg",
            IconName::Pause => "icons/pause.svg",
            IconName::Pencil => "icons/pencil.svg",
            IconName::Play => "icons/play.svg",
            IconName::Plus => "icons/plus.svg",
            IconName::Pointer => "icons/pointer.svg",
            IconName::Power => "icons/power.svg",
            IconName::Refresh => "icons/refresh.svg",
            IconName::Return => "icons/return.svg",
            IconName::Search => "icons/search.svg",
            IconName::Server => "icons/server.svg",
            IconName::Settings => "icons/settings.svg",
            IconName::Shield => "icons/shield.svg",
            IconName::Sidebar => "icons/sidebar.svg",
            IconName::Sparkle => "icons/sparkle.svg",
            IconName::Speaker => "icons/speaker.svg",
            IconName::Steam => "icons/steam.svg",
            IconName::Stop => "icons/stop.svg",
            IconName::Trash => "icons/trash.svg",
            IconName::Type => "icons/type.svg",
            IconName::User => "icons/user.svg",
            IconName::Warning => "icons/warning.svg",
            IconName::WifiOff => "icons/wifi-off.svg",
        }
    }
}

/// Icon sizes on one optical scale.
pub struct IconSize;

impl IconSize {
    pub const SMALL: f32 = 13.0;
    pub const REGULAR: f32 = 16.0;
    pub const LARGE: f32 = 20.0;
    pub const DISPLAY: f32 = 28.0;
}

#[derive(IntoElement)]
pub struct Icon {
    name: IconName,
    size: f32,
    color: Rgba,
}

impl Icon {
    pub fn new(name: IconName, size: f32, color: Rgba) -> Icon {
        Icon { name, size, color }
    }
}

impl RenderOnce for Icon {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        svg()
            .path(self.name.path())
            .flex_none()
            .size(px(self.size))
            .text_color(self.color)
    }
}

pub fn icon(name: IconName, size: f32, color: Rgba) -> Icon {
    Icon::new(name, size, color)
}

/// The embedded icons, as GPUI's asset source.
#[derive(Clone, Copy, Debug, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(embedded(path).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(if path.trim_end_matches('/') == "icons" {
            IconName::ALL.iter().map(|i| i.path().into()).collect()
        } else {
            Vec::new()
        })
    }
}

fn embedded(path: &str) -> Option<&'static [u8]> {
    Some(match path {
        "icons/activity.svg" => include_bytes!("../assets/icons/activity.svg"),
        "icons/agent.svg" => include_bytes!("../assets/icons/agent.svg"),
        "icons/arrow-right.svg" => include_bytes!("../assets/icons/arrow-right.svg"),
        "icons/arrow-up.svg" => include_bytes!("../assets/icons/arrow-up.svg"),
        "icons/bolt.svg" => include_bytes!("../assets/icons/bolt.svg"),
        "icons/chart-bar.svg" => include_bytes!("../assets/icons/chart-bar.svg"),
        "icons/check.svg" => include_bytes!("../assets/icons/check.svg"),
        "icons/check-circle.svg" => include_bytes!("../assets/icons/check-circle.svg"),
        "icons/chevron-down.svg" => include_bytes!("../assets/icons/chevron-down.svg"),
        "icons/chevron-left.svg" => include_bytes!("../assets/icons/chevron-left.svg"),
        "icons/chevron-right.svg" => include_bytes!("../assets/icons/chevron-right.svg"),
        "icons/chevron-up-down.svg" => include_bytes!("../assets/icons/chevron-up-down.svg"),
        "icons/clock.svg" => include_bytes!("../assets/icons/clock.svg"),
        "icons/close.svg" => include_bytes!("../assets/icons/close.svg"),
        "icons/close-circle.svg" => include_bytes!("../assets/icons/close-circle.svg"),
        "icons/copy.svg" => include_bytes!("../assets/icons/copy.svg"),
        "icons/cursor-click.svg" => include_bytes!("../assets/icons/cursor-click.svg"),
        "icons/devices.svg" => include_bytes!("../assets/icons/devices.svg"),
        "icons/display.svg" => include_bytes!("../assets/icons/display.svg"),
        "icons/download.svg" => include_bytes!("../assets/icons/download.svg"),
        "icons/external-link.svg" => include_bytes!("../assets/icons/external-link.svg"),
        "icons/eye.svg" => include_bytes!("../assets/icons/eye.svg"),
        "icons/film.svg" => include_bytes!("../assets/icons/film.svg"),
        "icons/gamepad.svg" => include_bytes!("../assets/icons/gamepad.svg"),
        "icons/globe.svg" => include_bytes!("../assets/icons/globe.svg"),
        "icons/hand.svg" => include_bytes!("../assets/icons/hand.svg"),
        "icons/key.svg" => include_bytes!("../assets/icons/key.svg"),
        "icons/keyboard.svg" => include_bytes!("../assets/icons/keyboard.svg"),
        "icons/laptop.svg" => include_bytes!("../assets/icons/laptop.svg"),
        "icons/link.svg" => include_bytes!("../assets/icons/link.svg"),
        "icons/list.svg" => include_bytes!("../assets/icons/list.svg"),
        "icons/lock.svg" => include_bytes!("../assets/icons/lock.svg"),
        "icons/message.svg" => include_bytes!("../assets/icons/message.svg"),
        "icons/monitor.svg" => include_bytes!("../assets/icons/monitor.svg"),
        "icons/more.svg" => include_bytes!("../assets/icons/more.svg"),
        "icons/network.svg" => include_bytes!("../assets/icons/network.svg"),
        "icons/pause.svg" => include_bytes!("../assets/icons/pause.svg"),
        "icons/pencil.svg" => include_bytes!("../assets/icons/pencil.svg"),
        "icons/play.svg" => include_bytes!("../assets/icons/play.svg"),
        "icons/plus.svg" => include_bytes!("../assets/icons/plus.svg"),
        "icons/pointer.svg" => include_bytes!("../assets/icons/pointer.svg"),
        "icons/power.svg" => include_bytes!("../assets/icons/power.svg"),
        "icons/refresh.svg" => include_bytes!("../assets/icons/refresh.svg"),
        "icons/return.svg" => include_bytes!("../assets/icons/return.svg"),
        "icons/search.svg" => include_bytes!("../assets/icons/search.svg"),
        "icons/server.svg" => include_bytes!("../assets/icons/server.svg"),
        "icons/settings.svg" => include_bytes!("../assets/icons/settings.svg"),
        "icons/shield.svg" => include_bytes!("../assets/icons/shield.svg"),
        "icons/sidebar.svg" => include_bytes!("../assets/icons/sidebar.svg"),
        "icons/sparkle.svg" => include_bytes!("../assets/icons/sparkle.svg"),
        "icons/speaker.svg" => include_bytes!("../assets/icons/speaker.svg"),
        "icons/steam.svg" => include_bytes!("../assets/icons/steam.svg"),
        "icons/stop.svg" => include_bytes!("../assets/icons/stop.svg"),
        "icons/trash.svg" => include_bytes!("../assets/icons/trash.svg"),
        "icons/type.svg" => include_bytes!("../assets/icons/type.svg"),
        "icons/user.svg" => include_bytes!("../assets/icons/user.svg"),
        "icons/warning.svg" => include_bytes!("../assets/icons/warning.svg"),
        "icons/wifi-off.svg" => include_bytes!("../assets/icons/wifi-off.svg"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_is_embedded() {
        for i in IconName::ALL {
            assert!(embedded(i.path()).is_some(), "{:?}", i);
        }
    }
}
