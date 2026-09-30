//! Starting the host from its window, on a Mac: a copy installed by a
//! package manager has had no script run for it, so the app itself sets the
//! host up to run whenever the user is logged in (the LaunchAgent
//! `tools/build-pong-app --install` writes) and starts it.
//!
//! Only a Mac: on Windows the host is a service that an administrator
//! installs (`pong install`), on Linux a systemd user unit; the window says
//! how, and cannot do either for the user.

#[cfg(target_os = "macos")]
use pingpong_ui::login::LoginItem;

/// The host at login, kept running: `Pong.app`'s executable as `pong host`.
#[cfg(target_os = "macos")]
const HOST: LoginItem = LoginItem {
    id: "dev.pingpong.Pong",
    name: "Pong",
    args: &["host"],
    keep_alive: true,
};

/// The host's executable: in `Pong.app`, beside the bundle this app is in.
#[cfg(target_os = "macos")]
fn host_binary() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // Pong Control.app/Contents/MacOS/Pong Control
    let bundle = exe.parent()?.parent()?.parent()?;
    if bundle.extension()? != "app" {
        return None;
    }
    let host = bundle.parent()?.join("Pong.app/Contents/MacOS/pong");
    host.is_file().then_some(host)
}

/// Whether the window can start the host itself.
pub fn can_start() -> bool {
    #[cfg(target_os = "macos")]
    return host_binary().is_some();
    #[cfg(not(target_os = "macos"))]
    false
}

/// Set the host to run whenever the user is logged in, and start it now.
pub fn start() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let host = host_binary().ok_or("Pong.app is not beside this app.")?;
        tracing::info!(host = %host.display(), "starting the host, now and at login");
        HOST.set(&host, true)?;
        HOST.start_now()
    }
    #[cfg(not(target_os = "macos"))]
    Err("The host is not started from its window on this system.".into())
}
