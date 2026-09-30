//! Wake-on-LAN, as Moonlight wakes a sleeping PC: the host announces the
//! hardware addresses of its network adapters, and a client that wants it
//! awake broadcasts a magic packet for each.

/// A network adapter's hardware address.
pub type Mac = [u8; 6];

/// `02:1a:2b:3c:4d:5e`.
pub fn format(mac: &Mac) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// `02:1a:2b:3c:4d:5e`, `02-1a-2b-3c-4d-5e` or `021a2b3c4d5e`.
pub fn parse(s: &str) -> Option<Mac> {
    let hex: String = s.chars().filter(|c| !matches!(c, ':' | '-')).collect();
    if hex.len() != 12 {
        return None;
    }
    let mut mac = [0u8; 6];
    for (i, b) in mac.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(mac)
}

/// Several, comma-separated, as the mDNS record carries them. Malformed ones
/// are skipped.
pub fn parse_list(s: &str) -> Vec<Mac> {
    s.split(',').filter_map(|m| parse(m.trim())).collect()
}

/// Six 0xFF bytes, then the address sixteen times: what a network adapter
/// armed for Wake-on-LAN listens for.
pub fn magic_packet(mac: &Mac) -> [u8; 102] {
    let mut p = [0xFFu8; 102];
    for i in 0..16 {
        p[6 + i * 6..12 + i * 6].copy_from_slice(mac);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_round_trip_in_every_spelling() {
        let mac = [0x02, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e];
        assert_eq!(format(&mac), "02:1a:2b:3c:4d:5e");
        for s in ["02:1a:2b:3c:4d:5e", "02-1A-2B-3C-4D-5E", "021a2b3c4d5e"] {
            assert_eq!(parse(s), Some(mac), "{s}");
        }
        assert_eq!(parse("02:1a:2b:3c:4d"), None);
        assert_eq!(parse("zz1a2b3c4d5e"), None);
        assert_eq!(
            parse_list("021a2b3c4d5e, bad,001122334455"),
            vec![mac, [0, 0x11, 0x22, 0x33, 0x44, 0x55]]
        );
    }

    #[test]
    fn the_magic_packet_is_a_sync_stream_and_sixteen_addresses() {
        let mac = [1, 2, 3, 4, 5, 6];
        let p = magic_packet(&mac);
        assert_eq!(&p[..6], &[0xFF; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &mac);
        }
    }
}
