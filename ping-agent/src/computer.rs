//! A computer for an agent to use: one paired host at a time, through a
//! headless session, with the actions every computer-use model knows
//! (Claude's computer tool's, which OpenAI's map onto) and a screenshot of
//! the settled screen after each.
//!
//! Coordinates are stream pixels, which are the screenshot's pixels: the
//! host makes its display the stream's size (Windows, Mac) or scales its
//! screen to it (Linux), so nothing is ever rescaled here.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pingpong_proto::control::{agent_state, AgentNote, AgentState, Control};
use pingpong_proto::input::{Button, InputEvent};
use pingpong_proto::screen::Query;

use crate::frame::Rgb;
use crate::headless::{HeadlessOptions, HeadlessSession};

/// A screenshot, as PNG.
#[derive(Clone)]
pub struct Shot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// What an action did: words for the model, and the screen after it.
#[derive(Clone, Default)]
pub struct Outcome {
    pub text: String,
    pub shot: Option<Shot>,
}

impl Outcome {
    fn text(t: impl Into<String>) -> Outcome {
        Outcome {
            text: t.into(),
            shot: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mouse {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl Mouse {
    fn button(self) -> Button {
        match self {
            Mouse::Left => Button::Left,
            Mouse::Right => Button::Right,
            Mouse::Middle => Button::Middle,
            Mouse::Back => Button::X1,
            Mouse::Forward => Button::X2,
        }
    }

    pub fn parse(s: &str) -> Option<Mouse> {
        match s.to_ascii_lowercase().as_str() {
            "left" | "" => Some(Mouse::Left),
            "right" => Some(Mouse::Right),
            "middle" | "wheel" => Some(Mouse::Middle),
            "back" | "x1" => Some(Mouse::Back),
            "forward" | "x2" => Some(Mouse::Forward),
            _ => None,
        }
    }
}

/// What an agent can do with the computer.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Screenshot,
    /// A region `[x0, y0, x1, y1]`, enlarged.
    Zoom {
        region: [u32; 4],
    },
    Click {
        at: Option<(u32, u32)>,
        button: Mouse,
        count: u8,
        modifiers: Option<String>,
    },
    MouseMove {
        at: (u32, u32),
    },
    MouseDown {
        button: Mouse,
    },
    MouseUp {
        button: Mouse,
    },
    /// Press at the first point, move through the rest, release at the last.
    Drag {
        path: Vec<(u32, u32)>,
        modifiers: Option<String>,
    },
    /// Wheel notches: positive `down` scrolls down, positive `right` right.
    Scroll {
        at: Option<(u32, u32)>,
        down: i32,
        right: i32,
        modifiers: Option<String>,
    },
    Type {
        text: String,
    },
    Key {
        keys: String,
        repeat: u32,
    },
    HoldKey {
        keys: String,
        seconds: f32,
    },
    Wait {
        seconds: f32,
    },
    CursorPosition,
}

impl Action {
    /// Where on the screen it acts, if somewhere (a drag: where it ends).
    pub fn point(&self) -> Option<(u32, u32)> {
        match self {
            Action::Click { at, .. } | Action::Scroll { at, .. } => *at,
            Action::MouseMove { at } => Some(*at),
            Action::Drag { path, .. } => path.last().copied(),
            _ => None,
        }
    }

    /// A few words for the host's activity log and the watcher.
    pub fn describe(&self) -> String {
        let at =
            |p: &Option<(u32, u32)>| p.map(|(x, y)| format!(" ({x}, {y})")).unwrap_or_default();
        match self {
            Action::Screenshot => "screenshot".into(),
            Action::Zoom { region } => format!("zoom {region:?}"),
            Action::Click {
                at: p,
                button,
                count,
                modifiers,
            } => {
                let kind = match (count, button) {
                    (2, Mouse::Left) => "double_click".to_string(),
                    (3, Mouse::Left) => "triple_click".to_string(),
                    (_, b) => format!("{}_click", format!("{b:?}").to_lowercase()),
                };
                format!(
                    "{}{kind}{}",
                    modifiers
                        .as_ref()
                        .map(|m| format!("{m}+"))
                        .unwrap_or_default(),
                    at(p)
                )
            }
            Action::MouseMove { at: (x, y) } => format!("mouse_move ({x}, {y})"),
            Action::MouseDown { button } => {
                format!("{}_mouse_down", format!("{button:?}").to_lowercase())
            }
            Action::MouseUp { button } => {
                format!("{}_mouse_up", format!("{button:?}").to_lowercase())
            }
            Action::Drag { path, .. } => match (path.first(), path.last()) {
                (Some(a), Some(b)) => format!("drag ({}, {}) -> ({}, {})", a.0, a.1, b.0, b.1),
                _ => "drag".into(),
            },
            Action::Scroll {
                at: p, down, right, ..
            } => format!("scroll down {down} right {right}{}", at(p)),
            Action::Type { text } => {
                let shown: String = text.chars().take(24).collect();
                let more = if text.chars().count() > 24 { "…" } else { "" };
                format!("type {shown:?}{more}")
            }
            Action::Key { keys, repeat } if *repeat > 1 => format!("key {keys} x{repeat}"),
            Action::Key { keys, .. } => format!("key {keys}"),
            Action::HoldKey { keys, seconds } => format!("hold_key {keys} {seconds}s"),
            Action::Wait { seconds } => format!("wait {seconds}s"),
            Action::CursorPosition => "cursor_position".into(),
        }
    }

    /// Whether it touches the host's keyboard or mouse.
    fn is_input(&self) -> bool {
        !matches!(
            self,
            Action::Screenshot | Action::Zoom { .. } | Action::Wait { .. } | Action::CursorPosition
        )
    }
}

/// Something done, for whoever watches the run (the app's agent panel).
pub struct Done {
    pub action: String,
    pub ok: bool,
    pub text: String,
    /// A small picture of the screen after it.
    pub thumbnail: Option<Shot>,
    /// How long it waited for a person to give the keyboard and mouse back
    /// (it was then not done: the model looks again).
    pub held: Option<Duration>,
    /// Where it acted, as fractions of the screen (0..1 across, 0..1 down):
    /// the marker on the picture after it.
    pub point: Option<(f32, f32)>,
}

/// A step of the plan a model shares (`share_plan`, or Codex's own).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PlanStep {
    pub text: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Done,
}

impl PlanStatus {
    /// As models write it ("in_progress", "completed", …).
    pub fn parse(s: &str) -> PlanStatus {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '-'], "_")
            .as_str()
        {
            "in_progress" | "active" | "doing" | "current" => PlanStatus::InProgress,
            "done" | "completed" | "complete" | "finished" => PlanStatus::Done,
            _ => PlanStatus::Pending,
        }
    }
}

