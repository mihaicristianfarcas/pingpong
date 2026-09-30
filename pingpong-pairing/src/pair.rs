//! Pairing: Moonlight's user experience, a password-authenticated key exchange
//! underneath.
//!
//! 1. Ping shows a 4-digit PIN and connects to Pong's pairing port, with its
//!    SPAKE2 message and a fresh ML-KEM-768 encapsulation key.
//! 2. Pong shows "<client> wants to pair" in its web UI; the user types the PIN.
//! 3. Pong answers with its SPAKE2 message and an ML-KEM ciphertext to Ping's
//!    key. Both derive the pairing's keys from the SPAKE2 secret and the
//!    ML-KEM secret together, each proves it derived them (a confirmation MAC
//!    over the whole transcript), then sends its X25519 and ML-KEM-768 public
//!    keys (and its extras) sealed under them.
//!
//! A wrong PIN fails the confirmation; an eavesdropper learns nothing it can
//! brute-force offline (that is what SPAKE2 buys over Moonlight's PIN-derived
//! AES key). And the keys are hybrid, as the tunnel's are: a recording of a
//! pairing opens only for someone who breaks both SPAKE2 (elliptic curves: a
//! future quantum computer would) and ML-KEM (built to resist one), so what
//! pairing carries is kept from "harvest now, decrypt later". The PIN check
//! itself stays SPAKE2's: defeating it takes an attacker in the middle while
//! the pairing happens. The keys exchanged are what pq-boringtun then
//! authenticates every session with, so pairing happens once and nothing is
//! ever copied by hand.
//!
//! Version 2. There is no falling back to version 1 (SPAKE2 alone): a
//! fallback would let an attacker force the classical pairing. Pairings made
//! with version 1 stay valid; they are not redone.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hmac::{Hmac, Mac};
use ml_kem::{Decapsulate, Encapsulate, EncapsulationKey768, Kem, KeyExport, MlKem768};
use pingpong_transport::PublicIdentity;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};

pub const VERSION: u32 = 2;
/// ML-KEM-768's encapsulation key and ciphertext, in bytes.
const KEM_KEY_LEN: usize = 1184;
const KEM_CT_LEN: usize = 1088;
const MAX_FRAME: usize = 64 * 1024;
/// How long the host waits for the user to type the PIN.
pub const PIN_TIMEOUT: Duration = Duration::from_secs(300);

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

#[derive(Debug)]
pub enum PairError {
    Io(std::io::Error),
    Protocol(String),
    /// The PIN typed on the host did not match the one this client showed.
    WrongPin,
    Refused(String),
    /// This side gave up (see [`Cancel`]).
    Cancelled,
}

impl std::fmt::Display for PairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairError::Io(e) => write!(f, "{e}"),
            PairError::Protocol(m) => write!(f, "pairing protocol error: {m}"),
            PairError::WrongPin => write!(f, "the PIN did not match"),
            PairError::Refused(r) => write!(f, "{r}"),
            PairError::Cancelled => write!(f, "pairing was cancelled"),
        }
    }
}

impl std::error::Error for PairError {}

