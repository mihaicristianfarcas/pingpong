//! Subscriptions: the maker's own agent, run headless with pingpong's MCP
//! server as its only tools.
//!
//! - **Codex** (`codex exec --json`): its shell, its own computer use and
//!   browser, apps and plugins are switched off and the user's Codex config
//!   is not read, so the only thing it can touch is the pingpong host.
//!   pingpong's tools are approved up front (the run is the user's request).
//! - **Claude Code** (`claude -p --output-format stream-json`): no built-in
//!   tools (`--tools ""`), only pingpong's (`--allowedTools mcp__pingpong`),
//!   no other MCP server (`--strict-mcp-config`), no session kept.
//!
//! The MCP server is this program again (`mcp`), writing what it does to an
//! events file the run follows for the action log and its thumbnails.
//! `PING_AGENT_MCP` replaces that command (tests run the server elsewhere,
//! e.g. `tools/linux/agent-desktop mcp`), and `PING_AGENT_PATH_MAP=LOCAL=REMOTE`
//! says where the events file is as that server sees it.

use std::io::{BufRead, BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use super::{AgentSettings, RunContext, RunEvent};
use crate::runner::Resume;

/// A Codex program found here.
#[derive(Debug, Clone)]
pub struct Program {
    pub path: PathBuf,
    pub version: String,
}

fn version_of(path: &Path) -> Option<String> {
    let out = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

fn version_key(v: &str) -> Vec<u32> {
    v.split(['.', '-'])
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

fn on_path(name: &str) -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join(name))
                .filter(|p| p.is_file())
                .collect()
        })
        .unwrap_or_default()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// The newest Codex here: the one set in settings, else on PATH or the one
/// Codex's own app server keeps up to date. (Codex 0.157 could not call MCP
/// tools at all -- its code-mode host timed out -- so the newest wins.)
pub fn codex_program(settings: &AgentSettings) -> Result<Program, String> {
    if !settings.codex_path.trim().is_empty() {
        let path = PathBuf::from(settings.codex_path.trim());
        let version =
            version_of(&path).ok_or_else(|| format!("{} does not run", path.display()))?;
        return Ok(Program { path, version });
    }
    let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
    let mut found: Vec<PathBuf> = on_path(exe);
    for extra in ["/opt/homebrew/bin/codex", "/usr/local/bin/codex"] {
        found.push(PathBuf::from(extra));
    }
    if let Ok(rd) = std::fs::read_dir(home().join(".codex/packages/app-server-daemon/releases")) {
        for e in rd.flatten() {
            found.push(e.path().join("bin").join(exe));
        }
    }
    found
        .into_iter()
        .filter(|p| p.is_file())
        .filter_map(|p| {
            version_of(&p).map(|v| Program {
                path: p,
                version: v,
            })
        })
        .max_by_key(|p| version_key(&p.version))
        .ok_or_else(|| {
            "Codex is not installed (https://developers.openai.com/codex); sign \
                in with `codex login`."
                .to_string()
        })
}

/// Claude Code: the one set in settings, else on PATH or where its
/// installer puts it.
pub fn claude_program(settings: &AgentSettings) -> Result<PathBuf, String> {
    if !settings.claude_path.trim().is_empty() {
        return Ok(PathBuf::from(settings.claude_path.trim()));
    }
    let exe = if cfg!(windows) {
        "claude.exe"
    } else {
        "claude"
    };
    on_path(exe)
        .into_iter()
        .chain([
            home().join(".local/bin").join(exe),
            home().join(".claude/local").join(exe),
        ])
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "Claude Code is not installed (https://claude.com/claude-code); \
                sign in by running `claude` once."
                .to_string()
        })
}

