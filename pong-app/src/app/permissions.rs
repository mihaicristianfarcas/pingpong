//! What each paired device may do here: the presets offered with a pairing
//! request's PIN, the line under each device on Devices, and the sheet that
//! sets each permission (`pingpong_proto::permission`).

use std::time::{Duration, Instant};

use gpui::{div, prelude::*, px, AnyElement, Context, ElementId, FontWeight};
use pingpong_proto::permission::{self, Permissions};
use pingpong_ui::{button, rows, section, select, setting, sheet, switch, Choice, Theme, Type};

use crate::api::Client;
use crate::worker::Cmd;

use super::PongApp;

/// How long a change made here stands against the host's older word (the
/// next refresh may have left before the change arrived).
const AHEAD_FOR: Duration = Duration::from_secs(3);
/// What the sheet leaves of the window's height: its own padding (24 each
/// side) and a margin to the window's edges.
const SHEET_MARGIN: f32 = 96.0;

/// A set of permissions with a name, to choose in one go.
pub(super) struct Preset {
    pub label: &'static str,
    pub detail: &'static str,
    pub permissions: Permissions,
}

pub(super) fn presets(agent: bool) -> [Preset; 3] {
    if agent {
        [
            Preset {
                label: "See and control",
                detail: "Sees the screen, uses the keyboard and mouse.",
                permissions: Permissions::AGENT_ALL,
            },
            Preset {
                label: "Only while watched",
                detail: "Acts only while someone watches its session.",
                permissions: Permissions::AGENT_WATCHED,
            },
            Preset {
                label: "See only",
                detail: "Sees the screen; no keyboard or mouse.",
                permissions: Permissions::SEE_ONLY,
            },
        ]
    } else {
        [
            Preset {
                label: "Everything",
                detail: "Input, clipboard, apps, taking over, watching agents.",
                permissions: Permissions::PERSON_ALL,
            },
            Preset {
                label: "See and control",
                detail: "Keyboard, mouse and controllers; nothing else.",
                permissions: Permissions::PERSON_CONTROL,
            },
            Preset {
                label: "See only",
                detail: "Sees the screen; its input is ignored.",
                permissions: Permissions::SEE_ONLY,
            },
        ]
    }
}

/// One permission as the sheet shows it.
struct Row {
    bit: u16,
    label: &'static str,
    detail: &'static str,
}

/// The sheet's sections, for a person's device or an agent.
fn sections(agent: bool) -> Vec<(&'static str, Vec<Row>)> {
    let see = Row {
        bit: permission::VIEW,
        label: "See the screen",
        detail: if agent {
            "Take screenshots of this computer. Off: the agent is turned away."
        } else {
            "Stream this computer's screen and sound. Off: the device is turned away."
        },
    };
    let keyboard = Row {
        bit: permission::KEYBOARD,
        label: "Keyboard",
        detail: "Type, and press keys and shortcuts.",
    };
    let mouse = Row {
        bit: permission::MOUSE,
        label: "Mouse",
        detail: "Move the pointer, click and scroll.",
    };
    if agent {
        return vec![
            ("Screen", vec![see]),
            ("Input", vec![keyboard, mouse]),
            (
                "Supervision",
                vec![Row {
                    bit: permission::UNWATCHED,
                    label: "Act while nobody watches",
                    detail: "Off: the agent waits until someone watches its session \
                        (Log In in Ping).",
                }],
            ),
        ];
    }
    vec![
        (
            "Screen",
            vec![
                see,
                Row {
                    bit: permission::LAUNCH,
                    label: "Start apps",
                    detail: "Open an app with the stream, such as Steam Big Picture.",
                },
                Row {
                    bit: permission::TAKE_OVER,
                    label: "Take over",
                    detail: "Stream while another device does, ending its stream (when \
                        Settings > General lets clients take over).",
                },
            ],
        ),
        (
            "Input",
            vec![
                keyboard,
                mouse,
                Row {
                    bit: permission::CONTROLLER,
                    label: "Controllers",
                    detail: "Play with game controllers (on a Windows host).",
                },
            ],
        ),
        (
            "Clipboard",
            vec![
                Row {
                    bit: permission::CLIPBOARD_READ,
                    label: "Copy from this computer",
                    detail: "What is copied here can be pasted on the device.",
                },
                Row {
                    bit: permission::CLIPBOARD_WRITE,
                    label: "Paste to this computer",
                    detail: "What is copied on the device can be pasted here.",
                },
            ],
        ),
        (
            "AI agents",
            vec![Row {
                bit: permission::WATCH,
                label: "Watch AI agents",
                detail: "Watch an agent's session, pause or stop it, and take over \
                    with the keyboard or mouse.",
            }],
        ),
    ]
}

/// A few words on what a device may do, for its line on Devices.
pub(super) fn summary(agent: bool, p: Permissions) -> String {
    if !p.allows(permission::VIEW) {
        return "Turned away".into();
    }
    if let Some(preset) = presets(agent).iter().find(|s| s.permissions == p) {
        return preset.label.into();
    }
    let possible = Permissions::possible(agent);
    let count = |p: Permissions| p.names().len();
    format!("{} of {} permissions", count(p), count(possible))
}

/// The choices of a select of presets, and which one `p` is.
fn preset_choices(agent: bool, p: Permissions) -> (Vec<Choice>, Option<usize>) {
    let presets = presets(agent);
    let at = presets.iter().position(|s| s.permissions == p);
    let choices = presets
        .iter()
        .map(|s| Choice::new(s.label).detail(s.detail))
        .collect();
    (choices, at)
}

