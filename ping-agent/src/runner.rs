//! A run: one task, one host, one provider, on a thread of its own, with a
//! time and an action budget, stoppable at any moment. Everything it does
//! comes out as [`RunEvent`]s (for the app's agent panel or the CLI).
//!
//! A run may be a turn of a conversation ([`SessionLink`]): then the host
//! connection is the conversation's and outlives the turn, Codex and Claude
//! Code reach it over the session's local MCP endpoint and continue their own
//! thread, and the API loops are told what was said before.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::computer::{Computer, Config};
use crate::judge::Judge;
use crate::mcp::SharedComputer;
use crate::providers::{self, AgentSettings, Confirm, Events, Provider, RunContext, RunEvent};

pub struct Run {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    control: Option<PathBuf>,
}

pub struct Task {
    pub data_dir: PathBuf,
    pub host: String,
    pub task: String,
    pub settings: AgentSettings,
    /// A turn of a conversation: its connection, endpoint and memory.
    pub session: Option<SessionLink>,
}

/// What a conversation's turns share.
#[derive(Clone)]
pub struct SessionLink {
    /// Connected to the host (or connecting at the first action) for the
    /// whole conversation.
    pub computer: SharedComputer,
    /// The computer's tools over HTTP on localhost, for Codex and Claude Code.
    pub mcp_url: String,
    pub mcp_token: String,
    /// Where the conversation keeps its files (control folder, Codex's
    /// working folder, Claude Code's MCP config).
    pub dir: PathBuf,
    pub continuity: Arc<parking_lot::Mutex<Continuity>>,
    /// The computer's waits for people (see `computer::Waits`).
    pub waits: crate::computer::Waits,
}

