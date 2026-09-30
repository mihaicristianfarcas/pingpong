//! Keeping the host's data folder to the host, on Windows.
//!
//! Pong's data folder there is under `C:\ProgramData`, whose default
//! permissions let every local user read what is in it and create files in
//! it. The folder holds the host's private keys, its rendezvous keys, the web
//! UI's TLS key and credentials, and the paired clients: readable, they let a
//! local user impersonate the host; and a file planted before the host first
//! wrote it (`web-credentials.toml`) would be an admin account.
//!
//! So the folder is private by default: only SYSTEM, Administrators and the
//! folder's owner may read or write it, and everything created in it inherits
//! that. Two files are readable by every user, because Pong's window -- which
//! is not elevated -- needs them to find and trust the host: `config.toml`
//! (the web port) and `web-cert.pem` (the certificate to pin).
//!
//! On macOS and Linux the host runs as its user, in that user's own folder,
//! and secrets are written 0600: nothing to do here.

use std::path::Path;

/// Files in the data folder every local user may read.
#[cfg_attr(not(windows), allow(dead_code))]
const PUBLIC_FILES: [&str; 2] = ["config.toml", "web-cert.pem"];

/// Make `dir` private (see the module's doc), along with what is already in
/// it. Called when the host starts, before it creates anything there.
///
/// Best effort: a caller who may not change the folder's permissions (a
/// user who is not an administrator, listing clients) changes nothing.
pub fn secure_data_dir(dir: &Path) {
    imp::secure_data_dir(dir)
}

/// Let every local user read `path` (one of [`PUBLIC_FILES`], after it is
/// written: a new file is born private).
pub fn make_public(path: &Path) {
    imp::make_public(path)
}

/// A file only SYSTEM and Administrators can read, not even its owner: the
/// host's tokens, which a window that is not elevated must sign in for.
pub fn restrict_to_admins(path: &Path) -> std::io::Result<()> {
    imp::restrict_to_admins(path)
}

#[cfg(windows)]
mod imp {
    use std::path::Path;

    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    use super::PUBLIC_FILES;

    // Security descriptors, in SDDL. `P`: protected (nothing inherited from
    // ProgramData). SY: SYSTEM, BA: Administrators, OW: whoever owns the
    // object (so a host run by hand in a folder of the user's own keeps its
    // files), BU: Users. `OICI`: inherited by files and folders inside.

    /// The data folder: private, and what is created in it is too. Users may
    /// list it and pass through it (to the two public files), no more.
    const FOLDER: PCWSTR =
        w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)(A;;0x1200a9;;;BU)");
    /// A folder inside it (the logs).
    const SUBFOLDER: PCWSTR = w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)");
    /// A file that was there before the folder was private.
    const PRIVATE_FILE: PCWSTR = w!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)");
    /// One of the public files.
    const PUBLIC_FILE: PCWSTR = w!("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;OW)(A;;FR;;;BU)");
    /// The host's tokens.
    const ADMINS_ONLY: PCWSTR = w!("D:P(A;;FA;;;SY)(A;;FA;;;BA)");

    /// Replace `path`'s access list with `sddl`.
    fn set_dacl(path: &Path, sddl: PCWSTR) -> std::io::Result<()> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `sddl` is a NUL-terminated constant; `sd` receives a
        // descriptor the system allocates, used only until it is freed below.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl,
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
            .map_err(std::io::Error::other)?;
            let ok = SetFileSecurityW(
                &HSTRING::from(path.as_os_str()),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                sd,
            );
            let _ = LocalFree(Some(HLOCAL(sd.0)));
            ok.ok().map_err(std::io::Error::other)
        }
    }

    pub fn secure_data_dir(dir: &Path) {
        if let Err(e) = std::fs::create_dir_all(dir).and_then(|()| set_dacl(dir, FOLDER)) {
            // Not ours to change (not an administrator): the host itself,
            // which is, does it when it starts.
            tracing::debug!(dir = %dir.display(), error = %e, "data folder permissions unchanged");
            return;
        }
        // What is already there keeps the permissions it was created with
        // (changing a folder's does not reach into it), so set each.
        secure_entries(dir, true);
    }

    fn secure_entries(dir: &Path, top: bool) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
            let name = entry.file_name();
            let public = top && PUBLIC_FILES.iter().any(|p| name == *p);
            let result = if is_dir {
                set_dacl(&path, SUBFOLDER)
            } else if public {
                set_dacl(&path, PUBLIC_FILE)
            } else if top && is_token(&name) {
                set_dacl(&path, ADMINS_ONLY)
            } else {
                set_dacl(&path, PRIVATE_FILE)
            };
            if let Err(e) = result {
                tracing::warn!(path = %path.display(), error = %e, "could not set permissions");
            }
            if is_dir {
                secure_entries(&path, false);
            }
        }
    }

    /// The files `restrict_to_admins` guards (see `web`): they stay closed
    /// to their owner too.
    fn is_token(name: &std::ffi::OsStr) -> bool {
        name == "local-token" || name == "app-tokens.toml"
    }

    pub fn make_public(path: &Path) {
        if let Err(e) = set_dacl(path, PUBLIC_FILE) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not make the file readable by Pong's window"
            );
        }
    }

    pub fn restrict_to_admins(path: &Path) -> std::io::Result<()> {
        set_dacl(path, ADMINS_ONLY)
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    pub fn secure_data_dir(_dir: &Path) {}

    pub fn make_public(_path: &Path) {}

    pub fn restrict_to_admins(_path: &Path) -> std::io::Result<()> {
        Ok(())
    }
}
