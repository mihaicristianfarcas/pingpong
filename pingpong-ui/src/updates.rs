//! The update check's face, the same in Ping's window and Pong's: a quiet
//! row at the sidebar's foot when something newer exists, the sheet that
//! says what and installs it (also what "Check for Updates" opens), and the
//! setting that chooses what to be told about. The check and the installer
//! are `pingpong-update`'s.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui::{
    div, prelude::*, px, App, ClipboardItem, ElementId, FontWeight, SharedString, Stateful, Window,
};
use pingpong_update::{Build, Channel, Install, Program, Status, Update};

use crate::controls::{button, select, setting, spinner, Choice};
use crate::icon::{icon, IconName};
use crate::theme::{Ink, Radius, Theme, Type};

/// Which app asks, and which build of it this is.
#[derive(Debug, Clone, Copy)]
pub struct UpdateApp {
    pub program: Program,
    pub build: Build,
}

impl UpdateApp {
    fn name(&self) -> &'static str {
        self.program.name()
    }

    /// Whether this copy installs updates itself.
    fn installs(&self) -> bool {
        pingpong_update::install::available(self.program, &self.build).is_ok()
    }
}

/// What installing an update interrupts, said before it starts.
fn interrupts(program: Program) -> &'static str {
    match program {
        Program::Ping => "A stream that is open ends.",
        Program::Pong if cfg!(windows) => {
            "Windows asks for an administrator's permission, and streams to this PC stop \
                while Pong restarts."
        }
        Program::Pong => "Streams to this computer stop while the host restarts.",
    }
}

/// The row at the sidebar's foot: "Ping 0.7.0 is available". `None` while
/// there is nothing newer. The caller makes a click open the sheet.
pub fn notice(app: &UpdateApp, status: &Status, t: Theme) -> Option<Stateful<gpui::Div>> {
    let update = status.update.as_ref()?;
    Some(
        div()
            .id("update-notice")
            .mx(px(8.0))
            .px(px(8.0))
            .py(px(6.0))
            .flex()
            .items_center()
            .gap(px(7.0))
            .rounded(px(Radius::ROW))
            .bg(Ink::ACCENT.alpha(if t.dark { 0.12 } else { 0.10 }))
            .border_1()
            .border_color(Ink::ACCENT.alpha(0.30))
            .text_size(px(Type::META + 0.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(t.primary.alpha(0.9))
            .cursor_pointer()
            .hover(|s| s.bg(Ink::ACCENT.alpha(0.18)))
            .child(icon(IconName::Download, 13.0, t.ink(Ink::ACCENT)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(update.headline(app.name())),
            ),
    )
}

/// "just now", "3 hours ago", "2 days ago".
fn ago(unix: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(unix);
    let plural = |n: u64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match secs {
        0..=89 => "just now".into(),
        90..=2699 => plural((secs + 30) / 60, "minute"),
        2700..=129_599 => plural((secs + 1800) / 3600, "hour"),
        _ => plural((secs + 43_200) / 86_400, "day"),
    }
}

/// An error as a sentence: a capital first, a full stop last.
fn sentence(text: &str) -> String {
    let mut chars = text.trim().chars();
    let mut out: String = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => return String::new(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

/// What the sheet says: a title, a line or two under it.
struct Said {
    title: String,
    text: String,
}

fn said(app: &UpdateApp, status: &Status) -> Said {
    if let Some(Install::Failed(why)) = &status.install {
        return Said {
            title: match &status.update {
                Some(Update::Release { version, .. }) => {
                    format!("{} {version} was not installed", app.name())
                }
                _ => "The update was not installed".into(),
            },
            text: sentence(why),
        };
    }
    match (&status.update, &status.error) {
        (Some(update), _) => Said {
            title: update.headline(app.name()),
            text: match update {
                Update::Release { .. } if app.installs() => format!(
                    "This is {} {}. {} {}",
                    app.name(),
                    app.build.describe(),
                    update.how(app.program, &app.build),
                    interrupts(app.program)
                ),
                _ => format!(
                    "This is {} {}. {}",
                    app.name(),
                    app.build.describe(),
                    update.how(app.program, &app.build)
                ),
            },
        },
        (None, Some(error)) => Said {
            title: "Couldn't check for updates".into(),
            text: format!("{error}. Check the connection and try again."),
        },
        (None, None) if status.checked_unix == 0 => Said {
            title: "Not checked yet".into(),
            text: format!(
                "This is {} {}. GitHub has not been asked whether there is a newer one.",
                app.name(),
                app.build.describe()
            ),
        },
        (None, None) => Said {
            title: format!("{} is up to date", app.name()),
            text: format!(
                "{} {} is the newest there is. Checked {}.",
                app.name(),
                app.build.describe(),
                ago(status.checked_unix)
            ),
        },
    }
}

/// A sheet's title, as Ping's and Pong's own sheets set theirs.
fn sheet_title(text: impl Into<SharedString>, t: Theme) -> gpui::Div {
    div()
        .text_size(px(17.0))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.primary)
        .child(text.into())
}

/// "12.3 MB".
fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1_000_000.0)
}

/// An installation under way: what it is doing, and how far the download
/// is.
fn installing(app: &UpdateApp, status: &Status, install: &Install, t: Theme) -> gpui::Div {
    let version = match &status.update {
        Some(Update::Release { version, .. }) => format!(" {version}"),
        _ => String::new(),
    };
    let line = |text: String| {
        div()
            .flex()
            .items_center()
            .gap(px(7.0))
            .text_size(px(Type::BODY))
            .text_color(t.secondary)
            .child(spinner("update-installing", 12.0, t.tertiary))
            .child(text)
    };
    let body = div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .child(sheet_title(format!("Updating {}{version}", app.name()), t));
    match install {
        Install::Starting => body.child(line("Asking GitHub for it…".into())),
        Install::Downloading { done, total } => {
            let fraction = if *total == 0 {
                0.0
            } else {
                (*done as f32 / *total as f32).clamp(0.0, 1.0)
            };
            body.child(line(format!(
                "Downloading… {} of {}",
                megabytes(*done),
                megabytes(*total)
            )))
            .child(
                div()
                    .w_full()
                    .h(px(4.0))
                    .rounded(px(2.0))
                    .bg(t.primary.alpha(0.08))
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(fraction))
                            .rounded(px(2.0))
                            .bg(t.ink(Ink::ACCENT)),
                    ),
            )
        }
        Install::Installing => body.child(line("Checking it and installing…".into())),
        Install::Restarting | Install::Failed(_) => {
            body.child(line(format!("{} is restarting…", app.name())))
        }
    }
}

