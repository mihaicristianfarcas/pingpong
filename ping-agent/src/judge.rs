//! A second look at what an agent is about to do, by a model that sees the
//! screen: Cloudflare's clef (`@cf/cloudflare/clef` on Workers AI). Clef is
//! a decision model: it reads a picture and typed questions (TypeSafe's
//! System One API: `noul`, `choice`, `score`, with images added) and returns
//! a probability for each answer. It writes no text and takes no actions, so
//! it judges the driving model's steps and never replaces it.
//!
//! Three checks, each one the person turns on (Agent setup; `checks` in the
//! agent's settings):
//!
//! - **clicks**: before a left click, a drag's drop or Enter, the screen
//!   with a ring where it lands: does it delete, spend, send, change
//!   settings or install? `risk`'s rules judge typed text and chords but no
//!   click, since only something that sees the screen knows what is under
//!   the pointer. A likely yes waits for the person's go-ahead, as a rule's
//!   catch does.
//! - **secret fields**: before typing, is the focus in a field for a
//!   password, PIN or key? Clef is told how many characters, never which.
//! - **personal information**: before a screen goes to the driving model,
//!   does it show credentials, payment details, ID numbers, health records,
//!   private messages or people's addresses? The person decides whether the
//!   model sees such a screen; on a no it gets a blank one.
//!
//! The checks only add holds: a click clef thinks harmless still meets the
//! rules and the model's own `ask_approval`. When clef cannot be reached,
//! clicks and typing go on as they would without it, and a screen goes to
//! the model only if the person allows it.
//!
//! Every check sends the screen to Cloudflare (docs/ai-agents.md says so),
//! at most `MAX_SIDE` pixels on its longer side.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use pingpong_proto::input::{scancode, Key};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::computer::{Action, Mouse, Shot};
use crate::frame::Rgb;

/// Which checks clef makes, and the account it runs on (part of
/// `AgentSettings`; the token is saved with the API keys, as `TOKEN_ID`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Checks {
    /// Left clicks, a drag's drop and Enter.
    pub clicks: bool,
    /// Typing into a password, PIN or key field.
    pub secret_fields: bool,
    /// Every screen, before the model sees it.
    pub personal_info: bool,
    /// One of `MODELS`.
    pub model: String,
    /// The Cloudflare account whose Workers AI runs clef: an ID, not a
    /// secret.
    pub account_id: String,
}

impl Default for Checks {
    fn default() -> Self {
        Checks {
            clicks: false,
            secret_fields: false,
            personal_info: false,
            model: MODELS[0].to_string(),
            account_id: String::new(),
        }
    }
}

impl Checks {
    pub fn any(&self) -> bool {
        self.clicks || self.secret_fields || self.personal_info
    }
}

/// The models, as Workers AI names them after `@cf/cloudflare/`; the first
/// is the default. Clef (27B) leads clef-flash (9B) where most clicks fall:
/// telling what fits none of the options (CLINC150+OOS, 97.4 against 66.8
/// in Cloudflare's own run of its Decision Index). Flash is faster (38.8
/// against 209.3 ms median there), but an agent acts seconds apart.
pub const MODELS: [&str; 2] = ["clef", "clef-flash"];

/// The token's environment variable (the name Cloudflare's clef examples
/// use; wrangler's `CLOUDFLARE_API_TOKEN` is often a token for something
/// else).
pub const TOKEN_ENV: &str = "CLOUDFLARE_AUTH_TOKEN";
/// The account's, when it is not in the settings.
pub const ACCOUNT_ENV: &str = "CLOUDFLARE_ACCOUNT_ID";
/// The token's name among the saved keys (`providers::secrets`).
pub const TOKEN_ID: &str = "cloudflare";

const API: &str = "https://api.cloudflare.com/client/v4";

/// Clef's probability from which a check holds a step. Even odds: a hold
/// costs the person one question, a miss a deleted file or a card number
/// shown. Not yet measured on screens; `ping-agent check` measures it on
/// your own.
pub const HOLD_AT: f64 = 0.5;

