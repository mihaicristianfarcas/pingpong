//! Talking to the host on this computer: its web API over HTTPS on
//! localhost, trusting only the certificate Pong made for itself, with the
//! local token Pong leaves for its own user or an app token got by signing in.
//!
//! PONG_APP_URL, PONG_APP_CERT and PONG_APP_TOKEN point it at another host
//! (tests).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

/// Where Pong keeps its state: the same place `pong` does.
pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PONG_DATA_DIR") {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        base.join("Pong")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        home.join("Library/Application Support/Pong")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let env = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        let home = env("HOME").unwrap_or_else(std::env::temp_dir);
        env("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home.join(".config"))
            .join("pong")
    }
}

/// This user's own app token: in the user's profile on Windows (the host's
/// folder is the service's), else beside the host's state.
fn app_token_path() -> PathBuf {
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("Pong").join("app-token");
    }
    data_dir().join("app-token")
}

#[derive(Debug, Clone, PartialEq)]
pub enum Failure {
    /// Nothing answers: Pong is not running.
    NotRunning,
    /// The host wants the admin account (created first when `setup`).
    SignIn,
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::NotRunning => write!(f, "Pong is not running"),
            Failure::SignIn => write!(f, "sign in to Pong"),
            Failure::Other(e) => write!(f, "{e}"),
        }
    }
}

pub struct Api {
    base: String,
    http: ureq::Agent,
    token: Option<String>,
    /// The token is the local one (no signing out).
    pub local: bool,
}

#[derive(Deserialize)]
struct PortOnly {
    #[serde(default = "default_web_port")]
    web_port: u16,
}

fn default_web_port() -> u16 {
    47802
}