pub type PlanObserver = Arc<dyn Fn(Vec<PlanStep>) + Send + Sync>;

/// The connection as the session panel shows it.
#[derive(Debug, Clone, Copy)]
pub struct LinkStatus {
    pub rtt_ms: f32,
    pub loss_pct: f32,
    /// Who drives, as the host says (`None`: not said yet).
    pub agent: Option<AgentState>,
}

/// How long an action in a run or session Ping starts waits while a person
/// has the keyboard and mouse (took over, paused the agent, uses the host,
/// answers a secure screen), before the agent gives up and reports.
pub const HOLD_WAIT: Duration = Duration::from_secs(15 * 60);

/// What others see of an action that waits for a person, and how they end
/// the wait: shared with the run, which does not count the time waited
/// against its minutes, and with the session, whose Stop and End Session
/// must not wait for it.
#[derive(Clone, Default)]
pub struct Waits(Arc<WaitsInner>);

#[derive(Default)]
struct WaitsInner {
    /// Bumped to end whatever waits now.
    interrupt: AtomicU64,
    /// Waited for people this turn.
    waited_ms: AtomicU64,
    waiting: AtomicBool,
}

impl Waits {
    /// End any wait now (the turn was stopped, the session ends).
    pub fn interrupt(&self) {
        self.0.interrupt.fetch_add(1, Ordering::AcqRel);
    }

    fn generation(&self) -> u64 {
        self.0.interrupt.load(Ordering::Acquire)
    }

    /// Time spent this turn waiting for people.
    pub fn waited(&self) -> Duration {
        Duration::from_millis(self.0.waited_ms.load(Ordering::Acquire))
    }

    pub fn add_waited(&self, d: Duration) {
        self.0
            .waited_ms
            .fetch_add(d.as_millis() as u64, Ordering::AcqRel);
    }

    /// Whether an action waits for a person right now.
    pub fn waiting(&self) -> bool {
        self.0.waiting.load(Ordering::Acquire)
    }
}

pub type Observer = Arc<dyn Fn(Done) + Send + Sync>;

/// How the computer behaves.
#[derive(Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    /// The host to use when none is named.
    pub default_host: Option<String>,
    pub session: HeadlessOptions,
    /// Answer every action with the settled screen (else "OK": the agent
    /// asks for screenshots itself).
    pub screenshot_after_actions: bool,
    /// Stop taking input actions after this many (the run's budget).
    pub max_actions: Option<u32>,
    /// Tell the host what each action is (its activity log).
    pub notes: bool,
    /// How long to wait for the screen to settle after an action.
    pub settle_max: Duration,
    /// A run's end (seconds since the Unix epoch): no action after it.
    pub until: Option<u64>,
    /// The process that started the run: no action once it is gone (an
    /// agent left running by a Ping that crashed stops acting).
    pub owner_pid: Option<u32>,
    /// The run's control folder (pause, approve steps: see `control`), and
    /// which steps wait for a yes.
    pub control: Option<(PathBuf, crate::providers::Approvals)>,
    /// Observers get the screen after each action as the model sees it, not
    /// a thumbnail (Ping's "what the agent sees").
    pub full_screens: bool,
    /// How long an action waits while a person has the keyboard and mouse.
    /// Short by default: an MCP client gives a call a minute (Codex's
    /// default); Ping's own runs and sessions raise it to `HOLD_WAIT`.
    pub hold_wait: Duration,
}

impl Config {
    pub fn new(data_dir: PathBuf) -> Config {
        Config {
            data_dir,
            default_host: None,
            session: HeadlessOptions::default(),
            screenshot_after_actions: true,
            max_actions: None,
            notes: true,
            settle_max: Duration::from_millis(2500),
            until: None,
            owner_pid: None,
            control: None,
            full_screens: false,
            hold_wait: Duration::from_secs(50),
        }
    }
}

pub struct Computer {
    pub config: Config,
    session: Option<HeadlessSession>,
    /// Where this computer last put the pointer.
    pointer: Option<(u32, u32)>,
    actions: u32,
    observer: Option<Observer>,
    control: Option<crate::control::Control>,
    waits: Waits,
    /// Set when the last action waited for a person instead of being done.
    held_last: Option<Duration>,
    plan_observer: Option<PlanObserver>,
}

