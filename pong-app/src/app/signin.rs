//! Before the host can be shown: Pong not running, or signing in (Windows).
//! Pages like the others: a title at the top, rows under it.

use gpui::{div, prelude::*, px, AnyElement, Context, Window};
use pingpong_ui::{
    button, field, notice, page_header, rows, section, setting, spinner, IconName, Ink, TextField,
    Theme, Type,
};

use crate::host;
use crate::worker::Cmd;

use super::{page, PongApp};

impl PongApp {
    pub(super) fn not_running(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let can_start = host::can_start();
        let how = match std::env::consts::OS {
            "windows" => {
                "Pong runs as a service, PongService. Start it in Services, or \
                    install it with pong install from an administrator's terminal."
            }
            "macos" if can_start => {
                "Pong then runs whenever you are logged in, and asks for Screen \
                    Recording and Accessibility the first time."
            }
            "macos" => {
                "Put Pong.app beside this app (both in Applications) and start it \
                    here, or run tools/build-pong-app --install from the source."
            }
            _ => "Start it with systemctl --user start pong, or run pong host.",
        };
        let start: AnyElement = if can_start {
            button("start-host", "Start Pong", t)
                .solid()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.start_error = host::start().err();
                    if let Some(e) = &this.start_error {
                        tracing::warn!(error = e, "the host was not started");
                    }
                    this.send(Cmd::Refresh);
                    cx.notify();
                }))
                .into_any_element()
        } else {
            div().into_any_element()
        };
        let waiting = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(spinner("waiting-host", 11.0, t.tertiary))
            .child(
                button("retry", "Try Again", t).on_click(cx.listener(|this, _, _, cx| {
                    this.send(Cmd::Refresh);
                    cx.notify();
                })),
            );
        let content = div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(page_header(
                "Pong isn't running",
                Some("This window connects as soon as it starts.".into()),
                None,
                t,
            ))
            .children(
                self.start_error
                    .clone()
                    .map(|e| notice(IconName::Warning, Ink::DANGER, e, t)),
            )
            .child(
                section("", t).child(rows(
                    [
                        setting(
                            if can_start {
                                "Start it here"
                            } else {
                                "Start it"
                            },
                            Some(how.into()),
                            start,
                            t,
                        )
                        .into_any_element(),
                        setting(
                            "Waiting for it",
                            Some("Looking for the host on this computer.".into()),
                            waiting,
                            t,
                        )
                        .into_any_element(),
                    ],
                    t,
                )),
            );
        page(t, [], "not-running-page", content)
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
        let content = div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(page_header(title, Some(detail.into()), None, t))
            .child(
                section("", t).child(rows(
                    [
                        setting(
                            "User name",
                            None,
                            div().w(px(240.0)).child(field(&self.user, t)),
                            t,
                        )
                        .into_any_element(),
                        setting(
                            "Password",
                            None,
                            div().w(px(240.0)).child(field(&self.password, t)),
                            t,
                        )
                        .into_any_element(),
                    ],
                    t,
                )),
            )
            .children(
                self.sign_in_error
                    .clone()
                    .map(|e| notice(IconName::Warning, Ink::DANGER, e, t)),
            )
            .child(
                div().flex().justify_end().child(
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
                    .disabled(!ok)
                    .on_click(cx.listener(|this, _, _, cx| this.sign_in(cx))),
                ),
            );
        page(t, [], "sign-in-page", content)
    }
}

/// While something is on its way (the connection, the host's first
/// answer): what, with a spinner, where a page's title goes.
pub(super) fn waiting(id: &'static str, what: &'static str, t: Theme) -> AnyElement {
    page(
        t,
        [],
        id,
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(Type::BODY))
            .text_color(t.tertiary)
            .child(spinner("waiting-spin", 13.0, t.tertiary))
            .child(what),
    )
}
