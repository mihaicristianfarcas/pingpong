//! The Xbox page: signing in with a Microsoft account, the account's
//! consoles (streamed from, woken, turned off), and the games it may play
//! in Xbox Cloud Gaming. The services answer in a second or so: each list
//! is fetched on a thread of its own and shown when it arrives; the page
//! says it is looking meanwhile.
//!
//! Signing in is Microsoft's device code: a sheet shows the code, the user
//! enters it at microsoft.com/link on any device, and the sheet closes by
//! itself when they have.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gpui::{div, prelude::*, px, AnyElement, Context, Entity, FontWeight, SharedString, Window};
use ping_core::xbox::account::{self, AuthError, CloudLibrary, Command, Console, DeviceCode};
use ping_core::xbox::Target;
use pingpong_ui::{
    button, icon_button, rows, section, setting, spinner, IconName, Ink, Radius, TextField, Theme,
};

use crate::app::{page_body, sheet_buttons, sheet_text, sheet_title, toolbar, PingApp};

/// Games listed before the rest is left to the search field: a Game Pass
/// catalogue has hundreds.
const GAMES_SHOWN: usize = 60;

/// Something fetched on a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetch<T> {
    Idle,
    Loading,
    Done(T),
    Failed(String),
}

/// Signing in, while the sheet is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignInState {
    /// Asking Microsoft for a code.
    Starting,
    /// The code is shown; waiting for the user to enter it.
    Code(DeviceCode),
    Failed(String),
}

/// What the threads hand back.
#[derive(Default)]
struct Arrivals {
    consoles: Option<Result<Vec<Console>, AuthError>>,
    cloud: Option<Result<Option<CloudLibrary>, AuthError>>,
    sign_in: Option<SignInState>,
    signed_in: Option<String>,
    command: Option<Result<(), AuthError>>,
}

pub struct XboxState {
    /// The signed-in account's gamertag.
    pub gamertag: Option<String>,
    pub sign_in: Option<SignInState>,
    sign_in_cancel: Arc<AtomicBool>,
    pub consoles: Fetch<Vec<Console>>,
    pub cloud: Fetch<Option<CloudLibrary>>,
    /// The games' search field, once the page has been drawn.
    pub filter: Option<Entity<TextField>>,
    /// A command's error, shown once.
    pub error: Option<String>,
    arrivals: Arc<Mutex<Arrivals>>,
    wake: Arc<dyn Fn() + Send + Sync>,
}

impl XboxState {
    /// `wake` tells the window there is news.
    pub fn new(wake: Arc<dyn Fn() + Send + Sync>) -> XboxState {
        XboxState {
            gamertag: account::signed_in(),
            sign_in: None,
            sign_in_cancel: Arc::new(AtomicBool::new(false)),
            consoles: Fetch::Idle,
            cloud: Fetch::Idle,
            filter: None,
            error: None,
            arrivals: Arc::default(),
            wake,
        }
    }

