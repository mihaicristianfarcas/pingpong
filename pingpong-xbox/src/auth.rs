//! Signing in with a Microsoft account, and the tokens each Xbox service
//! takes, as Greenlight's sign-in library does it (`xal-node`, `msal.ts`):
//!
//! 1. **Device code** (OAuth 2.0, RFC 8628): Ping shows a code; the user
//!    types it at microsoft.com/link on any device and signs in there. Ping
//!    never sees the password. Greenlight's client id and scopes: the Xbox
//!    app's public client, `xboxlive.signin offline_access`.
//! 2. **Xbox Live user token** from the Microsoft access token
//!    (`user.auth.xboxlive.com`), then **XSTS tokens** from it for each
//!    relying party: `http://xboxlive.com` (consoles, profile) and
//!    `http://gssv.xboxlive.com/` (streaming).
//! 3. **Streaming tokens** (`gsToken`) per offering from the streaming
//!    service: `xhome` for consoles, `xgpuweb` for Game Pass's cloud, or
//!    `xgpuwebf2p` (free-to-play games only) for an account without it.
//! 4. A **transfer token** from the refresh token, which a cloud session
//!    asks for before it connects.
//!
//! Each is renewed from the one before when it is about to expire, and
//! kept ([`crate::store`]). Tokens are never logged.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::{form, Body, Http, HttpError};
use crate::store::{Account, AccountStore, Region, StreamingToken, XstsToken, RENEW_BEFORE_SECS};
use crate::time::{now_unix, parse_utc};

/// The Xbox app's public OAuth client, which Greenlight signs in as.
pub const CLIENT_ID: &str = "1f907974-e22b-4810-a9de-d9647380c97e";
const SCOPE: &str = "xboxlive.signin openid profile offline_access";
const LOGIN_HOST: &str = "login.microsoftonline.com";

/// Why there is no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// No account, or its sign-in was revoked or expired: sign in again.
    SignedOut,
    /// The account cannot use Xbox Live, or this offering, for a reason
    /// the user can act on.
    Refused(String),
    /// The service could not be reached or answered oddly; try again.
    Failed(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthError::SignedOut => f.write_str("Sign in with your Microsoft account again."),
            AuthError::Refused(s) | AuthError::Failed(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<HttpError> for AuthError {
    fn from(e: HttpError) -> Self {
        AuthError::Failed(e.to_string())
    }
}

/// A sign-in under way: the code the user types, and where.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    pub user_code: String,
    pub device_code: String,
    /// `https://www.microsoft.com/link`.
    pub verification_uri: String,
    pub expires_in: u64,
    #[serde(default = "five")]
    pub interval: u64,
}

fn five() -> u64 {
    5
}

/// Start signing in: the code to show.
pub fn start_sign_in(http: &Http) -> Result<DeviceCode, AuthError> {
    let body = form(&[("client_id", CLIENT_ID), ("scope", SCOPE)]);
    let answer = http.ok(
        "POST",
        LOGIN_HOST,
        "/consumers/oauth2/v2.0/devicecode",
        &[],
        Body::Form(&body),
    )?;
    Ok(answer.json("the Microsoft sign-in")?)
}

#[derive(Deserialize)]
struct MsaTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: u64,
}

