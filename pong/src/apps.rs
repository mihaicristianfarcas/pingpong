//! Apps a session can start with, as Apollo's: something opened as the
//! signed-in user when the stream starts, and closed again when it ends
//! (Apollo's "Steam Big Picture": `steam://open/bigpicture`, undone by
//! `steam://close/bigpicture`).

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ALL_ACCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTUPINFOW,
};

use pingpong_proto::control::app;

pub struct App {
    pub name: &'static str,
    pub open: &'static str,
    pub close: Option<&'static str>,
}

/// What `id` (from the client's request) starts; None for the desktop.
pub fn app(id: u8) -> Option<App> {
    match id {
        app::STEAM_BIG_PICTURE => Some(App {
            name: "Steam Big Picture",
            open: "steam://open/bigpicture",
            close: Some("steam://close/bigpicture"),
        }),
        _ => None,
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Open `target` (a URL, a program) as the user signed in at the console, the
/// way double-clicking it would. The host runs as SYSTEM: Steam, and the
/// user's own apps, must be started as the user.
pub fn open_as_user(target: &str) -> Result<(), String> {
    unsafe {
        let session = WTSGetActiveConsoleSessionId();
        let mut user = HANDLE::default();
        WTSQueryUserToken(session, &mut user).map_err(|e| format!("nobody is signed in ({e})"))?;
        let mut token = HANDLE::default();
        let dup = DuplicateTokenEx(
            user,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut token,
        );
        let _ = CloseHandle(user);
        dup.map_err(|e| format!("DuplicateTokenEx: {e}"))?;

        let mut env: *mut std::ffi::c_void = std::ptr::null_mut();
        let have_env = CreateEnvironmentBlock(&mut env, Some(token), false).is_ok();
        let mut desktop = wide("winsta0\\default");
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            ..Default::default()
        };
        // `start` hands a URL to its registered handler, as Explorer does.
        let mut cmd = wide(&format!("cmd.exe /d /c start \"\" \"{target}\""));
        let mut pi = PROCESS_INFORMATION::default();
        let result = CreateProcessAsUserW(
            Some(token),
            PCWSTR::null(),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW,
            have_env.then_some(env as *const std::ffi::c_void),
            None,
            &si,
            &mut pi,
        );
        if have_env {
            let _ = DestroyEnvironmentBlock(env);
        }
        let _ = CloseHandle(token);
        result.map_err(|e| format!("CreateProcessAsUserW: {e}"))?;
        let _ = CloseHandle(pi.hThread);
        let _ = CloseHandle(pi.hProcess);
        Ok(())
    }
}