impl PongApp {
    /// The select that chooses what a device asking to pair may do.
    pub(super) fn pairing_choice(
        &mut self,
        id: u32,
        agent: bool,
        default: Permissions,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chosen = self.pair_choice.get(&id).copied().unwrap_or(default);
        let (choices, at) = preset_choices(agent, chosen);
        let this = cx.weak_entity();
        select(ElementId::Name(format!("may-{id}").into()), choices, at, t)
            .width(170.0)
            .on_select(move |i, _, cx| {
                let _ = this.update(cx, |app, cx| {
                    app.pair_choice.insert(id, presets(agent)[i].permissions);
                    cx.notify();
                });
            })
            .into_any_element()
    }

    /// Open the permissions sheet for the device with `key`.
    pub(super) fn edit_permissions(&mut self, key: String, cx: &mut Context<Self>) {
        self.confirm = None;
        self.editing = Some(key);
        cx.notify();
    }

    /// Give the device with `key` these permissions: here at once, on the
    /// host as soon as it answers.
    fn set_permissions(&mut self, key: &str, p: Permissions, cx: &mut Context<Self>) {
        let Some(c) = self.snap.clients.iter_mut().find(|c| c.key == key) else {
            return;
        };
        let p = p.fit(c.agent);
        if c.permissions == p {
            return;
        }
        c.permissions = p;
        self.permissions_ahead = Some((key.to_string(), p, Instant::now()));
        self.send(Cmd::Permissions(key.to_string(), p));
        cx.notify();
    }

    /// A change made here, kept over a host's word that predates it.
    pub(super) fn keep_permissions_ahead(&mut self, clients: &mut [Client]) {
        let Some((key, p, at)) = &self.permissions_ahead else {
            return;
        };
        if at.elapsed() > AHEAD_FOR {
            self.permissions_ahead = None;
            return;
        }
        if let Some(c) = clients.iter_mut().find(|c| &c.key == key) {
            c.permissions = *p;
        }
    }

    /// The sheet that sets what one device may do, in a window `window_h`
    /// points tall.
    pub(super) fn permissions_sheet(
        &mut self,
        window_h: f32,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let key = self.editing.clone()?;
        let Some(c) = self.snap.clients.iter().find(|c| c.key == key).cloned() else {
            // Unpaired meanwhile.
            self.editing = None;
            return None;
        };
        let (choices, at) = preset_choices(c.agent, c.permissions);
        let this = cx.weak_entity();
        let preset_key = key.clone();
        let preset = setting(
            "Start from",
            Some("A set to begin with; each permission below changes on its own.".into()),
            select("permissions-preset", choices, at, t)
                .shown(if at.is_some() {
                    summary(c.agent, c.permissions)
                } else {
                    "Custom".into()
                })
                .width(170.0)
                .on_select(move |i, _, cx| {
                    let _ = this.update(cx, |app, cx| {
                        let p = presets(c.agent)[i].permissions;
                        app.set_permissions(&preset_key, p, cx);
                    });
                }),
            t,
        );
        let note = if c.agent {
            "A change applies at once, to a session that is running too."
        } else {
            "A change applies at once, to a stream that is running too. Clipboard \
                sharing that was off when a stream started comes on with the next one."
        };
        let head = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .text_size(px(17.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("What {} may do", c.name)),
            )
            .child(
                div()
                    .text_size(px(Type::META + 0.5))
                    .line_height(px(16.0))
                    .text_color(t.secondary)
                    .child(note),
            );
        let mut list = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(rows([preset.into_any_element()], t));
        for (title, rows_of) in sections(c.agent) {
            let items: Vec<AnyElement> = rows_of
                .into_iter()
                .map(|row| {
                    let this = cx.weak_entity();
                    let (key, now) = (key.clone(), c.permissions);
                    let on = now.allows(row.bit);
                    setting(
                        row.label,
                        Some(row.detail.into()),
                        switch(ElementId::Name(format!("perm-{}", row.bit).into()), on, t)
                            .on_toggle(move |on, _, cx| {
                                let _ = this.update(cx, |app, cx| {
                                    app.set_permissions(&key, now.with(row.bit, on), cx);
                                });
                            }),
                        t,
                    )
                    .into_any_element()
                })
                .collect();
            list = list.child(section(title, t).child(rows(items, t)));
        }
        // The title and Done stay put; the permissions take what height the
        // window leaves them, and scroll in what they cannot have.
        let body = div()
            .flex()
            .flex_col()
            .gap(px(14.0))
            .max_h(px((window_h - SHEET_MARGIN).max(240.0)))
            .child(head.flex_none())
            .child(
                div()
                    .id("permissions-sheet-body")
                    .flex_shrink(1.0)
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .child(
                div().flex_none().flex().justify_end().child(
                    button("permissions-done", "Done", t)
                        .solid()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.editing = None;
                            cx.notify();
                        })),
                ),
            );
        Some(sheet("permissions", 460.0, t, body).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_devices_line_names_its_preset_or_counts_what_it_may_do() {
        assert_eq!(summary(false, Permissions::PERSON_ALL), "Everything");
        assert_eq!(summary(false, Permissions::SEE_ONLY), "See only");
        assert_eq!(
            summary(true, Permissions::AGENT_WATCHED),
            "Only while watched"
        );
        assert_eq!(summary(true, Permissions::NONE), "Turned away");
        let custom = Permissions::PERSON_CONTROL.with(permission::CLIPBOARD_READ, true);
        assert_eq!(summary(false, custom), "5 of 9 permissions");
    }

    #[test]
    fn every_permission_a_device_can_have_has_a_row() {
        for agent in [false, true] {
            let shown = sections(agent)
                .iter()
                .flat_map(|(_, rows)| rows.iter().map(|r| r.bit))
                .fold(0, |all, bit| all | bit);
            assert_eq!(shown, Permissions::possible(agent).bits(), "agent: {agent}");
        }
    }
}
