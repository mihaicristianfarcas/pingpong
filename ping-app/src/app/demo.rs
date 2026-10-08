//! PING_UI_DEMO: driving the window without clicking, for checks and screenshots (see docs/ui.md).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{Context, Window};

use crate::settings::Tab;

use super::{Page, PingApp};

/// PING_UI_DEMO drives the UI for automated checks and screenshots, a
/// comma-separated list: `pair=HOST[:PORT]` opens the pairing sheet, `add`
/// the add-host sheet, `alone` leaves out the hosts found on the network,
/// `size=WxH` makes the window that size (points), to see a long page
/// whole, `unpair=NAME` its confirmation, `menu=NAME` a host's menu,
/// `open=ID` opens the select with that id (`fps`, `codec`),
/// `settings[=video|audio|input|agents]` a settings page,
/// `agents[=setup|sample|sample-live|sample-ask]` the Agents page (the setup page or a
/// sample session), `chat=MESSAGE` a message to the agent session (a new one
/// on the first agent host; each waits for the turn before it), `stream=NAME` streams from that host,
/// `stop-after=SECS` ends a stream after that long as the user would,
/// `update` and `update-main` make the update check say there is a newer
/// release, or newer commits on main, `updates` opens its sheet, `menus`
/// writes the menu bar to the log, `snapshot=PATH` saves the window as a
/// PNG, `quit` quits after it.
#[derive(Default)]
pub(super) struct Demo {
    actions: Vec<String>,
    /// End the stream after this long, as the user would.
    pub(super) stop_after: Option<Duration>,
    /// When the next step is due.
    pub(super) at: Option<Instant>,
    snapshot: Option<PathBuf>,
    quit: bool,
    /// The snapshot is of a stream's page, taken while it runs.
    during_stream: bool,
}

impl Demo {
    /// The steps `PING_UI_DEMO` asks for, if it is set.
    pub(super) fn from_env() -> Demo {
        let mut demo = Demo::default();
        if let Ok(spec) = std::env::var("PING_UI_DEMO") {
            for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                if let Some(path) = part.strip_prefix("snapshot=") {
                    demo.snapshot = Some(PathBuf::from(path));
                } else if let Some(secs) = part.strip_prefix("stop-after=") {
                    demo.stop_after = secs.parse().ok().map(Duration::from_secs_f64);
                } else if part == "quit" {
                    demo.quit = true;
                } else {
                    demo.actions.push(part.to_string());
                }
            }
            demo.at = Some(Instant::now() + Duration::from_millis(1500));
        }
        demo
    }

    /// Whether the next steps are for while a stream runs (its page, the
    /// log-in window, a wait).
    pub(super) fn runs_during_stream(&self) -> bool {
        self.during_stream
            || self
                .actions
                .iter()
                .any(|a| a == "desktop" || a.ends_with("-login") || a.starts_with("wait="))
    }
}

