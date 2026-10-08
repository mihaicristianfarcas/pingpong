//! Markdown, as models write it, drawn with the theme: paragraphs,
//! headings, emphasis, inline code and code blocks, lists (nested, numbered,
//! tasks), block quotes, links (opened in the browser), rules and tables.

use std::ops::Range;

use gpui::{
    div, prelude::*, px, AnyElement, App, ElementId, FontStyle, FontWeight, HighlightStyle,
    InteractiveText, SharedString, StrikethroughStyle, StyledText, UnderlineStyle, Window,
};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use crate::theme::{Radius, Theme, Type};

#[derive(Default, Clone, Copy, PartialEq)]
struct Marks {
    bold: bool,
    italic: bool,
    code: bool,
    strike: bool,
    link: bool,
}

/// Text with its marked ranges.
#[derive(Default, Clone)]
struct Inline {
    text: String,
    runs: Vec<(Range<usize>, Marks)>,
    links: Vec<(Range<usize>, String)>,
}

impl Inline {
    fn push(&mut self, s: &str, marks: Marks) {
        let start = self.text.len();
        self.text.push_str(s);
        if marks != Marks::default() {
            self.runs.push((start..self.text.len(), marks));
        }
    }

    fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }
}

enum Block {
    Paragraph(Inline),
    Heading(u8, Inline),
    Code(String),
    List {
        start: Option<u64>,
        items: Vec<Vec<Block>>,
    },
    Quote(Vec<Block>),
    Rule,
    Table(Vec<Vec<Inline>>),
}

/// Parse `source` into blocks.
fn parse(source: &str) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS);
    let events: Vec<Event> = Parser::new_ext(source, opts).collect();
    let mut pos = 0;
    parse_blocks(&events, &mut pos, None)
}

/// Blocks until the closing tag `until` (or the end).
fn parse_blocks(events: &[Event], pos: &mut usize, until: Option<TagEnd>) -> Vec<Block> {
    let mut out = Vec::new();
    // Loose text in a tight list item is a paragraph of its own.
    let mut loose = Inline::default();
    let flush = |loose: &mut Inline, out: &mut Vec<Block>| {
        if !loose.is_empty() {
            out.push(Block::Paragraph(std::mem::take(loose)));
        } else {
            *loose = Inline::default();
        }
    };
    while *pos < events.len() {
        let ev = events[*pos].clone();
        *pos += 1;
        match ev {
            Event::End(end) if Some(end) == until => {
                flush(&mut loose, &mut out);
                return out;
            }
            Event::Start(Tag::Paragraph) => {
                flush(&mut loose, &mut out);
                out.push(Block::Paragraph(parse_inline(
                    events,
                    pos,
                    TagEnd::Paragraph,
                )));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush(&mut loose, &mut out);
                let n = match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    HeadingLevel::H3 => 3,
                    _ => 4,
                };
                out.push(Block::Heading(
                    n,
                    parse_inline(events, pos, TagEnd::Heading(level)),
                ));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut loose, &mut out);
                let mut code = String::new();
                while *pos < events.len() {
                    let e = events[*pos].clone();
                    *pos += 1;
                    match e {
                        Event::Text(t) => code.push_str(&t),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                }
                let _ = matches!(kind, CodeBlockKind::Fenced(_));
                out.push(Block::Code(code.trim_end_matches('\n').to_string()));
            }
            Event::Start(Tag::List(start)) => {
                flush(&mut loose, &mut out);
                let mut items = Vec::new();
                while *pos < events.len() {
                    match events[*pos].clone() {
                        Event::Start(Tag::Item) => {
                            *pos += 1;
                            items.push(parse_blocks(events, pos, Some(TagEnd::Item)));
                        }
                        Event::End(TagEnd::List(_)) => {
                            *pos += 1;
                            break;
                        }
                        _ => *pos += 1,
                    }
                }
                out.push(Block::List { start, items });
            }
            Event::Start(Tag::BlockQuote(_)) => {
                flush(&mut loose, &mut out);
                out.push(Block::Quote(parse_blocks(
                    events,
                    pos,
                    Some(TagEnd::BlockQuote(None)),
                )));
            }
            Event::Start(Tag::Table(_)) => {
                flush(&mut loose, &mut out);
                let mut rows: Vec<Vec<Inline>> = Vec::new();
                while *pos < events.len() {
                    match events[*pos].clone() {
                        Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                            *pos += 1;
                            rows.push(Vec::new());
                        }
                        Event::Start(Tag::TableCell) => {
                            *pos += 1;
                            let cell = parse_inline(events, pos, TagEnd::TableCell);
                            if let Some(r) = rows.last_mut() {
                                r.push(cell);
                            }
                        }
                        Event::End(TagEnd::Table) => {
                            *pos += 1;
                            break;
                        }
                        _ => *pos += 1,
                    }
                }
                out.push(Block::Table(rows));
            }
            Event::Rule => {
                flush(&mut loose, &mut out);
                out.push(Block::Rule);
            }
            Event::TaskListMarker(done) => {
                loose.push(if done { "☑ " } else { "☐ " }, Marks::default())
            }
            // Inline content outside a paragraph (a tight list item).
            Event::Text(_)
            | Event::Code(_)
            | Event::Start(Tag::Emphasis)
            | Event::Start(Tag::Strong)
            | Event::Start(Tag::Link { .. })
            | Event::SoftBreak
            | Event::HardBreak => {
                *pos -= 1;
                let mut inline = parse_inline_until_block(events, pos);
                if loose.text.is_empty() {
                    loose = inline;
                } else {
                    let shift = loose.text.len();
                    loose.text.push_str(&inline.text);
                    loose.runs.extend(
                        inline
                            .runs
                            .drain(..)
                            .map(|(r, m)| (r.start + shift..r.end + shift, m)),
                    );
                    loose.links.extend(
                        inline
                            .links
                            .drain(..)
                            .map(|(r, u)| (r.start + shift..r.end + shift, u)),
                    );
                }
            }
            _ => {}
        }
    }
    flush(&mut loose, &mut out);
    out
}

