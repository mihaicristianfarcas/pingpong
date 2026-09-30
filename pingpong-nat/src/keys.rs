//! A device's rendezvous keys, kept next to its tunnel identity: the Ed25519
//! seed its records are signed with and, on a host, the secret they are
//! sealed with.

use std::path::Path;

use crate::rendezvous::{random_32, Secret, Seed};

#[derive(Clone)]
pub struct RendezvousKeys {
    pub seed: Seed,
    /// Hosts only.
    pub secret: Option<Secret>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

impl RendezvousKeys {
    /// Load the keys at `path`, or make and save new ones (with a secret when
    /// `host`). `write` saves a file readable only by its owner.
    pub fn load_or_create(
        path: &Path,
        host: bool,
        write: impl Fn(&Path, &[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<RendezvousKeys> {
        if let Ok(text) = std::fs::read_to_string(path) {
            let field = |name: &str| {
                text.lines()
                    .filter_map(|l| l.split_once('='))
                    .find(|(k, _)| k.trim() == name)
                    .and_then(|(_, v)| unhex(v.trim().trim_matches('"')))
            };
            if let Some(seed) = field("seed") {
                let secret = field("secret");
                if !host || secret.is_some() {
                    return Ok(RendezvousKeys { seed, secret });
                }
            }
        }
        let keys = RendezvousKeys {
            seed: random_32(),
            secret: host.then(random_32),
        };
        let mut text = format!(
            "# Keys for finding paired devices across the internet \
                (docs/networking.md).\nseed = \"{}\"\n",
            hex(&keys.seed)
        );
        if let Some(s) = keys.secret {
            text.push_str(&format!("secret = \"{}\"\n", hex(&s)));
        }
        write(path, text.as_bytes())?;
        Ok(keys)
    }

    pub fn public(&self) -> [u8; 32] {
        crate::rendezvous::public_key(&self.seed)
    }
}

/// This machine's global IPv6 addresses with `port`: reachable without NAT
/// traversal where the firewall allows (it usually does for replies).
pub fn global_ipv6(port: u16) -> Vec<std::net::SocketAddr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V6(v6) if (v6.segments()[0] & 0xE000) == 0x2000 => {
                Some(std::net::SocketAddr::new(std::net::IpAddr::V6(v6), port))
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_persist() {
        let dir = std::env::temp_dir().join(format!("pp-rv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rendezvous.toml");
        let write = |p: &Path, b: &[u8]| std::fs::write(p, b);
        let a = RendezvousKeys::load_or_create(&path, true, write).unwrap();
        let b = RendezvousKeys::load_or_create(&path, true, write).unwrap();
        assert_eq!(a.seed, b.seed);
        assert_eq!(a.secret, b.secret);
        assert!(a.secret.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}
