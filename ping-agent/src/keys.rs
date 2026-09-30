//! Key names as agents write them -- xdotool's (`ctrl+s`, `Return`,
//! `alt+Tab`, `Page_Down`), which Claude's computer tool uses, and the
//! uppercase ones OpenAI's computer tool sends (`CTRL`, `ENTER`, `ARROWLEFT`)
//! -- into the stream's keys (positional, US layout: the host maps them
//! through its own).

use pingpong_proto::input::{scancode, Key};

/// A chord: modifiers held, then one key pressed (or only modifiers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// Scancodes in the order to press them (released in reverse).
    pub keys: Vec<u16>,
}

/// Parse `ctrl+shift+t`, `Return`, `super`, `F5`, `a`, `ctrl+alt+Delete`.
/// A lone `+` names the plus key (`ctrl++` is ctrl and plus).
pub fn parse(text: &str) -> Result<Chord, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("no key given".into());
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c == '+' && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    let mut keys = Vec::new();
    let chord = parts.len() > 1;
    for p in &parts {
        // In a chord a capital letter is the letter's key (OpenAI's
        // `["CTRL", "L"]`, a model's `ctrl+S`); alone, it types a capital.
        let p = match p.as_bytes() {
            [c] if chord && c.is_ascii_uppercase() => p.to_ascii_lowercase(),
            _ => p.clone(),
        };
        let named = name(&p).ok_or_else(|| {
            format!(
                "unknown key {p:?} (use xdotool names: \
                    ctrl+s, Return, Tab, Escape, Page_Down, F5, super)"
            )
        })?;
        for k in named {
            let sc = scancode(k);
            if !keys.contains(&sc) {
                keys.push(sc);
            }
        }
    }
    Ok(Chord { keys })
}

