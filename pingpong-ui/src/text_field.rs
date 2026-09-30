//! A text field: one line (scrolling sideways) or several (wrapping and
//! growing). Built on GPUI's input handler, so IME composition, the
//! character palette and dictation work as in any native field.

use std::ops::Range;

use gpui::{
    actions, div, fill, point, prelude::*, px, size, App, Bounds, ClipboardItem, Context,
    CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler, EventEmitter,
    FocusHandle, Focusable, GlobalElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, Style, TextRun, UTF16Selection,
    UnderlineStyle, Window, WrappedLine,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme::{Radius, Theme, Type};

actions!(
    pingpong_text_field,
    [
        Backspace,
        BackspaceWord,
        BackspaceLine,
        Delete,
        Left,
        Right,
        Up,
        Down,
        WordLeft,
        WordRight,
        SelectLeft,
        SelectRight,
        SelectWordLeft,
        SelectWordRight,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Paste,
        Cut,
        Copy,
        Enter,
        Newline,
        Escape,
        Tab,
        TabBack,
        ShowCharacterPalette,
    ]
);

const CONTEXT: &str = "PingpongTextField";

pub fn bind_keys(cx: &mut App) {
    let cmd = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    let word = if cfg!(target_os = "macos") {
        "alt"
    } else {
        "ctrl"
    };
    let c = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, c),
        KeyBinding::new(&format!("{word}-backspace"), BackspaceWord, c),
        KeyBinding::new("delete", Delete, c),
        KeyBinding::new("left", Left, c),
        KeyBinding::new("right", Right, c),
        KeyBinding::new("up", Up, c),
        KeyBinding::new("down", Down, c),
        KeyBinding::new(&format!("{word}-left"), WordLeft, c),
        KeyBinding::new(&format!("{word}-right"), WordRight, c),
        KeyBinding::new("shift-left", SelectLeft, c),
        KeyBinding::new("shift-right", SelectRight, c),
        KeyBinding::new(&format!("{word}-shift-left"), SelectWordLeft, c),
        KeyBinding::new(&format!("{word}-shift-right"), SelectWordRight, c),
        KeyBinding::new(&format!("{cmd}-a"), SelectAll, c),
        KeyBinding::new(&format!("{cmd}-v"), Paste, c),
        KeyBinding::new(&format!("{cmd}-c"), Copy, c),
        KeyBinding::new(&format!("{cmd}-x"), Cut, c),
        KeyBinding::new("home", Home, c),
        KeyBinding::new("end", End, c),
        KeyBinding::new("shift-home", SelectHome, c),
        KeyBinding::new("shift-end", SelectEnd, c),
        KeyBinding::new("enter", Enter, c),
        KeyBinding::new("shift-enter", Newline, c),
        KeyBinding::new("escape", Escape, c),
        KeyBinding::new("tab", Tab, c),
        KeyBinding::new("shift-tab", TabBack, c),
    ]);
    if cfg!(target_os = "macos") {
        cx.bind_keys([
            KeyBinding::new("cmd-backspace", BackspaceLine, c),
            KeyBinding::new("cmd-left", Home, c),
            KeyBinding::new("cmd-right", End, c),
            KeyBinding::new("cmd-shift-left", SelectHome, c),
            KeyBinding::new("cmd-shift-right", SelectEnd, c),
            KeyBinding::new("ctrl-a", Home, c),
            KeyBinding::new("ctrl-e", End, c),
            KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, c),
        ]);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldEvent {
    Changed,
    /// Enter (in a multi-line field, Shift+Enter adds a line instead).
    Submit,
    Cancel,
}

pub struct TextField {
    focus: FocusHandle,
    text: String,
    placeholder: SharedString,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    multiline: bool,
    password: bool,
    disabled: bool,
    mono: bool,
    selecting: bool,
    /// Laid out last frame: for the mouse and the IME.
    layout: Option<Layout>,
    scroll_x: Pixels,
}

struct Layout {
    lines: Vec<WrappedLine>,
    /// Byte offset each paragraph starts at, in `shown` text.
    starts: Vec<usize>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    scroll_x: Pixels,
}

impl EventEmitter<FieldEvent> for TextField {}