impl From<std::io::Error> for PairError {
    fn from(e: std::io::Error) -> Self {
        PairError::Io(e)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Msg {
    /// Client → host: describe yourself (no pairing).
    Info,
    HostInfo {
        name: String,
        id: String,
        version: String,
        port: u16,
    },
    /// Client → host: start pairing. `agent`: the identity is an AI agent's
    /// (shown with the PIN prompt; the sealed `Extras::agent` is what counts).
    Hello {
        version: u32,
        name: String,
        spake: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        agent: bool,
        /// The client's fresh ML-KEM-768 encapsulation key (base64).
        #[serde(default)]
        kem: String,
    },
    /// Host → client: the PIN prompt is up; waiting for the user.
    Waiting,
    /// `kem`: the ML-KEM ciphertext to the client's key (base64).
    Challenge {
        spake: String,
        confirm: String,
        #[serde(default)]
        kem: String,
    },
    Proof {
        confirm: String,
        sealed: String,
    },
    Welcome {
        sealed: String,
    },
    Refused {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sealed {
    name: String,
    x25519: String,
    mlkem: String,
    /// The sender's rendezvous key (Ed25519 public, base64): see
    /// [`Extras`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rendezvous: Option<String>,
    /// Host to client only: the secret its rendezvous records are sealed with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rendezvous_secret: Option<String>,
    /// Client to host only: this identity belongs to an AI agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    agent: bool,
}

/// What pairing exchanges besides the tunnel keys: how to find each other
/// across the internet (see docs/networking.md). Hosts and clients that predate
/// it send none, and pair all the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Extras {
    /// Ed25519 public key the device signs its rendezvous records with.
    pub rendezvous: Option<[u8; 32]>,
    /// The host's rendezvous secret (host to client only).
    pub rendezvous_secret: Option<[u8; 32]>,
    /// Client to host only: the identity pairing is an AI agent's, which the
    /// host holds to its agent rules (see docs/ai-agents.md). Sealed with
    /// the PIN's key, so only the device the user typed the PIN for says so.
    pub agent: bool,
}

impl Extras {
    fn into_sealed(self, name: &str, id: &PublicIdentity) -> Sealed {
        let (x25519, mlkem) = id.to_b64();
        Sealed {
            name: name.to_string(),
            x25519,
            mlkem,
            rendezvous: self.rendezvous.map(|k| b64().encode(k)),
            rendezvous_secret: self.rendezvous_secret.map(|k| b64().encode(k)),
            agent: self.agent,
        }
    }

    fn from_sealed(s: &Sealed) -> Extras {
        let key = |v: &Option<String>| {
            v.as_ref()
                .and_then(|t| b64().decode(t).ok())
                .and_then(|b| b.try_into().ok())
        };
        Extras {
            rendezvous: key(&s.rendezvous),
            rendezvous_secret: key(&s.rendezvous_secret),
            agent: s.agent,
        }
    }
}

pub fn write_msg(s: &mut impl Write, m: &Msg) -> Result<(), PairError> {
    let body = serde_json::to_vec(m).map_err(|e| PairError::Protocol(e.to_string()))?;
    s.write_all(&(body.len() as u32).to_be_bytes())?;
    s.write_all(&body)?;
    s.flush()?;
    Ok(())
}

pub fn read_msg(s: &mut impl Read) -> Result<Msg, PairError> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(PairError::Protocol(format!("frame of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| PairError::Protocol(e.to_string()))
}

struct Keys {
    confirm: [u8; 32],
    seal: [u8; 32],
}

/// The pairing's keys, from both secrets: SPAKE2's (the PIN) and ML-KEM's.
fn derive(spake: &[u8], kem: &[u8]) -> Keys {
    let ikm = [spake, kem].concat();
    let hk = hkdf::Hkdf::<Sha256>::new(Some(b"pingpong pairing v2: SPAKE2 + ML-KEM-768"), &ikm);
    let mut confirm = [0u8; 32];
    let mut seal = [0u8; 32];
    hk.expand(b"confirm", &mut confirm)
        .expect("32 bytes is a valid length");
    hk.expand(b"seal", &mut seal)
        .expect("32 bytes is a valid length");
    Keys { confirm, seal }
}

/// What both sides said, in order: the confirmation covers all of it.
struct Transcript<'a> {
    msg_a: &'a [u8],
    msg_b: &'a [u8],
    kem_key: &'a [u8],
    kem_ct: &'a [u8],
}

impl Transcript<'_> {
    fn mac(&self, keys: &Keys, who: &[u8]) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&keys.confirm).expect("any key length");
        for part in [who, self.msg_a, self.msg_b, self.kem_key, self.kem_ct] {
            // Length-prefixed: no two transcripts read the same.
            mac.update(&(part.len() as u32).to_be_bytes());
            mac.update(part);
        }
        mac
    }
}

fn confirm_tag(keys: &Keys, who: &[u8], t: &Transcript) -> Vec<u8> {
    t.mac(keys, who).finalize().into_bytes().to_vec()
}

fn verify_tag(keys: &Keys, who: &[u8], t: &Transcript, tag: &[u8]) -> bool {
    t.mac(keys, who).verify_slice(tag).is_ok()
}

fn kem_key(bytes: &[u8]) -> Result<EncapsulationKey768, PairError> {
    let bytes: [u8; KEM_KEY_LEN] = bytes
        .try_into()
        .map_err(|_| PairError::Protocol("bad ML-KEM key".into()))?;
    EncapsulationKey768::new((&bytes).into())
        .map_err(|_| PairError::Protocol("bad ML-KEM key".into()))
}

// Each direction seals exactly once under a key unique to this pairing, so a
// fixed per-direction nonce never repeats.
fn seal(keys: &Keys, direction: u8, s: &Sealed) -> String {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&keys.seal));
    let mut nonce = [0u8; 12];
    nonce[0] = direction;
    let plain = serde_json::to_vec(s).expect("serializable");
    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), plain.as_ref())
        .expect("encryption cannot fail");
    b64().encode(sealed)
}