/// One name, into the keys to hold for it (a shifted symbol is shift and
/// its key).
fn name(raw: &str) -> Option<Vec<Key>> {
    use Key::*;
    let one = |k: Key| Some(vec![k]);
    let shifted = |k: Key| Some(vec![ShiftLeft, k]);
    // Single characters: letters, digits, and the US layout's symbols.
    let mut chars = raw.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return char_key(c);
    }
    let lower = raw.to_ascii_lowercase().replace(['-', ' '], "_");
    match lower.as_str() {
        "ctrl" | "control" | "control_l" | "ctrl_l" | "lctrl" => one(ControlLeft),
        "control_r" | "ctrl_r" | "rctrl" => one(ControlRight),
        "alt" | "alt_l" | "option" | "opt" | "lalt" => one(AltLeft),
        "alt_r" | "altgr" | "iso_level3_shift" | "ralt" => one(AltRight),
        "shift" | "shift_l" | "lshift" => one(ShiftLeft),
        "shift_r" | "rshift" => one(ShiftRight),
        "super" | "super_l" | "win" | "windows" | "meta" | "meta_l" | "cmd" | "command"
        | "hyper" | "lsuper" => one(SuperLeft),
        "super_r" | "meta_r" | "rsuper" => one(SuperRight),
        "return" | "enter" | "kp_enter_main" => one(Enter),
        "kp_enter" => one(NumpadEnter),
        "tab" | "iso_left_tab" => one(Tab),
        "escape" | "esc" => one(Escape),
        "backspace" | "back_space" => one(Backspace),
        "delete" | "del" => one(Delete),
        "insert" | "ins" => one(Insert),
        "home" => one(Home),
        "end" => one(End),
        "page_up" | "pageup" | "prior" | "pgup" => one(PageUp),
        "page_down" | "pagedown" | "next" | "pgdn" => one(PageDown),
        "left" | "arrowleft" | "arrow_left" => one(ArrowLeft),
        "right" | "arrowright" | "arrow_right" => one(ArrowRight),
        "up" | "arrowup" | "arrow_up" => one(ArrowUp),
        "down" | "arrowdown" | "arrow_down" => one(ArrowDown),
        "space" | "spacebar" => one(Space),
        "caps_lock" | "capslock" => one(CapsLock),
        "num_lock" | "numlock" => one(NumLock),
        "scroll_lock" | "scrolllock" => one(ScrollLock),
        "print" | "printscreen" | "print_screen" | "sys_req" => one(PrintScreen),
        "menu" | "contextmenu" | "context_menu" | "apps" => one(ContextMenu),
        "minus" => one(Minus),
        "equal" | "equals" => one(Equal),
        "plus" => shifted(Equal),
        "bracketleft" => one(BracketLeft),
        "bracketright" => one(BracketRight),
        "braceleft" => shifted(BracketLeft),
        "braceright" => shifted(BracketRight),
        "semicolon" => one(Semicolon),
        "colon" => shifted(Semicolon),
        "apostrophe" | "quoteright" | "quote" => one(Quote),
        "quotedbl" => shifted(Quote),
        "grave" | "quoteleft" | "backquote" => one(Backquote),
        "asciitilde" | "tilde" => shifted(Backquote),
        "backslash" => one(Backslash),
        "bar" | "pipe" => shifted(Backslash),
        "comma" => one(Comma),
        "less" => shifted(Comma),
        "period" | "dot" => one(Period),
        "greater" => shifted(Period),
        "slash" => one(Slash),
        "question" => shifted(Slash),
        "exclam" => shifted(Digit1),
        "at" => shifted(Digit2),
        "numbersign" | "hash" => shifted(Digit3),
        "dollar" => shifted(Digit4),
        "percent" => shifted(Digit5),
        "asciicircum" | "caret" => shifted(Digit6),
        "ampersand" => shifted(Digit7),
        "asterisk" => shifted(Digit8),
        "parenleft" => shifted(Digit9),
        "parenright" => shifted(Digit0),
        "underscore" => shifted(Minus),
        "kp_add" => one(NumpadAdd),
        "kp_subtract" => one(NumpadSubtract),
        "kp_multiply" => one(NumpadMultiply),
        "kp_divide" => one(NumpadDivide),
        "kp_decimal" => one(NumpadDecimal),
        other => {
            if let Some(n) = other.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                return [F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12]
                    .get(n.checked_sub(1)? as usize)
                    .map(|&k| vec![k]);
            }
            if let Some(d) = other.strip_prefix("kp_").and_then(|n| n.parse::<u8>().ok()) {
                return [
                    Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7,
                    Numpad8, Numpad9,
                ]
                .get(d as usize)
                .map(|&k| vec![k]);
            }
            None
        }
    }
}

/// A character's key on a US layout (uppercase: shift and the letter).
pub fn char_key(c: char) -> Option<Vec<Key>> {
    use Key::*;
    let letters = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO,
        KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    let digits = [
        Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    ];
    if c.is_ascii_lowercase() {
        return Some(vec![letters[(c as u8 - b'a') as usize]]);
    }
    if c.is_ascii_uppercase() {
        return Some(vec![ShiftLeft, letters[(c as u8 - b'A') as usize]]);
    }
    if c.is_ascii_digit() {
        return Some(vec![digits[(c as u8 - b'0') as usize]]);
    }
    let plain = |k: Key| Some(vec![k]);
    let shift = |k: Key| Some(vec![ShiftLeft, k]);
    match c {
        ' ' => plain(Space),
        '-' => plain(Minus),
        '=' => plain(Equal),
        '[' => plain(BracketLeft),
        ']' => plain(BracketRight),
        ';' => plain(Semicolon),
        '\'' => plain(Quote),
        '`' => plain(Backquote),
        '\\' => plain(Backslash),
        ',' => plain(Comma),
        '.' => plain(Period),
        '/' => plain(Slash),
        '+' => shift(Equal),
        '_' => shift(Minus),
        '{' => shift(BracketLeft),
        '}' => shift(BracketRight),
        ':' => shift(Semicolon),
        '"' => shift(Quote),
        '~' => shift(Backquote),
        '|' => shift(Backslash),
        '<' => shift(Comma),
        '>' => shift(Period),
        '?' => shift(Slash),
        '!' => shift(Digit1),
        '@' => shift(Digit2),
        '#' => shift(Digit3),
        '$' => shift(Digit4),
        '%' => shift(Digit5),
        '^' => shift(Digit6),
        '&' => shift(Digit7),
        '*' => shift(Digit8),
        '(' => shift(Digit9),
        ')' => shift(Digit0),
        '\n' => plain(Enter),
        '\t' => plain(Tab),
        _ => None,
    }
}

