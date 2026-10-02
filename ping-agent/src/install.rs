//! Ping's MCP server in other agents' settings: Claude Code, Codex,
//! OpenCode, Gemini CLI, Cursor, Copilot CLI, Factory's Droid, Amp, Kiro,
//! Claude Desktop, and Diri's own agent accounts. Adding it writes one entry,
//! `pingpong`, into the file each of them reads its MCP servers from, the
//! way its own `mcp add` would; removing it takes that entry out and leaves
//! the rest of the file as it was.
//!
//! The files are edited here rather than through each agent's CLI: not every
//! agent has one, a GUI app does not find them on its `PATH`, and Diri's
//! accounts each have a settings folder of their own (`CODEX_HOME`,
//! `CLAUDE_CONFIG_DIR`) that a CLI only reaches through its environment.
//! JSON keeps its keys in their order and its indentation; Codex's TOML keeps
//! its comments (`toml_edit`). A file that is not plain JSON (JSONC, with
//! comments, as OpenCode and Zed allow) is not rewritten: the person is told
//! to add the entry by hand. Writes go to a temporary file renamed over the
//! old one, keeping its permissions (`~/.claude.json` is the owner's alone).
//!
//! The server is this program with `mcp` (`Ping mcp`, `ping-agent mcp`), and
//! `--data-dir` when Ping's data folder was moved (`PING_DATA_DIR`), since
//! agents start their servers with an environment of their own.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// The entry's name in every agent's settings.
pub const NAME: &str = "pingpong";

/// How long an agent that asks for a limit waits for a tool call: longer
/// than an action waits while a person holds the host (`Ping mcp`'s
/// `--hold-wait`, 50 s).
const TOOL_TIMEOUT_MS: u64 = 60_000;

/// How an agent starts the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub command: String,
    pub args: Vec<String>,
}

impl Server {
    /// This program as the server.
    pub fn this() -> Result<Server, String> {
        let exe = std::env::current_exe()
            .map_err(|e| format!("Where this program is is not known: {e}"))?;
        let mut args = vec!["mcp".to_string()];
        if let Some(dir) = std::env::var_os("PING_DATA_DIR") {
            args.push("--data-dir".into());
            args.push(PathBuf::from(dir).display().to_string());
        }
        Ok(Server {
            command: exe.display().to_string(),
            args,
        })
    }

    /// The command and its arguments, as one list (OpenCode's shape).
    fn argv(&self) -> Vec<String> {
        std::iter::once(self.command.clone())
            .chain(self.args.iter().cloned())
            .collect()
    }
}

/// An agent that can use MCP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum App {
    ClaudeCode,
    Codex,
    OpenCode,
    Gemini,
    Cursor,
    Copilot,
    Droid,
    Amp,
    Kiro,
    ClaudeDesktop,
    Diri,
}

impl App {
    pub const ALL: [App; 11] = [
        App::ClaudeCode,
        App::Codex,
        App::OpenCode,
        App::Gemini,
        App::Cursor,
        App::Copilot,
        App::Droid,
        App::Amp,
        App::Kiro,
        App::ClaudeDesktop,
        App::Diri,
    ];

    /// Its name on the command line.
    pub fn id(self) -> &'static str {
        match self {
            App::ClaudeCode => "claude-code",
            App::Codex => "codex",
            App::OpenCode => "opencode",
            App::Gemini => "gemini",
            App::Cursor => "cursor",
            App::Copilot => "copilot",
            App::Droid => "droid",
            App::Amp => "amp",
            App::Kiro => "kiro",
            App::ClaudeDesktop => "claude-desktop",
            App::Diri => "diri",
        }
    }

    /// Its name to a person.
    pub fn name(self) -> &'static str {
        match self {
            App::ClaudeCode => "Claude Code",
            App::Codex => "Codex",
            App::OpenCode => "OpenCode",
            App::Gemini => "Gemini CLI",
            App::Cursor => "Cursor",
            App::Copilot => "GitHub Copilot CLI",
            App::Droid => "Factory Droid",
            App::Amp => "Amp",
            App::Kiro => "Kiro",
            App::ClaudeDesktop => "Claude Desktop",
            App::Diri => "Diri",
        }
    }

    pub fn parse(s: &str) -> Option<App> {
        let s = s.to_ascii_lowercase();
        App::ALL.into_iter().find(|a| {
            a.id() == s
                || a.name().eq_ignore_ascii_case(&s)
                || (s == "claude" && *a == App::ClaudeCode)
        })
    }
}

