//! The Xbox account, for the app and the CLI: signing in and out, the
//! account's consoles, the cloud library. Each call that talks to the
//! services blocks for a round trip or a few: call them off the UI thread.

use std::sync::atomic::AtomicBool;

use pingpong_xbox::auth::{self, Auth, Offering};
use pingpong_xbox::gssv::{self, Kind, Service};
use pingpong_xbox::http::Http;
use pingpong_xbox::store::AccountStore;

pub use pingpong_xbox::auth::{AuthError, DeviceCode};
pub use pingpong_xbox::consoles::{Command, Console};
pub use pingpong_xbox::people::Friend;

/// A game the account may play in the cloud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudGame {
    /// What a cloud stream is started with.
    pub title_id: String,
    pub name: String,
    pub publisher: String,
}

/// What the account has in the cloud.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudLibrary {
    /// Game Pass Ultimate's whole catalogue, or free-to-play games only.
    pub game_pass: bool,
    pub games: Vec<CloudGame>,
}

fn http() -> Http {
    Http::new()
}

fn load() -> Result<Auth, AuthError> {
    Auth::load(http(), &crate::store::data_dir())
}

/// The signed-in account's gamertag, without asking the network; `None`
/// when signed out.
pub fn signed_in() -> Option<String> {
    let account = AccountStore::new(&crate::store::data_dir()).load();
    account.is_signed_in().then_some(account.gamertag)
}

/// Start signing in: the code to show, and where it is typed.
pub fn start_sign_in() -> Result<DeviceCode, AuthError> {
    auth::start_sign_in(&http())
}

/// Wait for the code to be entered (`cancel` stops waiting); the gamertag.
pub fn finish_sign_in(code: &DeviceCode, cancel: &AtomicBool) -> Result<String, AuthError> {
    auth::finish_sign_in(&http(), code, &crate::store::data_dir(), cancel).map(|a| a.gamertag)
}

pub fn sign_out() -> Result<(), String> {
    Auth::sign_out(&crate::store::data_dir())
}

/// The account's consoles.
pub fn consoles() -> Result<Vec<Console>, AuthError> {
    pingpong_xbox::consoles::list(&mut load()?)
}

/// The account's friends, those online first.
pub fn friends() -> Result<Vec<Friend>, AuthError> {
    pingpong_xbox::people::friends(&mut load()?)
}

/// Send `command` to the console `id` (wake it, turn it off).
pub fn command(id: &str, command: Command) -> Result<(), AuthError> {
    pingpong_xbox::consoles::send(&mut load()?, id, command)
}

/// The games the account may play in the cloud, by name; `None` when the
/// cloud is not offered to it. `recheck`: ask again whether it is (the
/// account may have joined Game Pass since it was asked).
pub fn cloud_library(recheck: bool) -> Result<Option<CloudLibrary>, AuthError> {
    let mut auth = load()?;
    let Some(offering) = auth.cloud_offering(recheck)? else {
        return Ok(None);
    };
    let token = auth.streaming_token(offering)?;
    let failed = |e: gssv::GssvError| AuthError::Failed(e.to_string());
    let service = Service::new(auth.http.clone(), &token, None, Kind::Cloud).map_err(failed)?;
    let titles: Vec<_> = service
        .titles()
        .map_err(failed)?
        .into_iter()
        .filter(|t| t.playable)
        .collect();
    let ids: Vec<String> = titles
        .iter()
        .map(|t| t.product_id.clone())
        .filter(|p| !p.is_empty())
        .collect();
    // Names are a nicety: without the catalogue, titles show by their id.
    let names = gssv::products(&auth.http, &token.market, &ids).unwrap_or_default();
    let mut games: Vec<CloudGame> = titles
        .into_iter()
        .map(|t| {
            let product = names.get(&t.product_id);
            CloudGame {
                name: product
                    .map(|p| p.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| t.title_id.clone()),
                publisher: product.map(|p| p.publisher.clone()).unwrap_or_default(),
                title_id: t.title_id,
            }
        })
        .collect();
    games.sort_by_key(|g| g.name.to_lowercase());
    Ok(Some(CloudLibrary {
        game_pass: offering == Offering::Cloud,
        games,
    }))
}
