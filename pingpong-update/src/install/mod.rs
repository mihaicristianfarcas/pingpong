//! Installing a newer release from the app itself: "Install and Restart" in
//! the update sheet.
//!
//! The release's own archive for this program and system is downloaded
//! from GitHub (the files the website and the Homebrew casks hand out too),
//! and its SHA-256 checked against the one GitHub keeps for it (the API's
//! `digest`, worked out by GitHub when the file was uploaded). On a Mac the
//! new app must also carry a Developer ID signature of the same team as the
//! app that installs it. Then each system puts it in place its own way:
//!
//! - **Windows**, Ping: its files are replaced where it runs (a running
//!   program cannot be overwritten, but it can be renamed aside). Pong: the
//!   archive's `install.ps1` runs as an administrator (Windows asks), as
//!   when Pong was installed, and opens Pong's window again after.
//! - **macOS**: the app bundles are swapped for the new ones, and the host
//!   restarted. A copy Homebrew installed is upgraded by Homebrew
//!   (`brew upgrade --cask`), so that Homebrew's record stays true.
//! - **Linux**: the archive's `install.sh`, as when it was installed.
//!
//! The app then quits, and its new copy starts with `--after-update PID`:
//! it waits for the old one to be gone ([`after_update`]) before it claims
//! being the one copy that runs. A note in the app's data folder
//! (`update-pending.toml`) says which version was being installed: the next
//! start says whether it was ([`finish`]).
//!
//! Only a packaged release installs updates: a build from a checkout is
//! updated the way it was made.

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::github::{self, Asset, Fetch};
use crate::{Build, Status, Version, REPOSITORY};

/// Which program installs itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    Ping,
    /// Pong: the host and its window, which installs both.
    Pong,
}

impl Program {
    /// Its name, which is also how its release archives begin.
    pub fn name(self) -> &'static str {
        match self {
            Program::Ping => "Ping",
            Program::Pong => "Pong",
        }
    }

    /// The Homebrew cask that installs it on a Mac.
    pub fn cask(self) -> &'static str {
        match self {
            Program::Ping => "ping",
            Program::Pong => "pong",
        }
    }

    /// The release archive of `version` for this system, as the packaging
    /// scripts name it: `Ping-0.9.0-windows-x86_64.zip`.
    pub fn archive(self, version: &str) -> String {
        archive_name(self, version, std::env::consts::OS, std::env::consts::ARCH)
    }
}

fn archive_name(program: Program, version: &str, os: &str, arch: &str) -> String {
    // tools/package-macos names Apple silicon as `uname -m` does.
    let arch = match (os, arch) {
        ("macos", "aarch64") => "arm64",
        (_, arch) => arch,
    };
    let ext = if os == "linux" { "tar.gz" } else { "zip" };
    format!("{}-{version}-{os}-{arch}.{ext}", program.name())
}

/// How far an installation has come, for the window to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// Asking GitHub for the release's files.
    Starting,
    /// Bytes downloaded so far, of how many.
    Downloading { done: u64, total: u64 },
    /// Unpacked, checked; being put in place.
    Installing,
    /// Done here: the app quits, and the new copy (or the installer that
    /// puts it in place) takes over.
    Restarting,
    /// Why it was not installed: the installer's own words, as a rule.
    Failed(String),
}

impl Install {
    /// Still under way.
    pub fn busy(&self) -> bool {
        !matches!(self, Install::Failed(_))
    }
}

static AVAILABLE: OnceLock<Result<(), String>> = OnceLock::new();

/// Have [`available`] say yes, whatever this copy is (the UI demos, to show
/// the sheet as a release shows it). Only before it is first asked.
pub fn pretend_available() {
    let _ = AVAILABLE.set(Ok(()));
}

/// Whether this copy of `program` can install updates itself, and if not,
/// why: a sentence for the update sheet. Worked out once (it looks at where
/// the app is and whether that folder can be written).
pub fn available(program: Program, build: &Build) -> Result<(), String> {
    AVAILABLE
        .get_or_init(|| {
            if !build.release {
                return Err(
                    "It was built from a checkout: pull main and build again to update.".into(),
                );
            }
            platform::method(program).map(|_| ())
        })
        .clone()
}

/// What an update the app started says to its next start.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct Pending {
    /// The version being installed.
    version: String,
}

impl Pending {
    fn path(dir: &Path) -> PathBuf {
        dir.join("update-pending.toml")
    }