/// How the next turn continues the conversation.
#[derive(Debug, Clone, Default)]
pub struct Continuity {
    /// The model program's own thread, when it keeps one.
    pub resume: Resume,
    /// The exchanges so far (the user's words, the reply), for models told
    /// the conversation rather than resuming it.
    pub history: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum Resume {
    #[default]
    Fresh,
    /// `codex exec resume THREAD`.
    Codex(String),
    /// `claude --resume SESSION` (the id is chosen before the first turn).
    Claude(String),
}

impl Run {
    /// Start `task`; `events` hears everything, ending with `Finished` or
    /// `Failed`; `confirm` is asked when the model wants a person's go-ahead.
    pub fn start(task: Task, events: Events, confirm: Confirm) -> Run {
        let stop = Arc::new(AtomicBool::new(false));
        let run_dir = match &task.session {
            Some(link) => Ok(link.dir.clone()),
            None => providers::cli::run_dir(&task.data_dir),
        };
        let control = run_dir.as_ref().ok().map(|d| d.join("control"));
        tracing::info!(
            provider = task.settings.provider.id(),
            model = task.settings.model,
            host = task.host,
            conversation = task.session.is_some(),
            approvals = task.settings.approvals.id(),
            "run starts"
        );
        // Steps waiting for the user's yes (the control folder's asks: a
        // risky action, the model's own request, every action, or a screen
        // with personal information), put to `confirm` one at a time.
        if let Some(dir) = control.clone().filter(|_| {
            task.settings.approvals != crate::providers::Approvals::Off
                || task.settings.checks.personal_info
        }) {
            let (stop, events, confirm) = (stop.clone(), events.clone(), confirm.clone());
            std::thread::Builder::new()
                .name("agent-asks".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        for (n, ask) in crate::control::pending(&dir) {
                            events(RunEvent::Confirm(ask.clone()));
                            crate::control::answer(&dir, n, confirm(&ask.question()));
                        }
                        std::thread::sleep(Duration::from_millis(200));
                    }
                })
                .expect("spawning the asks thread");
        }
        let handle = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("agent-run".into())
                .spawn(move || {
                    let run_dir = match run_dir {
                        Ok(d) => d,
                        Err(e) => {
                            events(RunEvent::Failed(e));
                            return;
                        }
                    };
                    let ctx = RunContext {
                        run_dir,
                        deadline: Instant::now()
                            + Duration::from_secs(task.settings.max_minutes.max(1) as u64 * 60),
                        data_dir: task.data_dir,
                        host: task.host,
                        task: task.task,
                        settings: task.settings,
                        events,
                        confirm,
                        stop,
                        waits: task
                            .session
                            .as_ref()
                            .map(|l| l.waits.clone())
                            .unwrap_or_default(),
                        session: task.session,
                    };
                    let started = Instant::now();
                    let result = run(&ctx);
                    // Over: whatever still follows the run lets go.
                    ctx.stop.store(true, Ordering::Relaxed);
                    if let Some(link) = &ctx.session {
                        // An action of a model that is gone may still wait
                        // for a person: it ends here.
                        link.waits.interrupt();
                        link.computer.lock().end_turn();
                        if let Ok(reply) = &result {
                            link.continuity
                                .lock()
                                .history
                                .push((ctx.task.clone(), reply.clone()));
                        }
                    }
                    match &result {
                        Ok(summary) => tracing::info!(
                            secs = started.elapsed().as_secs(),
                            chars = summary.len(),
                            "run finished"
                        ),
                        Err(e) => tracing::warn!(
                            secs = started.elapsed().as_secs(),
                            error = e,
                            "run failed"
                        ),
                    }
                    ctx.emit(match result {
                        Ok(summary) => RunEvent::Finished(summary),
                        Err(e) => RunEvent::Failed(e),
                    });
                })
                .expect("spawning the agent run")
        };
        Run {
            stop,
            handle: Some(handle),
            control,
        }
    }

    /// Hold the agent's next action until resumed (it waits, then hears
    /// why), or let it go on.
    pub fn set_paused(&self, paused: bool) {
        if let Some(dir) = &self.control {
            crate::control::set_paused(dir, paused);
        }
    }

    /// Stop the run (the model's program is killed, the session ends).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // A paused agent must not wait on.
        self.set_paused(false);
    }

    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Wait for it to end.
    pub fn join(mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(ctx: &RunContext) -> Result<String, String> {
    if ctx.task.trim().is_empty() {
        return Err("Say what the agent should do.".into());
    }
    if let Some(link) = &ctx.session {
        return run_turn(ctx, link);
    }
    match ctx.settings.provider {
        Provider::Codex => providers::cli::run_codex(ctx),
        Provider::ClaudeCode => providers::cli::run_claude(ctx),
        api => {
            // The model is ours to drive: connect here, and let every action
            // be seen as it happens.
            let mut config = Config::new(ctx.data_dir.clone());
            config.session.width = ctx.settings.width;
            config.session.height = ctx.settings.height;
            config.max_actions = Some(ctx.settings.max_actions);
            config.control = Some((ctx.control_dir(), ctx.settings.approvals));
            config.until = Some(ctx.until_epoch());
            config.hold_wait = crate::computer::HOLD_WAIT;
            config.judge = Judge::new(&ctx.data_dir, &ctx.settings.checks).map(Arc::new);
            let mut computer = Computer::new(config);
            computer.set_waits(ctx.waits.clone());
            let events = ctx.events.clone();
            computer.observe(Arc::new(move |d| {
                events(RunEvent::Action {
                    text: d.action,
                    ok: d.ok,
                    detail: d.text,
                    thumbnail: d.thumbnail.map(|t| t.png),
                })
            }));
            ctx.emit(RunEvent::Status(format!("Connecting to {}…", ctx.host)));
            computer.connect(
                Some(&ctx.host),
                Some((ctx.settings.width, ctx.settings.height)),
            )?;
            let result = match api {
                Provider::Anthropic => providers::anthropic::run(ctx, &mut computer),
                Provider::Openai => providers::openai::run(ctx, &mut computer),
                _ => providers::chat::run(ctx, &mut computer),
            };
            computer.disconnect();
            result
        }
    }
}

/// A turn of a conversation, on its shared connection.
fn run_turn(ctx: &RunContext, link: &SessionLink) -> Result<String, String> {
    let until = ctx.until_epoch();
    {
        let mut c = link.computer.lock();
        c.config.session.width = ctx.settings.width;
        c.config.session.height = ctx.settings.height;
        c.config.judge = Judge::new(&ctx.data_dir, &ctx.settings.checks).map(Arc::new);
        c.begin_turn(
            ctx.settings.max_actions,
            until,
            ctx.control_dir(),
            ctx.settings.approvals,
        );
    }
    match ctx.settings.provider {
        Provider::Codex => providers::cli::run_codex(ctx),
        Provider::ClaudeCode => providers::cli::run_claude(ctx),
        api => {
            // The model is told the conversation so far, then the new words.
            let history = link.continuity.lock().history.clone();
            let told = RunContext {
                task: providers::conversation_task(&history, &ctx.task),
                ..ctx.clone_shallow()
            };
            let mut computer = link.computer.lock();
            if computer.session().is_none() {
                ctx.emit(RunEvent::Status(format!("Connecting to {}…", ctx.host)));
                computer.connect(
                    Some(&ctx.host),
                    Some((ctx.settings.width, ctx.settings.height)),
                )?;
            }
            match api {
                Provider::Anthropic => providers::anthropic::run(&told, &mut computer),
                Provider::Openai => providers::openai::run(&told, &mut computer),
                _ => providers::chat::run(&told, &mut computer),
            }
        }
    }
}