fn open(keys: &Keys, direction: u8, text: &str) -> Result<Sealed, PairError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&keys.seal));
    let mut nonce = [0u8; 12];
    nonce[0] = direction;
    let raw = b64()
        .decode(text)
        .map_err(|_| PairError::Protocol("bad base64".into()))?;
    let plain = cipher
        .decrypt(Nonce::from_slice(&nonce), raw.as_ref())
        .map_err(|_| PairError::Protocol("sealed identity failed to open".into()))?;
    serde_json::from_slice(&plain).map_err(|e| PairError::Protocol(e.to_string()))
}

fn public_of(s: &Sealed) -> Result<PublicIdentity, PairError> {
    PublicIdentity::from_b64(&s.x25519, &s.mlkem).map_err(|e| PairError::Protocol(e.to_string()))
}

/// A random 4-digit PIN, as Moonlight shows.
pub fn new_pin() -> String {
    let n = rand_core::RngCore::next_u32(&mut rand_core::OsRng) % 10_000;
    format!("{n:04}")
}

const CLIENT_ID: &[u8] = b"pingpong client";
const HOST_ID: &[u8] = b"pingpong host";

/// What the client learns from a successful pairing.
#[derive(Debug, Clone)]
pub struct PairedHost {
    pub name: String,
    pub public: PublicIdentity,
    pub extras: Extras,
}

/// Stops a client's pairing from another thread: the connection is closed,
/// so the host drops the request from its web UI.
#[derive(Clone, Default)]
pub struct Cancel {
    cancelled: Arc<AtomicBool>,
    stream: Arc<Mutex<Option<TcpStream>>>,
}

impl Cancel {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(s) = self.stream.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Client side. Blocks until the user has typed `pin` on the host (or it
/// fails). `on_waiting` fires once the host is showing its PIN prompt.
pub fn pair_client(
    addr: SocketAddr,
    client_name: &str,
    identity: &PublicIdentity,
    pin: &str,
    on_waiting: impl FnOnce(),
) -> Result<PairedHost, PairError> {
    pair_client_with(
        addr,
        client_name,
        identity,
        &Extras::default(),
        pin,
        &Cancel::default(),
        on_waiting,
    )
}

/// [`pair_client`], sending `extras` and stoppable with `cancel`.
pub fn pair_client_with(
    addr: SocketAddr,
    client_name: &str,
    identity: &PublicIdentity,
    extras: &Extras,
    pin: &str,
    cancel: &Cancel,
    on_waiting: impl FnOnce(),
) -> Result<PairedHost, PairError> {
    let result = pair_inner(addr, client_name, identity, extras, pin, cancel, on_waiting);
    if cancel.is_cancelled() {
        return Err(PairError::Cancelled);
    }
    result
}

fn pair_inner(
    addr: SocketAddr,
    client_name: &str,
    identity: &PublicIdentity,
    extras: &Extras,
    pin: &str,
    cancel: &Cancel,
    on_waiting: impl FnOnce(),
) -> Result<PairedHost, PairError> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    *cancel.stream.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.try_clone()?);
    if cancel.is_cancelled() {
        return Err(PairError::Cancelled);
    }
    s.set_read_timeout(Some(PIN_TIMEOUT + Duration::from_secs(30)))?;
    let (state, msg_a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(pin.as_bytes()),
        &Identity::new(CLIENT_ID),
        &Identity::new(HOST_ID),
    );
    // This pairing's own ML-KEM key: used once, then gone.
    let (dk, ek) = MlKem768::generate_keypair();
    let kem_key = ek.to_bytes().to_vec();
    write_msg(
        &mut s,
        &Msg::Hello {
            version: VERSION,
            name: client_name.to_string(),
            spake: b64().encode(&msg_a),
            agent: extras.agent,
            kem: b64().encode(&kem_key),
        },
    )?;

