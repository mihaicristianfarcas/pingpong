//! Rendezvous records on the BitTorrent Mainline DHT (BEP 44 mutable items):
//! where a paired device can be reached, readable only by devices paired with
//! the same host.
//!
//! Each device signs its records with its own Ed25519 key (BEP 44 requires
//! it; the DHT stores only what verifies). The content is sealed with
//! ChaCha20-Poly1305 under the host's 32-byte rendezvous secret, which pairing
//! hands to each client, so the DHT's nodes see ciphertext under a key they
//! cannot link to anyone. The sequence number is the publication time in
//! milliseconds, so a newer record always wins.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use mainline::{Dht, MutableItem, SigningKey};

/// Distinguishes pingpong's items under a device key (and versions them).
/// A host publishes under this salt.
pub const SALT: &[u8] = b"pingpong/rv/1";

/// A client publishes one record per host it is paired with, each under a
/// salt naming the host.
pub fn client_salt(host_key: &[u8; 32]) -> Vec<u8> {
    let mut s = SALT.to_vec();
    s.push(b'/');
    s.extend(
        host_key[..8]
            .iter()
            .flat_map(|b| format!("{b:02x}").into_bytes()),
    );
    s
}
/// BEP 44's limit on a value.
const MAX_VALUE: usize = 1000;
const MAX_ENDPOINTS: usize = 12;

pub type Seed = [u8; 32];
pub type Secret = [u8; 32];

pub fn random_32() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
    b
}

/// The public half of a device's rendezvous key.
pub fn public_key(seed: &Seed) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The host: where to find it.
    Host = 0,
    /// A client asking to connect now: where to punch towards.
    Intent = 1,
    /// A client that is open and may connect: keep a path warm towards it.
    Presence = 2,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: Kind,
    /// Milliseconds since the Unix epoch, when published.
    pub at_ms: u64,
    /// Tells two intents from each other.
    pub nonce: u64,
    pub endpoints: Vec<SocketAddr>,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Record {
    pub fn new(kind: Kind, endpoints: Vec<SocketAddr>) -> Record {
        let nonce = rand_core::RngCore::next_u64(&mut rand_core::OsRng);
        Record {
            kind,
            at_ms: now_ms(),
            nonce,
            endpoints,
        }
    }

    pub fn age(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.at_ms))
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(32 + self.endpoints.len() * 19);
        b.push(self.kind as u8);
        b.extend_from_slice(&self.at_ms.to_le_bytes());
        b.extend_from_slice(&self.nonce.to_le_bytes());
        let eps: Vec<&SocketAddr> = self.endpoints.iter().take(MAX_ENDPOINTS).collect();
        b.push(eps.len() as u8);
        for ep in eps {
            match ep.ip() {
                IpAddr::V4(ip) => {
                    b.push(4);
                    b.extend_from_slice(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    b.push(6);
                    b.extend_from_slice(&ip.octets());
                }
            }
            b.extend_from_slice(&ep.port().to_le_bytes());
        }
        b
    }

    fn decode(b: &[u8]) -> Option<Record> {
        let kind = match *b.first()? {
            0 => Kind::Host,
            1 => Kind::Intent,
            2 => Kind::Presence,
            _ => return None,
        };
        let at_ms = u64::from_le_bytes(b.get(1..9)?.try_into().ok()?);
        let nonce = u64::from_le_bytes(b.get(9..17)?.try_into().ok()?);
        let n = *b.get(17)? as usize;
        let mut off = 18;
        let mut endpoints = Vec::with_capacity(n);
        for _ in 0..n.min(MAX_ENDPOINTS) {
            let ip = match *b.get(off)? {
                4 => {
                    let o: [u8; 4] = b.get(off + 1..off + 5)?.try_into().ok()?;
                    off += 5;
                    IpAddr::V4(Ipv4Addr::from(o))
                }
                6 => {
                    let o: [u8; 16] = b.get(off + 1..off + 17)?.try_into().ok()?;
                    off += 17;
                    IpAddr::V6(Ipv6Addr::from(o))
                }
                _ => return None,
            };
            let port = u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?);
            off += 2;
            endpoints.push(SocketAddr::new(ip, port));
        }
        Some(Record {
            kind,
            at_ms,
            nonce,
            endpoints,
        })
    }
}

/// Seal a record for the devices that know `secret`.
pub fn seal(secret: &Secret, record: &Record) -> Vec<u8> {
    let nonce: [u8; 12] = random_32()[..12].try_into().expect("12 bytes");
    let cipher = ChaCha20Poly1305::new(Key::from_slice(secret));
    let body = cipher
        .encrypt(Nonce::from_slice(&nonce), record.encode().as_slice())
        .expect("in-memory encryption");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&body);
    out
}

