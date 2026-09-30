//! Paired clients. Pairing adds one; the web UI lists and removes them.
//!
//! A client is a person's device, or an AI agent's identity on one: an agent
//! pairs with a key of its own, so the host knows it by key, not by what it
//! claims, and holds its sessions to the agent rules (`session`): it never
//! takes over a person, a person always takes over from it, and its access
//! can be cut to view-only or off here without unpairing it.

use std::path::{Path, PathBuf};

use pingpong_transport::PublicIdentity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Client {
    pub name: String,
    pub x25519: String,
    pub mlkem: String,
    /// Seconds since the Unix epoch.
    pub paired_at: u64,
    /// The client's rendezvous key (Ed25519, hex), for connecting from the
    /// internet. Absent for clients paired before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendezvous: Option<String>,
    /// An AI agent's identity (paired as one).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent: bool,
    /// What an agent may do here (ignored for people).
    #[serde(default, skip_serializing_if = "Access::is_default")]
    pub access: Access,
}

/// What an agent identity may do on this host.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// See the screen and use the keyboard and mouse.
    #[default]
    Control,
    /// See the screen only.
    View,
    /// Nothing: its sessions are refused.
    Off,
}

impl Access {
    fn is_default(&self) -> bool {
        *self == Access::Control
    }

    pub fn parse(s: &str) -> Option<Access> {
        match s {
            "control" => Some(Access::Control),
            "view" => Some(Access::View),
            "off" => Some(Access::Off),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Access::Control => "control",
            Access::View => "view",
            Access::Off => "off",
        }
    }
}

/// Who a peer is, as far as sessions go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Person,
    Agent(Access),
}

impl Client {
    pub fn public(&self) -> Option<PublicIdentity> {
        PublicIdentity::from_b64(&self.x25519, &self.mlkem).ok()
    }

    pub fn rendezvous_key(&self) -> Option<[u8; 32]> {
        let h = self.rendezvous.as_deref()?;
        let bytes: Vec<u8> = (0..h.len())
            .step_by(2)
            .filter_map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
            .collect();
        bytes.try_into().ok()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default, rename = "client")]
    clients: Vec<Client>,
}

pub struct Clients {
    path: PathBuf,
    list: Vec<Client>,
}

impl Clients {
    pub fn load(dir: &Path) -> Clients {
        let path = dir.join("clients.toml");
        let list = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str::<File>(&t).ok())
            .map(|f| f.clients)
            .unwrap_or_default();
        Clients { path, list }
    }

    pub fn list(&self) -> &[Client] {
        &self.list
    }

    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(&File {
            clients: self.list.clone(),
        })
        .map_err(std::io::Error::other)?;
        pingpong_transport::identity::write_private(&self.path, text.as_bytes())
    }

    /// Add (or rename, if the key is already known) a client. `agent`: the
    /// identity is an AI agent's.
    pub fn add(
        &mut self,
        name: &str,
        public: &PublicIdentity,
        rendezvous: Option<[u8; 32]>,
        agent: bool,
    ) -> std::io::Result<Client> {
        let (x, m) = public.to_b64();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rendezvous = rendezvous.map(|k| k.iter().map(|b| format!("{b:02x}")).collect());
        let client = Client {
            name: name.to_string(),
            x25519: x.clone(),
            mlkem: m,
            paired_at: now,
            rendezvous,
            agent,
            access: Access::default(),
        };
        match self.list.iter_mut().find(|c| c.x25519 == x) {
            Some(existing) => *existing = client.clone(),
            None => self.list.push(client.clone()),
        }
        self.save()?;
        Ok(client)
    }

    /// Record a client's rendezvous key (learned over the tunnel from a
    /// client paired before pairing exchanged it). True if it changed.
    pub fn set_rendezvous(&mut self, x25519_b64: &str, key: [u8; 32]) -> std::io::Result<bool> {
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let Some(c) = self.list.iter_mut().find(|c| c.x25519 == x25519_b64) else {
            return Ok(false);
        };
        if c.rendezvous.as_deref() == Some(hex.as_str()) {
            return Ok(false);
        }
        c.rendezvous = Some(hex);
        self.save()?;
        Ok(true)
    }

    pub fn remove(&mut self, x25519_b64: &str) -> std::io::Result<Option<Client>> {
        let Some(i) = self.list.iter().position(|c| c.x25519 == x25519_b64) else {
            return Ok(None);
        };
        let removed = self.list.remove(i);
        self.save()?;
        Ok(Some(removed))
    }

    /// Set what an agent may do. False if there is no such client.
    pub fn set_access(&mut self, x25519_b64: &str, access: Access) -> std::io::Result<bool> {
        let Some(c) = self.list.iter_mut().find(|c| c.x25519 == x25519_b64) else {
            return Ok(false);
        };
        c.access = access;
        self.save()?;
        Ok(true)
    }

    /// Who the client with this key is (a person when unknown: every peer
    /// the tunnel admits is paired).
    pub fn role_of(&self, key: &[u8; 32]) -> Role {
        match self
            .list
            .iter()
            .find(|c| c.public().is_some_and(|p| &p.x25519 == key))
        {
            Some(c) if c.agent => Role::Agent(c.access),
            _ => Role::Person,
        }
    }

    pub fn name_of(&self, key: &[u8; 32]) -> Option<&str> {
        self.list
            .iter()
            .find(|c| c.public().is_some_and(|p| &p.x25519 == key))
            .map(|c| c.name.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_transport::Identity;

    #[test]
    fn clients_persist_and_are_keyed_by_public_key() {
        let dir = tempfile::tempdir().unwrap();
        let id = Identity::generate();
        let mut c = Clients::load(dir.path());
        c.add("mac", id.public(), None, false).unwrap();
        c.add("macbook", id.public(), None, false).unwrap();
        let c = Clients::load(dir.path());
        assert_eq!(c.list().len(), 1);
        assert_eq!(c.name_of(&id.public().x25519), Some("macbook"));
        assert_eq!(c.list()[0].public().as_ref(), Some(id.public()));
        assert_eq!(c.role_of(&id.public().x25519), Role::Person);
    }

    #[test]
    fn agents_are_known_by_key_and_their_access_persists() {
        let dir = tempfile::tempdir().unwrap();
        let (person, agent) = (Identity::generate(), Identity::generate());
        let mut c = Clients::load(dir.path());
        c.add("mac", person.public(), None, false).unwrap();
        c.add("mac agent", agent.public(), None, true).unwrap();
        assert_eq!(
            c.role_of(&agent.public().x25519),
            Role::Agent(Access::Control)
        );
        let (x, _) = agent.public().to_b64();
        assert!(c.set_access(&x, Access::View).unwrap());
        let c = Clients::load(dir.path());
        assert_eq!(c.role_of(&agent.public().x25519), Role::Agent(Access::View));
        assert_eq!(c.role_of(&person.public().x25519), Role::Person);
        // A file from before agents reads as people.
        std::fs::write(
            dir.path().join("clients.toml"),
            "[[client]]\nname = \"old\"\nx25519 = \"a\"\nmlkem = \"b\"\npaired_at = 1\n",
        )
        .unwrap();
        let c = Clients::load(dir.path());
        assert!(!c.list()[0].agent);
    }
}