    fn save(&self, dir: &Path) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        std::fs::write(Pending::path(dir), text).map_err(|e| e.to_string())
    }

    fn take(dir: &Path) -> Option<Pending> {
        let path = Pending::path(dir);
        let text = std::fs::read_to_string(&path).ok()?;
        let _ = std::fs::remove_file(&path);
        toml::from_str(&text).ok()
    }
}

/// Where an installer's output goes: read back when the update did not
/// take.
pub(crate) fn log_path(dir: &Path) -> PathBuf {
    dir.join("update.log")
}

/// Where an update is downloaded and unpacked, apart from the app's data
/// (on Windows that is the roaming profile, no place for an archive).
fn staging(program: Program, version: &str) -> PathBuf {
    std::env::temp_dir()
        .join("pingpong-update")
        .join(format!("{}-{version}", program.name()))
}

/// The start after an update the app began: was it installed? `None` when
/// none was pending; otherwise what to show if it was not. The staging
/// folder goes either way.
pub(crate) fn finish(program: Program, build: &Build, dir: &Path) -> Option<Install> {
    platform::tidy(program);
    let pending = Pending::take(dir)?;
    let _ = std::fs::remove_dir_all(staging(program, &pending.version));
    let (Some(wanted), Some(mine)) = (
        Version::parse(&pending.version),
        Version::parse(build.version),
    ) else {
        return None;
    };
    if mine >= wanted {
        tracing::info!(version = build.version, "update installed");
        return None;
    }
    let log = std::fs::read_to_string(log_path(dir)).unwrap_or_default();
    let why = last_words(&log);
    tracing::warn!(version = %pending.version, why, "the update was not installed");
    Some(Install::Failed(match why {
        "" => "its installer stopped without saying why".into(),
        why => why.to_string(),
    }))
}

/// The last line of an installer's output that says something: its error,
/// as a rule. PowerShell follows an error with where it happened (`At
/// line…`, `+ …`): that is skipped.
fn last_words(log: &str) -> &str {
    log.lines()
        .map(|l| l.trim().trim_start_matches('\u{feff}'))
        .rev()
        .find(|l| !l.is_empty() && !l.starts_with('+') && !l.starts_with("At "))
        .unwrap_or("")
}

/// The longest archive taken (Ping for Windows, with FFmpeg, is ~50 MB).
const MAX_ARCHIVE: u64 = 1024 * 1024 * 1024;

/// Write `from` to `to` while hashing it, and keep it only if it is the
/// file the release lists: its size and SHA-256. `progress` hears the bytes
/// so far.
fn save_verified(
    mut from: impl Read,
    to: &Path,
    asset: &Asset,
    progress: &dyn Fn(u64),
) -> Result<(), String> {
    let mut file = std::fs::File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    let result = loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => break Err(format!("the download broke off: {e}")),
        };
        done += n as u64;
        if done > asset.size {
            break Err("the download is longer than the release says".into());
        }
        hash.update(&buf[..n]);
        if let Err(e) = file.write_all(&buf[..n]) {
            break Err(format!("{}: {e}", to.display()));
        }
        progress(done);
    };
    let result = result.and_then(|()| {
        if done != asset.size {
            return Err(format!(
                "the download ended at {done} of {} bytes",
                asset.size
            ));
        }
        let sum: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if sum != asset.sha256 {
            return Err(
                "the download is not the file the release lists (its SHA-256 differs)".into(),
            );
        }
        file.sync_all().map_err(|e| e.to_string())
    });
    if result.is_err() {
        drop(file);
        let _ = std::fs::remove_file(to);
    }
    result
}

/// Install the latest release of `program`, on the calling thread (the
/// update check's install thread), saying how it goes in `status`. `dir`
/// is the app's data folder. On success the status is
/// [`Install::Restarting`] and the app must quit.
pub(crate) fn run(
    program: Program,
    build: &Build,
    dir: &Path,
    status: &Mutex<Status>,
    wake: &dyn Fn(),
) {
    let say = |install: Install| {
        status.lock().install = Some(install);
        wake();
    };
    say(Install::Starting);
    match install(
        program,
        build,
        dir,
        &|done, total| say(Install::Downloading { done, total }),
        &|| say(Install::Installing),
    ) {
        Ok(()) => {
            tracing::info!("update in place; restarting");
            say(Install::Restarting);
        }
        Err(e) => {
            tracing::warn!(error = e, "update not installed");
            say(Install::Failed(e));
        }
    }
}

