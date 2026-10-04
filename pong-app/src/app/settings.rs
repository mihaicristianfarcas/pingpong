//! The settings pages (General, Video, Network, AI agents): each saves as it changes.
//!
//! All of them are the host's, kept by the host, but for General's last
//! section: the window's own (its icon at login, the update check), which
//! are this user's and kept by the app.

use std::time::Instant;

use gpui::{div, prelude::*, px, AnyElement, Context, ElementId};
use pingpong_ui::login::LoginItem;
use pingpong_ui::{
    field, notice, rows, section, select, setting, stepper, switch, IconName, Ink, Theme,
};
use serde_json::json;

use super::{page, Page, PongApp, UPDATE_APP};

/// The app at login: in the tray, without its window.
const AT_LOGIN: LoginItem = LoginItem {
    id: crate::background::APP_ID,
    name: "Pong",
    args: &["--background"],
    keep_alive: false,
};

/// Whether this copy of the app starts at login as things stand.
pub(super) fn starts_at_login() -> bool {
    std::env::current_exe().is_ok_and(|exe| AT_LOGIN.enabled(&exe))
}

impl PongApp {
    pub(super) fn settings(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        if self.config().is_none() {
            return super::signin::waiting("settings-page", "Reading the settings…", t);
        }
        let mac = self.os() == "macos";
        let restart = if mac {
            "Restart Pong"
        } else {
            "Restart PongService"
        };
        let (title, subtitle, body): (&str, &str, AnyElement) = match self.page {
            Page::General => (
                "General",
                "This host, as Ping sees it.",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(20.0))
                    .child(
                        section("", t).child(rows(
                            [
                                setting(
                                    "Name",
                                    Some(
                                        format!(
                                            "Shown to clients. Press Return \
                                                to save; {restart} for it to apply."
                                        )
                                        .into(),
                                    ),
                                    div().w(px(200.0)).child(field(&self.name, t)),
                                    t,
                                )
                                .into_any_element(),
                                self.toggle(
                                    "allow_takeover",
                                    "Let a client take over a session",
                                    "Otherwise a second client is refused while one streams.",
                                    t,
                                    cx,
                                ),
                                self.toggle(
                                    "keep_host_displays",
                                    if mac {
                                        "Keep this Mac's displays on while streaming"
                                    } else {
                                        "Keep this PC's monitors on while streaming"
                                    },
                                    "Off: the virtual display becomes the whole desktop, as in \
                                        Apollo, so windows open where the client sees them.",
                                    t,
                                    cx,
                                ),
                                self.toggle(
                                    "clipboard",
                                    "Share the clipboard with clients",
                                    "Text, images and files copied on either side can be \
                                        pasted on the other, when the client asks for it too. \
                                        Password managers' copies are never shared.",
                                    t,
                                    cx,
                                ),
                            ],
                            t,
                        )),
                    )
                    .child(section("This window", t).child(rows(self.window_rows(t, cx), t)))
                    .into_any_element(),
            ),
            Page::Video => {
                let mut encoder = Vec::new();
                if !mac {
                    let presets: Vec<String> = (1..=7)
                        .map(|p| match p {
                            1 => "P1 (fastest)".to_string(),
                            7 => "P7 (best quality)".to_string(),
                            p => format!("P{p}"),
                        })
                        .collect();
                    let preset = self.cfg_u64("nvenc_preset").clamp(1, 7) as usize - 1;
                    let this = cx.weak_entity();
                    encoder.push(
                        setting(
                            "NVENC preset",
                            Some("Lower is faster. Apollo uses P1.".into()),
                            select("nvenc-preset", presets, Some(preset), t)
                                .width(170.0)
                                .on_select(move |i, _, cx| {
                                    let _ = this.update(cx, |app, cx| {
                                        app.set_config("nvenc_preset", json!(i + 1), cx)
                                    });
                                }),
                            t,
                        )
                        .into_any_element(),
                    );
                    encoder.push(self.toggle(
                        "nvenc_two_pass",
                        "Two-pass encoding",
                        "A quarter-resolution first pass for better bit allocation, \
                            at no extra latency.",
                        t,
                        cx,
                    ));
                    encoder.push(self.toggle(
                        "nvidia_max_power",
                        "Full GPU power for Pong",
                        "The NVIDIA driver keeps its clocks up for Pong instead of \
                            lowering them between frames, which slows the encoder. \
                            Applies after Pong restarts.",
                        t,
                        cx,
                    ));
                    encoder.push(self.toggle(
                        "nvidia_dxgi_present",
                        "OpenGL and Vulkan through DXGI",
                        "Full-screen OpenGL and Vulkan games are captured at their full \
                            frame rate. Changes the driver's setting for every program \
                            while Pong runs, and puts it back when it stops. Applies \
                            after Pong restarts.",
                        t,
                        cx,
                    ));
                }
                encoder.push(self.toggle(
                    "allow_hevc",
                    "Allow HEVC",
                    "Better quality per \
                        bit than H.264, when the client decodes it.",
                    t,
                    cx,
                ));
                if !mac {
                    encoder.push(self.toggle(
                        "allow_av1",
                        "Allow AV1",
                        "When both this GPU and the client support it.",
                        t,
                        cx,
                    ));
                }
                let limits = vec![
                    self.choice(
                        "max_bitrate_kbps",
                        "Maximum bitrate",
                        "What a client may ask for.",
                        &[0, 20_000, 50_000, 80_000, 100_000, 150_000],
                        |v| {
                            if v == 0 {
                                "The client's choice".into()
                            } else {
                                format!("{} Mbps", v / 1000)
                            }
                        },
                        t,
                        cx,
                    ),
                    self.choice(
                        "max_fps",
                        "Maximum frame rate",
                        "What a client may ask for.",
                        &[0, 30, 60, 90, 120, 144],
                        |v| {
                            if v == 0 {
                                "The client's choice".into()
                            } else {
                                format!("{v} FPS")
                            }
                        },
                        t,
                        cx,
                    ),
                    self.choice(
                        "pace_mbps",
                        "Send pacing",
                        "Frames leave spread at this \
                            rate instead of in one burst. Lower it for Wi-Fi or slow uplinks.",
                        &[100, 200, 400, 800, 1000],
                        |v| format!("{v} Mbit/s"),
                        t,
                        cx,
                    ),
                    self.toggle(
                        "adaptive_bitrate",
                        "Adapt the bitrate to the network",
                        "Lower it when the network congests, climb back when it \
                            clears. Random loss is left to FEC.",
                        t,
                        cx,
                    ),
                ];
                (
                    "Video",
                    "The encoder and what clients may ask of it. Changes apply to the next \
                        session.",
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(20.0))
                        .child(section("Encoder", t).child(rows(encoder, t)))
                        .child(section("Limits", t).child(rows(limits, t)))
                        .into_any_element(),
                )
            }
            Page::Network => (
                "Network",
                "How devices reach this host.",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(20.0))
                    .child(section("", t).child(rows(
                        [
                            self.toggle(
                                "internet_access",
                                "Allow streaming over the internet",
                                "Paired devices connect from anywhere, with no port \
                                    forwarding: this host's public address is published \
                                    sealed, readable only by them.",
                                t,
                                cx,
                            ),
                            self.toggle(
                                "port_mapping",
                                "Ask the router to forward the port",
                                "UPnP or NAT-PMP, as Apollo does. Helps where hole \
                                    punching cannot; a router behind another NAT (the \
                                    ISP's box) cannot help, and is left alone.",
                                t,
                                cx,
                            ),
                        ],
                        t,
                    )))
                    .child(
                        section("Ports", t).child(rows(
                            [
                                ("Tunnel (UDP)", 0usize),
                                ("Pairing (TCP)", 1),
                                ("Web UI (TCP)", 2),
                            ]
                            .into_iter()
                            .map(|(label, i)| {
                                setting(
                                    label,
                                    None,
                                    div().w(px(90.0)).child(field(&self.ports[i], t)),
                                    t,
                                )
                                .into_any_element()
                            }),
                            t,
                        )),
                    )
                    .child(pingpong_ui::footnote(
                        format!(
                            "Press Return in a field to save \
                                it. {restart} for port changes to apply."
                        ),
                        t,
                    ))
                    .into_any_element(),
            ),
            _ => {
                let hold = self.cfg_u64("agent_local_input_hold_secs") as u32;
                let this = cx.weak_entity();
                (
                    "AI agents",
                    "What agents paired with this host may do. Each one's own access is \
                        set on Devices.",
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(20.0))
                        .child(
                            section("", t).child(rows(
                                [
                                    self.toggle(
                                        "agents",
                                        "Allow paired AI agents",
                                        "Agents pair like devices and are marked as agents. \
                                            They never take over a person, and a person always \
                                            takes over from them.",
                                        t,
                                        cx,
                                    ),
                                    setting(
                                        "Hold after local input",
                                        Some(
                                            "Using this computer's own keyboard or mouse \
                                                holds an agent's for this many seconds. 0: never \
                                                hold."
                                                .into(),
                                        ),
                                        stepper("hold", hold, 0..=120, 5, t, move |v, _, cx| {
                                            let _ = this.update(cx, |app, cx| {
                                                app.set_config(
                                                    "agent_local_input_hold_secs",
                                                    json!(v),
                                                    cx,
                                                )
                                            });
                                        }),
                                        t,
                                    )
                                    .into_any_element(),
                                ],
                                t,
                            )),
                        )
                        .into_any_element(),
                )
            }
        };
        let mut content = div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(pingpong_ui::page_header(
                title,
                Some(subtitle.into()),
                None,
                t,
            ));
        if self.restart_needed {
            content = content.child(notice(
                IconName::Refresh,
                Ink::ATTENTION,
                format!("Saved. {restart} for the name or port change to apply."),
                t,
            ));
        }
        if let Some((glyph, tint, text, _)) = self.note.clone() {
            content = content.child(notice(glyph, tint, text, t));
        }
        page(t, [], "settings-page", content.child(body))
    }

    /// The window's own settings: its icon at login (where there is a tray
    /// for it), and the update check.
    fn window_rows(&mut self, t: Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if pingpong_ui::tray::supported() && pingpong_ui::login::supported() {
            let this = cx.weak_entity();
            out.push(
                setting(
                    "Show Pong's icon at login",
                    Some(
                        format!(
                            "Pong's icon is in the {} from the moment you log in, without \
                                this window: what the host is doing, a device asking to pair, \
                                and the window itself are a click away.",
                            if cfg!(target_os = "macos") {
                                "menu bar"
                            } else {
                                "taskbar's notification area"
                            }
                        )
                        .into(),
                    ),
                    switch("at-login", self.starts_at_login, t).on_toggle(move |on, _, cx| {
                        let _ = this.update(cx, |app, cx| app.set_starts_at_login(on, cx));
                    }),
                    t,
                )
                .into_any_element(),
            );
        }
        let this = cx.weak_entity();
        out.push(
            pingpong_ui::updates::channel_setting(
                "update-channel",
                &UPDATE_APP,
                self.prefs.update_channel(),
                t,
                move |channel, _, cx| {
                    let _ = this.update(cx, |app, cx| {
                        tracing::info!(?channel, "update check");
                        app.prefs.updates = Some(channel);
                        app.prefs.save(&crate::api::app_dir());
                        app.updates.set_channel(channel);
                        cx.notify();
                    });
                },
            )
            .into_any_element(),
        );
        let this = cx.weak_entity();
        out.push(
            pingpong_ui::updates::version_setting(
                &UPDATE_APP,
                &self.updates.status(),
                t,
                move |_, cx| {
                    let _ = this.update(cx, |app, cx| {
                        app.update_sheet = true;
                        app.updates.check_now();
                        cx.notify();
                    });
                },
            )
            .into_any_element(),
        );
        out
    }

    fn set_starts_at_login(&mut self, on: bool, cx: &mut Context<Self>) {
        let done = std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|exe| AT_LOGIN.set(&exe, on));
        match done {
            Ok(()) => tracing::info!(on, "Pong's icon at login"),
            Err(e) => {
                tracing::warn!(error = e, "Pong's icon at login was not changed");
                self.note = Some((IconName::Warning, Ink::DANGER, e, Instant::now()));
            }
        }
        self.starts_at_login = starts_at_login();
        cx.notify();
    }

    pub(super) fn toggle(
        &self,
        key: &'static str,
        label: &str,
        detail: &str,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.weak_entity();
        setting(
            label.to_string(),
            Some(detail.to_string().into()),
            switch(ElementId::Name(key.into()), self.cfg_bool(key), t).on_toggle(
                move |on, _, cx| {
                    let _ = this.update(cx, |app, cx| app.set_config(key, json!(on), cx));
                },
            ),
            t,
        )
        .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn choice(
        &self,
        key: &'static str,
        label: &str,
        detail: &str,
        values: &[u64],
        name: impl Fn(u64) -> String,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.cfg_u64(key);
        let mut values = values.to_vec();
        if !values.contains(&current) {
            values.push(current);
            values.sort_unstable();
        }
        let names: Vec<String> = values.iter().map(|v| name(*v)).collect();
        let index = values.iter().position(|v| *v == current);
        let this = cx.weak_entity();
        setting(
            label.to_string(),
            Some(detail.to_string().into()),
            select(ElementId::Name(key.into()), names, index, t)
                .width(170.0)
                .on_select(move |i, _, cx| {
                    let v = values[i];
                    let _ = this.update(cx, |app, cx| app.set_config(key, json!(v), cx));
                }),
            t,
        )
        .into_any_element()
    }
}