    /// Run `f` on a thread of its own; it hands its result over in `put`.
    fn spawn<T: Send + 'static>(
        &self,
        name: &str,
        f: impl FnOnce() -> T + Send + 'static,
        put: fn(&mut Arrivals, T),
    ) {
        let (arrivals, wake) = (self.arrivals.clone(), self.wake.clone());
        let _ = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let out = f();
                put(&mut lock(&arrivals), out);
                wake();
            });
    }

    /// Fetch the consoles and the cloud library, if signed in.
    pub fn refresh(&mut self) {
        if self.gamertag.is_none() {
            return;
        }
        self.consoles = Fetch::Loading;
        self.cloud = Fetch::Loading;
        self.spawn("xbox-consoles", account::consoles, |a, r| {
            a.consoles = Some(r)
        });
        self.spawn(
            "xbox-cloud",
            || account::cloud_library(true),
            |a, r| a.cloud = Some(r),
        );
    }

    /// The page was opened: fetch what has not been.
    pub fn opened(&mut self) {
        if self.consoles == Fetch::Idle {
            self.refresh();
        }
    }

    pub fn start_sign_in(&mut self) {
        tracing::info!("signing in to Xbox");
        self.sign_in = Some(SignInState::Starting);
        let cancel = Arc::new(AtomicBool::new(false));
        self.sign_in_cancel = cancel.clone();
        let (arrivals, wake) = (self.arrivals.clone(), self.wake.clone());
        let _ = std::thread::Builder::new()
            .name("xbox-sign-in".into())
            .spawn(move || {
                let code = match account::start_sign_in() {
                    Ok(c) => c,
                    Err(e) => {
                        lock(&arrivals).sign_in = Some(SignInState::Failed(e.to_string()));
                        wake();
                        return;
                    }
                };
                lock(&arrivals).sign_in = Some(SignInState::Code(code.clone()));
                wake();
                match account::finish_sign_in(&code, &cancel) {
                    Ok(gamertag) => lock(&arrivals).signed_in = Some(gamertag),
                    Err(_) if cancel.load(Ordering::Relaxed) => {}
                    Err(e) => lock(&arrivals).sign_in = Some(SignInState::Failed(e.to_string())),
                }
                wake();
            });
    }

    pub fn cancel_sign_in(&mut self) {
        self.sign_in_cancel.store(true, Ordering::Relaxed);
        self.sign_in = None;
    }

    pub fn sign_out(&mut self) {
        tracing::info!("signing out of Xbox");
        if let Err(e) = account::sign_out() {
            self.error = Some(e);
            return;
        }
        self.gamertag = None;
        self.consoles = Fetch::Idle;
        self.cloud = Fetch::Idle;
    }

    /// Wake a console, or turn it off; the list is fetched again after.
    pub fn command(&mut self, id: String, command: Command) {
        tracing::info!(?command, "console command");
        self.spawn(
            "xbox-command",
            move || account::command(&id, command),
            |a, r| a.command = Some(r),
        );
    }

    fn failed(&mut self, e: AuthError) -> String {
        if e == AuthError::SignedOut {
            self.gamertag = None;
        }
        e.to_string()
    }

    /// Take in what the threads found. True if anything changed.
    pub fn update(&mut self) -> bool {
        let got = std::mem::take(&mut *lock(&self.arrivals));
        let mut changed = false;
        if let Some(r) = got.consoles {
            self.consoles = match r {
                Ok(list) => Fetch::Done(list),
                Err(e) => Fetch::Failed(self.failed(e)),
            };
            changed = true;
        }
        if let Some(r) = got.cloud {
            self.cloud = match r {
                Ok(library) => Fetch::Done(library),
                Err(e) => Fetch::Failed(self.failed(e)),
            };
            changed = true;
        }
        if let Some(state) = got.sign_in {
            if self.sign_in.is_some() {
                self.sign_in = Some(state);
                changed = true;
            }
        }
        if let Some(gamertag) = got.signed_in {
            tracing::info!("signed in to Xbox");
            self.gamertag = Some(gamertag);
            self.sign_in = None;
            self.refresh();
            changed = true;
        }
        if let Some(r) = got.command {
            if let Err(e) = r {
                self.error = Some(self.failed(e));
            }
            // A woken console takes a moment to say it is on.
            self.refresh();
            changed = true;
        }
        changed
    }
}