/// Screens are sent at most this many pixels on the longer side: 1280 x 800
/// is the agent's display by default, and a larger one costs clef more
/// tokens for text it reads well enough at this size.
const MAX_SIDE: u32 = 1280;
/// Clef takes images of at most 4 MiB.
const MAX_IMAGE: usize = 4 << 20;
/// A check that takes longer is given up (clef answers in 239 ms at p95 in
/// Cloudflare's run; the rest is the network).
const TIMEOUT: Duration = Duration::from_secs(10);

/// The ring that shows clef where a click lands: magenta, which few user
/// interfaces use, round a short label such as a dialog's "Delete" (about
/// 55 pixels wide at `MAX_SIDE`) so the label stays readable, and a dot at
/// the exact point. A 14-pixel ring sat on that label's letters.
const RING: [u8; 3] = [255, 0, 255];
const RING_RADIUS: u32 = 32;
const RING_WIDTH: u32 = 3;
const DOT_RADIUS: u32 = 2;

/// What a click or Enter may do that waits for a yes, as options of one
/// question: (id, what clef is told, what the person is told).
const EFFECTS: [(&str, &str, &str); 6] = [
    (
        "deletes",
        "Deletes, removes, empties or overwrites files, messages, accounts or data",
        "deletes something",
    ),
    (
        "spends",
        "Buys, pays, orders, bids, donates or subscribes",
        "spends money",
    ),
    (
        "sends",
        "Sends, posts, publishes, shares or submits something on the person's behalf",
        "sends or posts something",
    ),
    (
        "settings",
        "Changes account, security, privacy or system settings, or grants a permission",
        "changes settings",
    ),
    (
        "installs",
        "Installs, updates or runs software",
        "installs or runs software",
    ),
    (
        "none",
        "None of these: opens, reads, navigates, selects, scrolls, focuses a field, or cancels",
        "",
    ),
];

/// Personal information clef looks for on a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Personal {
    Credentials,
    Payment,
    Identity,
    Health,
    Messages,
    Contacts,
    /// Clef could not look: the screen counts as showing something.
    Unchecked,
}

impl Personal {
    const LOOKED_FOR: [Personal; 6] = [
        Personal::Credentials,
        Personal::Payment,
        Personal::Identity,
        Personal::Health,
        Personal::Messages,
        Personal::Contacts,
    ];

    fn id(self) -> &'static str {
        match self {
            Personal::Credentials => "credentials",
            Personal::Payment => "payment",
            Personal::Identity => "identity",
            Personal::Health => "health",
            Personal::Messages => "messages",
            Personal::Contacts => "contacts",
            Personal::Unchecked => "unchecked",
        }
    }

    fn question(self) -> &'static str {
        match self {
            Personal::Credentials => {
                "Does the screen show a password, recovery code, API key, token or private key \
                    in readable text?"
            }
            Personal::Payment => {
                "Does the screen show a payment card number, a bank account or routing number, \
                    or a person's balances or transactions?"
            }
            Personal::Identity => {
                "Does the screen show a government ID number: passport, driving licence, \
                    national ID, tax or social security number?"
            }
            Personal::Health => {
                "Does the screen show medical, health or insurance records about a person?"
            }
            Personal::Messages => {
                "Does the screen show the content of private messages, chats or emails?"
            }
            Personal::Contacts => {
                "Does the screen show people's home addresses, personal phone numbers or dates \
                    of birth?"
            }
            Personal::Unchecked => "",
        }
    }

    /// For the person and the model: "payment or bank details".
    pub fn describe(self) -> &'static str {
        match self {
            Personal::Credentials => "passwords or keys",
            Personal::Payment => "payment or bank details",
            Personal::Identity => "ID numbers",
            Personal::Health => "health records",
            Personal::Messages => "private messages",
            Personal::Contacts => "people's addresses or phone numbers",
            Personal::Unchecked => "what clef could not check",
        }
    }
}

