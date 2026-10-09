//! The streaming service ("gssv"): a region's web API that starts a session
//! on a console or a cloud server, carries the WebRTC offer and answer and
//! the ICE candidates between the two ends, and keeps the session alive.
//! Greenlight's `xcloudapi.ts` and `streammanager.ts`:
//!
//! ```text
//! POST /v5/sessions/{home|cloud}/play           → sessionPath
//! GET  …/{id}/state   until Provisioned          (WaitingForResources: queued;
//!                                                 ReadyToConnect: POST …/connect)
//! POST …/{id}/sdp  then GET …/{id}/sdp           → the answer (204: not yet)
//! POST …/{id}/ice  then GET …/{id}/ice           → the console's candidates
//! POST …/{id}/keepalive  every 30 s;  DELETE …/{id} to end
//! ```
//!
//! The cloud's catalogue is here too: the titles this account may play,
//! named by the Game Pass catalogue.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::http::{Body, Http, HttpError};
use crate::ice::{parse_exchange, IceCandidate};
use crate::store::StreamingToken;

/// How often a session is kept alive (Greenlight's interval).
pub const KEEPALIVE_EVERY: Duration = Duration::from_secs(30);
/// How often a session's state is asked while it is being set up.
pub const STATE_POLL_EVERY: Duration = Duration::from_secs(1);
/// How long an answer that is not there yet (204) is waited for between
/// asks (Greenlight's 750 ms).
const NOT_YET_RETRY: Duration = Duration::from_millis(750);
/// How long the console has to answer the offer.
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// A console at home, or a server in the cloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Home,
    Cloud,
}

impl Kind {
    fn path(self) -> &'static str {
        match self {
            Kind::Home => "home",
            Kind::Cloud => "cloud",
        }
    }
}

/// What the client tells the service about itself and the picture it
/// wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaySettings {
    pub width: u32,
    pub height: u32,
    /// The language games should use (`en-US`).
    pub locale: String,
    pub timezone_offset_minutes: i32,
}

impl Default for PlaySettings {
    fn default() -> Self {
        PlaySettings {
            width: 1920,
            height: 1080,
            locale: "en-US".into(),
            timezone_offset_minutes: 0,
        }
    }
}

/// Where a session stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Provisioning,
    /// In the queue for a cloud server.
    WaitingForResources,
    /// The service wants the transfer token before it connects.
    ReadyToConnect,
    /// Ready: negotiate the connection.
    Provisioned,
    Failed {
        code: String,
        message: String,
    },
    Other(String),
}

/// A failure from the streaming service, with what the person can do about
/// it where that is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GssvError(pub String);

impl std::fmt::Display for GssvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GssvError {}

impl From<HttpError> for GssvError {
    fn from(e: HttpError) -> Self {
        GssvError(e.to_string())
    }
}

/// One region of the streaming service, for one offering.
#[derive(Clone)]
pub struct Service {
    http: Http,
    host: String,
    token: String,
    kind: Kind,
}

