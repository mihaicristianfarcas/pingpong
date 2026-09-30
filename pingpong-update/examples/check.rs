//! Asks GitHub once what the apps' update check would, and prints the answer:
//! what this build is, and whether a newer release or newer commits on main
//! exist.
//!
//!   cargo run -p pingpong-update --example check [releases|main]
//!
//! Nothing is kept: the answer goes to a temporary folder.

use std::time::Duration;

use pingpong_update::{Build, Channel, Checker};

fn main() {
    let build = Build::this();
    let channel = match std::env::args().nth(1).as_deref() {
        Some("releases") => Channel::Releases,
        Some("main") => Channel::Main,
        None => Channel::default_for(&build),
        Some(other) => {
            eprintln!("unknown channel {other}\n\nusage: check [releases|main]");
            std::process::exit(2);
        }
    };
    println!(
        "this build: {} (release: {}), following {channel:?}",
        build.describe(),
        build.release
    );
    let dir = std::env::temp_dir().join(format!("pingpong-update-check-{}", std::process::id()));
    let (done, wait) = std::sync::mpsc::channel();
    let checker = Checker::start(build, dir.clone(), channel, move || {
        let _ = done.send(());
    });
    checker.check_now();
    // The first wake says the check began; the answer follows.
    loop {
        if wait.recv_timeout(Duration::from_secs(40)).is_err() {
            eprintln!("no answer");
            break;
        }
        let status = checker.status();
        if status.checking || (status.checked_unix == 0 && status.error.is_none()) {
            continue;
        }
        match (&status.update, &status.error) {
            (_, Some(e)) => println!("failed: {e}"),
            (Some(u), None) => {
                println!("{}\n{}\n{}", u.headline("pingpong"), u.url(), u.how("ping"))
            }
            (None, None) => println!("up to date"),
        }
        break;
    }
    let _ = std::fs::remove_dir_all(dir);
}
