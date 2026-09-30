//! The host's log, following its end.

use gpui::{div, prelude::*, px, AnyElement, ClipboardItem, Context};
use pingpong_ui::{button, IconName, Ink, Radius, Theme};

use super::PongApp;

impl PongApp {
    pub(super) fn logs(&mut self, t: Theme, _cx: &mut Context<Self>) -> AnyElement {
        let text = self.snap.logs.clone().unwrap_or_default();
        let copy = text.clone();
        let lines: Vec<AnyElement> = text
            .lines()
            .map(|l| {
                // "2026-09-29T07:23:36.084844Z  INFO pong::host: …": the time
                // of day, the level, the rest.
                let mut parts = l.splitn(2, ' ');
                let stamp = parts.next().unwrap_or_default();
                let rest = parts.next().unwrap_or_default().trim_start();
                let (level, message) = rest.split_once(' ').unwrap_or(("", rest));
                let time = stamp
                    .get(11..19)
                    .filter(|_| stamp.len() > 20 && stamp.as_bytes()[10] == b'T');
                let Some(time) = time else {
                    return div()
                        .text_color(t.primary.alpha(0.78))
                        .child(l.to_string())
                        .into_any_element();
                };
                let tint = match level {
                    "ERROR" => t.ink(Ink::DANGER),
                    "WARN" => t.ink(Ink::ATTENTION),
                    _ => t.tertiary,
                };
                div()
                    .flex()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex_none()
                            .text_color(t.tertiary)
                            .child(time.to_string()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(40.0))
                            .text_color(tint)
                            .child(level.to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_color(t.primary.alpha(0.82))
                            .child(message.trim_start().to_string()),
                    )
                    .into_any_element()
            })
            .collect();
        let empty = lines.is_empty();
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(pingpong_ui::toolbar_row([button("copy-logs", "Copy", t)
                .icon(IconName::Copy)
                .disabled(empty)
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                })
                .into_any_element()]))
            .child(
                div()
                    .flex_none()
                    .px(px(28.0))
                    .pb(px(14.0))
                    .child(pingpong_ui::page_header(
                        "Logs",
                        Some("The last 400 lines of pong.log.".into()),
                        None,
                        t,
                    )),
            )
            .child(
                div().flex_1().min_h_0().px(px(28.0)).pb(px(24.0)).child(
                    div()
                        .id("logs")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.logs_scroll)
                        .rounded(px(Radius::CARD))
                        .border_1()
                        .border_color(t.card_stroke)
                        .bg(t.card)
                        .p(px(12.0))
                        .font_family(pingpong_ui::mono_font())
                        .text_size(px(11.0))
                        .line_height(px(16.0))
                        .when(empty, |d| {
                            d.child(div().text_color(t.tertiary).child("Reading the log…"))
                        })
                        .children(lines),
                ),
            )
            .into_any_element()
    }
}
