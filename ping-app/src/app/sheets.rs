//! The sheets over the window (pairing, adding a host, unpairing, alerts, what the update
//! check found) and a host's menu.

use gpui::{
    anchored, deferred, div, prelude::*, px, AnyElement, App, ClickEvent, Context, FontWeight,
    Window,
};
use pingpong_ui::{button, field, spinner, IconName, Radius, Theme};

use crate::model::{Item, PairState};

use super::{device_word, sheet_buttons, sheet_text, sheet_title, PingApp};

/// The PIN, one rounded box per digit.
pub(super) fn pin_digits(pin: &str, t: Theme) -> impl IntoElement {
    div()
        .py(px(4.0))
        .flex()
        .gap(px(10.0))
        .children(pin.chars().map(move |c| {
            div()
                .w(px(54.0))
                .h(px(66.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::CARD))
                .bg(t.primary.alpha(0.06))
                .border_1()
                .border_color(t.card_stroke)
                .text_size(px(34.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.primary)
                .child(c.to_string())
        }))
}

/// A host menu's entry: `f` on that host.
pub(super) fn item_action(
    cx: &mut Context<PingApp>,
    f: fn(&mut PingApp, &Item, &mut Window, &mut Context<PingApp>),
    item: &Item,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let (this, item) = (cx.weak_entity(), item.clone());
    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
        let _ = this.update(cx, |app, cx| f(app, &item, window, cx));
    }
}