/// Where a run keeps its events, thumbnails and control folder.
pub fn run_dir(data_dir: &Path) -> Result<PathBuf, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = ping_core::store::agent_dir(data_dir)
        .join("runs")
        .join(stamp.to_string());
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// The MCP server's command line for this run.
fn mcp_command(ctx: &RunContext, events: &Path) -> Result<(String, Vec<String>), String> {
    let map = std::env::var("PING_AGENT_PATH_MAP").ok().and_then(|m| {
        m.split_once('=')
            .map(|(a, b)| (a.to_string(), b.to_string()))
    });
    let remote = |p: &Path| {
        let p = p.display().to_string();
        match &map {
            Some((local, remote)) if p.starts_with(local.as_str()) => {
                format!("{remote}{}", &p[local.len()..])
            }
            _ => p,
        }
    };
    let control = remote(&ctx.control_dir());
    let events = remote(events);
    let mut args: Vec<String> = vec![
        "mcp".to_string(),
        "--host".into(),
        ctx.host.clone(),
        "--events".into(),
        events,
        "--max-actions".into(),
        ctx.settings.max_actions.to_string(),
        "--size".into(),
        format!("{}x{}", ctx.settings.width, ctx.settings.height),
        "--until".into(),
        ctx.until_epoch().to_string(),
        "--hold-wait".into(),
        crate::computer::HOLD_WAIT.as_secs().to_string(),
        "--control-dir".into(),
        control,
    ];
    args.extend([
        "--approvals".to_string(),
        ctx.settings.approvals.id().to_string(),
    ]);
    // The server reads Jev's key itself (saved, or its variable): a key is
    // never on a command line.
    if ctx.settings.jev.check_clicks {
        args.push("--jev-clicks".into());
    }
    // A server on this machine also stops if this process dies (one run
    // elsewhere, PING_AGENT_MCP, cannot see it), and is told where Ping's
    // data is: Codex starts MCP servers with an environment of its own,
    // without PING_DATA_DIR. (A server elsewhere keeps its own.)
    if std::env::var("PING_AGENT_MCP").is_err() {
        args.extend(["--owner-pid".to_string(), std::process::id().to_string()]);
        args.extend(["--data-dir".to_string(), ctx.data_dir.display().to_string()]);
    }
    if let Ok(prefix) = std::env::var("PING_AGENT_MCP") {
        let mut words: Vec<String> = prefix.split_whitespace().map(str::to_string).collect();
        if words.is_empty() {
            return Err("PING_AGENT_MCP is empty".into());
        }
        let program = words.remove(0);
        // The prefix names its own subcommand ("... mcp").
        if words.last().is_some_and(|w| w == "mcp") {
            args.remove(0);
        }
        words.extend(args);
        return Ok((program, words));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok((exe.display().to_string(), args))
}

/// Follows the MCP server's events file, turning each line into an action
/// event (with its thumbnail read back).
struct EventTail {
    path: PathBuf,
    pos: u64,
    remote_prefix: Option<(String, String)>,
}

impl EventTail {
    fn new(path: PathBuf) -> EventTail {
        let map = std::env::var("PING_AGENT_PATH_MAP").ok().and_then(|m| {
            m.split_once('=')
                .map(|(a, b)| (a.to_string(), b.to_string()))
        });
        EventTail {
            path,
            pos: 0,
            remote_prefix: map,
        }
    }

    fn poll(&mut self, ctx: &RunContext) {
        let Ok(mut f) = std::fs::File::open(&self.path) else {
            return;
        };
        if f.seek(std::io::SeekFrom::Start(self.pos)).is_err() {
            return;
        }
        let mut text = String::new();
        if f.read_to_string(&mut text).is_err() {
            return;
        }
        // Only whole lines.
        let Some(end) = text.rfind('\n') else { return };
        self.pos += end as u64 + 1;
        for line in text[..end].lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            // Waited for a person (in the MCP server's process): not the
            // run's time.
            if let Some(ms) = v["held_ms"].as_u64() {
                ctx.waits.add_waited(Duration::from_millis(ms));
            }
            if let Some(plan) = v.get("plan") {
                if let Ok(steps) = serde_json::from_value(plan.clone()) {
                    ctx.emit(RunEvent::Plan(steps));
                }
                continue;
            }
            let thumb = v["thumbnail"].as_str().map(|p| match &self.remote_prefix {
                Some((local, remote)) if p.starts_with(remote.as_str()) => {
                    format!("{local}{}", &p[remote.len()..])
                }
                _ => p.to_string(),
            });
            ctx.emit(RunEvent::Action {
                text: v["action"].as_str().unwrap_or_default().to_string(),
                ok: v["ok"].as_bool().unwrap_or(true),
                detail: v["text"].as_str().unwrap_or_default().to_string(),
                thumbnail: thumb.and_then(|p| std::fs::read(p).ok()),
            });
        }
    }
}

