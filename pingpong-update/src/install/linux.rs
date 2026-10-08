//! Installing an update on Linux: the release archive's `install.sh`, as
//! the first install ran it. It puts the programs in `~/.local/bin` (over
//! running ones: `install` replaces a file rather than writing into it)
//! and restarts Pong's user service.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use super::{run_logged, Program};

/// How this copy is updated: the programs go to `bin`.
pub(crate) struct Method {
    bin: PathBuf,
}

/// The program that opens the app again.
fn app(program: Program) -> &'static str {
    match program {
        Program::Ping => "ping-app",
        Program::Pong => "pong-app",
    }
}

pub(crate) fn method(_program: Program) -> Result<Method, String> {
    // Where install.sh puts the programs.
    let bin = std::env::var_os("XDG_BIN_HOME")
        .filter(|b| !b.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(|h| PathBuf::from(h).join(".local/bin"))
        })
        .ok_or("There is no home folder to install to.")?;
    Ok(Method { bin })
}

pub(crate) fn needs_archive(_method: &Method) -> bool {
    true
}

pub(crate) fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let out = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(into)
        .output()
        .map_err(|e| format!("tar did not run: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "the archive did not unpack: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Run the archive's install.sh, then start the new copy.
pub(crate) fn place(
    method: &Method,
    program: Program,
    unpacked: &Path,
    _stage: &Path,
    dir: &Path,
) -> Result<(), String> {
    // The archive is one folder: Ping-0.9.0-linux-x86_64/.
    let folder = std::fs::read_dir(unpacked)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.join("install.sh").is_file())
        .ok_or("the archive has no install.sh")?;
    let mut install = Command::new("bash");
    install.arg(folder.join("install.sh")).current_dir(&folder);
    run_logged(install, dir)?;
    let new = method.bin.join(app(program));
    Command::new(&new)
        .args(["--after-update", &std::process::id().to_string()])
        .spawn()
        .map(drop)
        .map_err(|e| format!("{} did not start: {e}", new.display()))
}

/// `install::rehearse`: the archive's folder copied to `folder`, its
/// install.sh not run (it would restart the user's own host).
pub(crate) fn rehearse(_program: Program, unpacked: &Path, folder: &Path) -> Result<(), String> {
    let out = Command::new("cp")
        .arg("-a")
        .arg(format!("{}/.", unpacked.display()))
        .arg(folder)
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Nothing is left behind: install.sh replaces the programs in place.
pub(crate) fn tidy(_program: Program) {}

/// Wait until process `pid` has exited, or `timeout`.
pub(crate) fn wait_for_exit(pid: u32, timeout: Duration) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    let until = Instant::now() + timeout;
    // SAFETY: signal 0 only asks whether the process exists.
    while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn script(path: &Path, text: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{text}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_archives_install_sh_runs_then_the_new_app_starts_after_this_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (unpacked, bin, data) = (
            tmp.path().join("unpacked"),
            tmp.path().join("bin"),
            tmp.path().join("data"),
        );
        let folder = unpacked.join("Ping-0.10.0-linux-x86_64");
        for d in [&folder, &bin, &data] {
            std::fs::create_dir_all(d).unwrap();
        }
        let installed = tmp.path().join("installed");
        script(
            &folder.join("install.sh"),
            &format!("echo installing; touch {}", installed.display()),
        );
        let started = tmp.path().join("started");
        script(
            &bin.join("ping-app"),
            &format!("echo \"$@\" > {}", started.display()),
        );
        place(&Method { bin }, Program::Ping, &unpacked, tmp.path(), &data).unwrap();
        assert!(installed.exists());
        assert_eq!(
            std::fs::read_to_string(super::super::log_path(&data)).unwrap(),
            "installing\n"
        );
        // The new copy is started, told to wait for this one.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !started.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            std::fs::read_to_string(&started).unwrap().trim(),
            format!("--after-update {}", std::process::id())
        );
    }

    #[test]
    fn an_install_sh_that_fails_says_why_and_starts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (unpacked, bin, data) = (
            tmp.path().join("unpacked"),
            tmp.path().join("bin"),
            tmp.path().join("data"),
        );
        let folder = unpacked.join("Pong-0.10.0-linux-x86_64");
        for d in [&folder, &bin, &data] {
            std::fs::create_dir_all(d).unwrap();
        }
        script(
            &folder.join("install.sh"),
            "echo 'These libraries are missing:' >&2; echo '  libavcodec.so.60' >&2; exit 1",
        );
        let err = place(&Method { bin }, Program::Pong, &unpacked, tmp.path(), &data).unwrap_err();
        assert_eq!(err, "libavcodec.so.60");
    }
}