impl PingApp {
    pub fn render_xbox(
        &mut self,
        t: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.xbox.opened();
        let bar = toolbar(self.xbox.gamertag.is_some().then(|| {
            icon_button("xbox-refresh", IconName::Refresh, "Look again", t)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.xbox.refresh();
                    cx.notify();
                }))
                .into_any_element()
        }));
        let subtitle: SharedString = match &self.xbox.gamertag {
            Some(gt) => format!("Signed in as {gt}.").into(),
            None => "Your Xbox consoles, and Xbox Cloud Gaming, in a Ping window.".into(),
        };
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(22.0))
            .child(pingpong_ui::page_header("Xbox", Some(subtitle), None, t));
        if let Some(e) = self.xbox.error.clone() {
            body = body.child(pingpong_ui::notice(IconName::Warning, Ink::ATTENTION, e, t));
        }
        body = match self.xbox.gamertag.clone() {
            None => body.child(section("Microsoft account", t).child(rows(
                [setting(
                    "Sign in",
                    Some(
                        "With the account your Xbox uses, at microsoft.com/link on any device: \
                         Ping never sees your password."
                            .into(),
                    ),
                    button("xbox-sign-in", "Sign In", t).solid().on_click(cx.listener(
                        |this, _, _, cx| {
                            this.xbox.start_sign_in();
                            cx.notify();
                        },
                    )),
                    t,
                )
                .into_any_element()],
                t,
            ))),
            Some(gamertag) => body
                .child(section("Consoles", t).child(self.console_rows(t, cx)))
                .child(section("Cloud gaming", t).child(self.cloud_rows(t, window, cx)))
                .child(section("Account", t).child(rows(
                    [setting(
                        format!("Signed in as {gamertag}"),
                        Some(
                            "The sign-in is kept in Ping's data folder, readable only by you. \
                             Signing out forgets it."
                                .into(),
                        ),
                        button("xbox-sign-out", "Sign Out", t).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.xbox.sign_out();
                                cx.notify();
                            },
                        )),
                        t,
                    )
                    .into_any_element()],
                    t,
                ))),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar)
            .child(page_body("xbox-page", pingpong_ui::Metrics::FORM, body))
            .into_any_element()
    }

    fn console_rows(&mut self, t: Theme, cx: &mut Context<Self>) -> AnyElement {
        let row = |label: SharedString, detail: Option<SharedString>, control: AnyElement| {
            setting(label, detail, control, t).into_any_element()
        };
        match self.xbox.consoles.clone() {
            Fetch::Idle | Fetch::Loading => rows(
                [row(
                    "Looking for your consoles…".into(),
                    None,
                    spinner("xbox-consoles-spin", 13.0, t.tertiary).into_any_element(),
                )],
                t,
            )
            .into_any_element(),
            Fetch::Failed(e) => rows(
                [row(
                    "The consoles could not be listed".into(),
                    Some(e.into()),
                    div().into_any_element(),
                )],
                t,
            )
            .into_any_element(),
            Fetch::Done(list) if list.is_empty() => rows(
                [row(
                    "No consoles on this account".into(),
                    Some(
                        "Sign in on the console with this account, and turn on Settings > \
                         Devices & connections > Remote features."
                            .into(),
                    ),
                    div().into_any_element(),
                )],
                t,
            )
            .into_any_element(),
            Fetch::Done(list) => rows(
                list.into_iter().map(|c| {
                    let mut detail = format!("{} · {}", c.model(), c.state());
                    if let Some(why) = c.cannot_stream() {
                        detail = format!("{detail}. {why}");
                    } else if c.is_asleep() {
                        detail.push_str(". Streaming wakes it.");
                    }
                    let (id, name) = (c.id.clone(), c.name.clone());
                    let power = {
                        let id = c.id.clone();
                        let on = c.is_on();
                        button(
                            SharedString::from(format!("xbox-power-{}", c.id)),
                            if on { "Turn Off" } else { "Turn On" },
                            t,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let command = if on {
                                Command::TurnOff
                            } else {
                                Command::WakeUp
                            };
                            this.xbox.command(id.clone(), command);
                            cx.notify();
                        }))
                    };
                    let stream = button(
                        SharedString::from(format!("xbox-stream-{}", c.id)),
                        "Stream",
                        t,
                    )
                    .solid()
                    .icon(IconName::Play)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.stream_xbox(
                            Target::Console {
                                id: id.clone(),
                                name: name.clone(),
                            },
                            window,
                            cx,
                        )
                    }));
                    row(
                        c.name.clone().into(),
                        Some(detail.into()),
                        div()
                            .flex()
                            .gap(px(8.0))
                            .when(c.remote_management_enabled, |d| d.child(power))
                            .child(stream)
                            .into_any_element(),
                    )
                }),
                t,
            )
            .into_any_element(),
        }
    }

    fn cloud_rows(&mut self, t: Theme, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let row = |label: SharedString, detail: Option<SharedString>, control: AnyElement| {
            setting(label, detail, control, t).into_any_element()
        };
        let library = match self.xbox.cloud.clone() {
            Fetch::Idle | Fetch::Loading => {
                return rows(
                    [row(
                        "Looking for your cloud games…".into(),
                        None,
                        spinner("xbox-cloud-spin", 13.0, t.tertiary).into_any_element(),
                    )],
                    t,
                )
                .into_any_element()
            }
            Fetch::Failed(e) => {
                return rows([row("The cloud games could not be listed".into(), Some(e.into()), div().into_any_element())], t)
                    .into_any_element()
            }
            Fetch::Done(None) => {
                return rows(
                    [row(
                        "Xbox Cloud Gaming is not offered to this account here".into(),
                        Some("It is offered in some countries, with Game Pass Ultimate or for free-to-play games.".into()),
                        div().into_any_element(),
                    )],
                    t,
                )
                .into_any_element()
            }
            Fetch::Done(Some(library)) => library,
        };
        let filter = self
            .xbox
            .filter
            .get_or_insert_with(|| {
                let field = cx.new(|cx| TextField::new(cx).placeholder("Find a game"));
                cx.subscribe_in(&field, window, |_, _, _, _, cx| cx.notify())
                    .detach();
                field
            })
            .clone();
        let wanted = filter.read(cx).text().trim().to_lowercase();
        let matching: Vec<_> = library
            .games
            .iter()
            .filter(|g| wanted.is_empty() || g.name.to_lowercase().contains(&wanted))
            .cloned()
            .collect();
        let more = matching.len().saturating_sub(GAMES_SHOWN);
        let mut list: Vec<AnyElement> = vec![setting(
            if library.game_pass {
                "Game Pass Ultimate"
            } else {
                "Free-to-play games"
            },
            Some(
                match library.games.len() {
                    1 => "One game to play on Microsoft's servers: no console needed.".to_string(),
                    n => format!("{n} games to play on Microsoft's servers: no console needed."),
                }
                .into(),
            ),
            div().w(px(200.0)).child(pingpong_ui::field(&filter, t)),
            t,
        )
        .into_any_element()];
        list.extend(matching.into_iter().take(GAMES_SHOWN).map(|g| {
            let (title_id, name) = (g.title_id.clone(), g.name.clone());
            row(
                g.name.into(),
                (!g.publisher.is_empty()).then(|| g.publisher.into()),
                button(
                    SharedString::from(format!("xbox-play-{}", g.title_id)),
                    "Play",
                    t,
                )
                .icon(IconName::Play)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.stream_xbox(
                        Target::Cloud {
                            title_id: title_id.clone(),
                            name: name.clone(),
                        },
                        window,
                        cx,
                    )
                }))
                .into_any_element(),
            )
        }));
        let mut out = div().flex().flex_col().gap(px(6.0)).child(rows(list, t));
        if more > 0 {
            out = out.child(pingpong_ui::footnote(
                format!("{more} more: type in Find a game."),
                t,
            ));
        }
        out.into_any_element()
    }

    /// The sign-in sheet, while signing in.
    pub fn xbox_sheet(&mut self, t: Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.xbox.sign_in.clone()?;
        let cancel =
            button("xbox-sign-in-cancel", "Cancel", t).on_click(cx.listener(|this, _, _, cx| {
                this.xbox.cancel_sign_in();
                cx.notify();
            }));
        let body =
            match state {
                SignInState::Starting => div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(sheet_title("Sign in to Xbox", t))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(7.0))
                            .text_size(px(12.0))
                            .text_color(t.secondary)
                            .child(spinner("xbox-sign-in-spin", 12.0, t.tertiary))
                            .child("Asking Microsoft for a code…"),
                    )
                    .child(sheet_buttons().child(cancel)),
                SignInState::Code(code) => {
                    let url = code.verification_uri.clone();
                    let copy = code.user_code.clone();
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(sheet_title("Sign in to Xbox", t))
                        .child(sheet_text(
                            format!(
                                "On any device, open {} and enter this code. Sign in with the \
                             account your Xbox uses.",
                                url.trim_start_matches("https://")
                                    .trim_start_matches("www.")
                            ),
                            t,
                        ))
                        .child(code_boxes(&code.user_code, t))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(7.0))
                                .text_size(px(12.0))
                                .text_color(t.secondary)
                                .child(spinner("xbox-sign-in-wait", 12.0, t.tertiary))
                                .child("Waiting for you to sign in…"),
                        )
                        .child(
                            sheet_buttons()
                                .child(cancel)
                                .child(button("xbox-copy-code", "Copy Code", t).on_click(
                                    cx.listener(move |_, _, _, cx| {
                                        cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                            copy.clone(),
                                        ))
                                    }),
                                ))
                                .child(
                                    button("xbox-open-link", "Open the Page", t)
                                        .solid()
                                        .icon(IconName::ExternalLink)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.open_url(&url, cx)
                                        })),
                                ),
                        )
                }
                SignInState::Failed(e) => div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(sheet_title("Signing in didn't work", t))
                    .child(sheet_text(e, t))
                    .child(sheet_buttons().child(
                        button("xbox-sign-in-close", "Close", t).on_click(cx.listener(
                            |this, _, _, cx| {
                                this.xbox.cancel_sign_in();
                                cx.notify();
                            },
                        )),
                    )),
            };
        Some(pingpong_ui::sheet("xbox-sign-in-sheet", 440.0, t, body).into_any_element())
    }
}

/// The arrivals, whatever a thread that panicked holding them left: each
/// field is whole on its own.
fn lock(arrivals: &Mutex<Arrivals>) -> std::sync::MutexGuard<'_, Arrivals> {
    arrivals.lock().unwrap_or_else(|e| e.into_inner())
}

/// The code, one box per character, as the pairing PIN is shown.
fn code_boxes(code: &str, t: Theme) -> impl IntoElement {
    div()
        .py(px(4.0))
        .flex()
        .gap(px(6.0))
        .children(code.chars().filter(|c| !c.is_whitespace()).map(move |c| {
            div()
                .w(px(38.0))
                .h(px(50.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(Radius::CARD))
                .bg(t.primary.alpha(0.06))
                .border_1()
                .border_color(t.card_stroke)
                .text_size(px(26.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(t.primary)
                .child(c.to_string())
        }))
}
