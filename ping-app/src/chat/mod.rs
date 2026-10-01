//! An agent session: a conversation with an AI agent about one host, open in
//! the sidebar until you end it. You and the agent take turns; when a message
//! needs the computer, the agent uses the host (its own connection, kept for
//! the whole session), and you can log in to that desktop at any time to
//! watch or take over. "What the agent sees" shows the screen after its last
//! action, as the model saw it. Everything is in memory: ending the session,
//! or quitting Ping, forgets it.

mod panel;
mod sample;
mod view;

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use gpui::{prelude::*, Context, Entity, Image, ImageFormat, ScrollHandle, Window};
use ping_agent::computer::{LinkStatus, PlanStep};
use ping_agent::conversation::Conversation;
use ping_agent::jev::Ending;
use ping_agent::providers::{AgentSettings, Ask, Provider, RunEvent};
use pingpong_proto::control::agent_state;
use pingpong_ui::{FieldEvent, TextField};

use crate::app::{PingApp, Waker};
use crate::notify::{Notice, NoticeKind};

/// The strip's pictures: this many pixels on their longer side (sharp at
/// twice their size on screen).
const STRIP_PIXELS: u32 = 240;

/// After this long with no message and nobody watching, the host is let go
/// (its display back to its owner); the next message connects again.
const IDLE_RELEASE: Duration = Duration::from_secs(20 * 60);

pub enum ChatMsg {
    Run(RunEvent),
    /// An action, with the screen after it (and a small copy for the strip)
    /// and where it acted.
    Done {
        text: String,
        ok: bool,
        detail: String,
        full: Option<Vec<u8>>,
        thumb: Option<Vec<u8>>,
        point: Option<(f32, f32)>,
    },
    Plan(Vec<PlanStep>),
    /// A connection made for logging in (or its failure).
    Connected(Result<(), String>),
    Released,
}

struct Act {
    text: String,
    ok: bool,
    detail: String,
}

/// What the agent saw after one of its actions.
struct Step {
    caption: String,
    ok: bool,
    full: Arc<Image>,
    thumb: Arc<Image>,
    /// Where it acted, as fractions of the screen: the marker.
    point: Option<(f32, f32)>,
    at: Instant,
}

enum Item {
    You(String),
    Reply(String),
    Actions { list: Vec<Act>, open: bool },
    Note(String),
    Error(String),
}

struct Turn {
    started: Instant,
    paused: bool,
    /// A step waiting for the user's yes.
    question: Option<Ask>,
    answers: Sender<bool>,
    status: Option<String>,
}

/// What the sidebar says about a session.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChatState {
    Working,
    Waiting,
    Paused,
    Connecting,
    Connected,
    Idle,
}

pub struct Chat {
    pub id: u64,
    pub host: String,
    pub title: String,
    settings: AgentSettings,
    conversation: Option<Conversation>,
    items: Vec<Item>,
    turn: Option<Turn>,
    tx: Sender<ChatMsg>,
    rx: Receiver<ChatMsg>,
    waker: Waker,
    /// What the agent saw after each action, oldest first.
    steps: Vec<Step>,
    /// The step shown large (`None`: the latest, following along).
    selected: Option<usize>,
    strip: ScrollHandle,
    /// A new step to bring into view on the strip (once it has been laid
    /// out: GPUI drops a scroll asked for before).
    strip_follow: std::cell::Cell<bool>,
    pub panel_scroll: ScrollHandle,
    /// The model's plan, as last shared.
    plan: Vec<PlanStep>,
    /// The connection as last seen, and when it was looked at.
    link: Option<LinkStatus>,
    link_looked: Instant,
    turn_actions: u32,
    pub panel: bool,
    pub composer: Entity<TextField>,
    scroll: ScrollHandle,
    tokens: (u64, u64, Option<f64>),
    actions: u32,
    last_active: Instant,
    connected: bool,
    connecting: bool,
    /// An action waits for the user to give the keyboard and mouse back.
    held: bool,
    /// Log in once the connection is up.
    pub watch_pending: bool,
    /// Notifications to show, if the session is not on screen.
    pub notices: Vec<Notice>,
    /// The agent waits at a secure screen (said once per wait).
    secure_wait: bool,
    /// How the last turn ended, as Jev read it (cleared by a new message).
    ending: Option<Ending>,
}

