//! Agents: sessions with an AI agent that uses one of your hosts. This page
//! starts one (the host, the model, the first message); each session then
//! lives in the sidebar (see `chat`) until you end it. The agent is this
//! device's own identity on the host (paired once, from here), and the model
//! is one you already have: a Claude or ChatGPT plan through Claude Code or
//! Codex, or an API key.
//!
//! The setup (models, keys, limits, which hosts, other agents through
//! `Ping mcp`) is a settings page of its own, also here.

mod setup;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, AnyElement, Context, ElementId, Entity, FontWeight, Window};
use ping_agent::providers::{self, AgentSettings, Approvals, Provider};
use pingpong_ui::{
    button, chip, field, icon, rows, section, select, setting, Choice, FieldEvent, IconName, Ink,
    Radius, TextField, Theme, Type,
};

use crate::app::{page_body, toolbar, Page, PingApp};
use crate::chat::{provider_name, Chat};
use crate::settings::Tab;

fn short(p: Provider) -> &'static str {
    provider_name(p)
}

pub struct AgentsState {
    dir: PathBuf,
    pub settings: AgentSettings,
    /// The host a new session is for.
    host: String,
    availability: Vec<(Provider, Result<String, String>)>,
    checked: Option<Instant>,
    key_note: Option<String>,
    mcp_note: Option<String>,
    /// A new session's first message.
    composer: Entity<TextField>,
    key_field: Entity<TextField>,
    /// Jev's key (TypeSafe's or OpenRouter's), and what saving it said.
    jev_key: Entity<TextField>,
    jev_note: Option<String>,
    base_url: Entity<TextField>,
    program: Entity<TextField>,
    /// The open sessions, newest last.
    pub chats: Vec<Chat>,
    next_id: u64,
}

impl AgentsState {
    pub fn new(dir: PathBuf, window: &mut Window, cx: &mut Context<PingApp>) -> AgentsState {
        let settings = AgentSettings::load(&dir);
        let composer = cx.new(|cx| {
            TextField::new(cx)
                .multiline()
                .placeholder("Ask the agent, or give it something to do…")
        });
        cx.subscribe_in(
            &composer,
            window,
            |this: &mut PingApp, _, event, window, cx| match event {
                FieldEvent::Submit => this.start_chat(window, cx),
                FieldEvent::Changed => cx.notify(),
                FieldEvent::Cancel => {}
            },
        )
        .detach();
        let key_field = cx.new(|cx| TextField::new(cx).password().placeholder("Paste a key"));
        cx.subscribe_in(
            &key_field,
            window,
            |this: &mut PingApp, _, event, _, cx| match event {
                FieldEvent::Submit => this.agents.save_key(cx),
                _ => cx.notify(),
            },
        )
        .detach();
        let jev_key = cx.new(|cx| TextField::new(cx).password().placeholder("Paste a key"));
        cx.subscribe_in(
            &jev_key,
            window,
            |this: &mut PingApp, _, event, _, cx| match event {
                FieldEvent::Submit => this.agents.save_jev_key(cx),
                _ => cx.notify(),
            },
        )
        .detach();
        let base_url = cx.new(|cx| {
            let mut f = TextField::new(cx).placeholder("http://localhost:11434/v1");
            f.set_text(settings.base_url.clone(), cx);
            f
        });
        cx.subscribe_in(&base_url, window, |this: &mut PingApp, f, event, _, cx| {
            if *event == FieldEvent::Changed {
                this.agents.settings.base_url = f.read(cx).text().trim().to_string();
                this.agents.save_settings();
                this.agents.checked = None;
                cx.notify();
            }
        })
        .detach();
        let program = cx.new(|cx| TextField::new(cx).placeholder("Found by itself"));
        cx.subscribe_in(&program, window, |this: &mut PingApp, f, event, _, cx| {
            if *event == FieldEvent::Changed {
                let text = f.read(cx).text().trim().to_string();
                match this.agents.settings.provider {
                    Provider::Codex => this.agents.settings.codex_path = text,
                    _ => this.agents.settings.claude_path = text,
                }
                this.agents.save_settings();
                this.agents.checked = None;
                cx.notify();
            }
        })
        .detach();
        let mut state = AgentsState {
            dir,
            settings,
            host: String::new(),
            availability: Vec::new(),
            checked: None,
            key_note: None,
            mcp_note: None,
            composer,
            key_field,
            jev_key,
            jev_note: None,
            base_url,
            program,
            chats: Vec::new(),
            next_id: 1,
        };
        state.sync_program(cx);
        state
    }

