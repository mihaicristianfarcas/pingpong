//! The mock's web APIs: Microsoft's sign-in, Xbox Live's tokens, the
//! console list and commands, and the streaming service, at the same paths
//! and with the same JSON as the real ones (see `pingpong_xbox`'s modules
//! for where each shape comes from). Requests arrive as
//! `/HOST/PATH` (`pingpong_xbox::http`).
//!
//! Tokens are fixed strings, checked where the real service checks them,
//! so a client that sends the wrong token to the wrong place fails here
//! as it would there.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;
use pingpong_xbox::http::parse_form;
use serde_json::{json, Value};

use crate::http::{Request, Response};
use crate::peer::Peer;
use crate::{Cloud, Config, Record};

pub const CONSOLE_ID: &str = "F4000000MOCK";
pub const CONSOLE_NAME: &str = "Mock Xbox";
pub const CLOUD_TITLE: &str = "MOCKGAME";
const CLOUD_PRODUCT: &str = "9MOCKPRODUCT";

const ACCESS: &str = "mock-msa-access";
const REFRESH: &str = "mock-msa-refresh";
const USER_TOKEN: &str = "mock-user-token";
const WEB_TOKEN: &str = "mock-web-token";
const GSSV_TOKEN: &str = "mock-gssv-token";
const UHS: &str = "1234567890";
const HOME_TOKEN: &str = "mock-gs-home";
const CLOUD_TOKEN: &str = "mock-gs-cloud";
/// The streaming service's hosts, as the login answer names them.
const HOME_HOST: &str = "home.mock.invalid";
const CLOUD_HOST: &str = "cloud.mock.invalid";

struct Session {
    kind: String,
    polls: u32,
    connected: bool,
    peer: Option<Peer>,
    /// The SDP answer's GET has said "not yet" once.
    sdp_asked: bool,
}

pub struct Service {
    config: Config,
    pub record: Arc<Mutex<Record>>,
    pub view: Arc<Mutex<crate::picture::InputView>>,
    device_polls: u32,
    awake: bool,
    sessions: HashMap<String, Session>,
    next_session: u32,
}

impl Service {
    pub fn new(config: Config, record: Arc<Mutex<Record>>) -> Service {
        Service {
            awake: !config.console_asleep,
            config,
            record,
            view: Arc::default(),
            device_polls: 0,
            sessions: HashMap::new(),
            next_session: 1,
        }
    }

    pub fn set_link(&mut self, link: crate::Link) {
        self.config.link = link;
    }

    /// The peers of every live session (for the controls).
    pub fn peers(&self) -> impl Iterator<Item = &Peer> {
        self.sessions.values().filter_map(|s| s.peer.as_ref())
    }