/// "payment or bank details and private messages".
pub fn describe_all(kinds: &[Personal]) -> String {
    let words: Vec<&str> = kinds.iter().map(|k| k.describe()).collect();
    match words.split_last() {
        None => String::new(),
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}

/// What a screen showing `found` comes to, given what the person said this
/// turn about each kind (true: the model may see it).
#[derive(Debug, PartialEq)]
pub enum Showing {
    Show,
    /// The person said no to these before.
    Withhold(Vec<Personal>),
    /// The person has not been asked about these.
    Ask(Vec<Personal>),
}

pub fn showing(found: &[Personal], answered: &BTreeMap<Personal, bool>) -> Showing {
    let refused: Vec<Personal> = found
        .iter()
        .copied()
        .filter(|k| answered.get(k) == Some(&false))
        .collect();
    if !refused.is_empty() {
        return Showing::Withhold(refused);
    }
    let open: Vec<Personal> = found
        .iter()
        .copied()
        .filter(|k| !answered.contains_key(k))
        .collect();
    if open.is_empty() {
        Showing::Show
    } else {
        Showing::Ask(open)
    }
}

/// An action clef looks at before it is done.
#[derive(Debug, Clone, PartialEq)]
pub enum Subject {
    /// A click (or a drop) at a point of the screen: the ring's centre.
    Click { at: (u32, u32), doing: &'static str },
    /// Enter: it acts on what has the keyboard focus.
    Enter,
    /// Typing this many characters where the focus is.
    Typing { chars: usize },
}

impl Subject {
    /// What clef would look at in `action` (`pointer`: where the pointer
    /// is), if a check in `checks` covers it.
    pub fn of(action: &Action, pointer: Option<(u32, u32)>, checks: &Checks) -> Option<Subject> {
        match action {
            Action::Click {
                at,
                button: Mouse::Left,
                count,
                ..
            } if checks.clicks => Some(Subject::Click {
                at: at.or(pointer)?,
                doing: match count {
                    2 => "double-click",
                    3 => "triple-click",
                    _ => "click",
                },
            }),
            Action::Drag { path, .. } if checks.clicks => Some(Subject::Click {
                at: *path.last()?,
                doing: "drag something and drop it",
            }),
            // A press and a release made apart are a click all the same.
            Action::MouseUp {
                button: Mouse::Left,
            } if checks.clicks => Some(Subject::Click {
                at: pointer?,
                doing: "let go of the mouse button (ending a click or a drag)",
            }),
            Action::Key { keys, .. } if checks.clicks && presses_enter(keys) => {
                Some(Subject::Enter)
            }
            Action::Type { text } if checks.secret_fields && !text.trim().is_empty() => {
                Some(Subject::Typing {
                    chars: text.chars().count(),
                })
            }
            _ => None,
        }
    }
}

/// Whether the chord ends in Enter (with or without modifiers: Ctrl+Enter
/// sends in many mail and chat apps).
fn presses_enter(keys: &str) -> bool {
    crate::keys::parse(keys)
        .ok()
        .and_then(|c| c.keys.last().copied())
        .is_some_and(|k| k == scancode(Key::Enter) || k == scancode(Key::NumpadEnter))
}

const SCREEN: &str = "The image is the screen of a computer that an AI agent operates.";

/// The question for a click at the ring (`doing`), or for Enter (`None`).
pub fn click_request(model: &str, image: String, doing: Option<&str>) -> Value {
    let state = match doing {
        Some(doing) => {
            format!(
                "{SCREEN} The agent is about to {doing} at the magenta dot, in the middle of \
                    the magenta ring."
            )
        }
        None => format!(
            "{SCREEN} The agent is about to press Enter, which acts on what has the keyboard \
                focus: a dialog's default button, the focused button or link, or the form being \
                filled in."
        ),
    };
    let criteria: Map<String, Value> = EFFECTS
        .iter()
        .map(|(id, option, _)| (id.to_string(), json!(option)))
        .collect();
    json!({
        "model": model,
        "state": state,
        "images": [image],
        "questions": {"effect": {
            "type": "choice",
            "instructions": "What does this do, once done?",
            "criteria": criteria,
        }},
    })
}

/// The question for typing `chars` characters where the focus is.
pub fn typing_request(model: &str, image: String, chars: usize) -> Value {
    json!({
        "model": model,
        "state": format!(
            "{SCREEN} The agent is about to type {chars} characters where the keyboard focus is."
        ),
        "images": [image],
        "questions": {"secret": {
            "type": "noul",
            "instructions": "Is the keyboard focus in a field for a password, PIN, passphrase, \
                recovery code, API key or token, or a card's number or security code, or in a \
                terminal asking for a password?",
            "criteria": {
                "true": "A field for a secret: what is typed there is a password, a key or a card number",
                "false": "An ordinary place to type: a search box, an address bar, a document, a \
                    message, a terminal's command line, or no field at all",
            },
        }},
    })
}

/// The questions for a screen about to go to the model.
pub fn personal_request(model: &str, image: String) -> Value {
    let questions: Map<String, Value> = Personal::LOOKED_FOR
        .iter()
        .map(|k| {
            (
                k.id().to_string(),
                json!({"type": "noul", "instructions": k.question()}),
            )
        })
        .collect();
    json!({
        "model": model,
        "state": format!(
            "{SCREEN} Before the agent's model is shown it, decide which personal information \
                it shows in a form someone could read."
        ),
        "images": [image],
        "questions": questions,
    })
}

/// Clef's answers in a reply (Workers AI wraps the model's output in
/// `result`).
pub fn answers(reply: &Value) -> Option<&Map<String, Value>> {
    reply
        .get("result")
        .unwrap_or(reply)
        .get("answers")?
        .as_object()
}

fn probability(v: Option<&Value>) -> Option<f64> {
    v?.as_f64().filter(|p| (0.0..=1.0).contains(p))
}

/// Why a click or Enter waits, from clef's answers: the likeliest of the
/// effects that wait, when those together reach `HOLD_AT`.
pub fn effect_verdict(answers: &Map<String, Value>) -> Result<Option<String>, String> {
    let probs = answers
        .get("effect")
        .and_then(|a| a.get("probabilities"))
        .and_then(Value::as_object)
        .ok_or("clef's answer has no probabilities for `effect`")?;
    let harmful: Vec<(&str, f64)> = EFFECTS
        .iter()
        .filter(|(id, ..)| *id != "none")
        .map(|(id, _, words)| (*words, probability(probs.get(*id)).unwrap_or(0.0)))
        .collect();
    let any = harmful.iter().map(|(_, p)| p).sum::<f64>().min(1.0);
    if any < HOLD_AT {
        return Ok(None);
    }
    let (words, _) = harmful
        .iter()
        .copied()
        .fold(("", -1.0), |best, h| if h.1 > best.1 { h } else { best });
    Ok(Some(format!(
        "Clef, looking at the screen: it likely {words} ({:.0}%).",
        any * 100.0
    )))
}

/// Why typing waits, from clef's answers.
pub fn typing_verdict(answers: &Map<String, Value>) -> Result<Option<String>, String> {
    let p = probability(answers.get("secret").and_then(|a| a.get("noul")))
        .ok_or("clef's answer has no probability for `secret`")?;
    Ok((p >= HOLD_AT).then(|| {
        format!(
            "Clef, looking at the screen: it types into a password, PIN or key field ({:.0}%).",
            p * 100.0
        )
    }))
}

/// The personal information clef finds, from its answers. An answer
/// missing is an error: a screen goes to the model only when every kind
/// was looked for.
pub fn personal_verdict(answers: &Map<String, Value>) -> Result<Vec<Personal>, String> {
    let mut found = Vec::new();
    for kind in Personal::LOOKED_FOR {
        let p = probability(answers.get(kind.id()).and_then(|a| a.get("noul")))
            .ok_or_else(|| format!("clef's answer has no probability for `{}`", kind.id()))?;
        if p >= HOLD_AT {
            found.push(kind);
        }
    }
    Ok(found)
}

/// `screen` as clef gets it: at most `MAX_SIDE` across, with the ring at
/// `ring` (a point of `screen`), as a PNG data URL.
pub fn picture(screen: &Rgb, ring: Option<(u32, u32)>) -> String {
    let mut fitted = screen.fit(MAX_SIDE);
    if let Some((x, y)) = ring {
        let scale = |v: u32, from: u32, to: u32| (v as u64 * to as u64 / from.max(1) as u64) as u32;
        let at = (
            scale(x, screen.width, fitted.width),
            scale(y, screen.height, fitted.height),
        );
        fitted.ring(at, RING_RADIUS, RING_WIDTH, RING);
        fitted.ring(at, 0, DOT_RADIUS, RING);
    }
    data_url(&fitted.png())
}

fn data_url(png: &[u8]) -> String {
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

/// Clef on Workers AI, for the checks the person turned on.
pub struct Judge {
    checks: Checks,
    model: &'static str,
    /// The run URL, or why there is none (no account set).
    url: Result<String, String>,
    token: Option<String>,
    http: ureq::Agent,
}

impl Judge {
    /// The judge for the checks in `checks`, or None when none is on. The
    /// account and token come from the environment, else `checks` and the
    /// saved keys; when one is missing, every check fails (and fails safe:
    /// see the module doc), saying what to set.
    pub fn new(data_dir: &Path, checks: &Checks) -> Option<Judge> {
        if !checks.any() {
            return None;
        }
        let model = MODELS
            .into_iter()
            .find(|m| *m == checks.model.trim())
            .unwrap_or(MODELS[0]);
        // The environment wins, as it does for every key.
        let account = std::env::var(ACCOUNT_ENV)
            .ok()
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| checks.account_id.clone());
        let account = Some(account.trim().to_string()).filter(|a| !a.is_empty());
        let url = match account {
            // An account ID is 32 hex digits: anything else would be a
            // different URL.
            Some(a) if a.chars().all(|c| c.is_ascii_alphanumeric()) => {
                Ok(format!("{API}/accounts/{a}/ai/run/@cf/cloudflare/{model}"))
            }
            Some(_) => Err("the Cloudflare account ID is malformed".to_string()),
            None => Err(format!(
                "no Cloudflare account ID (Agent setup, or {ACCOUNT_ENV})"
            )),
        };
        let token = crate::providers::secrets::named(data_dir, TOKEN_ID, Some(TOKEN_ENV));
        Some(Judge::with(checks.clone(), model, url, token))
    }

    pub(crate) fn with(
        checks: Checks,
        model: &'static str,
        url: Result<String, String>,
        token: Option<String>,
    ) -> Judge {
        Judge {
            checks,
            model,
            url,
            token,
            http: ureq::Agent::config_builder()
                .timeout_global(Some(TIMEOUT))
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }

    pub fn checks(&self) -> &Checks {
        &self.checks
    }

    pub fn model(&self) -> &'static str {
        self.model
    }

    /// Why `subject` should wait for the person's yes, if clef thinks it
    /// should; `screen` is the screen now.
    pub fn judge(&self, screen: &Rgb, subject: &Subject) -> Result<Option<String>, String> {
        let reply = self.ask(&self.request(screen, subject))?;
        let answers = answers(&reply).ok_or("clef's reply has no answers")?;
        match subject {
            Subject::Typing { .. } => typing_verdict(answers),
            _ => effect_verdict(answers),
        }
    }

    /// The question for `subject`.
    pub fn request(&self, screen: &Rgb, subject: &Subject) -> Value {
        match subject {
            Subject::Click { at, doing } => {
                click_request(self.model, picture(screen, Some(*at)), Some(doing))
            }
            Subject::Enter => click_request(self.model, picture(screen, None), None),
            Subject::Typing { chars } => typing_request(self.model, picture(screen, None), *chars),
        }
    }

    /// The personal information on `shot` (a screen as the model gets it).
    pub fn personal(&self, shot: &Shot) -> Result<Vec<Personal>, String> {
        let reply = self.ask(&self.personal_request(shot)?)?;
        personal_verdict(answers(&reply).ok_or("clef's reply has no answers")?)
    }

    pub fn personal_request(&self, shot: &Shot) -> Result<Value, String> {
        let png = if shot.width.max(shot.height) > MAX_SIDE {
            crate::computer::shrink_png(&shot.png, MAX_SIDE)
                .ok_or("the screen could not be made smaller for clef")?
                .png
        } else {
            shot.png.clone()
        };
        if png.len() > MAX_IMAGE {
            return Err(format!(
                "the screen is {} KiB as PNG, more than clef takes",
                png.len() / 1024
            ));
        }
        Ok(personal_request(self.model, data_url(&png)))
    }

    /// Send `body` to clef; its reply, or what went wrong.
    pub fn ask(&self, body: &Value) -> Result<Value, String> {
        let url = self.url.as_ref().map_err(|e| format!("clef: {e}"))?;
        let token = self.token.as_ref().ok_or_else(|| {
            format!("clef: no Cloudflare API token (Agent setup, `ping-agent set-key cloudflare`, or {TOKEN_ENV})")
        })?;
        let started = Instant::now();
        let mut resp = self
            .http
            .post(url)
            .header("authorization", format!("Bearer {token}"))
            .send_json(body)
            .map_err(|e| format!("clef: {e}"))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .with_config()
            .limit(1 << 20)
            .read_to_string()
            .map_err(|e| format!("clef: {e}"))?;
        let reply: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let input_tokens = reply
            .pointer("/result/usage/input_tokens")
            .and_then(|t| t.as_u64());
        tracing::trace!(
            status,
            ms = started.elapsed().as_millis() as u64,
            input_tokens,
            "clef answered"
        );
        if !(200..300).contains(&status) {
            // Cloudflare's API says what was wrong in `errors`.
            let why = reply
                .pointer("/errors/0/message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(format!("clef: {status}: {why}"));
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Rgb {
        Rgb {
            width: 2560,
            height: 1600,
            data: vec![255; 2560 * 1600 * 3],
        }
    }

    fn checks() -> Checks {
        Checks {
            clicks: true,
            secret_fields: true,
            personal_info: true,
            ..Checks::default()
        }
    }

    fn click(at: Option<(u32, u32)>, button: Mouse, count: u8) -> Action {
        Action::Click {
            at,
            button,
            count,
            modifiers: None,
        }
    }

    #[test]
    fn clicks_drops_enter_and_typing_are_looked_at_and_the_rest_is_not() {
        let c = checks();
        assert_eq!(
            Subject::of(&click(Some((5, 6)), Mouse::Left, 2), None, &c),
            Some(Subject::Click {
                at: (5, 6),
                doing: "double-click"
            })
        );
        // A click where the pointer is.
        assert_eq!(
            Subject::of(&click(None, Mouse::Left, 1), Some((7, 8)), &c),
            Some(Subject::Click {
                at: (7, 8),
                doing: "click"
            })
        );
        assert_eq!(Subject::of(&click(None, Mouse::Left, 1), None, &c), None);
        assert_eq!(
            Subject::of(&click(Some((1, 1)), Mouse::Right, 1), None, &c),
            None
        );
        let drag = Action::Drag {
            path: vec![(1, 1), (90, 40)],
            modifiers: None,
        };
        assert!(matches!(
            Subject::of(&drag, None, &c),
            Some(Subject::Click { at: (90, 40), .. })
        ));
        for keys in ["Return", "ctrl+Return", "KP_Enter", "ENTER"] {
            let key = Action::Key {
                keys: keys.into(),
                repeat: 1,
            };
            assert_eq!(Subject::of(&key, None, &c), Some(Subject::Enter), "{keys}");
        }
        let tab = Action::Key {
            keys: "Tab".into(),
            repeat: 1,
        };
        assert_eq!(Subject::of(&tab, None, &c), None);
        let typed = Action::Type {
            text: "hunter2".into(),
        };
        assert_eq!(
            Subject::of(&typed, None, &c),
            Some(Subject::Typing { chars: 7 })
        );
        // Each check only when it is on.
        let off = Checks::default();
        assert_eq!(Subject::of(&typed, None, &off), None);
        assert_eq!(
            Subject::of(&click(Some((5, 6)), Mouse::Left, 1), None, &off),
            None
        );
    }

    #[test]
    fn what_is_typed_never_reaches_clef() {
        let j = Judge::with(checks(), "clef", Err(String::new()), None);
        let body = j.request(&screen(), &Subject::Typing { chars: 7 });
        let text = body.to_string();
        assert!(text.contains("7 characters"));
        assert!(!text.contains("hunter2"));
        assert!(body["images"][0]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));
    }

    #[test]
    fn a_click_is_shown_with_a_ring_on_a_smaller_screen() {
        let body = click_request(
            "clef",
            picture(&screen(), Some((2000, 1000))),
            Some("click"),
        );
        let url = body["images"][0].as_str().unwrap();
        let png = base64::engine::general_purpose::STANDARD
            .decode(url.trim_start_matches("data:image/png;base64,"))
            .unwrap();
        let shot = crate::computer::shrink_png(&png, 4000).unwrap();
        assert_eq!((shot.width, shot.height), (1280, 800));
        // The ring is where the click lands on the smaller screen: (1000, 500).
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut buf).unwrap();
        let px = |x: usize, y: usize| &buf[(y * 1280 + x) * 3..(y * 1280 + x) * 3 + 3];
        assert_eq!(px(1000, 500), RING);
        assert_eq!(px(1000 + RING_RADIUS as usize / 2, 500), [255, 255, 255]);
        assert_eq!(px(1000 + RING_RADIUS as usize + 1, 500), RING);
        let options = body["questions"]["effect"]["criteria"].as_object().unwrap();
        assert_eq!(options.len(), EFFECTS.len());
        assert!(body["state"].as_str().unwrap().contains("magenta ring"));
    }

    fn choice(probabilities: Value) -> Map<String, Value> {
        json!({"effect": {"type": "choice", "choice": "none", "probabilities": probabilities, "confidence": 0.5}})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn a_click_waits_when_the_harmful_effects_together_are_likely() {
        let harmless = choice(json!({"none": 0.8, "deletes": 0.1, "sends": 0.1}));
        assert_eq!(effect_verdict(&harmless).unwrap(), None);
        // No single effect is likely, but together they are.
        let spread = choice(json!({"none": 0.4, "deletes": 0.25, "sends": 0.35}));
        assert_eq!(
            effect_verdict(&spread).unwrap().unwrap(),
            "Clef, looking at the screen: it likely sends or posts something (60%)."
        );
        // Workers AI's wrapping.
        let reply = json!({"result": {"model": "clef", "answers": {"effect": {"probabilities": {"deletes": 0.9, "none": 0.1}}}, "usage": {"input_tokens": 1, "output_tokens": 0}}, "success": true});
        let v = effect_verdict(answers(&reply).unwrap()).unwrap().unwrap();
        assert!(v.contains("deletes something (90%)"), "{v}");
    }

    #[test]
    fn a_malformed_answer_is_an_error_and_never_a_panic() {
        for reply in [
            json!(null),
            json!("text"),
            json!({"result": []}),
            json!({"answers": {"effect": 3}}),
            json!({"answers": {"effect": {"probabilities": [0.5]}}}),
            json!({"answers": {"secret": {"noul": "yes"}}}),
            json!({"answers": {"credentials": {"noul": 7.0}}}),
        ] {
            if let Some(a) = answers(&reply) {
                assert!(effect_verdict(a).is_err() || effect_verdict(a) == Ok(None));
                assert!(typing_verdict(a).is_err());
                assert!(personal_verdict(a).is_err());
            }
        }
        // Probabilities out of range count for nothing.
        let wild = choice(json!({"deletes": 5.0, "sends": -1.0, "none": 0.1}));
        assert_eq!(effect_verdict(&wild).unwrap(), None);
    }

    #[test]
    fn a_secret_field_waits() {
        let a = json!({"secret": {"type": "noul", "noul": 0.93}});
        assert!(typing_verdict(a.as_object().unwrap())
            .unwrap()
            .unwrap()
            .contains("password, PIN or key field (93%)"));
        let a = json!({"secret": {"type": "noul", "noul": 0.2}});
        assert_eq!(typing_verdict(a.as_object().unwrap()).unwrap(), None);
    }

    #[test]
    fn personal_information_is_found_by_kind_and_every_kind_must_be_answered() {
        let mut answers: Map<String, Value> = Personal::LOOKED_FOR
            .iter()
            .map(|k| (k.id().to_string(), json!({"type": "noul", "noul": 0.1})))
            .collect();
        assert_eq!(personal_verdict(&answers).unwrap(), vec![]);
        answers.insert("payment".into(), json!({"type": "noul", "noul": 0.7}));
        answers.insert("messages".into(), json!({"type": "noul", "noul": 0.5}));
        let found = personal_verdict(&answers).unwrap();
        assert_eq!(found, vec![Personal::Payment, Personal::Messages]);
        assert_eq!(
            describe_all(&found),
            "payment or bank details and private messages"
        );
        answers.remove("health");
        assert!(personal_verdict(&answers).unwrap_err().contains("health"));
        let body = personal_request("clef", String::new());
        assert_eq!(
            body["questions"].as_object().unwrap().len(),
            Personal::LOOKED_FOR.len()
        );
    }

    #[test]
    fn the_persons_answers_hold_for_the_kinds_they_were_about() {
        let mut answered = BTreeMap::new();
        assert_eq!(showing(&[], &answered), Showing::Show);
        assert_eq!(
            showing(&[Personal::Payment], &answered),
            Showing::Ask(vec![Personal::Payment])
        );
        answered.insert(Personal::Payment, true);
        assert_eq!(showing(&[Personal::Payment], &answered), Showing::Show);
        // A new kind is asked about; one refused is withheld without asking.
        assert_eq!(
            showing(&[Personal::Payment, Personal::Health], &answered),
            Showing::Ask(vec![Personal::Health])
        );
        answered.insert(Personal::Health, false);
        assert_eq!(
            showing(&[Personal::Payment, Personal::Health], &answered),
            Showing::Withhold(vec![Personal::Health])
        );
    }

    #[test]
    fn without_an_account_or_a_token_every_check_says_what_to_set() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Judge::new(dir.path(), &Checks::default()).is_none());
        let j = Judge::with(
            checks(),
            "clef",
            Err("no Cloudflare account ID (Agent setup, or CLOUDFLARE_ACCOUNT_ID)".into()),
            None,
        );
        let e = j
            .judge(&screen(), &Subject::Typing { chars: 3 })
            .unwrap_err();
        assert!(e.contains("account ID"), "{e}");
        let j = Judge::with(checks(), "clef", Ok("http://127.0.0.1:9/".into()), None);
        let e = j.judge(&screen(), &Subject::Enter).unwrap_err();
        assert!(e.contains("set-key cloudflare"), "{e}");
    }

    /// One request to a local stand-in for Workers AI: what it was sent
    /// (head and body), after answering `status` and `reply`.
    fn serve_once(status: u16, reply: Value) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/accounts/abc/ai/run/@cf/cloudflare/clef",
            listener.local_addr().unwrap()
        );
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut head = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let text = reply.to_string();
            let mut stream = stream;
            write!(
                stream,
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}",
                text.len()
            )
            .unwrap();
            format!("{head}{}", String::from_utf8_lossy(&body))
        });
        (url, handle)
    }

    #[test]
    fn a_check_is_one_authorised_request_and_its_answer() {
        let (url, server) = serve_once(
            200,
            json!({"result": {"model": "clef", "answers": {"secret": {"type": "noul", "noul": 0.8}}, "usage": {"input_tokens": 1200, "output_tokens": 0}}, "success": true, "errors": []}),
        );
        let j = Judge::with(checks(), "clef", Ok(url), Some("tok".into()));
        let why = j
            .judge(&screen(), &Subject::Typing { chars: 4 })
            .unwrap()
            .unwrap();
        assert!(why.contains("(80%)"), "{why}");
        let sent = server.join().unwrap();
        assert!(sent.starts_with("POST /accounts/abc/ai/run/@cf/cloudflare/clef "));
        assert!(sent
            .to_ascii_lowercase()
            .contains("authorization: bearer tok"));
        let (_, body) = sent.split_once("\r\n\r\n").unwrap();
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["model"], "clef");
        assert_eq!(body["questions"]["secret"]["type"], "noul");
    }

    #[test]
    fn cloudflares_error_is_passed_on() {
        let (url, server) = serve_once(
            403,
            json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}]}),
        );
        let j = Judge::with(checks(), "clef", Ok(url), Some("tok".into()));
        let e = j.judge(&screen(), &Subject::Enter).unwrap_err();
        assert_eq!(e, "clef: 403: Authentication error");
        server.join().unwrap();
    }
}
