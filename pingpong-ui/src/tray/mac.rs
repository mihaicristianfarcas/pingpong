//! The menu bar's status item (`NSStatusItem`): a template image with a
//! menu. Menu items send their action to a small Objective-C object that
//! owns the queue the app reads.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{
    define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSBitmapImageRep, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSSize, NSString};

use super::{Events, TrayEvent, TrayImage, TrayItem};

/// A status item's image is 18 points high at the most; the mark is drawn
/// on a square of that.
const POINTS: f64 = 18.0;

struct TargetIvars {
    events: Events,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and the class
    // implements no `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "PingpongTrayTarget"]
    #[ivars = TargetIvars]
    struct Target;

    impl Target {
        /// A menu item was chosen: its tag is the item's id.
        #[unsafe(method(chosen:))]
        fn chosen(&self, item: &NSMenuItem) {
            let _ = self
                .ivars()
                .events
                .unbounded_send(TrayEvent::Item(item.tag() as u32));
        }
    }
);

impl Target {
    fn new(events: Events, mtm: MainThreadMarker) -> Retained<Target> {
        let this = Target::alloc(mtm).set_ivars(TargetIvars { events });
        // SAFETY: `init` is NSObject's designated initializer, sent to a
        // freshly allocated object whose ivars are set.
        unsafe { msg_send![super(this), init] }
    }
}

pub(super) struct Tray {
    item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    /// Menu items hold their target weakly: this keeps it.
    target: Retained<Target>,
    mtm: MainThreadMarker,
}

/// The mark as a template image: both pixel densities as representations of
/// one 18-point image, so the menu bar picks the one for its display.
fn template(image: TrayImage) -> Option<Retained<NSImage>> {
    let size = NSSize::new(POINTS, POINTS);
    let out = NSImage::initWithSize(NSImage::alloc(), size);
    let mut any = false;
    for png in [image.template_png, image.template_png_2x] {
        if let Some(rep) = NSBitmapImageRep::imageRepWithData(&NSData::with_bytes(png)) {
            // Its size in points, whatever its pixels: that is what makes
            // the second one the 2x representation.
            rep.setSize(size);
            out.addRepresentation(&rep);
            any = true;
        }
    }
    out.setTemplate(true);
    any.then_some(out)
}

impl Tray {
    pub(super) fn new(
        app_id: &str,
        tooltip: &str,
        image: TrayImage,
        events: Events,
    ) -> Option<Tray> {
        // AppKit is the main thread's; so is whoever builds the app's UI.
        let mtm = MainThreadMarker::new()?;
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        // Where the user dragged it is remembered under this name.
        item.setAutosaveName(Some(&NSString::from_str(app_id)));
        let button = item.button(mtm)?;
        match template(image) {
            Some(image) => button.setImage(Some(&image)),
            // No picture to show: the name, so there is something to click.
            None => button.setTitle(&NSString::from_str(tooltip)),
        }
        button.setToolTip(Some(&NSString::from_str(tooltip)));
        let menu = NSMenu::new(mtm);
        // Items are enabled as the app says, not by who answers their action.
        menu.setAutoenablesItems(false);
        item.setMenu(Some(&menu));
        Some(Tray {
            item,
            menu,
            target: Target::new(events, mtm),
            mtm,
        })
    }

    pub(super) fn set_menu(&self, items: &[TrayItem]) {
        self.menu.removeAllItems();
        for item in items {
            let entry = match item {
                TrayItem::Separator => NSMenuItem::separatorItem(self.mtm),
                TrayItem::Label(text) => {
                    // SAFETY: no action: there is no selector to be valid.
                    let entry = unsafe {
                        NSMenuItem::initWithTitle_action_keyEquivalent(
                            NSMenuItem::alloc(self.mtm),
                            &NSString::from_str(text),
                            None,
                            &NSString::from_str(""),
                        )
                    };
                    entry.setEnabled(false);
                    entry
                }
                TrayItem::Action { id, title } => {
                    // SAFETY: `chosen:` is the method `Target` defines, taking
                    // the menu item that sent it; the target outlives the
                    // menu (both are dropped with this `Tray`).
                    let entry = unsafe {
                        let entry = NSMenuItem::initWithTitle_action_keyEquivalent(
                            NSMenuItem::alloc(self.mtm),
                            &NSString::from_str(title),
                            Some(sel!(chosen:)),
                            &NSString::from_str(""),
                        );
                        let target: &AnyObject = &self.target;
                        entry.setTarget(Some(target));
                        entry
                    };
                    entry.setTag(*id as isize);
                    entry
                }
            };
            self.menu.addItem(&entry);
        }
    }

    pub(super) fn set_tooltip(&self, tooltip: &str) {
        if let Some(button) = self.item.button(self.mtm) {
            button.setToolTip(Some(&NSString::from_str(tooltip)));
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}