/// Inline events up to `end`.
fn parse_inline(events: &[Event], pos: &mut usize, end: TagEnd) -> Inline {
    let mut inline = Inline::default();
    let mut marks = Marks::default();
    let mut link: Option<(usize, String)> = None;
    while *pos < events.len() {
        let ev = events[*pos].clone();
        *pos += 1;
        if matches!(&ev, Event::End(e) if *e == end) {
            break;
        }
        inline_event(&mut inline, &mut marks, &mut link, ev);
    }
    inline
}

/// Inline events up to the next block start or end.
fn parse_inline_until_block(events: &[Event], pos: &mut usize) -> Inline {
    let mut inline = Inline::default();
    let mut marks = Marks::default();
    let mut link: Option<(usize, String)> = None;
    while *pos < events.len() {
        let ev = events[*pos].clone();
        let inline_kind = matches!(
            ev,
            Event::Text(_)
                | Event::Code(_)
                | Event::SoftBreak
                | Event::HardBreak
                | Event::Start(Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. })
                | Event::End(
                    TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link
                )
        );
        if !inline_kind {
            break;
        }
        *pos += 1;
        inline_event(&mut inline, &mut marks, &mut link, ev);
    }
    inline
}

fn inline_event(
    inline: &mut Inline,
    marks: &mut Marks,
    link: &mut Option<(usize, String)>,
    ev: Event,
) {
    match ev {
        Event::Text(t) => inline.push(&t, *marks),
        Event::Code(t) => inline.push(
            &t,
            Marks {
                code: true,
                ..*marks
            },
        ),
        Event::SoftBreak => inline.push(" ", *marks),
        Event::HardBreak => inline.push("\n", *marks),
        Event::Start(Tag::Emphasis) => marks.italic = true,
        Event::End(TagEnd::Emphasis) => marks.italic = false,
        Event::Start(Tag::Strong) => marks.bold = true,
        Event::End(TagEnd::Strong) => marks.bold = false,
        Event::Start(Tag::Strikethrough) => marks.strike = true,
        Event::End(TagEnd::Strikethrough) => marks.strike = false,
        Event::Start(Tag::Link { dest_url, .. }) => {
            marks.link = true;
            *link = Some((inline.text.len(), dest_url.to_string()));
        }
        Event::End(TagEnd::Link) => {
            marks.link = false;
            if let Some((start, url)) = link.take() {
                inline.links.push((start..inline.text.len(), url));
            }
        }
        _ => {}
    }
}