impl Focusable for TextField {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TextField {
    pub fn new(cx: &mut Context<Self>) -> TextField {
        TextField {
            focus: cx.focus_handle(),
            text: String::new(),
            placeholder: SharedString::default(),
            selected: 0..0,
            reversed: false,
            marked: None,
            multiline: false,
            password: false,
            disabled: false,
            mono: false,
            selecting: false,
            layout: None,
            scroll_x: px(0.0),
        }
    }

    pub fn placeholder(mut self, p: impl Into<SharedString>) -> Self {
        self.placeholder = p.into();
        self
    }

    pub fn multiline(mut self) -> Self {
        self.multiline = true;
        self
    }

    pub fn password(mut self) -> Self {
        self.password = true;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        if !self.multiline {
            self.text = self.text.replace('\n', " ");
        }
        self.selected = self.text.len()..self.text.len();
        self.marked = None;
        cx.notify();
    }

    pub fn set_placeholder(&mut self, p: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.placeholder = p.into();
        cx.notify();
    }

    pub fn set_disabled(&mut self, disabled: bool, cx: &mut Context<Self>) {
        if self.disabled != disabled {
            self.disabled = disabled;
            cx.notify();
        }
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    /// Put the keyboard in `field`.
    pub fn focus(field: &Entity<TextField>, window: &mut Window, cx: &mut App) {
        let handle = field.read(cx).focus.clone();
        window.focus(&handle, cx);
    }

    fn cursor(&self) -> usize {
        if self.reversed {
            self.selected.start
        } else {
            self.selected.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.text.len());
        self.selected = offset..offset;
        self.reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.text.len());
        if self.reversed {
            self.selected.start = offset
        } else {
            self.selected.end = offset
        }
        if self.selected.end < self.selected.start {
            self.reversed = !self.reversed;
            self.selected = self.selected.end..self.selected.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > offset).then_some(i))
            .unwrap_or(self.text.len())
    }

