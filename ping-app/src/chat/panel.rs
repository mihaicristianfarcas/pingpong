//! What the agent sees: the latest screen, every step so far, the plan and the session.

use std::time::Duration;

use gpui::{
    div, img, prelude::*, px, AnyElement, Context, ElementId, FontWeight, ObjectFit, Window,
};
use ping_agent::computer::{PlanStatus, PlanStep};
use pingpong_proto::control::agent_state;
use pingpong_ui::{icon, IconName, Ink, Radius, Theme, Type};

use crate::app::PingApp;

use super::{provider_name, thousands, Chat};

/// Where the agent acted: a ring and a dot, on a picture `w` × `h`.
pub(super) fn marker(
    p: (f32, f32),
    w: f32,
    h: f32,
    size: f32,
    ok: bool,
    t: Theme,
) -> impl IntoElement {
    let c = t.ink(if ok { Ink::ACCENT } else { Ink::ATTENTION });
    let (x, y) = (
        p.0.clamp(0.0, 1.0) * w - size / 2.0,
        p.1.clamp(0.0, 1.0) * h - size / 2.0,
    );
    div()
        .absolute()
        .left(px(x))
        .top(px(y))
        .size(px(size))
        .rounded_full()
        .border_2()
        .border_color(c)
        .bg(c.alpha(0.22))
        .flex()
        .items_center()
        .justify_center()
        .child(div().size(px((size * 0.28).max(3.0))).rounded_full().bg(c))
}

/// The model's plan: what is done, what it is on, what is left.
pub(super) fn plan_card(plan: &[PlanStep], t: Theme) -> impl IntoElement {
    let done = plan.iter().filter(|p| p.status == PlanStatus::Done).count();
    let title = if plan.is_empty() {
        "Plan".to_string()
    } else {
        format!("Plan · {done} of {}", plan.len())
    };
    let body = if plan.is_empty() {
        pingpong_ui::card(t)
            .px(px(12.0))
            .py(px(10.0))
            .text_size(px(Type::META + 0.5))
            .line_height(px(16.0))
            .text_color(t.tertiary)
            .child(
                "When a task has several steps, the agent's plan shows here, ticked off \
                    as it goes.",
            )
    } else {
        pingpong_ui::card(t)
            .py(px(4.0))
            .children(plan.iter().map(|p| {
                let glyph = match p.status {
                    PlanStatus::Done => {
                        icon(IconName::CheckCircle, 14.0, t.ink(Ink::FRESH)).into_any_element()
                    }
                    PlanStatus::InProgress => pingpong_ui::live_dot(Ink::ACCENT).into_any_element(),
                    PlanStatus::Pending => div()
                        .size(px(10.0))
                        .rounded_full()
                        .border_1()
                        .border_color(t.quaternary)
                        .into_any_element(),
                };
                div()
                    .flex()
                    .items_start()
                    .gap(px(9.0))
                    .px(px(12.0))
                    .py(px(5.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(14.0))
                            .h(px(17.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(glyph),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(Type::BODY - 0.5))
                            .line_height(px(17.0))
                            .text_color(match p.status {
                                PlanStatus::Done => t.tertiary,
                                PlanStatus::InProgress => t.primary,
                                PlanStatus::Pending => t.secondary,
                            })
                            .when(p.status == PlanStatus::InProgress, |d| {
                                d.font_weight(FontWeight::MEDIUM)
                            })
                            .child(p.text.clone()),
                    )
            }))
    };
    pingpong_ui::section(title, t).pt(px(8.0)).child(body)
}

/// The host, who does the thinking, who has the keyboard and mouse, the
/// connection, this turn, the tokens. The page's title bar holds only the
/// session's title, so this card is where the rest is said.
pub(super) fn session_card(chat: &Chat, watching: bool, t: Theme) -> impl IntoElement {
    let agent = if chat.settings.model.is_empty() {
        provider_name(chat.settings.provider).to_string()
    } else {
        format!(
            "{} · {}",
            provider_name(chat.settings.provider),
            chat.settings.model
        )
    };
    let control = match chat.link {
        None => "Not connected".to_string(),
        Some(l) => match l.agent {
            Some(a) if a.flags & agent_state::TAKEN_OVER != 0 => {
                if watching {
                    "You took over".into()
                } else {
                    "A person who logged in".into()
                }
            }
            Some(a) if a.flags & agent_state::SECURE_DESKTOP != 0 => "A secure screen is up".into(),
            Some(a) if a.flags & agent_state::PAUSED != 0 => "Paused".into(),
            Some(a) if a.flags & agent_state::LOCAL_INPUT != 0 => {
                format!("Someone at {}", chat.host)
            }
            Some(a) if a.flags & agent_state::VIEW_ONLY != 0 => "See only".into(),
            Some(a) if watching => format!(
                "The agent · you watch{}",
                if a.watchers > 1 {
                    format!(" with {}", a.watchers - 1)
                } else {
                    String::new()
                }
            ),
            Some(a) if a.watchers > 0 => format!("The agent · {} watching", a.watchers),
            _ => "The agent".into(),
        },
    };
    let connection = match chat.link {
        Some(l) if l.rtt_ms < 10.0 => format!("{:.1} ms · {:.1}% loss", l.rtt_ms, l.loss_pct),
        Some(l) => format!("{:.0} ms · {:.1}% loss", l.rtt_ms, l.loss_pct),
        None => "—".into(),
    };
    let turn = match &chat.turn {
        Some(turn) => {
            let waited = chat
                .conversation
                .as_ref()
                .map(|c| c.waited())
                .unwrap_or_default();
            let budget = Duration::from_secs(chat.settings.max_minutes.max(1) as u64 * 60) + waited;
            let left = budget.saturating_sub(turn.started.elapsed());
            format!(
                "{} of {} actions · {} min left",
                chat.turn_actions,
                chat.settings.max_actions,
                left.as_secs().div_ceil(60)
            )
        }
        None if chat.turn_actions > 0 => format!(
            "Last turn: {} action{}",
            chat.turn_actions,
            if chat.turn_actions == 1 { "" } else { "s" }
        ),
        None => "No turn yet".into(),
    };
    let tokens = if chat.tokens.0 + chat.tokens.1 == 0 {
        "—".to_string()
    } else {
        format!(
            "{} in · {} out{}",
            thousands(chat.tokens.0),
            thousands(chat.tokens.1),
            chat.tokens
                .2
                .map(|c| format!(" · ${c:.2}"))
                .unwrap_or_default()
        )
    };
    let row = |label: &'static str, value: String| {
        div()
            .min_h(px(32.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Type::BODY - 0.5))
                    .text_color(t.secondary)
                    .child(label),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Type::BODY - 0.5))
                    .child(value),
            )
            .into_any_element()
    };
    pingpong_ui::section("Session", t)
        .pt(px(8.0))
        .child(pingpong_ui::rows(
            [
                row("Host", chat.host.clone()),
                row("Agent", agent),
                row("Keyboard and mouse", control),
                row("Connection", connection),
                row("This turn", turn),
                row("Tokens", tokens),
            ],
            t,
        ))
}