/// The sheet's content: what the check found and what to do about it.
/// `on_check` asks GitHub again, `on_install` installs the release,
/// `on_close` puts the sheet away.
pub fn sheet_body(
    app: &UpdateApp,
    status: &Status,
    t: Theme,
    on_check: impl Fn(&mut Window, &mut App) + 'static,
    on_install: impl Fn(&mut Window, &mut App) + 'static,
    on_close: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Div {
    if let Some(install) = status.install.as_ref().filter(|i| i.busy()) {
        return installing(app, status, install, t);
    }
    let body = div().flex().flex_col().gap(px(10.0));
    if status.checking {
        return body.child(sheet_title("Checking for updates", t)).child(
            div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .text_size(px(Type::BODY))
                .text_color(t.secondary)
                .child(spinner("update-checking", 12.0, t.tertiary))
                .child("Asking GitHub…"),
        );
    }
    let said = said(app, status);
    let update = status.update.clone();
    let failed = matches!(status.install, Some(Install::Failed(_)));
    let installs = failed || app.installs() && matches!(update, Some(Update::Release { .. }));
    // A copy Homebrew installed that cannot install itself here: the
    // command, to copy.
    let command = update
        .as_ref()
        .filter(|u| {
            !installs
                && matches!(u, Update::Release { .. })
                && pingpong_update::installed_by_homebrew(app.program.cask())
        })
        .map(|_| format!("brew upgrade --cask {}", app.program.cask()));
    let mut actions = div().pt(px(8.0)).flex().justify_end().gap(px(8.0));
    match &update {
        Some(update) if installs => {
            let url = update.url().to_string();
            actions = actions
                .child(
                    button("update-close", if failed { "OK" } else { "Later" }, t)
                        .on_click(move |_, window, cx| on_close(window, cx)),
                )
                .child(
                    button("update-open", update.link_label(), t)
                        .icon(IconName::ExternalLink)
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
                .child(
                    button(
                        "update-install",
                        if failed {
                            "Try Again"
                        } else {
                            "Install and Restart"
                        },
                        t,
                    )
                    .icon(IconName::Download)
                    .solid()
                    .on_click(move |_, window, cx| on_install(window, cx)),
                );
        }
        Some(update) => {
            let url = update.url().to_string();
            actions = actions
                .child(
                    button("update-close", "Later", t)
                        .on_click(move |_, window, cx| on_close(window, cx)),
                )
                .child(
                    button("update-open", update.link_label(), t)
                        .icon(IconName::ExternalLink)
                        .solid()
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                );
        }
        None => {
            actions = actions
                .child(
                    button("update-again", "Check Again", t)
                        .icon(IconName::Refresh)
                        .on_click(move |_, window, cx| on_check(window, cx)),
                )
                .child(
                    button("update-close", "OK", t)
                        .solid()
                        .on_click(move |_, window, cx| on_close(window, cx)),
                );
        }
    }
    body.child(sheet_title(said.title, t))
        .child(
            div()
                .text_size(px(Type::BODY))
                .line_height(px(18.0))
                .text_color(t.secondary)
                .child(said.text),
        )
        .when_some(command, |d, command| {
            let copy = command.clone();
            d.child(
                div()
                    .w_full()
                    .mt(px(2.0))
                    .pl(px(10.0))
                    .pr(px(4.0))
                    .py(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(Radius::CONTROL))
                    .bg(t.primary.alpha(0.05))
                    .border_1()
                    .border_color(t.card_stroke)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_family(crate::mono_font())
                            .text_size(px(12.0))
                            .child(command),
                    )
                    .child(
                        crate::controls::icon_button(
                            "update-copy",
                            IconName::Copy,
                            "Copy the command",
                            t,
                        )
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))
                        }),
                    ),
            )
        })
        .child(actions)
}