fn short_title(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(42).collect();
    if line.chars().count() > 42 {
        out.push('…');
    }
    if out.is_empty() {
        "New session".into()
    } else {
        out
    }
}

pub fn provider_name(p: Provider) -> &'static str {
    match p {
        Provider::Codex => "Codex",
        Provider::ClaudeCode => "Claude Code",
        Provider::Anthropic => "Anthropic API",
        Provider::Openai => "OpenAI API",
        Provider::Openrouter => "OpenRouter",
        Provider::Custom => "Custom endpoint",
    }
}

fn thousands(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{:.0}k", n as f64 / 1e3)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// Markdown as plain text, at most `max` characters (a notification's).
fn plain(md: &str, max: usize) -> String {
    let text: String = md
        .replace(['*', '`', '#', '|'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() > max {
        format!("{}…", text.chars().take(max).collect::<String>().trim_end())
    } else {
        text
    }
}

impl Chat {
    /// Open a session about `host` with these settings (kept for its life).
    pub fn open(
        id: u64,
        host: &str,
        settings: AgentSettings,
        waker: Waker,
        window: &mut Window,
        cx: &mut Context<PingApp>,
    ) -> Result<Chat, String> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let on_action: ping_agent::computer::Observer = {
            let (tx, waker) = (tx.clone(), waker.clone());
            Arc::new(move |d| {
                // The strip's copy is made here, off the window's thread.
                let full = d.thumbnail.map(|t| t.png);
                let thumb = full
                    .as_deref()
                    .and_then(|p| ping_agent::computer::shrink_png(p, STRIP_PIXELS))
                    .map(|s| s.png);
                let _ = tx.send(ChatMsg::Done {
                    text: d.action,
                    ok: d.ok,
                    detail: d.text,
                    full,
                    thumb,
                    point: d.point,
                });
                waker.wake();
            })
        };
        let on_plan: ping_agent::computer::PlanObserver = {
            let (tx, waker) = (tx.clone(), waker.clone());
            Arc::new(move |steps| {
                let _ = tx.send(ChatMsg::Plan(steps));
                waker.wake();
            })
        };
        let conversation = Conversation::open(
            &ping_core::store::data_dir(),
            host,
            &settings,
            on_action,
            on_plan,
        )?;
        let chat = Chat::new(
            id,
            host,
            settings,
            Some(conversation),
            tx,
            rx,
            waker,
            window,
            cx,
        );
        tracing::info!(
            id,
            host,
            provider = chat.settings.provider.id(),
            model = chat.settings.model,
            "agent session opened"
        );
        Ok(chat)
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        id: u64,
        host: &str,
        settings: AgentSettings,
        conversation: Option<Conversation>,
        tx: Sender<ChatMsg>,
        rx: Receiver<ChatMsg>,
        waker: Waker,
        window: &mut Window,
        cx: &mut Context<PingApp>,
    ) -> Chat {
        let composer = cx.new(|cx| {
            TextField::new(cx)
                .multiline()
                .placeholder("Message the agent…")
        });
        cx.subscribe_in(
            &composer,
            window,
            move |this: &mut PingApp, _, event, _, cx| match event {
                FieldEvent::Submit => this.chat_send(id, cx),
                FieldEvent::Changed => cx.notify(),
                FieldEvent::Cancel => {}
            },
        )
        .detach();
        Chat {
            id,
            host: host.to_string(),
            title: "New session".into(),
            settings,
            conversation,
            items: Vec::new(),
            turn: None,
            tx,
            rx,
            waker,
            steps: Vec::new(),
            selected: None,
            strip: ScrollHandle::new(),
            strip_follow: std::cell::Cell::new(false),
            panel_scroll: ScrollHandle::new(),
            plan: Vec::new(),
            link: None,
            link_looked: Instant::now(),
            turn_actions: 0,
            panel: true,
            composer,
            scroll: ScrollHandle::new(),
            tokens: (0, 0, None),
            actions: 0,
            last_active: Instant::now(),
            connected: false,
            connecting: false,
            held: false,
            watch_pending: false,
            notices: Vec::new(),
            secure_wait: false,
            ending: None,
        }
    }

    fn notice(&mut self, kind: NoticeKind, title: String, body: String) {
        self.notices.push(Notice { kind, title, body });
    }

    pub fn busy(&self) -> bool {
        self.turn.is_some()
    }

    pub fn awaiting(&self) -> bool {
        self.turn.as_ref().is_some_and(|t| t.question.is_some())
    }

    pub fn state(&self) -> ChatState {
        match &self.turn {
            Some(t) if t.question.is_some() || self.held => ChatState::Waiting,
            Some(t) if t.paused => ChatState::Paused,
            Some(_) => ChatState::Working,
            // The turn is over, and Jev read it as waiting on the person.
            None if matches!(self.ending, Some(Ending::Question | Ending::NeedsPerson)) => {
                ChatState::Waiting
            }
            None if self.connecting => ChatState::Connecting,
            None if self.connected => ChatState::Connected,
            None => ChatState::Idle,
        }
    }

    /// The user's next message.
    pub fn send(&mut self, text: String) -> Result<(), String> {
        let Some(conversation) = &mut self.conversation else {
            return Err("This session has ended.".into());
        };
        if self.turn.is_some() {
            return Err("The agent is still on the last message.".into());
        }
        let (answer_tx, answer_rx) = crossbeam_channel::bounded::<bool>(1);
        let events: ping_agent::providers::Events = {
            let (tx, waker) = (self.tx.clone(), self.waker.clone());
            Arc::new(move |e| {
                let _ = tx.send(ChatMsg::Run(e));
                waker.wake();
            })
        };
        // A step waits for the user's yes: the transcript shows the question
        // and the turn waits for the answer (as long as for a person who
        // took over; then it is a no). An answer to an earlier question that
        // came too late is not this one's.
        let confirm: ping_agent::providers::Confirm = Arc::new(move |_q: &str| {
            while answer_rx.try_recv().is_ok() {}
            answer_rx
                .recv_timeout(ping_agent::computer::HOLD_WAIT)
                .unwrap_or(false)
        });
        conversation.send(&text, &self.settings, events, confirm)?;
        if self.items.is_empty() {
            self.title = short_title(&text);
        }
        self.items.push(Item::You(text));
        self.turn_actions = 0;
        self.ending = None;
        self.turn = Some(Turn {
            started: Instant::now(),
            paused: false,
            question: None,
            answers: answer_tx,
            status: None,
        });
        self.last_active = Instant::now();
        self.scroll.scroll_to_bottom();
        Ok(())
    }

    fn push_action(&mut self, act: Act) {
        self.actions += 1;
        self.turn_actions += 1;
        match self.items.last_mut() {
            Some(Item::Actions { list, .. }) => list.push(act),
            _ => self.items.push(Item::Actions {
                list: vec![act],
                open: false,
            }),
        }
    }

    /// Take in what the turn and the connection said; true if anything did.
    pub fn update(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            match msg {
                ChatMsg::Run(ev) => self.on_event(ev),
                ChatMsg::Done {
                    text,
                    ok,
                    detail,
                    full,
                    thumb,
                    point,
                } => {
                    if let Some(full) = full {
                        self.add_step(&text, ok, full, thumb, point);
                    }
                    if text.starts_with("connect") {
                        self.connected = true;
                    }
                    self.last_active = Instant::now();
                    self.push_action(Act { text, ok, detail });
                }
                ChatMsg::Plan(steps) => self.plan = steps,
                ChatMsg::Connected(r) => {
                    self.connecting = false;
                    match r {
                        Ok(()) => self.connected = true,
                        Err(e) => {
                            self.watch_pending = false;
                            self.items.push(Item::Error(format!(
                                "Could not connect to {}: {e}",
                                self.host
                            )));
                        }
                    }
                }
                ChatMsg::Released => {
                    self.connected = false;
                    self.items.push(Item::Note(format!(
                        "Let {} go after {} idle minutes; the next message connects again.",
                        self.host,
                        IDLE_RELEASE.as_secs() / 60
                    )));
                }
            }
        }
        if let Some(c) = self.conversation.as_ref().and_then(Conversation::connected) {
            if c != self.connected {
                self.connected = c;
                changed = true;
            }
        }
        // The connection, once a second: the session card.
        if self.link_looked.elapsed() >= Duration::from_secs(1) {
            self.link_looked = Instant::now();
            if let Some(now) = self
                .conversation
                .as_ref()
                .and_then(Conversation::link_status)
            {
                let key = |l: &Option<LinkStatus>| {
                    l.map(|l| {
                        (
                            (l.rtt_ms * 2.0).round() as i32,
                            (l.loss_pct * 10.0).round() as i32,
                            l.agent.map(|a| (a.flags, a.watchers)),
                        )
                    })
                };
                if key(&now) != key(&self.link) {
                    changed = true;
                }
                self.link = now;
            }
        }
        let held = self.turn.is_some()
            && self
                .conversation
                .as_ref()
                .is_some_and(Conversation::waiting_for_person);
        if held != self.held {
            self.held = held;
            changed = true;
        }
        // Held by a sign-in, lock or administrator screen: only a person at
        // it (or logged in) can go on.
        let secure = held
            && self
                .link
                .and_then(|l| l.agent)
                .is_some_and(|a| a.flags & agent_state::SECURE_DESKTOP != 0);
        if secure && !self.secure_wait {
            self.notice(
                NoticeKind::Waiting,
                format!("{}: the agent waits for you", self.host),
                format!(
                    "A sign-in, lock or administrator screen is up on {}: only a \
                        person can answer it. Log in to answer it; the agent goes on after.",
                    self.host
                ),
            );
        }
        self.secure_wait = secure;
        if changed {
            self.scroll.scroll_to_bottom();
        }
        changed
    }

    fn on_event(&mut self, ev: RunEvent) {
        match ev {
            RunEvent::Status(s) => {
                if let Some(t) = &mut self.turn {
                    t.status = Some(s);
                }
            }
            RunEvent::Thought(text) => self.items.push(Item::Reply(text)),
            RunEvent::Action {
                text,
                ok,
                detail,
                thumbnail,
            } => {
                if let Some(png) = thumbnail {
                    self.add_step(&text, ok, png, None, None);
                }
                if text.starts_with("connect") {
                    self.connected = true;
                }
                self.last_active = Instant::now();
                self.push_action(Act { text, ok, detail });
            }
            RunEvent::Plan(steps) => self.plan = steps,
            RunEvent::Note(text) => self.items.push(Item::Note(text)),
            RunEvent::Ending(e) => self.ending = Some(e),
            RunEvent::Usage {
                input,
                output,
                cost_usd,
                ..
            } => {
                // Totals per turn, summed over the session.
                self.tokens = (
                    self.tokens.0.max(input),
                    self.tokens.1.max(output),
                    cost_usd.or(self.tokens.2),
                );
            }
            RunEvent::Confirm(q) => {
                let body = if q.why.is_empty() {
                    q.what.clone()
                } else {
                    format!("{}\n{}", q.what, q.why)
                };
                self.notice(
                    NoticeKind::Ask,
                    format!("{}: the agent asks for your go-ahead", self.host),
                    body,
                );
                if let Some(t) = &mut self.turn {
                    t.question = Some(q);
                }
            }
            RunEvent::Finished(summary) => {
                // The summary is usually the model's last words: once.
                let last_reply = self.items.iter().rev().find_map(|i| match i {
                    Item::Reply(r) => Some(r.trim().to_string()),
                    Item::You(_) => Some(String::new()),
                    _ => None,
                });
                let (kind, title) = match self.ending {
                    Some(Ending::Question) => (NoticeKind::Waiting, "the agent asks you something"),
                    Some(Ending::NeedsPerson) => {
                        (NoticeKind::Waiting, "the agent needs you at the host")
                    }
                    Some(Ending::NotDone) => (NoticeKind::Stopped, "the agent could not finish"),
                    Some(Ending::Done) | None => (NoticeKind::Done, "done"),
                };
                self.notice(
                    kind,
                    format!("{}: {title}", self.host),
                    plain(&summary, 220),
                );
                if last_reply.as_deref() != Some(summary.trim()) {
                    self.items.push(Item::Reply(summary));
                }
                self.finish_turn();
            }
            RunEvent::Failed(e) => {
                if e.trim() == "Stopped." {
                    self.items.push(Item::Note("You stopped the agent.".into()));
                } else {
                    self.notice(
                        NoticeKind::Stopped,
                        format!("{}: the agent stopped", self.host),
                        plain(&e, 220),
                    );
                    self.items.push(Item::Error(e));
                }
                self.finish_turn();
            }
        }
    }

    /// A picture for the panel and its strip.
    fn add_step(
        &mut self,
        caption: &str,
        ok: bool,
        full: Vec<u8>,
        thumb: Option<Vec<u8>>,
        point: Option<(f32, f32)>,
    ) {
        let full = Arc::new(Image::from_bytes(ImageFormat::Png, full));
        let thumb = thumb
            .map(|t| Arc::new(Image::from_bytes(ImageFormat::Png, t)))
            .unwrap_or_else(|| full.clone());
        self.steps.push(Step {
            caption: caption.to_string(),
            ok,
            full,
            thumb,
            point,
            at: Instant::now(),
        });
        if self.selected.is_none() {
            self.strip_follow.set(true);
        }
    }

    /// Show step `ix` (0-based) large (demo).
    pub fn select_step(&mut self, ix: usize) {
        self.selected = (ix + 1 < self.steps.len()).then_some(ix);
    }

    fn finish_turn(&mut self) {
        if let Some(t) = self.turn.take() {
            tracing::info!(
                id = self.id,
                host = self.host,
                secs = t.started.elapsed().as_secs(),
                actions = self.actions,
                "agent turn over"
            );
        }
        // The turn's actions fold away once it is over.
        for item in &mut self.items {
            if let Item::Actions { open, .. } = item {
                *open = false;
            }
        }
        self.last_active = Instant::now();
    }

    /// Answer the step waiting for a yes (from the page, or a notification).
    pub fn answer(&mut self, yes: bool) {
        if let Some(t) = &mut self.turn {
            let Some(q) = t.question.take() else { return };
            let _ = t.answers.try_send(yes);
            tracing::info!(id = self.id, yes, "the user answered the agent");
            self.items.push(Item::Note(if yes {
                format!("You allowed: {}", q.what)
            } else {
                format!("You said no to: {}", q.what)
            }));
        }
    }

    /// PING_UI_DEMO: a step waiting for a yes.
    pub fn demo_ask(&mut self, ask: Ask) {
        if let Some(t) = &mut self.turn {
            t.question = Some(ask);
        }
        self.scroll.scroll_to_bottom();
    }

    /// The step waiting for a yes, if one is.
    pub fn question(&self) -> Option<&Ask> {
        self.turn.as_ref().and_then(|t| t.question.as_ref())
    }

    fn toggle_pause(&mut self) {
        if let (Some(t), Some(c)) = (&mut self.turn, &self.conversation) {
            t.paused = !t.paused;
            c.set_paused(t.paused);
        }
    }

    fn stop(&self) {
        if let Some(c) = &self.conversation {
            c.stop();
        }
    }

    /// Connect now (to log in), on a thread: connecting takes a moment.
    pub fn connect(&mut self) {
        let Some(c) = &self.conversation else { return };
        if self.connected || self.connecting {
            return;
        }
        self.connecting = true;
        let (computer, tx, waker, host) = (
            c.computer(),
            self.tx.clone(),
            self.waker.clone(),
            self.host.clone(),
        );
        std::thread::spawn(move || {
            tracing::info!(host, "connecting for the user to log in");
            let r = ping_agent::conversation::connect(&computer);
            let _ = tx.send(ChatMsg::Connected(r));
            waker.wake();
        });
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Let the host go after a long idle while nobody watches.
    pub fn idle_check(&mut self, watched: bool) {
        if self.turn.is_some()
            || watched
            || !self.connected
            || self.last_active.elapsed() < IDLE_RELEASE
        {
            return;
        }
        let Some(c) = &self.conversation else { return };
        self.last_active = Instant::now();
        let (computer, tx, waker, host) = (
            c.computer(),
            self.tx.clone(),
            self.waker.clone(),
            self.host.clone(),
        );
        std::thread::spawn(move || {
            tracing::info!(host, "agent session idle: letting the host go");
            computer.lock().disconnect();
            let _ = tx.send(ChatMsg::Released);
            waker.wake();
        });
    }

    /// End it: the turn stops, the host is let go (on a thread: stopping
    /// waits for the model's program).
    pub fn end(mut self) {
        tracing::info!(
            id = self.id,
            host = self.host,
            "agent session ended by the user"
        );
        if let Some(c) = self.conversation.take() {
            std::thread::spawn(move || c.end());
        }
    }
}

// ---------------------------------------------------------------------------
// The session's page
