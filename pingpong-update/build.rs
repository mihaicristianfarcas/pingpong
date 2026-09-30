//! What this build was made from, for the update check to compare: the
//! checkout's commit (`PINGPONG_COMMIT`; empty when the source is an archive,
//! as a package manager's is) and whether it is a packaged release
//! (`PINGPONG_RELEASE`, set by `tools/package-macos` and the release
//! workflow).

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn main() {
    println!("cargo:rerun-if-env-changed=PINGPONG_RELEASE");
    println!("cargo:rerun-if-env-changed=PINGPONG_COMMIT");
    let repo =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("set by cargo")).join("..");
    // A packager that builds outside a checkout can still say the commit.
    let commit = std::env::var("PINGPONG_COMMIT")
        .ok()
        .filter(|c| !c.is_empty())
        .or_else(|| {
            // Only this repository's own commit: not that of a checkout the
            // source happens to be unpacked inside.
            let top = git(&repo, &["rev-parse", "--show-toplevel"])?;
            let same = Path::new(&top).canonicalize().ok()? == repo.canonicalize().ok()?;
            same.then(|| git(&repo, &["rev-parse", "HEAD"])).flatten()
        })
        .unwrap_or_default();
    // Rebuild when the checkout moves to another commit: HEAD names the
    // branch, the branch's ref (loose, or packed) names the commit.
    if let Some(git_dir) = git(&repo, &["rev-parse", "--absolute-git-dir"]) {
        let git_dir = PathBuf::from(git_dir);
        let mut watch = vec![git_dir.join("HEAD"), git_dir.join("packed-refs")];
        if let Some(branch) = git(&repo, &["symbolic-ref", "-q", "HEAD"]) {
            watch.push(git_dir.join(branch));
        }
        // Only files that exist: cargo reruns the script on every build for
        // one that does not.
        for path in watch.iter().filter(|p| p.exists()) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let release = std::env::var("PINGPONG_RELEASE").is_ok_and(|v| !v.is_empty() && v != "0");
    println!("cargo:rustc-env=PINGPONG_BUILD_COMMIT={commit}");
    println!(
        "cargo:rustc-env=PINGPONG_BUILD_RELEASE={}",
        if release { "1" } else { "" }
    );
}