/// Modifier names (`shift`, `ctrl+shift`) held around a click or scroll.
pub fn modifiers(text: Option<&str>) -> Result<Vec<u16>, String> {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        None => Ok(Vec::new()),
        Some(t) => Ok(parse(t)?.keys),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(k: Key) -> u16 {
        scancode(k)
    }

    #[test]
    fn xdotool_chords() {
        assert_eq!(
            parse("ctrl+s").unwrap().keys,
            vec![sc(Key::ControlLeft), sc(Key::KeyS)]
        );
        assert_eq!(parse("Return").unwrap().keys, vec![sc(Key::Enter)]);
        assert_eq!(
            parse("alt+Tab").unwrap().keys,
            vec![sc(Key::AltLeft), sc(Key::Tab)]
        );
        assert_eq!(
            parse("ctrl+alt+Delete").unwrap().keys,
            vec![sc(Key::ControlLeft), sc(Key::AltLeft), sc(Key::Delete)]
        );
        assert_eq!(parse("Page_Down").unwrap().keys, vec![sc(Key::PageDown)]);
        assert_eq!(parse("F5").unwrap().keys, vec![sc(Key::F5)]);
        assert_eq!(parse("super").unwrap().keys, vec![sc(Key::SuperLeft)]);
        assert_eq!(parse("KP_7").unwrap().keys, vec![sc(Key::Numpad7)]);
    }

    #[test]
    fn openai_names() {
        assert_eq!(
            parse("CTRL+L").unwrap().keys,
            vec![sc(Key::ControlLeft), sc(Key::KeyL)]
        );
        assert_eq!(
            parse("A").unwrap().keys,
            vec![sc(Key::ShiftLeft), sc(Key::KeyA)]
        );
        assert_eq!(parse("ENTER").unwrap().keys, vec![sc(Key::Enter)]);
        assert_eq!(parse("ARROWLEFT").unwrap().keys, vec![sc(Key::ArrowLeft)]);
        assert_eq!(parse("ESC").unwrap().keys, vec![sc(Key::Escape)]);
    }

    #[test]
    fn symbols_and_plus() {
        assert_eq!(
            parse("ctrl++").unwrap().keys,
            vec![sc(Key::ControlLeft), sc(Key::ShiftLeft), sc(Key::Equal)]
        );
        assert_eq!(
            parse("plus").unwrap().keys,
            vec![sc(Key::ShiftLeft), sc(Key::Equal)]
        );
        assert_eq!(
            parse("?").unwrap().keys,
            vec![sc(Key::ShiftLeft), sc(Key::Slash)]
        );
        // The shift of a symbol is not pressed twice.
        assert_eq!(
            parse("shift+question").unwrap().keys,
            vec![sc(Key::ShiftLeft), sc(Key::Slash)]
        );
    }

    #[test]
    fn unknown_names_say_so() {
        assert!(parse("hyperdrive").is_err());
        assert!(parse("").is_err());
        assert!(modifiers(None).unwrap().is_empty());
        assert_eq!(
            modifiers(Some("ctrl+shift")).unwrap(),
            vec![sc(Key::ControlLeft), sc(Key::ShiftLeft)]
        );
    }
}