fn read_token(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

impl Api {
    /// Look at what the host left on disk: its port, its certificate, a token.
    pub fn discover() -> Api {
        let dir = data_dir();
        let base = std::env::var("PONG_APP_URL").unwrap_or_else(|_| {
            let port = std::fs::read_to_string(dir.join("config.toml"))
                .ok()
                .and_then(|t| toml::from_str::<PortOnly>(&t).ok())
                .map(|c| c.web_port)
                .unwrap_or(default_web_port());
            format!("https://localhost:{port}")
        });
        let cert_path = std::env::var_os("PONG_APP_CERT")
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join("web-cert.pem"));
        let pem = std::fs::read(&cert_path).ok();
        let mut tls = ureq::tls::TlsConfig::builder();
        match pem
            .as_deref()
            .and_then(|p| ureq::tls::Certificate::from_pem(p).ok())
        {
            Some(cert) => tls = tls.root_certs(ureq::tls::RootCerts::new_with_certs(&[cert])),
            // No certificate yet: the host has not run. Nothing will answer.
            None => tracing::info!(path = %cert_path.display(), "no certificate from Pong yet"),
        }
        let http: ureq::Agent = ureq::Agent::config_builder()
            .tls_config(tls.build())
            .timeout_global(Some(Duration::from_secs(8)))
            .timeout_connect(Some(Duration::from_millis(1500)))
            .http_status_as_error(false)
            .build()
            .into();
        let (token, local) = match std::env::var("PONG_APP_TOKEN")
            .ok()
            .or_else(|| read_token(&dir.join("local-token")))
        {
            Some(t) => (Some(t), true),
            None => (read_token(&app_token_path()), false),
        };
        Api {
            base,
            http,
            token,
            local,
        }
    }

    pub fn has_token(&self) -> bool {
        self.token.is_some()
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, String), Failure> {
        let url = format!("{}{path}", self.base);
        let auth = self.token.as_ref().map(|t| format!("Bearer {t}"));
        let result = match (method, body) {
            ("GET", _) => {
                let mut r = self.http.get(&url);
                if let Some(a) = &auth {
                    r = r.header("Authorization", a);
                }
                r.call()
            }
            ("DELETE", _) => {
                let mut r = self.http.delete(&url);
                if let Some(a) = &auth {
                    r = r.header("Authorization", a);
                }
                r.call()
            }
            (m, body) => {
                let mut r = if m == "PUT" {
                    self.http.put(&url)
                } else {
                    self.http.post(&url)
                };
                if let Some(a) = &auth {
                    r = r.header("Authorization", a);
                }
                r.send_json(body.cloned().unwrap_or(Value::Object(Default::default())))
            }
        };
        match result {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let text = response.body_mut().read_to_string().unwrap_or_default();
                if status == 401 {
                    return Err(Failure::SignIn);
                }
                Ok((status, text))
            }
            Err(ureq::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
                ) =>
            {
                Err(Failure::NotRunning)
            }
            Err(ureq::Error::ConnectionFailed) | Err(ureq::Error::HostNotFound) => {
                Err(Failure::NotRunning)
            }
            Err(e) => Err(Failure::Other(e.to_string())),
        }
    }

    fn json(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, Failure> {
        let (status, text) = self.call(method, path, body)?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        if (200..300).contains(&status) {
            Ok(value)
        } else {
            let msg = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("HTTP {status}"));
            Err(Failure::Other(msg))
        }
    }

    pub fn get(&self, path: &str) -> Result<Value, Failure> {
        self.json("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Value, Failure> {
        self.json("POST", path, Some(body))
    }

    pub fn put(&self, path: &str, body: &Value) -> Result<Value, Failure> {
        self.json("PUT", path, Some(body))
    }

    pub fn delete(&self, path: &str) -> Result<Value, Failure> {
        self.json("DELETE", path, None)
    }

    pub fn text(&self, path: &str) -> Result<String, Failure> {
        let (status, text) = self.call("GET", path, None)?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(Failure::Other(format!("HTTP {status}")))
        }
    }

    /// Sign in with the admin account (creating it when `setup`), and keep
    /// the app token it gives.
    pub fn sign_in(
        &mut self,
        user: &str,
        password: &str,
        setup: bool,
        name: &str,
    ) -> Result<(), String> {
        let body =
            serde_json::json!({"user": user, "password": password, "setup": setup, "name": name});
        let v = self.post("/api/app-token", &body).map_err(|e| match e {
            Failure::SignIn => "Wrong user name or password.".to_string(),
            e => e.to_string(),
        })?;
        let token = v
            .get("token")
            .and_then(Value::as_str)
            .ok_or("The host did not give a token.")?
            .to_string();
        let path = app_token_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        write_private(&path, token.as_bytes())
            .map_err(|e| format!("The token was not saved: {e}"))?;
        self.token = Some(token);
        self.local = false;
        Ok(())
    }

    /// Forget this app's token (on the host too).
    pub fn sign_out(&mut self) {
        if self.local {
            return;
        }
        let _ = self.delete("/api/app-token");
        let _ = std::fs::remove_file(app_token_path());
        self.token = None;
    }

    /// The host no longer takes the token: forget it.
    pub fn drop_token(&mut self) {
        if !self.local {
            let _ = std::fs::remove_file(app_token_path());
        }
        self.token = None;
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        // The user's own profile: private to them already.
        std::fs::write(path, bytes)
    }
}

// ---------------------------------------------------------------------------
// What the host reports

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct HostState {
    pub setup_needed: bool,
    pub signed_in: bool,
    pub name: String,
    pub os: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Status {
    pub name: String,
    pub id: String,
    pub version: String,
    pub port: u16,
    pub session: Option<Session>,
    pub clients: usize,
    pub pending: Vec<Pending>,
    pub tunnels: usize,
    pub internet: bool,
    pub public: Vec<String>,
    /// The router's port mapping ("" before the first try).
    pub port_mapping: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Session {
    pub client: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub codec: String,
    pub bitrate_kbps: u32,
    pub started_unix: u64,
    pub encoded_fps: f64,
    pub mbps: f64,
    pub host_latency_ms: f64,
    pub recoveries: u64,
    pub client_loss_pct: f64,
    pub rtt_ms: f64,
    pub agent: Option<AgentStatus>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentStatus {
    pub access: String,
    pub flags: u8,
    pub watchers: Vec<String>,
    pub controller: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pending {
    pub id: u32,
    pub client_name: String,
    pub agent: bool,
    pub peer: String,
    pub waiting_secs: u64,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Client {
    pub name: String,
    pub id: String,
    pub key: String,
    pub paired_at: u64,
    pub online: bool,
    pub agent: bool,
    pub access: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct LogEntry {
    pub unix_ms: u64,
    pub client: String,
    pub text: String,
}
