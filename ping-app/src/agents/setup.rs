//! Agent setup, a settings page: the model and what it needs, limits, which
//! hosts the agent may use, and `Ping mcp` for other agents.

use gpui::{
    div, prelude::*, px, AnyElement, ClipboardItem, Context, ElementId, SharedString, Window,
};
use ping_agent::providers::{self, AgentSettings, Approvals, Provider};
use pingpong_ui::{
    button, chip, field, rows, section, select, setting, stepper, switch, IconName, Ink, Theme,
    Type,
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

/// Register Ping's MCP server with a CLI agent (the user asked, by a click).
pub(super) fn add_mcp(cmd: &[&str], exe: &str, settings: &AgentSettings, claude: bool) -> String {
    let program = if claude {
        providers::cli::claude_program(settings).map(|p| p.display().to_string())
    } else {
        providers::cli::codex_program(settings).map(|p| p.path.display().to_string())
    };
    let program = match program {
        Ok(p) => p,
        Err(e) => return e,
    };
    let out = std::process::Command::new(&program)
        .args(&cmd[1..])
        .arg(exe)
        .arg("mcp")
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => {
            format!(
                "Added: {} can use your hosts now (as the tools of the \"pingpong\" server).",
                if claude { "Claude Code" } else { "Codex" }
            )
        }
        Ok(o) => format!(
            "{} said: {}",
            cmd[0],
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => e.to_string(),
    }
}

impl PingApp {
    /// Jev's key, and what it does with it.
    fn jev_rows(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let service = ping_agent::jev::service(&self.dir);
        let saved = providers::secrets::saved_named(&self.dir, ping_agent::jev::KEY_ID);
        let has_text = !self.agents.jev_key.read(cx).text().trim().is_empty();
        let detail = self
            .agents
            .jev_note
            .clone()
            .unwrap_or_else(|| match service {
                Some(s) if saved => format!(
                    "A {s} key is saved, readable only by you. {} works too.",
                    ping_agent::jev::KEY_ENV
                ),
                Some(s) => format!("A {s} key is set in {}.", ping_agent::jev::KEY_ENV),
                None => format!(
                    "A TypeSafe key, or an OpenRouter one (OpenRouter serves Jev too). Saved \
                    readable only by you, or set {}.",
                    ping_agent::jev::KEY_ENV
                ),
            });
        let key_row =
            setting(
                "Key",
                Some(detail.into()),
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(div().w(px(180.0)).child(field(&self.agents.jev_key, t)))
                    .child(
                        button("jev-key-save", "Save", t)
                            .disabled(!has_text)
                            .on_click(cx.listener(|this, _, _, cx| this.agents.save_jev_key(cx))),
                    )
                    .when(saved, |d| {
                        d.child(button("jev-key-forget", "Forget", t).danger().on_click(
                            cx.listener(|this, _, _, cx| {
                                this.agents.jev_note = Some(
                                    match providers::secrets::set_named(
                                        &this.dir,
                                        ping_agent::jev::KEY_ID,
                                        None,
                                    ) {
                                        Ok(()) => "Forgotten.".into(),
                                        Err(e) => e.to_string(),
                                    },
                                );
                                cx.notify();
                            }),
                        ))
                    }),
                t,
            )
            .into_any_element();
        let off = service.is_none();
        let jev = self.agents.settings.jev.clone();
        let toggle =
            |id: &'static str, on: bool, set: fn(&mut ping_agent::jev::JevSettings, bool)| {
                let this = cx.weak_entity();
                switch(id, on && !off, t)
                    .disabled(off)
                    .on_toggle(move |on, _, cx| {
                        let _ = this.update(cx, |app, cx| {
                            set(&mut app.agents.settings.jev, on);
                            app.agents.save_settings();
                            tracing::info!(setting = id, on, "Jev setting changed");
                            cx.notify();
                        });
                    })
            };
        rows(
            [
                key_row,
                setting(
                    "Check clicks",
                    Some(
                        "Before a click, the host says what is under it, and Jev judges whether \
                            it deletes, spends, sends, changes settings or installs. If it \
                            likely does, the click asks for your go-ahead. With go-ahead for \
                            risky steps only; Jev never lets a step through that would ask."
                            .into(),
                    ),
                    toggle("jev-clicks", jev.check_clicks, |j, on| j.check_clicks = on),
                    t,
                )
                .into_any_element(),
                setting(
                    "Sort how turns end",
                    Some(
                        "Jev reads the agent's last words: done, a question for you, a person \
                            needed at the host, or not done. The session and its notification \
                            say which."
                            .into(),
                    ),
                    toggle("jev-endings", jev.sort_endings, |j, on| j.sort_endings = on),
                    t,
                )
                .into_any_element(),
                setting(
                    "Pick the model",
                    Some(
                        "When Jev judges a session's first message routine, the session runs at \
                            low effort, on Claude Sonnet with an Anthropic key or Claude Code. \
                            Never on a heavier model than yours."
                            .into(),
                    ),
                    toggle("jev-model", jev.pick_model, |j, on| j.pick_model = on),
                    t,
                )
                .into_any_element(),
            ],
            t,
        )
        .into_any_element()
    }

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

        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "Ping".into());
        let mcp_line = format!("\"{exe}\" mcp");
        let copy_line = mcp_line.clone();
        let config_exe = exe.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(section("Model", t).child(rows(providers, t)))
            .child(section(short(p), t).child(rows(needs, t)))
            .when(p == Provider::Openrouter, |d| {
                d.child(pingpong_ui::footnote(
                    "OpenRouter reaches hundreds of models with one key, TypeSafe's Jev \
                        Router (typesafe/jev-router) among them: it picks a model per \
                        step. Models ending in :free cost nothing.",
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
            .child(section("Jev", t).child(self.jev_rows(t, cx)))
            .when(ping_agent::jev::key(&self.dir).is_none(), |d| {
                d.child(pingpong_ui::footnote(
                    "Jev is TypeSafe's decision model: yes-or-no judgments in a fraction of a \
                        second, a few thousandths of a cent each, beside the model you chose. \
                        It is off until a key is saved.",
                    t,
                ))
            })
            .child(section("Hosts the agent may use", t).child(self.agent_host_rows(t, cx)))
            .child(section("Your hosts in other agents", t).child(rows(
                [
                    setting("Claude Code", Some("Adds Ping as the \"pingpong\" MCP server, \
                        for you everywhere.".into()), button("mcp-claude", "Add", t).on_click(cx.listener({
                        let exe = exe.clone();
                        move |this, _, _, cx| {
                            this.agents.mcp_note = Some(add_mcp(&["claude", "mcp", "add", "--scope", "user", "pingpong", "--"], &exe, &this.agents.settings, true));
                            cx.notify();
                        }
                    })), t)
                    .into_any_element(),
                    setting("Codex", Some("The same, for Codex.".into()), button("mcp-codex", "Add", t).on_click(cx.listener({
                        let exe = exe.clone();
                        move |this, _, _, cx| {
                            this.agents.mcp_note = Some(add_mcp(&["codex", "mcp", "add", "pingpong", "--"], &exe, &this.agents.settings, false));
                            cx.notify();
                        }
                    })), t)
                    .into_any_element(),
                    setting("Claude Desktop, Cursor and others", Some("Copies the server's \
                        settings, to paste into claude_desktop_config.json or \
                        ~/.cursor/mcp.json.".into()), button("mcp-copy", "Copy", t).icon(IconName::Copy).on_click(cx.listener(move |this, _, _, cx| {
                        let config = serde_json::json!({"mcpServers": {"pingpong": {"command": config_exe, "args": ["mcp"]}}});
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
                ],
                t,
            )))
            .children(self.agents.mcp_note.clone().map(|n| pingpong_ui::footnote(n, t)))
            .into_any_element()
    }
}