    pub fn handle(&mut self, r: &Request) -> Response {
        let (host, path) = split(&r.path);
        let path_only = path.split('?').next().unwrap_or(path);
        tracing::debug!(method = %r.method, %host, path = %path_only, "mock request");
        match (r.method.as_str(), host, path_only) {
            ("POST", "login.microsoftonline.com", "/consumers/oauth2/v2.0/devicecode") => {
                Response::json(json!({
                    "user_code": "MOCK1234",
                    "device_code": "mock-device-code",
                    "verification_uri": "https://www.microsoft.com/link",
                    "expires_in": 900,
                    "interval": 1,
                    "message": "Enter MOCK1234 at https://www.microsoft.com/link"
                }))
            }
            ("POST", "login.microsoftonline.com", "/consumers/oauth2/v2.0/token") => {
                self.token(&parse_form(&r.text()))
            }
            ("POST", "login.live.com", "/oauth20_token.srf") => {
                let form = parse_form(&r.text());
                if field(&form, "refresh_token") != Some(REFRESH) {
                    return Response::status(400, r#"{"error":"invalid_grant"}"#);
                }
                Response::json(json!({ "access_token": "mock-transfer-token", "expires_in": 3600 }))
            }
            ("POST", "user.auth.xboxlive.com", "/user/authenticate") => {
                let ticket = r.json()["Properties"]["RpsTicket"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned();
                if ticket != format!("d={ACCESS}") {
                    return Response::status(401, "");
                }
                xsts(USER_TOKEN, false)
            }
            ("POST", "xsts.auth.xboxlive.com", "/xsts/authorize") => {
                let body = r.json();
                if body["Properties"]["UserTokens"][0] != USER_TOKEN {
                    return Response::status(401, r#"{"XErr":2148916233}"#);
                }
                match body["RelyingParty"].as_str() {
                    Some("http://xboxlive.com") => xsts(WEB_TOKEN, true),
                    Some("http://gssv.xboxlive.com/") => xsts(GSSV_TOKEN, false),
                    _ => Response::status(400, ""),
                }
            }
            ("POST", login, "/v2/login/user")
                if login.ends_with(".gssv-play-prod.xboxlive.com") =>
            {
                if r.json()["token"] != GSSV_TOKEN {
                    return Response::status(401, "");
                }
                self.streaming_login(login.split('.').next().unwrap_or(""))
            }
            ("GET", "xccs.xboxlive.com", "/lists/devices") => {
                if !self.web_authorized(r) {
                    return Response::status(401, "");
                }
                Response::json(json!({
                    "status": { "errorCode": "OK", "errorMessage": null },
                    "result": [{
                        "id": CONSOLE_ID,
                        "name": CONSOLE_NAME,
                        "locale": "en-US",
                        "region": "",
                        "consoleType": "XboxSeriesX",
                        "powerState": if self.awake { "On" } else { "ConnectedStandby" },
                        "digitalAssistantRemoteControlEnabled": false,
                        "remoteManagementEnabled": true,
                        "consoleStreamingEnabled": true,
                        "wirelessWarning": false,
                        "outOfHomeWarning": false,
                        "storageDevices": []
                    }],
                    "agentUserId": null
                }))
            }
            ("POST", "xccs.xboxlive.com", "/commands") => {
                if !self.web_authorized(r) {
                    return Response::status(401, "");
                }
                let body = r.json();
                match (body["linkedXboxId"].as_str(), body["command"].as_str()) {
                    (Some(CONSOLE_ID), Some("WakeUp")) => {
                        self.awake = true;
                        self.record.lock().woken += 1;
                    }
                    (Some(CONSOLE_ID), Some("TurnOff")) => self.awake = false,
                    (Some(CONSOLE_ID), _) => {}
                    _ => {
                        return Response::json(
                            json!({ "status": { "errorCode": "NotFound", "errorMessage": "No such console" } }),
                        )
                    }
                }
                Response::json(json!({ "status": { "errorCode": "OK", "errorMessage": null } }))
            }
            ("GET", "peoplehub.xboxlive.com", people)
                if people.starts_with("/users/me/people/social") =>
            {
                if !self.web_authorized(r) {
                    return Response::status(401, "");
                }
                Response::json(json!({ "people": [
                    { "xuid": "2533274800000002", "gamertag": "Mock Friend", "displayName": "",
                      "presenceState": "Online", "presenceText": "Online",
                      "presenceDetails": [{ "IsGame": true, "IsPrimary": true, "PresenceText": "Mock Game", "TitleId": "1" }] },
                    { "xuid": "2533274800000003", "gamertag": "Away Friend", "displayName": "",
                      "presenceState": "Offline", "presenceText": "Last seen 1h ago: Mock Xbox", "presenceDetails": [] }
                ]}))
            }
            ("POST", "catalog.gamepass.com", "/v3/products") => Response::json(json!({
                "Products": {
                    CLOUD_PRODUCT: { "ProductTitle": "Mock Game", "PublisherName": "pingpong", "StoreId": CLOUD_PRODUCT }
                }
            })),
            (method, HOME_HOST | CLOUD_HOST, path) => {
                let expected = if host == HOME_HOST {
                    HOME_TOKEN
                } else {
                    CLOUD_TOKEN
                };
                if r.header("Authorization") != Some(&format!("Bearer {expected}")) {
                    return Response::status(401, "");
                }
                self.streaming(method, path, r)
            }
            _ => Response::status(
                404,
                format!("{{\"message\":\"no mock for {host}{path_only}\"}}"),
            ),
        }
    }

    fn web_authorized(&self, r: &Request) -> bool {
        r.header("Authorization") == Some(&format!("XBL3.0 x={UHS};{WEB_TOKEN}"))
    }

    fn token(&mut self, form: &[(String, String)]) -> Response {
        match field(form, "grant_type") {
            Some("urn:ietf:params:oauth:grant-type:device_code") => {
                if field(form, "device_code") != Some("mock-device-code") {
                    return Response::status(400, r#"{"error":"bad_verification_code"}"#);
                }
                self.device_polls += 1;
                if self.device_polls <= self.config.pending_polls {
                    return Response::status(400, r#"{"error":"authorization_pending"}"#);
                }
                self.device_polls = 0;
                self.record.lock().sign_ins += 1;
            }
            Some("refresh_token") => {
                if field(form, "refresh_token") != Some(REFRESH) {
                    return Response::status(400, r#"{"error":"invalid_grant"}"#);
                }
                self.record.lock().refreshes += 1;
            }
            _ => return Response::status(400, r#"{"error":"unsupported_grant_type"}"#),
        }
        Response::json(json!({
            "token_type": "Bearer",
            "scope": "XboxLive.signin openid profile offline_access",
            "expires_in": 3600,
            "access_token": ACCESS,
            "refresh_token": REFRESH
        }))
    }

    fn streaming_login(&self, offering: &str) -> Response {
        let (token, host) = match (offering, self.config.cloud) {
            ("xhome", _) => (HOME_TOKEN, HOME_HOST),
            ("xgpuweb", Cloud::GamePass) | ("xgpuwebf2p", Cloud::FreeToPlay) => {
                (CLOUD_TOKEN, CLOUD_HOST)
            }
            _ => return Response::status(403, r#"{"code":"NotEntitled"}"#),
        };
        Response::json(json!({
            "offeringSettings": {
                "allowRegionSelection": false,
                "regions": [{
                    "name": "MockRegion",
                    "baseUri": format!("https://{host}/"),
                    "networkTestHostname": host,
                    "isDefault": true,
                    "systemUpdateGroups": null,
                    "fallbackPriority": -1
                }],
                "selectableServerTypes": null,
                "clientCloudSettings": { "Environments": [] }
            },
            "market": "US",
            "gsToken": token,
            "tokenType": "bearer",
            "durationInSeconds": 14400
        }))
    }

    fn streaming(&mut self, method: &str, path: &str, r: &Request) -> Response {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match (method, parts.as_slice()) {
            ("POST", ["v5", "sessions", kind, "play"]) => {
                let body = r.json();
                let target = match *kind {
                    "home" => body["serverId"].as_str(),
                    _ => body["titleId"].as_str(),
                }
                .unwrap_or("")
                .to_owned();
                let known = match *kind {
                    "home" => target == CONSOLE_ID,
                    _ => target == CLOUD_TITLE,
                };
                if !known {
                    return Response::status(404, r#"{"message":"not found"}"#);
                }
                let device: Value = r
                    .header("X-MS-Device-Info")
                    .and_then(|d| serde_json::from_str(d).ok())
                    .unwrap_or(Value::Null);
                let id = format!("MOCKSESSION{}", self.next_session);
                self.next_session += 1;
                self.record.lock().plays.push(crate::Play {
                    kind: kind.to_string(),
                    target,
                    device,
                    settings: body["settings"].clone(),
                });
                self.sessions.insert(
                    id.clone(),
                    Session {
                        kind: kind.to_string(),
                        polls: 0,
                        connected: false,
                        peer: None,
                        sdp_asked: false,
                    },
                );
                Response::json(json!({
                    "sessionPath": format!("v5/sessions/{kind}/{id}"),
                    "sessionId": id,
                    "state": "Provisioning"
                }))
            }
            ("GET", ["v5", "sessions", _, id, "state"]) => {
                let (queue, awake) = (self.config.queue_polls, self.awake);
                let Some(s) = self.sessions.get_mut(*id) else {
                    return Response::status(404, "");
                };
                s.polls += 1;
                let state = if s.kind == "home" {
                    if !awake {
                        // The console never wakes on its own here: the
                        // client has to ask.
                        return Response::json(json!({
                            "state": "Failed",
                            "errorDetails": { "code": "WNSError", "message": "WaitingForServerToRegister" }
                        }));
                    }
                    if s.polls < 2 {
                        "Provisioning"
                    } else {
                        "Provisioned"
                    }
                } else if s.polls < 2 {
                    "Provisioning"
                } else if s.polls < 2 + queue {
                    "WaitingForResources"
                } else if !s.connected {
                    "ReadyToConnect"
                } else {
                    "Provisioned"
                };
                Response::json(json!({ "state": state, "platform": s.kind }))
            }
            ("POST", ["v5", "sessions", _, id, "connect"]) => {
                let Some(s) = self.sessions.get_mut(*id) else {
                    return Response::status(404, "");
                };
                s.connected = true;
                let token = r.json()["userToken"].as_str().unwrap_or("").to_owned();
                self.record.lock().transfer_tokens.push(token);
                Response::status(200, "")
            }
            ("POST", ["v5", "sessions", _, id, "keepalive"]) => {
                self.record.lock().keepalives += 1;
                if self.sessions.contains_key(*id) {
                    Response::json(json!({ "alive": true }))
                } else {
                    Response::status(404, "")
                }
            }
            ("POST", ["v5", "sessions", _, id, "sdp"]) => {
                let (record, view) = (self.record.clone(), self.view.clone());
                let Some(s) = self.sessions.get_mut(*id) else {
                    return Response::status(404, "");
                };
                let body = r.json();
                let Some(offer) = body["sdp"].as_str() else {
                    return Response::status(400, "");
                };
                self.record.lock().sdp_configuration = body["configuration"].clone();
                match Peer::answer(offer, r.local.ip(), record, view, self.config.link) {
                    Ok(p) => {
                        s.peer = Some(p);
                        Response::json(json!({}))
                    }
                    Err(e) => Response::status(400, format!("{{\"message\":{}}}", json!(e))),
                }
            }
            ("GET", ["v5", "sessions", _, id, "sdp"]) => {
                let Some(s) = self.sessions.get_mut(*id) else {
                    return Response::status(404, "");
                };
                // "Not yet" once, as the real service says while the console
                // makes its answer.
                if !s.sdp_asked {
                    s.sdp_asked = true;
                    return Response::status(204, "");
                }
                let Some(peer) = &s.peer else {
                    return Response::status(204, "");
                };
                let exchange =
                    json!({ "sdp": peer.answer, "sdpType": "answer", "status": "success" });
                Response::json(
                    json!({ "exchangeResponse": exchange.to_string(), "errorDetails": null }),
                )
            }
            ("POST", ["v5", "sessions", _, id, "ice"]) => {
                let Some(peer) = self.sessions.get(*id).and_then(|s| s.peer.as_ref()) else {
                    return Response::status(404, "");
                };
                let lines: Vec<String> = r.json()["candidate"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|c| c["candidate"].as_str())
                            .map(|c| c.trim_start_matches("a=").to_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                self.record.lock().client_candidates = lines.clone();
                peer.add_remote(lines);
                Response::json(json!({}))
            }
            ("GET", ["v5", "sessions", _, id, "ice"]) => {
                let Some(peer) = self.sessions.get(*id).and_then(|s| s.peer.as_ref()) else {
                    return Response::status(404, "");
                };
                let ours = json!([
                    { "candidate": format!("a={} ", peer.candidate), "messageType": "iceCandidate", "sdpMLineIndex": "0", "sdpMid": "0" },
                    { "candidate": "a=end-of-candidates", "messageType": "iceCandidate", "sdpMLineIndex": "0", "sdpMid": "0" }
                ]);
                Response::json(
                    json!({ "exchangeResponse": ours.to_string(), "errorDetails": null }),
                )
            }
            ("DELETE", ["v5", "sessions", _, id]) => {
                if self.sessions.remove(*id).is_some() {
                    self.record.lock().ended_sessions += 1;
                }
                Response::status(200, "")
            }
            ("GET", ["v1", "waittime", _]) => Response::json(json!({
                "estimatedProvisioningTimeInSeconds": 10,
                "estimatedAllocationTimeInSeconds": 20,
                "estimatedTotalWaitTimeInSeconds": 30
            })),
            ("GET", ["v2", "titles"]) | ("GET", ["v2", "titles", "mru"]) => Response::json(json!({
                "results": [{
                    "titleId": CLOUD_TITLE,
                    "details": {
                        "productId": CLOUD_PRODUCT,
                        "hasEntitlement": self.config.cloud == Cloud::GamePass,
                        "isFreeInStore": true
                    }
                }]
            })),
            _ => Response::status(404, format!("{{\"message\":\"no mock for {path}\"}}")),
        }
    }
}

fn field<'a>(form: &'a [(String, String)], name: &str) -> Option<&'a str> {
    form.iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// `/HOST/PATH` as (HOST, /PATH).
fn split(path: &str) -> (&str, &str) {
    let rest = path.trim_start_matches('/');
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    }
}

/// An Xbox Live token answer, valid for a day.
fn xsts(token: &str, profile: bool) -> Response {
    let not_after = pingpong_xbox::time::now_unix() + 24 * 3600;
    let mut xui = json!({ "uhs": UHS });
    if profile {
        xui["gtg"] = json!("Mock Player");
        xui["xid"] = json!("2533274800000001");
    }
    Response::json(json!({
        "IssueInstant": "2026-01-01T00:00:00.0000000Z",
        "NotAfter": format_utc(not_after),
        "Token": token,
        "DisplayClaims": { "xui": [xui] }
    }))
}

/// Seconds since the epoch as RFC 3339 UTC (Howard Hinnant's
/// `civil_from_days`).
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.0000000Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_written_are_times_read() {
        for t in [0, 951_868_800, 1_791_344_321, 4_102_444_800] {
            assert_eq!(pingpong_xbox::time::parse_utc(&format_utc(t)), Some(t));
        }
    }

    #[test]
    fn paths_split_into_host_and_path() {
        assert_eq!(
            split("/xsts.auth.xboxlive.com/xsts/authorize"),
            ("xsts.auth.xboxlive.com", "/xsts/authorize")
        );
        assert_eq!(split("/host"), ("host", "/"));
    }
}
