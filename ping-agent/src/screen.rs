//! The screen as text, for a model to read beside the screenshot: what the
//! host's accessibility tree says is in the front window (see
//! `pingpong_proto::screen`), one element a line, indented as they nest,
//! each with the point to click it.
//!
//! The labels are what the apps call their controls, exactly: a model that
//! reads "button "Send" at (1180, 92)" need not make the label out from
//! pixels. What an app does not publish (a game, a canvas, some Electron
//! apps) is not here, and nothing stands in for it: the screenshot does.

use pingpong_proto::screen::{flags, ScreenText, Status};

/// Nesting shown at most: deeper elements are indented no further.
const MAX_INDENT: usize = 6;

/// `text` as the lines a model reads.
pub fn describe(text: &ScreenText) -> String {
    if text.status == Status::Unavailable {
        return format!(
            "The host cannot read its screen as text: {} Use the screenshot.",
            text.note
        );
    }
    let mut out = String::new();
    match (text.app.is_empty(), text.window.is_empty()) {
        (false, false) => out.push_str(&format!("{} — \"{}\"\n", text.app, text.window)),
        (false, true) => out.push_str(&format!("{}\n", text.app)),
        (true, false) => out.push_str(&format!("\"{}\"\n", text.window)),
        (true, true) => {}
    }
    if text.elements.is_empty() {
        out.push_str(
            "The app publishes nothing in its accessibility tree here (a game, a canvas, or \
                an app without one): use the screenshot.",
        );
        return out;
    }
    for e in &text.elements {
        let indent = "  ".repeat((e.depth as usize).min(MAX_INDENT));
        let (x, y) = e.center();
        out.push_str(&indent);
        out.push_str(e.role.name());
        if !e.label.is_empty() {
            out.push_str(&format!(" \"{}\"", e.label.replace('\n', " ")));
        }
        out.push_str(&format!(" at ({x}, {y})"));
        let states: Vec<&str> = [
            (flags::FOCUSED, "focused"),
            (flags::DISABLED, "disabled"),
            (flags::ON, "on"),
            (flags::SECRET, "password"),
        ]
        .iter()
        .filter(|(f, _)| e.flags & f != 0)
        .map(|(_, s)| *s)
        .collect();
        if !states.is_empty() {
            out.push_str(&format!(" [{}]", states.join(", ")));
        }
        out.push('\n');
    }
    if text.truncated || !text.note.is_empty() {
        let note = if text.note.is_empty() {
            "There is more than this list holds."
        } else {
            &text.note
        };
        out.push_str(&format!("({note})\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_proto::screen::{Element, Role};

    #[test]
    fn elements_read_as_nested_lines_with_their_points() {
        let text = ScreenText {
            app: "Mail".into(),
            window: "New Message".into(),
            elements: vec![
                Element {
                    role: Role::Toolbar,
                    flags: 0,
                    label: "Compose".into(),
                    x: 0,
                    y: 0,
                    w: 1280,
                    h: 40,
                    depth: 0,
                },
                Element {
                    role: Role::Button,
                    flags: flags::DISABLED,
                    label: "Send".into(),
                    x: 1160,
                    y: 8,
                    w: 40,
                    h: 24,
                    depth: 1,
                },
                Element {
                    role: Role::TextField,
                    flags: flags::FOCUSED,
                    label: "To:".into(),
                    x: 100,
                    y: 60,
                    w: 600,
                    h: 20,
                    depth: 1,
                },
            ],
            ..ScreenText::default()
        };
        assert_eq!(
            describe(&text),
            "Mail — \"New Message\"\n\
             toolbar \"Compose\" at (640, 20)\n  \
             button \"Send\" at (1180, 20) [disabled]\n  \
             text field \"To:\" at (400, 70) [focused]\n"
        );
    }

    #[test]
    fn nothing_published_and_unreadable_say_to_use_the_screenshot() {
        let empty = ScreenText {
            app: "Game".into(),
            ..ScreenText::default()
        };
        assert!(describe(&empty).contains("use the screenshot"));
        let off = ScreenText::unavailable("The screen is locked.");
        assert!(describe(&off).contains("The screen is locked."));
    }
}