    fn previous_word(&self, offset: usize) -> usize {
        let before = &self.text[..offset];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric());
        trimmed
            .rfind(|c: char| !c.is_alphanumeric())
            .map(|i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8))
            .unwrap_or(0)
    }

    fn next_word(&self, offset: usize) -> usize {
        let after = &self.text[offset..];
        let skip = after
            .find(|c: char| c.is_alphanumeric())
            .unwrap_or(after.len());
        let rest = &after[skip..];
        offset
            + skip
            + rest
                .find(|c: char| !c.is_alphanumeric())
                .unwrap_or(rest.len())
    }

    fn line_start(&self, offset: usize) -> usize {
        self.text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.text[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(self.text.len())
    }

    fn edit(
        &mut self,
        range: Range<usize>,
        new: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let new = if self.multiline {
            new.to_string()
        } else {
            new.replace('\n', " ")
        };
        self.text.replace_range(range.clone(), &new);
        let at = range.start + new.len();
        self.selected = at..at;
        self.reversed = false;
        self.marked = None;
        window.invalidate_character_coordinates();
        cx.emit(FieldEvent::Changed);
        cx.notify();
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let prev = self.previous_boundary(self.cursor());
            self.select_to(prev, cx);
        }
        self.edit(self.selected.clone(), "", window, cx);
    }

    fn backspace_word(&mut self, _: &BackspaceWord, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let prev = self.previous_word(self.cursor());
            self.select_to(prev, cx);
        }
        self.edit(self.selected.clone(), "", window, cx);
    }

    fn backspace_line(&mut self, _: &BackspaceLine, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let start = self.line_start(self.cursor());
            self.select_to(start, cx);
        }
        self.edit(self.selected.clone(), "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            let next = self.next_boundary(self.cursor());
            self.select_to(next, cx);
        }
        self.edit(self.selected.clone(), "", window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.move_to(self.previous_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selected.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            self.move_to(self.next_boundary(self.cursor()), cx);
        } else {
            self.move_to(self.selected.end, cx);
        }
    }

    fn vertical(&mut self, down: bool, cx: &mut Context<Self>) {
        let Some(l) = &self.layout else {
            self.move_to(if down { self.text.len() } else { 0 }, cx);
            return;
        };
        let Some(p) = self.position_for(self.cursor()) else {
            return;
        };
        let target = point(
            p.x,
            if down {
                p.y + l.line_height * 1.5
            } else {
                p.y - l.line_height * 0.5
            },
        );
        let index = self.index_for(target + l.bounds.origin);
        self.move_to(index, cx);
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(false, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        self.vertical(true, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_word(self.cursor()), cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word(self.cursor()), cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    fn select_word_left(&mut self, _: &SelectWordLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word(self.cursor()), cx);
    }

    fn select_word_right(&mut self, _: &SelectWordRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word(self.cursor()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected = 0..self.text.len();
        self.reversed = false;
        cx.notify();
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_start(self.cursor()), cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_end(self.cursor()), cx);
    }

    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_start(self.cursor()), cx);
    }

    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.line_end(self.cursor()), cx);
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.multiline {
                text
            } else {
                text.trim().replace(['\n', '\r'], " ")
            };
            self.edit(self.selected.clone(), &text, window, cx);
        }
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() && !self.password {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[self.selected.clone()].to_string(),
            ));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() && !self.password {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.text[self.selected.clone()].to_string(),
            ));
            self.edit(self.selected.clone(), "", window, cx);
        }
    }

    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FieldEvent::Submit);
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.edit(self.selected.clone(), "\n", window, cx);
        } else {
            cx.emit(FieldEvent::Submit);
        }
    }

    fn escape(&mut self, _: &Escape, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(FieldEvent::Cancel);
    }

    fn tab(&mut self, _: &Tab, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_next(cx);
    }

    fn tab_back(&mut self, _: &TabBack, window: &mut Window, cx: &mut Context<Self>) {
        window.focus_prev(cx);
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        window.focus(&self.focus, cx);
        self.selecting = true;
        let index = self.index_for(event.position);
        if event.click_count >= 2 {
            let start = self.previous_word(self.next_boundary(index).min(self.text.len()));
            let end = self.next_word(start);
            self.selected = start..end;
            self.reversed = false;
            cx.notify();
        } else if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.selecting {
            self.select_to(self.index_for(event.position), cx);
        }
    }

    /// What is drawn: dots for a password.
    fn shown(&self) -> String {
        if self.password {
            self.text.chars().map(|_| '•').collect()
        } else {
            self.text.clone()
        }
    }

    /// Byte offset in `text` of an offset in `shown` (they differ for a password).
    fn text_offset(&self, shown_offset: usize) -> usize {
        if !self.password {
            return shown_offset.min(self.text.len());
        }
        let n = shown_offset / "•".len();
        self.text
            .char_indices()
            .nth(n)
            .map(|(i, _)| i)
            .unwrap_or(self.text.len())
    }

    fn shown_offset(&self, text_offset: usize) -> usize {
        if !self.password {
            return text_offset;
        }
        self.text[..text_offset.min(self.text.len())]
            .chars()
            .count()
            * "•".len()
    }

    /// Where `offset` (in `text`) is drawn, relative to the field's origin.
    fn position_for(&self, offset: usize) -> Option<Point<Pixels>> {
        let l = self.layout.as_ref()?;
        let shown = self.shown_offset(offset);
        let mut y = px(0.0);
        for (i, line) in l.lines.iter().enumerate() {
            let start = l.starts[i];
            let end = start + line.len();
            if shown <= end {
                let p = line.position_for_index(shown - start, l.line_height)?;
                return Some(point(p.x - l.scroll_x, y + p.y));
            }
            y += l.line_height * (line.wrap_boundaries().len() + 1) as f32;
        }
        None
    }

    fn index_for(&self, position: Point<Pixels>) -> usize {
        let Some(l) = &self.layout else { return 0 };
        let local = position - l.bounds.origin;
        let mut y = px(0.0);
        for (i, line) in l.lines.iter().enumerate() {
            let h = l.line_height * (line.wrap_boundaries().len() + 1) as f32;
            if local.y < y + h || i + 1 == l.lines.len() {
                let p = point(
                    local.x + l.scroll_x,
                    (local.y - y).max(px(0.0)).min(h - px(1.0)),
                );
                let ix = match line.closest_index_for_position(p, l.line_height) {
                    Ok(ix) | Err(ix) => ix,
                };
                return self.text_offset(l.starts[i] + ix);
            }
            y += h;
        }
        self.text.len()
    }
}

