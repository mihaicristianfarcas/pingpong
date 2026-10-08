//! The controls both apps are built from. Each takes the [`Theme`] it paints
//! with, so a view decides its appearance once per frame.

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    anchored, deferred, div, percentage, point, prelude::*, px, svg, Animation, AnimationExt,
    AnyElement, AnyView, App, BoxShadow, ClickEvent, Div, ElementId, FontWeight, Rgba,
    SharedString, Stateful, Transformation, Window,
};

use crate::icon::{icon, IconName, IconSize};
use crate::theme::{rgba, Ink, Layer, Metrics, Radius, Theme, Type};

type OnClick = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
/// What a control calls with its new value.
type Handler<T> = Rc<dyn Fn(T, &mut Window, &mut App)>;

// ---------------------------------------------------------------------------
// Buttons

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ButtonKind {
    /// A bordered, quiet button: most actions.
    #[default]
    Quiet,
    /// Filled with the text colour: the one main action on a surface.
    Solid,
    /// Red text on a red wash: removing, stopping.
    Danger,
    /// No chrome until hovered: toolbars and inline actions.
    Ghost,
}

#[derive(IntoElement)]
pub struct Button {
    id: ElementId,
    label: Option<SharedString>,
    icon: Option<IconName>,
    kind: ButtonKind,
    theme: Theme,
    disabled: bool,
    large: bool,
    full_width: bool,
    tooltip: Option<SharedString>,
    on_click: Option<OnClick>,
}

pub fn button(id: impl Into<ElementId>, label: impl Into<SharedString>, theme: Theme) -> Button {
    Button {
        id: id.into(),
        label: Some(label.into()),
        icon: None,
        kind: ButtonKind::Quiet,
        theme,
        disabled: false,
        large: false,
        full_width: false,
        tooltip: None,
        on_click: None,
    }
}

/// A square toolbar button with only an icon (and a tooltip).
pub fn icon_button(
    id: impl Into<ElementId>,
    name: IconName,
    tooltip: impl Into<SharedString>,
    theme: Theme,
) -> Button {
    Button {
        id: id.into(),
        label: None,
        icon: Some(name),
        kind: ButtonKind::Ghost,
        theme,
        disabled: false,
        large: false,
        full_width: false,
        tooltip: Some(tooltip.into()),
        on_click: None,
    }
}