/// Where an agent keeps things: the home folder and the environment
/// variables that move them. A value, so tests can make up a computer.
#[derive(Debug, Clone)]
pub struct Places {
    pub home: PathBuf,
    pub env: HashMap<String, String>,
}

impl Places {
    /// This computer, as the signed-in user has it.
    pub fn here() -> Places {
        let env: HashMap<String, String> = std::env::vars().collect();
        let home = env
            .get("HOME")
            .or_else(|| env.get("USERPROFILE"))
            .filter(|h| !h.is_empty())
            .map(PathBuf::from)
            .unwrap_or_default();
        Places { home, env }
    }

    fn var(&self, key: &str) -> Option<PathBuf> {
        self.env
            .get(key)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    }

    /// `~/.config`, or where XDG_CONFIG_HOME says (on every system: the
    /// agents that use it, OpenCode and Amp, read it on macOS too).
    fn config(&self) -> PathBuf {
        self.var("XDG_CONFIG_HOME")
            .unwrap_or_else(|| self.home.join(".config"))
    }

    fn codex_home(&self) -> PathBuf {
        self.var("CODEX_HOME")
            .unwrap_or_else(|| self.home.join(".codex"))
    }

    fn claude_json(&self) -> PathBuf {
        match self.var("CLAUDE_CONFIG_DIR") {
            Some(dir) => dir.join(".claude.json"),
            None => self.home.join(".claude.json"),
        }
    }

    fn claude_desktop(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support/Claude")
        } else if cfg!(windows) {
            self.var("APPDATA")
                .unwrap_or_else(|| self.home.join("AppData/Roaming"))
                .join("Claude")
        } else {
            self.config().join("Claude")
        }
    }

    /// Where Diri keeps its agent accounts (`accounts.json`, beside its
    /// daemon's socket): its own data folder on a Mac, the runtime folder
    /// on Linux.
    fn diri(&self) -> PathBuf {
        if let Some(root) = self.var("DIRI_APP_SUPPORT") {
            return root;
        }
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support/Dirijor")
        } else if cfg!(target_os = "linux") {
            match self.var("XDG_RUNTIME_DIR") {
                Some(run) => run.join("diri"),
                None => self
                    .var("XDG_STATE_HOME")
                    .unwrap_or_else(|| self.home.join(".local/state"))
                    .join("diri/run"),
            }
        } else {
            self.home.join(".diri")
        }
    }
}

/// How an entry is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// `{"command": ..., "args": [...]}` (Claude Desktop, Cursor, Gemini,
    /// Amp, Kiro).
    Plain,
    /// Plus `"type": "stdio"` (Claude Code, Factory Droid).
    Stdio,
    /// `{"type": "local", "command": [...], "enabled": true, "timeout":
    /// 60000}` (OpenCode, whose requests otherwise give up after 5 s).
    OpenCode,
    /// `{"type": "local", ..., "tools": ["*"]}` (Copilot CLI: without
    /// `tools` it offers none of them).
    Copilot,
}

/// One settings file and where in it the servers are.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// A JSON file; `key` is the object holding the servers.
    Json {
        path: PathBuf,
        key: &'static str,
        shape: Shape,
    },
    /// Codex's `config.toml`, `[mcp_servers.NAME]`.
    CodexToml { path: PathBuf },
}

impl Target {
    fn json(path: PathBuf, key: &'static str, shape: Shape) -> Target {
        Target::Json { path, key, shape }
    }

    fn path(&self) -> &Path {
        match self {
            Target::Json { path, .. } | Target::CodexToml { path } => path,
        }
    }
}

/// What an agent's settings say about the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The agent is not on this computer.
    Missing,
    /// It is, without the server.
    Absent,
    /// The server is there: this program's (`current`), or another copy's
    /// (an older build, one elsewhere), which adding again replaces.
    Added { current: bool },
    /// The settings could not be read (comments in JSON, a syntax error).
    Unreadable(String),
    /// Nothing to add here: the agent reads another one's settings (Diri
    /// with no account of its own uses Claude Code's and Codex's).
    Shared(&'static str),
}

