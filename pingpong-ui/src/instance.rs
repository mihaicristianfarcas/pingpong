//! One running copy of an app per data folder: starting it again shows the
//! copy that runs, as apps do, and adds no second window, tray icon or
//! claim on the same files.
//!
//! The data folder is what makes two copies one app: a check run with
//! `PING_DATA_DIR` or `PONG_DATA_DIR` elsewhere is another app as far as
//! this goes, and leaves the user's own alone.
//!
//! - Unix: a socket in the data folder, named for the app. The first copy
//!   listens on it; a
//!   later one that can connect says "show" and leaves. A socket nobody
//!   answers is a crashed copy's, and is replaced.
//! - Windows: a named mutex says a copy runs, a named event tells it to show
//!   itself (both in the `Local\` namespace: one per signed-in session).

use std::path::Path;

/// This process is the app's one copy for as long as this lives.
pub struct Instance {
    #[cfg(unix)]
    socket: Option<std::path::PathBuf>,
    #[cfg(windows)]
    mutex: windows::Win32::Foundation::HANDLE,
}

/// Become the one copy of `app` (a short name: "ping") that keeps its data
/// in `dir`, or, if one runs already, ask it to show itself and return
/// `None` (the caller then exits). `on_show` is called on a thread of its
/// own when a later copy asks.
///
/// Anything that stops the check itself (a folder that cannot hold a
/// socket) lets the app run unguarded rather than not at all.
pub fn claim(dir: &Path, app: &str, on_show: impl Fn() + Send + 'static) -> Option<Instance> {
    platform::claim(dir, app, Box::new(on_show))
}

/// A name for `dir` that is safe in an object's name: FNV-1a of its path.
#[cfg_attr(not(windows), allow(dead_code))]
fn dir_tag(dir: &Path) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in dir.to_string_lossy().to_lowercase().bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(unix)]
mod platform {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::time::Duration;

    use super::Instance;

    const SHOW: &[u8] = b"show";

    pub(super) fn claim(dir: &Path, app: &str, on_show: Box<dyn Fn() + Send>) -> Option<Instance> {
        let path = dir.join(format!("{app}.sock"));
        let _ = std::fs::create_dir_all(dir);
        let unguarded = Some(Instance { socket: None });
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // A copy that runs answers; a crashed one left only the
                // file.
                if let Ok(mut running) = UnixStream::connect(&path) {
                    let _ = running.set_write_timeout(Some(Duration::from_secs(2)));
                    let _ = running.write_all(SHOW);
                    return None;
                }
                let _ = std::fs::remove_file(&path);
                match UnixListener::bind(&path) {
                    Ok(l) => l,
                    Err(_) => return unguarded,
                }
            }
            // A path too long for a socket, a read-only folder: run anyway.
            Err(_) => return unguarded,
        };
        let spawned = std::thread::Builder::new()
            .name("instance".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                    let mut said = [0u8; 4];
                    if stream.read_exact(&mut said).is_ok() && said == SHOW {
                        on_show();
                    }
                }
            });
        if spawned.is_err() {
            let _ = std::fs::remove_file(&path);
            return unguarded;
        }
        Some(Instance { socket: Some(path) })
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            if let Some(path) = &self.socket {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use windows::core::HSTRING;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
        INFINITE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{AllowSetForegroundWindow, ASFW_ANY};

    use super::{dir_tag, Instance};

    /// A handle another thread waits on: handles are the process's, and
    /// waiting on an event is safe from any thread.
    struct SendHandle(HANDLE);
    // SAFETY: see above; the handle is never closed while the thread runs
    // (it is leaked to it).
    unsafe impl Send for SendHandle {}

    pub(super) fn claim(dir: &Path, app: &str, on_show: Box<dyn Fn() + Send>) -> Option<Instance> {
        let tag = dir_tag(dir);
        let mutex_name = HSTRING::from(format!("Local\\pingpong-{app}-{tag}"));
        let event_name = HSTRING::from(format!("Local\\pingpong-{app}-{tag}-show"));
        // SAFETY: named kernel objects, made or opened with valid names;
        // every handle is checked before use.
        unsafe {
            let unguarded = Some(Instance {
                mutex: HANDLE::default(),
            });
            let Ok(mutex) = CreateMutexW(None, false, &mutex_name) else {
                return unguarded;
            };
            if GetLastError() == ERROR_ALREADY_EXISTS {
                // Windows lets a process bring its window to the front only
                // if the one in front allows it: this one was just started
                // by the user, so it can, and hands that to the first copy.
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                // The first copy makes its event right after its mutex: give
                // it a moment if both were started together.
                for _ in 0..20 {
                    if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, &event_name) {
                        let _ = SetEvent(event);
                        let _ = CloseHandle(event);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                let _ = CloseHandle(mutex);
                return None;
            }
            // Auto-reset: each "show" wakes the waiting thread once.
            if let Ok(event) = CreateEventW(None, false, false, &event_name) {
                let event = SendHandle(event);
                let _ = std::thread::Builder::new()
                    .name("instance".into())
                    .spawn(move || {
                        let event = event;
                        while WaitForSingleObject(event.0, INFINITE) == WAIT_OBJECT_0 {
                            on_show();
                        }
                    });
            }
            Some(Instance { mutex })
        }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            if !self.mutex.is_invalid() {
                // SAFETY: the mutex handle `claim` made, closed once.
                let _ = unsafe { CloseHandle(self.mutex) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_is_named_the_same_every_time_and_unlike_another() {
        let a = dir_tag(Path::new("/home/someone/.config/pong"));
        assert_eq!(a, dir_tag(Path::new("/home/someone/.config/pong")));
        assert_ne!(a, dir_tag(Path::new("/home/someone/.config/ping")));
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[cfg(unix)]
    #[test]
    fn a_second_copy_asks_the_first_to_show_itself_and_is_refused() {
        // A short name: a socket's path is limited to about a hundred bytes.
        let dir = std::env::temp_dir().join(format!("pp-inst-{}", std::process::id()));
        let (shown, wait) = std::sync::mpsc::channel();
        let first = claim(&dir, "app", move || {
            let _ = shown.send(());
        });
        assert!(first.is_some());
        assert!(
            claim(&dir, "app", || {}).is_none(),
            "the second copy is refused"
        );
        // Another app keeping its data in the same folder is another app.
        let other = claim(&dir, "other", || {});
        assert!(other.is_some());
        drop(other);
        wait.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the first copy is asked to show itself");
        // The first copy gone, the folder is free again.
        drop(first);
        let again = claim(&dir, "app", || {});
        assert!(again.is_some());
        drop(again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_crashed_copys_socket_does_not_lock_the_app_out() {
        let dir = std::env::temp_dir().join(format!("pp-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // What a copy that died without cleaning up leaves: the file, and
        // nobody listening.
        drop(std::os::unix::net::UnixListener::bind(dir.join("app.sock")).unwrap());
        assert!(dir.join("app.sock").exists());
        assert!(claim(&dir, "app", || {}).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
