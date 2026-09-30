//! A conversation with an agent about one host: the user and the model take
//! turns, and the model uses the host when a message needs it. The host
//! connection is the conversation's -- made at the first action (or when the
//! user asks to watch), kept between turns, ended with the conversation -- so
//! the user can watch or take over at any time, and the next turn finds the
//! desktop as the last one (or the user) left it.
//!
//! Codex and Claude Code reach the connection over a local MCP endpoint of
//! the conversation's own (HTTP on localhost, a token of its own) and go on
//! with their own thread from turn to turn; API keys' models are told the
//! conversation so far. Nothing is kept on disk beyond the conversation's
//! folder of working files, removed when it ends.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::computer::{Computer, Config, LinkStatus, Observer, PlanObserver, Waits};
use crate::mcp::{self, HttpServer, SharedComputer};
use crate::providers::{AgentSettings, Confirm, Events};
use crate::runner::{Continuity, Run, SessionLink, Task};

pub struct Conversation {
    pub host: String,
    computer: SharedComputer,
    /// The computer's waits for people, reachable without its lock.
    waits: Waits,
    http: HttpServer,
    dir: PathBuf,
    data_dir: PathBuf,
    continuity: Arc<parking_lot::Mutex<Continuity>>,
    run: Option<Run>,
}

impl Conversation {
    /// Open a conversation about `host`; `on_action` hears each action the
    /// model takes, with the screen after it as the model saw it.
    pub fn open(
        data_dir: &Path,
        host: &str,
        settings: &AgentSettings,
        on_action: Observer,
        on_plan: PlanObserver,
    ) -> Result<Conversation, String> {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dir = ping_core::store::agent_dir(data_dir)
            .join("conversations")
            .join(stamp.to_string());
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let mut config = Config::new(data_dir.to_path_buf());
        config.default_host = Some(host.to_string());
        config.session.width = settings.width;
        config.session.height = settings.height;
        config.full_screens = true;
        // A person who logs in and takes over may take a while.
        config.hold_wait = crate::computer::HOLD_WAIT;
        // Nothing acts outside a turn.
        config.until = Some(0);
        let mut computer = Computer::new(config);
        computer.observe(on_action);
        computer.observe_plan(on_plan);
        let waits = computer.waits();
        let computer: SharedComputer = Arc::new(parking_lot::Mutex::new(computer));
        let http = mcp::serve_http(
            computer.clone(),
            mcp::Options {
                image_dir: None,
                events: None,
            },
        )
        .map_err(|e| e.to_string())?;
        tracing::info!(host, dir = %dir.display(), "conversation opened");
        Ok(Conversation {
            host: host.to_string(),
            computer,
            waits,
            http,
            dir,
            data_dir: data_dir.to_path_buf(),
            continuity: Arc::new(parking_lot::Mutex::new(Continuity::default())),
            run: None,
        })
    }

    /// The user's next message: a turn, with its own budget, on the shared
    /// connection. `events` hears the turn (ending with `Finished` or
    /// `Failed`); `confirm` is asked what needs a person's yes.
    pub fn send(
        &mut self,
        message: &str,
        settings: &AgentSettings,
        events: Events,
        confirm: Confirm,
    ) -> Result<(), String> {
        if self.busy() {
            return Err("The agent is still on the last message.".into());
        }
        let link = SessionLink {
            computer: self.computer.clone(),
            mcp_url: self.http.url.clone(),
            mcp_token: self.http.token.clone(),
            dir: self.dir.clone(),
            continuity: self.continuity.clone(),
            waits: self.waits.clone(),
        };
        tracing::info!(
            host = self.host,
            provider = settings.provider.id(),
            chars = message.len(),
            "message to the agent"
        );
        self.run = Some(Run::start(
            Task {
                data_dir: self.data_dir.clone(),
                host: self.host.clone(),
                task: message.to_string(),
                settings: settings.clone(),
                session: Some(link),
            },
            events,
            confirm,
        ));
        Ok(())
    }

    /// An action waits for a person to give the keyboard and mouse back.
    pub fn waiting_for_person(&self) -> bool {
        self.waits.waiting()
    }

    /// Time this turn waited for people (not counted against its minutes).
    pub fn waited(&self) -> std::time::Duration {
        self.waits.waited()
    }

    /// The connection now, unless an action holds the computer (then
    /// `None`, as when there is none: keep what was seen last).
    pub fn link_status(&self) -> Option<Option<LinkStatus>> {
        self.computer.try_lock().map(|c| c.link_status())
    }

    pub fn busy(&self) -> bool {
        self.run.as_ref().is_some_and(Run::is_running)
    }

    /// Stop the turn in progress (the conversation goes on).
    pub fn stop(&self) {
        // An action waiting for a person ends (and is not done) first.
        self.waits.interrupt();
        if let Some(r) = &self.run {
            tracing::info!(host = self.host, "turn stopped by the user");
            r.stop();
        }
    }

    pub fn set_paused(&self, paused: bool) {
        if let Some(r) = &self.run {
            tracing::info!(host = self.host, paused, "turn paused");
            r.set_paused(paused);
        }
    }

    /// The connection, to use from a thread of the caller's (connecting
    /// takes a moment).
    pub fn computer(&self) -> SharedComputer {
        self.computer.clone()
    }

    /// Whether the host is connected; None while an action holds it.
    pub fn connected(&self) -> Option<bool> {
        self.computer.try_lock().map(|c| c.session().is_some())
    }

    /// Continue a model program's own thread (whether one is kept).
    pub fn continues(&self) -> bool {
        self.continuity.lock().resume != crate::runner::Resume::Fresh
    }

    /// End the conversation: the turn stops, the host is let go, the
    /// endpoint closes, the working files go.
    pub fn end(mut self) {
        tracing::info!(host = self.host, "conversation ends");
        self.waits.interrupt();
        if let Some(run) = self.run.take() {
            run.stop();
            run.join();
        }
        self.http.stop();
        self.computer.lock().disconnect();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Open a connection for `Conversation::computer` (a thread's job).
pub fn connect(computer: &SharedComputer) -> Result<(), String> {
    let mut c = computer.lock();
    if c.session().is_some() {
        return Ok(());
    }
    c.connect(None, None).map(|_| ())
}
