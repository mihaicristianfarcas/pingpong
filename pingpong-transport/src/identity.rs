//! Long-term keys: an X25519 static keypair (the WireGuard identity) and an
//! ML-KEM-768 static keypair for pq-boringtun's static-KEM authentication.
//!
//! The private half is generated once per install and never leaves the
//! machine; the public half is what pairing exchanges.

use std::path::Path;
use std::sync::Arc;

use base64::Engine;
use boringtun::noise::handshake::{MlKemPublicKey, MlKemStaticSecret};
use boringtun::noise::MLKEM768_SEED_SIZE;
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use x25519_dalek::{PublicKey, StaticSecret};

/// ML-KEM-768 encapsulation key size.
pub const MLKEM_PUBLIC_LEN: usize = 1184;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

#[derive(Debug)]
pub enum IdentityError {
    Io(std::io::Error),
    Parse(String),
    BadKey(&'static str),
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::Io(e) => write!(f, "{e}"),
            IdentityError::Parse(e) => write!(f, "{e}"),
            IdentityError::BadKey(what) => write!(f, "malformed {what}"),
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<std::io::Error> for IdentityError {
    fn from(e: std::io::Error) -> Self {
        IdentityError::Io(e)
    }
}

/// This machine's keys.
pub struct Identity {
    x25519: StaticSecret,
    mlkem_seed: [u8; MLKEM768_SEED_SIZE],
    mlkem: Arc<MlKemStaticSecret>,
    public: PublicIdentity,
}

/// What a peer needs to know about us: both public keys.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PublicIdentity {
    pub x25519: [u8; 32],
    pub mlkem: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct IdentityFile {
    x25519_private: String,
    mlkem_seed: String,
}

impl Identity {
    pub fn generate() -> Identity {
        let x25519 = StaticSecret::random_from_rng(rand_core::OsRng);
        let mut seed = [0u8; MLKEM768_SEED_SIZE];
        rand_core::OsRng.fill_bytes(&mut seed);
        Identity::from_parts(x25519, seed)
    }

    fn from_parts(x25519: StaticSecret, mlkem_seed: [u8; MLKEM768_SEED_SIZE]) -> Identity {
        let mlkem = Arc::new(MlKemStaticSecret::from_seed(&mlkem_seed));
        let public = PublicIdentity {
            x25519: *PublicKey::from(&x25519).as_bytes(),
            mlkem: mlkem.encapsulation_key_bytes().to_vec(),
        };
        Identity {
            x25519,
            mlkem_seed,
            mlkem,
            public,
        }
    }

    /// Load the identity at `path`, or create and save one if there is none.
    pub fn load_or_create(path: &Path) -> Result<Identity, IdentityError> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let file: IdentityFile =
                    toml::from_str(&text).map_err(|e| IdentityError::Parse(e.to_string()))?;
                let x: [u8; 32] = b64()
                    .decode(file.x25519_private.trim())
                    .ok()
                    .and_then(|v| v.try_into().ok())
                    .ok_or(IdentityError::BadKey("x25519_private"))?;
                let seed: [u8; MLKEM768_SEED_SIZE] = b64()
                    .decode(file.mlkem_seed.trim())
                    .ok()
                    .and_then(|v| v.try_into().ok())
                    .ok_or(IdentityError::BadKey("mlkem_seed"))?;
                Ok(Identity::from_parts(StaticSecret::from(x), seed))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Identity::generate();
                id.save(path)?;
                Ok(id)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), IdentityError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = IdentityFile {
            x25519_private: b64().encode(self.x25519.to_bytes()),
            mlkem_seed: b64().encode(self.mlkem_seed),
        };
        let text = toml::to_string(&file).map_err(|e| IdentityError::Parse(e.to_string()))?;
        write_private(path, text.as_bytes())?;
        Ok(())
    }

    pub fn public(&self) -> &PublicIdentity {
        &self.public
    }

    pub(crate) fn x25519(&self) -> &StaticSecret {
        &self.x25519
    }

    pub(crate) fn x25519_public(&self) -> PublicKey {
        PublicKey::from(self.public.x25519)
    }

    pub(crate) fn mlkem(&self) -> &Arc<MlKemStaticSecret> {
        &self.mlkem
    }
}

impl PublicIdentity {
    pub fn to_b64(&self) -> (String, String) {
        (b64().encode(self.x25519), b64().encode(&self.mlkem))
    }

    pub fn from_b64(x25519: &str, mlkem: &str) -> Result<PublicIdentity, IdentityError> {
        let x: [u8; 32] = b64()
            .decode(x25519.trim())
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or(IdentityError::BadKey("x25519 public key"))?;
        let m = b64()
            .decode(mlkem.trim())
            .map_err(|_| IdentityError::BadKey("ML-KEM public key"))?;
        PublicIdentity::from_bytes(x, m)
    }

    pub fn from_bytes(x25519: [u8; 32], mlkem: Vec<u8>) -> Result<PublicIdentity, IdentityError> {
        if mlkem.len() != MLKEM_PUBLIC_LEN || MlKemPublicKey::from_bytes(&mlkem).is_err() {
            return Err(IdentityError::BadKey("ML-KEM public key"));
        }
        Ok(PublicIdentity { x25519, mlkem })
    }

    /// A short, stable, human-comparable id (first 8 bytes of the X25519 key,
    /// hex). Used for display and file names, never for authentication.
    pub fn short_id(&self) -> String {
        self.x25519[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    pub(crate) fn mlkem_key(&self) -> MlKemPublicKey {
        MlKemPublicKey::from_bytes(&self.mlkem).expect("validated on construction")
    }
}

/// Write a file readable only by its owner.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        #[cfg(unix)]
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut f = std::fs::File::create(&tmp)?;
        std::io::Write::write_all(&mut f, bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("identity.toml");
        let a = Identity::load_or_create(&path).unwrap();
        let b = Identity::load_or_create(&path).unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(a.public().mlkem.len(), MLKEM_PUBLIC_LEN);
    }

    #[test]
    fn public_identity_round_trips_through_base64() {
        let id = Identity::generate();
        let (x, m) = id.public().to_b64();
        assert_eq!(&PublicIdentity::from_b64(&x, &m).unwrap(), id.public());
    }

    #[test]
    fn a_truncated_mlkem_key_is_rejected() {
        let id = Identity::generate();
        let mut bad = id.public().mlkem.clone();
        bad.pop();
        assert!(PublicIdentity::from_bytes(id.public().x25519, bad).is_err());
    }
}