/// The setting that chooses what to be told about. A build with no commit
/// to compare (a packaged release's source, an archive) is not offered
/// `main`.
pub fn channel_setting(
    id: impl Into<ElementId>,
    app: &UpdateApp,
    channel: Channel,
    t: Theme,
    on_change: impl Fn(Channel, &mut Window, &mut App) + 'static,
) -> gpui::Div {
    let mut channels = vec![Channel::Off, Channel::Releases];
    let mut choices = vec![Choice::new("Off"), Choice::new("New releases")];
    if app.build.follows_main() {
        channels.push(Channel::Main);
        choices.push(
            Choice::new("Releases and main")
                .detail(format!("this build: {}", app.build.short_commit())),
        );
    }
    let selected = channels
        .iter()
        .position(|c| *c == channel.for_build(&app.build));
    let detail: SharedString = format!(
        "{} asks GitHub once a day whether there is a newer one, and says so at the \
            foot of the sidebar. An update is installed only when you choose to.",
        app.name()
    )
    .into();
    setting(
        "Look for updates",
        Some(detail),
        select(id, choices, selected, t)
            .width(190.0)
            .on_select(move |i, window, cx| on_change(channels[i], window, cx)),
        t,
    )
}

/// "Ping 0.6.0 (4f87575)" and when GitHub was last asked, with a button to
/// ask now.
pub fn version_setting(
    app: &UpdateApp,
    status: &Status,
    t: Theme,
    on_check: impl Fn(&mut Window, &mut App) + 'static,
) -> gpui::Div {
    let detail: SharedString = if status.install.as_ref().is_some_and(Install::busy) {
        "Installing an update…".into()
    } else if status.checking {
        "Asking GitHub…".into()
    } else if let Some(update) = &status.update {
        update.headline(app.name()).into()
    } else if let Some(error) = &status.error {
        format!("The last check failed: {error}.").into()
    } else if status.checked_unix == 0 {
        "Not checked for updates yet.".into()
    } else {
        format!("Up to date. Checked {}.", ago(status.checked_unix)).into()
    };
    setting(
        format!("{} {}", app.name(), app.build.describe()),
        Some(detail),
        button("update-check-now", "Check for Updates…", t)
            .disabled(status.checking)
            .on_click(move |_, window, cx| on_check(window, cx)),
        t,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn times_are_said_the_way_people_say_them() {
        assert_eq!(ago(now()), "just now");
        assert_eq!(ago(now() - 120), "2 minutes ago");
        assert_eq!(ago(now() - 3600), "1 hour ago");
        assert_eq!(ago(now() - 2 * 3600), "2 hours ago");
        assert_eq!(ago(now() - 3600 * 23), "23 hours ago");
        assert_eq!(ago(now() - 86_400 * 2), "2 days ago");
        assert_eq!(ago(now() - 86_400 * 36 / 24), "2 days ago");
    }

    #[test]
    fn the_sheet_says_what_was_found_or_why_nothing_was() {
        let app = UpdateApp {
            program: Program::Ping,
            build: Build {
                version: "0.6.0",
                commit: "",
                release: true,
            },
        };
        let mut status = Status::default();
        assert_eq!(said(&app, &status).title, "Not checked yet");
        status.checked_unix = now();
        assert_eq!(said(&app, &status).title, "Ping is up to date");
        status.error = Some("no route to host".into());
        let failed = said(&app, &status);
        assert_eq!(failed.title, "Couldn't check for updates");
        assert!(failed.text.starts_with("no route to host."));
        // A known update is said even when the latest check failed.
        status.update = Some(Update::Release {
            version: "0.7.0".into(),
            url: String::new(),
        });
        assert_eq!(said(&app, &status).title, "Ping 0.7.0 is available");
        // An installation that did not take says so first, and why.
        status.install = Some(Install::Failed(
            "the download broke off: connection reset".into(),
        ));
        let failed = said(&app, &status);
        assert_eq!(failed.title, "Ping 0.7.0 was not installed");
        assert_eq!(failed.text, "The download broke off: connection reset.");
    }
}
