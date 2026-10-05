//! Crash-restore state (v2 design §6.4).
//!
//! If the host dies with a display in a mode it set, nothing puts it back.
//! Writing the pre-session state to disk means the next start can.
//!
//! With SudoVDA the virtual display itself self-heals -- the driver's watchdog
//! reaps it within ~3 s once nothing is pinging (v2 design §6.1). What does NOT
//! self-heal is the primary-display assignment and desktop arrangement that
//! `SetDisplayConfig` changed, so that is what this records.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// The display that was primary before the session started.
///
/// Not a `DisplayMode`: with SudoVDA the physical display's own mode is never
/// touched (v2 design §6.3), so there is no resolution to put back. What the session
/// changes -- and what a crash therefore strands -- is the primary assignment.
///
/// The three integers are an opaque platform identity. On Windows they are the
/// CCD adapter LUID and target id, which is the only handle that survives
/// display re-enumeration; GDI names like `\\.\DISPLAY1` do not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedState {
    pub primary_adapter_low: u32,
    pub primary_adapter_high: i32,
    pub primary_target_id: u32,
    /// Whether the session deactivated the host's other displays.
    ///
    /// A session owns the desktop: leaving the physical displays attached means
    /// apps still open on them and remember positions there, which is the
    /// "windows keep appearing on my other monitor" fault. Turning them off is
    /// what Sunshine's "deactivate other displays" (`ensure_only_display`) does,
    /// and Apollo's default, and why they do not have that problem.
    ///
    /// It is also the one change here a crash strands VISIBLY -- a dead server
    /// leaves the host's monitors dark, which is much worse than a stranded
    /// primary assignment. `#[serde(default)]` so a state file written by an
    /// older build still loads rather than being read as corrupt and ignored,
    /// which would strand exactly what this exists to recover.
    #[serde(default)]
    pub displays_disabled: bool,
}

pub fn save(path: &Path, s: &SavedState) -> std::io::Result<()> {
    let text = toml::to_string(s).map_err(std::io::Error::other)?;
    std::fs::write(path, text)
}

/// Returns `None` for absent OR unreadable OR corrupt state.
///
/// All three mean the same thing to the caller -- "nothing to restore" -- and
/// a half-written file after a crash is precisely the case this module exists
/// for, so it must not be an error path.
pub fn load(path: &Path) -> Option<SavedState> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

pub fn clear(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("pingpong-display-test-{name}.toml"));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn saved_state_round_trips() {
        let path = tmp("round-trip");
        let s = SavedState {
            primary_adapter_low: 112_252,
            primary_adapter_high: 0,
            primary_target_id: 256,
            displays_disabled: true,
        };
        save(&path, &s).expect("saves");
        assert_eq!(load(&path), Some(s));
    }

    #[test]
    fn missing_file_is_none_not_an_error() {
        // "No stale state" is the normal startup case (v2 design §6.4).
        assert_eq!(load(&tmp("absent")), None);
    }

    #[test]
    fn corrupt_file_is_none_not_a_panic() {
        // A half-written file after a crash is exactly the case this exists for.
        let path = tmp("corrupt");
        std::fs::write(&path, b"\xff\xfe not toml at all {{{").expect("writes");
        assert_eq!(load(&path), None);
    }

    #[test]
    fn clear_removes_and_is_idempotent() {
        let path = tmp("clear");
        let s = SavedState {
            primary_adapter_low: 1,
            primary_adapter_high: 0,
            primary_target_id: 0,
            displays_disabled: false,
        };
        save(&path, &s).expect("saves");
        clear(&path);
        assert_eq!(load(&path), None);
        clear(&path); // must not panic on an already-absent file
    }
}
