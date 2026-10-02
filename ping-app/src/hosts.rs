//! Moonlight's PC list: every paired host and every host seen on the network,
//! as quiet cards. A click streams (or wakes, or pairs); a right click, or
//! the card's "…", has the rest.

use gpui::{
    div, prelude::*, px, AnyElement, Context, ElementId, FontWeight, MouseButton, MouseDownEvent,
    SharedString, Window,
};
use pingpong_ui::{
    button, icon, icon_button, rows, section, setting, spinner, IconName, Ink, Metrics, Radius,
    Theme, Type,
};

use crate::app::{page_body, toolbar, Page, PingApp};
use crate::model::Item;
use crate::settings::{mbps, Tab};

const CARD_W: f32 = 200.0;
const CARD_H: f32 = 176.0;

impl PingApp {
    pub fn render_hosts(
        &mut self,
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let bar = toolbar([
            icon_button(
                "refresh",
                IconName::Refresh,
                format!("Look for hosts again ({})", pingpong_ui::shortcut("R")),
                t,
            )
            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
            .into_any_element(),
            button("add-host", "Add Host", t)
                .icon(IconName::Plus)
                .tooltip(format!(
                    "Pair with a host by its address ({})",
                    pingpong_ui::shortcut("N")
                ))
                .on_click(cx.listener(|this, _, window, cx| this.show_add_host(window, cx)))
                .into_any_element(),
        ]);
        if self.model.items.is_empty() {
            return div()
                .size_full()
                .flex()
                .flex_col()
                .child(bar)
                .child(self.empty(t, cx))
                .into_any_element();
        }
        let paired = self.model.items.iter().filter(|i| i.is_paired()).count();
        let online = self
            .model
            .items
            .iter()
            .filter(|i| i.is_paired() && i.online == Some(true))
            .count();
        let unknown = self
            .model
            .items
            .iter()
            .any(|i| i.is_paired() && i.online.is_none());
        let found = self.model.items.len() - paired;
        let mut parts = vec![format!("{paired} paired")];
        if paired > 0 {
            parts.push(match (online, unknown) {
                (0, true) => "checking…".into(),
                (0, false) => "none online".into(),
                (n, _) => format!("{n} online"),
            });
        }
        if found > 0 {
            parts.push(format!("{found} more on your network"));
        }
        let native = self.native(window, cx);
        let (w, h, fps) = self.prefs.mode(&native);
        let summary = format!(
            "A click streams at {w} × {h}, {fps} FPS, {}.",
            mbps(self.prefs.bitrate(&native))
        );
        let cards: Vec<AnyElement> = self
            .model
            .items
            .clone()
            .iter()
            .map(|item| self.card(item, t, cx).into_any_element())
            .collect();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(page_body(
                "hosts-page",
                1100.0,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(22.0))
                    .child(pingpong_ui::page_header(
                        "Hosts",
                        Some(parts.join(" · ").into()),
                        None,
                        t,
                    ))
                    .child(div().flex().flex_wrap().gap(px(14.0)).children(cards))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .text_size(px(Type::META + 0.5))
                            .text_color(t.tertiary)
                            .child(icon(IconName::Display, 13.0, t.tertiary))
                            .child(summary)
                            .child(
                                div()
                                    .id("hosts-video-settings")
                                    .text_color(t.secondary)
                                    .cursor_pointer()
                                    .hover(|s| s.text_color(t.primary))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.set_page(Page::Settings(Tab::Video), cx)
                                    }))
                                    .child("Video settings"),
                            ),
                    ),
            ))
            .into_any_element()
    }

    fn card(&mut self, item: &Item, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let waking = self.model.is_waking(item);
        let group: SharedString = format!("host-{}", item.id).into();
        let (status, tint, busy) = status(item, waking, t);
        let off = item.online == Some(false) && item.is_paired();
        let glyph = if item.is_mac() {
            IconName::Laptop
        } else {
            IconName::Monitor
        };
        let open = item.clone();
        let right = item.clone();
        let more = item.clone();
        let hint = if !item.is_paired() {
            "Pair"
        } else if item.offline_but_wakeable() {
            "Wake"
        } else {
            "Stream"
        };
        let open_menu = self.menu.as_ref().is_some_and(|(i, _)| i.id == item.id);
        div()
            .id(ElementId::Name(group.clone()))
            .group(group.clone())
            .relative()
            .w(px(CARD_W))
            .h(px(CARD_H))
            .flex()
            .flex_col()
            .items_center()
            .rounded(px(Radius::PANEL))
            .bg(t.card)
            .border_1()
            .border_color(t.card_stroke)
            .cursor_pointer()
            .hover(|s| {
                s.bg(t.primary.alpha(if t.dark { 0.05 } else { 0.9 }))
                    .border_color(t.primary.alpha(0.13))
            })
            .active(|s| s.opacity(0.85))
            .when(open_menu, |d| d.border_color(t.primary.alpha(0.18)))
            .on_click(cx.listener(move |this, _, window, cx| this.open(&open, window, cx)))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| {
                    this.menu = Some((right.clone(), e.position));
                    cx.notify();
                }),
            )
            // The computer, dimmed while it is off; a lock until it is paired.
            .child(
                div()
                    .mt(px(30.0))
                    .relative()
                    .size(px(58.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(
                        glyph,
                        52.0,
                        if off {
                            t.quaternary
                        } else {
                            t.primary.alpha(0.86)
                        },
                    ))
                    .when(!item.is_paired(), |d| {
                        d.child(
                            div()
                                .absolute()
                                .right(px(-4.0))
                                .bottom(px(2.0))
                                .size(px(22.0))
                                .rounded(px(11.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .bg(t.floating)
                                .border_1()
                                .border_color(t.card_stroke)
                                .child(icon(IconName::Lock, 12.0, t.ink(Ink::ATTENTION))),
                        )
                    }),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .px(px(16.0))
                    .max_w_full()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(Type::BODY + 1.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.primary)
                    .child(item.name.clone()),
            )
            .child(
                div()
                    .mt(px(5.0))
                    .h(px(16.0))
                    .relative()
                    .w_full()
                    .flex()
                    .justify_center()
                    // The status, and in its place on hover what a click does.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(px(Type::META + 0.5))
                            .text_color(t.secondary)
                            .group_hover(group.clone(), |s| s.opacity(0.0))
                            .child(if busy {
                                spinner(ElementId::Name(format!("{group}-spin").into()), 10.0, tint)
                                    .into_any_element()
                            } else {
                                pingpong_ui::dot(tint, 6.0).into_any_element()
                            })
                            .child(status),
                    )
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .justify_center()
                            .items_center()
                            .gap(px(4.0))
                            .opacity(0.0)
                            .group_hover(group.clone(), |s| s.opacity(1.0))
                            .text_size(px(Type::META + 0.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(t.primary)
                            .child(hint)
                            .child(icon(IconName::ArrowRight, 11.0, t.primary)),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(px(8.0))
                    .right(px(8.0))
                    .opacity(if open_menu { 1.0 } else { 0.0 })
                    .group_hover(group, |s| s.opacity(1.0))
                    .child(
                        icon_button(
                            ElementId::Name(format!("more-{}", item.id).into()),
                            IconName::More,
                            "More",
                            t,
                        )
                        .on_click(cx.listener(
                            move |this, e: &gpui::ClickEvent, _, cx| {
                                cx.stop_propagation();
                                this.menu = Some((more.clone(), e.position()));
                                cx.notify();
                            },
                        )),
                    ),
            )
    }

    /// No host yet: the page as it is with hosts, its rows saying how to
    /// get one.
    fn empty(&mut self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let searching = self.model.searching;
        let look: AnyElement = if searching {
            div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .text_size(px(Type::META + 0.5))
                .text_color(t.tertiary)
                .child(spinner("empty-searching", 11.0, t.tertiary))
                .child("Looking…")
                .into_any_element()
        } else {
            button("empty-refresh", "Look Again", t)
                .on_click(cx.listener(|this, _, _, cx| this.refresh(cx)))
                .into_any_element()
        };
        page_body(
            "hosts-page",
            1100.0,
            div()
                .flex()
                .flex_col()
                .gap(px(22.0))
                .child(pingpong_ui::page_header(
                    "Hosts",
                    Some(
                        if searching {
                            "None yet. Looking on your network…"
                        } else {
                            "None yet."
                        }
                        .into(),
                    ),
                    None,
                    t,
                ))
                .child(
                    section("Your first host", t)
                        .max_w(px(Metrics::FORM))
                        .child(rows(
                            [
                                setting(
                                    "On this network",
                                    Some(
                                        "Install Pong on the PC or Mac you want to stream \
                                            from: it shows up here by itself."
                                            .into(),
                                    ),
                                    look,
                                    t,
                                )
                                .into_any_element(),
                                setting(
                                    "Anywhere else",
                                    Some(
                                        "Add it by its address or name. It pairs with a \
                                            PIN, as a host found on the network does."
                                            .into(),
                                    ),
                                    button("empty-add", "Add Host…", t).solid().on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.show_add_host(window, cx)
                                        }),
                                    ),
                                    t,
                                )
                                .into_any_element(),
                            ],
                            t,
                        )),
                ),
        )
    }
}

fn status(item: &Item, waking: bool, t: Theme) -> (&'static str, gpui::Rgba, bool) {
    if !item.is_paired() {
        return ("Not paired", t.ink(Ink::ATTENTION), false);
    }
    if waking && item.online != Some(true) {
        return ("Waking…", t.ink(Ink::ATTENTION), true);
    }
    match item.online {
        Some(true) => ("Online", t.ink(Ink::FRESH), false),
        Some(false) if item.can_wake() => ("Asleep · click to wake", Ink::IDLE, false),
        Some(false) => ("Offline", Ink::IDLE, false),
        None => ("Checking…", t.quaternary, true),
    }
}