    let mut on_waiting = Some(on_waiting);
    let (msg_b, host_confirm, kem_ct) = loop {
        match read_msg(&mut s)? {
            Msg::Waiting => {
                if let Some(f) = on_waiting.take() {
                    f();
                }
            }
            Msg::Challenge {
                spake,
                confirm,
                kem,
            } => {
                let b = b64()
                    .decode(spake)
                    .map_err(|_| PairError::Protocol("bad base64".into()))?;
                let c = b64()
                    .decode(confirm)
                    .map_err(|_| PairError::Protocol("bad base64".into()))?;
                let k = b64()
                    .decode(kem)
                    .map_err(|_| PairError::Protocol("bad base64".into()))?;
                break (b, c, k);
            }
            Msg::Refused { reason } => return Err(PairError::Refused(reason)),
            other => return Err(PairError::Protocol(format!("unexpected {other:?}"))),
        }
    };
    let shared = state.finish(&msg_b).map_err(|_| PairError::WrongPin)?;
    let ct: [u8; KEM_CT_LEN] = kem_ct
        .as_slice()
        .try_into()
        .map_err(|_| PairError::Protocol("bad ML-KEM ciphertext".into()))?;
    let kem_shared = dk.decapsulate(&ct.into());
    let keys = derive(&shared, &kem_shared);
    let t = Transcript {
        msg_a: &msg_a,
        msg_b: &msg_b,
        kem_key: &kem_key,
        kem_ct: &kem_ct,
    };
    if !verify_tag(&keys, HOST_ID, &t, &host_confirm) {
        return Err(PairError::WrongPin);
    }
    let proof = Msg::Proof {
        confirm: b64().encode(confirm_tag(&keys, CLIENT_ID, &t)),
        sealed: seal(&keys, 0, &extras.into_sealed(client_name, identity)),
    };
    write_msg(&mut s, &proof)?;
    match read_msg(&mut s)? {
        Msg::Welcome { sealed } => {
            let host = open(&keys, 1, &sealed)?;
            Ok(PairedHost {
                public: public_of(&host)?,
                extras: Extras::from_sealed(&host),
                name: host.name,
            })
        }
        Msg::Refused { reason } => Err(PairError::Refused(reason)),
        other => Err(PairError::Protocol(format!("unexpected {other:?}"))),
    }
}

/// Ask a host for its name and id (no pairing, no secrets).
pub fn host_info(addr: SocketAddr) -> Result<(String, String, String, u16), PairError> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    write_msg(&mut s, &Msg::Info)?;
    match read_msg(&mut s)? {
        Msg::HostInfo {
            name,
            id,
            version,
            port,
        } => Ok((name, id, version, port)),
        other => Err(PairError::Protocol(format!("unexpected {other:?}"))),
    }
}

/// Host side: a client asking to pair, waiting for the PIN.
pub struct PairRequest {
    pub client_name: String,
    /// The client says it is an AI agent (for the prompt; the sealed extras
    /// confirm it).
    pub agent: bool,
    pub peer: SocketAddr,
    stream: TcpStream,
    msg_a: Vec<u8>,
    kem_key: Vec<u8>,
}

pub enum Incoming {
    /// Answered already; nothing else to do.
    Info,
    Pair(PairRequest),
}