/// Draw `source` as Markdown.
pub fn markdown(id: impl Into<ElementId>, source: &str, t: Theme) -> AnyElement {
    let id: ElementId = id.into();
    let blocks = parse(source);
    let mut n = 0usize;
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .children(blocks.iter().map(|b| render_block(&id, &mut n, b, t, 0)))
        .into_any_element()
}

fn render_block(
    id: &ElementId,
    n: &mut usize,
    block: &Block,
    t: Theme,
    depth: usize,
) -> AnyElement {
    *n += 1;
    let key = ElementId::Name(format!("{id:?}-{n}").into());
    match block {
        Block::Paragraph(inline) => {
            render_inline(key, inline, t, Type::BODY, FontWeight::NORMAL).into_any_element()
        }
        Block::Heading(level, inline) => {
            let size = match level {
                1 => 17.0,
                2 => 15.0,
                _ => Type::BODY + 0.5,
            };
            div()
                .pt(px(if *level <= 2 { 4.0 } else { 2.0 }))
                .child(render_inline(key, inline, t, size, FontWeight::SEMIBOLD))
                .into_any_element()
        }
        Block::Code(code) => div()
            .w_full()
            .px(px(10.0))
            .py(px(8.0))
            .rounded(px(Radius::ROW))
            .bg(t.primary.alpha(if t.dark { 0.05 } else { 0.045 }))
            .border_1()
            .border_color(t.card_stroke)
            .font_family(crate::mono_font())
            .text_size(px(12.0))
            .line_height(px(17.0))
            .text_color(t.primary.alpha(0.9))
            .child(code.clone())
            .into_any_element(),
        Block::List { start, items } => div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(items.iter().enumerate().map(|(i, item)| {
                let marker = match start {
                    Some(s) => format!("{}.", s + i as u64),
                    None => if depth.is_multiple_of(2) {
                        "•"
                    } else {
                        "◦"
                    }
                    .to_string(),
                };
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .min_w(px(if start.is_some() { 18.0 } else { 10.0 }))
                            .text_size(px(Type::BODY))
                            .line_height(px(20.0))
                            .text_color(t.tertiary)
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .children(item.iter().map(|b| render_block(id, n, b, t, depth + 1))),
                    )
            }))
            .into_any_element(),
        Block::Quote(blocks) => div()
            .pl(px(12.0))
            .border_l_2()
            .border_color(t.primary.alpha(0.15))
            .text_color(t.secondary)
            .flex()
            .flex_col()
            .gap(px(6.0))
            .children(blocks.iter().map(|b| render_block(id, n, b, t, depth)))
            .into_any_element(),
        Block::Rule => div()
            .my(px(4.0))
            .h(px(1.0))
            .w_full()
            .bg(t.hairline)
            .into_any_element(),
        Block::Table(rows) => div()
            .rounded(px(Radius::ROW))
            .border_1()
            .border_color(t.card_stroke)
            .overflow_hidden()
            .flex()
            .flex_col()
            .children(rows.iter().enumerate().map(|(r, row)| {
                div()
                    .flex()
                    .when(r > 0, |d| d.border_t_1().border_color(t.hairline))
                    .when(r == 0, |d| d.bg(t.primary.alpha(0.04)))
                    .children(row.iter().enumerate().map(|(c, cell)| {
                        *n += 1;
                        div()
                            .flex_1()
                            .min_w_0()
                            .px(px(10.0))
                            .py(px(6.0))
                            .when(c > 0, |d| d.border_l_1().border_color(t.hairline))
                            .child(render_inline(
                                ElementId::Name(format!("{id:?}-{n}").into()),
                                cell,
                                t,
                                12.5,
                                if r == 0 {
                                    FontWeight::SEMIBOLD
                                } else {
                                    FontWeight::NORMAL
                                },
                            ))
                    }))
            }))
            .into_any_element(),
    }
}

