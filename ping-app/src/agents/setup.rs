//! Agent setup, a settings page: the model and what it needs, limits, which
//! hosts the agent may use, and `Ping mcp` in other agents' settings (see
//! `ping_agent::install`).

use gpui::{
    div, prelude::*, px, AnyElement, ClipboardItem, Context, ElementId, SharedString, Window,
};
use ping_agent::install::{self, State};
use ping_agent::providers::{self, AgentSettings, Approvals, Provider};
use pingpong_ui::{
    button, chip, field, rows, section, select, setting, stepper, IconName, Ink, Theme, Type,
};

use crate::app::PingApp;

use super::short;

/// A path with the home folder as "~".
pub(super) fn home_short(s: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => s.replace(&home, "~"),
        _ => s.to_string(),
    }
}

impl PingApp {
    pub fn render_agent_setup(
        &mut self,
        t: Theme,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.agents.check_providers();
        let selected = self.agents.settings.provider;
        let providers = self
            .agents
            .availability
            .clone()
            .into_iter()
            .map(|(p, state)| {
                let on = p == selected;
                let detail: SharedString = match &state {
                    Ok(s) if p.is_subscription() => home_short(s).into(),
                    Ok(_) => "A key is saved or set in the environment.".into(),
                    Err(e) => e.clone().into(),
                };
                div()
                    .id(ElementId::Name(format!("provider-{}", p.id()).into()))
                    .min_h(px(pingpong_ui::Metrics::SETTINGS_ROW))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(t.primary.alpha(0.025)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.agents.settings.provider = p;
                        this.agents.settings.model = p.models()[0].to_string();
                        this.agents.key_note = None;
                        this.agents.save_settings();
                        this.agents.sync_program(cx);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_none()
                            .size(px(16.0))
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(if on { t.primary } else { t.primary.alpha(0.25) })
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(on, |d| {
                                d.child(div().size(px(8.0)).rounded(px(4.0)).bg(t.primary))
                            }),
                    )
                    .child(pingpong_ui::label_stack(p.label(), Some(detail), t))
                    .child(if state.is_ok() {
                        chip("Ready", t.ink(Ink::FRESH), t)
                    } else {
                        chip("Not set up", t.tertiary, t)
                    })
                    .into_any_element()
            });

        // What the chosen provider needs.
        let p = selected;
        let mut needs: Vec<AnyElement> = Vec::new();
        if p.is_subscription() {
            needs.push(
                setting(
                    "Program",
                    Some(
                        match p {
                            Provider::Codex => {
                                "Codex runs with your ChatGPT sign-in \
                                    (codex login). Ping gives it only the host's tools: no \
                                    shell, no files, no browser."
                            }
                            _ => {
                                "Claude Code runs with your Claude sign-in (run claude \
                                    once). Ping gives it only the host's tools: no shell, no \
                                    files."
                            }
                        }
                        .into(),
                    ),
                    div().w(px(220.0)).child(field(&self.agents.program, t)),
                    t,
                )
                .into_any_element(),
            );
        } else {
            if p == Provider::Custom {
                needs.push(
                    setting(
                        "Endpoint",
                        Some("An OpenAI-compatible API: Ollama, LM Studio, a gateway.".into()),
                        div().w(px(240.0)).child(field(&self.agents.base_url, t)),
                        t,
                    )
                    .into_any_element(),
                );
            }
            let saved = providers::secrets::saved(&self.dir, p);
            let has_text = !self.agents.key_field.read(cx).text().trim().is_empty();
            let detail = self
                .agents
                .key_note
                .clone()
                .unwrap_or_else(|| match p.key_env() {
                    Some(env) if saved => format!("Saved, readable only by you. {env} works too."),
                    Some(env) => format!("Saved readable only by you, or set {env}."),
                    None => "Saved readable only by you.".into(),
                });
            needs.push(
                setting(
                    "API key",
                    Some(detail.into()),
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(div().w(px(180.0)).child(field(&self.agents.key_field, t)))
                        .child(
                            button("key-save", "Save", t)
                                .disabled(!has_text)
                                .on_click(cx.listener(|this, _, _, cx| this.agents.save_key(cx))),
                        )
                        .when(saved, |d| {
                            d.child(button("key-forget", "Forget", t).danger().on_click(
                                cx.listener(move |this, _, _, cx| {
                                    this.agents.key_note =
                                        Some(match providers::secrets::set(&this.dir, p, None) {
                                            Ok(()) => "Forgotten.".into(),
                                            Err(e) => e.to_string(),
                                        });
                                    this.agents.checked = None;
                                    cx.notify();
                                }),
                            ))
                        }),
                    t,
                )
                .into_any_element(),
            );
        }
        let s = self.agents.settings.clone();
        let this = cx.weak_entity();
        let set = move |f: fn(&mut AgentSettings, u32)| {
            let this = this.clone();
            move |v: u32, _: &mut Window, cx: &mut gpui::App| {
                let _ = this.update(cx, |app, cx| {
                    f(&mut app.agents.settings, v);
                    app.agents.save_settings();
                    cx.notify();
                });
            }
        };
        let sizes = [
            (1280u16, 800u16),
            (1366, 768),
            (1280, 720),
            (1920, 1080),
            (1024, 768),
        ];
        let this = cx.weak_entity();
        let display = select(
            "agent-display",
            sizes
                .iter()
                .map(|(w, h)| format!("{w} × {h}"))
                .collect::<Vec<_>>(),
            sizes.iter().position(|sz| *sz == (s.width, s.height)),
            t,
        )
        .width(140.0)
        .on_select(move |i, _, cx| {
            let _ = this.update(cx, |app, cx| {
                (app.agents.settings.width, app.agents.settings.height) = sizes[i];
                app.agents.save_settings();
                cx.notify();
            });
        });
        let this = cx.weak_entity();
        let choices = [Approvals::Risky, Approvals::Every, Approvals::Off];
        let ask = select(
            "agent-approvals-setting",
            ["Risky steps", "Every step", "Never"],
            choices.iter().position(|x| *x == s.approvals),
            t,
        )
        .width(140.0)
        .on_select(move |i, _, cx| {
            let _ = this.update(cx, |app, cx| {
                app.agents.settings.approvals = choices[i];
                app.agents.save_settings();
                cx.notify();
            });
        });

        let server = install::Server::this().unwrap_or_else(|_| install::Server {
            command: "Ping".into(),
            args: vec!["mcp".into()],
        });
        let mcp_line = std::iter::once(format!("\"{}\"", server.command))
            .chain(server.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        let copy_line = mcp_line.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(section("Model", t).child(rows(providers, t)))
            .child(section(short(p), t).child(rows(needs, t)))
            .when(p == Provider::Openrouter, |d| {
                d.child(pingpong_ui::footnote(
                    "OpenRouter reaches hundreds of models with one key. Models \
                        ending in :free cost nothing.",
                    t,
                ))
            })
            .child(section("Limits", t).child(rows(
                [
                    setting("Actions per run", Some("The agent stops after this many \
                        clicks and keystrokes.".into()), stepper("max-actions", s.max_actions, 5..=500, 5, t, set(|s, v| s.max_actions = v)), t)
                        .into_any_element(),
                    setting("Minutes per run", Some("And after this long.".into()), stepper("max-minutes", s.max_minutes, 1..=180, 1, t, set(|s, v| s.max_minutes = v)), t).into_any_element(),
                    setting("The agent's display", Some("The host makes a display this \
                        size for the agent: 1280 × 800 is what models read best.".into()), display, t).into_any_element(),
                    setting(
                        "Ask for your go-ahead",
                        Some("Risky steps: deleting, spending, sending, changing settings, \
                            typing a password or a key. The model asks, and rules catch \
                            the commands and keys it did not ask about. A notification \
                            asks when Ping is in the background.".into()),
                        ask,
                        t,
                    )
                    .into_any_element(),
                ],
                t,
            )))
            .child(section("Hosts the agent may use", t).child(self.agent_host_rows(t, cx)))
            .child(section("Your hosts in other agents", t).child(rows(
                self.mcp_app_rows(t, cx).into_iter().chain([
                    setting("Others", Some("Copies the server's settings, to paste where \
                        another agent keeps its MCP servers.".into()), button("mcp-copy", "Copy", t).icon(IconName::Copy).on_click(cx.listener(move |this, _, _, cx| {
                        let config = serde_json::json!({"mcpServers": {install::NAME: {"command": server.command, "args": server.args}}});
                        cx.write_to_clipboard(ClipboardItem::new_string(serde_json::to_string_pretty(&config).unwrap_or_default()));
                        this.agents.mcp_note = Some("Copied.".into());
                        cx.notify();
                    })), t)
                    .into_any_element(),
                    div()
                        .px(px(12.0))
                        .py(px(10.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().font_family(pingpong_ui::mono_font()).text_size(px(Type::META)).text_color(t.secondary).child(mcp_line))
                        .child(pingpong_ui::icon_button("copy-mcp-line", IconName::Copy, "Copy the command", t).on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy_line.clone()))))
                        .into_any_element(),
                ]),
                t,
            )))
            .children(self.mcp_missing_note(t))
            .children(self.agents.mcp_note.clone().map(|n| pingpong_ui::footnote(n, t)))
            .into_any_element()
    }

    /// A row for each agent on this computer: is Ping's server in its
    /// settings, and the button that changes that.
    fn mcp_app_rows(&mut self, t: Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let places = install::Places::here();
        self.agents
            .mcp_apps
            .clone()
            .into_iter()
            .filter(|(_, state)| *state != State::Missing)
            .map(|(app, state)| {
                let files = install::files(app, &places)
                    .iter()
                    .map(|f| install::tilde(f, &places.home))
                    .collect::<Vec<_>>()
                    .join(", ");
                let (detail, chip_, action): (String, Option<AnyElement>, Option<(&str, bool)>) =
                    match &state {
                        State::Absent => {
                            (format!("Adds it to {files}."), None, Some(("Add", true)))
                        }
                        State::Added { current: true } => (
                            format!("In {files}: its new sessions have your hosts as tools."),
                            Some(chip("Added", t.ink(Ink::FRESH), t).into_any_element()),
                            Some(("Remove", false)),
                        ),
                        State::Added { current: false } => (
                            format!("In {files}, for another copy of Ping."),
                            Some(chip("Elsewhere", t.ink(Ink::ATTENTION), t).into_any_element()),
                            Some(("Update", true)),
                        ),
                        State::Unreadable(e) => (e.clone(), None, None),
                        State::Shared(why) => (why.to_string(), None, None),
                        State::Missing => (String::new(), None, None),
                    };
                let control = div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .children(chip_)
                    .children(action.map(|(label, adds)| {
                        button(
                            ElementId::Name(format!("mcp-{}", app.id()).into()),
                            label,
                            t,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let places = install::Places::here();
                            let done = if adds {
                                install::Server::this()
                                    .and_then(|server| install::add(app, &places, &server))
                            } else {
                                install::remove(app, &places)
                            };
                            this.agents.mcp_note = Some(done.unwrap_or_else(|e| e));
                            this.agents.check_mcp_apps();
                            cx.notify();
                        }))
                    }));
                setting(app.name(), Some(detail.into()), control, t).into_any_element()
            })
            .collect()
    }

    /// What was done last, or the agents this looks for that are not here.
    fn mcp_missing_note(&self, t: Theme) -> Option<gpui::Div> {
        if let Some(note) = &self.agents.mcp_note {
            return Some(pingpong_ui::footnote(note.clone(), t));
        }
        let missing: Vec<&str> = self
            .agents
            .mcp_apps
            .iter()
            .filter(|(_, s)| *s == State::Missing)
            .map(|(a, _)| a.name())
            .collect();
        (!missing.is_empty()).then(|| {
            pingpong_ui::footnote(
                format!(
                    "Not on this computer: {}. Ping mcp install, from a terminal, does \
                        the same.",
                    missing.join(", ")
                ),
                t,
            )
        })
    }
}
