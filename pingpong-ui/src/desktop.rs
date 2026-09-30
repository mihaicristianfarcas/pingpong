//! What the apps ask of the desktop beyond their own windows: the standard
//! About panel, staying out of the Dock, and (for checks) what the menu bar
//! holds. These are a Mac's: Windows and Linux draw no menu bar for a GPUI
//! window and have no Dock to stay out of, so each function says what it
//! does there.

/// Be an app without a Dock icon or a place in the app switcher: its tray
/// icon and the windows it opens are all there is of it. GPUI makes every
/// app a regular one as it finishes launching, so this is called after
/// that, from the launch callback.
///
/// Elsewhere nothing: a window is in the taskbar while it is open and gone
/// when it closes, which is already this.
pub fn hide_from_dock() {
    #[cfg(target_os = "macos")]
    mac::hide_from_dock();
}

/// The system's About panel: the app's icon, `name`, `version`, and `build`
/// in brackets after it when there is one (a checkout's commit).
///
/// Elsewhere nothing: there is no menu to ask for it from; the sidebar's
/// foot and the settings say the version.
pub fn show_about(name: &str, version: &str, build: &str) {
    #[cfg(target_os = "macos")]
    mac::show_about(name, version, build);
    #[cfg(not(target_os = "macos"))]
    let _ = (name, version, build);
}

/// The menu bar as it is: a line for each menu ("Ping", "View", ...) and,
/// indented under it, each item with its key ("Hosts  ⌘1"), separators as
/// "-". For checks that the menus are the ones meant, the system's own
/// additions included. Empty where there is no menu bar.
pub fn menu_bar() -> Vec<String> {
    #[cfg(target_os = "macos")]
    return mac::menu_bar();
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

#[cfg(target_os = "macos")]
mod mac {
    use objc2::runtime::AnyObject;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{
        NSAboutPanelOptionApplicationName, NSAboutPanelOptionApplicationVersion,
        NSAboutPanelOptionVersion, NSApplication, NSApplicationActivationPolicy,
        NSEventModifierFlags, NSMenu,
    };
    use objc2_foundation::{NSDictionary, NSString};

    pub(super) fn hide_from_dock() {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        NSApplication::sharedApplication(mtm)
            .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }

    pub(super) fn show_about(name: &str, version: &str, build: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        let (name, version, build) = (
            NSString::from_str(name),
            NSString::from_str(version),
            NSString::from_str(build),
        );
        // Said here, not left to the bundle's Info.plist: a build run
        // outside a bundle has none, and would show no version at all.
        let values: [&AnyObject; 3] = [&name, &version, &build];
        // SAFETY: the option keys are AppKit's own constants, and each takes
        // an NSString, which is what it is given.
        unsafe {
            let options = NSDictionary::from_slices(
                &[
                    NSAboutPanelOptionApplicationName,
                    NSAboutPanelOptionApplicationVersion,
                    NSAboutPanelOptionVersion,
                ],
                &values,
            );
            app.orderFrontStandardAboutPanelWithOptions(&options);
        }
        // An app without a Dock icon is not in front just because its menu
        // was used: the panel would open behind whatever is.
        app.activate();
    }

    fn lines(menu: &NSMenu, depth: usize, out: &mut Vec<String>) {
        for item in menu.itemArray().iter() {
            let indent = "  ".repeat(depth);
            if item.isSeparatorItem() {
                out.push(format!("{indent}-"));
                continue;
            }
            let mut line = format!("{indent}{}", item.title());
            let key = item.keyEquivalent().to_string();
            if !key.is_empty() {
                let mods = item.keyEquivalentModifierMask();
                let mut chord = String::new();
                for (flag, glyph) in [
                    (NSEventModifierFlags::Control, '⌃'),
                    (NSEventModifierFlags::Option, '⌥'),
                    (NSEventModifierFlags::Shift, '⇧'),
                    (NSEventModifierFlags::Command, '⌘'),
                ] {
                    if mods.contains(flag) {
                        chord.push(glyph);
                    }
                }
                line.push_str(&format!("  {chord}{}", key.to_uppercase()));
            }
            out.push(line);
            if let Some(submenu) = item.submenu() {
                lines(&submenu, depth + 1, out);
            }
        }
    }

    pub(super) fn menu_bar() -> Vec<String> {
        let mut out = Vec::new();
        if let Some(mtm) = MainThreadMarker::new() {
            if let Some(menu) = NSApplication::sharedApplication(mtm).mainMenu() {
                lines(&menu, 0, &mut out);
            }
        }
        out
    }
}
