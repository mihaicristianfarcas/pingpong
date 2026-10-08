//! The sidebar: Hosts, Agents, the open sessions, the settings pages.

use gpui::{div, prelude::*, px, AnyElement, Context, ElementId, FontWeight};
use pingpong_ui::{dot, icon, spinner, IconName, Ink, Metrics, Radius, Theme, Type};

use crate::settings::Tab;

use super::{Page, PingApp};

/// Ping's ball, as in its icon.
pub fn brand_mark(size: f32) -> impl IntoElement {
    div()
        .flex_none()
        .size(px(size))
        .rounded(px(size / 2.0))
        .bg(Ink::BALL)
        .shadow(vec![gpui::BoxShadow {
            color: Ink::BALL.alpha(0.45).into(),
            offset: gpui::point(px(0.0), px(0.0)),
            blur_radius: px(size * 0.6),
            spread_radius: px(0.0),
            inset: false,
        }])
}

impl PingApp {
    pub(super) fn sidebar(&mut self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.page;
        let paired = self.model.items.iter().filter(|i| i.is_paired()).count();
        let nav = |id: &'static str,
                   label: &'static str,
                   name: IconName,
                   target: Page,
                   trailing: Option<AnyElement>,
                   cx: &mut Context<Self>| {
            let on = page == target;
            div()
                .id(id)
                .h(px(Metrics::NAV_ROW))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .border_1()
                .border_color(pingpong_ui::rgba(0.0, 0.0, 0.0, 0.0))
                .text_size(px(Type::BODY))
                .text_color(if on { t.primary } else { t.primary.alpha(0.78) })
                .when(on, |d| {
                    d.bg(t.selected)
                        .border_color(t.selected_stroke)
                        .shadow(pingpong_ui::lift(0.06))
                })
                .when(!on, |d| d.cursor_pointer().hover(|s| s.bg(t.hover)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_page(target, cx)))
                .child(icon(name, 15.0, if on { t.primary } else { t.secondary }))
                .child(div().flex_1().child(label))
                .children(trailing)
        };
        let agent_badge = if self.agents.is_running() {
            Some(pingpong_ui::live_dot(Ink::FRESH).into_any_element())
        } else if self.agents.awaiting_answer() {
            Some(dot(Ink::ATTENTION, 7.0).into_any_element())
        } else {
            None
        };
        let count = (paired > 0).then(|| {
            div()
                .text_size(px(Type::META))
                .text_color(t.tertiary)
                .child(paired.to_string())
                .into_any_element()
        });
        let header = |label: &'static str| {
            div()
                .pt(px(14.0))
                .pb(px(4.0))
                .px(px(9.0))
                .text_size(px(Type::META))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.tertiary)
                .child(label)
        };
        let footer: AnyElement = if self.model.searching {
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(spinner("sidebar-searching", 11.0, t.tertiary))
                .child("Looking for hosts…")
                .into_any_element()
        } else {
            div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .child(brand_mark(8.0))
                .child(format!("Ping {}", env!("CARGO_PKG_VERSION")))
                .into_any_element()
        };
        div()
            .flex_none()
            .w(px(Metrics::SIDEBAR))
            .h_full()
            .flex()
            .flex_col()
            .bg(t.sidebar)
            .border_r_1()
            .border_color(t.hairline)
            // The title bar's lane: the traffic lights on a Mac; elsewhere the
            // system's title bar is above, and a little air does.
            .child(div().flex_none().h(px(if cfg!(target_os = "macos") {
                Metrics::TOOLBAR
            } else {
                10.0
            })))
            .child(
                div()
                    .flex_1()
                    .px(px(8.0))
                    .pt(px(6.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(nav(
                        "nav-hosts",
                        "Hosts",
                        IconName::Devices,
                        Page::Hosts,
                        count,
                        cx,
                    ))
                    .child(nav(
                        "nav-agents",
                        "Agents",
                        IconName::Agent,
                        Page::Agents,
                        agent_badge,
                        cx,
                    ))
                    .children(self.session_rows(t, cx))
                    .child(header("Settings"))
                    .child(nav(
                        "nav-general",
                        "General",
                        IconName::Settings,
                        Page::Settings(Tab::General),
                        None,
                        cx,
                    ))
                    .child(nav(
                        "nav-video",
                        "Video",
                        IconName::Display,
                        Page::Settings(Tab::Video),
                        None,
                        cx,
                    ))
                    .child(nav(
                        "nav-audio",
                        "Audio",
                        IconName::Speaker,
                        Page::Settings(Tab::Audio),
                        None,
                        cx,
                    ))
                    .child(nav(
                        "nav-input",
                        "Input",
                        IconName::Keyboard,
                        Page::Settings(Tab::Input),
                        None,
                        cx,
                    ))
                    .child(nav(
                        "nav-agent-setup",
                        "Agent setup",
                        IconName::Sparkle,
                        Page::Settings(Tab::Agents),
                        None,
                        cx,
                    )),
            )
            .children(
                pingpong_ui::updates::notice(&super::UPDATE_APP, &self.update_status, t).map(
                    |notice| {
                        notice.on_click(cx.listener(|this, _, _, cx| this.show_updates(false, cx)))
                    },
                ),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(16.0))
                    .py(px(12.0))
                    .text_size(px(Type::META))
                    .text_color(t.tertiary)
                    .child(footer),
            )
    }

    /// The sidebar's open sessions: agents' and your desktops.
    pub(super) fn session_rows(&mut self, t: Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        use crate::chat::ChatState;
        let page = self.page;
        let mut out = Vec::new();
        let desktops: Vec<(u64, String, u64)> = self
            .streams
            .iter()
            .filter(|s| s.chat.is_none())
            .map(|s| (s.id, s.host.clone(), s.since.elapsed().as_secs()))
            .collect();
        if self.agents.chats.is_empty() && desktops.is_empty() {
            return out;
        }
        out.push(
            div()
                .pt(px(14.0))
                .pb(px(4.0))
                .px(px(9.0))
                .text_size(px(Type::META))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.tertiary)
                .child("Sessions")
                .into_any_element(),
        );
        let row = |id: ElementId,
                   on: bool,
                   glyph: IconName,
                   title: String,
                   sub: String,
                   badge: AnyElement| {
            div()
                .id(id)
                .min_h(px(40.0))
                .px(px(8.0))
                .py(px(4.0))
                .flex()
                .items_center()
                .gap(px(8.0))
                .rounded(px(Radius::ROW))
                .border_1()
                .border_color(pingpong_ui::rgba(0.0, 0.0, 0.0, 0.0))
                .when(on, |d| {
                    d.bg(t.selected)
                        .border_color(t.selected_stroke)
                        .shadow(pingpong_ui::lift(0.06))
                })
                .when(!on, |d| d.cursor_pointer().hover(|s| s.bg(t.hover)))
                .child(icon(glyph, 15.0, if on { t.primary } else { t.secondary }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(Type::BODY))
                                .text_color(if on { t.primary } else { t.primary.alpha(0.85) })
                                .child(title),
                        )
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(Type::META))
                                .text_color(t.tertiary)
                                .child(sub),
                        ),
                )
                .child(badge)
        };
        let watched: Vec<u64> = self.streams.iter().filter_map(|s| s.chat).collect();
        let chats: Vec<(u64, String, String, ChatState)> = self
            .agents
            .chats
            .iter()
            .map(|c| (c.id, c.title.clone(), c.host.clone(), c.state()))
            .collect();
        for (id, title, host, state) in chats {
            let (label, badge): (&str, AnyElement) = match state {
                ChatState::Working => (
                    "Working",
                    pingpong_ui::live_dot(Ink::FRESH).into_any_element(),
                ),
                ChatState::Waiting => (
                    "Waiting for you",
                    dot(Ink::ATTENTION, 7.0).into_any_element(),
                ),
                ChatState::Paused => ("Paused", dot(Ink::ATTENTION, 7.0).into_any_element()),
                ChatState::Connecting => (
                    "Connecting…",
                    spinner(
                        ElementId::Name(format!("side-spin-{id}").into()),
                        11.0,
                        t.tertiary,
                    )
                    .into_any_element(),
                ),
                ChatState::Connected => {
                    ("Connected", dot(t.ink(Ink::FRESH), 6.0).into_any_element())
                }
                ChatState::Idle => ("Idle", div().into_any_element()),
            };
            let sub = if watched.contains(&id) {
                format!("{host} · {label} · you're logged in")
            } else {
                format!("{host} · {label}")
            };
            out.push(
                row(
                    ElementId::Name(format!("side-chat-{id}").into()),
                    page == Page::Chat(id),
                    IconName::Agent,
                    title,
                    sub,
                    badge,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_page(Page::Chat(id), cx)))
                .into_any_element(),
            );
        }
        for (id, host, secs) in desktops {
            let sub = format!(
                "Your desktop · {}",
                if secs < 60 {
                    "now".to_string()
                } else {
                    format!("{} min", secs / 60)
                }
            );
            out.push(
                row(
                    ElementId::Name(format!("side-desk-{id}").into()),
                    page == Page::Desktop(id),
                    IconName::Display,
                    host,
                    sub,
                    pingpong_ui::live_dot(Ink::FRESH).into_any_element(),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_page(Page::Desktop(id), cx)))
                .into_any_element(),
            );
        }
        out
    }
}
