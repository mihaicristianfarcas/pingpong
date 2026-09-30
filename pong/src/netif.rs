//! The hardware addresses a client can wake this machine by: its Ethernet
//! and Wi-Fi adapters, connected ones first. Announced over mDNS (see
//! `pingpong_pairing::wake`); whether the adapter really wakes the machine
//! is up to its driver and firmware settings.

use pingpong_pairing::wake::Mac;

/// At most this many: the mDNS record stays small.
const MAX: usize = 4;

#[cfg(windows)]
pub fn wake_macs() -> Vec<Mac> {
    use windows::Win32::Foundation::ERROR_BUFFER_OVERFLOW;
    use windows::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER,
        GAA_FLAG_SKIP_MULTICAST, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows::Win32::NetworkManagement::Ndis::IfOperStatusUp;
    use windows::Win32::Networking::WinSock::AF_UNSPEC;

    // Ethernet (6) and Wi-Fi (71): not tunnels such as Tailscale's.
    const ETHERNET: u32 = 6;
    const WIFI: u32 = 71;

    let mut size: u32 = 16 * 1024;
    let mut buf: Vec<u64> = Vec::new(); // u64s: the list wants 8-byte alignment
    loop {
        buf.resize((size as usize).div_ceil(8), 0);
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC.0 as u32,
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                None,
                Some(buf.as_mut_ptr() as *mut IP_ADAPTER_ADDRESSES_LH),
                &mut size,
            )
        };
        match rc {
            0 => break,
            rc if rc == ERROR_BUFFER_OVERFLOW.0 => continue,
            _ => return Vec::new(),
        }
    }
    let mut found: Vec<(bool, Mac)> = Vec::new();
    let mut p = buf.as_ptr() as *const IP_ADAPTER_ADDRESSES_LH;
    while let Some(a) = unsafe { p.as_ref() } {
        p = a.Next;
        if a.PhysicalAddressLength != 6 || !matches!(a.IfType, ETHERNET | WIFI) {
            continue;
        }
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&a.PhysicalAddress[..6]);
        if mac != [0; 6] && !found.iter().any(|(_, m)| *m == mac) {
            found.push((a.OperStatus == IfOperStatusUp, mac));
        }
    }
    connected_first(found)
}

#[cfg(target_os = "macos")]
pub fn wake_macs() -> Vec<Mac> {
    use std::ffi::CStr;
    // en*: Ethernet, Wi-Fi and the Thunderbolt ports (not bridges, tunnels
    // or AWDL). Connected: it has an IPv4 address (an empty Thunderbolt port
    // is "running" too).
    let mut links: Vec<(Vec<u8>, Mac)> = Vec::new();
    let mut connected: Vec<Vec<u8>> = Vec::new();
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return Vec::new();
        }
        let mut p = list;
        while let Some(ifa) = p.as_ref() {
            p = ifa.ifa_next;
            let name = CStr::from_ptr(ifa.ifa_name).to_bytes();
            if ifa.ifa_addr.is_null() || !name.starts_with(b"en") {
                continue;
            }
            match (*ifa.ifa_addr).sa_family as i32 {
                libc::AF_INET => connected.push(name.to_vec()),
                libc::AF_LINK => {
                    let link = &*(ifa.ifa_addr as *const libc::sockaddr_dl);
                    if link.sdl_alen != 6 {
                        continue;
                    }
                    // The address follows the interface's name in sdl_data.
                    let at = (link.sdl_data.as_ptr() as *const u8).add(link.sdl_nlen as usize);
                    let mut mac = [0u8; 6];
                    std::ptr::copy_nonoverlapping(at, mac.as_mut_ptr(), 6);
                    if mac != [0; 6] && !links.iter().any(|(_, m)| *m == mac) {
                        links.push((name.to_vec(), mac));
                    }
                }
                _ => {}
            }
        }
        libc::freeifaddrs(list);
    }
    connected_first(
        links
            .into_iter()
            .map(|(name, mac)| (connected.contains(&name), mac))
            .collect(),
    )
}

/// Linux: the adapters with a device behind them (not bridges, tunnels or
/// containers' veths), Ethernet or Wi-Fi alike (ARPHRD_ETHER).
#[cfg(target_os = "linux")]
pub fn wake_macs() -> Vec<Mac> {
    let Ok(dir) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut found: Vec<(bool, Mac)> = Vec::new();
    for entry in dir.flatten() {
        let path = entry.path();
        let read = |f: &str| {
            std::fs::read_to_string(path.join(f))
                .unwrap_or_default()
                .trim()
                .to_string()
        };
        if !path.join("device").exists() || read("type") != "1" {
            continue;
        }
        let parts: Vec<u8> = read("address")
            .split(':')
            .filter_map(|h| u8::from_str_radix(h, 16).ok())
            .collect();
        let Ok(mac) = <Mac>::try_from(parts.as_slice()) else {
            continue;
        };
        if mac != [0; 6] && !found.iter().any(|(_, m)| *m == mac) {
            found.push((read("operstate") == "up", mac));
        }
    }
    connected_first(found)
}

fn connected_first(mut found: Vec<(bool, Mac)>) -> Vec<Mac> {
    found.sort_by_key(|(up, _)| !*up);
    found.into_iter().map(|(_, m)| m).take(MAX).collect()
}
