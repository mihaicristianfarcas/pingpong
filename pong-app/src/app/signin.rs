//! Before the host can be shown: Pong not running, or signing in (Windows).

use gpui::{div, prelude::*, px, AnyElement, Context, FontWeight, Window};
use pingpong_ui::{button, field, notice, spinner, IconName, Ink, TextField, Theme, Type};

use crate::host;
use crate::worker::Cmd;

use super::{centered, mark, PongApp};

impl PongApp {
    pub(super) fn not_running(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let can_start = host::can_start();
        let how = match std::env::consts::OS {
            "windows" => {
                "Pong runs as a service, PongService. Start it in Services, or \
                    install it with pong install from an administrator's terminal."
            }
            "macos" if can_start => {
                "Start it here: Pong then runs whenever you are logged in, and \
                    asks for Screen Recording and Accessibility the first time."
            }
            "macos" => {
                "Put Pong.app beside this app (both in Applications) and start it \
                    here, or run tools/build-pong-app --install from the source."
            }
            _ => "Start it with systemctl --user start pong, or run pong host.",
        };
        centered(
            div()
                .max_w(px(420.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.0))
                .child(mark(IconName::Power, t))
                .child(
                    div()
                        .pt(px(6.0))
                        .text_size(px(Type::TITLE + 2.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Pong isn't running"),
                )
                .child(
                    div()
                        .text_center()
                        .text_size(px(Type::BODY))
                        .line_height(px(19.0))
                        .text_color(t.tertiary)
                        .child(how),
                )
                .child(
                    div()
                        .pt(px(6.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(Type::META))
                        .text_color(t.tertiary)
                        .child(spinner("waiting-host", 11.0, t.tertiary))
                        .child("This window connects as soon as it starts."),
                )
                .children(
                    self.start_error
                        .clone()
                        .map(|e| notice(IconName::Warning, Ink::DANGER, e, t)),
                )
                .child(
                    div()
                        .pt(px(8.0))
                        .flex()
                        .gap(px(8.0))
                        .child(button("retry", "Try Again", t).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.send(Cmd::Refresh);
                                cx.notify();
                            },
                        )))
                        .when(can_start, |d| {
                            d.child(button("start-host", "Start Pong", t).solid().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.start_error = host::start().err();
                                    if let Some(e) = &this.start_error {
                                        tracing::warn!(error = e, "the host was not started");
                                    }
                                    this.send(Cmd::Refresh);
                                    cx.notify();
                                }),
                            ))
                        }),
                ),
        )
    }

    pub(super) fn sign_in_page(
        &mut self,
        setup: bool,
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let host = if self.snap.state.name.is_empty() {
            "this computer".to_string()
        } else {
            self.snap.state.name.clone()
        };
        let (title, detail, action) = if setup {
            (
                "Create Pong's admin account".to_string(),
                format!(
                    "It guards pairing and settings on {host}, here and in Pong's web \
                        UI. The password needs 8 characters or more."
                ),
                "Create Account",
            )
        } else {
            (
                format!("Sign in to Pong on {host}"),
                "Use the account you made for Pong's web UI. This window stays signed in."
                    .to_string(),
                "Sign In",
            )
        };
        if !self.user.read(cx).is_focused(window)
            && !self.password.read(cx).is_focused(window)
            && self.user.read(cx).text().is_empty()
        {
            TextField::focus(&self.user, window, cx);
        }
        let ok = !self.user.read(cx).text().trim().is_empty()
            && !self.password.read(cx).text().is_empty()
            && !self.signing_in;
        centered(
            div()
                .w(px(340.0))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(div().flex().justify_center().child(mark(IconName::Lock, t)))
                .child(
                    div()
                        .pt(px(6.0))
                        .text_center()
                        .text_size(px(Type::TITLE + 2.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(
                    div()
                        .text_center()
                        .text_size(px(Type::BODY))
                        .line_height(px(19.0))
                        .text_color(t.tertiary)
                        .child(detail),
                )
                .child(
                    div()
                        .pt(px(6.0))
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .child(field(&self.user, t))
                        .child(field(&self.password, t)),
                )
                .children(
                    self.sign_in_error
                        .clone()
                        .map(|e| notice(IconName::Warning, Ink::DANGER, e, t)),
                )
                .child(
                    button(
                        "sign-in",
                        if self.signing_in {
                            "Signing in…"
                        } else {
                            action
                        },
                        t,
                    )
                    .solid()
                    .large()
                    .full_width()
                    .disabled(!ok)
                    .on_click(cx.listener(|this, _, _, cx| this.sign_in(cx))),
                ),
        )
    }
}