impl EntityInputHandler for TextField {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = from_utf16(&self.text, &range);
        actual.replace(to_utf16(&self.text, &range));
        Some(self.text[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: to_utf16(&self.text, &self.selected),
            reversed: self.reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| to_utf16(&self.text, r))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| from_utf16(&self.text, &r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        self.edit(range, new, window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new: &str,
        new_selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }
        let range = range
            .map(|r| from_utf16(&self.text, &r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        self.text.replace_range(range.clone(), new);
        self.marked = (!new.is_empty()).then(|| range.start..range.start + new.len());
        self.selected = new_selected
            .map(|r| from_utf16(new, &r))
            .map(|r| range.start + r.start..range.start + r.end)
            .unwrap_or(range.start + new.len()..range.start + new.len());
        cx.emit(FieldEvent::Changed);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = from_utf16(&self.text, &range);
        let l = self.layout.as_ref()?;
        let a = self.position_for(range.start)?;
        let b = self.position_for(range.end)?;
        Some(Bounds::from_corners(
            bounds.origin + a,
            bounds.origin + point(b.x, b.y + l.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        p: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self.index_for(p);
        Some(self.text[..index].encode_utf16().count())
    }
}

fn from_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    let conv = |units: usize| {
        let mut count = 0;
        for (i, c) in text.char_indices() {
            if count >= units {
                return i;
            }
            count += c.len_utf16();
        }
        text.len()
    };
    conv(range.start)..conv(range.end)
}

fn to_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    text[..range.start].encode_utf16().count()..text[..range.end].encode_utf16().count()
}

/// Draws the field's text, selection and caret.
struct FieldElement {
    field: Entity<TextField>,
    theme: Theme,
    size: f32,
}

struct Prepaint {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    line_height: Pixels,
    caret: Option<Bounds<Pixels>>,
    selections: Vec<Bounds<Pixels>>,
    placeholder: bool,
}

impl IntoElement for FieldElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

fn shape(
    text: &str,
    size: Pixels,
    color: gpui::Hsla,
    marked: Option<&Range<usize>>,
    wrap: Option<Pixels>,
    window: &Window,
) -> Vec<WrappedLine> {
    let style = window.text_style();
    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = match marked {
        Some(m) if m.end <= text.len() => vec![
            TextRun {
                len: m.start,
                ..run.clone()
            },
            TextRun {
                len: m.end - m.start,
                underline: Some(UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..run.clone()
            },
            TextRun {
                len: text.len() - m.end,
                ..run
            },
        ]
        .into_iter()
        .filter(|r| r.len > 0)
        .collect(),
        _ => vec![run],
    };
    window
        .text_system()
        .shape_text(
            SharedString::from(text.to_string()),
            size,
            &runs,
            wrap,
            None,
        )
        .map(|l| l.into_vec())
        .unwrap_or_default()
}

impl Element for FieldElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let field = self.field.read(cx);
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        let line_height = window.line_height();
        if !field.multiline {
            style.size.height = line_height.into();
            return (window.request_layout(style, [], cx), ());
        }
        let text = if field.text.is_empty() {
            field.placeholder.to_string()
        } else {
            field.shown()
        };
        let size = px(self.size);
        let layout = window.request_measured_layout(style, move |known, available, window, _cx| {
            let width = known.width.or(match available.width {
                gpui::AvailableSpace::Definite(w) => Some(w),
                _ => None,
            });
            let lines = shape(&text, size, gpui::black(), None, width, window);
            let rows: usize = lines
                .iter()
                .map(|l| l.wrap_boundaries().len() + 1)
                .sum::<usize>()
                .max(1);
            gpui::size(width.unwrap_or(px(100.0)), line_height * rows as f32)
        });
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let t = self.theme;
        let field = self.field.read(cx);
        let placeholder = field.text.is_empty();
        let line_height = window.line_height();
        let (text, color) = if placeholder {
            (field.placeholder.to_string(), t.quaternary)
        } else {
            (field.shown(), t.primary)
        };
        let color = if field.disabled && !placeholder {
            t.tertiary
        } else {
            color
        };
        let marked = field
            .marked
            .as_ref()
            .map(|m| field.shown_offset(m.start)..field.shown_offset(m.end));
        let wrap = field.multiline.then_some(bounds.size.width);
        let lines = shape(
            &text,
            px(self.size),
            color.into(),
            marked.as_ref(),
            wrap,
            window,
        );
        let mut starts = Vec::with_capacity(lines.len());
        let mut at = 0;
        for l in &lines {
            starts.push(at);
            at += l.len() + 1;
        }
        // One line scrolls to keep the caret in view.
        let mut scroll_x = field.scroll_x;
        let cursor = if placeholder {
            0
        } else {
            field.shown_offset(field.cursor())
        };
        if !field.multiline {
            if let Some(line) = lines.first() {
                let x = line
                    .position_for_index(cursor.min(line.len()), line_height)
                    .map(|p| p.x)
                    .unwrap_or_default();
                if x - scroll_x > bounds.size.width - px(2.0) {
                    scroll_x = x - bounds.size.width + px(2.0);
                } else if x < scroll_x {
                    scroll_x = x;
                }
                scroll_x = scroll_x.max(px(0.0));
            }
        }
        let pos = |offset: usize| -> Option<Point<Pixels>> {
            let mut y = px(0.0);
            for (i, line) in lines.iter().enumerate() {
                let end = starts[i] + line.len();
                if offset <= end {
                    let p = line.position_for_index(offset - starts[i], line_height)?;
                    return Some(point(p.x - scroll_x, y + p.y));
                }
                y += line_height * (line.wrap_boundaries().len() + 1) as f32;
            }
            None
        };
        let focused = field.focus.is_focused(window) && !field.disabled;
        let mut selections = Vec::new();
        let mut caret = None;
        if focused && !placeholder && !field.selected.is_empty() {
            let (a, b) = (
                field.shown_offset(field.selected.start),
                field.shown_offset(field.selected.end),
            );
            if let (Some(pa), Some(pb)) = (pos(a), pos(b)) {
                if pa.y == pb.y {
                    selections.push(Bounds::from_corners(
                        bounds.origin + pa,
                        bounds.origin + point(pb.x, pb.y + line_height),
                    ));
                } else {
                    // First row to the edge, full rows between, last row from the start.
                    selections.push(Bounds::from_corners(
                        bounds.origin + pa,
                        bounds.origin + point(bounds.size.width, pa.y + line_height),
                    ));
                    let mut y = pa.y + line_height;
                    while y < pb.y {
                        selections.push(Bounds::from_corners(
                            bounds.origin + point(px(0.0), y),
                            bounds.origin + point(bounds.size.width, y + line_height),
                        ));
                        y += line_height;
                    }
                    selections.push(Bounds::from_corners(
                        bounds.origin + point(px(0.0), pb.y),
                        bounds.origin + point(pb.x, pb.y + line_height),
                    ));
                }
            }
        } else if focused {
            if let Some(p) = pos(cursor) {
                caret = Some(Bounds::new(
                    bounds.origin + point(p.x, p.y + px(2.0)),
                    size(px(1.5), line_height - px(4.0)),
                ));
            }
        }
        self.field.update(cx, |f, _| f.scroll_x = scroll_x);
        Prepaint {
            lines,
            starts,
            line_height,
            caret,
            selections,
            placeholder,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let t = self.theme;
        let focus = self.field.read(cx).focus.clone();
        let scroll_x = self.field.read(cx).scroll_x;
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.field.clone()),
            cx,
        );
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for s in prepaint.selections.drain(..) {
                window.paint_quad(fill(s, t.focus.alpha(0.28)));
            }
            let mut y = px(0.0);
            for line in &prepaint.lines {
                let _ = line.paint(
                    bounds.origin + point(-scroll_x, y),
                    prepaint.line_height,
                    gpui::TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                y += prepaint.line_height * (line.wrap_boundaries().len() + 1) as f32;
            }
            if let Some(c) = prepaint.caret.take() {
                window.paint_quad(fill(c, t.focus.alpha(1.0)));
            }
        });
        let lines = std::mem::take(&mut prepaint.lines);
        let starts = std::mem::take(&mut prepaint.starts);
        let line_height = prepaint.line_height;
        let placeholder = prepaint.placeholder;
        self.field.update(cx, |f, _| {
            f.layout = (!placeholder).then_some(Layout {
                lines,
                starts,
                bounds,
                line_height,
                scroll_x,
            });
        });
    }
}

/// Draw a text field: `Entity<TextField>` plus how it looks here.
#[derive(IntoElement)]
pub struct Field {
    field: Entity<TextField>,
    theme: Theme,
    chrome: bool,
    min_rows: usize,
}

pub fn field(field: &Entity<TextField>, theme: Theme) -> Field {
    Field {
        field: field.clone(),
        theme,
        chrome: true,
        min_rows: 1,
    }
}

impl Field {
    /// No box of its own: the field sits inside a composer.
    pub fn bare(mut self) -> Self {
        self.chrome = false;
        self
    }

