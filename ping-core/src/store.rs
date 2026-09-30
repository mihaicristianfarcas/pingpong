//! This client's identity and the hosts it has paired with, in
//! `~/Library/Application Support/Ping`, `%APPDATA%\Ping` or `~/.config/ping`
//! (`PING_DATA_DIR` overrides).

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use pingpong_transport::{Identity, PublicIdentity};
use serde::{Deserialize, Serialize};

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PING_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let env = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        // %APPDATA%\Ping: roams with the user's profile, like their pairings.
        return env("APPDATA")
            .unwrap_or_else(std::env::temp_dir)
            .join("Ping");
    }
    let home = env("HOME").unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Ping")
    } else {
        env("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("ping")
    }
}

/// The UDP port this client's tunnel uses, the same every time: a NAT then
/// gives it the same public port too, which is what lets a host keep a path
/// warm towards it (see `wan`). Picked at random on first use.
pub fn tunnel_port(dir: &Path) -> u16 {
    let path = dir.join("port");
    if let Some(p) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| t.trim().parse::<u16>().ok())
    {
        return p;
    }
    let p = 49_152 + (rand_core::RngCore::next_u32(&mut rand_core::OsRng) % 16_000) as u16;
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&path, p.to_string());
    p
}

/// Where this device's AI agent keeps its own identity and the hosts it is
/// paired with: an agent is a client of its own to a host (so the host
/// holds it to the agent rules, and a person can watch it from this same
/// device), but the same store.
pub fn agent_dir(dir: &Path) -> PathBuf {
    dir.join("agent")
}