/// Wait for the user to sign in with `code` (polling as often as the
/// service allows) and keep the account in `dir`. `cancel` stops waiting.
pub fn finish_sign_in(
    http: &Http,
    code: &DeviceCode,
    dir: &Path,
    cancel: &AtomicBool,
) -> Result<Account, AuthError> {
    let body = form(&[
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("client_id", CLIENT_ID),
        ("device_code", &code.device_code),
    ]);
    let deadline = Instant::now() + Duration::from_secs(code.expires_in.max(60));
    let mut interval = Duration::from_secs(code.interval.clamp(1, 30));
    loop {
        // Sleep in short steps so a cancel is quick.
        let wake = Instant::now() + interval;
        while Instant::now() < wake {
            if cancel.load(Ordering::Relaxed) {
                return Err(AuthError::Failed("Signing in was cancelled.".into()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if Instant::now() >= deadline {
            return Err(AuthError::Failed(
                "The code expired before it was entered. Start again for a new one.".into(),
            ));
        }
        let answer = http.request(
            "POST",
            LOGIN_HOST,
            "/consumers/oauth2/v2.0/token",
            &[],
            Body::Form(&body),
        )?;
        if answer.status == 200 {
            let t: MsaTokens = answer.json("the Microsoft sign-in")?;
            let store = AccountStore::new(dir);
            let mut account = Account {
                refresh_token: t.refresh_token,
                access_token: t.access_token,
                access_expires: now_unix() + t.expires_in,
                // A new sign-in keeps this installation's id.
                install_id: store.load().install_id,
                ..Account::default()
            };
            if account.install_id.is_empty() {
                account.install_id = crate::uuid_v4();
            }
            let mut auth = Auth {
                http: http.clone(),
                store,
                account,
            };
            // Prove the account can use Xbox Live now, while the user is
            // looking, and learn its gamertag.
            auth.web_token()?;
            auth.save();
            return Ok(auth.account);
        }
        let error = serde_json::from_str::<Value>(&answer.body)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        match error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += Duration::from_secs(5),
            "authorization_declined" => {
                return Err(AuthError::Refused("Signing in was declined.".into()))
            }
            "expired_token" => {
                return Err(AuthError::Failed(
                    "The code expired before it was entered. Start again for a new one.".into(),
                ))
            }
            _ => {
                return Err(AuthError::Failed(format!(
                    "Signing in failed: {}",
                    crate::http::summary(&answer.body)
                )))
            }
        }
    }
}

/// The streaming offerings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Offering {
    /// Consoles at home.
    Home,
    /// Game Pass Ultimate's cloud.
    Cloud,
    /// The cloud's free-to-play games, for any account.
    CloudFree,
}

impl Offering {
    pub fn id(self) -> &'static str {
        match self {
            Offering::Home => "xhome",
            Offering::Cloud => "xgpuweb",
            Offering::CloudFree => "xgpuwebf2p",
        }
    }

    fn from_id(id: &str) -> Option<Offering> {
        match id {
            "xhome" => Some(Offering::Home),
            "xgpuweb" => Some(Offering::Cloud),
            "xgpuwebf2p" => Some(Offering::CloudFree),
            _ => None,
        }
    }
}

/// A signed-in account, renewing its tokens as they are needed and keeping
/// them.
pub struct Auth {
    pub http: Http,
    store: AccountStore,
    pub account: Account,
}

impl Auth {
    /// The account kept in `dir`; [`AuthError::SignedOut`] if there is none.
    pub fn load(http: Http, dir: &Path) -> Result<Auth, AuthError> {
        let store = AccountStore::new(dir);
        let account = store.load();
        if !account.is_signed_in() {
            return Err(AuthError::SignedOut);
        }
        Ok(Auth {
            http,
            store,
            account,
        })
    }

    fn save(&self) {
        if let Err(e) = self.store.save(&self.account) {
            tracing::warn!("{e}");
        }
    }

    /// Forget the account.
    pub fn sign_out(dir: &Path) -> Result<(), String> {
        AccountStore::new(dir).clear()
    }

    fn access_token(&mut self) -> Result<String, AuthError> {
        if !self.account.access_token.is_empty()
            && self.account.access_expires > now_unix() + RENEW_BEFORE_SECS
        {
            return Ok(self.account.access_token.clone());
        }
        let body = form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", &self.account.refresh_token),
            ("scope", SCOPE),
        ]);
        let answer = self.http.request(
            "POST",
            LOGIN_HOST,
            "/consumers/oauth2/v2.0/token",
            &[],
            Body::Form(&body),
        )?;
        if answer.status == 400 || answer.status == 401 {
            // invalid_grant: revoked, expired, or the password changed.
            return Err(AuthError::SignedOut);
        }
        if answer.status != 200 {
            return Err(AuthError::Failed(format!(
                "Microsoft's sign-in answered {}: {}",
                answer.status,
                crate::http::summary(&answer.body)
            )));
        }
        let t: MsaTokens = answer.json("the Microsoft sign-in")?;
        self.account.access_token = t.access_token;
        self.account.access_expires = now_unix() + t.expires_in;
        if !t.refresh_token.is_empty() {
            self.account.refresh_token = t.refresh_token;
        }
        self.save();
        Ok(self.account.access_token.clone())
    }

    fn user_token(&mut self) -> Result<String, AuthError> {
        if self.account.user_token.is_fresh() {
            return Ok(self.account.user_token.token.clone());
        }
        let access = self.access_token()?;
        let body = json!({
            "Properties": {
                "AuthMethod": "RPS",
                "RpsTicket": format!("d={access}"),
                "SiteName": "user.auth.xboxlive.com"
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        });
        let answer = self.http.request(
            "POST",
            "user.auth.xboxlive.com",
            "/user/authenticate",
            &[("x-xbl-contract-version", "1")],
            Body::Json(&body),
        )?;
        let token = xsts_answer(answer.status, &answer.body)?;
        self.account.user_token = token.token;
        Ok(self.account.user_token.token.clone())
    }

    fn xsts(&mut self, relying_party: &str) -> Result<XstsAnswer, AuthError> {
        let user = self.user_token()?;
        let body = json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [user] },
            "RelyingParty": relying_party,
            "TokenType": "JWT"
        });
        let answer = self.http.request(
            "POST",
            "xsts.auth.xboxlive.com",
            "/xsts/authorize",
            &[("x-xbl-contract-version", "1")],
            Body::Json(&body),
        )?;
        xsts_answer(answer.status, &answer.body)
    }

    /// The token for Xbox Live's web APIs (consoles, profiles).
    pub fn web_token(&mut self) -> Result<XstsToken, AuthError> {
        if self.account.web_token.is_fresh() {
            return Ok(self.account.web_token.clone());
        }
        let a = self.xsts("http://xboxlive.com")?;
        if let Some(gt) = a.gamertag {
            self.account.gamertag = gt;
        }
        if let Some(xuid) = a.xuid {
            self.account.xuid = xuid;
        }
        self.account.web_token = a.token;
        self.save();
        Ok(self.account.web_token.clone())
    }

    fn gssv_token(&mut self) -> Result<String, AuthError> {
        if !self.account.gssv_token.is_fresh() {
            self.account.gssv_token = self.xsts("http://gssv.xboxlive.com/")?.token;
        }
        Ok(self.account.gssv_token.token.clone())
    }

    /// The streaming token for `offering`, from the store while it lasts.
    pub fn streaming_token(&mut self, offering: Offering) -> Result<StreamingToken, AuthError> {
        if let Some(t) = self.account.streaming.get(offering.id()) {
            if t.is_fresh() {
                return Ok(t.clone());
            }
        }
        let gssv = self.gssv_token()?;
        let host = format!("{}.gssv-play-prod.xboxlive.com", offering.id());
        let body = json!({ "token": gssv, "offeringId": offering.id() });
        let answer = self.http.request(
            "POST",
            &host,
            "/v2/login/user",
            &[("x-gssv-client", "XboxComBrowser")],
            Body::Json(&body),
        )?;
        if answer.status == 401 || answer.status == 403 {
            return Err(AuthError::Refused(match offering {
                Offering::Home => {
                    "This account cannot stream from consoles (Xbox remote play is not \
                        offered to it)."
                        .into()
                }
                _ => "Xbox Cloud Gaming is not offered to this account here.".into(),
            }));
        }
        if answer.status != 200 {
            return Err(AuthError::Failed(format!(
                "The Xbox streaming service answered {}: {}",
                answer.status,
                crate::http::summary(&answer.body)
            )));
        }
        let token = parse_streaming_token(&answer.body)?;
        self.account
            .streaming
            .insert(offering.id().to_owned(), token.clone());
        self.save();
        Ok(token)
    }

    /// The cloud offering this account has: Game Pass's, else the
    /// free-to-play one, else `None`. Asked once, then kept; `recheck`
    /// asks again (the account may have joined Game Pass since).
    pub fn cloud_offering(&mut self, recheck: bool) -> Result<Option<Offering>, AuthError> {
        if !recheck && !self.account.cloud_offering.is_empty() {
            return Ok(Offering::from_id(&self.account.cloud_offering));
        }
        let mut found = None;
        for offering in [Offering::Cloud, Offering::CloudFree] {
            match self.streaming_token(offering) {
                Ok(_) => {
                    found = Some(offering);
                    break;
                }
                Err(AuthError::Refused(_)) => {}
                Err(e) => return Err(e),
            }
        }
        self.account.cloud_offering = found.map_or("none", Offering::id).to_owned();
        self.save();
        Ok(found)
    }

    /// The token a cloud session asks for before it connects (Greenlight's
    /// "MSAL auth", `streammanager.ts`): the console transfer token, from
    /// the refresh token at login.live.com.
    pub fn transfer_token(&mut self) -> Result<String, AuthError> {
        let body = form(&[
            ("client_id", CLIENT_ID),
            (
                "scope",
                "service::http://Passport.NET/purpose::PURPOSE_XBOX_CLOUD_CONSOLE_TRANSFER_TOKEN",
            ),
            ("grant_type", "refresh_token"),
            ("refresh_token", &self.account.refresh_token),
        ]);
        let answer = self.http.request(
            "POST",
            "login.live.com",
            "/oauth20_token.srf",
            &[],
            Body::Form(&body),
        )?;
        if answer.status == 400 || answer.status == 401 {
            return Err(AuthError::SignedOut);
        }
        if answer.status != 200 {
            return Err(AuthError::Failed(format!(
                "Microsoft's sign-in answered {}: {}",
                answer.status,
                crate::http::summary(&answer.body)
            )));
        }
        let t: MsaTokens = answer.json("the console transfer token")?;
        Ok(t.access_token)
    }
}