pub struct HostDescription {
    pub name: String,
    pub id: String,
    pub version: String,
    pub port: u16,
}

/// Read a new connection's first message. `Info` is answered here; a pairing
/// request is returned after telling the client the prompt is up.
pub fn accept(mut stream: TcpStream, me: &HostDescription) -> Result<Incoming, PairError> {
    let peer = stream.peer_addr()?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    match read_msg(&mut stream)? {
        Msg::Info => {
            write_msg(
                &mut stream,
                &Msg::HostInfo {
                    name: me.name.clone(),
                    id: me.id.clone(),
                    version: me.version.clone(),
                    port: me.port,
                },
            )?;
            Ok(Incoming::Info)
        }
        Msg::Hello {
            version,
            name,
            spake,
            agent,
            kem,
        } => {
            if version != VERSION {
                // Version 1 (SPAKE2 alone) is not offered even to an old
                // client: an attacker could otherwise force it.
                let reason = if version < VERSION {
                    "This host pairs with a newer, post-quantum exchange: update Ping (or \
                        the agent) to pair with it."
                } else {
                    "This host is older than the device pairing with it: update Pong to pair."
                };
                let _ = write_msg(
                    &mut stream,
                    &Msg::Refused {
                        reason: reason.into(),
                    },
                );
                return Err(PairError::Protocol(format!("pairing version {version}")));
            }
            let msg_a = b64()
                .decode(spake)
                .map_err(|_| PairError::Protocol("bad base64".into()))?;
            let kem_key = b64()
                .decode(kem)
                .map_err(|_| PairError::Protocol("bad base64".into()))?;
            if let Err(e) = self::kem_key(&kem_key) {
                let _ = write_msg(
                    &mut stream,
                    &Msg::Refused {
                        reason: "the pairing request is malformed".into(),
                    },
                );
                return Err(e);
            }
            let name: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
            write_msg(&mut stream, &Msg::Waiting)?;
            Ok(Incoming::Pair(PairRequest {
                client_name: name,
                agent,
                peer,
                stream,
                msg_a,
                kem_key,
            }))
        }
        other => Err(PairError::Protocol(format!("unexpected {other:?}"))),
    }
}

impl PairRequest {
    /// Whether the client is still connected (it hangs up when it cancels).
    pub fn is_alive(&self) -> bool {
        if self.stream.set_nonblocking(true).is_err() {
            return false;
        }
        let r = self.stream.peek(&mut [0u8; 1]);
        let _ = self.stream.set_nonblocking(false);
        match r {
            Ok(0) => false,
            Ok(_) => true,
            Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
        }
    }

    /// Refuse without a PIN (the user declined, or it timed out).
    pub fn refuse(mut self, reason: &str) {
        let _ = write_msg(
            &mut self.stream,
            &Msg::Refused {
                reason: reason.to_string(),
            },
        );
    }

