//! What a Mac host needs macOS to allow: Screen Recording, to capture at all,
//! and Accessibility, for the client's keyboard and mouse to act. Both are
//! granted per app in System Settings > Privacy & Security, to Pong.app, or,
//! run from a terminal, to the terminal.
//!
//! Checked at start. As Pong.app, missing ones are asked for with macOS's own
//! prompts; from a terminal, only reported (the prompt would be for the
//! terminal, which is the user's to decide). `PONG_NO_PROMPTS` has Pong.app
//! only report them too (tests run it while someone works at the Mac).

use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CFType};

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
}

/// Whether macOS lets this process capture the screen.
pub fn screen_capture_allowed() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

fn bundled() -> bool {
    std::env::current_exe().is_ok_and(|p| p.to_string_lossy().contains(".app/Contents/MacOS/"))
}

pub fn check() {
    let bundled = bundled() && std::env::var_os("PONG_NO_PROMPTS").is_none();
    if unsafe { CGPreflightScreenCaptureAccess() } {
        tracing::info!("Screen Recording permission: granted");
    } else {
        if bundled {
            unsafe { CGRequestScreenCaptureAccess() };
        }
        tracing::warn!(
            "no Screen Recording permission: sessions cannot capture this Mac \
                (System Settings > Privacy & Security > Screen & System Audio Recording)"
        );
    }
    let trusted = if bundled {
        let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
        let yes: &CFType = CFBoolean::new(true);
        let options = CFDictionary::<CFString, CFType>::from_slices(&[&*key], &[yes]);
        unsafe {
            AXIsProcessTrustedWithOptions(
                &*options as *const CFDictionary<CFString, CFType> as *const std::ffi::c_void,
            )
        }
    } else {
        pingpong_input::macos::trusted()
    };
    if trusted {
        tracing::info!("Accessibility permission: granted");
    } else {
        tracing::warn!(
            "no Accessibility permission: the client's keyboard and mouse will be ignored \
                (System Settings > Privacy & Security > Accessibility)"
        );
    }
}