struct XstsAnswer {
    token: XstsToken,
    gamertag: Option<String>,
    xuid: Option<String>,
}

/// Read an Xbox Live token answer, or say why the account was refused.
fn xsts_answer(status: u16, body: &str) -> Result<XstsAnswer, AuthError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Answer {
        token: String,
        not_after: String,
        display_claims: Claims,
    }
    #[derive(Deserialize)]
    struct Claims {
        xui: Vec<Xui>,
    }
    #[derive(Deserialize)]
    struct Xui {
        uhs: String,
        #[serde(default)]
        gtg: Option<String>,
        #[serde(default)]
        xid: Option<String>,
    }
    if status == 401 {
        let code = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|v| v.get("XErr").and_then(Value::as_u64));
        return Err(match code {
            Some(c) => AuthError::Refused(xerr_message(c)),
            None => AuthError::SignedOut,
        });
    }
    if status != 200 {
        return Err(AuthError::Failed(format!(
            "Xbox Live answered {status}: {}",
            crate::http::summary(body)
        )));
    }
    let a: Answer = serde_json::from_str(body)
        .map_err(|e| AuthError::Failed(format!("Xbox Live's answer could not be read: {e}")))?;
    let xui = a
        .display_claims
        .xui
        .into_iter()
        .next()
        .ok_or_else(|| AuthError::Failed("Xbox Live's answer had no user in it.".into()))?;
    Ok(XstsAnswer {
        token: XstsToken {
            token: a.token,
            uhs: xui.uhs,
            expires: parse_utc(&a.not_after).unwrap_or(0),
        },
        gamertag: xui.gtg,
        xuid: xui.xid,
    })
}