/// Where `app` keeps its servers; empty when it is not here.
fn targets(app: App, places: &Places) -> Result<Vec<Target>, State> {
    let present = |p: PathBuf| {
        if p.exists() {
            Ok(())
        } else {
            Err(State::Missing)
        }
    };
    let home = &places.home;
    let one = |t: Target| Ok(vec![t]);
    match app {
        App::ClaudeCode => {
            let file = places.claude_json();
            if !file.exists() && !home.join(".claude").exists() {
                return Err(State::Missing);
            }
            one(Target::json(file, "mcpServers", Shape::Stdio))
        }
        App::Codex => {
            let dir = places.codex_home();
            present(dir.clone())?;
            one(Target::CodexToml {
                path: dir.join("config.toml"),
            })
        }
        App::OpenCode => {
            let dir = places.config().join("opencode");
            if !dir.exists() && !home.join(".local/share/opencode").exists() {
                return Err(State::Missing);
            }
            // Its settings file may be JSONC (then left alone, below).
            let path = ["opencode.json", "opencode.jsonc", "config.json"]
                .iter()
                .map(|f| dir.join(f))
                .find(|p| p.exists())
                .unwrap_or_else(|| dir.join("opencode.json"));
            one(Target::json(path, "mcp", Shape::OpenCode))
        }
        App::Gemini => {
            present(home.join(".gemini"))?;
            one(Target::json(
                home.join(".gemini/settings.json"),
                "mcpServers",
                Shape::Plain,
            ))
        }
        App::Cursor => {
            present(home.join(".cursor"))?;
            one(Target::json(
                home.join(".cursor/mcp.json"),
                "mcpServers",
                Shape::Plain,
            ))
        }
        App::Copilot => {
            present(home.join(".copilot"))?;
            one(Target::json(
                home.join(".copilot/mcp-config.json"),
                "mcpServers",
                Shape::Copilot,
            ))
        }
        App::Droid => {
            present(home.join(".factory"))?;
            one(Target::json(
                home.join(".factory/mcp.json"),
                "mcpServers",
                Shape::Stdio,
            ))
        }
        App::Amp => {
            let dir = places.config().join("amp");
            present(dir.clone())?;
            // One key with a dot in it, not a nested object.
            one(Target::json(
                dir.join("settings.json"),
                "amp.mcpServers",
                Shape::Plain,
            ))
        }
        App::Kiro => {
            present(home.join(".kiro"))?;
            one(Target::json(
                home.join(".kiro/settings/mcp.json"),
                "mcpServers",
                Shape::Plain,
            ))
        }
        App::ClaudeDesktop => {
            let dir = places.claude_desktop();
            present(dir.clone())?;
            one(Target::json(
                dir.join("claude_desktop_config.json"),
                "mcpServers",
                Shape::Plain,
            ))
        }
        App::Diri => {
            let dir = places.diri();
            present(dir.clone())?;
            let targets = diri_targets(&dir.join("accounts.json"));
            if targets.is_empty() {
                return Err(State::Shared(
                    "Its agents use your Claude Code and Codex settings.",
                ));
            }
            Ok(targets)
        }
    }
}

/// Diri's agent accounts on this computer that keep settings of their own:
/// a Codex account's `CODEX_HOME`, a Claude Code account's
/// `CLAUDE_CONFIG_DIR` (one that only signs in apart shares `~/.claude`).
fn diri_targets(accounts: &Path) -> Vec<Target> {
    let Some(file) = std::fs::read(accounts)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return Vec::new();
    };
    let profiles = file
        .get("profiles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for p in profiles {
        let remote = p.get("host").is_some_and(|h| !h.is_null());
        let home = p.get("config_home").and_then(Value::as_str).unwrap_or("");
        if remote || home.is_empty() || !Path::new(home).is_absolute() {
            continue;
        }
        let home = PathBuf::from(home);
        let target = match p.get("agent").and_then(Value::as_str) {
            Some("codex") => Target::CodexToml {
                path: home.join("config.toml"),
            },
            Some("claude-code") if p.get("login_store").is_none_or(Value::is_null) => {
                Target::json(home.join(".claude.json"), "mcpServers", Shape::Stdio)
            }
            _ => continue,
        };
        if !out.contains(&target) {
            out.push(target);
        }
    }
    out
}