impl PingApp {
    pub(super) fn sheets(
        &mut self,
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if let Some(p) = &self.pairing {
            let title = format!("Pair with {}", p.title);
            let body: AnyElement = match &p.state {
                PairState::Paired(name) => div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(sheet_title(format!("Paired with {name}"), t))
                    .child(sheet_text(
                        "It is in your hosts now: click it to start streaming.",
                        t,
                    ))
                    .child(
                        sheet_buttons().child(button("pair-done", "Done", t).solid().on_click(
                            cx.listener(|this, _, _, cx| {
                                this.pairing = None;
                                cx.notify();
                            }),
                        )),
                    )
                    .into_any_element(),
                PairState::Failed(error) => div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(sheet_title("Pairing didn't work", t))
                    .child(sheet_text(error.clone(), t))
                    .child(
                        sheet_buttons().child(button("pair-close", "Close", t).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.pairing = None;
                                cx.notify();
                            }),
                        )),
                    )
                    .into_any_element(),
                PairState::Connecting | PairState::Waiting => {
                    let waiting = p.state == PairState::Waiting;
                    let web = p.web_url.clone();
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(sheet_title(title, t))
                        .child(sheet_text(
                            "Enter this PIN on the host to let this device connect.",
                            t,
                        ))
                        .child(pin_digits(&p.pin, t))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(7.0))
                                .text_size(px(12.0))
                                .text_color(t.secondary)
                                .child(spinner("pair-spin", 12.0, t.tertiary))
                                .child(if waiting {
                                    "Waiting for the PIN on the host…"
                                } else {
                                    "Connecting…"
                                }),
                        )
                        .child(sheet_text(
                            "On the host, open Pong: the request is at the top of \
                                Devices. Pong's web UI works too, under Pair a device.",
                            t,
                        ))
                        .child(
                            sheet_buttons()
                                .when_some(web, |d, url| {
                                    d.child(
                                        button("pair-web", "Open Pong's Web UI", t)
                                            .icon(IconName::ExternalLink)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.open_url(&url, cx)
                                            })),
                                    )
                                })
                                .child(button("pair-cancel", "Cancel", t).on_click(cx.listener(
                                    |this, _, _, cx| {
                                        if let Some(p) = this.pairing.take() {
                                            p.cancel();
                                        }
                                        cx.notify();
                                    },
                                ))),
                        )
                        .into_any_element()
                }
            };
            return Some(pingpong_ui::sheet("pairing", 400.0, t, body).into_any_element());
        }
        if let Some(f) = self.add_host.clone() {
            let ok = !f.read(cx).text().trim().is_empty();
            let submit = f.clone();
            let body = div()
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(sheet_title("Add a host", t))
                .child(sheet_text(
                    "Enter the address or name of the computer running Pong. \
                        It pairs with a PIN, as a host found on the network does.",
                    t,
                ))
                .child(field(&f, t))
                .child(
                    sheet_buttons()
                        .child(button("add-cancel", "Cancel", t).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.add_host = None;
                                cx.notify();
                            },
                        )))
                        .child(
                            button("add-pair", "Pair…", t)
                                .solid()
                                .disabled(!ok)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let address = submit.read(cx).text().trim().to_string();
                                    this.add_host = None;
                                    let web = crate::model::web_url_for(&address);
                                    this.pair_with(address.clone(), address, web, false, cx);
                                })),
                        ),
                );
            return Some(pingpong_ui::sheet("add-host", 380.0, t, body).into_any_element());
        }
        if let Some(item) = self.confirm_unpair.clone() {
            let body =
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(sheet_title(format!("Unpair {}?", item.name), t))
                    .child(sheet_text(
                        format!(
                            "This {} forgets the host. To stream from it again, pair again \
                                (and remove this {} in Pong if you like).",
                            device_word(),
                            device_word()
                        ),
                        t,
                    ))
                    .child(
                        sheet_buttons()
                            .child(button("unpair-cancel", "Cancel", t).on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.confirm_unpair = None;
                                    cx.notify();
                                },
                            )))
                            .child(button("unpair-yes", "Unpair", t).danger().on_click(
                                cx.listener(move |this, _, _, cx| {
                                    tracing::info!(host = item.name, "unpaired");
                                    this.model.remove(&item);
                                    this.confirm_unpair = None;
                                    cx.notify();
                                }),
                            )),
                    );
            return Some(pingpong_ui::sheet("unpair", 360.0, t, body).into_any_element());
        }
        if let Some(a) = &self.alert {
            let body = div()
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(sheet_title(a.title.clone(), t))
                .child(sheet_text(a.message.clone(), t))
                .child(
                    sheet_buttons().child(button("alert-ok", "OK", t).solid().on_click(
                        cx.listener(|this, _, _, cx| {
                            this.alert = None;
                            cx.notify();
                        }),
                    )),
                );
            return Some(pingpong_ui::sheet("alert", 360.0, t, body).into_any_element());
        }
        if self.update_sheet {
            let (check, install, close) = (cx.weak_entity(), cx.weak_entity(), cx.weak_entity());
            let body = pingpong_ui::updates::sheet_body(
                &super::UPDATE_APP,
                &self.update_status,
                t,
                move |_, cx| {
                    let _ = check.update(cx, |this, _| this.updates.check_now());
                },
                move |_, cx| {
                    let _ = install.update(cx, |this, _| this.updates.install());
                },
                move |_, cx| {
                    let _ = close.update(cx, |this, cx| {
                        this.close_update_sheet();
                        cx.notify();
                    });
                },
            );
            return Some(pingpong_ui::sheet("updates", 380.0, t, body).into_any_element());
        }
        let _ = window;
        None
    }

    pub(super) fn host_menu(&mut self, t: Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (item, at) = self.menu.clone()?;
        let mut items = Vec::new();
        let it = item_action;
        if item.can_wake() && item.online != Some(true) {
            items.push(
                pingpong_ui::MenuItem::new(
                    format!("Wake {}", item.name),
                    it(cx, |a, i, _, cx| a.wake(i, cx), &item),
                )
                .icon(IconName::Power),
            );
        }
        if item.is_paired() {
            items.push(
                pingpong_ui::MenuItem::new(
                    "Stream Desktop",
                    it(cx, |a, i, w, cx| a.stream(i, false, w, cx), &item),
                )
                .icon(IconName::Display),
            );
            items.push(
                pingpong_ui::MenuItem::new(
                    "Stream Steam Big Picture",
                    it(cx, |a, i, w, cx| a.stream(i, true, w, cx), &item),
                )
                .icon(IconName::Gamepad),
            );
        } else {
            items.push(
                pingpong_ui::MenuItem::new(
                    "Pair…",
                    it(cx, |a, i, _, cx| a.start_pairing(i, cx), &item),
                )
                .icon(IconName::Link),
            );
        }
        if let Some(url) = item.web_url() {
            let open = cx
                .listener(move |this: &mut PingApp, _: &ClickEvent, _, cx| this.open_url(&url, cx));
            items.push(
                pingpong_ui::MenuItem::new("Open Pong's Web UI", open)
                    .icon(IconName::ExternalLink)
                    .separated(),
            );
        }
        if item.is_paired() {
            items.push(
                pingpong_ui::MenuItem::new(
                    "Unpair…",
                    it(cx, |a, i, _, cx| a.confirm_unpair(i, cx), &item),
                )
                .icon(IconName::Trash)
                .danger()
                .separated(),
            );
        }
        let close = cx.listener(|this: &mut PingApp, _: &gpui::MouseDownEvent, _, cx| {
            this.menu = None;
            cx.notify();
        });
        Some(
            deferred(
                anchored()
                    .position(at)
                    .snap_to_window_with_margin(px(8.0))
                    .child(
                        pingpong_ui::menu("host-menu", items, t)
                            .occlude()
                            .on_mouse_down_out(close),
                    ),
            )
            .priority(pingpong_ui::Layer::MENU)
            .into_any_element(),
        )
    }
}
