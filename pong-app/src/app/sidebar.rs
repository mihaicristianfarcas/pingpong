//! The sidebar: the pages, and the host's name and state at its foot.

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight};
use pingpong_ui::{dot, icon, icon_button, live_dot, IconName, Ink, Metrics, Radius, Theme, Type};

use crate::worker::Conn;

use super::{Confirm, Page, PongApp};

impl PongApp {
    pub(super) fn sidebar(&mut self, t: Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let page = self.page;
        let ready = self.snap.conn == Conn::Ready;
        let pending = self
            .snap
            .status
            .as_ref()
            .map(|s| s.pending.len())
            .unwrap_or(0);
        let streaming = self
            .snap
            .status
            .as_ref()
            .and_then(|s| s.session.as_ref())
            .is_some();
        let nav = |id: &'static str,
                   label: &'static str,
                   name: IconName,
                   target: Page,
                   trailing: Option<AnyElement>,
                   cx: &mut Context<Self>| {
            let on = page == target && ready;
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
                .when(!ready, |d| d.opacity(0.45))
                .when(!on && ready, |d| {
                    d.cursor_pointer()
                        .hover(|s| s.bg(t.hover))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_page(target, cx)))
                })
                .child(icon(name, 15.0, if on { t.primary } else { t.secondary }))
                .child(div().flex_1().child(label))
                .children(trailing)
        };
        let overview_badge = streaming.then(|| live_dot(Ink::FRESH).into_any_element());
        let devices_badge = (pending > 0).then(|| {
            div()
                .px(px(6.0))
                .rounded(px(8.0))
                .bg(Ink::ACCENT)
                .text_size(px(Type::META))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(pingpong_ui::rgba(1.0, 1.0, 1.0, 1.0))
                .child(pending.to_string())
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
        let (dot_color, state) = match &self.snap.conn {
            Conn::Ready if streaming => (Ink::FRESH, "Streaming"),
            Conn::Ready => (Ink::FRESH, "Running"),
            Conn::Connecting => (Ink::IDLE, "Connecting…"),
            Conn::NotRunning => (Ink::DANGER, "Not running"),
            Conn::SignIn { .. } => (Ink::ATTENTION, "Signed out"),
        };
        let host = if self.snap.state.name.is_empty() {
            "This computer".to_string()
        } else {
            self.snap.state.name.clone()
        };
        let can_sign_out = ready && !self.snap.local;
        div()
            .flex_none()
            .w(px(Metrics::SIDEBAR))
            .h_full()
            .flex()
            .flex_col()
            .bg(t.sidebar)
            .border_r_1()
            .border_color(t.hairline)
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
                        "nav-overview",
                        "Overview",
                        IconName::Activity,
                        Page::Overview,
                        overview_badge,
                        cx,
                    ))
                    .child(nav(
                        "nav-devices",
                        "Devices",
                        IconName::Devices,
                        Page::Devices,
                        devices_badge,
                        cx,
                    ))
                    .child(header("Settings"))
                    .child(nav(
                        "nav-general",
                        "General",
                        IconName::Settings,
                        Page::General,
                        None,
                        cx,
                    ))
                    .child(nav(
                        "nav-video",
                        "Video",
                        IconName::Display,
                        Page::Video,
                        None,
                        cx,
                    ))
                    .child(nav(
                        "nav-network",
                        "Network",
                        IconName::Globe,
                        Page::Network,
                        None,
                        cx,
                    ))
                    .child(nav(
                        "nav-agents",
                        "AI agents",
                        IconName::Agent,
                        Page::Agents,
                        None,
                        cx,
                    ))
                    .child(div().h(px(10.0)))
                    .child(nav(
                        "nav-logs",
                        "Logs",
                        IconName::List,
                        Page::Logs,
                        None,
                        cx,
                    )),
            )
            .children(
                pingpong_ui::updates::notice(&super::UPDATE_APP, &self.updates.status(), t).map(
                    |notice| notice.on_click(cx.listener(|this, _, _, cx| this.show_updates(cx))),
                ),
            )
            .child(
                div()
                    .flex_none()
                    .mx(px(8.0))
                    .mb(px(8.0))
                    .px(px(8.0))
                    .py(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .rounded(px(Radius::ROW))
                    .child(
                        div()
                            .flex_none()
                            .size(px(28.0))
                            .rounded(px(Radius::ROW))
                            .bg(t.primary.alpha(0.06))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(
                                if self.os() == "macos" {
                                    IconName::Laptop
                                } else {
                                    IconName::Monitor
                                },
                                15.0,
                                t.secondary,
                            )),
                    )
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
                                    .text_size(px(12.5))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(host),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(5.0))
                                    .text_size(px(Type::META))
                                    .text_color(t.tertiary)
                                    .child(dot(dot_color, 6.0))
                                    .child(state),
                            ),
                    )
                    .when(can_sign_out, |d| {
                        d.child(
                            icon_button("sign-out", IconName::Power, "Sign out of Pong here", t)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.confirm = Some(Confirm::SignOut);
                                    cx.notify();
                                })),
                        )
                    }),
            )
    }
}