impl Button {
    pub fn kind(mut self, kind: ButtonKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn solid(self) -> Self {
        self.kind(ButtonKind::Solid)
    }

    pub fn danger(self) -> Self {
        self.kind(ButtonKind::Danger)
    }

    pub fn ghost(self) -> Self {
        self.kind(ButtonKind::Ghost)
    }

    pub fn icon(mut self, name: IconName) -> Self {
        self.icon = Some(name);
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// 32 pt tall: the main action of a sheet or an empty state.
    pub fn large(mut self) -> Self {
        self.large = true;
        self
    }

    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }

    pub fn tooltip(mut self, text: impl Into<SharedString>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    pub fn on_click(mut self, f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Button {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let t = self.theme;
        let height = if self.large { 32.0 } else { Metrics::CONTROL };
        let icon_only = self.label.is_none();
        let (fill, hover, stroke, text) = match self.kind {
            ButtonKind::Quiet => (
                t.control,
                t.control_hover,
                Some(t.control_stroke),
                t.primary,
            ),
            ButtonKind::Solid => (t.solid(), t.solid().opacity(0.86), None, t.on_solid()),
            ButtonKind::Danger => (
                Ink::DANGER.alpha(0.14),
                Ink::DANGER.alpha(0.22),
                None,
                t.ink(Ink::DANGER),
            ),
            ButtonKind::Ghost => (rgba(0.0, 0.0, 0.0, 0.0), t.hover, None, t.secondary),
        };
        let disabled = self.disabled;
        let text = if disabled { text.opacity(0.45) } else { text };
        let fill = if disabled && self.kind == ButtonKind::Solid {
            t.primary.alpha(0.18)
        } else {
            fill
        };
        let text = if disabled && self.kind == ButtonKind::Solid {
            t.tertiary
        } else {
            text
        };
        let icon_size = if icon_only { IconSize::REGULAR } else { 14.0 };
        let on_click = self.on_click.clone();
        let mut el = div()
            .id(self.id)
            .flex_none()
            .h(px(height))
            .when(icon_only, |d| d.w(px(height)).justify_center())
            .when(!icon_only, |d| {
                d.px(px(if self.large { 14.0 } else { 10.0 }))
            })
            .when(self.full_width, |d| d.w_full().justify_center())
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(if self.large {
                Radius::ROW
            } else {
                Radius::CONTROL
            }))
            .bg(fill)
            .when_some(stroke, |d, s| d.border_1().border_color(s))
            .text_size(px(if self.large { Type::BODY } else { 12.0 }))
            .font_weight(if self.kind == ButtonKind::Solid {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::MEDIUM
            })
            .text_color(text)
            .whitespace_nowrap()
            .when_some(self.icon, |d, name| d.child(icon(name, icon_size, text)))
            .when_some(self.label, |d, l| d.child(l));
        if !disabled {
            el = el
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .active(move |s| s.opacity(0.82))
                .when_some(on_click, |d, f| d.on_click(move |e, w, cx| f(e, w, cx)));
        }
        if let Some(tip) = self.tooltip {
            el = el.tooltip(move |_, cx| tooltip(tip.clone(), cx));
        }
        el
    }
}

// ---------------------------------------------------------------------------
// Tooltips

pub struct Tooltip {
    text: SharedString,
}

impl Render for Tooltip {
    fn render(&mut self, window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let t = Theme::of(window);
        div()
            .font_family(crate::ui_font())
            .max_w(px(300.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(Radius::CONTROL))
            .bg(t.floating)
            .border_1()
            .border_color(t.floating_stroke)
            .shadow(lift(0.25))
            .text_size(px(Type::META + 0.5))
            .text_color(t.secondary)
            .child(self.text.clone())
    }
}

pub fn tooltip(text: impl Into<SharedString>, cx: &mut App) -> AnyView {
    let text = text.into();
    cx.new(|_| Tooltip { text }).into()
}

// ---------------------------------------------------------------------------
// Switch

#[derive(IntoElement)]
pub struct Switch {
    id: ElementId,
    on: bool,
    disabled: bool,
    theme: Theme,
    on_toggle: Option<Handler<bool>>,
}

pub fn switch(id: impl Into<ElementId>, on: bool, theme: Theme) -> Switch {
    Switch {
        id: id.into(),
        on,
        disabled: false,
        theme,
        on_toggle: None,
    }
}

impl Switch {
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_toggle(mut self, f: impl Fn(bool, &mut Window, &mut App) + 'static) -> Self {
        self.on_toggle = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Switch {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let t = self.theme;
        let on = self.on;
        let track = if on {
            Ink::FRESH.alpha(if t.dark { 0.72 } else { 0.85 })
        } else {
            t.primary.alpha(0.14)
        };
        let knob = if t.dark {
            t.primary
        } else {
            rgba(1.0, 1.0, 1.0, 1.0)
        };
        let toggle = self.on_toggle.clone();
        div()
            .id(self.id)
            .flex_none()
            .w(px(30.0))
            .h(px(18.0))
            .p(px(2.0))
            .rounded(px(9.0))
            .bg(track)
            .flex()
            .when(on, |d| d.justify_end())
            .when(self.disabled, |d| d.opacity(0.4))
            .child(
                div()
                    .size(px(14.0))
                    .rounded(px(7.0))
                    .bg(knob)
                    .shadow(lift(0.18)),
            )
            .when(!self.disabled, |d| {
                d.cursor_pointer()
                    .when_some(toggle, |d, f| d.on_click(move |_, w, cx| f(!on, w, cx)))
            })
    }
}

// ---------------------------------------------------------------------------
// Segmented control

#[derive(IntoElement)]
pub struct Segmented {
    id: ElementId,
    items: Vec<SharedString>,
    selected: usize,
    theme: Theme,
    on_select: Option<Handler<usize>>,
}

pub fn segmented(
    id: impl Into<ElementId>,
    items: impl IntoIterator<Item = impl Into<SharedString>>,
    selected: usize,
    theme: Theme,
) -> Segmented {
    Segmented {
        id: id.into(),
        items: items.into_iter().map(Into::into).collect(),
        selected,
        theme,
        on_select: None,
    }
}

impl Segmented {
    pub fn on_select(mut self, f: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Segmented {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let t = self.theme;
        let id = self.id.clone();
        div()
            .id(self.id)
            .flex_none()
            .flex()
            .p(px(2.0))
            .gap(px(2.0))
            .rounded(px(Radius::ROW))
            .bg(t.primary.alpha(0.06))
            .children(self.items.into_iter().enumerate().map(|(i, label)| {
                let on = i == self.selected;
                let select = self.on_select.clone();
                div()
                    .id(ElementId::Name(format!("{id:?}-{i}").into()))
                    .h(px(22.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .rounded(px(Radius::CONTROL - 1.0))
                    .border_1()
                    .border_color(rgba(0.0, 0.0, 0.0, 0.0))
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if on { t.primary } else { t.secondary })
                    .when(on, |d| {
                        d.bg(t.selected)
                            .border_color(t.selected_stroke)
                            .shadow(lift(0.08))
                    })
                    .when(!on, |d| {
                        d.cursor_pointer()
                            .hover(|s| s.text_color(t.primary))
                            .when_some(select, |d, f| d.on_click(move |_, w, cx| f(i, w, cx)))
                    })
                    .child(label)
            }))
    }
}

// ---------------------------------------------------------------------------
// Select: a button that opens a menu of choices

#[derive(Clone)]
pub struct Choice {
    pub label: SharedString,
    pub detail: Option<SharedString>,
    pub disabled: bool,
}

impl Choice {
    pub fn new(label: impl Into<SharedString>) -> Choice {
        Choice {
            label: label.into(),
            detail: None,
            disabled: false,
        }
    }

    pub fn detail(mut self, detail: impl Into<SharedString>) -> Choice {
        self.detail = Some(detail.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Choice {
        self.disabled = disabled;
        self
    }
}

impl<S: Into<SharedString>> From<S> for Choice {
    fn from(s: S) -> Choice {
        Choice::new(s)
    }
}

/// The select a UI demo opens (`open=ID` in PING_UI_DEMO and PONG_UI_DEMO),
/// so its menu can be checked and screenshotted without a click.
static DEMO_OPEN: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Open the select with id `id` the next time it is drawn (a UI demo step).
pub fn open_select(id: &str) {
    if let Ok(mut open) = DEMO_OPEN.lock() {
        *open = Some(id.to_string());
    }
}

/// Whether a demo asked for the select `id` to open; asked once.
fn demo_opens(id: &ElementId) -> bool {
    let ElementId::Name(name) = id else {
        return false;
    };
    let Ok(mut open) = DEMO_OPEN.lock() else {
        return false;
    };
    let asked = open.as_deref() == Some(name.as_ref());
    if asked {
        *open = None;
    }
    asked
}

#[derive(IntoElement)]
pub struct Select {
    id: ElementId,
    choices: Vec<Choice>,
    selected: Option<usize>,
    /// What the button says; the selected choice's label by default.
    shown: Option<SharedString>,
    theme: Theme,
    width: Option<f32>,
    disabled: bool,
    chip: bool,
    leading: Option<IconName>,
    on_select: Option<Handler<usize>>,
}

pub fn select(
    id: impl Into<ElementId>,
    choices: impl IntoIterator<Item = impl Into<Choice>>,
    selected: Option<usize>,
    theme: Theme,
) -> Select {
    Select {
        id: id.into(),
        choices: choices.into_iter().map(Into::into).collect(),
        selected,
        shown: None,
        theme,
        width: None,
        disabled: false,
        chip: false,
        leading: None,
        on_select: None,
    }
}

impl Select {
    pub fn width(mut self, w: f32) -> Self {
        self.width = Some(w);
        self
    }

    pub fn shown(mut self, text: impl Into<SharedString>) -> Self {
        self.shown = Some(text.into());
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Borderless and compact: a picker inside a composer or toolbar.
    pub fn chip(mut self) -> Self {
        self.chip = true;
        self
    }

    pub fn leading(mut self, name: IconName) -> Self {
        self.leading = Some(name);
        self
    }

    pub fn on_select(mut self, f: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.on_select = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Select {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = self.theme;
        let open = window.use_keyed_state(self.id.clone(), cx, |_, _| false);
        if demo_opens(&self.id) {
            open.update(cx, |o, _| *o = true);
        }
        let is_open = *open.read(cx);
        let shown = self
            .shown
            .clone()
            .or_else(|| {
                self.selected
                    .and_then(|i| self.choices.get(i))
                    .map(|c| c.label.clone())
            })
            .unwrap_or_else(|| "Choose…".into());
        let text = if self.disabled {
            t.quaternary
        } else if self.chip {
            t.secondary
        } else {
            t.primary
        };
        let toggle = open.clone();
        let chip = self.chip;
        let mut el = div()
            .id(self.id.clone())
            .relative()
            .flex_none()
            .h(px(if chip { 24.0 } else { Metrics::CONTROL }))
            .when_some(self.width, |d, w| d.w(px(w)))
            .px(px(if chip { 7.0 } else { 9.0 }))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(Radius::CONTROL))
            .when(!chip, |d| {
                d.bg(t.control).border_1().border_color(t.control_stroke)
            })
            .when(chip && is_open, |d| d.bg(t.hover))
            .text_size(px(12.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(text)
            .when_some(self.leading, |d, name| d.child(icon(name, 13.0, text)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(shown),
            )
            .child(icon(
                IconName::ChevronUpDown,
                12.0,
                if self.disabled {
                    t.quaternary
                } else {
                    t.tertiary
                },
            ));
        if !self.disabled {
            el = el
                .cursor_pointer()
                .hover(move |s| s.bg(if chip { t.hover } else { t.control_hover }))
                .on_click(move |_, _, cx| {
                    toggle.update(cx, |o, cx| {
                        *o = !*o;
                        cx.notify();
                    })
                });
        }
        if is_open {
            let close = open.clone();
            let menu = self.choices.into_iter().enumerate().map(|(i, c)| {
                let on = self.selected == Some(i);
                let select = self.on_select.clone();
                let close = open.clone();
                menu_row(
                    ElementId::Name(format!("{:?}-choice-{i}", self.id).into()),
                    t,
                )
                .when(c.disabled, |d| d.opacity(0.45))
                .child(
                    div()
                        .w(px(14.0))
                        .flex_none()
                        .when(on, |d| d.child(icon(IconName::Check, 13.0, t.primary))),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        .child(div().text_color(t.primary).child(c.label))
                        .when_some(c.detail, |d, detail| {
                            d.child(
                                div()
                                    .text_size(px(Type::META))
                                    .text_color(t.tertiary)
                                    .child(detail),
                            )
                        }),
                )
                .when(!c.disabled, |d| {
                    d.on_click(move |_, w, cx| {
                        close.update(cx, |o, cx| {
                            *o = false;
                            cx.notify();
                        });
                        if let Some(f) = &select {
                            f(i, w, cx);
                        }
                    })
                })
            });
            el = el.child(
                deferred(
                    anchored().snap_to_window_with_margin(px(8.0)).child(
                        div()
                            .mt(px(if chip { 28.0 } else { 30.0 }))
                            .occlude()
                            .child(
                                floating(t)
                                    .min_w(px(self.width.unwrap_or(160.0).max(160.0)))
                                    .p(px(4.0))
                                    .children(menu),
                            )
                            .on_mouse_down_out(move |_, _, cx| {
                                close.update(cx, |o, cx| {
                                    *o = false;
                                    cx.notify();
                                })
                            }),
                    ),
                )
                .priority(Layer::MENU),
            );
        }
        el
    }
}

/// A row in a floating menu: hover lifts it as a glass pill.
pub fn menu_row(id: impl Into<ElementId>, t: Theme) -> Stateful<Div> {
    div()
        .id(id)
        .min_h(px(26.0))
        .px(px(8.0))
        .py(px(3.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(Radius::CONTROL))
        .border_1()
        .border_color(rgba(0.0, 0.0, 0.0, 0.0))
        .text_size(px(Type::BODY))
        .cursor_pointer()
        .hover(move |s| s.bg(t.selected).border_color(t.selected_stroke))
}

/// A menu of actions at a point (right click), or under a control.
pub struct MenuItem {
    pub label: SharedString,
    pub icon: Option<IconName>,
    pub danger: bool,
    pub separator_before: bool,
    pub on_click: OnClick,
}

impl MenuItem {
    pub fn new(
        label: impl Into<SharedString>,
        f: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> MenuItem {
        MenuItem {
            label: label.into(),
            icon: None,
            danger: false,
            separator_before: false,
            on_click: Rc::new(f),
        }
    }

    pub fn icon(mut self, name: IconName) -> MenuItem {
        self.icon = Some(name);
        self
    }

    pub fn danger(mut self) -> MenuItem {
        self.danger = true;
        self
    }

    pub fn separated(mut self) -> MenuItem {
        self.separator_before = true;
        self
    }
}

/// The menu's panel (place it with `anchored`).
pub fn menu(id: &str, items: Vec<MenuItem>, t: Theme) -> Div {
    floating(t)
        .min_w(px(200.0))
        .p(px(4.0))
        .children(items.into_iter().enumerate().flat_map(|(i, item)| {
            let color = if item.danger {
                t.ink(Ink::DANGER)
            } else {
                t.primary
            };
            let f = item.on_click.clone();
            let sep = item.separator_before.then(|| {
                div()
                    .my(px(4.0))
                    .mx(px(6.0))
                    .h(px(1.0))
                    .bg(t.hairline)
                    .into_any_element()
            });
            let row = menu_row(ElementId::Name(format!("{id}-{i}").into()), t)
                .text_color(color)
                .child(
                    div()
                        .w(px(16.0))
                        .flex_none()
                        .when_some(item.icon, |d, n| d.child(icon(n, 14.0, color))),
                )
                .child(item.label)
                .on_click(move |e, w, cx| f(e, w, cx))
                .into_any_element();
            sep.into_iter().chain(std::iter::once(row))
        }))
}

// ---------------------------------------------------------------------------
// Surfaces

/// Floating chrome: menus, popovers, sheets.
pub fn floating(t: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(Radius::PANEL))
        .bg(t.floating)
        .border_1()
        .border_color(t.floating_stroke)
        .shadow(vec![
            BoxShadow {
                color: rgba(0.0, 0.0, 0.0, if t.dark { 0.40 } else { 0.16 }).into(),
                offset: point(px(0.0), px(14.0)),
                blur_radius: px(36.0),
                spread_radius: px(0.0),
                inset: false,
            },
            BoxShadow {
                color: rgba(1.0, 1.0, 1.0, if t.dark { 0.04 } else { 0.5 }).into(),
                offset: point(px(0.0), px(1.0)),
                blur_radius: px(0.0),
                spread_radius: px(0.0),
                inset: true,
            },
        ])
        .font_family(crate::ui_font())
}

pub fn lift(alpha: f32) -> Vec<BoxShadow> {
    vec![BoxShadow {
        color: rgba(0.0, 0.0, 0.0, alpha).into(),
        offset: point(px(0.0), px(1.0)),
        blur_radius: px(3.0),
        spread_radius: px(0.0),
        inset: false,
    }]
}

/// A modal sheet over the window: a scrim, and a centred panel.
pub fn sheet(
    id: impl Into<ElementId>,
    width: f32,
    t: Theme,
    content: impl IntoElement,
) -> impl IntoElement {
    deferred(
        div()
            .id(id)
            .absolute()
            .inset_0()
            .occlude()
            .bg(t.scrim)
            .flex()
            .items_center()
            .justify_center()
            .child(
                floating(t)
                    .rounded(px(Radius::SHEET))
                    .w(px(width))
                    .p(px(24.0))
                    .child(content)
                    .with_animation(
                        "sheet-in",
                        Animation::new(Duration::from_millis(160))
                            .with_easing(gpui::ease_out_quint()),
                        |d, delta| d.opacity(0.7 + 0.3 * delta).mt(px(8.0 * (1.0 - delta))),
                    ),
            ),
    )
    .priority(Layer::SHEET)
}

/// Titled group of rows (a settings section).
pub fn section(title: impl Into<SharedString>, t: Theme) -> Div {
    let title: SharedString = title.into();
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .when(!title.is_empty(), |d| {
            d.child(
                div()
                    .px(px(2.0))
                    .text_size(px(Type::META))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(t.tertiary)
                    .child(title),
            )
        })
}

/// The card a section's rows sit in; rows are divided by hairlines.
pub fn card(t: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(px(Radius::ROW + 1.0))
        .border_1()
        .border_color(t.card_stroke)
        .bg(t.card)
        .overflow_hidden()
}

/// A card's rows with hairlines between them.
pub fn rows(rows: impl IntoIterator<Item = AnyElement>, t: Theme) -> Div {
    let mut out = card(t);
    for (i, row) in rows.into_iter().enumerate() {
        if i > 0 {
            out = out.child(div().mx(px(12.0)).h(px(1.0)).bg(t.hairline));
        }
        out = out.child(row);
    }
    out
}

/// A setting: its name and a line on what it does, the control at the right.
pub fn setting(
    label: impl Into<SharedString>,
    detail: Option<SharedString>,
    control: impl IntoElement,
    t: Theme,
) -> Div {
    div()
        .min_h(px(Metrics::SETTINGS_ROW))
        .px(px(12.0))
        .py(px(8.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .child(label_stack(label, detail, t))
        .child(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(control),
        )
}

pub fn label_stack(label: impl Into<SharedString>, detail: Option<SharedString>, t: Theme) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .child(
            div()
                .text_size(px(Type::BODY))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.primary)
                .child(label.into()),
        )
        .when_some(detail, |d, detail| {
            d.child(
                div()
                    .text_size(px(Type::META))
                    .line_height(px(15.0))
                    .text_color(t.tertiary)
                    .child(detail),
            )
        })
}

/// A page's title, a line under it, and actions at the right.
pub fn page_header(
    title: impl Into<SharedString>,
    subtitle: Option<SharedString>,
    trailing: Option<AnyElement>,
    t: Theme,
) -> Div {
    div()
        .flex()
        .items_start()
        .justify_between()
        .gap(px(16.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .min_w_0()
                .child(
                    div()
                        .text_size(px(Type::DISPLAY))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(t.primary)
                        .child(title.into()),
                )
                .when_some(subtitle, |d, s| {
                    d.child(
                        div()
                            .text_size(px(Type::BODY))
                            .text_color(t.tertiary)
                            .child(s),
                    )
                }),
        )
        .when_some(trailing, |d, e| {
            d.child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .pt(px(4.0))
                    .child(e),
            )
        })
}

/// Small print under a card.
pub fn footnote(text: impl Into<SharedString>, t: Theme) -> Div {
    div()
        .px(px(2.0))
        .text_size(px(Type::META))
        .line_height(px(15.0))
        .text_color(t.tertiary)
        .child(text.into())
}

pub fn hairline(t: Theme) -> Div {
    div().flex_none().h(px(1.0)).w_full().bg(t.hairline)
}

// ---------------------------------------------------------------------------
// Status

/// A compact label in a quiet pill (a state chip).
pub fn chip(label: impl Into<SharedString>, tint: Rgba, t: Theme) -> Div {
    div()
        .flex_none()
        .px(px(6.0))
        .py(px(1.0))
        .rounded(px(Radius::CHIP))
        .bg(if tint == t.secondary || tint == t.tertiary {
            t.primary.alpha(0.06)
        } else {
            tint.alpha(0.13)
        })
        .text_size(px(Type::META))
        .line_height(px(15.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(tint)
        .whitespace_nowrap()
        .child(label.into())
}

pub fn dot(color: Rgba, size: f32) -> Div {
    div()
        .flex_none()
        .size(px(size))
        .rounded(px(size / 2.0))
        .bg(color)
}

/// A status dot with a soft halo, for "live".
pub fn live_dot(color: Rgba) -> impl IntoElement {
    div()
        .relative()
        .flex_none()
        .size(px(14.0))
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .absolute()
                .size(px(14.0))
                .rounded(px(7.0))
                .bg(color.alpha(0.25))
                .with_animation(
                    "live-halo",
                    Animation::new(Duration::from_millis(1800)).repeat(),
                    |d, delta| {
                        let pulse = 1.0 - (delta * 2.0 - 1.0).abs();
                        d.opacity(0.25 + 0.75 * pulse)
                    },
                ),
        )
        .child(dot(color, 7.0))
}

/// Work in progress: a turning arrow.
pub fn spinner(id: impl Into<ElementId>, size: f32, color: Rgba) -> impl IntoElement {
    svg()
        .path(IconName::Refresh.path())
        .flex_none()
        .size(px(size))
        .text_color(color)
        .with_animation(
            id,
            Animation::new(Duration::from_millis(900)).repeat(),
            |s, delta| s.with_transformation(Transformation::rotate(percentage(delta))),
        )
}

/// A key or chord, as a keycap.
pub fn kbd(keys: impl Into<SharedString>, t: Theme) -> Div {
    div()
        .flex_none()
        .h(px(20.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .rounded(px(Radius::CHIP))
        .border_1()
        .border_color(t.control_stroke)
        .bg(t.primary.alpha(0.04))
        .text_size(px(Type::META + 0.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(t.secondary)
        .child(keys.into())
}

/// A statistic: a large number and what it counts.
pub fn stat(value: impl Into<SharedString>, label: impl Into<SharedString>, t: Theme) -> Div {
    div()
        .flex_1()
        .min_w(px(96.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(12.0))
        .py(px(10.0))
        .child(
            div()
                .text_size(px(18.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.primary)
                .child(value.into()),
        )
        .child(
            div()
                .text_size(px(Type::META))
                .text_color(t.tertiary)
                .child(label.into()),
        )
}

/// A message in a tinted card: success, warning, failure.
pub fn notice(icon_name: IconName, tint: Rgba, text: impl Into<SharedString>, t: Theme) -> Div {
    let ink = t.ink(tint);
    div()
        .flex()
        .items_start()
        .gap(px(8.0))
        .px(px(12.0))
        .py(px(9.0))
        .rounded(px(Radius::ROW))
        .bg(tint.alpha(if t.dark { 0.10 } else { 0.09 }))
        .text_size(px(12.0))
        .line_height(px(17.0))
        .text_color(if t.dark {
            t.primary.alpha(0.88)
        } else {
            t.primary.alpha(0.8)
        })
        .child(div().pt(px(1.0)).child(icon(icon_name, 14.0, ink)))
        .child(div().flex_1().min_w_0().child(text.into()))
}

// ---------------------------------------------------------------------------
// Slider and stepper

struct SliderState {
    bounds: Option<gpui::Bounds<gpui::Pixels>>,
    dragging: bool,
}

/// A horizontal slider over 0..=1.
#[derive(IntoElement)]
pub struct Slider {
    id: ElementId,
    value: f32,
    width: f32,
    theme: Theme,
    disabled: bool,
    on_change: Option<Handler<f32>>,
}

pub fn slider(id: impl Into<ElementId>, value: f32, width: f32, theme: Theme) -> Slider {
    Slider {
        id: id.into(),
        value: value.clamp(0.0, 1.0),
        width,
        theme,
        disabled: false,
        on_change: None,
    }
}

impl Slider {
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn on_change(mut self, f: impl Fn(f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for Slider {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let t = self.theme;
        let state = window.use_keyed_state(self.id.clone(), cx, |_, _| SliderState {
            bounds: None,
            dragging: false,
        });
        const KNOB: f32 = 14.0;
        let value_at = |bounds: gpui::Bounds<gpui::Pixels>, x: gpui::Pixels| -> f32 {
            let travel = f32::from(bounds.size.width) - KNOB;
            ((f32::from(x - bounds.origin.x) - KNOB / 2.0) / travel.max(1.0)).clamp(0.0, 1.0)
        };
        let fill_color = if self.disabled {
            t.primary.alpha(0.18)
        } else {
            t.primary.alpha(0.55)
        };
        let knob_color = if t.dark {
            rgba(0.92, 0.92, 0.93, 1.0)
        } else {
            rgba(1.0, 1.0, 1.0, 1.0)
        };
        let (s1, s2, s3) = (state.clone(), state.clone(), state.clone());
        let (f1, f2) = (self.on_change.clone(), self.on_change.clone());
        let disabled = self.disabled;
        div()
            .id(self.id)
            .relative()
            .flex_none()
            .w(px(self.width))
            .h(px(18.0))
            .flex()
            .items_center()
            .when(disabled, |d| d.opacity(0.5))
            .child(
                gpui::canvas(
                    move |bounds, _, cx| s1.update(cx, |s, _| s.bounds = Some(bounds)),
                    move |_, _, window, _| {
                        // Follow the pointer anywhere while dragging.
                        window.on_mouse_event(
                            move |e: &gpui::MouseMoveEvent, phase, window, cx| {
                                if phase != gpui::DispatchPhase::Bubble {
                                    return;
                                }
                                let (dragging, bounds) = {
                                    let s = s2.read(cx);
                                    (s.dragging, s.bounds)
                                };
                                if !dragging || e.pressed_button != Some(gpui::MouseButton::Left) {
                                    return;
                                }
                                if let (Some(b), Some(f)) = (bounds, &f1) {
                                    f(value_at(b, e.position.x), window, cx);
                                }
                            },
                        );
                        let s = s3.clone();
                        window.on_mouse_event(move |_: &gpui::MouseUpEvent, _, _, cx| {
                            s.update(cx, |s, _| s.dragging = false);
                        });
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                div()
                    .mx(px(KNOB / 2.0))
                    .flex_1()
                    .h(px(4.0))
                    .rounded(px(2.0))
                    .bg(t.primary.alpha(0.10))
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(self.value))
                            .rounded(px(2.0))
                            .bg(fill_color),
                    ),
            )
            .child(
                div()
                    .absolute()
                    .top(px(2.0))
                    .left(px(self.value * (self.width - KNOB)))
                    .size(px(KNOB))
                    .rounded(px(KNOB / 2.0))
                    .bg(knob_color)
                    .border_1()
                    .border_color(rgba(0.0, 0.0, 0.0, 0.12))
                    .shadow(lift(0.25)),
            )
            .when(!disabled, |d| {
                d.cursor_pointer()
                    .on_mouse_down(gpui::MouseButton::Left, move |e, window, cx| {
                        let bounds = state.update(cx, |s, _| {
                            s.dragging = true;
                            s.bounds
                        });
                        if let (Some(b), Some(f)) = (bounds, &f2) {
                            f(value_at(b, e.position.x), window, cx);
                        }
                    })
            })
    }
}

/// − value +, for a bounded count.
pub fn stepper(
    id: &str,
    value: u32,
    range: std::ops::RangeInclusive<u32>,
    step: u32,
    t: Theme,
    on_change: impl Fn(u32, &mut Window, &mut App) + 'static,
) -> Div {
    let on_change = Rc::new(on_change);
    let (lo, hi) = (*range.start(), *range.end());
    let (dec, inc) = (on_change.clone(), on_change);
    let arrow = |id: String, label: &'static str, enabled: bool, f: Handler<u32>, next: u32| {
        div()
            .id(ElementId::Name(id.into()))
            .size(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Radius::CHIP))
            .text_color(if enabled { t.secondary } else { t.quaternary })
            .text_size(px(14.0))
            .when(enabled, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(t.hover))
                    .on_click(move |_, w, cx| f(next, w, cx))
            })
            .child(label)
    };
    div()
        .flex()
        .items_center()
        .gap(px(2.0))
        .h(px(Metrics::CONTROL))
        .px(px(2.0))
        .rounded(px(Radius::CONTROL))
        .bg(t.control)
        .border_1()
        .border_color(t.control_stroke)
        .child(arrow(
            format!("{id}-dec"),
            "−",
            value > lo,
            dec,
            value.saturating_sub(step).max(lo),
        ))
        .child(
            div()
                .min_w(px(34.0))
                .flex()
                .justify_center()
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(t.primary)
                .child(value.to_string()),
        )
        .child(arrow(
            format!("{id}-inc"),
            "+",
            value < hi,
            inc,
            (value + step).min(hi),
        ))
}

/// A page's own strip along the title bar: its actions at the right.
pub fn toolbar_row(trailing: impl IntoIterator<Item = AnyElement>) -> Div {
    div()
        .flex_none()
        .h(px(Metrics::TOOLBAR))
        .px(px(14.0))
        .flex()
        .items_center()
        .justify_end()
        .gap(px(6.0))
        .children(trailing)
}