impl Service {
    /// The service in `region` (the account's default when `None` or
    /// unknown).
    pub fn new(
        http: Http,
        token: &StreamingToken,
        region: Option<&str>,
        kind: Kind,
    ) -> Result<Service, GssvError> {
        let region = region
            .and_then(|r| token.region(r))
            .or(token.default_region())
            .ok_or_else(|| GssvError("The streaming service offered no region.".into()))?;
        let host = region
            .base_uri
            .split_once("://")
            .map_or(region.base_uri.as_str(), |(_, h)| h)
            .trim_end_matches('/')
            .to_owned();
        Ok(Service {
            http,
            host,
            token: token.token.clone(),
            kind,
        })
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// The same service, giving up on a request after a few seconds (for
    /// ending a session and keeping it alive).
    pub fn quick(&self) -> Service {
        Service {
            http: self.http.quick(),
            ..self.clone()
        }
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Body<'_>,
    ) -> Result<crate::http::Answer, GssvError> {
        let bearer = format!("Bearer {}", self.token);
        Ok(self.http.ok(
            method,
            &self.host,
            path,
            &[("Authorization", &bearer)],
            body,
        )?)
    }

    fn session(&self, id: &str, rest: &str) -> String {
        format!("/v5/sessions/{}/{id}{rest}", self.kind.path())
    }

    /// Start a session on `target`: a console's id at home, a title's id in
    /// the cloud. Returns the session's id.
    pub fn play(&self, target: &str, settings: &PlaySettings) -> Result<String, GssvError> {
        let (title, server) = match self.kind {
            Kind::Home => ("", target),
            Kind::Cloud => (target, ""),
        };
        let body = json!({
            "titleId": title,
            "systemUpdateGroup": "",
            "clientSessionId": "",
            "settings": {
                "nanoVersion": "V3;WebrtcTransport.dll",
                "enableTextToSpeech": false,
                "highContrast": 0,
                "locale": settings.locale,
                "useIceConnection": false,
                "timezoneOffsetMinutes": settings.timezone_offset_minutes,
                "sdkType": "web",
                "osName": "windows"
            },
            "serverId": server,
            "fallbackRegionNames": []
        });
        let device = device_info(settings);
        let bearer = format!("Bearer {}", self.token);
        let path = format!("/v5/sessions/{}/play", self.kind.path());
        let answer = self.http.request(
            "POST",
            &self.host,
            &path,
            &[("Authorization", &bearer), ("X-MS-Device-Info", &device)],
            Body::Json(&body),
        )?;
        if !(200..300).contains(&answer.status) {
            return Err(play_error(answer.status, &answer.body));
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Play {
            session_path: String,
        }
        let p: Play = answer.json("the streaming service")?;
        session_id(&p.session_path)
            .ok_or_else(|| GssvError("The streaming service started no session.".into()))
    }

    pub fn state(&self, id: &str) -> Result<SessionState, GssvError> {
        let a = self.request("GET", &self.session(id, "/state"), Body::None)?;
        parse_state(&a.body)
    }

    /// Give the session the transfer token (`ReadyToConnect`).
    pub fn connect(&self, id: &str, transfer_token: &str) -> Result<(), GssvError> {
        let body = json!({ "userToken": transfer_token });
        self.request("POST", &self.session(id, "/connect"), Body::Json(&body))?;
        Ok(())
    }

    pub fn keepalive(&self, id: &str) -> Result<(), GssvError> {
        self.request("POST", &self.session(id, "/keepalive"), Body::None)?;
        Ok(())
    }

    /// End the session.
    pub fn stop(&self, id: &str) -> Result<(), GssvError> {
        self.request("DELETE", &self.session(id, ""), Body::None)?;
        Ok(())
    }

    /// Send the offer; the console's answer. `stop` gives up waiting.
    pub fn exchange_sdp(
        &self,
        id: &str,
        offer: &str,
        stop: &AtomicBool,
    ) -> Result<String, GssvError> {
        let body = json!({
            "messageType": "offer",
            "sdp": offer,
            "configuration": crate::messages::channel_versions(),
        });
        self.request("POST", &self.session(id, "/sdp"), Body::Json(&body))?;
        let exchange = self.wait_exchange(&self.session(id, "/sdp"), stop)?;
        let v: Value = serde_json::from_str(&exchange)
            .map_err(|_| GssvError("The console's answer could not be read.".into()))?;
        match v.get("sdp").and_then(Value::as_str) {
            Some(sdp) => Ok(sdp.to_owned()),
            None => Err(GssvError(format!(
                "The console turned the connection down ({}).",
                v.get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
            ))),
        }
    }

    /// Send our candidates; the console's. `stop` gives up waiting.
    pub fn exchange_ice(
        &self,
        id: &str,
        ours: &[IceCandidate],
        stop: &AtomicBool,
    ) -> Result<Vec<IceCandidate>, GssvError> {
        let body = json!({ "messageType": "iceCandidate", "candidate": ours });
        self.request("POST", &self.session(id, "/ice"), Body::Json(&body))?;
        let exchange = self.wait_exchange(&self.session(id, "/ice"), stop)?;
        parse_exchange(&exchange).map_err(GssvError)
    }

    /// GET `path` until the console has answered (204 means not yet): its
    /// `exchangeResponse`.
    fn wait_exchange(&self, path: &str, stop: &AtomicBool) -> Result<String, GssvError> {
        let bearer = format!("Bearer {}", self.token);
        let deadline = Instant::now() + EXCHANGE_TIMEOUT;
        loop {
            let a = self.http.request(
                "GET",
                &self.host,
                path,
                &[("Authorization", &bearer)],
                Body::None,
            )?;
            match a.status {
                204 if Instant::now() < deadline => {
                    if !pause(stop, NOT_YET_RETRY) {
                        return Err(GssvError("Stopped.".into()));
                    }
                }
                204 => return Err(GssvError("The console did not answer in time.".into())),
                200..=299 => {
                    #[derive(Deserialize)]
                    #[serde(rename_all = "camelCase")]
                    struct Exchange {
                        exchange_response: String,
                    }
                    let e: Exchange = a.json("the console's answer")?;
                    return Ok(e.exchange_response);
                }
                s => {
                    return Err(GssvError(format!(
                        "The streaming service answered {s}: {}",
                        crate::http::summary(&a.body)
                    )))
                }
            }
        }
    }

    /// The queue's estimated wait for `title`, in seconds.
    pub fn wait_time(&self, title: &str) -> Result<Option<u64>, GssvError> {
        let a = self.request("GET", &format!("/v1/waittime/{title}"), Body::None)?;
        let v: Value = serde_json::from_str(&a.body).unwrap_or(Value::Null);
        Ok(v.get("estimatedTotalWaitTimeInSeconds")
            .and_then(Value::as_u64))
    }

    /// The cloud titles this account may play (entitled, or free).
    pub fn titles(&self) -> Result<Vec<CloudTitle>, GssvError> {
        let a = self.request("GET", "/v2/titles", Body::None)?;
        parse_titles(&a.body)
    }

    /// The account's recently played cloud titles, newest first.
    pub fn recent_titles(&self) -> Result<Vec<String>, GssvError> {
        let a = self.request("GET", "/v2/titles/mru?mr=25", Body::None)?;
        Ok(parse_titles(&a.body)?
            .into_iter()
            .map(|t| t.title_id)
            .collect())
    }
}

/// Wait `d`, or less if `stop` is set; whether it was not.
pub fn pause(stop: &AtomicBool, d: Duration) -> bool {
    let until = Instant::now() + d;
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
    !stop.load(Ordering::Relaxed)
}

/// The `X-MS-Device-Info` the web client sends, with this client's size:
/// the service picks the stream's resolution from it (Greenlight sends a
/// fixed 1920×1080).
fn device_info(s: &PlaySettings) -> String {
    json!({
        "appInfo": {
            "env": {
                "clientAppId": "Microsoft.GamingApp",
                "clientAppType": "native",
                "clientAppVersion": "2203.1001.4.0",
                "clientSdkVersion": "8.5.2",
                "httpEnvironment": "prod",
                "sdkInstallId": ""
            }
        },
        "dev": {
            "hw": { "make": "Microsoft", "model": "Surface Pro", "sdktype": "native" },
            "os": { "name": "Windows 11", "ver": "22631.2715", "platform": "desktop" },
            "displayInfo": {
                "dimensions": { "widthInPixels": s.width, "heightInPixels": s.height },
                "pixelDensity": { "dpiX": 1, "dpiY": 1 }
            }
        }
    })
    .to_string()
}

/// The session's id: the last part of its path (`v5/sessions/home/ID`).
fn session_id(path: &str) -> Option<String> {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn play_error(status: u16, body: &str) -> GssvError {
    let summary = crate::http::summary(body);
    GssvError(match status {
        401 | 403 => "The streaming service refused this account; sign in again.".into(),
        404 => "The console was not found. Is it still linked to this account?".into(),
        409 => "This console is already streaming to another device.".into(),
        _ => format!("The session could not start ({status}: {summary})."),
    })
}

/// Read a session's state.
pub fn parse_state(body: &str) -> Result<SessionState, GssvError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct State {
        state: String,
        #[serde(default)]
        error_details: Option<Details>,
    }
    #[derive(Deserialize)]
    struct Details {
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        message: Option<String>,
    }
    let s: State = serde_json::from_str(body)
        .map_err(|_| GssvError("The session's state could not be read.".into()))?;
    Ok(match s.state.as_str() {
        "Provisioning" => SessionState::Provisioning,
        "WaitingForResources" => SessionState::WaitingForResources,
        "ReadyToConnect" => SessionState::ReadyToConnect,
        "Provisioned" => SessionState::Provisioned,
        "Failed" => {
            let d = s.error_details;
            SessionState::Failed {
                code: d.as_ref().and_then(|d| d.code.clone()).unwrap_or_default(),
                message: d.and_then(|d| d.message).unwrap_or_default(),
            }
        }
        other => SessionState::Other(other.to_owned()),
    })
}

/// What a failed session's error means for the person streaming.
pub fn failure_message(code: &str, message: &str) -> String {
    if message.contains("WaitingForServerToRegister") {
        return "The console is not connected to Xbox Live. Is it on (or asleep) and online?"
            .into();
    }
    match code {
        "NoEntitlement" => "This account cannot play that game in the cloud.".into(),
        "" => format!("The session failed: {message}"),
        _ if message.is_empty() => format!("The session failed ({code})."),
        _ => format!("The session failed ({code}): {message}"),
    }
}

/// A title in the cloud catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudTitle {
    /// What a cloud session is started with.
    pub title_id: String,
    /// The Store's id, for its name and art.
    pub product_id: String,
    /// The account may play it (Game Pass, owned, or free).
    pub playable: bool,
}

pub fn parse_titles(body: &str) -> Result<Vec<CloudTitle>, GssvError> {
    #[derive(Deserialize)]
    struct List {
        #[serde(default)]
        results: Vec<Item>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Item {
        #[serde(default)]
        title_id: Option<String>,
        #[serde(default)]
        details: Option<Details>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Details {
        #[serde(default)]
        product_id: Option<String>,
        #[serde(default)]
        has_entitlement: bool,
        #[serde(default)]
        is_free_in_store: bool,
    }
    let list: List = serde_json::from_str(body)
        .map_err(|_| GssvError("The cloud catalogue could not be read.".into()))?;
    Ok(list
        .results
        .into_iter()
        .filter_map(|i| {
            let details = i.details;
            Some(CloudTitle {
                title_id: i.title_id?,
                product_id: details
                    .as_ref()
                    .and_then(|d| d.product_id.clone())
                    .unwrap_or_default(),
                playable: details.is_some_and(|d| d.has_entitlement || d.is_free_in_store),
            })
        })
        .collect())
}

/// A title's name and art, from the Game Pass catalogue.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Product {
    pub name: String,
    pub publisher: String,
    /// A square tile, when the catalogue has one.
    pub image: Option<String>,
}

/// Names for Store product ids, in batches of a hundred as the catalogue
/// takes them (Greenlight's `titlemanager.ts`). The catalogue is public:
/// no token.
pub fn products(
    http: &Http,
    market: &str,
    ids: &[String],
) -> Result<HashMap<String, Product>, GssvError> {
    let market = if market.is_empty() { "US" } else { market };
    let mut out = HashMap::new();
    for batch in ids.chunks(100) {
        let body = json!({ "Products": batch });
        let a = http.ok(
            "POST",
            "catalog.gamepass.com",
            &format!("/v3/products?market={market}&language=en-US&hydration=RemoteHighSapphire0"),
            &[
                ("ms-cv", "0"),
                ("calling-app-name", "Xbox Cloud Gaming Web"),
                ("calling-app-version", "21.0.0"),
            ],
            Body::Json(&body),
        )?;
        out.extend(parse_products(&a.body)?);
    }
    Ok(out)
}

pub fn parse_products(body: &str) -> Result<HashMap<String, Product>, GssvError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Answer {
        #[serde(default)]
        products: HashMap<String, P>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct P {
        #[serde(default)]
        product_title: String,
        #[serde(default)]
        publisher_name: String,
        #[serde(rename = "Image_Tile", default)]
        image_tile: Option<Image>,
    }
    #[derive(Deserialize)]
    struct Image {
        #[serde(rename = "URL", default)]
        url: Option<String>,
    }
    let a: Answer = serde_json::from_str(body)
        .map_err(|_| GssvError("The Game Pass catalogue could not be read.".into()))?;
    Ok(a.products
        .into_iter()
        .map(|(id, p)| {
            let image = p.image_tile.and_then(|i| i.url).map(|u| {
                if u.starts_with("//") {
                    format!("https:{u}")
                } else {
                    u
                }
            });
            (
                id,
                Product {
                    name: p.product_title,
                    publisher: p.publisher_name,
                    image,
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Region;

    #[test]
    fn the_session_id_is_the_last_part_of_its_path() {
        assert_eq!(
            session_id("v5/sessions/home/ABC-123").as_deref(),
            Some("ABC-123")
        );
        assert_eq!(session_id("/v5/sessions/cloud/X/").as_deref(), Some("X"));
        assert_eq!(session_id(""), None);
    }

    #[test]
    fn the_region_host_is_taken_without_its_scheme() {
        let token = StreamingToken {
            token: "t".into(),
            regions: vec![
                Region {
                    name: "UKSouth".into(),
                    base_uri: "https://uks.core.gssv-play-prod.xboxlive.com".into(),
                    is_default: false,
                },
                Region {
                    name: "WestEurope".into(),
                    base_uri: "https://weu.core.gssv-play-prod.xboxlive.com".into(),
                    is_default: true,
                },
            ],
            ..Default::default()
        };
        let s = Service::new(Http::with_mock(None), &token, None, Kind::Cloud).unwrap();
        assert_eq!(s.host, "weu.core.gssv-play-prod.xboxlive.com");
        let s = Service::new(Http::with_mock(None), &token, Some("uksouth"), Kind::Cloud).unwrap();
        assert_eq!(s.host, "uks.core.gssv-play-prod.xboxlive.com");
        assert_eq!(s.session("ID", "/sdp"), "/v5/sessions/cloud/ID/sdp");
    }

    #[test]
    fn states_and_failures_read_as_the_service_writes_them() {
        assert_eq!(
            parse_state(r#"{"state":"Provisioned","platform":"xhome"}"#).unwrap(),
            SessionState::Provisioned
        );
        let failed = parse_state(
            r#"{"state":"Failed","errorDetails":{"code":"WNSError","message":"WaitingForServerToRegister"}}"#,
        )
        .unwrap();
        let SessionState::Failed { code, message } = failed else {
            panic!("a failure");
        };
        assert!(failure_message(&code, &message).contains("not connected to Xbox Live"));
        assert!(parse_state("<html>").is_err());
    }

    #[test]
    fn the_catalogue_says_what_may_be_played() {
        let body = r#"{"results":[
            {"titleId":"FORTNITE","details":{"productId":"BT5P2X999VH2","hasEntitlement":false,"isFreeInStore":true}},
            {"titleId":"HALO","details":{"productId":"9NP1P1WFS0LB","hasEntitlement":true}},
            {"titleId":"LOCKED","details":{"productId":"P3"}},
            {"details":{"productId":"NOID"}}]}"#;
        let t = parse_titles(body).unwrap();
        assert_eq!(t.len(), 3);
        assert!(t[0].playable && t[1].playable && !t[2].playable);
        assert_eq!(t[1].product_id, "9NP1P1WFS0LB");
        let products = parse_products(
            r#"{"Products":{"BT5P2X999VH2":{"ProductTitle":"Fortnite","PublisherName":"Epic Games",
                "Image_Tile":{"URL":"//store-images.s-microsoft.com/x.png"}}}}"#,
        )
        .unwrap();
        let p = &products["BT5P2X999VH2"];
        assert_eq!(p.name, "Fortnite");
        assert_eq!(
            p.image.as_deref(),
            Some("https://store-images.s-microsoft.com/x.png")
        );
    }

    #[test]
    fn the_device_info_carries_the_clients_size() {
        let d: Value = serde_json::from_str(&device_info(&PlaySettings {
            width: 2560,
            height: 1440,
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(d["dev"]["displayInfo"]["dimensions"]["widthInPixels"], 2560);
    }
}
