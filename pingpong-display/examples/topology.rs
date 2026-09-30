//! Print what Windows' display configuration holds: the projection the
//! database would apply, and every available path, active or not. A debugging
//! aid for virtual displays that arrive but never join the desktop. Run it in
//! the console session (`tools/host-run.ps1`), not over SSH.

#[cfg(windows)]
fn main() {
    use windows::Win32::Devices::Display::*;
    use windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_ACTIVE;

    unsafe {
        for (label, flags) in [("all", QDC_ALL_PATHS), ("database", QDC_DATABASE_CURRENT)] {
            let (mut n_paths, mut n_modes) = (0u32, 0u32);
            if GetDisplayConfigBufferSizes(flags, &mut n_paths, &mut n_modes).is_err() {
                println!("{label}: GetDisplayConfigBufferSizes failed");
                continue;
            }
            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_paths as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_modes as usize];
            let mut topology = DISPLAYCONFIG_TOPOLOGY_ID::default();
            let rc = QueryDisplayConfig(
                flags,
                &mut n_paths,
                paths.as_mut_ptr(),
                &mut n_modes,
                modes.as_mut_ptr(),
                if flags == QDC_DATABASE_CURRENT {
                    Some(&mut topology)
                } else {
                    None
                },
            );
            println!(
                "{label}: rc={rc:?} paths={n_paths} modes={n_modes} topology={}",
                topology.0
            );
            for p in &paths[..n_paths as usize] {
                if !p.targetInfo.targetAvailable.as_bool() && flags == QDC_ALL_PATHS {
                    continue;
                }
                let mut name = DISPLAYCONFIG_TARGET_DEVICE_NAME {
                    header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                        size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                        adapterId: p.targetInfo.adapterId,
                        id: p.targetInfo.id,
                    },
                    ..Default::default()
                };
                let _ = DisplayConfigGetDeviceInfo(&mut name.header);
                let friendly = String::from_utf16_lossy(&name.monitorFriendlyDeviceName);
                let path = String::from_utf16_lossy(&name.monitorDevicePath);
                println!(
                    "  adapter={:#x} source={} target={} active={} available={} name={} \
                        edid_flags={:#x} tech={} device={}",
                    p.targetInfo.adapterId.LowPart,
                    p.sourceInfo.id,
                    p.targetInfo.id,
                    p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0,
                    p.targetInfo.targetAvailable.as_bool(),
                    friendly.trim_end_matches('\0'),
                    name.flags.Anonymous.value,
                    name.outputTechnology.0,
                    path.trim_end_matches('\0'),
                );
            }
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows only");
}
