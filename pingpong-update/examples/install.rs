//! Installs the latest release of Ping or Pong into a folder of your
//! choosing the way the apps install an update into their own (see
//! `pingpong_update::install::rehearse`), to check the installer without
//! touching the installed apps:
//!
//!   cargo run -p pingpong-update --example install -- ping|pong FOLDER
//!
//! On a Mac, put the release's apps of an older version in FOLDER first:
//! the new ones must be signed by the same team.

use pingpong_update::install::{rehearse, Program};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (program, folder) = match args.as_slice() {
        [p, folder] if p == "ping" => (Program::Ping, folder),
        [p, folder] if p == "pong" => (Program::Pong, folder),
        _ => {
            eprintln!("usage: install ping|pong FOLDER");
            std::process::exit(2);
        }
    };
    let last = std::cell::Cell::new(0u64);
    let result = rehearse(program, std::path::Path::new(folder), &|done, total| {
        // A line per 10%.
        let step = total / 10 + 1;
        if done / step != last.get() / step || done == total {
            println!("downloaded {done} of {total} bytes");
        }
        last.set(done);
    });
    match result {
        Ok(version) => println!("installed {} {version} in {folder}", program.name()),
        Err(e) => {
            eprintln!("not installed: {e}");
            std::process::exit(1);
        }
    }
}