    /// Finish with the PIN the user typed. Returns the client's name and keys.
    /// Finish with the PIN the user typed, sending `extras` (the host's
    /// rendezvous key and secret). Returns the client's name, keys and extras.
    pub fn complete(
        mut self,
        pin: &str,
        host_name: &str,
        host: &PublicIdentity,
        extras: &Extras,
    ) -> Result<(String, PublicIdentity, Extras), PairError> {
        let (state, msg_b) = Spake2::<Ed25519Group>::start_b(
            &Password::new(pin.trim().as_bytes()),
            &Identity::new(CLIENT_ID),
            &Identity::new(HOST_ID),
        );
        let shared = state.finish(&self.msg_a).map_err(|_| PairError::WrongPin)?;
        let (ct, kem_shared) = kem_key(&self.kem_key)?.encapsulate();
        let kem_ct = ct.to_vec();
        let keys = derive(&shared, &kem_shared);
        let t = Transcript {
            msg_a: &self.msg_a,
            msg_b: &msg_b,
            kem_key: &self.kem_key,
            kem_ct: &kem_ct,
        };
        write_msg(
            &mut self.stream,
            &Msg::Challenge {
                spake: b64().encode(&msg_b),
                confirm: b64().encode(confirm_tag(&keys, HOST_ID, &t)),
                kem: b64().encode(&kem_ct),
            },
        )?;
        self.stream
            .set_read_timeout(Some(Duration::from_secs(10)))?;
        let (confirm, sealed) = match read_msg(&mut self.stream) {
            Ok(Msg::Proof { confirm, sealed }) => (confirm, sealed),
            // A client whose PIN differs rejects our confirmation and hangs up.
            Ok(_) | Err(PairError::Io(_)) => return Err(PairError::WrongPin),
            Err(e) => return Err(e),
        };
        let tag = b64()
            .decode(confirm)
            .map_err(|_| PairError::Protocol("bad base64".into()))?;
        if !verify_tag(&keys, CLIENT_ID, &t, &tag) {
            return Err(PairError::WrongPin);
        }
        let client = open(&keys, 0, &sealed)?;
        let public = public_of(&client)?;
        write_msg(
            &mut self.stream,
            &Msg::Welcome {
                sealed: seal(&keys, 1, &extras.into_sealed(host_name, host)),
            },
        )?;
        Ok((self.client_name, public, Extras::from_sealed(&client)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_transport::Identity as Keys2;
    use std::net::TcpListener;

    type HostResult = Result<(String, PublicIdentity, Extras), PairError>;

    const HOST_EXTRAS: Extras = Extras {
        rendezvous: Some([7; 32]),
        rendezvous_secret: Some([9; 32]),
        agent: false,
    };

    fn host_thread(
        pin: &'static str,
    ) -> (
        SocketAddr,
        std::thread::JoinHandle<HostResult>,
        PublicIdentity,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let host = Keys2::generate().public().clone();
        let h = host.clone();
        let t = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let me = HostDescription {
                name: "box".into(),
                id: "id".into(),
                version: "3".into(),
                port: 47800,
            };
            match accept(stream, &me)? {
                Incoming::Pair(req) => {
                    assert_eq!(req.client_name, "mac");
                    req.complete(pin, "box", &h, &HOST_EXTRAS)
                }
                Incoming::Info => unreachable!(),
            }
        });
        (addr, t, host)
    }

    #[test]
    fn matching_pins_exchange_both_identities() {
        let (addr, host, host_public) = host_thread("4821");
        let client = Keys2::generate().public().clone();
        let mut waited = false;
        let mine = Extras {
            rendezvous: Some([3; 32]),
            rendezvous_secret: None,
            agent: false,
        };
        let paired = pair_client_with(
            addr,
            "mac",
            &client,
            &mine,
            "4821",
            &Cancel::default(),
            || waited = true,
        )
        .unwrap();
        assert!(waited);
        assert_eq!(paired.name, "box");
        assert_eq!(paired.public, host_public);
        assert_eq!(
            paired.extras, HOST_EXTRAS,
            "the client learns how to find the host"
        );
        let (name, got, extras) = host.join().unwrap().unwrap();
        assert_eq!(name, "mac");
        assert_eq!(got, client);
        assert_eq!(extras, mine, "and the host how to find the client");
    }

    #[test]
    fn an_agent_says_so_sealed() {
        let (addr, host, _) = host_thread("1111");
        let client = Keys2::generate().public().clone();
        let mine = Extras {
            rendezvous: None,
            rendezvous_secret: None,
            agent: true,
        };
        pair_client_with(
            addr,
            "mac",
            &client,
            &mine,
            "1111",
            &Cancel::default(),
            || {},
        )
        .unwrap();
        let (_, _, extras) = host.join().unwrap().unwrap();
        assert!(extras.agent);
    }

    #[test]
    fn a_wrong_pin_pairs_nobody() {
        let (addr, host, _) = host_thread("0000");
        let client = Keys2::generate().public().clone();
        let r = pair_client(addr, "mac", &client, "4821", || {});
        assert!(matches!(r, Err(PairError::WrongPin)), "{r:?}");
        assert!(matches!(host.join().unwrap(), Err(PairError::WrongPin)));
    }

    #[test]
    fn an_old_client_is_told_to_update() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let host = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let me = HostDescription {
                name: "box".into(),
                id: "id".into(),
                version: "3".into(),
                port: 47800,
            };
            accept(s, &me).map(|_| ())
        });
        let mut s = TcpStream::connect(addr).unwrap();
        let old = Msg::Hello {
            version: 1,
            name: "old".into(),
            spake: b64().encode([0u8; 33]),
            agent: false,
            kem: String::new(),
        };
        write_msg(&mut s, &old).unwrap();
        match read_msg(&mut s).unwrap() {
            Msg::Refused { reason } => assert!(reason.contains("update Ping"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(host.join().unwrap().is_err(), "no fallback to SPAKE2 alone");
    }

    /// Between client and host, changing the ML-KEM ciphertext on its way:
    /// the keys then differ, so the confirmation fails.
    #[test]
    fn a_changed_ciphertext_pairs_nobody() {
        let (host_addr, host, _) = host_thread("4821");
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = proxy.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut c, _) = proxy.accept().unwrap();
            let mut h = TcpStream::connect(host_addr).unwrap();
            let hello = read_msg(&mut c).unwrap();
            write_msg(&mut h, &hello).unwrap();
            loop {
                let Ok(msg) = read_msg(&mut h) else { return };
                let msg = match msg {
                    Msg::Challenge {
                        spake,
                        confirm,
                        kem,
                    } => {
                        let mut ct = b64().decode(kem).unwrap();
                        ct[0] ^= 1;
                        Msg::Challenge {
                            spake,
                            confirm,
                            kem: b64().encode(ct),
                        }
                    }
                    m => m,
                };
                let challenge = matches!(msg, Msg::Challenge { .. });
                if write_msg(&mut c, &msg).is_err() {
                    return;
                }
                if challenge {
                    // Whatever the client says next goes on to the host.
                    match read_msg(&mut c) {
                        Ok(m) => {
                            let _ = write_msg(&mut h, &m);
                        }
                        Err(_) => return,
                    }
                }
            }
        });
        let client = Keys2::generate().public().clone();
        let r = pair_client(proxy_addr, "mac", &client, "4821", || {});
        assert!(matches!(r, Err(PairError::WrongPin)), "{r:?}");
        assert!(host.join().unwrap().is_err());
    }

    #[test]
    fn both_secrets_make_the_keys() {
        let a = derive(&[1; 32], &[2; 32]);
        let b = derive(&[1; 32], &[3; 32]);
        assert_ne!(a.seal, b.seal, "the ML-KEM secret changes the keys");
        assert_ne!(
            a.confirm,
            derive(&[4; 32], &[2; 32]).confirm,
            "and so does SPAKE2's"
        );
    }

    #[test]
    fn info_needs_no_pairing() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let me = HostDescription {
                name: "box".into(),
                id: "abc".into(),
                version: "3".into(),
                port: 47800,
            };
            let _ = accept(s, &me);
        });
        let (name, id, _, port) = host_info(addr).unwrap();
        assert_eq!((name.as_str(), id.as_str(), port), ("box", "abc", 47800));
    }

    #[test]
    fn a_cancelled_client_is_seen_as_gone_by_the_host() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let cancel = Cancel::default();
        let client = {
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let id = pingpong_transport::Identity::generate();
                pair_client_with(
                    addr,
                    "laptop",
                    id.public(),
                    &Extras::default(),
                    "1234",
                    &cancel,
                    || {},
                )
            })
        };
        let (stream, _) = listener.accept().unwrap();
        let me = HostDescription {
            name: "h".into(),
            id: "x".into(),
            version: "0".into(),
            port: 1,
        };
        let Ok(Incoming::Pair(req)) = accept(stream, &me) else {
            panic!("expected a pairing request")
        };
        assert!(req.is_alive());
        cancel.cancel();
        assert!(matches!(client.join().unwrap(), Err(PairError::Cancelled)));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while req.is_alive() {
            assert!(
                std::time::Instant::now() < deadline,
                "host still thinks the client is there"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