/// What `app`'s settings say about the server.
pub fn state(app: App, places: &Places, server: &Server) -> State {
    let targets = match targets(app, places) {
        Ok(t) => t,
        Err(state) => return state,
    };
    let mut current = true;
    for t in &targets {
        match read_entry(t) {
            Err(e) => return State::Unreadable(e),
            Ok(None) => return State::Absent,
            Ok(Some(entry)) => current &= entry == *server,
        }
    }
    State::Added { current }
}

/// The settings files `app` reads its servers from (for saying where).
pub fn files(app: App, places: &Places) -> Vec<PathBuf> {
    targets(app, places)
        .map(|t| t.iter().map(|t| t.path().to_path_buf()).collect())
        .unwrap_or_default()
}

/// Add the server to `app` (or replace another copy's). Says what it did.
pub fn add(app: App, places: &Places, server: &Server) -> Result<String, String> {
    let targets = targets(app, places).map_err(|s| refusal(app, s))?;
    for t in &targets {
        edit(t, Some(server))?;
    }
    tracing::info!(app = app.id(), files = targets.len(), "MCP server added");
    Ok(format!(
        "Added to {}: its new sessions have your hosts as tools ({}).",
        app.name(),
        where_(&targets, places)
    ))
}

/// Take the server out of `app`'s settings.
pub fn remove(app: App, places: &Places) -> Result<String, String> {
    let targets = targets(app, places).map_err(|s| refusal(app, s))?;
    for t in &targets {
        edit(t, None)?;
    }
    tracing::info!(app = app.id(), "MCP server removed");
    Ok(format!(
        "Removed from {} ({}).",
        app.name(),
        where_(&targets, places)
    ))
}

fn refusal(app: App, state: State) -> String {
    match state {
        State::Missing => format!("{} is not on this computer.", app.name()),
        State::Shared(why) => format!("Nothing to change in {}: {why}", app.name()),
        State::Unreadable(e) => e,
        _ => String::new(),
    }
}

