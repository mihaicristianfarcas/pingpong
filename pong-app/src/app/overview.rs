//! The overview: a pairing request, the session with its numbers, this host.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight};
use pingpong_ui::{
    button, chip, icon, label_stack, live_dot, rows, section, stat, IconName, Ink, Radius, Theme,
    Type,
};

use crate::api::{LogEntry, Status};
use crate::worker::Cmd;

use super::{ago, device_word, duration, info_row, now_ms, page, Confirm, Page, PongApp};

impl PongApp {
    pub(super) fn overview(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(s) = self.snap.status.clone() else {
            return super::signin::waiting("overview-page", "Asking Pong how it is…", t);
        };
        let subtitle = match &s.session {
            Some(sess) if sess.agent.is_some() => format!(
                "An AI agent, {}, is using this {}.",
                sess.client,
                device_word(self.os())
            ),
            Some(sess) => format!("Streaming to {}.", sess.client),
            None => format!(
                "Ready for Ping. {} paired device{}.",
                s.clients,
                if s.clients == 1 { "" } else { "s" }
            ),
        };
        let pending = s.pending.first().cloned();
        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(pingpong_ui::page_header(
                s.name.clone(),
                Some(subtitle.into()),
                None,
                t,
            ));
        if let Some(p) = pending {
            content =
                content.child(
                    div()
                        .id("pairing-banner")
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .px(px(14.0))
                        .py(px(12.0))
                        .rounded(px(Radius::CARD))
                        .bg(Ink::ACCENT.alpha(if t.dark { 0.12 } else { 0.10 }))
                        .border_1()
                        .border_color(Ink::ACCENT.alpha(0.35))
                        .child(icon(IconName::Link, 16.0, t.ink(Ink::ACCENT)))
                        .child(label_stack(
                            format!("{} wants to pair", p.client_name),
                            Some("Enter the PIN it shows to let it connect.".into()),
                            t,
                        ))
                        .child(button("go-pair", "Enter PIN", t).solid().on_click(
                            cx.listener(|this, _, _, cx| this.set_page(Page::Devices, cx)),
                        )),
                );
        }
        content = content.child(self.session_card(&s, t, cx));
        content = content.child(section("This host", t).child(rows(
            [
                info_row("Name", s.name.clone(), false, t),
                info_row("Host ID", s.id.clone(), true, t),
                info_row("Tunnel", format!("UDP {}", s.port), false, t),
                info_row(
                    "Internet",
                    if !s.internet {
                        "Off".to_string()
                    } else if s.public.is_empty() {
                        "On · finding its address…".to_string()
                    } else {
                        format!("On · {}", s.public.join(", "))
                    },
                    false,
                    t,
                ),
                info_row(
                    "Router",
                    if s.internet && !s.port_mapping.is_empty() {
                        s.port_mapping.clone()
                    } else {
                        "—".into()
                    },
                    false,
                    t,
                ),
                info_row(
                    "Devices",
                    format!("{} paired · {} connected now", s.clients, s.tunnels),
                    false,
                    t,
                ),
                info_row("Version", format!("Pong {}", s.version), false, t),
            ],
            t,
        )));
        if !self.snap.log.is_empty() {
            let now = now_ms();
            let entries: Vec<LogEntry> = self.snap.log.iter().rev().take(40).cloned().collect();
            content = content.child(section("Agent activity", t).child(rows(
                entries.into_iter().map(|e| {
                    div()
                        .min_h(px(34.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .child(
                            div()
                                .w(px(46.0))
                                .flex_none()
                                .text_size(px(Type::META))
                                .text_color(t.tertiary)
                                .child(ago(now.saturating_sub(e.unix_ms) / 1000)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .font_family(pingpong_ui::mono_font())
                                .text_size(px(12.0))
                                .text_color(t.primary.alpha(0.86))
                                .child(e.text),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(Type::META))
                                .text_color(t.tertiary)
                                .child(e.client),
                        )
                        .into_any_element()
                }),
                t,
            )));
        }
        page(t, [], "overview-page", content)
    }

    pub(super) fn session_card(
        &mut self,
        s: &Status,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(sess) = s.session.clone() else {
            return section("Session", t)
                .child(
                    pingpong_ui::card(t).child(
                        div()
                            .px(px(14.0))
                            .py(px(16.0))
                            .flex()
                            .items_center()
                            .gap(px(12.0))
                            .child(
                                div()
                                    .size(px(34.0))
                                    .rounded(px(Radius::CARD))
                                    .bg(t.primary.alpha(0.06))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(icon(IconName::Display, 17.0, t.secondary)),
                            )
                            .child(label_stack(
                                "Nobody is streaming",
                                Some(
                                    format!("Open Ping on a paired device and choose {}.", s.name)
                                        .into(),
                                ),
                                t,
                            )),
                    ),
                )
                .into_any_element();
        };
        let since = now_ms() / 1000 - sess.started_unix.min(now_ms() / 1000);
        let meta = format!(
            "{} × {} · {} fps · {} · {} Mbps · {}",
            sess.width,
            sess.height,
            sess.fps,
            sess.codec.to_uppercase(),
            sess.bitrate_kbps / 1000,
            duration(since)
        );
        let mut actions: Vec<AnyElement> = Vec::new();
        let agent = sess.agent.clone();
        if let Some(a) = &agent {
            if a.controller.is_some() {
                actions.push(
                    button("hand-back", "Hand Back", t)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.send(Cmd::AgentOp("handback"));
                            cx.notify();
                        }))
                        .into_any_element(),
                );
            }
            let paused = a.flags & 1 != 0;
            actions.push(
                button("agent-pause", if paused { "Resume" } else { "Pause" }, t)
                    .icon(if paused {
                        IconName::Play
                    } else {
                        IconName::Pause
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.send(Cmd::AgentOp(if paused { "resume" } else { "pause" }));
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            actions.push(
                button("agent-stop", "Stop Agent", t)
                    .danger()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm = Some(Confirm::StopAgent);
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        } else {
            let client = sess.client.clone();
            actions.push(
                button("end-session", "End Session", t)
                    .danger()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.confirm = Some(Confirm::EndSession(client.clone()));
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let stats = [
            (format!("{:.0}", sess.encoded_fps), "frames / s"),
            (format!("{:.1}", sess.mbps), "Mbit/s sent"),
            (format!("{:.1} ms", sess.host_latency_ms), "host latency"),
            (
                if sess.rtt_ms > 0.0 {
                    format!("{:.1} ms", sess.rtt_ms)
                } else {
                    "—".into()
                },
                "round trip",
            ),
            (format!("{:.2}%", sess.client_loss_pct), "packet loss"),
            (sess.recoveries.to_string(), "recoveries"),
        ];
        let mut stat_row = div().flex();
        for (i, (v, l)) in stats.into_iter().enumerate() {
            if i > 0 {
                stat_row = stat_row.child(div().w(px(1.0)).my(px(10.0)).bg(t.hairline));
            }
            stat_row = stat_row.child(stat(v, l, t));
        }
        let agent_line = agent.map(|a| {
            let holds = [
                (1u8, "paused"),
                (16, "a person took over"),
                (2, "someone is using this computer"),
                (4, "a secure screen is up"),
                (8, "view only"),
            ];
            let held: Vec<&str> = holds
                .iter()
                .filter(|(bit, _)| a.flags & bit != 0)
                .map(|(_, t)| *t)
                .collect();
            let mut line = if held.is_empty() {
                "In control of the keyboard and mouse".to_string()
            } else {
                format!("Held: {}", held.join(", "))
            };
            if !a.watchers.is_empty() {
                line.push_str(&format!(" · watched by {}", a.watchers.join(", ")));
            }
            line
        });
        section("Session", t)
            .child(
                pingpong_ui::card(t)
                    .child(
                        div()
                            .px(px(14.0))
                            .py(px(14.0))
                            .flex()
                            .items_center()
                            .gap(px(12.0))
                            .child(live_dot(if sess.agent.is_some() {
                                Ink::ACCENT
                            } else {
                                Ink::FRESH
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.0))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(
                                                div()
                                                    .text_size(px(Type::TITLE))
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .child(sess.client.clone()),
                                            )
                                            .when(sess.agent.is_some(), |d| {
                                                d.child(chip("AI agent", t.ink(Ink::ACCENT), t))
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(Type::META + 0.5))
                                            .text_color(t.tertiary)
                                            .child(meta),
                                    )
                                    .children(agent_line.map(|l| {
                                        div()
                                            .text_size(px(Type::META + 0.5))
                                            .text_color(t.secondary)
                                            .child(l)
                                    })),
                            )
                            .child(div().flex_none().flex().gap(px(6.0)).children(actions)),
                    )
                    .child(pingpong_ui::hairline(t))
                    .child(stat_row),
            )
            .into_any_element()
    }
}
