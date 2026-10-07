//! The window: a sidebar (Hosts, Agents, the open sessions, the settings
//! pages) beside the page it selects, and the sheets over them (pairing,
//! adding a host, unpairing, alerts). The logic under it is the same the
//! `pingctl` CLI drives.
//!
//! Sessions: your own desktop streams (until the stream ends, from its window
//! or its hotkey) and agent sessions (until you end them). Neither outlives
//! the app.

mod actions;
mod demo;
mod desktop;
mod sheets;
mod sidebar;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use futures::StreamExt;
use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, FocusHandle, FontWeight, Pixels, Point,
    SharedString, Window,
};
use ping_core::session::{self, NativeMode, Session};
use pingpong_ui::updates::UpdateApp;
use pingpong_ui::{FieldEvent, TextField, Theme, Type};
use pingpong_update::{Build, Channel, Checker, Install, Program};

use crate::agents::AgentsState;
use crate::model::{Item, Model, Pairing};
use crate::prefs::Prefs;
use crate::settings::Tab;
use demo::Demo;

/// Ping, to the update check.
pub const UPDATE_APP: UpdateApp = UpdateApp {
    program: Program::Ping,
    build: Build::this(),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Hosts,
    /// A new agent session.
    Agents,
    /// An open agent session.
    Chat(u64),
    /// One of your desktop streams.
    Desktop(u64),
    /// Xbox: the account's consoles and cloud games.
    Xbox,
    Settings(Tab),
}

/// Background work says there is news: the window takes it in.
#[derive(Clone)]
pub struct Waker(futures::channel::mpsc::UnboundedSender<()>);

impl Waker {
    pub fn wake(&self) {
        let _ = self.0.unbounded_send(());
    }
}

/// A stream window: your desktop, or you logged in to an agent session's.
struct StreamSession {
    id: u64,
    host: String,
    session: Session,
    since: Instant,
    /// The agent session this logs in to (a watcher's stream).
    chat: Option<u64>,
    /// The mode asked for, to say.
    mode: (u16, u16, u32),
    fullscreen: bool,
    /// From an Xbox, not a Pong host.
    xbox: bool,
}

struct Alert {
    title: String,
    message: String,
}

pub struct PingApp {
    pub model: Model,
    pub prefs: Prefs,
    pub dir: PathBuf,
    pub page: Page,
    pairing: Option<Pairing>,
    /// The add-host sheet's field, while it is open.
    add_host: Option<Entity<TextField>>,
    confirm_unpair: Option<Item>,
    alert: Option<Alert>,
    /// A host's menu, and where it opened.
    pub menu: Option<(Item, Point<Pixels>)>,
    streams: Vec<StreamSession>,
    next_stream: u64,
    ended_tx: Sender<(u64, Option<String>)>,
    ended_rx: Receiver<(u64, Option<String>)>,
    native: Option<(NativeMode, Instant)>,
    /// What "Import from Moonlight" did.
    pub imported: Option<String>,
    demo: Demo,
    pub waker: Waker,
    pub agents: AgentsState,
    pub xbox: crate::xbox::XboxState,
    /// Notifications about agent sessions not on screen.
    pub notifier: crate::notify::Notifier,
    /// The update check, and what it said when last looked at.
    pub updates: Checker,
    pub update_status: pingpong_update::Status,
    /// The sheet that says what the update check found.
    update_sheet: bool,
    focus: FocusHandle,
}