fn install(
    program: Program,
    build: &Build,
    dir: &Path,
    downloading: &dyn Fn(u64, u64),
    installing: &dyn Fn(),
) -> Result<(), String> {
    available(program, build)?;
    let method = platform::method(program)?;
    let api = github::Api::new(build);
    let slug = github::slug(REPOSITORY).ok_or("this build does not know its repository")?;
    let (latest, _) =
        github::latest(&api as &dyn Fetch, slug)?.ok_or("GitHub lists no release to install")?;
    let mine = Version::parse(build.version).ok_or("this build's version is not a version")?;
    if latest.version <= mine {
        return Err(format!(
            "{} {} is the newest release: there is nothing to install",
            program.name(),
            build.version
        ));
    }
    let version = latest.version.to_string();
    let stage = staging(program, &version);
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    if !platform::needs_archive(&method) {
        // A package manager's upgrade: it fetches and checks its own.
        installing();
        return place(&method, program, &version, &stage, &stage, dir);
    }
    let name = program.archive(&version);
    let asset = latest
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| format!("release {version} has no {name} with a checksum"))?;
    if asset.size > MAX_ARCHIVE {
        return Err(format!("{name} is larger than any release should be"));
    }

    let archive = stage.join(&asset.name);
    tracing::info!(version, archive = %asset.name, "downloading the update");
    downloading(0, asset.size);
    let body = api.download(&asset.url, asset.size)?;
    // Said at each whole percent: a read is as small as a TLS record, and
    // each saying redraws the window.
    let said = std::cell::Cell::new(0u64);
    save_verified(body, &archive, asset, &|done| {
        let percent = done * 100 / asset.size.max(1);
        if percent != said.get() || done == asset.size {
            said.set(percent);
            downloading(done, asset.size);
        }
    })?;

    installing();
    let unpacked = stage.join("unpacked");
    std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    platform::unpack(&archive, &unpacked)?;
    place(&method, program, &version, &unpacked, &stage, dir)
}

/// The last step, which may hand over to an installer that outlives the
/// app: the note for the next start first.
fn place(
    method: &platform::Method,
    program: Program,
    version: &str,
    unpacked: &Path,
    stage: &Path,
    dir: &Path,
) -> Result<(), String> {
    let _ = std::fs::write(log_path(dir), "");
    Pending {
        version: version.to_string(),
    }
    .save(dir)
    .map_err(|e| format!("the update's note could not be written: {e}"))?;
    let placed = platform::place(method, program, unpacked, stage, dir);
    if placed.is_err() {
        let _ = Pending::take(dir);
    }
    placed
}

/// A check of the installer that leaves the installed apps alone
/// (`examples/install.rs`): the latest release's archive of `program` for
/// this system, downloaded and checked as an update is, unpacked, and put
/// in `folder` the way the app would put it in its own (macOS: the bundles
/// swapped, signed by the team that signed those already in `folder`;
/// Windows: Ping's files replaced; Linux: only unpacked, as install.sh
/// would restart the user's own host). Nothing quits or opens. Returns the
/// version.
pub fn rehearse(
    program: Program,
    folder: &Path,
    progress: &dyn Fn(u64, u64),
) -> Result<String, String> {
    let build = Build::this();
    let api = github::Api::new(&build);
    let slug = github::slug(REPOSITORY).ok_or("this build does not know its repository")?;
    let (latest, _) =
        github::latest(&api as &dyn Fetch, slug)?.ok_or("GitHub lists no release to install")?;
    let version = latest.version.to_string();
    let name = program.archive(&version);
    let asset = latest
        .assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| format!("release {version} has no {name} with a checksum"))?;
    let stage = std::env::temp_dir()
        .join("pingpong-update-rehearsal")
        .join(format!("{}-{version}", program.name()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    let archive = stage.join(&asset.name);
    let body = api.download(&asset.url, asset.size)?;
    save_verified(body, &archive, asset, &|done| progress(done, asset.size))?;
    let unpacked = stage.join("unpacked");
    std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    platform::unpack(&archive, &unpacked)?;
    std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    platform::rehearse(program, &unpacked, folder)?;
    let _ = std::fs::remove_dir_all(&stage);
    Ok(version)
}

/// Run `command`, its output to the update log; an error says how it
/// ended.
#[allow(dead_code)] // not every system's installer runs one
pub(crate) fn run_logged(mut command: std::process::Command, dir: &Path) -> Result<(), String> {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(dir))
        .map_err(|e| e.to_string())?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let status = command
        .stdout(log)
        .stderr(err)
        .stdin(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("{command:?} did not start: {e}"))?;
    if status.success() {
        return Ok(());
    }
    let text = std::fs::read_to_string(log_path(dir)).unwrap_or_default();
    Err(match last_words(&text) {
        "" => format!("the installer ended with {status}"),
        why => why.to_string(),
    })
}