/// After an action, the screen is looked at no sooner than this (an app
/// takes a moment to react), and counts as settled once nothing but a caret
/// has changed for `QUIET`.
const REACT: Duration = Duration::from_millis(150);
const QUIET: Duration = Duration::from_millis(350);
const CLICK_HOLD: Duration = Duration::from_millis(30);
/// A new session's first picture: after at least this, once still for
/// `CONNECT_QUIET`, at most `CONNECT_SETTLE_MAX`.
const CONNECT_SETTLE_MIN: Duration = Duration::from_millis(1500);
const CONNECT_QUIET: Duration = Duration::from_millis(700);
const CONNECT_SETTLE_MAX: Duration = Duration::from_secs(8);
const CLICK_GAP: Duration = Duration::from_millis(70);

impl Computer {
    pub fn new(config: Config) -> Computer {
        let control = config
            .control
            .clone()
            .map(|(dir, approvals)| crate::control::Control::new(dir, approvals));
        Computer {
            config,
            session: None,
            pointer: None,
            actions: 0,
            observer: None,
            control,
            waits: Waits::default(),
            held_last: None,
            plan_observer: None,
        }
    }

    /// Hear the plans the model shares.
    pub fn observe_plan(&mut self, observer: PlanObserver) {
        self.plan_observer = Some(observer);
    }

    /// The model's plan, for the person watching.
    pub fn share_plan(&self, steps: Vec<PlanStep>) {
        tracing::info!(
            steps = steps.len(),
            done = steps
                .iter()
                .filter(|s| s.status == PlanStatus::Done)
                .count(),
            "plan shared"
        );
        if let Some(obs) = &self.plan_observer {
            obs(steps);
        }
    }

    /// The connection now, if there is one.
    pub fn link_status(&self) -> Option<LinkStatus> {
        let s = self.session.as_ref()?;
        if s.ended().is_some() {
            return None;
        }
        let st = s.stats();
        Some(LinkStatus {
            rtt_ms: st.rtt_ms,
            loss_pct: st.packet_loss_pct,
            agent: s.agent_state(),
        })
    }

    /// See and end its waits for people (see `Waits`).
    pub fn waits(&self) -> Waits {
        self.waits.clone()
    }

    /// Share the run's `Waits`.
    pub fn set_waits(&mut self, waits: Waits) {
        self.waits = waits;
    }

    pub fn observe(&mut self, observer: Observer) {
        self.observer = Some(observer);
    }

