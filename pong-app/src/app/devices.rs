//! Devices: pairing requests first, then the paired devices and agents.

use gpui::{div, prelude::*, px, AnyElement, Context, ElementId, FontWeight};
use pingpong_ui::{
    button, chip, field, icon, icon_button, notice, rows, section, select, IconName, Ink, Metrics,
    Theme, Type,
};

use crate::api::{Client, Pending};
use crate::worker::Cmd;

use super::{date, duration, page, Confirm, PongApp};

impl PongApp {
    pub(super) fn devices(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let pending: Vec<Pending> = self
            .snap
            .status
            .as_ref()
            .map(|s| s.pending.clone())
            .unwrap_or_default();
        let name = self.snap.state.name.clone();
        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(pingpong_ui::page_header(
                "Devices",
                Some("Who may connect. Unpairing a device revokes its key at once.".into()),
                None,
                t,
            ));
        if let Some((glyph, tint, text, _)) = self.note.clone() {
            content = content.child(notice(glyph, tint, text, t));
        }
        let requests: Vec<AnyElement> = if pending.is_empty() {
            vec![div()
                .px(px(12.0))
                .py(px(14.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(icon(IconName::Link, 15.0, t.tertiary))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.5))
                        .line_height(px(17.0))
                        .text_color(t.tertiary)
                        .child(format!(
                            "To pair a device, choose {name} in Ping (or add it by \
                                address). Its request shows here, with the PIN to type."
                        )),
                )
                .into_any_element()]
        } else {
            pending.iter().map(|p| self.request_row(p, t, cx)).collect()
        };
        content = content.child(section("Pairing requests", t).child(rows(requests, t)));
        let clients = self.snap.clients.clone();
        let paired: Vec<AnyElement> = if clients.is_empty() {
            vec![div()
                .px(px(12.0))
                .py(px(14.0))
                .text_size(px(12.5))
                .text_color(t.tertiary)
                .child("No paired devices yet.")
                .into_any_element()]
        } else {
            clients.iter().map(|c| self.client_row(c, t, cx)).collect()
        };
        content = content.child(section("Paired", t).child(rows(paired, t)));
        page(t, [], "devices-page", content)
    }

    pub(super) fn request_row(
        &mut self,
        p: &Pending,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = p.id;
        let pin = self.pins.get(&id).cloned();
        let has_pin = pin
            .as_ref()
            .is_some_and(|f| !f.read(cx).text().trim().is_empty());
        let error = self.pin_errors.get(&id).cloned();
        let submit = pin.clone();
        div()
            .px(px(12.0))
            .py(px(12.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(icon(if p.agent { IconName::Agent } else { IconName::Laptop }, 18.0, t.secondary))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(div().text_size(px(Type::BODY)).font_weight(FontWeight::MEDIUM).child(p.client_name.clone()))
                                    .when(p.agent, |d| d.child(chip("AI agent", t.ink(Ink::ACCENT), t))),
                            )
                            .child(div().text_size(px(Type::META)).text_color(t.tertiary).child(format!("From {} · waiting {}", p.peer, duration(p.waiting_secs)))),
                    )
                    .children(pin.map(|f| div().w(px(92.0)).child(field(&f, t))))
                    .child(button(ElementId::Name(format!("pin-{id}").into()), "Pair", t).solid().disabled(!has_pin).on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = &submit {
                            let pin = f.read(cx).text().trim().to_string();
                            this.pin_errors.remove(&id);
                            this.send(Cmd::SubmitPin(id, pin));
                            cx.notify();
                        }
                    })))
                    .child(button(ElementId::Name(format!("decline-{id}").into()), "Decline", t).on_click(cx.listener(move |this, _, _, cx| {
                        this.send(Cmd::Decline(id));
                        cx.notify();
                    }))),
            )
            .when(p.agent, |d| {
                d.child(div().pl(px(28.0)).text_size(px(Type::META + 0.5)).line_height(px(16.0)).text_color(t.tertiary).child(
                    "An AI agent asks to use this computer. Paired, it sees the screen and \
                        uses the keyboard and mouse, but never while a person streams, \
                        never on a secure screen, and you can pause, watch or stop it.",
                ))
            })
            .children(error.map(|e| div().pl(px(28.0)).child(notice(IconName::Warning, Ink::DANGER, e, t))))
            .into_any_element()
    }

    pub(super) fn client_row(
        &mut self,
        c: &Client,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let detail = format!("Paired {} · {}", date(c.paired_at), c.id);
        let access: AnyElement = if c.agent {
            let values = ["control", "view", "off"];
            let key = c.key.clone();
            let this = cx.weak_entity();
            select(
                ElementId::Name(format!("access-{}", c.id).into()),
                ["See and control", "See only", "Off"],
                values.iter().position(|v| *v == c.access),
                t,
            )
            .width(150.0)
            .on_select(move |i, _, cx| {
                let _ = this.update(cx, |app, cx| {
                    app.send(Cmd::Access(key.clone(), values[i].to_string()));
                    cx.notify();
                });
            })
            .into_any_element()
        } else {
            div()
                .text_size(px(12.0))
                .text_color(t.tertiary)
                .child("Full access")
                .into_any_element()
        };
        let client = c.clone();
        div()
            .min_h(px(Metrics::SETTINGS_ROW))
            .px(px(12.0))
            .py(px(8.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(icon(
                if c.agent {
                    IconName::Agent
                } else {
                    IconName::Laptop
                },
                18.0,
                t.secondary,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(px(Type::BODY))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(c.name.clone()),
                            )
                            .when(c.agent, |d| {
                                d.child(chip("AI agent", t.ink(Ink::ACCENT), t))
                            })
                            .when(c.online, |d| {
                                d.child(chip("Connected", t.ink(Ink::FRESH), t))
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(Type::META))
                            .text_color(t.tertiary)
                            .child(detail),
                    ),
            )
            .child(access)
            .child(
                icon_button(
                    ElementId::Name(format!("unpair-{}", c.id).into()),
                    IconName::Trash,
                    "Unpair…",
                    t,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.confirm = Some(Confirm::Unpair(client.clone()));
                    cx.notify();
                })),
            )
            .into_any_element()
    }
}
