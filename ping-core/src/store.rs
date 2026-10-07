//! This client's identity and the hosts it has paired with, in
//! `~/Library/Application Support/Ping`, `%APPDATA%\Ping` or `~/.config/ping`
//! (`PING_DATA_DIR` overrides).

use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

/// Streams starting now that wait for [`TunnelLock`]: whoever borrowed the
/// tunnel port lets go of it when this is not zero.
static STREAMS_WAITING: AtomicUsize = AtomicUsize::new(0);

/// Who is using this identity's tunnel. Streams hold the lock shared, so
/// two can run at once (to different hosts). The host list's probe and the
/// presence check borrow the tunnel port between streams and hold the lock
/// alone, or skip.
///
/// Looks removable, is not: a handshake from a probe reaches a host this
/// identity is streaming from as the same peer. The host moves that peer's
/// tunnel to the probe's address (any datagram that authenticates does,
/// `Endpoint::receive`) and installs new keys, and the stream goes dark. A
/// stream from another process (`pingctl stream` beside the app) counts too,
/// so this is a lock on a file, not on memory.
pub struct TunnelLock {
    /// Held for its lock, which goes with it.
    _held: std::fs::File,
}

impl TunnelLock {
    /// For a stream. Waits while the port is borrowed: a borrower in this
    /// process lets go within one receive timeout (it watches
    /// [`stream_waiting`]); one in another process, within its own time
    /// (1.5 s for the probe). None if the lock file cannot be opened.
    pub fn for_stream(dir: &Path) -> Option<TunnelLock> {
        let file = Self::open(dir)?;
        STREAMS_WAITING.fetch_add(1, Ordering::SeqCst);
        let locked = file.lock_shared();
        STREAMS_WAITING.fetch_sub(1, Ordering::SeqCst);
        locked.ok().map(|()| TunnelLock { _held: file })
    }

    /// To borrow the tunnel port between streams. None while a stream runs
    /// as this identity, or waits to start.
    pub fn for_borrowing(dir: &Path) -> Option<TunnelLock> {
        if stream_waiting() {
            return None;
        }
        let file = Self::open(dir)?;
        file.try_lock().ok().map(|()| TunnelLock { _held: file })
    }

    fn open(dir: &Path) -> Option<std::fs::File> {
        let _ = std::fs::create_dir_all(dir);
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("tunnel.lock"))
            .ok()
    }
}

/// A stream in this process is waiting to start: a borrower of the tunnel
/// port gives it back now.
pub fn stream_waiting() -> bool {
    STREAMS_WAITING.load(Ordering::SeqCst) != 0
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
    /// A host paired by hand, from its public keys (`pongctl identity`): nothing
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
        let list = Self::read(&path).unwrap_or_else(|e| {
            // Loaded every few seconds (the host list's poll): said once.
            static SAID: AtomicBool = AtomicBool::new(false);
            if !SAID.swap(true, Ordering::Relaxed) {
                tracing::error!(path = %path.display(), error = e, "the paired hosts could not \
                    be read: Ping goes on with none, and leaves the file as it is");
            }
            Vec::new()
        });
        Hosts { path, list }
    }

    /// The hosts in `path`: none if there is no file yet. Anything else that
    /// is not a list of hosts (a damaged file, one a newer Ping wrote) is an
    /// error, so that it is not taken for an empty list and saved over.
    fn read(path: &Path) -> Result<Vec<KnownHost>, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<File>(&text)
                .map(|f| f.hosts)
                .map_err(|e| e.to_string().lines().next().unwrap_or_default().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Re-read, apply `f`, and save if it says it changed something. Refused
    /// when the file cannot be read: saving would replace every pairing in
    /// it.
    fn modify(&mut self, f: impl FnOnce(&mut Vec<KnownHost>) -> bool) -> std::io::Result<bool> {
        let _guard = file_lock();
        self.list = Self::read(&self.path).map_err(|e| {
            std::io::Error::other(format!(
                "Ping could not read {} ({e}) and will not save over it. Fix the file or move \
                    it away, then try again.",
                self.path.display()
            ))
        })?;
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Tests that take a [`TunnelLock`] share `STREAMS_WAITING` (here and in
    /// `pair`): one at a time.
    pub(crate) fn serial() -> std::sync::MutexGuard<'static, ()> {
        static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ping-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_tunnel_port_is_not_borrowed_while_a_stream_runs() {
        let _serial = serial();
        let dir = scratch("lock-stream");
        let stream = TunnelLock::for_stream(&dir).expect("a stream takes the lock");
        let second = TunnelLock::for_stream(&dir).expect("streams share it");
        assert!(TunnelLock::for_borrowing(&dir).is_none());
        drop(stream);
        assert!(TunnelLock::for_borrowing(&dir).is_none());
        drop(second);
        assert!(TunnelLock::for_borrowing(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stream_waits_for_a_borrower_to_let_go() {
        let _serial = serial();
        let dir = scratch("lock-borrow");
        let borrowed = TunnelLock::for_borrowing(&dir).expect("nothing holds it");
        assert!(TunnelLock::for_borrowing(&dir).is_none());
        let started = std::time::Instant::now();
        let waiter = {
            let dir = dir.clone();
            std::thread::spawn(move || TunnelLock::for_stream(&dir).is_some())
        };
        while !stream_waiting() {
            std::thread::yield_now();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(borrowed);
        assert!(waiter.join().unwrap());
        assert!(started.elapsed() >= std::time::Duration::from_millis(50));
        assert!(!stream_waiting());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hosts_file_that_cannot_be_read_is_left_as_it_is() {
        let dir = scratch("hosts-damaged");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hosts.toml");
        let damaged = "[[host]]\nname = \"gaming-pc\"\naddress = ";
        std::fs::write(&path, damaged).unwrap();
        let mut hosts = Hosts::load(&dir);
        assert!(hosts.list().is_empty());
        let refused = hosts
            .upsert(KnownHost::by_hand("other", "192.168.1.20:47800", "x", "m"))
            .unwrap_err();
        assert!(
            refused.to_string().contains("will not save over it"),
            "{refused}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), damaged);
        // No file yet is no hosts yet, and saving works.
        std::fs::remove_file(&path).unwrap();
        Hosts::load(&dir)
            .upsert(KnownHost::by_hand("other", "192.168.1.20:47800", "x", "m"))
            .unwrap();
        assert_eq!(Hosts::load(&dir).list().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