    pub fn host(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.host_name.as_str())
    }

    pub fn session(&self) -> Option<&HeadlessSession> {
        self.session.as_ref()
    }

    pub fn actions_taken(&self) -> u32 {
        self.actions
    }

    /// A new turn of a conversation on the same connection: its own budget
    /// of actions and end, and which steps wait for a yes.
    pub fn begin_turn(
        &mut self,
        max_actions: u32,
        until: u64,
        control_dir: PathBuf,
        approvals: crate::providers::Approvals,
    ) {
        self.actions = 0;
        self.waits.0.waited_ms.store(0, Ordering::Release);
        self.config.max_actions = Some(max_actions);
        self.config.until = Some(until);
        self.config.control = Some((control_dir.clone(), approvals));
        self.control = Some(crate::control::Control::new(control_dir, approvals));
        tracing::info!(max_actions, approvals = approvals.id(), "turn begins");
    }

    /// The model asks the person before a step (`ask_approval`); the time
    /// it waits is not the run's.
    pub fn ask_approval(&mut self, what: &str, why: &str) -> Result<Outcome, String> {
        let what = what.trim();
        if what.is_empty() {
            return Err("ask_approval needs `action`: exactly what you will do.".into());
        }
        let stop = self.stop_check();
        let hold = self.config.hold_wait;
        let Some(control) = &mut self.control else {
            return Ok(Outcome::text(
                "Nobody can be asked here (no one runs this session from Ping). Don't do \
                    it: report that it needs a person's go-ahead.",
            ));
        };
        let waited = Instant::now();
        let ask = crate::providers::Ask {
            what: what.to_string(),
            why: why.trim().to_string(),
        };
        let answer = control.request(&ask, &stop, hold);
        self.count_waited(waited.elapsed());
        Ok(Outcome::text(if answer? {
            "Approved: go ahead, as you described it."
        } else {
            "The person said no. Don't do it: find another way, or stop and tell them."
        }))
    }

    /// Whether a wait for a person must end: the turn was stopped, or Ping
    /// is gone.
    fn stop_check(&self) -> impl Fn() -> Option<String> {
        let (owner, waits) = (self.config.owner_pid, self.waits.clone());
        let generation = waits.generation();
        move || {
            if waits.generation() != generation {
                return Some("The turn was stopped.".to_string());
            }
            owner
                .filter(|&pid| !alive(pid))
                .map(|_| "Ping, which started this run, is gone. Stop here.".to_string())
        }
    }

    /// Between turns nothing acts: the budget and end are the turn's.
    pub fn end_turn(&mut self) {
        tracing::info!(actions = self.actions, "turn ends");
        self.config.until = Some(0);
    }

    fn picture(&self, shot: &Shot) -> Option<Shot> {
        if self.config.full_screens {
            Some(shot.clone())
        } else {
            thumbnail(shot)
        }
    }

    /// Start using `host` (or the default one), ending any other session.
    pub fn connect(
        &mut self,
        host: Option<&str>,
        size: Option<(u16, u16)>,
    ) -> Result<Outcome, String> {
        let host = host
            .map(str::to_string)
            .or_else(|| self.config.default_host.clone())
            .or_else(|| {
                let hosts = crate::headless::agent_hosts(&self.config.data_dir);
                (hosts.len() == 1).then(|| hosts[0].name.clone())
            })
            .ok_or_else(|| {
                let names: Vec<String> = crate::headless::agent_hosts(&self.config.data_dir)
                    .into_iter()
                    .map(|h| h.name)
                    .collect();
                format!(
                    "Name a host to connect to: {}.",
                    if names.is_empty() {
                        "none is paired".into()
                    } else {
                        names.join(", ")
                    }
                )
            })?;
        self.disconnect();
        let mut opts = self.config.session.clone();
        if let Some((w, h)) = size {
            opts.width = w.clamp(640, 3840);
            opts.height = h.clamp(480, 2400);
        }
        tracing::info!(host, width = opts.width, height = opts.height, "connecting");
        let session =
            HeadlessSession::connect(&self.config.data_dir, &host, &opts, Duration::from_secs(45))?;
        let (w, h) = session.size();
        let os = match session.host_os {
            pingpong_proto::control::host::MACOS => "macOS",
            pingpong_proto::control::host::LINUX => "Linux",
            _ => "Windows",
        };
        self.session = Some(session);
        self.pointer = None;
        self.note(&format!("connected at {w}x{h}"));
        // A host that just made its display settles first: Windows shows the
        // desktop, then black for a second or two (which can hold still
        // long enough to look settled), then the desktop again, sometimes
        // shrunk and animating in between.
        {
            let s = self.session.as_ref().expect("connected");
            let started = Instant::now();
            let deadline = started + CONNECT_SETTLE_MAX;
            let mut since = started;
            loop {
                let min = if since == started {
                    CONNECT_SETTLE_MIN
                } else {
                    Duration::ZERO
                };
                let snap = s.frames.wait_settled(
                    since,
                    min,
                    CONNECT_QUIET,
                    deadline.saturating_duration_since(since),
                );
                if snap.is_some_and(|sn| sn.settled && !sn.picture.is_black()) {
                    break;
                }
                let now = Instant::now();
                if now >= deadline || !s.frames.wait_change(now, deadline - now) {
                    break;
                }
                since = Instant::now();
            }
        }
        let mut out = self.screenshot_outcome(None)?;
        out.text = format!(
            "Connected to {host} ({os}). The screen is {w}x{h} pixels; coordinates are pixels of this screenshot, \
                (0, 0) at the top left.{}",
            if out.text.is_empty() { String::new() } else { format!(" {}", out.text) }
        );
        // The watcher sees the screen from the start.
        if let Some(obs) = &self.observer {
            obs(Done {
                action: format!("connect {host}"),
                ok: true,
                text: format!("{w}x{h}, {os}"),
                thumbnail: out.shot.as_ref().and_then(|s| self.picture(s)),
                held: None,
                point: None,
            });
        }
        Ok(out)
    }

    pub fn disconnect(&mut self) {
        if let Some(s) = self.session.take() {
            tracing::info!(host = s.host_name, "disconnecting");
            s.close();
        }
    }

    /// Do `action`, connecting to the default host first if need be.
    pub fn act(&mut self, action: Action) -> Result<Outcome, String> {
        let described = action.describe();
        let started = std::time::Instant::now();
        self.held_last = None;
        // Where it acts: a button pressed or let go acts where the pointer is.
        let point = action.point().or(match action {
            Action::MouseDown { .. } | Action::MouseUp { .. } => self.pointer,
            _ => None,
        });
        let result = self.act_inner(&action);
        let held = self.held_last.take();
        let point = point.and_then(|(x, y)| {
            let (w, h) = self.session.as_ref().map(|s| s.size())?;
            (w > 0 && h > 0).then(|| (x as f32 / w as f32, y as f32 / h as f32))
        });
        match &result {
            Ok(_) if held.is_some() => tracing::info!(
                action = described,
                ms = started.elapsed().as_millis() as u64,
                "action not done: it waited for a person"
            ),
            Ok(_) => tracing::info!(
                action = described,
                ms = started.elapsed().as_millis() as u64,
                "action"
            ),
            Err(e) => tracing::info!(
                action = described,
                ms = started.elapsed().as_millis() as u64,
                error = e,
                "action refused or failed"
            ),
        }
        if let Some(obs) = &self.observer {
            let (ok, text, thumbnail) = match &result {
                Ok(o) => (
                    held.is_none(),
                    o.text.clone(),
                    o.shot.as_ref().and_then(|s| self.picture(s)),
                ),
                Err(e) => (false, e.clone(), None),
            };
            obs(Done {
                action: described,
                ok,
                text,
                thumbnail,
                held,
                point,
            });
        }
        result
    }

    fn act_inner(&mut self, action: &Action) -> Result<Outcome, String> {
        if self.session.is_none() {
            self.connect(None, None)?;
        }
        if let Some(reason) = self.session.as_ref().and_then(|s| s.ended()) {
            self.session = None;
            return Err(format!(
                "The session ended: {reason} Call connect to start a new one."
            ));
        }
        if action.is_input() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if self.config.until.is_some_and(|u| now >= u) {
                return Err(
                    "This run's time is up. Stop here and report what you did and what is left."
                        .into(),
                );
            }
            if self.config.owner_pid.is_some_and(|pid| !alive(pid)) {
                return Err("Ping, which started this run, is gone. Stop here.".into());
            }
            if let Some(max) = self.config.max_actions {
                if self.actions >= max {
                    return Err(format!(
                        "This run's budget of {max} actions is spent. Stop here and report \
                            what you did and what is left."
                    ));
                }
            }
            if let Some(state) = self
                .session
                .as_ref()
                .and_then(|s| s.agent_state())
                .filter(|st| !st.agent_may_act())
            {
                return self.wait_for_person(action, state);
            }
            if self.control.is_some() {
                // Paused from Ping, or waiting for a yes: time spent waiting
                // for the person is not the run's.
                let stop = self.stop_check();
                let risk = crate::risk::assess(action);
                let waited = Instant::now();
                let hold = self.config.hold_wait;
                let gated = self.control.as_mut().expect("there").gate(
                    &action.describe(),
                    risk.as_deref(),
                    &stop,
                    hold,
                );
                self.count_waited(waited.elapsed());
                gated?;
            }
            self.actions += 1;
            self.note(&action.describe());
        }
        let started = Instant::now();
        let text = match action {
            Action::Screenshot => return self.screenshot_outcome(None),
            Action::Zoom { region } => return self.zoom(*region),
            Action::Wait { seconds } => {
                std::thread::sleep(Duration::from_secs_f32(seconds.clamp(0.0, 30.0)));
                return self.screenshot_outcome(None);
            }
            Action::CursorPosition => {
                return Ok(Outcome::text(match self.pointer {
                    Some((x, y)) => format!("X={x}, Y={y}"),
                    None => "The pointer has not been moved this session; its position is \
                        what the screenshot shows."
                        .into(),
                }))
            }
            Action::Click {
                at,
                button,
                count,
                modifiers,
            } => {
                let mods = crate::keys::modifiers(modifiers.as_deref())?;
                if let Some(p) = at {
                    self.move_to(*p)?;
                }
                self.with_keys(&mods, |c| {
                    for i in 0..(*count).clamp(1, 3) {
                        if i > 0 {
                            std::thread::sleep(CLICK_GAP);
                        }
                        c.send(InputEvent::ButtonDown(button.button()));
                        std::thread::sleep(CLICK_HOLD);
                        c.send(InputEvent::ButtonUp(button.button()));
                    }
                });
                "OK".to_string()
            }
            Action::MouseMove { at } => {
                self.move_to(*at)?;
                "OK".to_string()
            }
            Action::MouseDown { button } => {
                self.send(InputEvent::ButtonDown(button.button()));
                "OK".to_string()
            }
            Action::MouseUp { button } => {
                self.send(InputEvent::ButtonUp(button.button()));
                "OK".to_string()
            }
            Action::Drag { path, modifiers } => {
                let mods = crate::keys::modifiers(modifiers.as_deref())?;
                let (Some(&first), Some(&last)) = (path.first(), path.last()) else {
                    return Err("a drag needs a start and an end".into());
                };
                for &p in path {
                    self.check(p)?;
                }
                self.move_to(first)?;
                self.with_keys(&mods, |c| {
                    c.send(InputEvent::ButtonDown(Button::Left));
                    std::thread::sleep(Duration::from_millis(60));
                    // Through each point in small steps: apps that track a
                    // drag want to see it move, not jump.
                    let mut from = first;
                    for &to in path.iter().skip(1) {
                        let steps = 12;
                        for i in 1..=steps {
                            let x = from.0 as f32
                                + (to.0 as f32 - from.0 as f32) * i as f32 / steps as f32;
                            let y = from.1 as f32
                                + (to.1 as f32 - from.1 as f32) * i as f32 / steps as f32;
                            c.send(InputEvent::MouseMoveAbs {
                                x: x.round() as u16,
                                y: y.round() as u16,
                            });
                            std::thread::sleep(Duration::from_millis(16));
                        }
                        from = to;
                    }
                    std::thread::sleep(Duration::from_millis(60));
                    c.send(InputEvent::ButtonUp(Button::Left));
                });
                self.pointer = Some(last);
                "OK".to_string()
            }
            Action::Scroll {
                at,
                down,
                right,
                modifiers,
            } => {
                let mods = crate::keys::modifiers(modifiers.as_deref())?;
                if let Some(p) = at {
                    self.move_to(*p)?;
                }
                // One notch is 120 (Windows' WHEEL_DELTA); positive dv is up.
                let (down, right) = ((*down).clamp(-50, 50), (*right).clamp(-50, 50));
                self.with_keys(&mods, |c| {
                    for _ in 0..down.unsigned_abs().max(right.unsigned_abs()) {
                        let dv = if down == 0 { 0 } else { -120 * down.signum() };
                        let dh = if right == 0 { 0 } else { 120 * right.signum() };
                        c.send(InputEvent::Wheel {
                            dv: dv as i16,
                            dh: dh as i16,
                        });
                        std::thread::sleep(Duration::from_millis(30));
                    }
                });
                "OK".to_string()
            }
            Action::Type { text } => {
                let session = self.session.as_ref().expect("connected");
                let input = session.input().clone();
                // A Windows host types a character every 15 ms (what the new
                // Notepad keeps up with; pingpong-input's Typist): the
                // screenshot after this waits for it.
                let per_char = if matches!(
                    session.host_os,
                    pingpong_proto::control::host::MACOS | pingpong_proto::control::host::LINUX
                ) {
                    2
                } else {
                    16
                };
                let mut typed = 0;
                // Chunks, paced: a long text sent at once can outrun the
                // host's input queue.
                let chars: Vec<char> = text.chars().collect();
                for chunk in chars.chunks(64) {
                    let s: String = chunk.iter().collect();
                    typed += input.type_text(&s);
                    std::thread::sleep(Duration::from_millis(20 + per_char * chunk.len() as u64));
                }
                if typed < chars.len() {
                    format!(
                        "Typed {typed} of {} characters (the rest is too long to type).",
                        chars.len()
                    )
                } else {
                    "OK".to_string()
                }
            }
            Action::Key { keys, repeat } => {
                let chord = crate::keys::parse(keys)?;
                for i in 0..(*repeat).clamp(1, 100) {
                    if i > 0 {
                        std::thread::sleep(Duration::from_millis(40));
                    }
                    self.with_keys(&chord.keys, |_| {});
                }
                "OK".to_string()
            }
            Action::HoldKey { keys, seconds } => {
                let chord = crate::keys::parse(keys)?;
                let secs = seconds.clamp(0.0, 30.0);
                self.with_keys(&chord.keys, |_| {
                    std::thread::sleep(Duration::from_secs_f32(secs))
                });
                "OK".to_string()
            }
        };
        if !self.config.screenshot_after_actions {
            return Ok(Outcome::text(text));
        }
        self.screenshot_outcome(Some(started))
    }

    /// The front window as text, from the host's accessibility tree
    /// (`read_screen`).
    pub fn read_screen(&mut self) -> Result<Outcome, String> {
        if self.session.is_none() {
            self.connect(None, None)?;
        }
        let session = self.session.as_ref().ok_or("Not connected.")?;
        let text = session.screen_text(Query::Window)?;
        Ok(Outcome::text(crate::screen::describe(&text)))
    }

    /// A person has the keyboard and mouse (or must answer a secure screen,
    /// or uses the host): wait for them, up to `hold_wait`. When they give
    /// it back, the action is not done -- the screen has likely changed --
    /// and the model gets the screen as it is now to decide again.
    fn wait_for_person(&mut self, action: &Action, state: AgentState) -> Result<Outcome, String> {
        let host = self
            .session
            .as_ref()
            .map(|s| s.host_name.clone())
            .unwrap_or_default();
        if state.flags & agent_state::VIEW_ONLY != 0 || self.config.hold_wait.is_zero() {
            return Err(held_reason(state, &host));
        }
        tracing::info!(
            host,
            reason = held_reason(state, &host),
            wait_secs = self.config.hold_wait.as_secs(),
            "the agent waits for the keyboard and mouse"
        );
        let started = Instant::now();
        let generation = self.waits.generation();
        self.waits.0.waiting.store(true, Ordering::Release);
        let first = state;
        let outcome = loop {
            let Some(s) = self.session.as_ref() else {
                break Err("Not connected.".to_string());
            };
            if let Some(r) = s.ended() {
                break Err(format!(
                    "The session ended: {r} Call connect to start a new one."
                ));
            }
            if self.waits.generation() != generation {
                break Err(
                    "The turn was stopped while the agent waited for the keyboard and mouse."
                        .to_string(),
                );
            }
            if self.config.owner_pid.is_some_and(|pid| !alive(pid)) {
                break Err("Ping, which started this run, is gone. Stop here.".to_string());
            }
            match s.agent_state() {
                Some(a) if !a.agent_may_act() => {
                    if a.flags & agent_state::VIEW_ONLY != 0 {
                        break Err(held_reason(a, &host));
                    }
                    if started.elapsed() >= self.config.hold_wait {
                        break Err(format!(
                            "Gave up waiting on {host}: {} for {}. Stop here and report \
                                where you are; the person can tell you to go on.",
                            hold_cause(a).to_lowercase(),
                            spoken(started.elapsed())
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                _ => break Ok(()),
            }
        };
        self.waits.0.waiting.store(false, Ordering::Release);
        let waited = started.elapsed();
        self.count_waited(waited);
        outcome?;
        tracing::info!(
            host,
            secs = waited.as_secs(),
            "the agent has the keyboard and mouse again"
        );
        self.held_last = Some(waited);
        let mut out = self.screenshot_outcome(None)?;
        out.text = format!(
            "{} for {}; the agent may act again now. `{}` was not done: the screen may \
                have changed, so look at it (below) and decide again.",
            hold_cause(first),
            spoken(waited),
            action.describe()
        );
        Ok(out)
    }

    /// Time waited for a person: not the run's, so its end moves on by as
    /// much.
    fn count_waited(&mut self, waited: Duration) {
        if waited < Duration::from_secs(1) {
            return;
        }
        self.waits.add_waited(waited);
        if let Some(until) = &mut self.config.until {
            if *until > 0 {
                *until += waited.as_secs();
            }
        }
    }

    fn check(&self, (x, y): (u32, u32)) -> Result<(), String> {
        let (w, h) = self.session.as_ref().map(|s| s.size()).unwrap_or((0, 0));
        if x >= w || y >= h {
            return Err(format!("({x}, {y}) is outside the {w}x{h} screen."));
        }
        Ok(())
    }

    fn move_to(&mut self, p: (u32, u32)) -> Result<(), String> {
        self.check(p)?;
        self.send(InputEvent::MouseMoveAbs {
            x: p.0 as u16,
            y: p.1 as u16,
        });
        self.pointer = Some(p);
        // Let the pointer land (hover effects, the app hit-testing) first.
        std::thread::sleep(Duration::from_millis(40));
        Ok(())
    }

    fn send(&self, ev: InputEvent) {
        if let Some(s) = &self.session {
            s.input().send(ev);
        }
    }

    /// Press `keys` in order, run `f`, release them in reverse.
    fn with_keys(&self, keys: &[u16], f: impl FnOnce(&Self)) {
        for &k in keys {
            self.send(InputEvent::KeyDown(k));
            std::thread::sleep(Duration::from_millis(12));
        }
        f(self);
        for &k in keys.iter().rev() {
            std::thread::sleep(Duration::from_millis(12));
            self.send(InputEvent::KeyUp(k));
        }
    }

    fn note(&self, text: &str) {
        if !self.config.notes {
            return;
        }
        if let Some(s) = &self.session {
            s.controls().send(Control::AgentNote(AgentNote::new(text)));
        }
    }

    /// The screen: settled after an action at `after`, or as it is now.
    fn screenshot_outcome(&mut self, after: Option<Instant>) -> Result<Outcome, String> {
        self.screenshot_settled(after.map(|t| (t, REACT, QUIET, self.config.settle_max)))
    }

    /// The screen once nothing but a caret has changed for `quiet` (looked
    /// at `min` after `since` at the earliest, `max` at the latest), or as
    /// it is now.
    fn screenshot_settled(
        &mut self,
        settle: Option<(Instant, Duration, Duration, Duration)>,
    ) -> Result<Outcome, String> {
        let after = settle.map(|s| s.0);
        let s = self.session.as_ref().ok_or("Not connected.")?;
        let snap = match settle {
            Some((t, min, quiet, max)) => s.frames.wait_settled(t, min, quiet, max),
            None => s.frames.latest(),
        }
        .ok_or("No picture from the host yet.")?;
        let rgb = snap.picture.to_rgb();
        let shot = Shot {
            png: rgb.png(),
            width: rgb.width,
            height: rgb.height,
        };
        let mut notes = Vec::new();
        if !snap.settled {
            notes.push(
                "The screen was still changing when this was taken; look again if \
                    something is loading."
                    .to_string(),
            );
        }
        if let Some(state) = s.agent_state().filter(|st| !st.agent_may_act()) {
            notes.push(held_reason(state, &s.host_name));
        }
        let text = if after.is_some() && notes.is_empty() {
            "OK".to_string()
        } else {
            notes.join(" ")
        };
        Ok(Outcome {
            text,
            shot: Some(shot),
        })
    }

    fn zoom(&self, region: [u32; 4]) -> Result<Outcome, String> {
        let s = self.session.as_ref().ok_or("Not connected.")?;
        let snap = s.frames.latest().ok_or("No picture from the host yet.")?;
        let rgb = snap.picture.to_rgb();
        let z = rgb.zoom(region, 1024).ok_or("That region is empty.")?;
        Ok(Outcome {
            text: format!(
                "Region {region:?} enlarged {:.1}x. Coordinates stay those of the full screenshot.",
                z.width as f32 / (region[2].abs_diff(region[0]).max(1)) as f32
            ),
            shot: Some(Shot {
                png: z.png(),
                width: z.width,
                height: z.height,
            }),
        })
    }

    /// The latest picture as RGB (for watchers in the same process).
    pub fn latest_rgb(&self) -> Option<Rgb> {
        Some(self.session.as_ref()?.frames.latest()?.picture.to_rgb())
    }

    /// A line about the session: host, size, network, who drives.
    pub fn status(&self) -> String {
        let Some(s) = &self.session else {
            let hosts: Vec<String> = crate::headless::agent_hosts(&self.config.data_dir)
                .into_iter()
                .map(|h| h.name)
                .collect();
            return format!(
                "Not connected. Hosts this agent may use: {}.",
                if hosts.is_empty() {
                    "none (pair one: `ping pair-agent HOST`)".into()
                } else {
                    hosts.join(", ")
                }
            );
        };
        if let Some(reason) = s.ended() {
            return format!(
                "The session with {} ended: {reason} The next action connects again.",
                s.host_name
            );
        }
        let (w, h) = s.size();
        let st = s.stats();
        let drive = match s.agent_state() {
            Some(a) if a.agent_may_act() => format!(
                "the agent has the keyboard and mouse ({} watching)",
                a.watchers
            ),
            Some(a) => held_reason(a, &s.host_name),
            None => "the host has not said who drives yet".into(),
        };
        format!(
            "Connected to {} at {w}x{h}; round trip {:.1} ms, {:.1}% loss, {} actions \
                taken; {drive}.{}",
            s.host_name,
            st.rtt_ms,
            st.packet_loss_pct,
            self.actions,
            s.notice().map(|n| format!(" {n}")).unwrap_or_default()
        )
    }

    /// Wait until the host gives the agent the keyboard and mouse again (a
    /// person handed back, the secure screen went), up to `max`.
    pub fn wait_for_control(&mut self, max: Duration) -> Result<String, String> {
        let started = Instant::now();
        let generation = self.waits.generation();
        let result = {
            let s = self.session.as_ref().ok_or("Not connected.")?;
            let deadline = started + max;
            loop {
                match s.agent_state() {
                    Some(a) if a.agent_may_act() => {
                        break Ok("The agent has the keyboard and mouse.".into())
                    }
                    None => break Ok("The agent has the keyboard and mouse.".into()),
                    Some(a) if Instant::now() >= deadline => {
                        break Err(held_reason(a, &s.host_name))
                    }
                    _ => std::thread::sleep(Duration::from_millis(250)),
                }
                if let Some(r) = s.ended() {
                    break Err(format!("The session ended: {r}"));
                }
                if self.waits.generation() != generation {
                    break Err("The turn was stopped.".into());
                }
            }
        };
        self.count_waited(started.elapsed());
        result
    }
}

impl Drop for Computer {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Whether process `pid` still runs.
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        // Signal 0: nothing sent, only whether it could be.
        unsafe { kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// What held the agent, for the model once it may act again.
fn hold_cause(state: AgentState) -> &'static str {
    let f = state.flags;
    if f & agent_state::TAKEN_OVER != 0 {
        "A person had the keyboard and mouse"
    } else if f & agent_state::SECURE_DESKTOP != 0 {
        "A secure screen (sign-in, lock or an admin prompt) was up"
    } else if f & agent_state::PAUSED != 0 {
        "A person paused the agent"
    } else if f & agent_state::LOCAL_INPUT != 0 {
        "Someone used the host's own keyboard or mouse"
    } else {
        "The agent's input was held"
    }
}

/// "12 s", "3 min 5 s".
fn spoken(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else if s.is_multiple_of(60) {
        format!("{} min", s / 60)
    } else {
        format!("{} min {} s", s / 60, s % 60)
    }
}

/// Why the agent may not act, in words an agent can act on.
pub fn held_reason(state: AgentState, host: &str) -> String {
    let f = state.flags;
    if f & agent_state::TAKEN_OVER != 0 {
        return format!(
            "A person took over the keyboard and mouse on {host}. Carry on: \
                your next action waits for them to hand back, then shows you the screen."
        );
    }
    if f & agent_state::SECURE_DESKTOP != 0 {
        return format!(
            "{host} shows a secure screen (sign-in, lock or an admin prompt): \
                only a person may answer it. Stop and ask for help."
        );
    }
    if f & agent_state::VIEW_ONLY != 0 {
        return format!(
            "This agent may only look at {host}: its access there is view-only. \
                Describe what to do instead of doing it."
        );
    }
    if f & agent_state::PAUSED != 0 {
        return format!(
            "A person paused the agent on {host}. Carry on: your next action \
                waits until they resume you, then shows you the screen."
        );
    }
    if f & agent_state::LOCAL_INPUT != 0 {
        return format!(
            "Someone is using {host}'s own keyboard or mouse. Carry on: your \
                next action waits until they stop for a few seconds, then shows you the screen."
        );
    }
    format!("The agent's input on {host} is held.")
}

fn thumbnail(shot: &Shot) -> Option<Shot> {
    shrink_png(&shot.png, 480)
}

/// A screenshot (as the computer takes them: RGB PNG) at most `max` pixels
/// on its longer side.
pub fn shrink_png(png: &[u8], max: u32) -> Option<Shot> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgb {
        return None;
    }
    buf.truncate(info.buffer_size());
    let small = Rgb {
        width: info.width,
        height: info.height,
        data: buf,
    }
    .fit(max);
    Some(Shot {
        png: small.png(),
        width: small.width,
        height: small.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_describe_themselves_briefly() {
        assert_eq!(
            Action::Click {
                at: Some((10, 20)),
                button: Mouse::Left,
                count: 2,
                modifiers: None
            }
            .describe(),
            "double_click (10, 20)"
        );
        assert_eq!(
            Action::Click {
                at: Some((1, 2)),
                button: Mouse::Right,
                count: 1,
                modifiers: Some("shift".into())
            }
            .describe(),
            "shift+right_click (1, 2)"
        );
        let long = Action::Type {
            text: "a".repeat(100),
        }
        .describe();
        assert!(long.len() < 40 && long.ends_with('…'), "{long}");
        assert!(AgentNote::new(&long).as_str().len() <= AgentNote::MAX);
    }

    #[test]
    fn waits_are_said_plainly() {
        assert_eq!(spoken(Duration::from_secs(12)), "12 s");
        assert_eq!(spoken(Duration::from_secs(180)), "3 min");
        assert_eq!(spoken(Duration::from_secs(245)), "4 min 5 s");
        let taken = AgentState {
            flags: agent_state::TAKEN_OVER,
            watchers: 1,
        };
        assert!(hold_cause(taken).starts_with("A person had"));
        let w = Waits::default();
        w.add_waited(Duration::from_secs(90));
        assert_eq!(w.waited(), Duration::from_secs(90));
        let g = w.generation();
        w.interrupt();
        assert_ne!(w.generation(), g);
    }

    #[test]
    fn held_reasons_name_what_holds_the_agent() {
        let r = held_reason(
            AgentState {
                flags: agent_state::SECURE_DESKTOP,
                watchers: 0,
            },
            "box",
        );
        assert!(r.contains("secure screen"));
        let r = held_reason(
            AgentState {
                flags: agent_state::TAKEN_OVER | agent_state::PAUSED,
                watchers: 1,
            },
            "box",
        );
        assert!(r.contains("took over"));
    }

    #[test]
    fn nothing_is_done_without_a_host() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Computer::new(Config::new(dir.path().to_path_buf()));
        let e = c.act(Action::Screenshot).err().unwrap();
        assert!(e.contains("none is paired"), "{e}");
        assert!(c.status().starts_with("Not connected"));
    }
}
