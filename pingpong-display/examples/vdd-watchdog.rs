//! Add the virtual display, ping the SudoVDA watchdog for a few seconds and
//! report whether the monitor survives, then remove it. A debugging aid for
//! monitors that vanish ~3 s after they are added.

#[cfg(windows)]
fn main() {
    use std::time::{Duration, Instant};

    use pingpong_display::sudovda::Device;
    use pingpong_display::DisplayMode;

    let device = Device::open().expect("open SudoVDA");
    println!("watchdog before add: {:?}", device.watchdog());
    let added = device
        .add(DisplayMode {
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
        })
        .expect("add");
    println!("added {added:?}");
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(8) {
        std::thread::sleep(Duration::from_millis(500));
        let ping = device.ping();
        println!(
            "{:>5} ms ping={ping:?} watchdog={:?}",
            started.elapsed().as_millis(),
            device.watchdog()
        );
    }
    println!("remove: {:?}", device.remove());
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