    pub fn is_running(&self) -> bool {
        self.chats.iter().any(Chat::busy)
    }

    pub fn awaiting_answer(&self) -> bool {
        self.chats.iter().any(Chat::awaiting)
    }

    fn save_settings(&self) {
        if let Err(e) = self.settings.save(&self.dir) {
            tracing::warn!(error = %e, "agent settings not saved");
        }
    }

    fn sync_program(&mut self, cx: &mut Context<PingApp>) {
        let path = match self.settings.provider {
            Provider::Codex => self.settings.codex_path.clone(),
            _ => self.settings.claude_path.clone(),
        };
        self.program.update(cx, |f, cx| f.set_text(path, cx));
    }

    fn save_key(&mut self, cx: &mut Context<PingApp>) {
        let key = self.key_field.read(cx).text().trim().to_string();
        if key.is_empty() {
            return;
        }
        self.key_note = Some(
            match providers::secrets::set(&self.dir, self.settings.provider, Some(&key)) {
                Ok(()) => {
                    tracing::info!(provider = self.settings.provider.id(), "API key saved");
                    "Saved, readable only by you.".into()
                }
                Err(e) => e.to_string(),
            },
        );
        self.key_field.update(cx, |f, cx| f.set_text("", cx));
        self.checked = None;
        cx.notify();
    }

