//! HTTPS to Microsoft's services, in one place: every request names its
//! host and path, so a test can send them all to a mock console instead
//! (`PING_XBOX_MOCK=http://127.0.0.1:PORT`: `https://HOST/PATH` becomes
//! `http://127.0.0.1:PORT/HOST/PATH`).
//!
//! Errors carry the status and the start of the body, never the request's
//! headers: those hold the tokens.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

/// The environment variable that sends every request to a mock console.
pub const MOCK_ENV: &str = "PING_XBOX_MOCK";

/// A request that did not get the answer it wanted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpError {
    /// The HTTP status, or `None` when no answer came (no network, a
    /// timeout).
    pub status: Option<u16>,
    /// What went wrong, for a person: the service's error text, or the
    /// network's.
    pub detail: String,
    /// The host and path asked (no query: it can hold codes).
    pub what: String,
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "{} answered {s}: {}", self.what, self.detail),
            None => write!(f, "{} did not answer: {}", self.what, self.detail),
        }
    }
}

impl std::error::Error for HttpError {}

/// An answer: its status and body.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub body: String,
}

impl Answer {
    pub fn json<T: DeserializeOwned>(&self, what: &str) -> Result<T, HttpError> {
        serde_json::from_str(&self.body).map_err(|e| HttpError {
            status: Some(self.status),
            detail: format!("an answer that could not be read ({e})"),
            what: what.to_owned(),
        })
    }
}

/// What a request carries.
pub enum Body<'a> {
    None,
    Json(&'a Value),
    /// `application/x-www-form-urlencoded`, already encoded.
    Form(&'a str),
}

#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
    mock: Option<String>,
}

impl Default for Http {
    fn default() -> Self {
        Http::new()
    }
}

impl Http {
    /// A client for the real services, or the mock when [`MOCK_ENV`] is
    /// set.
    pub fn new() -> Http {
        Http::with_mock(std::env::var(MOCK_ENV).ok().filter(|m| !m.is_empty()))
    }

    pub fn with_mock(mock: Option<String>) -> Http {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            .timeout_connect(Some(Duration::from_secs(8)))
            // Statuses are answers here: the services say "not yet" (204)
            // and "no" (4xx) with them.
            .http_status_as_error(false)
            .user_agent(format!("pingpong/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Http {
            agent,
            mock: mock.map(|m| m.trim_end_matches('/').to_owned()),
        }
    }

    pub fn is_mock(&self) -> bool {
        self.mock.is_some()
    }

    /// The mock's address, when requests go to one.
    pub fn mock_ip(&self) -> Option<std::net::IpAddr> {
        let m = self.mock.as_deref()?;
        let host = m.split("://").nth(1)?.split('/').next()?;
        let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
        host.trim_start_matches('[')
            .trim_end_matches(']')
            .parse()
            .ok()
    }

    /// Where a request for `host` and `path` goes.
    pub fn url(&self, host: &str, path: &str) -> String {
        match &self.mock {
            Some(m) => format!("{m}/{host}{path}"),
            None => format!("https://{host}{path}"),
        }
    }

    /// `method` `host``path` with these headers and body; any status is an
    /// [`Answer`], only a failure to get one an error.
    pub fn request(
        &self,
        method: &str,
        host: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Body<'_>,
    ) -> Result<Answer, HttpError> {
        let url = self.url(host, path);
        let what = format!("{host}{}", path.split('?').next().unwrap_or(path));
        let fail = |e: ureq::Error| HttpError {
            status: None,
            detail: e.to_string(),
            what: what.clone(),
        };
        let response = match (method, body) {
            ("GET", _) => {
                let mut r = self.agent.get(&url);
                for (k, v) in headers {
                    r = r.header(*k, *v);
                }
                r.call()
            }
            ("DELETE", _) => {
                let mut r = self.agent.delete(&url);
                for (k, v) in headers {
                    r = r.header(*k, *v);
                }
                r.call()
            }
            (_, body) => {
                let mut r = self.agent.post(&url);
                for (k, v) in headers {
                    r = r.header(*k, *v);
                }
                match body {
                    Body::None => r.header("Content-Type", "application/json").send("{}"),
                    Body::Json(v) => r
                        .header("Content-Type", "application/json")
                        .send(v.to_string()),
                    Body::Form(f) => r
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .send(f),
                }
            }
        }
        .map_err(fail)?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .with_config()
            .limit(16 << 20)
            .read_to_string()
            .map_err(fail)?;
        Ok(Answer { status, body })
    }

    /// A request that must succeed (2xx).
    pub fn ok(
        &self,
        method: &str,
        host: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Body<'_>,
    ) -> Result<Answer, HttpError> {
        let answer = self.request(method, host, path, headers, body)?;
        if (200..300).contains(&answer.status) {
            Ok(answer)
        } else {
            Err(HttpError {
                status: Some(answer.status),
                detail: summary(&answer.body),
                what: format!("{host}{}", path.split('?').next().unwrap_or(path)),
            })
        }
    }
}

/// The part of an error body worth showing: the service's message when it
/// has one, else the start of the body.
pub fn summary(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        for key in [
            "error_description",
            "message",
            "Message",
            "errorMessage",
            "error",
        ] {
            if let Some(s) = v.get(key).and_then(Value::as_str) {
                return clip(s);
            }
        }
        if let Some(code) = v.get("XErr").and_then(Value::as_u64) {
            return format!("Xbox error {code}");
        }
    }
    if body.trim().is_empty() {
        return "no details".into();
    }
    clip(body)
}