impl PingApp {
    pub(super) fn run_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.demo.at.is_none_or(|at| Instant::now() < at) {
            return;
        }
        if !self.demo.actions.is_empty()
            && !self.demo.actions.iter().any(|a| a.starts_with("stream="))
        {
            // An occluded window is not redrawn: bring it forward.
            window.activate_window();
            cx.activate(true);
        }
        let mut queue = std::mem::take(&mut self.demo.actions).into_iter();
        while let Some(action) = queue.next() {
            // chat=MESSAGE: to the (first) agent session, once it is idle; a
            // new one on the first agent host if there is none.
            if let Some(message) = action.strip_prefix("chat=") {
                if self.agents.chats.iter().any(|c| c.busy()) {
                    self.demo.actions.push(action);
                    self.demo.actions.extend(queue);
                    break;
                }
                match self
                    .agents
                    .chats
                    .first()
                    .map(|c| (c.id, c.composer.clone()))
                {
                    Some((id, composer)) => {
                        composer.update(cx, |f, cx| f.set_text(message.to_string(), cx));
                        self.chat_send(id, cx);
                    }
                    None => {
                        self.page = Page::Agents;
                        self.demo_new_chat(message, window, cx);
                    }
                }
                self.demo.actions.extend(queue);
                break;
            }
            // desktop: the (first) own desktop session's page, while it
            // streams (as the Dock icon brings the window back).
            if action == "desktop" {
                match self.streams.iter().find(|s| s.chat.is_none()).map(|s| s.id) {
                    Some(id) => {
                        self.page = Page::Desktop(id);
                        pingpong_ui::set_window_visible(window, true);
                        self.demo.during_stream = true;
                        self.demo.at = Some(Instant::now());
                    }
                    None => self.demo.actions.push(action),
                }
                continue;
            }
            // wait=SECS: the next steps that much later.
            if let Some(secs) = action
                .strip_prefix("wait=")
                .and_then(|s| s.parse::<f64>().ok())
            {
                self.demo.at = Some(Instant::now() + Duration::from_secs_f64(secs));
                self.demo.actions.extend(queue);
                break;
            }
            // close-login: close the log-in window as its close button does,
            // and take the snapshot whatever streams are left.
            if action == "close-login" || action == "vanish-login" {
                #[cfg(target_os = "macos")]
                if let Some(s) = self.streams.iter().find(|s| s.chat.is_some()) {
                    if action == "close-login" {
                        s.session.perform_close();
                    } else {
                        s.session.order_out();
                    }
                }
                self.demo.during_stream = true;
                continue;
            }
            // step=N: show step N of the (first) agent session; panel-end:
            // scroll its panel to the bottom.
            if let Some(n) = action
                .strip_prefix("step=")
                .and_then(|v| v.parse::<usize>().ok())
            {
                if let Some(c) = self.agents.chats.first_mut() {
                    c.select_step(n.saturating_sub(1));
                }
                continue;
            }
            if action == "panel-end" {
                if let Some(c) = self.agents.chats.first() {
                    c.panel_scroll.scroll_to_bottom();
                }
                continue;
            }
            // login: log in to the (first) agent session's desktop.
            if action == "login" {
                if let Some(id) = self.agents.chats.first().map(|c| c.id) {
                    self.chat_log_in(id, cx);
                }
                continue;
            }
            if action == "update" {
                self.updates.pretend(Some(pingpong_update::Update::Release {
                    version: "0.7.0".into(),
                    url: format!("{}/releases", pingpong_update::REPOSITORY),
                }));
            } else if action == "update-main" {
                self.updates.pretend(Some(pingpong_update::Update::Commits {
                    ahead: 4,
                    url: format!("{}/commits/main", pingpong_update::REPOSITORY),
                }));
            } else if action == "updates" {
                // The sheet as it is, without asking GitHub.
                self.update_sheet = true;
            } else if action == "menus" {
                for line in pingpong_ui::desktop::menu_bar() {
                    tracing::info!(target: "menu_bar", "{line}");
                }
            } else if let Some(address) = action.strip_prefix("pair=") {
                let web = crate::model::web_url_for(address);
                self.pair_with(address.to_string(), address.to_string(), web, false, cx);
            } else if action == "add" {
                self.show_add_host(window, cx);
            } else if action == "alone" {
                self.model.alone = true;
                self.model.rebuild();
            } else if let Some((w, h)) = action
                .strip_prefix("size=")
                .and_then(|v| v.split_once('x'))
                .and_then(|(w, h)| Some((w.parse::<f32>().ok()?, h.parse::<f32>().ok()?)))
            {
                window.resize(gpui::size(gpui::px(w), gpui::px(h)));
            } else if let Some(name) = action.strip_prefix("unpair=") {
                match self.model.items.iter().find(|i| i.name == name).cloned() {
                    Some(item) => self.confirm_unpair(&item, cx),
                    None => self.demo.actions.push(action),
                }
            } else if let Some(id) = action.strip_prefix("open=") {
                pingpong_ui::open_select(id);
            } else if let Some(name) = action.strip_prefix("menu=") {
                match self.model.items.iter().find(|i| i.name == name).cloned() {
                    Some(item) => {
                        let at = window.bounds().size;
                        self.menu = Some((item, gpui::point(at.width * 0.32, at.height * 0.42)));
                    }
                    None => self.demo.actions.push(action),
                }
            } else if let Some(rest) = action.strip_prefix("agents") {
                let what = rest.trim_start_matches('=');
                if what == "setup" {
                    self.page = Page::Settings(Tab::Agents);
                } else {
                    self.page = Page::Agents;
                    self.agents_demo(what, window, cx);
                }
            } else if let Some(tab) = action.strip_prefix("settings") {
                self.page = Page::Settings(Tab::parse(tab.trim_start_matches('=')));
            } else if let Some(name) = action.strip_prefix("stream=") {
                match self.model.items.iter().find(|i| i.name == name).cloned() {
                    Some(item) => self.stream(&item, false, window, cx),
                    // Not listed yet: try again next tick.
                    None => self.demo.actions.push(action),
                }
            }
        }
        cx.notify();
        // A stream started (or still to end), or an agent at work: the
        // snapshot waits for it.
        if self.agents.is_running()
            && !self
                .agents
                .chats
                .iter()
                .any(|c| c.title == "Clean up the Downloads folder")
            || !self.streams.is_empty() && !self.demo.during_stream
        {
            return;
        }
        if self.demo.stop_after.is_some()
            && self.demo.actions.iter().any(|a| a.starts_with("stream="))
        {
            return;
        }
        if !self.demo.actions.is_empty() {
            return;
        }
        match &self.demo.snapshot {
            Some(path) => {
                // Let the page settle for a moment first.
                if self
                    .demo
                    .at
                    .is_some_and(|at| at.elapsed() > Duration::from_millis(900))
                {
                    match pingpong_ui::snapshot(window, path) {
                        Ok(()) => tracing::info!(path = %path.display(), "snapshot saved"),
                        Err(e) => tracing::warn!(error = e, "snapshot not saved"),
                    }
                    self.demo.snapshot = None;
                    self.demo.at = None;
                    if self.demo.quit {
                        cx.quit();
                    }
                }
            }
            None => {
                self.demo.at = None;
                if self.demo.quit {
                    cx.quit();
                }
            }
        }
    }
}
