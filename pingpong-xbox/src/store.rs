//! The signed-in Microsoft account, kept between runs: `xbox.json` in Ping's
//! data folder, readable only by its owner (it holds the refresh token,
//! which signs in as the account until it is revoked).
//!
//! Every token is kept with its expiry, so starting a stream costs no
//! sign-in round trips while they last (Xbox Live's tokens last hours,
//! the streaming service's a few). Signing out deletes the file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::time::now_unix;

/// A token is renewed this long before it expires, so it does not expire
/// in the middle of the requests that use it.
pub const RENEW_BEFORE_SECS: u64 = 5 * 60;

pub const FILE: &str = "xbox.json";

/// An Xbox Live (XSTS) token: the token, the user hash it is used with, and
/// when it expires.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XstsToken {
    pub token: String,
    pub uhs: String,
    /// Seconds since the epoch.
    pub expires: u64,
}

impl XstsToken {
    pub fn is_fresh(&self) -> bool {
        !self.token.is_empty() && self.expires > now_unix() + RENEW_BEFORE_SECS
    }

    /// The `Authorization` header Xbox Live's web APIs take.
    pub fn authorization(&self) -> String {
        format!("XBL3.0 x={};{}", self.uhs, self.token)
    }
}

/// A region of the streaming service.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub name: String,
    /// `https://…` without a trailing slash.
    pub base_uri: String,
    pub is_default: bool,
}

/// A streaming service token (`gsToken`) for one offering.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamingToken {
    pub token: String,
    pub regions: Vec<Region>,
    /// The account's market (`"US"`), for the catalogue.
    pub market: String,
    pub expires: u64,
}

impl StreamingToken {
    pub fn is_fresh(&self) -> bool {
        !self.token.is_empty() && self.expires > now_unix() + RENEW_BEFORE_SECS
    }

    /// The region the service picked for this account, or the first.
    pub fn default_region(&self) -> Option<&Region> {
        self.regions
            .iter()
            .find(|r| r.is_default)
            .or(self.regions.first())
    }

    /// The region named `name` (case aside), when the user chose one.
    pub fn region(&self, name: &str) -> Option<&Region> {
        self.regions
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(name))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    /// The Microsoft account's OAuth tokens.
    pub refresh_token: String,
    pub access_token: String,
    pub access_expires: u64,
    /// Xbox Live: the user token, and XSTS tokens for the web APIs
    /// (`http://xboxlive.com`) and the streaming service.
    pub user_token: XstsToken,
    pub web_token: XstsToken,
    pub gssv_token: XstsToken,
    /// By offering: `xhome`, and `xgpuweb` or `xgpuwebf2p` for the cloud.
    pub streaming: BTreeMap<String, StreamingToken>,
    /// The cloud offering this account has, once known (`xgpuweb` with Game
    /// Pass Ultimate, `xgpuwebf2p` for free-to-play games only), or
    /// `"none"` where the cloud is not offered to it.
    pub cloud_offering: String,
    pub gamertag: String,
    pub xuid: String,
    /// This installation's id, which the console is told; made once.
    pub install_id: String,
}

impl Account {
    pub fn is_signed_in(&self) -> bool {
        !self.refresh_token.is_empty()
    }
}

/// The account file in `dir`.
pub struct AccountStore {
    path: PathBuf,
}

impl AccountStore {
    pub fn new(dir: &Path) -> AccountStore {
        AccountStore {
            path: dir.join(FILE),
        }
    }

    /// The stored account, or a signed-out one (no file, or a damaged one:
    /// signing in again writes a good one).
    pub fn load(&self) -> Account {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Account::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "the Xbox account file is damaged; signed out");
            Account::default()
        })
    }

    pub fn save(&self, account: &Account) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text = serde_json::to_string_pretty(account).map_err(|e| e.to_string())?;
        pingpong_transport::identity::write_private(&self.path, text.as_bytes())
            .map_err(|e| format!("cannot save the Xbox account: {e}"))
    }

    /// Sign out: forget every token.
    pub fn clear(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot sign out: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_round_trips_through_its_owner_only_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        assert!(!store.load().is_signed_in());
        let mut a = Account {
            refresh_token: "r".into(),
            gamertag: "Player".into(),
            ..Default::default()
        };
        a.streaming.insert(
            "xhome".into(),
            StreamingToken {
                token: "g".into(),
                regions: vec![Region {
                    name: "WestEurope".into(),
                    base_uri: "https://weu.gssv-play-prodxhome.xboxlive.com".into(),
                    is_default: true,
                }],
                market: "RO".into(),
                expires: 1,
            },
        );
        store.save(&a).unwrap();
        assert_eq!(store.load(), a);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        store.clear().unwrap();
        assert!(!store.load().is_signed_in());
        store.clear().unwrap();
    }

    #[test]
    fn a_damaged_file_is_signed_out_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE), "{ not json").unwrap();
        assert_eq!(AccountStore::new(dir.path()).load(), Account::default());
    }

    #[test]
    fn tokens_are_renewed_before_they_expire() {
        let soon = XstsToken {
            token: "t".into(),
            uhs: "u".into(),
            expires: now_unix() + 60,
        };
        assert!(!soon.is_fresh());
        let later = XstsToken {
            expires: now_unix() + 3600,
            ..soon.clone()
        };
        assert!(later.is_fresh());
        assert_eq!(later.authorization(), "XBL3.0 x=u;t");
        assert!(!XstsToken::default().is_fresh());
    }

    #[test]
    fn the_default_region_is_the_one_marked() {
        let t = StreamingToken {
            regions: vec![
                Region {
                    name: "A".into(),
                    ..Default::default()
                },
                Region {
                    name: "B".into(),
                    is_default: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(t.default_region().unwrap().name, "B");
        assert_eq!(t.region("a").unwrap().name, "A");
        assert!(StreamingToken::default().default_region().is_none());
    }
}
