//! A controller's Home (Xbox, PS) button, read from its raw HID reports.
//!
//! macOS 26 keeps the Home button from apps: its Games overlay opens and the
//! GameController framework never reports the press, whatever an app asks
//! (`preferredSystemGestureState`). The HID reports still carry it, and
//! Moonlight gets it that way, through SDL. This watches every game pad's
//! reports on a thread of its own and says whether a Home button is down.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};

type IOHIDManagerRef = *mut c_void;
type IOHIDValueRef = *mut c_void;
type IOHIDElementRef = *mut c_void;
type CFRunLoopRef = *mut c_void;

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOHIDManagerCreate(allocator: *const c_void, options: u32) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: *const c_void);
    fn IOHIDManagerRegisterInputValueCallback(
        manager: IOHIDManagerRef,
        callback: extern "C" fn(*mut c_void, i32, *mut c_void, IOHIDValueRef),
        context: *mut c_void,
    );
    fn IOHIDManagerScheduleWithRunLoop(
        manager: IOHIDManagerRef,
        run_loop: CFRunLoopRef,
        mode: *const c_void,
    );
    fn IOHIDManagerOpen(manager: IOHIDManagerRef, options: u32) -> i32;
    fn IOHIDManagerClose(manager: IOHIDManagerRef, options: u32) -> i32;
    fn IOHIDValueGetElement(value: IOHIDValueRef) -> IOHIDElementRef;
    fn IOHIDValueGetIntegerValue(value: IOHIDValueRef) -> isize;
    fn IOHIDElementGetUsagePage(element: IOHIDElementRef) -> u32;
    fn IOHIDElementGetUsage(element: IOHIDElementRef) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRun();
    fn CFRunLoopStop(run_loop: CFRunLoopRef);
    fn CFRelease(cf: *const c_void);
    static kCFRunLoopDefaultMode: *const c_void;
}

/// Consumer page, AC Home: Xbox controllers over Bluetooth; Generic
/// Desktop, System Main Menu: some others.
const HOME: [(u32, u32); 2] = [(0x0C, 0x223), (0x01, 0x85)];
/// Pages and usages the GameController framework already reports (sticks,
/// triggers, hat, numbered buttons): not logged.
fn known(page: u32, usage: u32) -> bool {
    page == 0x09
        || (page == 0x01 && (0x30..=0x39).contains(&usage))
        || (page == 0x02 && matches!(usage, 0xC4 | 0xC5))
}

pub struct HomeButtons {
    pressed: Arc<AtomicBool>,
    run_loop: Arc<AtomicPtr<c_void>>,
    thread: Option<JoinHandle<()>>,
}

impl HomeButtons {
    pub fn start() -> HomeButtons {
        let pressed = Arc::new(AtomicBool::new(false));
        let run_loop = Arc::new(AtomicPtr::new(std::ptr::null_mut()));
        let thread = {
            let (pressed, run_loop) = (pressed.clone(), run_loop.clone());
            std::thread::Builder::new()
                .name("ping-hid".into())
                .spawn(move || watch(pressed, run_loop))
                .ok()
        };
        HomeButtons {
            pressed,
            run_loop,
            thread,
        }
    }

    /// A game pad's Home button is down.
    pub fn pressed(&self) -> bool {
        self.pressed.load(Ordering::Relaxed)
    }
}

impl Drop for HomeButtons {
    fn drop(&mut self) {
        // The watcher may not have its run loop yet: wait for it briefly.
        for _ in 0..50 {
            let rl = self.run_loop.load(Ordering::Acquire);
            if !rl.is_null() {
                unsafe { CFRunLoopStop(rl) };
                break;
            }
            if self.thread.as_ref().is_none_or(|t| t.is_finished()) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

extern "C" fn on_value(
    context: *mut c_void,
    _result: i32,
    _sender: *mut c_void,
    value: IOHIDValueRef,
) {
    let pressed = unsafe { &*(context as *const AtomicBool) };
    let (page, usage, v) = unsafe {
        let element = IOHIDValueGetElement(value);
        (
            IOHIDElementGetUsagePage(element),
            IOHIDElementGetUsage(element),
            IOHIDValueGetIntegerValue(value),
        )
    };
    if HOME.contains(&(page, usage)) {
        pressed.store(v != 0, Ordering::Relaxed);
        tracing::debug!(page, usage, value = v, "controller Home button");
    } else if !known(page, usage) {
        tracing::trace!(page, usage, value = v, "controller HID input");
    }
}

fn watch(pressed: Arc<AtomicBool>, run_loop: Arc<AtomicPtr<c_void>>) {
    unsafe {
        let manager = IOHIDManagerCreate(std::ptr::null(), 0);
        if manager.is_null() {
            return;
        }
        // Game pads: Generic Desktop, Game Pad.
        let keys = [
            CFString::from_static_str("DeviceUsagePage"),
            CFString::from_static_str("DeviceUsage"),
        ];
        let values: [CFRetained<CFNumber>; 2] = [CFNumber::new_i32(0x01), CFNumber::new_i32(0x05)];
        let keys: [&CFString; 2] = [&keys[0], &keys[1]];
        let values: [&CFType; 2] = [values[0].as_ref(), values[1].as_ref()];
        let matching = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
        IOHIDManagerSetDeviceMatching(
            manager,
            &*matching as *const CFDictionary<CFString, CFType> as *const c_void,
        );
        IOHIDManagerRegisterInputValueCallback(
            manager,
            on_value,
            Arc::as_ptr(&pressed) as *mut c_void,
        );
        let rl = CFRunLoopGetCurrent();
        IOHIDManagerScheduleWithRunLoop(manager, rl, kCFRunLoopDefaultMode);
        let rc = IOHIDManagerOpen(manager, 0);
        if rc != 0 {
            tracing::warn!(
                error = format!("{rc:#x}"),
                "cannot read controllers' HID reports; the Home button stays with macOS"
            );
            CFRelease(manager);
            return;
        }
        run_loop.store(rl, Ordering::Release);
        CFRunLoopRun();
        IOHIDManagerClose(manager, 0);
        CFRelease(manager);
    }
}
