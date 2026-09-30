//! A desktop session's page: a stream of your own, while it runs.

use gpui::{div, prelude::*, px, AnyElement, Context};
use pingpong_ui::{button, IconName, Theme};

use super::{info, page_body, toolbar, PingApp};

/// Where a stream's window is, and how to get between it and Ping's.
pub(super) fn where_text(fullscreen: bool) -> String {
    match (fullscreen, cfg!(target_os = "macos")) {
        (true, true) => "Full screen, a Space of its own (swipe, or Ctrl+←/→)".into(),
        (true, false) => "Full screen, its own window (Alt+Tab)".into(),
        (false, _) => "In its own window".into(),
    }
}

impl PingApp {
    /// One of your desktop streams: what it is, and ending it.
    pub(super) fn render_desktop(
        &mut self,
        id: u64,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(s) = self.streams.iter().find(|s| s.id == id) else {
            return div().into_any_element();
        };
        let (w, h, fps) = s.mode;
        let fullscreen = s.fullscreen;
        let secs = s.since.elapsed().as_secs();
        let since = if secs < 60 {
            format!("{secs} s")
        } else if secs < 3600 {
            format!("{} min", secs / 60)
        } else {
            format!("{} h {} min", secs / 3600, secs / 60 % 60)
        };
        let host = s.host.clone();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar([
                button("desk-show", "Show Window", t)
                    .icon(IconName::Display)
                    .on_click(cx.listener(move |this, _, _, _| {
                        if let Some(s) = this.streams.iter().find(|s| s.id == id) {
                            s.session.show();
                        }
                    }))
                    .into_any_element(),
                button("desk-end", "End Session", t)
                    .danger()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        tracing::info!(id, "the user ends the desktop session");
                        this.stream_ended(id, None, window, cx);
                    }))
                    .into_any_element(),
            ]))
            .child(page_body(
                "desktop-page",
                pingpong_ui::Metrics::FORM,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(22.0))
                    .child(pingpong_ui::page_header(
                        host.clone(),
                        Some(format!("Your desktop session, for {since}.").into()),
                        None,
                        t,
                    ))
                    .child(pingpong_ui::section("", t).child(pingpong_ui::rows(
                        [
                            info("Mode", format!("{w} × {h} at {fps} FPS"), t),
                            info("Where", where_text(fullscreen), t),
                            info(
                                "To end it",
                                format!(
                                    "{} in the stream, or End Session here",
                                    crate::settings::chord("Q")
                                ),
                                t,
                            ),
                        ],
                        t,
                    ))),
            ))
            .into_any_element()
    }
}