fn clip(s: &str) -> String {
    const MAX: usize = 300;
    let s = s.trim();
    match s.char_indices().nth(MAX) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// `application/x-www-form-urlencoded` for a list of pairs.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode `application/x-www-form-urlencoded` (the mock reads forms).
pub fn parse_form(body: &str) -> Vec<(String, String)> {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let decode = |s: &str| {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            let escaped = (b[i] == b'%' && i + 2 < b.len())
                .then(|| Some(hex(b[i + 1])? << 4 | hex(b[i + 2])?))
                .flatten();
            match (b[i], escaped) {
                (_, Some(v)) => {
                    out.push(v);
                    i += 3;
                }
                (b'+', None) => {
                    out.push(b' ');
                    i += 1;
                }
                (c, None) => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    body.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mock_takes_every_host() {
        let real = Http::with_mock(None);
        assert_eq!(
            real.url("xsts.auth.xboxlive.com", "/xsts/authorize"),
            "https://xsts.auth.xboxlive.com/xsts/authorize"
        );
        let mock = Http::with_mock(Some("http://127.0.0.1:9/".into()));
        assert_eq!(mock.mock_ip(), Some("127.0.0.1".parse().unwrap()));
        assert_eq!(real.mock_ip(), None);
        assert_eq!(
            mock.url("xsts.auth.xboxlive.com", "/xsts/authorize"),
            "http://127.0.0.1:9/xsts.auth.xboxlive.com/xsts/authorize"
        );
    }

    #[test]
    fn forms_round_trip() {
        let f = form(&[
            ("scope", "xboxlive.signin offline_access"),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ]);
        assert_eq!(
            f,
            "scope=xboxlive.signin+offline_access&grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        );
        assert_eq!(
            parse_form(&f),
            vec![
                ("scope".into(), "xboxlive.signin offline_access".into()),
                (
                    "grant_type".into(),
                    "urn:ietf:params:oauth:grant-type:device_code".into()
                ),
            ]
        );
        assert_eq!(
            parse_form("a=%zz&b=%4&c=é%41"),
            vec![
                ("a".into(), "%zz".into()),
                ("b".into(), "%4".into()),
                ("c".into(), "éA".into())
            ]
        );
    }

    #[test]
    fn error_bodies_say_the_services_message() {
        assert_eq!(
            summary(r#"{"error":"authorization_pending","error_description":"Not yet."}"#),
            "Not yet."
        );
        assert_eq!(summary(r#"{"XErr":2148916233}"#), "Xbox error 2148916233");
        assert_eq!(summary(""), "no details");
        assert_eq!(summary(&"x".repeat(1000)).chars().count(), 301);
    }
}