    pub fn min_rows(mut self, rows: usize) -> Self {
        self.min_rows = rows;
        self
    }
}

impl RenderOnce for Field {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = self.theme;
        let f = self.field.read(cx);
        let focused = f.focus.is_focused(window);
        let disabled = f.disabled;
        let multiline = f.multiline;
        let text_size = Type::BODY;
        let line_height = 19.0;
        let entity = self.field.clone();
        div()
            .id(ElementId::Name(
                format!("field-{}", self.field.entity_id()).into(),
            ))
            .key_context(CONTEXT)
            .track_focus(&f.focus)
            .when(!disabled, |d| d.cursor(CursorStyle::IBeam))
            .on_action(window.listener_for(&entity, TextField::backspace))
            .on_action(window.listener_for(&entity, TextField::backspace_word))
            .on_action(window.listener_for(&entity, TextField::backspace_line))
            .on_action(window.listener_for(&entity, TextField::delete))
            .on_action(window.listener_for(&entity, TextField::left))
            .on_action(window.listener_for(&entity, TextField::right))
            .on_action(window.listener_for(&entity, TextField::up))
            .on_action(window.listener_for(&entity, TextField::down))
            .on_action(window.listener_for(&entity, TextField::word_left))
            .on_action(window.listener_for(&entity, TextField::word_right))
            .on_action(window.listener_for(&entity, TextField::select_left))
            .on_action(window.listener_for(&entity, TextField::select_right))
            .on_action(window.listener_for(&entity, TextField::select_word_left))
            .on_action(window.listener_for(&entity, TextField::select_word_right))
            .on_action(window.listener_for(&entity, TextField::select_all))
            .on_action(window.listener_for(&entity, TextField::home))
            .on_action(window.listener_for(&entity, TextField::end))
            .on_action(window.listener_for(&entity, TextField::select_home))
            .on_action(window.listener_for(&entity, TextField::select_end))
            .on_action(window.listener_for(&entity, TextField::paste))
            .on_action(window.listener_for(&entity, TextField::copy))
            .on_action(window.listener_for(&entity, TextField::cut))
            .on_action(window.listener_for(&entity, TextField::enter))
            .on_action(window.listener_for(&entity, TextField::newline))
            .on_action(window.listener_for(&entity, TextField::escape))
            .on_action(window.listener_for(&entity, TextField::tab))
            .on_action(window.listener_for(&entity, TextField::tab_back))
            .on_action(window.listener_for(&entity, TextField::show_character_palette))
            .on_mouse_down(
                MouseButton::Left,
                window.listener_for(&entity, TextField::on_mouse_down),
            )
            .on_mouse_up(
                MouseButton::Left,
                window.listener_for(&entity, TextField::on_mouse_up),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                window.listener_for(&entity, TextField::on_mouse_up),
            )
            .on_mouse_move(window.listener_for(&entity, TextField::on_mouse_move))
            .w_full()
            .text_size(px(text_size))
            .line_height(px(line_height))
            .when(f.mono, |d| d.font_family(crate::mono_font()))
            .when(self.chrome, |d| {
                d.px(px(9.0))
                    .py(px(if multiline { 7.0 } else { 3.5 }))
                    .rounded(px(Radius::CONTROL))
                    .bg(t.field)
                    .border_1()
                    .border_color(if focused { t.focus } else { t.field_stroke })
            })
            .when(disabled, |d| d.opacity(0.6))
            .child(
                div()
                    .w_full()
                    .when(multiline, |d| {
                        d.min_h(px(line_height * self.min_rows as f32))
                    })
                    .overflow_hidden()
                    .child(FieldElement {
                        field: self.field,
                        theme: t,
                        size: text_size,
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_ranges_round_trip_through_emoji() {
        let text = "a😀b";
        assert_eq!(from_utf16(text, &(1..3)), 1..5);
        assert_eq!(to_utf16(text, &(1..5)), 1..3);
    }
}
