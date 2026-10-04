//! What a Mac host needs macOS to allow: Screen Recording, to capture at all,
//! and Accessibility, for the client's keyboard and mouse to act. Both are
//! granted per app in System Settings > Privacy & Security, to Pong.app, or,
//! run from a terminal, to the terminal. Sound in surround (a Core Audio
//! tap, see `sound`) needs System Audio Recording besides, asked for the
//! first time a client wants surround.
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

/// What the person said to letting Pong record the system's sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    Granted,
    Denied,
    NotAsked,
}

/// `TCCAccessPreflight` and `TCCAccessRequest`, from the private TCC
/// framework: macOS has no public way to ask about System Audio Recording
/// before making a tap (which records silence without it). Looked up at
/// run time, so a macOS without them only loses the question.
fn tcc() -> Option<&'static (usize, usize)> {
    static TCC: std::sync::OnceLock<Option<(usize, usize)>> = std::sync::OnceLock::new();
    TCC.get_or_init(|| {
        // SAFETY: dlopen and dlsym with NUL-terminated names; the handle
        // stays open for the process.
        unsafe {
            let lib = libc::dlopen(
                c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC".as_ptr(),
                libc::RTLD_NOW,
            );
            if lib.is_null() {
                return None;
            }
            let preflight = libc::dlsym(lib, c"TCCAccessPreflight".as_ptr());
            let request = libc::dlsym(lib, c"TCCAccessRequest".as_ptr());
            (!preflight.is_null() && !request.is_null())
                .then_some((preflight as usize, request as usize))
        }
    })
    .as_ref()
}

const AUDIO_CAPTURE: &str = "kTCCServiceAudioCapture";

/// Whether macOS lets this process record the system's sound.
pub fn audio_capture() -> Consent {
    let Some(&(preflight, _)) = tcc() else {
        // Not knowable: let the tap try.
        return Consent::Granted;
    };
    let service = CFString::from_static_str(AUDIO_CAPTURE);
    // SAFETY: TCCAccessPreflight(CFStringRef, CFDictionaryRef) -> int, found
    // above; the string lives for the call.
    let answer = unsafe {
        let f: extern "C" fn(*const CFString, *const std::ffi::c_void) -> i32 =
            std::mem::transmute(preflight);
        f(&*service, std::ptr::null())
    };
    match answer {
        0 => Consent::Granted,
        1 => Consent::Denied,
        _ => Consent::NotAsked,
    }
}

/// Ask for System Audio Recording with macOS's own prompt: as Pong.app
/// only, as `check` asks for the others. The answer is for the next
/// session.
pub fn ask_audio_capture() {
    let bundled = bundled() && std::env::var_os("PONG_NO_PROMPTS").is_none();
    let Some(&(_, request)) = tcc().filter(|_| bundled) else {
        tracing::warn!(
            "sound in surround needs the System Audio Recording permission (System Settings > \
                Privacy & Security > Screen & System Audio Recording)"
        );
        return;
    };
    let service = CFString::from_static_str(AUDIO_CAPTURE);
    let answered = block2::RcBlock::new(|granted: u8| {
        tracing::info!(granted = granted != 0, "System Audio Recording answered");
    });
    // SAFETY: TCCAccessRequest(CFStringRef, CFDictionaryRef, void (^)(Boolean)),
    // found above; it copies the block and the string lives for the call.
    unsafe {
        let f: extern "C" fn(*const CFString, *const std::ffi::c_void, *mut std::ffi::c_void) =
            std::mem::transmute(request);
        f(
            &*service,
            std::ptr::null(),
            block2::RcBlock::as_ptr(&answered).cast(),
        );
    }
    tracing::info!("asked for System Audio Recording, for sound in surround");
}

/// Whether this Mac runs macOS `major` or later (`kern.osproductversion`).
pub fn macos_at_least(major: u32) -> bool {
    let mut buf = [0u8; 32];
    let mut len = buf.len();
    // SAFETY: sysctlbyname writes at most `len` bytes into `buf`, a live local.
    let ok = unsafe {
        libc::sysctlbyname(
            c"kern.osproductversion".as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    } == 0;
    ok && std::str::from_utf8(&buf[..len.min(buf.len())])
        .ok()
        .and_then(|v| {
            v.trim_end_matches('\0')
                .split('.')
                .next()?
                .parse::<u32>()
                .ok()
        })
        .is_some_and(|v| v >= major)
}