/// The files, with the home folder as `~`.
fn where_(targets: &[Target], places: &Places) -> String {
    targets
        .iter()
        .map(|t| tilde(t.path(), &places.home))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `path` with the home folder as `~`.
pub fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

// ---------------------------------------------------------------------------
// The files

/// The server's entry in `t`, if there is one.
fn read_entry(t: &Target) -> Result<Option<Server>, String> {
    match t {
        Target::Json { path, key, shape } => {
            let Some(doc) = read_json(path)? else {
                return Ok(None);
            };
            Ok(doc
                .get(*key)
                .and_then(|servers| servers.get(NAME))
                .map(|e| entry_server(e, *shape)))
        }
        Target::CodexToml { path } => {
            let Some(doc) = read_toml(path)? else {
                return Ok(None);
            };
            let Some(entry) = doc
                .get("mcp_servers")
                .and_then(|s| s.get(NAME))
                .and_then(|e| e.as_table_like())
            else {
                return Ok(None);
            };
            let command = entry
                .get("command")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string();
            let args = entry
                .get("args")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Ok(Some(Server { command, args }))
        }
    }
}

/// What an entry starts, whatever its shape.
fn entry_server(entry: &Value, shape: Shape) -> Server {
    let strings = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    if shape == Shape::OpenCode {
        let mut argv = strings(entry.get("command")).into_iter();
        return Server {
            command: argv.next().unwrap_or_default(),
            args: argv.collect(),
        };
    }
    Server {
        command: entry
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        args: strings(entry.get("args")),
    }
}

fn new_entry(server: &Server, shape: Shape) -> Value {
    match shape {
        Shape::Plain => json!({"command": server.command, "args": server.args}),
        Shape::Stdio => {
            json!({"type": "stdio", "command": server.command, "args": server.args})
        }
        Shape::OpenCode => json!({"type": "local", "command": server.argv(), "enabled": true,
            "timeout": TOOL_TIMEOUT_MS}),
        Shape::Copilot => json!({"type": "local", "command": server.command,
            "args": server.args, "tools": ["*"]}),
    }
}

/// Put the server's entry in `t` (`Some`), or take it out (`None`).
fn edit(t: &Target, server: Option<&Server>) -> Result<(), String> {
    match t {
        Target::Json { path, key, shape } => {
            let existing = std::fs::read_to_string(path).ok();
            let mut doc = match read_json(path)? {
                Some(doc) => doc,
                None if server.is_none() => return Ok(()),
                None => Value::Object(Map::new()),
            };
            let Some(root) = doc.as_object_mut() else {
                return Err(format!("{} is not a JSON object.", path.display()));
            };
            match server {
                Some(server) => {
                    let servers = root
                        .entry(key.to_string())
                        .or_insert_with(|| Value::Object(Map::new()));
                    let Some(servers) = servers.as_object_mut() else {
                        return Err(format!("\"{key}\" in {} is not an object.", path.display()));
                    };
                    servers.insert(NAME.into(), new_entry(server, *shape));
                }
                None => {
                    let gone = root
                        .get_mut(*key)
                        .and_then(Value::as_object_mut)
                        .and_then(|s| s.shift_remove(NAME));
                    if gone.is_none() {
                        return Ok(());
                    }
                }
            }
            let indent = existing.as_deref().map(indent_of).unwrap_or(2);
            write(path, &pretty(&doc, indent))
        }
        Target::CodexToml { path } => {
            let mut doc = match read_toml(path)? {
                Some(doc) => doc,
                None if server.is_none() => return Ok(()),
                None => toml_edit::DocumentMut::new(),
            };
            match server {
                Some(server) => {
                    let servers = doc
                        .entry("mcp_servers")
                        .or_insert_with(|| {
                            let mut t = toml_edit::Table::new();
                            // `[mcp_servers.pingpong]`, not an empty
                            // `[mcp_servers]` above it.
                            t.set_implicit(true);
                            toml_edit::Item::Table(t)
                        })
                        .as_table_like_mut()
                        .ok_or_else(|| {
                            format!("mcp_servers in {} is not a table.", path.display())
                        })?;
                    let mut entry = toml_edit::Table::new();
                    entry["command"] = toml_edit::value(server.command.as_str());
                    let mut args = toml_edit::Array::new();
                    for a in &server.args {
                        args.push(a.as_str());
                    }
                    entry["args"] = toml_edit::value(args);
                    servers.insert(NAME, toml_edit::Item::Table(entry));
                }
                None => {
                    let gone = doc
                        .get_mut("mcp_servers")
                        .and_then(|s| s.as_table_like_mut())
                        .and_then(|s| s.remove(NAME));
                    if gone.is_none() {
                        return Ok(());
                    }
                }
            }
            write(path, &doc.to_string())
        }
    }
}

/// The file's JSON; `None` if there is no file (or it is empty).
fn read_json(path: &Path) -> Result<Option<Value>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&text).map(Some).map_err(|e| {
        format!(
            "{} is not plain JSON (comments?), so it is left as it is: {e}. Add the \
                server there by hand.",
            path.display()
        )
    })
}

fn read_toml(path: &Path) -> Result<Option<toml_edit::DocumentMut>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    text.parse::<toml_edit::DocumentMut>()
        .map(Some)
        .map_err(|e| {
            format!(
                "{} could not be read, so it is left as it is: {e}",
                path.display()
            )
        })
}

/// The indentation the file uses (spaces before its first indented line).
fn indent_of(text: &str) -> usize {
    text.lines()
        .map(|l| l.len() - l.trim_start_matches(' ').len())
        .find(|&n| n > 0)
        .unwrap_or(2)
}

fn pretty(doc: &Value, indent: usize) -> String {
    use serde::Serialize;
    let spaces = " ".repeat(indent);
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(spaces.as_bytes());
    let mut ser = serde_json::Serializer::with_formatter(&mut out, formatter);
    // Serializing a Value into a Vec cannot fail.
    let _ = doc.serialize(&mut ser);
    let mut text = String::from_utf8(out).unwrap_or_default();
    text.push('\n');
    text
}