/// Run `child`, handing each stdout line to `on_line`, following the events
/// file, and killing it if the run is stopped. Returns its exit status and
/// the tail of its stderr.
fn supervise(
    mut child: Child,
    ctx: &RunContext,
    events: &Path,
    mut on_line: impl FnMut(&Value),
) -> (Option<i32>, String) {
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let (tx, rx) = crossbeam_channel::unbounded::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let err_tail = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
    {
        let err_tail = err_tail.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut t = err_tail.lock();
                t.push_str(&line);
                t.push('\n');
                let len = t.len();
                if len > 8000 {
                    let cut = t
                        .char_indices()
                        .map(|(i, _)| i)
                        .find(|&i| i >= len - 4000)
                        .unwrap_or(0);
                    t.drain(..cut);
                }
            }
        });
    }
    let mut tail = EventTail::new(events.to_path_buf());
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    on_line(&v);
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        tail.poll(ctx);
        if ctx.stopped() {
            let _ = child.kill();
            break;
        }
    }
    let status = child.wait().ok().and_then(|s| s.code());
    tail.poll(ctx);
    let err = err_tail.lock().clone();
    (status, err)
}

/// Codex's to-do list as a plan: done items done, the first open one in
/// progress.
fn codex_plan(item: &Value) -> Vec<crate::computer::PlanStep> {
    use crate::computer::{PlanStatus, PlanStep};
    let mut current = false;
    item["items"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|i| {
                    let text = i["text"].as_str()?.trim().to_string();
                    let status = if i["completed"].as_bool().unwrap_or(false) {
                        PlanStatus::Done
                    } else if !current {
                        current = true;
                        PlanStatus::InProgress
                    } else {
                        PlanStatus::Pending
                    };
                    (!text.is_empty()).then_some(PlanStep { text, status })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// How long the model's program lets one of pingpong's tools run: an action
/// can wait for a person for `HOLD_WAIT`, then settles and looks.
fn tool_timeout() -> Duration {
    crate::computer::HOLD_WAIT + Duration::from_secs(5 * 60)
}

fn task_prompt(ctx: &RunContext) -> String {
    if ctx.session.is_some() {
        return turn_prompt(ctx, false);
    }
    format!(
        "{}\n\nUse the pingpong tools (they connect to \"{}\" by themselves). Task:\n{}",
        super::prompt_for(ctx),
        ctx.host,
        ctx.task
    )
}

/// A conversation's message: the first with who the model is and what it
/// has (unless the program is told that apart, `system_told`), later ones
/// as they are, with a reminder to look before acting.
fn turn_prompt(ctx: &RunContext, system_told: bool) -> String {
    let resumed = ctx
        .session
        .as_ref()
        .is_some_and(|l| l.continuity.lock().resume != Resume::Fresh);
    if resumed {
        format!(
            "The user says:\n{}\n\n(They may have used the computer since your last \
                turn: look before you act on it.)",
            ctx.task
        )
    } else if system_told {
        format!(
            "The pingpong tools connect to \"{}\" by themselves. The user says:\n{}",
            ctx.host, ctx.task
        )
    } else {
        format!(
            "{}\n\nThe pingpong tools connect to \"{}\" by themselves. The user says:\n{}",
            super::prompt_for(ctx),
            ctx.host,
            ctx.task
        )
    }
}

/// A fresh random UUID (v4), for a conversation's Claude Code session.
fn uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the system's random source");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Run the task with Codex.
pub fn run_codex(ctx: &RunContext) -> Result<String, String> {
    let program = codex_program(&ctx.settings)?;
    let dir = ctx.run_dir.clone();
    let events = dir.join("events.jsonl");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let toml_str = |s: &str| serde_json::to_string(s).expect("a string");
    let session = ctx.session.as_ref();
    let resume = session
        .map(|l| l.continuity.lock().resume.clone())
        .unwrap_or_default();
    let mut cmd = Command::new(&program.path);
    cmd.current_dir(&work);
    match &resume {
        Resume::Codex(thread) => {
            ctx.emit(RunEvent::Status(format!(
                "Codex {} goes on…",
                program.version
            )));
            tracing::info!(codex = %program.path.display(), version = program.version, thread, "Codex resumes the conversation");
            cmd.args([
                "exec",
                "resume",
                "--json",
                "--skip-git-repo-check",
                "--ignore-user-config",
                "-c",
                "sandbox_mode=\"read-only\"",
            ]);
        }
        _ => {
            ctx.emit(RunEvent::Status(format!(
                "Starting Codex {}…",
                program.version
            )));
            tracing::info!(codex = %program.path.display(), version = program.version, conversation = session.is_some(), "Codex starts");
            cmd.args(["exec", "--json", "--skip-git-repo-check"]);
            // A task's thread is thrown away; a conversation's is resumed.
            if session.is_none() {
                cmd.arg("--ephemeral");
            }
            cmd.args(["--ignore-user-config", "--sandbox", "read-only", "-C"])
                .arg(&work);
        }
    }
    for feature in [
        "shell_tool",
        "unified_exec",
        "computer_use",
        "browser_use",
        "browser_use_external",
        "in_app_browser",
        "apps",
        "plugins",
        "image_generation",
        "multi_agent",
    ] {
        cmd.args(["--disable", feature]);
    }
    match session {
        // The conversation's own endpoint: its connection outlives the turn.
        // The token goes by environment, never on a command line.
        Some(link) => {
            cmd.arg("-c").arg(format!(
                "mcp_servers.pingpong.url={}",
                toml_str(&link.mcp_url)
            ));
            cmd.args([
                "-c",
                "mcp_servers.pingpong.bearer_token_env_var=\"PINGPONG_MCP_TOKEN\"",
            ]);
            cmd.env("PINGPONG_MCP_TOKEN", &link.mcp_token);
        }
        None => {
            let (mcp, mcp_args) = mcp_command(ctx, &events)?;
            cmd.arg("-c")
                .arg(format!("mcp_servers.pingpong.command={}", toml_str(&mcp)));
            cmd.arg("-c").arg(format!(
                "mcp_servers.pingpong.args=[{}]",
                mcp_args
                    .iter()
                    .map(|a| toml_str(a))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
    }
    cmd.args([
        "-c",
        "mcp_servers.pingpong.default_tools_approval_mode=\"approve\"",
    ]);
    // A call may wait for a person who took over (`computer::HOLD_WAIT`).
    cmd.arg("-c").arg(format!(
        "mcp_servers.pingpong.tool_timeout_sec={}",
        tool_timeout().as_secs()
    ));
    cmd.args(["-c", "mcp_servers.pingpong.startup_timeout_sec=90"]);
    let effort = match ctx.settings.effort.as_str() {
        e @ ("low" | "medium" | "high" | "xhigh") => e,
        _ => "medium",
    };
    cmd.arg("-c")
        .arg(format!("model_reasoning_effort=\"{effort}\""));
    let model = ctx.settings.model.trim();
    if !model.is_empty() {
        cmd.args(["-m", model]);
    }
    if let Resume::Codex(thread) = &resume {
        cmd.arg(thread);
    }
    cmd.arg(task_prompt(ctx));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", program.path.display()))?;
    tracing::info!(pid = child.id(), "Codex running");
    let mut last_message = String::new();
    let mut failure: Option<String> = None;
    let mut totals = (0u64, 0u64, 0u64);
    let (status, stderr) = supervise(child, ctx, &events, |v| match v["type"].as_str() {
        Some("thread.started") => {
            if let (Some(link), Some(id)) = (session, v["thread_id"].as_str()) {
                tracing::info!(thread = id, "Codex thread");
                link.continuity.lock().resume = Resume::Codex(id.to_string());
            }
        }
        // Codex's own plan (its to-do list), as it changes.
        Some("item.started" | "item.updated" | "item.completed")
            if v["item"]["type"] == "todo_list" =>
        {
            ctx.emit(RunEvent::Plan(codex_plan(&v["item"])));
        }
        Some("item.completed") => {
            let item = &v["item"];
            match item["type"].as_str() {
                Some("agent_message") => {
                    let text = item["text"].as_str().unwrap_or_default().trim().to_string();
                    if !text.is_empty() {
                        ctx.emit(RunEvent::Thought(text.clone()));
                        last_message = text;
                    }
                }
                Some("mcp_tool_call") => {
                    // Refused before reaching the server (approval, a wrong
                    // tool): the events file never hears of it.
                    if let Some(msg) = item["error"]["message"].as_str() {
                        ctx.emit(RunEvent::Action {
                            text: item["tool"].as_str().unwrap_or("tool").to_string(),
                            ok: false,
                            detail: msg.to_string(),
                            thumbnail: None,
                        });
                    }
                }
                Some("error") => ctx.emit(RunEvent::Status(
                    item["message"].as_str().unwrap_or_default().to_string(),
                )),
                _ => {}
            }
        }
        Some("turn.completed") => {
            let u = &v["usage"];
            totals.0 += u["input_tokens"].as_u64().unwrap_or(0);
            totals.1 += u["output_tokens"].as_u64().unwrap_or(0);
            totals.2 += u["cached_input_tokens"].as_u64().unwrap_or(0);
            ctx.emit(RunEvent::Usage {
                input: totals.0,
                output: totals.1,
                cached: totals.2,
                cost_usd: None,
            });
        }
        Some("turn.failed") => {
            failure = Some(
                v["error"]["message"]
                    .as_str()
                    .unwrap_or("the turn failed")
                    .to_string(),
            )
        }
        Some("error") => failure = Some(v["message"].as_str().unwrap_or("error").to_string()),
        _ => {}
    });
    tracing::info!(status = ?status, input = totals.0, output = totals.1, cached = totals.2, "Codex exited");
    if let Some(reason) = ctx.stop_reason() {
        return Err(reason);
    }
    if let Some(f) = failure {
        tracing::warn!(
            failure = f,
            stderr = stderr.lines().last().unwrap_or(""),
            "Codex failed"
        );
        return Err(format!("Codex: {f}"));
    }
    match status {
        Some(0) if !last_message.is_empty() => Ok(last_message),
        Some(0) => Ok("Codex finished without a summary.".into()),
        other => Err(format!(
            "Codex exited with {}: {}",
            other.map_or("a signal".into(), |c| c.to_string()),
            stderr
                .lines()
                .rfind(|l| !l.starts_with("Reading"))
                .unwrap_or("")
        )),
    }
}

/// Run the task with Claude Code.
pub fn run_claude(ctx: &RunContext) -> Result<String, String> {
    let program = claude_program(&ctx.settings)?;
    let dir = ctx.run_dir.clone();
    let events = dir.join("events.jsonl");
    let work = dir.join("work");
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let session = ctx.session.as_ref();
    let mut cmd = Command::new(&program);
    cmd.current_dir(&work);
    match session {
        Some(link) => {
            // The conversation's endpoint, its token in a file only this
            // user reads (never on a command line); the session kept, and
            // resumed from the second turn on.
            let config = json!({"mcpServers": {"pingpong": {"type": "http", "url": link.mcp_url,
                "headers": {"Authorization": format!("Bearer {}", link.mcp_token)}}}});
            let path = dir.join("claude-mcp.json");
            pingpong_transport::identity::write_private(&path, config.to_string().as_bytes())
                .map_err(|e| format!("{}: {e}", path.display()))?;
            let resume = link.continuity.lock().resume.clone();
            let prompt = turn_prompt(ctx, true);
            match resume {
                Resume::Claude(id) => {
                    ctx.emit(RunEvent::Status("Claude Code goes on…".into()));
                    tracing::info!(session = id, "Claude Code resumes the conversation");
                    cmd.args(["--resume", &id]);
                }
                _ => {
                    let id = uuid();
                    ctx.emit(RunEvent::Status("Starting Claude Code…".into()));
                    tracing::info!(session = id, "Claude Code starts a conversation");
                    cmd.args(["--session-id", &id]);
                    link.continuity.lock().resume = Resume::Claude(id);
                }
            }
            cmd.args(["-p", &prompt])
                .args([
                    "--output-format",
                    "stream-json",
                    "--verbose",
                    "--strict-mcp-config",
                ])
                .arg("--mcp-config")
                .arg(&path)
                .arg("--append-system-prompt")
                .arg(super::prompt_for(ctx));
        }
        None => {
            let (mcp, mcp_args) = mcp_command(ctx, &events)?;
            ctx.emit(RunEvent::Status("Starting Claude Code…".into()));
            let config = json!({"mcpServers": {"pingpong": {"type": "stdio", "command": mcp, "args": mcp_args}}});
            cmd.args([
                "-p",
                &format!(
                    "Use the pingpong tools (they connect to \"{}\" by themselves). Task:\n{}",
                    ctx.host, ctx.task
                ),
            ])
            .args([
                "--output-format",
                "stream-json",
                "--verbose",
                "--no-session-persistence",
                "--strict-mcp-config",
            ])
            .arg("--mcp-config")
            .arg(config.to_string())
            .arg("--append-system-prompt")
            .arg(super::prompt_for(ctx));
        }
    }
    cmd.args([
        "--tools",
        "",
        "--allowedTools",
        "mcp__pingpong",
        "--permission-mode",
        "dontAsk",
    ]);
    // A call may wait for a person who took over (`computer::HOLD_WAIT`).
    cmd.env("MCP_TOOL_TIMEOUT", tool_timeout().as_millis().to_string());
    let model = ctx.settings.model.trim();
    if !model.is_empty() {
        cmd.args(["--model", model]);
    }
    if matches!(
        ctx.settings.effort.as_str(),
        "low" | "medium" | "high" | "xhigh" | "max"
    ) {
        cmd.args(["--effort", &ctx.settings.effort]);
    }
    // Started from inside another Claude Code session (a developer's), it
    // must not think it is that session's child.
    for var in [
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SSE_PORT",
    ] {
        cmd.env_remove(var);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd
        .spawn()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    tracing::info!(pid = child.id(), claude = %program.display(), "Claude Code running");
    let mut result: Option<Result<String, String>> = None;
    let mut last_text = String::new();
    let (status, stderr) = supervise(child, ctx, &events, |v| match v["type"].as_str() {
        Some("assistant") => {
            for block in v["message"]["content"].as_array().into_iter().flatten() {
                if block["type"] == "text" {
                    let text = block["text"]
                        .as_str()
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    if !text.is_empty() {
                        ctx.emit(RunEvent::Thought(text.clone()));
                        last_text = text;
                    }
                }
            }
        }
        Some("result") => {
            let u = &v["usage"];
            ctx.emit(RunEvent::Usage {
                input: u["input_tokens"].as_u64().unwrap_or(0)
                    + u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
                output: u["output_tokens"].as_u64().unwrap_or(0),
                cached: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
                cost_usd: v["total_cost_usd"].as_f64(),
            });
            let text = v["result"].as_str().unwrap_or_default().to_string();
            result = Some(
                if v["is_error"].as_bool().unwrap_or(false) || v["subtype"] != "success" {
                    Err(format!(
                        "Claude Code: {}",
                        if text.is_empty() {
                            v["subtype"].as_str().unwrap_or("error").to_string()
                        } else {
                            text
                        }
                    ))
                } else {
                    Ok(text)
                },
            );
        }
        _ => {}
    });
    if let Some(reason) = ctx.stop_reason() {
        return Err(reason);
    }
    match result {
        Some(Ok(text)) if !text.trim().is_empty() => Ok(text),
        Some(Ok(_)) if !last_text.is_empty() => Ok(last_text),
        Some(r) => r,
        None => Err(format!(
            "Claude Code exited ({}) without a result: {}",
            status.map_or("a signal".into(), |c| c.to_string()),
            stderr.lines().last().unwrap_or("")
        )),
    }
}

#[cfg(test)]
mod plan_tests {
    use super::*;
    use crate::computer::PlanStatus;

    #[test]
    fn codex_todo_lists_become_plans() {
        let item = serde_json::json!({"type": "todo_list", "items": [
            {"text": "Open Notepad", "completed": true},
            {"text": "Type the list", "completed": false},
            {"text": "Save it", "completed": false}]});
        let plan = codex_plan(&item);
        assert_eq!(
            plan.iter().map(|s| s.status).collect::<Vec<_>>(),
            [
                PlanStatus::Done,
                PlanStatus::InProgress,
                PlanStatus::Pending
            ]
        );
        assert_eq!(plan[1].text, "Type the list");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(version_key("0.158.0") > version_key("0.157.1"));
        assert!(version_key("0.160.0") > version_key("0.99.9"));
        assert!(version_key("1.0.0-beta") > version_key("0.200.0"));
    }
}
