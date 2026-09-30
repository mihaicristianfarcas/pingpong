//! The app's own settings (General), Moonlight's (video, audio, input) and
//! the agents' setup, one page each: cards of labelled rows, each saying
//! what it does.

use gpui::{div, prelude::*, px, AnyElement, App, Context, SharedString, Window};
use pingpong_ui::{button, rows, section, select, setting, slider, switch, Choice, Theme, Type};

use crate::app::{page_body, toolbar, PingApp};
use crate::prefs::{parse_size, Prefs, FRAME_RATES, RESOLUTIONS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    General,
    Video,
    Audio,
    Input,
    Agents,
}

impl Tab {
    pub fn parse(s: &str) -> Tab {
        match s {
            "general" => Tab::General,
            "audio" => Tab::Audio,
            "input" => Tab::Input,
            "agents" => Tab::Agents,
            _ => Tab::Video,
        }
    }
}

/// Moonlight's streaming shortcut chord, as this platform writes it.
pub fn chord(key: &str) -> String {
    if cfg!(target_os = "macos") {
        format!("⌃⌥⇧{key}")
    } else {
        format!("Ctrl+Alt+Shift+{key}")
    }
}

pub fn mbps(kbps: u32) -> String {
    if kbps >= 10_000 || kbps.is_multiple_of(1000) {
        format!("{} Mbps", kbps / 1000)
    } else {
        format!("{:.1} Mbps", kbps as f64 / 1000.0)
    }
}

const MAX_MBPS: f32 = 150.0;