    fn save_jev_key(&mut self, cx: &mut Context<PingApp>) {
        let key = self.jev_key.read(cx).text().trim().to_string();
        if key.is_empty() {
            return;
        }
        if let Err(e) =
            providers::secrets::set_named(&self.dir, ping_agent::jev::KEY_ID, Some(&key))
        {
            self.jev_note = Some(e.to_string());
            cx.notify();
            return;
        }
        tracing::info!("Jev key saved");
        self.jev_note = Some("Saved, readable only by you. Checking it…".into());
        self.jev_key.update(cx, |f, cx| f.set_text("", cx));
        // Asked at once (nothing is charged for it), off the window's
        // thread: a wrong key would otherwise only show as checks that
        // never come.
        {
            let jev = ping_agent::jev::Jev::with_key(key);
            cx.spawn(async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { jev.check() })
                    .await;
                let _ = this.update(cx, |app, cx| {
                    let used = ping_agent::jev::key_variable()
                        .map(|v| format!(" {v} is set, though: its key is used instead."))
                        .unwrap_or_default();
                    app.agents.jev_note = Some(match result {
                        Ok(ok) => format!("Saved, readable only by you. {ok}{used}"),
                        Err(e) => format!("Saved, but {e}{used}"),
                    });
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    fn check_providers(&mut self) {
        if self
            .checked
            .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        self.checked = Some(Instant::now());
        self.availability = Provider::ALL
            .iter()
            .map(|&p| (p, providers::availability(&self.dir, &self.settings, p)))
            .collect();
    }

    fn ready(&self, p: Provider) -> Option<&Result<String, String>> {
        self.availability
            .iter()
            .find(|(q, _)| *q == p)
            .map(|(_, s)| s)
    }

    /// Take in what the sessions heard; true if anything changed.
    pub fn update(&mut self) -> bool {
        let mut changed = false;
        for c in &mut self.chats {
            changed |= c.update();
        }
        changed
    }
}

// ---------------------------------------------------------------------------
// A new session

impl PingApp {
    fn agent_hosts(&self) -> Vec<(String, String)> {
        ping_agent::headless::agent_hosts(&self.dir)
            .into_iter()
            .map(|h| (h.name, h.x25519))
            .collect()
    }

    /// Send the composer's words: to the host's open session, or to a new one.
    pub fn start_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.agents.composer.read(cx).text().trim().to_string();
        let host = self.agents.host.clone();
        if text.is_empty() || host.is_empty() {
            return;
        }
        if !self
            .agents
            .ready(self.agents.settings.provider)
            .is_some_and(|s| s.is_ok())
        {
            return;
        }
        let id = match self
            .agents
            .chats
            .iter()
            .find(|c| c.host == host)
            .map(|c| c.id)
        {
            Some(id) => id,
            None => {
                let id = self.agents.next_id;
                self.agents.next_id += 1;
                // Its notifications need the system's permission (asked once).
                self.notifier.authorize();
                match Chat::open(
                    id,
                    &host,
                    self.agents.settings.clone(),
                    self.waker.clone(),
                    window,
                    cx,
                ) {
                    Ok(c) => self.agents.chats.push(c),
                    Err(e) => {
                        tracing::warn!(error = e, host, "agent session not opened");
                        self.show_alert("Could not start the session", &e, cx);
                        return;
                    }
                }
                id
            }
        };
        self.agents.composer.update(cx, |f, cx| f.set_text("", cx));
        if let Some(c) = self.agents.chats.iter_mut().find(|c| c.id == id) {
            c.composer.update(cx, |f, cx| f.set_text(text, cx));
        }
        self.set_page(Page::Chat(id), cx);
        self.chat_send(id, cx);
        if let Some(c) = self.agents.chats.iter().find(|c| c.id == id) {
            TextField::focus(&c.composer, window, cx);
        }
    }

    /// PING_UI_DEMO: a session's first message, typed and sent.
    pub fn demo_new_chat(&mut self, message: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.agents.check_providers();
        if self.agents.host.is_empty() {
            self.agents.host = self
                .agent_hosts()
                .first()
                .map(|h| h.0.clone())
                .unwrap_or_default();
        }
        self.agents
            .composer
            .update(cx, |f, cx| f.set_text(message.to_string(), cx));
        self.start_chat(window, cx);
    }

    /// PING_UI_DEMO: `sample`/`sample-live` show a made-up session.
    pub fn agents_demo(&mut self, what: &str, window: &mut Window, cx: &mut Context<Self>) {
        if what == "sample" || what == "sample-live" || what == "sample-ask" || what == "sample-jev"
        {
            let id = self.agents.next_id;
            self.agents.next_id += 1;
            let mut chat = Chat::sample(id, self.waker.clone(), what != "sample", window, cx);
            if what == "sample-ask" {
                chat.demo_ask(ping_agent::providers::Ask {
                    what: "Empty the Recycle Bin (8.9 GB, 2 items)".into(),
                    why: "To free the space for good, as you asked.".into(),
                });
            }
            // A click Jev judged risky, waiting for a yes.
            if what == "sample-jev" {
                chat.demo_ask(ping_agent::providers::Ask {
                    what: "left_click (412, 96)".into(),
                    why: "Jev judges that clicking button \"Empty Recycle Bin\" may delete \
                        files or data (94% likely)."
                        .into(),
                });
            }
            self.agents.chats.push(chat);
            self.set_page(Page::Chat(id), cx);
        }
    }

    pub fn render_agents(
        &mut self,
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.agents.check_providers();
        let hosts = self.agent_hosts();
        if self.agents.host.is_empty() || !hosts.iter().any(|(n, _)| *n == self.agents.host) {
            self.agents.host = hosts.first().map(|h| h.0.clone()).unwrap_or_default();
        }
        let trailing =
            vec![
                pingpong_ui::icon_button("agent-setup", IconName::Settings, "Agent setup", t)
                    .on_click(
                        cx.listener(|this, _, _, cx| {
                            this.set_page(Page::Settings(Tab::Agents), cx)
                        }),
                    )
                    .into_any_element(),
            ];
        let body = if hosts.is_empty() {
            self.agents_first_host(t, cx)
        } else {
            self.agents_hero(&hosts, t, window, cx)
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar(trailing))
            .child(body)
            .into_any_element()
    }

    /// No host lets the agent in yet: say what that means and offer it.
    fn agents_first_host(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        page_body(
            "agents-first",
            560.0,
            div()
                .pt(px(40.0))
                .flex()
                .flex_col()
                .gap(px(18.0))
                .child(hero_mark(t))
                .child(pingpong_ui::page_header(
                    "Let an agent use a host",
                    Some(
                        "An AI agent works a host as you would through Ping: it sees the \
                            screen and uses the keyboard and mouse. It pairs with a key of its \
                            own, so the host knows it is an agent and holds it to the agent \
                            rules: never over a person, never on a secure screen."
                            .into(),
                    ),
                    None,
                    t,
                ))
                .child(section("Your hosts", t).child(self.agent_host_rows(t, cx)))
                .child(pingpong_ui::footnote(
                    "You type a PIN in Pong, as when you paired \
                        Ping. What the agent may do there is set in Pong, per agent.",
                    t,
                )),
        )
        .into_any_element()
    }

    fn agents_hero(
        &mut self,
        hosts: &[(String, String)],
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let host = self.agents.host.clone();
        let existing = self.agents.chats.iter().any(|c| c.host == host);
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .px(px(32.0))
            .pb(px(56.0))
            .gap(px(14.0))
            .child(
                div()
                    .text_size(px(Type::DISPLAY))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("What can the agent do for you?"),
            )
            .child(
                div()
                    .max_w(px(500.0))
                    .text_center()
                    .text_size(px(Type::BODY))
                    .line_height(px(19.0))
                    .text_color(t.tertiary)
                    .child(format!(
                        "A session with an agent that works {host} with you: ask it \
                            things, hand it chores, log in whenever you want to watch or \
                            take over. It stays in the sidebar until you end it."
                    )),
            )
            .child(
                div()
                    .pt(px(8.0))
                    .w_full()
                    .max_w(px(640.0))
                    .child(self.new_session_composer(hosts, t, window, cx)),
            )
            .when(existing, |d| {
                d.child(
                    div()
                        .text_size(px(Type::META + 0.5))
                        .text_color(t.tertiary)
                        .child(format!(
                            "{host} already has a session: this message goes there."
                        )),
                )
            })
            .into_any_element()
    }

    /// The first message, and what runs the session: the host, the model,
    /// how hard it thinks, whether it asks first.
    fn new_session_composer(
        &mut self,
        hosts: &[(String, String)],
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let a = &self.agents;
        let focused = a.composer.read(cx).is_focused(window);
        let p = a.settings.provider;
        let ready = a.ready(p).cloned();
        let can_run = !a.host.is_empty()
            && ready.as_ref().is_some_and(|r| r.is_ok())
            && !a.composer.read(cx).text().trim().is_empty();

        let host_names: Vec<String> = hosts.iter().map(|h| h.0.clone()).collect();
        let host_index = host_names.iter().position(|n| *n == a.host);
        let names = host_names.clone();
        let this = cx.weak_entity();
        let host_pick = select("agent-host", host_names, host_index, t)
            .chip()
            .leading(IconName::Monitor)
            .on_select(move |i, _, cx| {
                let _ = this.update(cx, |app, cx| {
                    app.agents.host = names[i].clone();
                    cx.notify();
                });
            });

        let providers: Vec<Choice> = a
            .availability
            .iter()
            .map(|(p, s)| {
                let c = Choice::new(short(*p));
                match s {
                    Ok(_) => c.detail(if p.is_subscription() {
                        "Your plan"
                    } else {
                        "Your key"
                    }),
                    Err(_) => c.detail("Not set up"),
                }
            })
            .collect();
        let provider_ids: Vec<Provider> = a.availability.iter().map(|(p, _)| *p).collect();
        let provider_index = provider_ids.iter().position(|q| *q == p);
        let this = cx.weak_entity();
        let provider_pick = select("agent-provider", providers, provider_index, t)
            .chip()
            .leading(IconName::Sparkle)
            .shown(short(p))
            .on_select(move |i, _, cx| {
                let _ = this.update(cx, |app, cx| {
                    let p = provider_ids[i];
                    app.agents.settings.provider = p;
                    app.agents.settings.model = p.models()[0].to_string();
                    app.agents.key_note = None;
                    app.agents.save_settings();
                    app.agents.sync_program(cx);
                    tracing::info!(provider = p.id(), "agent provider chosen");
                    cx.notify();
                });
            });

        let models: Vec<&'static str> = p.models().to_vec();
        let model_index = models.iter().position(|m| *m == a.settings.model);
        let shown = if a.settings.model.is_empty() {
            "Default model".to_string()
        } else {
            a.settings.model.clone()
        };
        let this = cx.weak_entity();
        let model_pick = select(
            "agent-model",
            models
                .iter()
                .map(|m| if m.is_empty() { "Default model" } else { m })
                .collect::<Vec<_>>(),
            model_index,
            t,
        )
        .chip()
        .shown(shown)
        .on_select(move |i, _, cx| {
            let _ = this.update(cx, |app, cx| {
                app.agents.settings.model = models[i].to_string();
                app.agents.save_settings();
                cx.notify();
            });
        });

        let efforts = ["low", "medium", "high"];
        let this = cx.weak_entity();
        let effort_pick = select(
            "agent-effort",
            ["Low effort", "Medium effort", "High effort"],
            efforts.iter().position(|e| *e == a.settings.effort),
            t,
        )
        .chip()
        .on_select(move |i, _, cx| {
            let _ = this.update(cx, |app, cx| {
                app.agents.settings.effort = efforts[i].to_string();
                app.agents.save_settings();
                cx.notify();
            });
        });

        let approvals = [Approvals::Risky, Approvals::Every, Approvals::Off];
        let this = cx.weak_entity();
        let ask_chip = select(
            "agent-approvals",
            ["Ask if risky", "Ask every step", "Never ask"],
            approvals.iter().position(|x| *x == a.settings.approvals),
            t,
        )
        .chip()
        .on_select(move |i, _, cx| {
            let _ = this.update(cx, |app, cx| {
                app.agents.settings.approvals = approvals[i];
                app.agents.save_settings();
                cx.notify();
            });
        });

        let run = div()
            .id("run")
            .size(px(30.0))
            .flex_none()
            .rounded(px(15.0))
            .flex()
            .items_center()
            .justify_center()
            .bg(if can_run {
                t.solid()
            } else {
                t.primary.alpha(0.12)
            })
            .child(icon(
                IconName::ArrowUp,
                16.0,
                if can_run { t.on_solid() } else { t.quaternary },
            ))
            .when(can_run, |d| {
                d.cursor_pointer()
                    .hover(|s| s.opacity(0.85))
                    .on_click(cx.listener(|this, _, window, cx| this.start_chat(window, cx)))
            })
            .tooltip(|_, cx| pingpong_ui::tooltip("Start (Return). Shift+Return adds a line.", cx));

        let warning = match &ready {
            Some(Err(why)) => Some(
                div()
                    .id("setup-warning")
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(4.0))
                    .text_size(px(Type::META + 0.5))
                    .text_color(t.ink(Ink::ATTENTION))
                    .child(icon(IconName::Warning, 12.0, t.ink(Ink::ATTENTION)))
                    .child(format!("{}: {}.", short(p), why.trim_end_matches('.')))
                    .child(
                        div()
                            .id("open-setup")
                            .text_color(t.primary)
                            .cursor_pointer()
                            .hover(|s| s.opacity(0.8))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.set_page(Page::Settings(Tab::Agents), cx)
                            }))
                            .child("Set it up"),
                    ),
            ),
            _ => None,
        };

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .px(px(14.0))
                    .pt(px(12.0))
                    .pb(px(10.0))
                    .rounded(px(Radius::PANEL + 2.0))
                    .bg(t.floating)
                    .border_1()
                    .border_color(if focused {
                        t.primary.alpha(0.22)
                    } else {
                        t.card_stroke
                    })
                    .shadow(pingpong_ui::lift(if t.dark { 0.25 } else { 0.06 }))
                    .child(
                        div()
                            .max_h(px(220.0))
                            .overflow_hidden()
                            .child(field(&self.agents.composer, t).bare().min_rows(2)),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(2.0))
                            .child(host_pick)
                            .child(provider_pick)
                            .child(model_pick)
                            .child(effort_pick)
                            .child(div().flex_1())
                            .child(ask_chip)
                            .child(div().w(px(6.0)))
                            .child(run),
                    ),
            )
            .children(warning)
    }

    fn agent_host_rows(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let agent = self.agent_hosts();
        let persons: Vec<_> = self
            .model
            .items
            .iter()
            .filter_map(|i| {
                let p = i.paired.as_ref()?;
                Some((
                    i.name.clone(),
                    p.key.clone(),
                    i.pairing_address(),
                    i.web_url(),
                    i.is_mac(),
                ))
            })
            .collect();
        if persons.is_empty() {
            return rows(
                [setting(
                    "No hosts yet",
                    Some("Pair Ping with a host first, on the Hosts page.".into()),
                    button("go-hosts", "Hosts", t)
                        .on_click(cx.listener(|this, _, _, cx| this.set_page(Page::Hosts, cx))),
                    t,
                )
                .into_any_element()],
                t,
            )
            .into_any_element();
        }
        rows(
            persons.into_iter().map(|(name, key, address, web, mac)| {
                let allowed = agent.iter().any(|(_, k)| *k == key);
                let control: AnyElement = if allowed {
                    chip("Allowed", t.ink(Ink::FRESH), t).into_any_element()
                } else {
                    let n = name.clone();
                    button(ElementId::Name(format!("allow-{name}").into()), "Allow…", t)
                        .disabled(address.is_none())
                        .tooltip(if address.is_some() {
                            "Pair the agent: you type a PIN in Pong"
                        } else {
                            "The host is not on the network now"
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(a) = &address {
                                this.pair_with(n.clone(), a.clone(), web.clone(), true, cx);
                            }
                        }))
                        .into_any_element()
                };
                div()
                    .min_h(px(pingpong_ui::Metrics::SETTINGS_ROW))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(icon(
                        if mac {
                            IconName::Laptop
                        } else {
                            IconName::Monitor
                        },
                        18.0,
                        t.secondary,
                    ))
                    .child(pingpong_ui::label_stack(
                        name,
                        Some(if allowed {
                            "The agent may use it. What it may do there is set in Pong, per agent."
                                .into()
                        } else {
                            "Not yet: the agent pairs with a PIN of its own.".into()
                        }),
                        t,
                    ))
                    .child(control)
                    .into_any_element()
            }),
            t,
        )
        .into_any_element()
    }

    // -----------------------------------------------------------------------
    // Agent setup (a settings page)
}

fn hero_mark(t: Theme) -> impl IntoElement {
    div()
        .size(px(52.0))
        .rounded(px(16.0))
        .bg(Ink::ACCENT.alpha(if t.dark { 0.14 } else { 0.12 }))
        .border_1()
        .border_color(Ink::ACCENT.alpha(0.25))
        .flex()
        .items_center()
        .justify_center()
        .child(icon(IconName::Agent, 26.0, t.ink(Ink::ACCENT)))
}
