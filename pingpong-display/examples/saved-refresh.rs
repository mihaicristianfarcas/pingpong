//! Set the refresh rate Windows remembers for the host's own monitor while
//! Pong's virtual display is connected: the arrangement Windows applies the
//! moment the virtual display next arrives. A repair tool for that saved
//! arrangement. Run in the console session (`tools/host-run.ps1`) with Pong
//! stopped:
//!
//!   saved-refresh HZ

#[cfg(windows)]
fn main() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use pingpong_display::sudovda::Device;
    use pingpong_display::windows::{gdi_name_for_target, CcdId};
    use pingpong_display::DisplayMode;
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::*;

    let hz: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .expect("usage: saved-refresh HZ");

    // The watchdog reaps the monitor within 3 s of the last ping.
    let stop = Arc::new(AtomicBool::new(false));
    let keepalive = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let d = Device::open().expect("open SudoVDA");
            while !stop.load(Ordering::Relaxed) {
                let _ = d.ping();
                std::thread::sleep(Duration::from_millis(500));
            }
        })
    };
    let device = Device::open().expect("open SudoVDA");
    let added = device
        .add(DisplayMode {
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
        })
        .expect("add");
    let id = CcdId {
        adapter_low: added.adapter_luid_low,
        adapter_high: added.adapter_luid_high,
        target_id: added.target_id,
    };
    let started = Instant::now();
    let vdd = loop {
        if let Some(name) = gdi_name_for_target(id) {
            break name;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the virtual display never attached"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    println!(
        "virtual display {vdd} after {} ms",
        started.elapsed().as_millis()
    );

    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let mut i = 0;
    loop {
        let mut dd = DISPLAY_DEVICEW {
            cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut dd, 0) }.as_bool() {
            break;
        }
        i += 1;
        let name = String::from_utf16_lossy(&dd.DeviceName)
            .trim_end_matches('\0')
            .to_string();
        if dd.StateFlags.0 & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0 == 0 || name == vdd {
            continue;
        }
        let name_w = wide(&name);
        let mut dm = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        if !unsafe { EnumDisplaySettingsW(PCWSTR(name_w.as_ptr()), ENUM_CURRENT_SETTINGS, &mut dm) }
            .as_bool()
        {
            continue;
        }
        println!(
            "{name}: {}x{} @ {} Hz",
            dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency
        );
        dm.dmDisplayFrequency = hz;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        let rc = unsafe {
            ChangeDisplaySettingsExW(
                PCWSTR(name_w.as_ptr()),
                Some(&dm),
                None,
                CDS_UPDATEREGISTRY | CDS_GLOBAL,
                None,
            )
        };
        println!("{name}: set to {hz} Hz and saved: {rc:?}");
    }

    std::thread::sleep(Duration::from_secs(2));
    println!("remove: {:?}", device.remove());
    stop.store(true, Ordering::Relaxed);
    let _ = keepalive.join();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