pub fn identity(dir: &Path) -> Result<Identity, String> {
    Identity::load_or_create(&dir.join("identity.toml")).map_err(|e| e.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnownHost {
    pub name: String,
    /// `host:port` as paired or entered (IP or DNS name): the remote address.
    pub address: String,
    /// Where discovery last saw it on the local network. Tried first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_address: Option<String>,
    /// How to find the host across the internet: its rendezvous key and the
    /// secret its records are sealed with (hex). From pairing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous_secret: Option<String>,
    /// Where the host's rendezvous record last said it is on the internet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wan_addresses: Vec<String>,
    /// Its network adapters' hardware addresses (`02:1a:2b:3c:4d:5e`), as it
    /// announced them on the local network: what Wake-on-LAN sends to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wake: Vec<String>,
    pub x25519: String,
    pub mlkem: String,
    #[serde(default)]
    pub paired_at: u64,
}

impl KnownHost {
    /// A host paired by hand, from its public keys (`pong identity`): nothing
    /// else is known about it yet.
    pub fn by_hand(name: &str, address: &str, x25519: &str, mlkem: &str) -> KnownHost {
        KnownHost {
            name: name.to_string(),
            address: address.to_string(),
            local_address: None,
            rendezvous: None,
            rendezvous_secret: None,
            wan_addresses: Vec::new(),
            wake: Vec::new(),
            x25519: x25519.to_string(),
            mlkem: mlkem.to_string(),
            paired_at: 0,
        }
    }

    pub fn public(&self) -> Option<PublicIdentity> {
        PublicIdentity::from_b64(&self.x25519, &self.mlkem).ok()
    }

    /// The host's rendezvous key and secret, if pairing provided them.
    pub fn rendezvous_keys(&self) -> Option<([u8; 32], [u8; 32])> {
        let unhex = |h: &str| -> Option<[u8; 32]> {
            let v: Vec<u8> = (0..h.len())
                .step_by(2)
                .filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
                .collect();
            v.try_into().ok()
        };
        Some((
            unhex(self.rendezvous.as_deref()?)?,
            unhex(self.rendezvous_secret.as_deref()?)?,
        ))
    }

    /// Addresses to try, local network first (see [`KnownHost::local_address`]).
    pub fn candidates(&self) -> (Vec<SocketAddr>, Vec<SocketAddr>) {
        let resolve = |s: &str| {
            s.to_socket_addrs()
                .map(|a| a.collect::<Vec<_>>())
                .unwrap_or_default()
        };
        // A link-local IPv6 address saved without its interface cannot be
        // sent to (older discovery saved such).
        let unscoped = |a: &SocketAddr| matches!(a, SocketAddr::V6(v6) if v6.ip().segments()[0] & 0xFFC0 == 0xFE80 && v6.scope_id() == 0);
        let local: Vec<SocketAddr> = self
            .local_address
            .as_deref()
            .map(resolve)
            .unwrap_or_default()
            .into_iter()
            .filter(|a| !unscoped(a))
            .collect();
        let mut remote: Vec<SocketAddr> = resolve(&self.address)
            .into_iter()
            .filter(|a| !local.contains(a))
            .collect();
        for a in self
            .wan_addresses
            .iter()
            .filter_map(|a| a.parse::<SocketAddr>().ok())
        {
            if !local.contains(&a) && !remote.contains(&a) {
                remote.push(a);
            }
        }
        (local, remote)
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "host")]
    hosts: Vec<KnownHost>,
}

pub struct Hosts {
    path: PathBuf,
    list: Vec<KnownHost>,
}

/// Several threads update hosts.toml (discovery, a stream, the presence
/// loop): each change re-reads the file under this lock, so none overwrites
/// another's with a stale copy.
fn file_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

impl Hosts {
    pub fn load(dir: &Path) -> Hosts {
        let path = dir.join("hosts.toml");
        let list = Self::read(&path);
        Hosts { path, list }
    }

    fn read(path: &Path) -> Vec<KnownHost> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| toml::from_str::<File>(&t).ok())
            .map(|f| f.hosts)
            .unwrap_or_default()
    }

    /// Re-read, apply `f`, and save if it says it changed something.
    fn modify(&mut self, f: impl FnOnce(&mut Vec<KnownHost>) -> bool) -> std::io::Result<bool> {
        let _guard = file_lock();
        self.list = Self::read(&self.path);
        let changed = f(&mut self.list);
        if changed {
            self.save()?;
        }
        Ok(changed)
    }

    pub fn list(&self) -> &[KnownHost] {
        &self.list
    }

    pub fn find(&self, name_or_address: &str) -> Option<&KnownHost> {
        self.list
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name_or_address) || h.address == name_or_address)
    }

    /// Add a host, or replace the one with the same key -- keeping what was
    /// learned about reaching it unless the new entry says otherwise.
    pub fn upsert(&mut self, host: KnownHost) -> std::io::Result<()> {
        self.modify(|list| {
            match list.iter_mut().find(|h| h.x25519 == host.x25519) {
                Some(existing) => {
                    let old = std::mem::replace(existing, host);
                    if existing.local_address.is_none() {
                        existing.local_address = old.local_address;
                    }
                    if existing.wan_addresses.is_empty() {
                        existing.wan_addresses = old.wan_addresses;
                    }
                    if existing.wake.is_empty() {
                        existing.wake = old.wake;
                    }
                    if existing.rendezvous.is_none() {
                        existing.rendezvous = old.rendezvous;
                        existing.rendezvous_secret = old.rendezvous_secret;
                    }
                }
                None => list.push(host),
            }
            true
        })
        .map(|_| ())
    }

    /// Record where discovery found the host with this key. True if it moved.
    pub fn set_local_address(&mut self, x25519: &str, addr: SocketAddr) -> std::io::Result<bool> {
        let text = addr.to_string();
        self.modify(|list| {
            let Some(h) = list.iter_mut().find(|h| h.x25519 == x25519) else {
                return false;
            };
            if h.local_address.as_deref() == Some(text.as_str()) {
                return false;
            }
            h.local_address = Some(text);
            true
        })
    }

    /// Remember the hardware addresses the host announced, to wake it by.
    /// True if they changed.
    pub fn set_wake(
        &mut self,
        x25519: &str,
        macs: &[pingpong_pairing::wake::Mac],
    ) -> std::io::Result<bool> {
        let v: Vec<String> = macs.iter().map(pingpong_pairing::wake::format).collect();
        self.modify(|list| {
            let Some(h) = list.iter_mut().find(|h| h.x25519 == x25519) else {
                return false;
            };
            if v.is_empty() || h.wake == v {
                return false;
            }
            h.wake = v;
            true
        })
    }

    /// Remember where the host's rendezvous record says it is. True if it
    /// changed.
    pub fn set_wan_addresses(
        &mut self,
        x25519: &str,
        addrs: &[SocketAddr],
    ) -> std::io::Result<bool> {
        let v: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        self.modify(|list| {
            let Some(h) = list.iter_mut().find(|h| h.x25519 == x25519) else {
                return false;
            };
            if h.wan_addresses == v {
                return false;
            }
            h.wan_addresses = v;
            true
        })
    }

    /// Record how to find the host with this tunnel key across the internet.
    /// True if it changed.
    pub fn set_rendezvous(
        &mut self,
        x25519: &str,
        key: [u8; 32],
        secret: [u8; 32],
    ) -> std::io::Result<bool> {
        let hex = |k: [u8; 32]| k.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let (k, sec) = (Some(hex(key)), Some(hex(secret)));
        self.modify(|list| {
            let Some(h) = list.iter_mut().find(|h| h.x25519 == x25519) else {
                return false;
            };
            if h.rendezvous == k && h.rendezvous_secret == sec {
                return false;
            }
            h.rendezvous = k;
            h.rendezvous_secret = sec;
            true
        })
    }

    pub fn remove(&mut self, name: &str) -> std::io::Result<bool> {
        self.modify(|list| {
            let before = list.len();
            list.retain(|h| !h.name.eq_ignore_ascii_case(name));
            list.len() != before
        })
    }

    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(&File {
            hosts: self.list.clone(),
        })
        .map_err(std::io::Error::other)?;
        pingpong_transport::identity::write_private(&self.path, text.as_bytes())
    }
}