fn render_inline(
    key: ElementId,
    inline: &Inline,
    t: Theme,
    size: f32,
    weight: FontWeight,
) -> impl IntoElement {
    let text: SharedString = inline.text.clone().into();
    let highlights: Vec<(Range<usize>, HighlightStyle)> = inline
        .runs
        .iter()
        .map(|(r, m)| {
            let mut h = HighlightStyle::default();
            if m.bold {
                h.font_weight = Some(FontWeight::SEMIBOLD);
            }
            if m.italic {
                h.font_style = Some(FontStyle::Italic);
            }
            if m.strike {
                h.strikethrough = Some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: None,
                });
            }
            if m.code {
                h.background_color = Some(t.primary.alpha(if t.dark { 0.07 } else { 0.06 }).into());
            }
            if m.link {
                h.color = Some(t.accent.into());
                h.underline = Some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(t.accent.alpha(0.45).into()),
                    wavy: false,
                });
            }
            (r.clone(), h)
        })
        .collect();
    let mono: Vec<(Range<usize>, SharedString)> = inline
        .runs
        .iter()
        .filter(|(_, m)| m.code)
        .map(|(r, _)| (r.clone(), crate::mono_font()))
        .collect();
    let styled = StyledText::new(text)
        .with_highlights(highlights)
        .with_font_family_overrides(mono);
    let urls: Vec<String> = inline.links.iter().map(|(_, u)| u.clone()).collect();
    let ranges: Vec<Range<usize>> = inline.links.iter().map(|(r, _)| r.clone()).collect();
    let body = InteractiveText::new(key, styled).on_click(
        ranges,
        move |i, _: &mut Window, cx: &mut App| {
            if let Some(u) = urls.get(i) {
                if u.starts_with("http://") || u.starts_with("https://") {
                    cx.open_url(u);
                }
            }
        },
    );
    div()
        .text_size(px(size))
        .line_height(px(size * 1.5))
        .font_weight(weight)
        .text_color(t.primary.alpha(0.92))
        .child(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_and_marks_come_out_of_markdown() {
        let b = parse(
            "# Title\n\nSome **bold** and `code`, a [link](https://x.y).\n\n- \
                one\n- two\n  1. deep\n\n```\nfn a() {}\n```\n\n> quoted\n\n| a | b \
                |\n|---|---|\n| 1 | 2 |\n",
        );
        assert!(matches!(b[0], Block::Heading(1, _)));
        let Block::Paragraph(p) = &b[1] else {
            panic!("a paragraph")
        };
        assert_eq!(p.text, "Some bold and code, a link.");
        assert!(p
            .runs
            .iter()
            .any(|(r, m)| m.bold && &p.text[r.clone()] == "bold"));
        assert!(p
            .runs
            .iter()
            .any(|(r, m)| m.code && &p.text[r.clone()] == "code"));
        assert_eq!(p.links[0].1, "https://x.y");
        let Block::List { items, start: None } = &b[2] else {
            panic!("a list")
        };
        assert_eq!(items.len(), 2);
        assert!(items[1]
            .iter()
            .any(|b| matches!(b, Block::List { start: Some(1), .. })));
        assert!(matches!(&b[3], Block::Code(c) if c == "fn a() {}"));
        assert!(matches!(b[4], Block::Quote(_)));
        assert!(matches!(&b[5], Block::Table(rows) if rows.len() == 2 && rows[1][1].text == "2"));
    }
}