impl PingApp {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> PingApp {
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        let waker = Waker(tx);
        // News from the poll, pairing and agent threads.
        cx.spawn_in(window, async move |this, cx| {
            while rx.next().await.is_some() {
                while rx.try_recv().is_ok() {}
                if this
                    .update_in(cx, |this, window, cx| this.poll(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        // A second's tick: "Waking…", elapsed times, the demo's timing.
        cx.spawn_in(window, async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            if this
                .update_in(cx, |this, window, cx| this.tick(window, cx))
                .is_err()
            {
                break;
            }
        })
        .detach();
        cx.observe_window_appearance(window, |_, _, cx| cx.notify())
            .detach();

        let model = {
            let waker = waker.clone();
            Model::new(move || waker.wake())
        };
        let dir = ping_core::store::data_dir();
        let (ended_tx, ended_rx) = crossbeam_channel::unbounded();
        let demo = Demo::from_env();
        let prefs = Prefs::load(&dir);
        // A check asks GitHub nothing on its own: what it shows is made up
        // (PING_UI_DEMO's `update`), or asked for by hand.
        let channel = if std::env::var_os("PING_UI_DEMO").is_some() {
            Channel::Off
        } else {
            prefs.update_channel()
        };
        let updates = {
            let waker = waker.clone();
            Checker::start(
                Program::Ping,
                Build::this(),
                dir.clone(),
                channel,
                move || waker.wake(),
            )
        };
        let agents = AgentsState::new(dir.clone(), window, cx);
        let xbox = {
            let waker = waker.clone();
            crate::xbox::XboxState::new(std::sync::Arc::new(move || waker.wake()))
        };
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        PingApp {
            model,
            prefs,
            dir,
            page: Page::Hosts,
            pairing: None,
            add_host: None,
            confirm_unpair: None,
            alert: None,
            menu: None,
            streams: Vec::new(),
            next_stream: 1,
            ended_tx,
            ended_rx,
            native: None,
            imported: None,
            demo,
            notifier: crate::notify::Notifier::new(waker.clone()),
            waker,
            agents,
            xbox,
            updates,
            update_status: pingpong_update::Status::default(),
            update_sheet: false,
            focus,
        }
    }

    /// This display's native mode, looked up at most once a second.
    pub fn native(&mut self, window: &Window, cx: &App) -> NativeMode {
        match self.native {
            Some((m, at)) if at.elapsed() < Duration::from_secs(1) => m,
            _ => {
                #[allow(unused_mut)]
                let mut m = session::native_mode();
                // Linux: the monitor this window is on (the refresh rate is
                // not known: 60).
                #[cfg(target_os = "linux")]
                if let Some(display) = window.display(cx) {
                    let b = display.bounds();
                    let scale = window.scale_factor();
                    m.width = ((f32::from(b.size.width) * scale).round() as u16) & !1;
                    m.height = ((f32::from(b.size.height) * scale).round() as u16) & !1;
                }
                let _ = (window, cx);
                self.native = Some((m, Instant::now()));
                m
            }
        }
    }

    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        while let Ok((id, reason)) = self.ended_rx.try_recv() {
            self.stream_ended(id, reason, window, cx);
        }
        let mut changed = self.model.update();
        if let Some(p) = &mut self.pairing {
            let before = p.state.clone();
            if p.update() {
                self.model.refresh();
                self.model.rebuild();
            }
            changed |= p.state != before;
        }
        changed |= self.agents.update();
        changed |= self.xbox.update();
        changed |= self.notifications(window, cx);
        let updates = self.updates.status();
        if updates != self.update_status {
            match &updates.install {
                // The new copy is in place, or its installer waits for this
                // one to quit.
                Some(Install::Restarting) => cx.quit(),
                // An update that did not take (this start found it so):
                // the sheet says why.
                Some(Install::Failed(_))
                    if !matches!(self.update_status.install, Some(Install::Failed(_))) =>
                {
                    self.update_sheet = true;
                }
                _ => {}
            }
            self.update_status = updates;
            changed = true;
        }
        // Logging in to an agent session that was connecting.
        let ready: Vec<u64> = self
            .agents
            .chats
            .iter()
            .filter(|c| c.watch_pending && c.is_connected())
            .map(|c| c.id)
            .collect();
        for id in ready {
            if let Some(c) = self.agents.chats.iter_mut().find(|c| c.id == id) {
                c.watch_pending = false;
            }
            self.watch_chat(id, cx);
        }
        if changed {
            cx.notify();
        }
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.set_foreground(window.is_window_active());
        // Keep "Waking…" and the searching spinner honest without input.
        if self.model.update() || self.agents.is_running() || !self.streams.is_empty() {
            cx.notify();
        }
        // A stream window closed some way that did not end its stream (it
        // would go on, unseen, and a log-in could not be opened again).
        let gone: Vec<u64> = self
            .streams
            .iter()
            .filter(|s| s.session.window_gone())
            .map(|s| s.id)
            .collect();
        for id in gone {
            tracing::warn!(id, "a stream's window is gone; ending the stream");
            self.stream_ended(id, None, window, cx);
        }
        // A session's agent that starts or stops waiting for the user, and
        // its connection's figures, say nothing: look.
        if self.agents.update() | self.notifications(window, cx) {
            cx.notify();
        }
        let watched: Vec<u64> = self.streams.iter().filter_map(|s| s.chat).collect();
        for c in &mut self.agents.chats {
            c.idle_check(watched.contains(&c.id));
        }
        match self.streams.first().map(|s| (s.id, s.since)) {
            None => self.run_demo(window, cx),
            Some((id, since)) => {
                if self.demo.runs_during_stream() {
                    self.run_demo(window, cx);
                }
                if let Some(after) = self.demo.stop_after {
                    if since.elapsed() >= after {
                        self.demo.stop_after = None;
                        tracing::info!("demo: ending the stream");
                        self.stream_ended(id, None, window, cx);
                    }
                }
            }
        }
    }

    /// Show what sessions not on screen have to say, withdraw requests that
    /// were answered, and act on what was clicked. True if anything changed.
    fn notifications(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let mut changed = false;
        let front = window.is_window_active();
        for c in &mut self.agents.chats {
            let on_screen = front && self.page == Page::Chat(c.id);
            for n in std::mem::take(&mut c.notices) {
                if !on_screen {
                    self.notifier.post(c.id, &n);
                }
            }
            if c.question().is_none() && self.notifier.has_ask(c.id) {
                self.notifier.answered(c.id);
            }
        }
        for tap in self.notifier.taps() {
            changed = true;
            match tap {
                crate::notify::Tap::Open(id) => {
                    if self.agents.chats.iter().any(|c| c.id == id) {
                        self.set_page(Page::Chat(id), cx);
                        window.activate_window();
                    }
                }
                crate::notify::Tap::Answer(id, yes) => {
                    if let Some(c) = self.agents.chats.iter_mut().find(|c| c.id == id) {
                        c.answer(yes);
                    }
                    self.notifier.answered(id);
                }
            }
        }
        changed
    }

    // -----------------------------------------------------------------------
    // What clicks do

    pub fn set_page(&mut self, page: Page, cx: &mut Context<Self>) {
        if self.page != page {
            tracing::debug!(?page, "page");
        }
        self.page = page;
        self.menu = None;
        cx.notify();
    }

    /// A card's click: wake, stream or pair, whichever it needs.
    pub fn open(&mut self, item: &Item, window: &mut Window, cx: &mut Context<Self>) {
        self.menu = None;
        if item.offline_but_wakeable() {
            self.wake(item, cx);
        } else if item.is_paired() {
            self.stream(item, false, window, cx);
        } else {
            self.start_pairing(item, cx);
        }
    }

    pub fn wake(&mut self, item: &Item, cx: &mut Context<Self>) {
        self.menu = None;
        tracing::info!(host = item.name, "waking the host");
        if let Err(e) = self.model.wake(item) {
            tracing::warn!(host = item.name, error = e, "wake failed");
            self.alert = Some(Alert {
                title: "Cannot wake the host".into(),
                message: e,
            });
        }
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.model.refresh();
        cx.notify();
    }

    pub fn start_pairing(&mut self, item: &Item, cx: &mut Context<Self>) {
        self.menu = None;
        if let Some(address) = item.pairing_address() {
            self.pair_with(item.name.clone(), address, item.web_url(), false, cx);
        }
    }

    pub fn pair_with(
        &mut self,
        title: String,
        address: String,
        web_url: Option<String>,
        agent: bool,
        cx: &mut Context<Self>,
    ) {
        tracing::info!(host = title, address, agent, "pairing");
        let waker = self.waker.clone();
        let title = if agent {
            format!("{title} for the agent")
        } else {
            title
        };
        self.pairing = Some(Pairing::start_as(
            title,
            address,
            web_url,
            agent,
            move || waker.wake(),
        ));
        cx.notify();
    }

    pub fn show_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let field = cx.new(|cx| TextField::new(cx).placeholder("192.168.1.20 or gaming-pc.local"));
        cx.subscribe_in(&field, window, |this, field, event, _, cx| match event {
            FieldEvent::Submit => {
                let address = field.read(cx).text().trim().to_string();
                if !address.is_empty() {
                    this.add_host = None;
                    let web = crate::model::web_url_for(&address);
                    this.pair_with(address.clone(), address, web, false, cx);
                }
            }
            FieldEvent::Cancel => {
                this.add_host = None;
                cx.notify();
            }
            FieldEvent::Changed => cx.notify(),
        })
        .detach();
        TextField::focus(&field, window, cx);
        self.add_host = Some(field);
        self.menu = None;
        cx.notify();
    }

    pub fn confirm_unpair(&mut self, item: &Item, cx: &mut Context<Self>) {
        self.menu = None;
        self.confirm_unpair = Some(item.clone());
        cx.notify();
    }

    pub fn open_url(&mut self, url: &str, cx: &mut Context<Self>) {
        self.menu = None;
        cx.open_url(url);
        cx.notify();
    }

    /// Log in to an agent session's desktop: a stream window on the agent's
    /// screen beside the app (which stays: the conversation goes on in it).
    /// The watcher is you (Ping's own identity); Ctrl+Alt+Shift+T in the
    /// window takes over and gives back.
    pub fn watch_chat(&mut self, chat: u64, cx: &mut Context<Self>) {
        if self.is_watching(chat) {
            return;
        }
        let Some(c) = self.agents.chats.iter().find(|c| c.id == chat) else {
            return;
        };
        let host = c.host.clone();
        // The host as Ping knows it (the same host, maybe named otherwise).
        let key = ping_agent::headless::agent_hosts(&self.dir)
            .into_iter()
            .find(|h| h.name == host)
            .map(|h| h.x25519)
            .unwrap_or_else(|| host.clone());
        let (width, height) = (self.agents.settings.width, self.agents.settings.height);
        let request = session::StreamRequest {
            width,
            height,
            fps: 30,
            bitrate_kbps: 12_000,
            fullscreen: false,
            audio_channels: 0,
            watch: true,
            clipboard: false,
            ..session::StreamRequest::default()
        };
        let id = self.next_stream;
        self.next_stream += 1;
        tracing::info!(host, chat, "logging in to the agent's desktop");
        match session::start(&key, &request, self.on_end(id)) {
            Ok(session) => self.streams.push(StreamSession {
                id,
                host,
                session,
                since: Instant::now(),
                chat: Some(chat),
                mode: (width, height, 30),
                fullscreen: false,
                xbox: false,
            }),
            Err(e) => {
                tracing::warn!(host, error = e, "could not log in to the agent's desktop");
                self.alert = Some(Alert {
                    title: format!("Cannot log in to {host}"),
                    message: e,
                });
            }
        }
        cx.notify();
    }

    pub fn is_watching(&self, chat: u64) -> bool {
        self.streams.iter().any(|s| s.chat == Some(chat))
    }

    /// Bring an agent session's log-in window forward.
    pub fn show_login(&self, chat: u64) {
        if let Some(s) = self.streams.iter().find(|s| s.chat == Some(chat)) {
            s.session.show();
        }
    }

    /// Close the log-in window of an agent session.
    pub fn stop_watching(&mut self, chat: u64) {
        while let Some(i) = self.streams.iter().position(|s| s.chat == Some(chat)) {
            let mut s = self.streams.remove(i);
            s.session.close();
        }
    }

    pub fn show_alert(&mut self, title: &str, message: &str, cx: &mut Context<Self>) {
        self.alert = Some(Alert {
            title: title.to_string(),
            message: message.to_string(),
        });
        cx.notify();
    }

    fn on_end(&self, id: u64) -> session::EndCallback {
        let (tx, waker) = (self.ended_tx.clone(), self.waker.clone());
        Arc::new(move |reason| {
            let _ = tx.send((id, reason));
            waker.wake();
        })
    }

    /// Stream from `item`, Steam Big Picture when `steam` (opened on the host
    /// for the stream and closed after, as Sunshine's Steam Big Picture app does). The stream has
    /// its own window; ours gets out of the way, as Moonlight's does.
    pub fn stream(
        &mut self,
        item: &Item,
        steam: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.menu = None;
        let Some(key) = item.paired.as_ref().map(|p| p.key.clone()) else {
            return;
        };
        // One desktop at a time (a watcher's windows aside).
        if let Some(open) = self.streams.iter().find(|s| s.chat.is_none()) {
            let id = open.id;
            self.set_page(Page::Desktop(id), cx);
            return;
        }
        let native = self.native(window, cx);
        let request = self.prefs.request(&native, steam);
        tracing::info!(
            host = item.name,
            width = request.width,
            height = request.height,
            fps = request.fps,
            kbps = request.bitrate_kbps,
            fullscreen = request.fullscreen,
            steam,
            "streaming"
        );
        let id = self.next_stream;
        self.next_stream += 1;
        match session::start(&key, &request, self.on_end(id)) {
            Ok(session) => {
                self.model.set_paused(true);
                self.streams.push(StreamSession {
                    id,
                    host: item.name.clone(),
                    session,
                    since: Instant::now(),
                    chat: None,
                    mode: (request.width, request.height, request.fps),
                    fullscreen: request.fullscreen,
                    xbox: false,
                });
            }
            Err(e) => {
                tracing::warn!(host = item.name, error = e, "stream not started");
                self.alert = Some(Alert {
                    title: format!("Cannot stream from {}", item.name),
                    message: e,
                });
            }
        }
        cx.notify();
    }

    /// Stream from an Xbox: a console of the account's, or a cloud game. As
    /// [`PingApp::stream`], in a window of its own.
    pub fn stream_xbox(
        &mut self,
        target: ping_core::xbox::Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(open) = self.streams.iter().find(|s| s.chat.is_none()) {
            let id = open.id;
            self.set_page(Page::Desktop(id), cx);
            return;
        }
        let native = self.native(window, cx);
        let request = self.prefs.request(&native, false);
        let name = target.name().to_owned();
        tracing::info!(
            target = name,
            width = request.width,
            height = request.height,
            "streaming from an Xbox"
        );
        let source = ping_core::xbox::XboxSource {
            target,
            keyboard_as_controller: self.prefs.xbox_keyboard_as_controller,
            region: None,
        };
        let id = self.next_stream;
        self.next_stream += 1;
        match session::start_xbox(source, &request, self.on_end(id)) {
            Ok(session) => {
                self.model.set_paused(true);
                self.streams.push(StreamSession {
                    id,
                    host: name,
                    session,
                    since: Instant::now(),
                    chat: None,
                    mode: (request.width, request.height, 60),
                    fullscreen: request.fullscreen,
                    xbox: true,
                });
            }
            Err(e) => {
                tracing::warn!(target = name, error = e, "Xbox stream not started");
                self.alert = Some(Alert {
                    title: format!("Cannot stream {name}"),
                    message: e,
                });
            }
        }
        cx.notify();
    }

    /// The stream ended (the user quit, or it failed): close it, come back.
    fn stream_ended(
        &mut self,
        id: u64,
        reason: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.streams.iter().position(|s| s.id == id) else {
            return;
        };
        let mut s = self.streams.remove(i);
        s.session.close();
        tracing::info!(
            host = s.host,
            secs = s.since.elapsed().as_secs(),
            watcher = s.chat.is_some(),
            reason = reason.as_deref().unwrap_or("quit"),
            "stream ended"
        );
        if s.chat.is_some() {
            // A log-in window closed: the agent session goes on.
            cx.notify();
            return;
        }
        if self.page == Page::Desktop(id) {
            self.page = if s.xbox { Page::Xbox } else { Page::Hosts };
        }
        self.model.set_paused(false);
        if self.demo.at.is_some() {
            // A demo snapshot after the stream: let the window come back first.
            self.demo.at = Some(Instant::now());
        }
        pingpong_ui::set_window_visible(window, true);
        cx.activate(true);
        if let Some(message) = reason {
            self.alert = Some(Alert {
                title: "Stream ended".into(),
                message,
            });
        }
        self.model.refresh();
        cx.notify();
    }

    pub fn save_prefs(&mut self, cx: &mut Context<Self>) {
        // What changed, setting by setting.
        let before = crate::prefs::Prefs::load(&self.dir);
        if let (Ok(serde_json::Value::Object(a)), Ok(serde_json::Value::Object(b))) = (
            serde_json::to_value(&before),
            serde_json::to_value(&self.prefs),
        ) {
            for (k, v) in &b {
                if a.get(k) != Some(v) {
                    tracing::info!(setting = k, from = %a.get(k).cloned().unwrap_or_default(), to = %v, "setting changed");
                }
            }
        }
        if before.update_channel() != self.prefs.update_channel() {
            self.updates.set_channel(self.prefs.update_channel());
        }
        self.prefs.save(&self.dir);
        cx.notify();
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        // The topmost thing open closes: a menu, a sheet, pairing, a page.
        let closed = self.menu.take().is_some()
            || self.add_host.take().is_some()
            || self.confirm_unpair.take().is_some()
            || self.alert.take().is_some()
            || self.close_update_sheet();
        if !closed {
            if let Some(p) = self.pairing.take() {
                p.cancel();
            } else if self.xbox.sign_in.is_some() {
                self.xbox.cancel_sign_in();
            } else if matches!(
                self.page,
                Page::Settings(_) | Page::Agents | Page::Desktop(_) | Page::Xbox
            ) {
                self.page = Page::Hosts;
            }
        }
        cx.notify();
    }

    /// Put the update sheet away, and what it said about an installation
    /// that did not take; whether it was open.
    pub(super) fn close_update_sheet(&mut self) -> bool {
        self.updates.dismiss_install();
        std::mem::take(&mut self.update_sheet)
    }

    fn sheet_open(&self) -> bool {
        self.pairing.is_some()
            || self.xbox.sign_in.is_some()
            || self.add_host.is_some()
            || self.confirm_unpair.is_some()
            || self.alert.is_some()
            || self.update_sheet
    }

    // -----------------------------------------------------------------------
    // The demo driver

    // -----------------------------------------------------------------------
    // Drawing
}

impl Render for PingApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = Theme::of(window);
        let page: AnyElement = match self.page {
            Page::Hosts => self.render_hosts(t, window, cx),
            Page::Agents => self.render_agents(t, window, cx),
            Page::Chat(id) => self.render_chat(id, t, window, cx),
            Page::Desktop(id) => self.render_desktop(id, t, cx),
            Page::Xbox => self.render_xbox(t, window, cx),
            Page::Settings(tab) => self.render_settings(tab, t, window, cx),
        };
        let sheet = self.sheets(t, window, cx);
        let menu = self.host_menu(t, cx);
        self.on_actions(div().key_context("PingApp").track_focus(&self.focus), cx)
            .size_full()
            .relative()
            .flex()
            .bg(t.background)
            .font_family(pingpong_ui::ui_font())
            .text_color(t.primary)
            .text_size(px(Type::BODY))
            .child(self.sidebar(t, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(page),
            )
            .children(menu)
            .children(sheet)
    }
}

pub use pingpong_ui::toolbar_row as toolbar;

/// A page's scrolling body, its content in a centred column.
pub fn page_body(id: &'static str, max_width: f32, content: impl IntoElement) -> impl IntoElement {
    div().id(id).flex_1().min_h_0().overflow_y_scroll().child(
        div()
            .w_full()
            .flex()
            .justify_center()
            .px(px(28.0))
            .pb(px(32.0))
            .child(div().w_full().max_w(px(max_width)).child(content)),
    )
}

pub fn sheet_title(text: impl Into<SharedString>, t: Theme) -> gpui::Div {
    div()
        .text_size(px(17.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.primary)
        .child(text.into())
}

pub fn sheet_text(text: impl Into<SharedString>, t: Theme) -> gpui::Div {
    div()
        .text_size(px(Type::BODY))
        .line_height(px(18.0))
        .text_color(t.secondary)
        .child(text.into())
}

/// A sheet's buttons, at its bottom right, the one that acts last.
pub fn sheet_buttons() -> gpui::Div {
    div().pt(px(8.0)).flex().justify_end().gap(px(8.0))
}

fn info(label: &'static str, value: String, t: Theme) -> AnyElement {
    div()
        .min_h(px(38.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .child(
            div()
                .text_size(px(Type::BODY))
                .text_color(t.secondary)
                .child(label),
        )
        .child(div().text_size(px(Type::BODY)).child(value))
        .into_any_element()
}

/// "Mac" or "PC": what the user calls this computer.
pub fn device_word() -> &'static str {
    if cfg!(target_os = "macos") {
        "Mac"
    } else {
        "PC"
    }
}