/// Called first thing by an app, before it claims being the one copy that
/// runs: started as an update's new copy (`--after-update PID`), it waits
/// for the old copy to quit; otherwise this returns at once.
pub fn after_update() {
    let mut args = std::env::args().skip_while(|a| a != "--after-update");
    if args.next().is_none() {
        return;
    }
    if let Some(pid) = args.next().and_then(|p| p.parse::<u32>().ok()) {
        platform::wait_for_exit(pid, AFTER_UPDATE_WAIT);
    }
}

/// How long a new copy waits for the old one to quit. Quitting takes it
/// well under a second; a stream ending takes a moment more.
const AFTER_UPDATE_WAIT: Duration = Duration::from_secs(20);

/// A string as a POSIX shell reads it literally: single-quoted.
#[allow(dead_code)] // the Unix installers' scripts
pub(crate) fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A string as PowerShell reads it literally: single-quoted.
#[allow(dead_code)] // the Windows installer's script
pub(crate) fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(data: &[u8]) -> Asset {
        Asset {
            name: "Ping-0.10.0-linux-x86_64.tar.gz".into(),
            url: String::new(),
            size: data.len() as u64,
            sha256: Sha256::digest(data)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        }
    }

    #[test]
    fn each_system_gets_the_archive_its_packaging_script_makes() {
        assert_eq!(
            archive_name(Program::Ping, "0.10.0", "windows", "x86_64"),
            "Ping-0.10.0-windows-x86_64.zip"
        );
        assert_eq!(
            archive_name(Program::Pong, "0.10.0", "macos", "aarch64"),
            "Pong-0.10.0-macos-arm64.zip"
        );
        assert_eq!(
            archive_name(Program::Pong, "0.10.0", "macos", "x86_64"),
            "Pong-0.10.0-macos-x86_64.zip"
        );
        assert_eq!(
            archive_name(Program::Ping, "0.10.0", "linux", "x86_64"),
            "Ping-0.10.0-linux-x86_64.tar.gz"
        );
    }

    #[test]
    fn a_download_is_kept_only_when_it_is_the_file_the_release_lists() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("archive");
        let data = b"the release's archive".to_vec();
        let seen = std::cell::Cell::new(0);
        save_verified(&data[..], &to, &asset(&data), &|n| seen.set(n)).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), data);
        assert_eq!(seen.get(), data.len() as u64);

        // Another file of the same size.
        let other = b"the attacker's archiv".to_vec();
        assert_eq!(other.len(), data.len());
        let err = save_verified(&other[..], &to, &asset(&data), &|_| {}).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert!(!to.exists());

        // Cut short, and too long.
        let err = save_verified(&data[..5], &to, &asset(&data), &|_| {}).unwrap_err();
        assert!(err.contains("ended at 5"), "{err}");
        let mut longer = data.clone();
        longer.extend_from_slice(b"and more");
        let err = save_verified(&longer[..], &to, &asset(&data), &|_| {}).unwrap_err();
        assert!(err.contains("longer"), "{err}");
        assert!(!to.exists());
    }

    #[test]
    fn the_next_start_says_whether_the_update_was_installed() {
        let dir = tempfile::tempdir().unwrap();
        let old = Build {
            version: "0.9.0",
            commit: "",
            release: true,
        };
        let new = Build {
            version: "0.10.0",
            ..old
        };
        // Nothing pending: nothing to say.
        assert_eq!(finish(Program::Ping, &new, dir.path()), None);

        Pending {
            version: "0.10.0".into(),
        }
        .save(dir.path())
        .unwrap();
        assert_eq!(finish(Program::Ping, &new, dir.path()), None);
        // Said once.
        assert!(!Pending::path(dir.path()).exists());

        Pending {
            version: "0.10.0".into(),
        }
        .save(dir.path())
        .unwrap();
        std::fs::write(
            log_path(dir.path()),
            "Copying files\nCopy-Item : Access to the path is denied.\n+ CategoryInfo\n\n",
        )
        .unwrap();
        // Still the old version: the installer's last word is why.
        assert_eq!(
            finish(Program::Pong, &old, dir.path()),
            Some(Install::Failed(
                "Copy-Item : Access to the path is denied.".into()
            ))
        );
    }

    #[test]
    fn quoted_strings_stay_whole_in_a_shell_and_in_powershell() {
        assert_eq!(
            sh_quote("/Users/o'brien/Ping.app"),
            r"'/Users/o'\''brien/Ping.app'"
        );
        assert_eq!(ps_quote(r"C:\Users\o'brien"), r"'C:\Users\o''brien'");
    }
}