impl PingApp {
    pub fn render_settings(
        &mut self,
        tab: Tab,
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, subtitle, body): (&str, &str, AnyElement) = match tab {
            Tab::General => (
                "General",
                "Ping itself: its version, and how it hears of a newer one.",
                self.general(t, cx),
            ),
            Tab::Video => (
                "Video",
                "How streams look. Changes apply to the next stream.",
                self.video(t, window, cx),
            ),
            Tab::Audio => ("Audio", "What you hear while streaming.", self.audio(t, cx)),
            Tab::Input => (
                "Input",
                "The keyboard, the mouse and controllers.",
                self.input(t, cx),
            ),
            Tab::Agents => (
                "Agent setup",
                "Who does the thinking, how far a run may go, and where.",
                self.render_agent_setup(t, window, cx),
            ),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar([]))
            .child(page_body(
                "settings-page",
                pingpong_ui::Metrics::FORM,
                div()
                    .flex()
                    .flex_col()
                    .gap(px(22.0))
                    .child(pingpong_ui::page_header(
                        title,
                        Some(subtitle.into()),
                        None,
                        t,
                    ))
                    .child(body),
            ))
            .into_any_element()
    }

    /// A switch bound to one preference.
    #[allow(clippy::too_many_arguments)]
    fn toggle(
        &self,
        id: &'static str,
        label: &str,
        detail: impl Into<SharedString>,
        get: fn(&Prefs) -> bool,
        set: fn(&mut Prefs, bool),
        enabled: bool,
        t: Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let this = cx.weak_entity();
        setting(
            label.to_string(),
            Some(detail.into()),
            switch(id, get(&self.prefs), t)
                .disabled(!enabled)
                .on_toggle(move |on, _, cx| {
                    let _ = this.update(cx, |app, cx| {
                        set(&mut app.prefs, on);
                        app.save_prefs(cx);
                    });
                }),
            t,
        )
        .when(!enabled, |d| d.opacity(0.55))
        .into_any_element()
    }

    /// Apply `f` to the preferences from a control's callback.
    fn prefs_setter<T: 'static>(
        &self,
        cx: &mut Context<Self>,
        f: impl Fn(&mut Prefs, T) + 'static,
    ) -> impl Fn(T, &mut Window, &mut App) + 'static {
        let this = cx.weak_entity();
        move |v, _, cx| {
            let _ = this.update(cx, |app, cx| {
                f(&mut app.prefs, v);
                app.save_prefs(cx);
            });
        }
    }

    fn general(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let check = cx.weak_entity();
        section("Updates", t)
            .child(rows(
                [
                    pingpong_ui::updates::channel_setting(
                        "update-channel",
                        &crate::app::UPDATE_APP,
                        self.prefs.update_channel(),
                        t,
                        self.prefs_setter(cx, |p, channel| p.updates = Some(channel)),
                    )
                    .into_any_element(),
                    pingpong_ui::updates::version_setting(
                        &crate::app::UPDATE_APP,
                        &self.update_status,
                        t,
                        move |_, cx| {
                            let _ = check.update(cx, |app, cx| app.show_updates(true, cx));
                        },
                    )
                    .into_any_element(),
                ],
                t,
            ))
            .into_any_element()
    }

    fn video(&mut self, t: Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let native = self.native(window, cx);
        let p = self.prefs.clone();

        // Resolution: this display, a Mac's scaled desktop, the named sizes,
        // and a custom size when one was set (imported from Moonlight).
        let mut values: Vec<String> = vec!["native".into()];
        let mut choices = vec![Choice::new(format!(
            "This display ({} × {})",
            native.width, native.height
        ))];
        if let Some((dw, dh)) = native.desktop.filter(|(dw, _)| *dw != native.width) {
            values.push("desktop".into());
            choices.push(Choice::new(format!("Scaled desktop ({dw} × {dh})")));
        }
        for (value, name) in RESOLUTIONS {
            values.push(value.into());
            let (w, h) = parse_size(value).unwrap_or_default();
            choices.push(Choice::new(name).detail(format!("{w} × {h}")));
        }
        if !p.is_listed_resolution() {
            let (w, h) = parse_size(&p.resolution).unwrap_or_default();
            values.push(p.resolution.clone());
            choices.push(Choice::new(format!("Custom ({w} × {h})")));
        }
        let res_index = values.iter().position(|v| *v == p.resolution);
        let resolution = select("resolution", choices, res_index, t)
            .width(230.0)
            .on_select(self.prefs_setter(cx, move |p, i: usize| p.resolution = values[i].clone()));

        let mut fps_values = vec![0u32];
        let mut fps_choices = vec![Choice::new(format!(
            "This display ({} FPS)",
            native.max_fps
        ))];
        for f in FRAME_RATES
            .into_iter()
            .filter(|f| *f <= native.max_fps.max(60))
        {
            fps_values.push(f);
            fps_choices.push(Choice::new(format!("{f} FPS")));
        }
        let fps_index = fps_values.iter().position(|v| *v == p.fps);
        let fps = select("fps", fps_choices, fps_index, t)
            .width(230.0)
            .on_select(self.prefs_setter(cx, move |p, i: usize| p.fps = fps_values[i]));

        let default_kbps = p.default_bitrate_kbps(&native);
        let (w, h, f) = p.mode(&native);
        let shown = if p.auto_bitrate {
            default_kbps
        } else {
            p.bitrate_kbps
        };
        let bitrate = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                slider(
                    "bitrate",
                    (shown as f32 / 1000.0 - 1.0) / (MAX_MBPS - 1.0),
                    170.0,
                    t,
                )
                .disabled(p.auto_bitrate)
                .on_change(self.prefs_setter(cx, |p, v: f32| {
                    p.bitrate_kbps = ((1.0 + v * (MAX_MBPS - 1.0)).round() as u32) * 1000;
                })),
            )
            .child(
                div()
                    .w(px(64.0))
                    .text_size(px(12.0))
                    .text_color(if p.auto_bitrate {
                        t.tertiary
                    } else {
                        t.primary
                    })
                    .child(mbps(shown)),
            );

        let codecs = ["auto", "hevc", "h264"];
        let codec = select(
            "codec",
            ["Automatic", "HEVC (H.265)", "H.264"],
            codecs.iter().position(|c| *c == p.codec),
            t,
        )
        .width(230.0)
        .on_select(self.prefs_setter(cx, move |p, i: usize| p.codec = codecs[i].into()));
        let display = pingpong_ui::segmented(
            "display-mode",
            ["Full screen", "Window"],
            if p.fullscreen { 0 } else { 1 },
            t,
        )
        .on_select(self.prefs_setter(cx, |p, i: usize| p.fullscreen = i == 0));

        let imported = self.imported.clone();
        div()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(
                section("Picture", t).child(rows(
                    [
                        setting(
                            "Resolution",
                            Some(
                                "The size the host streams at. This display \
                                    matches your screen pixel for pixel."
                                    .into(),
                            ),
                            resolution,
                            t,
                        )
                        .into_any_element(),
                        setting(
                            "Frame rate",
                            Some("Frames per second the host sends.".into()),
                            fps,
                            t,
                        )
                        .into_any_element(),
                        setting(
                            "Video codec",
                            Some(
                                "Automatic uses HEVC when the host offers \
                                    it: sharper at the same bitrate."
                                    .into(),
                            ),
                            codec,
                            t,
                        )
                        .into_any_element(),
                        setting(
                            "Display mode",
                            Some("Where the stream opens.".into()),
                            display,
                            t,
                        )
                        .into_any_element(),
                    ],
                    t,
                )),
            )
            .child(
                section("Bitrate", t).child(rows(
                    [
                        self.toggle(
                            "auto-bitrate",
                            "Automatic",
                            format!(
                                "{} for {w} × {h} at {f} FPS, as Moonlight picks it.",
                                mbps(default_kbps)
                            ),
                            |p| p.auto_bitrate,
                            |p, v| p.auto_bitrate = v,
                            true,
                            t,
                            cx,
                        ),
                        setting(
                            "Bitrate",
                            Some(
                                "More is sharper in motion, if the network \
                                    carries it."
                                    .into(),
                            ),
                            bitrate,
                            t,
                        )
                        .when(p.auto_bitrate, |d| d.opacity(0.6))
                        .into_any_element(),
                    ],
                    t,
                )),
            )
            .child(section("Presentation", t).child(rows(
                [
                    self.toggle(
                        "vsync",
                        "V-Sync",
                        "Present frames on the display's refresh. Off lowers latency \
                            slightly and may tear.",
                        |p| p.vsync,
                        |p, v| p.vsync = v,
                        true,
                        t,
                        cx,
                    ),
                    self.toggle(
                        "frame-pacing",
                        "Frame pacing",
                        "One frame per refresh for smoother motion, at about half a \
                            refresh of latency.",
                        |p| p.frame_pacing,
                        |p, v| p.frame_pacing = v,
                        p.vsync,
                        t,
                        cx,
                    ),
                    self.toggle(
                        "stats",
                        "Performance statistics",
                        format!(
                            "An overlay with frame times and the network. Toggle while \
                                streaming with {}.",
                            chord("S")
                        ),
                        |p| p.show_stats,
                        |p, v| p.show_stats = v,
                        true,
                        t,
                        cx,
                    ),
                    self.toggle(
                        "warnings",
                        "Connection warnings",
                        "A note in the corner while the network loses frames or lags.",
                        |p| p.connection_warnings,
                        |p, v| p.connection_warnings = v,
                        true,
                        t,
                        cx,
                    ),
                ],
                t,
            )))
            .child(
                section("Moonlight", t).child(rows(
                    [setting(
                        "Import from Moonlight",
                        Some(
                            imported
                                .unwrap_or_else(|| {
                                    format!(
                                        "Take over the resolution, \
                                            frame rate, bitrate and the rest Moonlight uses on this {}.",
                                        crate::app::device_word()
                                    )
                                })
                                .into(),
                        ),
                        button("import", "Import", t).on_click(cx.listener(
                            |this, _, window, cx| {
                                let native = this.native(window, cx);
                                this.imported = Some(match this.prefs.import_moonlight(&native) {
                                    Some(done) if done.is_empty() => "Imported.".into(),
                                    Some(done) => format!("Imported: {}.", done.join(", ")),
                                    None => format!(
                                        "Moonlight has no settings on this {}.",
                                        crate::app::device_word()
                                    ),
                                });
                                this.save_prefs(cx);
                            },
                        )),
                        t,
                    )
                    .into_any_element()],
                    t,
                )),
            )
            .into_any_element()
    }

    fn audio(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let p = self.prefs.clone();
        let channels = [2u8, 6, 8];
        let layout = select(
            "channels",
            ["Stereo", "5.1 surround", "7.1 surround"],
            channels.iter().position(|c| *c == p.audio_channels),
            t,
        )
        .width(170.0)
        .disabled(!p.audio)
        .on_select(self.prefs_setter(cx, move |p, i: usize| p.audio_channels = channels[i]));
        let layout_detail = if cfg!(target_os = "macos") {
            "Surround reaches your speakers or headphones through macOS, which downmixes \
                or spatializes it."
        } else {
            "Surround reaches your speakers or headphones through the system, which \
                downmixes it if they have fewer channels."
        };
        div()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(
                section("", t).child(rows(
                    [
                        self.toggle(
                            "audio",
                            "Stream audio",
                            "Play the host's sound here.",
                            |p| p.audio,
                            |p, v| p.audio = v,
                            true,
                            t,
                            cx,
                        ),
                        setting("Channels", Some(layout_detail.into()), layout, t)
                            .when(!p.audio, |d| d.opacity(0.55))
                            .into_any_element(),
                    ],
                    t,
                )),
            )
            .child(section("While streaming", t).child(rows(
                [
                    self.toggle(
                        "host-audio",
                        "Play on the host too",
                        "Off: the host's speakers stay quiet while you stream, as with Apollo.",
                        |p| p.host_audio,
                        |p, v| p.host_audio = v,
                        p.audio,
                        t,
                        cx,
                    ),
                    self.toggle(
                        "mute-background",
                        "Mute in the background",
                        "Silence the stream while another app is in front, as Moonlight's \
                            \"mute on focus loss\" does.",
                        |p| p.mute_in_background,
                        |p, v| p.mute_in_background = v,
                        p.audio,
                        t,
                        cx,
                    ),
                ],
                t,
            )))
            .into_any_element()
    }

    fn input(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut first = Vec::new();
        if cfg!(target_os = "macos") {
            first.push(self.toggle(
                "cmd-win",
                "Use ⌘ as the Windows key",
                "Command reaches a Windows host as the Windows key; Control stays Control.",
                |p| p.cmd_is_win,
                |p, v| p.cmd_is_win = v,
                true,
                t,
                cx,
            ));
        }
        first.push(self.toggle(
            "share-clipboard",
            "Share the clipboard",
            "Copy on this computer, paste on the host, and back: text, images, files and \
                folders. The host can turn it off. Password managers' copies stay here.",
            |p| p.share_clipboard,
            |p, v| p.share_clipboard = v,
            true,
            t,
            cx,
        ));
        first.push(self.toggle(
            "gamepad-mouse",
            "Controller as a mouse",
            "Hold Start (Menu) for a second: the left stick moves the pointer, the right \
                one scrolls, A, B and X click. Hold again to switch back.",
            |p| p.gamepad_mouse,
            |p, v| p.gamepad_mouse = v,
            true,
            t,
            cx,
        ));
        let shortcuts = [
            ("Q", "Stop streaming"),
            ("S", "Show or hide statistics"),
            ("Z", "Release or capture the mouse and keyboard"),
            ("M", "Switch the mouse between game and desktop mode"),
            ("D", "Minimize the stream"),
            ("X", "Switch between full screen and a window"),
            ("V", "Type the clipboard on the host"),
            ("T", "Take over from an agent you watch, and hand back"),
        ];
        div()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(section("", t).child(rows(first, t)))
            .child(section("Shortcuts while streaming", t).child(rows(
                shortcuts.into_iter().map(|(key, what)| {
                    div()
                        .min_h(px(38.0))
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(Type::BODY))
                                .text_color(t.primary.alpha(0.88))
                                .child(what),
                        )
                        .child(pingpong_ui::kbd(chord(key), t))
                        .into_any_element()
                }),
                t,
            )))
            .into_any_element()
    }
}