/// Replace `path` with `text`: a temporary file beside it, renamed over it,
/// with the old file's permissions (a new one is the owner's alone: it
/// names a program to run).
fn write(path: &Path, text: &str) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let temp = path.with_extension(format!("pingpong-{}.tmp", std::process::id()));
    std::fs::write(&temp, text).map_err(fail)?;
    let permissions = std::fs::metadata(path).map(|m| m.permissions());
    #[cfg(unix)]
    let permissions = permissions.or_else(|_| {
        use std::os::unix::fs::PermissionsExt;
        Ok::<_, std::io::Error>(std::fs::Permissions::from_mode(0o600))
    });
    if let Ok(p) = permissions {
        let _ = std::fs::set_permissions(&temp, p);
    }
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        fail(e)
    })
}

// ---------------------------------------------------------------------------
// The command line: `Ping mcp install ...`, `ping-agent mcp install ...`

/// `install [APP ...|--all]`, `uninstall APP ...`, `status`.
pub fn cli(args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    let places = Places::here();
    let server = match Server::this() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let verb = args.first().map(String::as_str).unwrap_or("status");
    let named: Vec<&String> = args.iter().skip(1).filter(|a| *a != "--all").collect();
    let mut apps = Vec::new();
    for n in &named {
        match App::parse(n) {
            Some(a) => apps.push(a),
            None => {
                eprintln!(
                    "{n}: not an agent this knows. One of: {}",
                    App::ALL.map(App::id).join(", ")
                );
                return ExitCode::FAILURE;
            }
        }
    }
    let all = apps.is_empty() || args.iter().any(|a| a == "--all");
    let chosen = |apps: &[App]| -> Vec<App> {
        if all {
            App::ALL.to_vec()
        } else {
            apps.to_vec()
        }
    };
    let mut ok = true;
    match verb {
        "status" | "list" => {
            for app in App::ALL {
                let said = match state(app, &places, &server) {
                    State::Missing => "not on this computer".to_string(),
                    State::Absent => "not added".to_string(),
                    State::Added { current: true } => "added".to_string(),
                    State::Added { current: false } => {
                        "added, for another copy of Ping (install again to point it here)"
                            .to_string()
                    }
                    State::Unreadable(e) => e,
                    State::Shared(why) => why.to_string(),
                };
                println!("{:<15} {said}", app.id());
            }
        }
        "install" | "add" => {
            if named.is_empty() && !args.iter().any(|a| a == "--all") {
                eprintln!(
                    "usage: mcp install APP ... | --all   (APP: {})",
                    App::ALL.map(App::id).join(", ")
                );
                return ExitCode::FAILURE;
            }
            for app in chosen(&apps) {
                match state(app, &places, &server) {
                    // --all: only where the agent is, and has room for it.
                    State::Missing | State::Shared(_) if all => continue,
                    State::Added { current: true } => {
                        println!("{}: added already", app.name());
                        continue;
                    }
                    _ => {}
                }
                match add(app, &places, &server) {
                    Ok(said) => println!("{said}"),
                    Err(e) => {
                        ok = false;
                        eprintln!("{e}");
                    }
                }
            }
        }
        "uninstall" | "remove" => {
            if named.is_empty() && !args.iter().any(|a| a == "--all") {
                eprintln!("usage: mcp uninstall APP ... | --all");
                return ExitCode::FAILURE;
            }
            for app in chosen(&apps) {
                if !matches!(state(app, &places, &server), State::Added { .. }) {
                    if !all {
                        println!("{}: not added", app.name());
                    }
                    continue;
                }
                match remove(app, &places) {
                    Ok(said) => println!("{said}"),
                    Err(e) => {
                        ok = false;
                        eprintln!("{e}");
                    }
                }
            }
        }
        _ => {
            eprintln!("usage: mcp install APP ... | --all, mcp uninstall APP ..., mcp status");
            return ExitCode::FAILURE;
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn computer() -> (tempfile::TempDir, Places) {
        let dir = tempfile::tempdir().unwrap();
        let places = Places {
            home: dir.path().to_path_buf(),
            env: HashMap::new(),
        };
        (dir, places)
    }

    fn ping() -> Server {
        Server {
            command: "/Applications/Ping.app/Contents/MacOS/Ping".into(),
            args: vec!["mcp".into()],
        }
    }

    fn read(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn an_agent_that_is_not_here_is_left_alone() {
        let (_d, places) = computer();
        for app in App::ALL {
            assert_eq!(state(app, &places, &ping()), State::Missing, "{app:?}");
            assert!(add(app, &places, &ping()).is_err());
        }
        assert!(std::fs::read_dir(&places.home).unwrap().next().is_none());
    }

    #[test]
    fn claude_code_keeps_everything_else_in_its_file() {
        let (_d, places) = computer();
        let file = places.home.join(".claude.json");
        std::fs::write(
            &file,
            "{\n    \"numStartups\": 4,\n    \"mcpServers\": {\n        \"other\": {\"command\": \"x\"}\n    },\n    \"zeta\": true\n}\n",
        )
        .unwrap();
        assert_eq!(state(App::ClaudeCode, &places, &ping()), State::Absent);
        add(App::ClaudeCode, &places, &ping()).unwrap();
        assert_eq!(
            state(App::ClaudeCode, &places, &ping()),
            State::Added { current: true }
        );
        let text = std::fs::read_to_string(&file).unwrap();
        let keys: Vec<String> = read(&file).as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["numStartups", "mcpServers", "zeta"], "order kept");
        assert!(text.contains("\n    \"numStartups\""), "indentation kept");
        let doc = read(&file);
        assert_eq!(doc["mcpServers"]["other"]["command"], "x");
        assert_eq!(doc["mcpServers"]["pingpong"]["type"], "stdio");
        assert_eq!(doc["mcpServers"]["pingpong"]["args"][0], "mcp");

        remove(App::ClaudeCode, &places).unwrap();
        assert_eq!(state(App::ClaudeCode, &places, &ping()), State::Absent);
        assert_eq!(read(&file)["mcpServers"]["other"]["command"], "x");
    }

    #[test]
    fn claude_config_dir_moves_claude_code_settings() {
        let (_d, mut places) = computer();
        let moved = places.home.join("profile");
        std::fs::create_dir_all(&moved).unwrap();
        places
            .env
            .insert("CLAUDE_CONFIG_DIR".into(), moved.display().to_string());
        add(App::ClaudeCode, &places, &ping()).unwrap_err();
        std::fs::write(moved.join(".claude.json"), "{}").unwrap();
        add(App::ClaudeCode, &places, &ping()).unwrap();
        assert!(read(&moved.join(".claude.json"))["mcpServers"]["pingpong"].is_object());
    }

    #[test]
    fn another_copy_of_ping_is_seen_and_replaced() {
        let (_d, places) = computer();
        std::fs::create_dir_all(places.home.join(".cursor")).unwrap();
        let old = Server {
            command: "/old/ping".into(),
            args: vec!["mcp".into()],
        };
        add(App::Cursor, &places, &old).unwrap();
        assert_eq!(
            state(App::Cursor, &places, &ping()),
            State::Added { current: false }
        );
        add(App::Cursor, &places, &ping()).unwrap();
        assert_eq!(
            state(App::Cursor, &places, &ping()),
            State::Added { current: true }
        );
    }

    #[test]
    fn codex_toml_keeps_its_comments_and_tables() {
        let (_d, places) = computer();
        let dir = places.home.join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.toml");
        std::fs::write(
            &file,
            "# my settings\nmodel = \"gpt-5\"\n\n[mcp_servers.other]\ncommand = \"x\"\n",
        )
        .unwrap();
        add(App::Codex, &places, &ping()).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.starts_with("# my settings\nmodel = \"gpt-5\""),
            "{text}"
        );
        assert!(text.contains("[mcp_servers.pingpong]"), "{text}");
        assert!(text.contains("[mcp_servers.other]"), "{text}");
        assert_eq!(
            state(App::Codex, &places, &ping()),
            State::Added { current: true }
        );
        remove(App::Codex, &places).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("pingpong") && text.contains("[mcp_servers.other]"));
    }

    #[test]
    fn a_new_codex_file_gets_one_table_for_the_server() {
        let (_d, places) = computer();
        std::fs::create_dir_all(places.home.join(".codex")).unwrap();
        add(App::Codex, &places, &ping()).unwrap();
        let text = std::fs::read_to_string(places.home.join(".codex/config.toml")).unwrap();
        assert_eq!(
            text,
            "[mcp_servers.pingpong]\ncommand = \"/Applications/Ping.app/Contents/MacOS/Ping\"\nargs = [\"mcp\"]\n"
        );
    }

    #[test]
    fn each_agent_gets_the_shape_it_reads() {
        let (_d, places) = computer();
        for dir in [".config/opencode", ".copilot", ".config/amp", ".factory"] {
            std::fs::create_dir_all(places.home.join(dir)).unwrap();
        }
        for app in [App::OpenCode, App::Copilot, App::Amp, App::Droid] {
            add(app, &places, &ping()).unwrap();
            assert_eq!(
                state(app, &places, &ping()),
                State::Added { current: true },
                "{app:?}"
            );
        }
        let open = read(&places.home.join(".config/opencode/opencode.json"));
        assert_eq!(
            open["mcp"]["pingpong"],
            json!({"type": "local", "command": ["/Applications/Ping.app/Contents/MacOS/Ping", "mcp"],
                "enabled": true, "timeout": 60000})
        );
        let copilot = read(&places.home.join(".copilot/mcp-config.json"));
        assert_eq!(copilot["mcpServers"]["pingpong"]["tools"], json!(["*"]));
        let amp = read(&places.home.join(".config/amp/settings.json"));
        assert!(amp["amp.mcpServers"]["pingpong"].is_object(), "a flat key");
        let droid = read(&places.home.join(".factory/mcp.json"));
        assert_eq!(droid["mcpServers"]["pingpong"]["type"], "stdio");
    }

    #[test]
    fn a_file_with_comments_is_not_rewritten() {
        let (_d, places) = computer();
        let dir = places.home.join(".config/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        let jsonc = "{\n  // mine\n  \"theme\": \"dark\"\n}\n";
        std::fs::write(dir.join("opencode.jsonc"), jsonc).unwrap();
        assert!(matches!(
            state(App::OpenCode, &places, &ping()),
            State::Unreadable(_)
        ));
        assert!(add(App::OpenCode, &places, &ping()).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("opencode.jsonc")).unwrap(),
            jsonc
        );
    }

    #[test]
    fn diri_accounts_with_homes_of_their_own_get_it() {
        let (_d, mut places) = computer();
        let diri = places.home.join("diri");
        std::fs::create_dir_all(&diri).unwrap();
        places
            .env
            .insert("DIRI_APP_SUPPORT".into(), diri.display().to_string());
        assert!(matches!(
            state(App::Diri, &places, &ping()),
            State::Shared(_)
        ));
        let codex = places.home.join("accounts/work-codex");
        let claude = places.home.join("accounts/work-claude");
        let accounts = json!({"profiles": [
            {"id": "a", "label": "Work", "agent": "codex", "config_home": codex},
            {"id": "b", "label": "Work", "agent": "claude-code", "config_home": claude},
            {"id": "c", "label": "Shared", "agent": "claude-code",
                "config_home": places.home.join(".claude"), "login_store": "/x"},
            {"id": "d", "label": "Remote", "agent": "codex", "config_home": "/r", "host": "box"},
        ]});
        std::fs::write(diri.join("accounts.json"), accounts.to_string()).unwrap();
        assert_eq!(state(App::Diri, &places, &ping()), State::Absent);
        add(App::Diri, &places, &ping()).unwrap();
        assert!(std::fs::read_to_string(codex.join("config.toml"))
            .unwrap()
            .contains("[mcp_servers.pingpong]"));
        assert!(read(&claude.join(".claude.json"))["mcpServers"]["pingpong"].is_object());
        assert!(!Path::new("/r/config.toml").exists());
        assert_eq!(
            state(App::Diri, &places, &ping()),
            State::Added { current: true }
        );
        remove(App::Diri, &places).unwrap();
        assert_eq!(state(App::Diri, &places, &ping()), State::Absent);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_settings_file_is_the_owners_alone_and_an_old_one_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, places) = computer();
        std::fs::create_dir_all(places.home.join(".gemini")).unwrap();
        add(App::Gemini, &places, &ping()).unwrap();
        let file = places.home.join(".gemini/settings.json");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        remove(App::Gemini, &places).unwrap();
        assert_eq!(mode(&file), 0o644);
    }

    #[test]
    fn apps_are_named_as_people_type_them() {
        assert_eq!(App::parse("claude"), Some(App::ClaudeCode));
        assert_eq!(App::parse("Claude Code"), Some(App::ClaudeCode));
        assert_eq!(App::parse("opencode"), Some(App::OpenCode));
        assert_eq!(App::parse("vim"), None);
    }
}