pub fn open(secret: &Secret, sealed: &[u8]) -> Option<Record> {
    let (nonce, body) = sealed.split_at_checked(12)?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(secret));
    let plain = cipher.decrypt(Nonce::from_slice(nonce), body).ok()?;
    Record::decode(&plain)
}

/// A DHT client. Joining the network takes a few seconds; keep one per
/// process.
pub struct Rendezvous {
    dht: Dht,
}

impl Rendezvous {
    /// Join the Mainline DHT (as a client: we answer no one's queries).
    pub fn join() -> std::io::Result<Rendezvous> {
        Ok(Rendezvous {
            dht: Dht::client()?,
        })
    }

    /// Join a private network instead (tests).
    pub fn join_with(bootstrap: &[String]) -> std::io::Result<Rendezvous> {
        Ok(Rendezvous {
            dht: Dht::builder().bootstrap(bootstrap).build()?,
        })
    }

    #[allow(deprecated)]
    pub fn is_ready(&self) -> bool {
        self.dht.bootstrapped()
    }

    /// Publish `record` under `seed`'s key and `salt` ([`SALT`] for a host,
    /// [`client_salt`] for a client). Blocking (a few round trips).
    #[allow(deprecated)]
    pub fn publish(
        &self,
        seed: &Seed,
        secret: &Secret,
        salt: &[u8],
        record: &Record,
    ) -> Result<(), String> {
        let value = seal(secret, record);
        if value.len() > MAX_VALUE {
            return Err("record too large".into());
        }
        let item = MutableItem::new(
            SigningKey::from_bytes(seed),
            &value,
            record.at_ms as i64,
            Some(salt),
        );
        self.dht
            .put_mutable(item, None)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The newest record published under `public`, if it opens with `secret`.
    /// Blocking.
    #[allow(deprecated)]
    pub fn resolve(&self, public: &[u8; 32], secret: &Secret, salt: &[u8]) -> Option<Record> {
        let item = self.dht.get_mutable_most_recent(public, Some(salt))?;
        open(secret, item.value())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: Kind) -> Record {
        Record::new(
            kind,
            vec![
                "203.0.113.7:47800".parse().unwrap(),
                "[2001:db8::1]:47800".parse().unwrap(),
            ],
        )
    }

    #[test]
    fn records_seal_and_open() {
        let secret = random_32();
        let r = record(Kind::Host);
        assert_eq!(open(&secret, &seal(&secret, &r)), Some(r.clone()));
        assert_eq!(
            open(&random_32(), &seal(&secret, &r)),
            None,
            "another host's secret opens nothing"
        );
        let mut tampered = seal(&secret, &r);
        tampered[20] ^= 1;
        assert_eq!(open(&secret, &tampered), None);
    }

    #[test]
    fn a_record_crosses_a_dht() {
        let net = mainline::Testnet::builder(8)
            .bind_address(Ipv4Addr::LOCALHOST)
            .build()
            .unwrap();
        let (seed, secret) = (random_32(), random_32());
        let host = Rendezvous::join_with(&net.bootstrap).unwrap();
        let client = Rendezvous::join_with(&net.bootstrap).unwrap();

        let first = record(Kind::Host);
        host.publish(&seed, &secret, SALT, &first).unwrap();
        assert_eq!(
            client.resolve(&public_key(&seed), &secret, SALT),
            Some(first.clone())
        );

        // A newer record replaces it.
        std::thread::sleep(Duration::from_millis(5));
        let second = Record::new(Kind::Intent, vec!["10.0.0.1:1".parse().unwrap()]);
        host.publish(&seed, &secret, SALT, &second).unwrap();
        assert_eq!(
            client.resolve(&public_key(&seed), &secret, SALT),
            Some(second.clone())
        );

        // A client's records for two hosts live side by side.
        let (h1, h2) = (random_32(), random_32());
        let p1 = Record::new(Kind::Presence, vec!["10.0.0.2:2".parse().unwrap()]);
        host.publish(&seed, &secret, &client_salt(&h1), &p1)
            .unwrap();
        assert_eq!(
            client.resolve(&public_key(&seed), &secret, &client_salt(&h1)),
            Some(p1)
        );
        assert_eq!(
            client.resolve(&public_key(&seed), &secret, &client_salt(&h2)),
            None
        );
        assert_eq!(
            client.resolve(&public_key(&seed), &secret, SALT),
            Some(second),
            "the host record is untouched"
        );

        assert_eq!(
            client.resolve(&public_key(&random_32()), &secret, SALT),
            None,
            "nothing under an unknown key"
        );
    }
}
