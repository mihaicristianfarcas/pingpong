//! A session's page: its toolbar, the transcript and the composer.

use std::time::Duration;

use gpui::{div, prelude::*, px, AnyElement, Context, ElementId, FontWeight, Window};
use ping_agent::providers::Approvals;
use pingpong_ui::{
    button, field, icon, icon_button, notice, spinner, IconName, Ink, Radius, Theme, Type,
};

use crate::app::PingApp;

use super::{Act, Chat, ChatState, Item};

pub(super) fn action_row(a: &Act, t: Theme) -> impl IntoElement {
    let glyph = if a.text.starts_with("type") {
        IconName::Type
    } else if a.text.starts_with("key") {
        IconName::Keyboard
    } else if a.text.starts_with("screenshot") || a.text.starts_with("zoom") {
        IconName::Eye
    } else if a.text.starts_with("scroll") {
        IconName::ChevronUpDown
    } else if a.text.starts_with("connect") {
        IconName::Link
    } else {
        IconName::CursorClick
    };
    let tint = if a.ok {
        t.tertiary
    } else {
        t.ink(Ink::ATTENTION)
    };
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .min_h(px(22.0))
        .pl(px(8.0))
        .child(icon(glyph, 12.0, tint))
        .child(
            div()
                .flex_none()
                .max_w(gpui::relative(0.6))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .font_family(pingpong_ui::mono_font())
                .text_size(px(11.5))
                .text_color(t.primary.alpha(0.78))
                .child(a.text.clone()),
        )
        .when(!a.detail.is_empty() && a.detail != "OK", |d| {
            d.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Type::META))
                    .text_color(t.tertiary)
                    .child(a.detail.clone()),
            )
        })
}

pub(super) fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

impl PingApp {
    pub(super) fn chat_mut(&mut self, id: u64) -> Option<&mut Chat> {
        self.agents.chats.iter_mut().find(|c| c.id == id)
    }

    pub fn chat_send(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(chat) = self.chat_mut(id) else {
            return;
        };
        let text = chat.composer.read(cx).text().trim().to_string();
        if text.is_empty() || chat.busy() {
            return;
        }
        match chat.send(text) {
            Ok(()) => {
                let composer = chat.composer.clone();
                composer.update(cx, |f, cx| f.set_text("", cx));
            }
            Err(e) => chat.items.push(Item::Error(e)),
        }
        cx.notify();
    }

    /// Log in to the agent's desktop: a stream window on its screen, where
    /// Ctrl+Alt+Shift+T takes the keyboard and mouse and gives them back.
    pub fn chat_log_in(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.is_watching(id) {
            self.show_login(id);
            return;
        }
        let Some(chat) = self.chat_mut(id) else {
            return;
        };
        if chat.conversation.is_none() {
            return;
        }
        if chat.is_connected() {
            self.watch_chat(id, cx);
        } else {
            chat.watch_pending = true;
            chat.connect();
        }
        cx.notify();
    }

    pub fn chat_end(&mut self, id: u64, cx: &mut Context<Self>) {
        self.stop_watching(id);
        if let Some(i) = self.agents.chats.iter().position(|c| c.id == id) {
            let chat = self.agents.chats.remove(i);
            chat.end();
        }
        self.set_page(crate::app::Page::Agents, cx);
    }