pub(super) fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 5 {
        "just now".into()
    } else if s < 60 {
        format!("{s} s ago")
    } else {
        format!("{} min ago", s / 60)
    }
}

impl PingApp {
    /// "What the agent sees": the screen after its last action (or the step
    /// picked on the strip), with a marker where it acted; under it the strip
    /// of every step, the model's plan, and the session's state.
    pub(super) fn chat_panel(
        &mut self,
        id: u64,
        watching: bool,
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(chat) = self.agents.chats.iter().find(|c| c.id == id) else {
            return div().into_any_element();
        };
        let width = (f32::from(window.bounds().size.width) - pingpong_ui::Metrics::SIDEBAR) * 0.46;
        let width = width.clamp(340.0, 760.0);
        let (sw, sh) = (
            chat.settings.width.max(1) as f32,
            chat.settings.height.max(1) as f32,
        );
        let (pic_w, pic_h) = (width - 32.0, (width - 32.0) * sh / sw);
        let n = chat.steps.len();
        let shown = chat.selected.filter(|&i| i < n).or(n.checked_sub(1));
        let step = shown.map(|i| &chat.steps[i]);
        let picture: AnyElement = match step {
            Some(s) => div()
                .size_full()
                .relative()
                .child(
                    img(s.full.clone())
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
                .when_some(s.point, |d, p| {
                    d.child(marker(p, pic_w, pic_h, 22.0, s.ok, t))
                })
                .into_any_element(),
            None => div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(icon(IconName::Eye, 24.0, t.quaternary))
                .child(
                    div()
                        .max_w(px(260.0))
                        .text_center()
                        .text_size(px(Type::META + 0.5))
                        .line_height(px(16.0))
                        .text_color(t.tertiary)
                        .child(
                            "Nothing yet. The agent looks at the screen when a message \
                                needs the computer.",
                        ),
                )
                .into_any_element(),
        };
        let past = chat.selected.is_some_and(|i| i + 1 < n);
        let caption = match (past, step) {
            (true, Some(s)) => format!("Step {} of {n} · {}", shown.unwrap_or(0) + 1, s.caption),
            (false, Some(s)) => format!("After {} · {}", s.caption, ago(s.at.elapsed())),
            _ => format!("{} × {}", chat.settings.width, chat.settings.height),
        };
        let header = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Type::META))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.tertiary)
                    .child("What the agent sees"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_right()
                    .text_size(px(Type::META))
                    .text_color(t.quaternary)
                    .child(caption),
            )
            .when(past, |d| {
                d.child(
                    div()
                        .id(ElementId::Name(format!("chat-{id}-latest").into()))
                        .flex_none()
                        .px(px(7.0))
                        .py(px(2.0))
                        .rounded(px(Radius::CHIP))
                        .text_size(px(Type::META))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(t.ink(Ink::ACCENT))
                        .bg(t.ink(Ink::ACCENT).alpha(0.12))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(c) = this.chat_mut(id) {
                                c.selected = None;
                                c.strip_follow.set(true);
                            }
                            cx.notify();
                        }))
                        .child("Latest"),
                )
            });

        if chat.strip_follow.get() && chat.strip.bounds().size.width > px(0.0) && n > 0 {
            chat.strip.scroll_to_item(n - 1);
            chat.strip_follow.set(false);
        }
        // Every step, oldest first: a click shows it above.
        let (tw, th) = (84.0, 84.0 * sh / sw);
        let thumbs: Vec<AnyElement> = chat
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let on = Some(i) == shown;
                div()
                    .id(ElementId::Name(format!("chat-{id}-step-{i}").into()))
                    .flex_none()
                    .w(px(tw))
                    .h(px(th))
                    .relative()
                    .rounded(px(Radius::CONTROL))
                    .overflow_hidden()
                    .border_2()
                    .border_color(if on {
                        t.ink(Ink::ACCENT)
                    } else {
                        t.card_stroke
                    })
                    .bg(t.primary.alpha(0.04))
                    .cursor_pointer()
                    .when(!on, |d| d.hover(|st| st.border_color(t.primary.alpha(0.3))))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(c) = this.chat_mut(id) {
                            c.selected = if i + 1 == c.steps.len() {
                                None
                            } else {
                                Some(i)
                            };
                        }
                        cx.notify();
                    }))
                    .child(
                        img(s.thumb.clone())
                            .size_full()
                            .object_fit(ObjectFit::Cover),
                    )
                    .when_some(s.point, |d, p| {
                        d.child(marker(p, tw - 4.0, th - 4.0, 11.0, s.ok, t))
                    })
                    .into_any_element()
            })
            .collect();
        let strip = if thumbs.is_empty() {
            div()
                .px(px(2.0))
                .text_size(px(Type::META + 0.5))
                .text_color(t.tertiary)
                .child("Each action the agent takes adds a step here.")
                .into_any_element()
        } else {
            div()
                .id(ElementId::Name(format!("chat-{id}-strip").into()))
                .w_full()
                .flex()
                .gap(px(6.0))
                .pb(px(4.0))
                .overflow_x_scroll()
                .track_scroll(&chat.strip)
                .children(thumbs)
                .into_any_element()
        };

        div()
            .flex_none()
            .w(px(width))
            .h_full()
            .border_l_1()
            .border_color(t.hairline)
            .bg(t.sidebar.alpha(0.5))
            .child(
                div()
                    .id(ElementId::Name(format!("chat-{id}-panel").into()))
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&chat.panel_scroll)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(10.0))
                            .p(px(16.0))
                            .child(header)
                            .child(
                                div()
                                    .w_full()
                                    .h(px(pic_h))
                                    .rounded(px(Radius::CARD))
                                    .overflow_hidden()
                                    .bg(t.primary.alpha(0.04))
                                    .border_1()
                                    .border_color(t.card_stroke)
                                    .child(picture),
                            )
                            .child(
                                div()
                                    .text_size(px(Type::META + 0.5))
                                    .line_height(px(16.0))
                                    .text_color(t.tertiary)
                                    .child(if watching {
                                        format!(
                                            "You are logged in to this desktop in its own \
                                                window. {} takes the keyboard and mouse, and gives \
                                                them back.",
                                            crate::settings::chord("T")
                                        )
                                    } else {
                                        "A picture after each action, not live. Log in to see \
                                            the desktop live, and to take over."
                                            .into()
                                    }),
                            )
                            .child(
                                pingpong_ui::section(
                                    if n == 0 {
                                        "Steps".to_string()
                                    } else {
                                        format!("Steps · {n}")
                                    },
                                    t,
                                )
                                .pt(px(8.0))
                                .child(strip),
                            )
                            .child(plan_card(&chat.plan, t))
                            .child(session_card(chat, watching, t)),
                    ),
            )
            .into_any_element()
    }
}