/// What Xbox Live's refusal codes mean for the person signing in.
fn xerr_message(code: u64) -> String {
    match code {
        2148916233 => "This Microsoft account has no Xbox profile yet. Sign in once at \
                       xbox.com to make one, then try again."
            .into(),
        2148916235 => "Xbox Live is not available in this account's country.".into(),
        2148916236 | 2148916237 => {
            "This account needs adult verification at xbox.com first.".into()
        }
        2148916238 => "This is a child's account: an adult must add it to a Microsoft \
                       family before it can use Xbox Live."
            .into(),
        2148916227 => "Xbox Live has banned this account.".into(),
        other => format!("Xbox Live refused this account (error {other})."),
    }
}

/// Read the streaming service's login answer.
pub fn parse_streaming_token(body: &str) -> Result<StreamingToken, AuthError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Answer {
        gs_token: String,
        #[serde(default)]
        market: String,
        duration_in_seconds: u64,
        offering_settings: Settings,
    }
    #[derive(Deserialize)]
    struct Settings {
        regions: Vec<R>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct R {
        name: String,
        base_uri: String,
        #[serde(default)]
        is_default: bool,
    }
    let a: Answer = serde_json::from_str(body).map_err(|e| {
        AuthError::Failed(format!(
            "The streaming service's answer could not be read: {e}"
        ))
    })?;
    if a.offering_settings.regions.is_empty() {
        return Err(AuthError::Failed(
            "The streaming service offered no region.".into(),
        ));
    }
    Ok(StreamingToken {
        token: a.gs_token,
        regions: a
            .offering_settings
            .regions
            .into_iter()
            .map(|r| Region {
                name: r.name,
                base_uri: r.base_uri.trim_end_matches('/').to_owned(),
                is_default: r.is_default,
            })
            .collect(),
        market: a.market,
        expires: now_unix() + a.duration_in_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_xsts_answer_gives_the_token_and_the_gamertag() {
        let body = r#"{"IssueInstant":"2026-10-06T19:00:00.0000000Z","NotAfter":"2026-10-07T11:00:00.0000000Z",
            "Token":"eyJ","DisplayClaims":{"xui":[{"gtg":"Player One","xid":"2533274800000000","uhs":"123"}]}}"#;
        let a = xsts_answer(200, body).ok().unwrap();
        assert_eq!(a.token.token, "eyJ");
        assert_eq!(a.token.uhs, "123");
        assert_eq!(a.token.expires, parse_utc("2026-10-07T11:00:00Z").unwrap());
        assert_eq!(a.gamertag.as_deref(), Some("Player One"));
    }

    #[test]
    fn xbox_live_refusals_say_what_to_do() {
        let e = xsts_answer(401, r#"{"Identity":"0","XErr":2148916233,"Message":""}"#)
            .err()
            .unwrap();
        assert!(matches!(&e, AuthError::Refused(m) if m.contains("no Xbox profile")));
        assert_eq!(xsts_answer(401, "").err(), Some(AuthError::SignedOut));
        assert!(matches!(
            xsts_answer(500, "oops").err(),
            Some(AuthError::Failed(_))
        ));
    }

    #[test]
    fn a_streaming_login_gives_regions() {
        let body = r#"{"offeringSettings":{"allowRegionSelection":false,"regions":[
            {"name":"WestEurope","baseUri":"https://weu.core.gssv-play-prod.xboxlive.com/","networkTestHostname":"x","isDefault":true,"systemUpdateGroups":null,"fallbackPriority":-1},
            {"name":"UKSouth","baseUri":"https://uks.core.gssv-play-prod.xboxlive.com","isDefault":false}],
            "selectableServerTypes":null,"clientCloudSettings":{"Environments":[]}},
            "market":"RO","gsToken":"gs","tokenType":"bearer","durationInSeconds":14400}"#;
        let t = parse_streaming_token(body).unwrap();
        assert_eq!(t.token, "gs");
        assert_eq!(
            t.default_region().unwrap().base_uri,
            "https://weu.core.gssv-play-prod.xboxlive.com"
        );
        assert!(t.is_fresh());
        assert!(parse_streaming_token("{}").is_err());
    }

    #[test]
    fn offerings_have_the_services_names() {
        for o in [Offering::Home, Offering::Cloud, Offering::CloudFree] {
            assert_eq!(Offering::from_id(o.id()), Some(o));
        }
        assert_eq!(Offering::from_id("none"), None);
    }
}