    pub fn render_chat(
        &mut self,
        id: u64,
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let watching = self.is_watching(id);
        let Some(chat) = self.agents.chats.iter().find(|c| c.id == id) else {
            return div().into_any_element();
        };
        let ended = chat.conversation.is_none();
        let panel = chat.panel;
        let busy = chat.busy();
        let connecting = chat.state() == ChatState::Connecting || chat.watch_pending;

        // Along the title bar: the session's title, and what can be done with
        // it. Its state, host and figures are the side panel's and the
        // sidebar row's to say.
        let bar = div()
            .flex_none()
            .h(px(pingpong_ui::Metrics::TOOLBAR))
            .px(px(14.0))
            .pl(px(24.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Type::BODY + 0.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(chat.title.clone()),
            )
            .child(
                button(
                    "chat-log-in",
                    if watching {
                        "Show Desktop"
                    } else if connecting {
                        "Connecting…"
                    } else {
                        "Log In"
                    },
                    t,
                )
                .icon(IconName::Display)
                .disabled(ended || (connecting && !watching))
                .tooltip(if watching {
                    "You are logged in: bring the desktop's window forward.".to_string()
                } else {
                    format!(
                        "Open the agent's desktop in a window. {} takes the keyboard \
                            and mouse, and gives them back.",
                        crate::settings::chord("T")
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| this.chat_log_in(id, cx))),
            )
            .child(
                icon_button(
                    "chat-panel",
                    IconName::Eye,
                    if panel {
                        "Hide what the agent sees"
                    } else {
                        "Show what the agent sees"
                    },
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(c) = this.chat_mut(id) {
                        c.panel = !c.panel;
                    }
                    cx.notify();
                })),
            )
            .child(
                button("chat-end", "End Session", t)
                    .danger()
                    .on_click(cx.listener(move |this, _, _, cx| this.chat_end(id, cx))),
            );

        let transcript = self.chat_transcript(id, t, cx);
        let composer = self.chat_composer(id, busy, ended, t, window, cx);
        let chat = self
            .agents
            .chats
            .iter()
            .find(|c| c.id == id)
            .expect("still there");
        let left = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id(ElementId::Name(format!("chat-{id}-scroll").into()))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&chat.scroll)
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .justify_center()
                            .px(px(24.0))
                            .pt(px(8.0))
                            .pb(px(20.0))
                            .child(
                                div()
                                    .w_full()
                                    .max_w(px(720.0))
                                    .flex()
                                    .flex_col()
                                    .gap(px(14.0))
                                    .children(transcript),
                            ),
                    ),
            )
            .child(
                div()
                    .px(px(24.0))
                    .pb(px(16.0))
                    .pt(px(6.0))
                    .flex()
                    .justify_center()
                    .child(div().w_full().max_w(px(720.0)).child(composer)),
            );
        let width = window.bounds().size.width;
        let show_panel = panel && width > px(760.0);
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(pingpong_ui::hairline(t))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(left)
                    .when(show_panel, |d| {
                        d.child(self.chat_panel(id, watching, t, window, cx))
                    }),
            )
            .into_any_element()
    }

    pub(super) fn chat_transcript(
        &mut self,
        id: u64,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(chat) = self.agents.chats.iter().find(|c| c.id == id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if chat.items.is_empty() {
            out.push(
                div()
                    .pt(px(40.0))
                    .text_center()
                    .text_size(px(Type::BODY))
                    .text_color(t.tertiary)
                    .child(format!(
                        "Ask anything about {}, or give it something to do.",
                        chat.host
                    ))
                    .into_any_element(),
            );
        }
        let last = chat.items.len().saturating_sub(1);
        for (i, item) in chat.items.iter().enumerate() {
            let el = match item {
                Item::You(text) => div()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .max_w(gpui::relative(0.82))
                            .px(px(12.0))
                            .py(px(8.0))
                            .rounded(px(Radius::PANEL))
                            .bg(t.primary.alpha(if t.dark { 0.08 } else { 0.06 }))
                            .text_size(px(Type::BODY))
                            .line_height(px(19.0))
                            .child(text.clone()),
                    )
                    .into_any_element(),
                Item::Reply(text) => pingpong_ui::markdown(
                    ElementId::Name(format!("chat-{id}-md-{i}").into()),
                    text,
                    t,
                ),
                Item::Actions { list, open } => {
                    let live = i == last && chat.busy();
                    let open = *open || live;
                    let n = list.len();
                    let failed = list.iter().filter(|a| !a.ok).count();
                    let latest = list.last().map(|a| a.text.clone()).unwrap_or_default();
                    let summary = if live {
                        format!("Using the computer · {latest}")
                    } else if failed > 0 {
                        format!(
                            "Used the computer · {n} action{} ({failed} didn't work)",
                            if n == 1 { "" } else { "s" }
                        )
                    } else {
                        format!(
                            "Used the computer · {n} action{}",
                            if n == 1 { "" } else { "s" }
                        )
                    };
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .id(ElementId::Name(format!("chat-{id}-acts-{i}").into()))
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .py(px(3.0))
                                .text_size(px(Type::META + 0.5))
                                .text_color(t.tertiary)
                                .cursor_pointer()
                                .hover(|s| s.text_color(t.secondary))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(Item::Actions { open, .. }) =
                                        this.chat_mut(id).and_then(|c| c.items.get_mut(i))
                                    {
                                        *open = !*open;
                                    }
                                    cx.notify();
                                }))
                                .child(if live {
                                    spinner(
                                        ElementId::Name(format!("chat-{id}-spin-{i}").into()),
                                        12.0,
                                        t.tertiary,
                                    )
                                    .into_any_element()
                                } else {
                                    icon(IconName::CursorClick, 12.0, t.tertiary).into_any_element()
                                })
                                .child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(summary),
                                )
                                .child(icon(
                                    if open {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    },
                                    11.0,
                                    t.quaternary,
                                )),
                        )
                        .when(open, |d| {
                            d.child(
                                div()
                                    .pl(px(4.0))
                                    .ml(px(5.0))
                                    .border_l_1()
                                    .border_color(t.hairline)
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .children(list.iter().map(|a| action_row(a, t))),
                            )
                        })
                        .into_any_element()
                }
                Item::Note(text) => div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(Type::META + 0.5))
                    .text_color(t.tertiary)
                    .child(icon(IconName::Clock, 12.0, t.quaternary))
                    .child(text.clone())
                    .into_any_element(),
                Item::Error(text) => {
                    notice(IconName::Warning, Ink::DANGER, text.clone(), t).into_any_element()
                }
            };
            out.push(el);
        }
        // The question waits here, in line, for an answer.
        if let Some(turn) = &chat.turn {
            if let Some(q) = &turn.question {
                out.push(
                    div()
                        .p(px(12.0))
                        .rounded(px(Radius::CARD))
                        .bg(Ink::ATTENTION.alpha(if t.dark { 0.09 } else { 0.08 }))
                        .border_1()
                        .border_color(Ink::ATTENTION.alpha(0.35))
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.0))
                                .child(icon(IconName::Hand, 15.0, t.ink(Ink::ATTENTION)))
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child("The agent asks for your go-ahead"),
                                ),
                        )
                        .child(
                            div()
                                .text_size(px(Type::BODY))
                                .line_height(px(19.0))
                                .text_color(t.primary.alpha(0.9))
                                .child(q.what.clone()),
                        )
                        .when(!q.why.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_size(px(Type::META + 0.5))
                                    .line_height(px(17.0))
                                    .text_color(t.secondary)
                                    .child(q.why.clone()),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .gap(px(8.0))
                                .child(button("answer-yes", "Allow", t).solid().on_click(
                                    cx.listener(move |this, _, _, cx| {
                                        if let Some(c) = this.chat_mut(id) {
                                            c.answer(true);
                                        }
                                        cx.notify();
                                    }),
                                ))
                                .child(button("answer-no", "Don't", t).on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        if let Some(c) = this.chat_mut(id) {
                                            c.answer(false);
                                        }
                                        cx.notify();
                                    },
                                ))),
                        )
                        .into_any_element(),
                );
            } else {
                let status = if chat.held {
                    format!(
                        "Waiting for you to hand back the keyboard and mouse (up to {} minutes)",
                        ping_agent::computer::HOLD_WAIT.as_secs() / 60
                    )
                } else if turn.paused {
                    "Paused before its next action".to_string()
                } else {
                    turn.status
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "Thinking…".into())
                };
                out.push(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(Type::META + 0.5))
                        .text_color(t.tertiary)
                        .child(spinner(
                            ElementId::Name(format!("chat-{id}-working").into()),
                            12.0,
                            t.tertiary,
                        ))
                        .child(status)
                        .child(
                            div()
                                .text_color(t.quaternary)
                                .child(clock(turn.started.elapsed())),
                        )
                        .into_any_element(),
                );
            }
        }
        out
    }

    pub(super) fn chat_composer(
        &mut self,
        id: u64,
        busy: bool,
        ended: bool,
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let chat = self
            .agents
            .chats
            .iter()
            .find(|c| c.id == id)
            .expect("the chat");
        let focused = chat.composer.read(cx).is_focused(window);
        let empty = chat.composer.read(cx).text().trim().is_empty();
        let paused = chat.turn.as_ref().is_some_and(|t| t.paused);
        let approvals = chat.settings.approvals;
        let ask = approvals == Approvals::Every;
        let composer = chat.composer.clone();
        let action: AnyElement = if busy {
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(
                    icon_button(
                        "chat-pause",
                        if paused {
                            IconName::Play
                        } else {
                            IconName::Pause
                        },
                        if paused {
                            "Resume"
                        } else {
                            "Pause before the next action"
                        },
                        t,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(c) = this.chat_mut(id) {
                            c.toggle_pause();
                        }
                        cx.notify();
                    })),
                )
                .child(
                    div()
                        .id("chat-stop")
                        .size(px(30.0))
                        .rounded(px(15.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(t.solid())
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.85))
                        .tooltip(|_, cx| {
                            pingpong_ui::tooltip("Stop the agent (the session stays open)", cx)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(c) = this.chat_mut(id) {
                                c.stop();
                            }
                            cx.notify();
                        }))
                        .child(div().size(px(10.0)).rounded(px(2.0)).bg(t.on_solid())),
                )
                .into_any_element()
        } else {
            let can = !empty && !ended;
            div()
                .id("chat-send")
                .size(px(30.0))
                .rounded(px(15.0))
                .flex()
                .items_center()
                .justify_center()
                .bg(if can {
                    t.solid()
                } else {
                    t.primary.alpha(0.12)
                })
                .child(icon(
                    IconName::ArrowUp,
                    16.0,
                    if can { t.on_solid() } else { t.quaternary },
                ))
                .when(can, |d| {
                    d.cursor_pointer()
                        .hover(|s| s.opacity(0.85))
                        .on_click(cx.listener(move |this, _, _, cx| this.chat_send(id, cx)))
                })
                .tooltip(|_, cx| {
                    pingpong_ui::tooltip("Send (Return). Shift+Return adds a line.", cx)
                })
                .into_any_element()
        };
        div()
            .w_full()
            .flex()
            .items_end()
            .gap(px(8.0))
            .px(px(12.0))
            .py(px(8.0))
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
                    .flex_1()
                    .min_w_0()
                    .py(px(4.0))
                    .max_h(px(180.0))
                    .overflow_hidden()
                    .child(field(&composer, t).bare().min_rows(1)),
            )
            .child(
                div()
                    .id("chat-ask")
                    .flex_none()
                    .h(px(26.0))
                    .px(px(7.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(Radius::CONTROL))
                    .text_size(px(12.0))
                    .text_color(if ask { t.ink(Ink::ACCENT) } else { t.tertiary })
                    .when(ask, |d| d.bg(Ink::ACCENT.alpha(0.12)))
                    .when(!busy, |d| {
                        d.cursor_pointer()
                            .hover(|s| s.bg(t.hover))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(c) = this.chat_mut(id) {
                                    // Risky steps, then every step, then never.
                                    c.settings.approvals = match c.settings.approvals {
                                        Approvals::Risky => Approvals::Every,
                                        Approvals::Every => Approvals::Off,
                                        Approvals::Off => Approvals::Risky,
                                    };
                                }
                                cx.notify();
                            }))
                    })
                    .tooltip(move |_, cx| {
                        let next = match approvals {
                            Approvals::Risky => "every step",
                            Approvals::Every => "never",
                            Approvals::Off => "risky steps",
                        };
                        pingpong_ui::tooltip(
                            format!("{} Click to ask before {next}.", approvals.describe()),
                            cx,
                        )
                    })
                    .child(icon(
                        IconName::Hand,
                        13.0,
                        if ask { t.ink(Ink::ACCENT) } else { t.tertiary },
                    ))
                    .when(approvals != Approvals::Risky, |d| {
                        d.child(if ask { "Every step" } else { "Off" })
                    }),
            )
            .child(action)
    }
}
