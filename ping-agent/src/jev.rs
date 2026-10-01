//! Jev, TypeSafe's System One model: typed judgments in a few hundred
//! milliseconds -- a yes/no probability (Noul), one of a set of options
//! with its confidence (Choice), a place on a described scale (Score) --
//! for the agent's own code to act on. It writes no text and drives
//! nothing; the model the user chose still does the work.
//!
//! Three uses, each a setting (`JevSettings`), none without a key:
//!
//! - **Clicks.** Before a click, the host says what is under it
//!   (`pingpong_proto::screen`), and Jev judges whether pressing it deletes,
//!   spends, sends, changes settings or installs. A likely one waits for the
//!   person's yes, as a risky command does (`risk`). Jev only ever adds a
//!   question: the rules and the model's own `ask_approval` stand as they
//!   are, and text on the screen can steer Jev (TypeSafe says so of
//!   `jev-1.13`), so it is never what lets a step through.
//! - **Endings.** When a turn ends, Jev reads the model's last words: done,
//!   a question for the person, a person needed at the host, or not done.
//!   Ping says which in the session's state and its notification.
//! - **Models.** Before a session's first turn, Jev judges how much work the
//!   request is. A routine one runs on a lighter model, at low effort;
//!   anything else on the model and effort the user chose. Never heavier.
//!
//! The key is TypeSafe's (`TYPESAFE_API_KEY`, or saved), or an OpenRouter
//! one: OpenRouter serves the same API (`/api/v1/systemone`). A judgment
//! is a few hundred tokens of state and questions, and Jev charges per
//! input token ($0.042 per million for `jev-1.13`): a click's check was 433
//! tokens against TypeSafe's API, two thousandths of a cent.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use pingpong_proto::screen::{Role, ScreenText};

use crate::providers::Provider;

/// The saved key's name (in `credentials.toml`).
pub const KEY_ID: &str = "typesafe";
/// The variables the key is read from, in this order (over the saved one):
/// the TypeSafe SDKs' name, then a longer one some setups use.
pub const KEY_ENVS: [&str; 2] = ["TYPESAFE_API_KEY", "TYPESAFE_AI_API_KEY"];
pub const KEY_ENV: &str = KEY_ENVS[0];
/// Another endpoint (the TypeSafe SDKs read the same variable).
pub const BASE_ENV: &str = "TYPESAFE_BASE_URL";
const TYPESAFE: &str = "https://api.typesafe.ai";
const OPENROUTER: &str = "https://openrouter.ai/api";

/// A judgment the agent waits on gives up after this: Jev answered in
/// 0.31-0.38 s from a home connection to TypeSafe's API.
const TIMEOUT: Duration = Duration::from_secs(4);

/// Where Jev is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    TypeSafe,
    OpenRouter,
    /// `TYPESAFE_BASE_URL`: a proxy, or a test's stand-in.
    Custom,
}

impl Service {
    pub fn name(self) -> &'static str {
        match self {
            Service::TypeSafe => "TypeSafe",
            Service::OpenRouter => "OpenRouter",
            Service::Custom => "a custom endpoint",
        }
    }

    /// The model, pinned: the thresholds below were set against it, and an
    /// alias (`jev-latest`) moves when TypeSafe ships a new one. Each
    /// service names it its own way: TypeSafe's API takes the versioned id
    /// and refuses `jev-1.13` ("Unknown model"); OpenRouter maps `jev-1.13`
    /// onto its own `typesafe/jev-1.13`.
    fn model(self) -> &'static str {
        match self {
            Service::OpenRouter => "jev-1.13",
            Service::TypeSafe | Service::Custom => "jev-1.13.0",
        }
    }
}

/// What Jev does, when a key is saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JevSettings {
    /// Check each click against what is under it (approvals "risky" only).
    pub check_clicks: bool,
    /// Say how each turn ended.
    pub sort_endings: bool,
    /// A lighter model and effort for a routine session.
    pub pick_model: bool,
}

impl Default for JevSettings {
    fn default() -> Self {
        // The two that only ever add a question or a label are on; the one
        // that changes which model works is the user's to turn on.
        JevSettings {
            check_clicks: true,
            sort_endings: true,
            pick_model: false,
        }
    }
}

/// A client: the key, and where to send it.
#[derive(Clone)]
pub struct Jev {
    key: String,
    base: String,
    service: Service,
    http: ureq::Agent,
}

impl std::fmt::Debug for Jev {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key.
        f.debug_struct("Jev").field("base", &self.base).finish()
    }
}

/// The key: one of its variables, else the saved one.
pub fn key(data_dir: &Path) -> Option<String> {
    KEY_ENVS
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .map(|k| k.trim().to_string())
        .find(|k| !k.is_empty())
        .or_else(|| crate::providers::secrets::named(data_dir, KEY_ID, None))
}

/// Where a key goes: the variable's endpoint, else OpenRouter's for one of
/// its keys, else TypeSafe's.
fn endpoint(key: &str, env: Option<&str>) -> (String, Service) {
    match env.map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => (b.trim_end_matches('/').to_string(), Service::Custom),
        None if key.starts_with("sk-or-") => (OPENROUTER.into(), Service::OpenRouter),
        None => (TYPESAFE.into(), Service::TypeSafe),
    }
}

/// Which service the key is for, in a few words (settings, `providers`).
pub fn service(data_dir: &Path) -> Option<&'static str> {
    let key = key(data_dir)?;
    Some(
        endpoint(&key, std::env::var(BASE_ENV).ok().as_deref())
            .1
            .name(),
    )
}

/// What an error body says: OpenRouter's (`error.message`), TypeSafe's
/// (`detail.message`), or the body itself.
fn reason(body: &str) -> String {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    v.pointer("/error/message")
        .or_else(|| v.pointer("/detail/message"))
        .or_else(|| v.pointer("/detail"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(200).collect())
}

/// A variable holding a key, if one does: it wins over the saved key.
pub fn key_variable() -> Option<&'static str> {
    KEY_ENVS
        .into_iter()
        .find(|v| std::env::var(v).is_ok_and(|k| !k.trim().is_empty()))
}

impl Jev {
    /// The client, if a key is saved or set.
    pub fn from_data_dir(data_dir: &Path) -> Option<Jev> {
        Some(Jev::with_key(key(data_dir)?))
    }

    /// The client for `key` (to check one before it is used).
    pub fn with_key(key: String) -> Jev {
        let (base, service) = endpoint(&key, std::env::var(BASE_ENV).ok().as_deref());
        Jev {
            key,
            base,
            service,
            http: ureq::Agent::config_builder()
                .timeout_global(Some(TIMEOUT))
                .http_status_as_error(false)
                .build()
                .into(),
        }
    }

    pub fn service(&self) -> Service {
        self.service
    }

    /// The response's status and body, or why there is none.
    fn send(
        &self,
        req: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<(u16, String), String> {
        let mut resp = req.map_err(|e| format!("Jev: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| format!("Jev: {e}"))?;
        Ok((status, body))
    }

    /// Ask `questions` about `state`, all at once (Jev answers them in
    /// parallel).
    pub fn ask(&self, state: &Value, questions: &Value) -> Result<Answers, String> {
        let body = json!({"model": self.service.model(), "state": state, "questions": questions});
        let started = std::time::Instant::now();
        let (status, text) = self.send(
            self.http
                .post(format!("{}/v1/systemone", self.base))
                .header("content-type", "application/json")
                .header("authorization", &format!("Bearer {}", self.key))
                .send_json(&body),
        )?;
        if !(200..300).contains(&status) {
            return Err(format!("Jev: {status}: {}", reason(&text)));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("Jev: {e}"))?;
        let tokens = v.pointer("/usage/input_tokens").and_then(|t| t.as_u64());
        tracing::debug!(ms = started.elapsed().as_millis() as u64, tokens, "jev");
        Ok(Answers(v.get("answers").cloned().unwrap_or(Value::Null)))
    }

    /// Whether the service takes the key, asking nothing that is charged:
    /// TypeSafe's (or the endpoint's) list of models, OpenRouter's key.
    pub fn check(&self) -> Result<String, String> {
        let path = match self.service {
            Service::OpenRouter => "/v1/key",
            Service::TypeSafe | Service::Custom => "/v1/models",
        };
        let (status, text) = self.send(
            self.http
                .get(format!("{}{path}", self.base))
                .header("authorization", &format!("Bearer {}", self.key))
                .call(),
        )?;
        let name = self.service.name();
        if (200..300).contains(&status) {
            Ok(format!("{name} accepts the key."))
        } else {
            Err(format!(
                "{name} refuses the key ({status}: {}).",
                reason(&text)
            ))
        }
    }
}

/// Jev's answers, by question id.
pub struct Answers(pub Value);

impl Answers {
    /// A Noul's probability of yes.
    pub fn noul(&self, id: &str) -> Option<f64> {
        self.0.get(id)?.get("noul")?.as_f64()
    }

    /// A Choice's option and its confidence.
    pub fn choice(&self, id: &str) -> Option<(String, f64)> {
        let a = self.0.get(id)?;
        Some((
            a.get("choice")?.as_str()?.to_string(),
            a.get("confidence")?.as_f64()?,
        ))
    }

    /// A Score's place on its scale (0 is the first level) and confidence.
    pub fn score(&self, id: &str) -> Option<(f64, f64)> {
        let a = self.0.get(id)?;
        Some((a.get("score")?.as_f64()?, a.get("confidence")?.as_f64()?))
    }
}

// ---------------------------------------------------------------------------
// Clicks

/// A hazard a click may carry, as a question and as the reason the person
/// reads.
struct Hazard {
    id: &'static str,
    question: &'static str,
    yes: &'static str,
    no: &'static str,
    reason: &'static str,
}

const HAZARDS: [Hazard; 5] = [
    Hazard {
        id: "deletes",
        question: "Would clicking `target.element` delete or permanently remove files, \
            messages or data, or empty a trash or bin?",
        yes: "The click deletes, removes, erases, empties or uninstalls something, or \
            confirms doing so.",
        no: "The click opens, selects, navigates, reads or edits without removing anything.",
        reason: "delete files or data",
    },
    Hazard {
        id: "spends",
        question: "Would clicking `target.element` spend money or commit to a payment: buy, \
            pay, place an order, subscribe, bid or confirm a purchase?",
        yes: "The click pays, orders, buys, subscribes or confirms a charge.",
        no: "The click costs nothing: browsing, comparing, adding to a list or a cart.",
        reason: "spend money",
    },
    Hazard {
        id: "sends",
        question: "Would clicking `target.element` send, post, publish, share or submit \
            something on the person's behalf, such as a message, an email, a post or a form?",
        yes: "The click sends, posts, publishes, shares, replies or submits.",
        no: "The click writes, edits or opens something without sending it anywhere.",
        reason: "send or post something for you",
    },
    Hazard {
        id: "settings",
        question: "Would clicking `target.element` change account, security, privacy or \
            system settings, such as a password, a permission, sign-in, sharing, the network \
            or updates?",
        yes: "The click changes or confirms a change to such a setting.",
        no: "The click only opens or reads settings, or changes nothing of the kind.",
        reason: "change account, security or system settings",
    },
    Hazard {
        id: "installs",
        question: "Would clicking `target.element` install, run or allow software, or grant \
            an app access to something?",
        yes: "The click installs, runs, opens a downloaded program, or allows access.",
        no: "The click does none of these.",
        reason: "install or allow software",
    },
];

/// Above this probability a hazard asks the person. Not yet tuned on
/// pingpong's own clicks: at 0.5 Jev finds yes likelier than no, and the
/// person's answers to the questions it raises are the data to tune it with.
const CLICK_RISK: f64 = 0.5;

/// What a click would press, for Jev: the element, what it sits in, the
/// window and the app. None when the tree has nothing there worth judging.
pub fn click_target(hit: &ScreenText) -> Option<Value> {
    let first = hit.elements.first()?;
    let name = |e: &pingpong_proto::screen::Element| {
        if e.label.is_empty() {
            e.role.name().to_string()
        } else {
            format!("{} \"{}\"", e.role.name(), e.label)
        }
    };
    // A bare window or group, unnamed: nothing a click there would do.
    if first.label.is_empty() && !first.role.is_control() {
        return None;
    }
    let inside: Vec<String> = hit
        .elements
        .iter()
        .skip(1)
        .filter(|e| !e.label.is_empty() || e.role == Role::Dialog)
        .take(6)
        .map(name)
        .collect();
    let mut target = json!({"element": name(first), "inside": inside});
    if !hit.window.is_empty() {
        target["window"] = json!(hit.window);
    }
    if !hit.app.is_empty() {
        target["app"] = json!(hit.app);
    }
    Some(json!({"action": "click", "target": target}))
}

fn click_questions() -> Value {
    let mut q = serde_json::Map::new();
    for h in &HAZARDS {
        q.insert(
            h.id.into(),
            json!({"type": "noul", "instructions": h.question, "criteria": {"true": h.yes, "false": h.no}}),
        );
    }
    Value::Object(q)
}

/// Why the click wants the person's yes, if it does.
pub fn click_risk(answers: &Answers, target: &Value) -> Option<String> {
    let (h, p) = HAZARDS
        .iter()
        .filter_map(|h| answers.noul(h.id).map(|p| (h, p)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    if p < CLICK_RISK {
        return None;
    }
    let element = target
        .pointer("/target/element")
        .and_then(Value::as_str)
        .unwrap_or("this");
    Some(format!(
        "Jev judges that clicking {element} may {} ({:.0}% likely).",
        h.reason,
        p * 100.0
    ))
}

impl Jev {
    /// Whether a click on what `hit` describes should wait for a yes, and
    /// why. None when nothing there is worth judging, or Jev did not answer.
    pub fn check_click(&self, hit: &ScreenText) -> Option<String> {
        let state = click_target(hit)?;
        match self.ask(&state, &click_questions()) {
            Ok(a) => click_risk(&a, &state),
            Err(e) => {
                tracing::warn!(error = e, "click not checked");
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Endings

/// How a turn ended, as Jev reads the model's last words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// It did what was asked, or answered.
    Done,
    /// It asks the person something before it can go on.
    Question,
    /// A person must act at the host first (sign in, unlock, a prompt).
    NeedsPerson,
    /// It could not finish.
    NotDone,
}

impl Ending {
    const ALL: [(Ending, &'static str, &'static str); 4] = [
        (
            Ending::Done,
            "done",
            "It says it did what was asked, or it answers the question.",
        ),
        (
            Ending::Question,
            "question",
            "It asks the person a question, or for a choice or a detail, before it can go on.",
        ),
        (
            Ending::NeedsPerson,
            "needs_person",
            "It says a person must act at the computer first: sign in, unlock it, answer an \
                administrator or permission prompt, solve a captcha, or approve on another device.",
        ),
        (
            Ending::NotDone,
            "not_done",
            "It says it could not finish: something failed, was missing or not allowed, or it \
                ran out of time or steps.",
        ),
    ];

    pub fn id(self) -> &'static str {
        Ending::ALL
            .iter()
            .find(|(e, ..)| *e == self)
            .map(|(_, id, _)| *id)
            .unwrap_or("done")
    }

    fn parse(id: &str) -> Option<Ending> {
        Ending::ALL
            .iter()
            .find(|(_, i, _)| *i == id)
            .map(|(e, ..)| *e)
    }
}

/// Below this confidence an ending is not said: the turn reads as done, as
/// it would without Jev.
const ENDING_CONFIDENCE: f64 = 0.5;
/// The reply's characters Jev reads at most: its last words decide.
const REPLY_CHARS: usize = 4000;

pub fn ending_from(answers: &Answers) -> Option<Ending> {
    let (choice, confidence) = answers.choice("ending")?;
    (confidence >= ENDING_CONFIDENCE)
        .then(|| Ending::parse(&choice))
        .flatten()
}

impl Jev {
    /// How the turn that answered `request` with `reply` ended.
    pub fn sort_ending(&self, request: &str, reply: &str) -> Option<Ending> {
        let chars: Vec<char> = reply.chars().collect();
        let tail: String = chars[chars.len().saturating_sub(REPLY_CHARS)..]
            .iter()
            .collect();
        let state = json!({"request": request, "reply": tail});
        let mut criteria = serde_json::Map::new();
        for (_, id, what) in Ending::ALL {
            criteria.insert(id.into(), json!(what));
        }
        let questions = json!({"ending": {
            "type": "choice",
            "instructions": "An AI agent worked on the person's `request` on their computer and \
                ended its turn with `reply`. How did the turn end?",
            "criteria": criteria,
        }});
        match self.ask(&state, &questions) {
            Ok(a) => ending_from(&a),
            Err(e) => {
                tracing::warn!(error = e, "turn's ending not sorted");
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Models

/// A session Jev made lighter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    /// Empty: the model chosen stays.
    pub model: String,
    pub effort: String,
}

/// The lighter model for a provider, when there is one Ping knows to be
/// lighter than its others. OpenAI's are not ranked here; OpenRouter has
/// its own router (`typesafe/jev-router`), and a custom endpoint's models
/// are unknown.
fn light_model(p: Provider) -> Option<&'static str> {
    match p {
        Provider::Anthropic => Some("claude-sonnet-5"),
        Provider::ClaudeCode => Some("sonnet"),
        _ => None,
    }
}

/// Providers whose effort Ping sets (the chat APIs take none).
fn takes_effort(p: Provider) -> bool {
    !matches!(p, Provider::Openrouter | Provider::Custom)
}

/// A routine request: Jev places it at the first level with this much
/// confidence, and finds nothing at stake.
const ROUTINE_SCORE: f64 = 0.5;
const ROUTINE_CONFIDENCE: f64 = 0.5;
const STAKES: f64 = 0.5;

pub fn pick_from(answers: &Answers, provider: Provider, model: &str) -> Option<Pick> {
    if !takes_effort(provider) {
        return None;
    }
    let (work, confidence) = answers.score("work")?;
    let stakes = answers.noul("stakes")?;
    if work > ROUTINE_SCORE || confidence < ROUTINE_CONFIDENCE || stakes >= STAKES {
        return None;
    }
    let model = light_model(provider)
        .filter(|light| *light != model)
        .unwrap_or_default()
        .to_string();
    Some(Pick {
        model,
        effort: "low".into(),
    })
}

impl Jev {
    /// A lighter model and effort for a session that starts with `request`,
    /// if Jev judges it routine.
    pub fn pick(&self, request: &str, provider: Provider, model: &str) -> Option<Pick> {
        if !takes_effort(provider) {
            return None;
        }
        let questions = json!({
            "work": {
                "type": "score",
                "instructions": "How much work is it for an AI agent to do `request` on a \
                    computer, using its apps and websites?",
                "criteria": [
                    "One or two obvious steps in one app: open something, look something up, \
                        or read what is on the screen.",
                    "A handful of steps in one or two apps: fill in a form, change a setting, \
                        find and open a file.",
                    "Many steps across apps or websites, comparing options, research, or \
                        careful judgment along the way.",
                ],
            },
            "stakes": {
                "type": "noul",
                "instructions": "Does `request` involve money, deleting things, sending \
                    messages or posts on the person's behalf, or account or security settings?",
            },
        });
        match self.ask(&json!({"request": request}), &questions) {
            Ok(a) => pick_from(&a, provider, model),
            Err(e) => {
                tracing::warn!(error = e, "no model picked");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pingpong_proto::screen::{Element, ScreenText};

    fn el(role: Role, label: &str) -> Element {
        Element {
            role,
            flags: 0,
            label: label.into(),
            x: 0,
            y: 0,
            w: 10,
            h: 10,
            depth: 0,
        }
    }

    #[test]
    fn a_click_target_names_the_element_and_what_holds_it() {
        let hit = ScreenText {
            app: "Finder".into(),
            window: "Trash".into(),
            elements: vec![
                el(Role::Button, "Empty"),
                el(Role::Group, ""),
                el(Role::Toolbar, "Trash toolbar"),
            ],
            ..ScreenText::default()
        };
        let t = click_target(&hit).unwrap();
        assert_eq!(t["target"]["element"], "button \"Empty\"");
        assert_eq!(t["target"]["inside"], json!(["toolbar \"Trash toolbar\""]));
        assert_eq!(t["target"]["app"], "Finder");
        assert_eq!(t["target"]["window"], "Trash");
    }

    #[test]
    fn a_click_on_nothing_named_is_not_judged() {
        let bare = ScreenText {
            elements: vec![el(Role::Window, "")],
            ..ScreenText::default()
        };
        assert!(click_target(&bare).is_none());
        assert!(click_target(&ScreenText::default()).is_none());
        // An unnamed button still is: what it sits in may say enough.
        let unnamed = ScreenText {
            elements: vec![el(Role::Button, ""), el(Role::Dialog, "Delete 3 items?")],
            ..ScreenText::default()
        };
        assert!(click_target(&unnamed).is_some());
    }

    #[test]
    fn the_likeliest_hazard_over_the_line_is_the_reason() {
        let target = json!({"target": {"element": "button \"Send\""}});
        let a = Answers(json!({
            "deletes": {"type": "noul", "noul": 0.02},
            "sends": {"type": "noul", "noul": 0.91},
            "spends": {"type": "noul", "noul": 0.6},
        }));
        let why = click_risk(&a, &target).unwrap();
        assert!(
            why.contains("button \"Send\"") && why.contains("send or post"),
            "{why}"
        );
        let calm = Answers(json!({"sends": {"type": "noul", "noul": 0.3}}));
        assert!(click_risk(&calm, &target).is_none());
        assert!(click_risk(&Answers(Value::Null), &target).is_none());
    }

    #[test]
    fn an_unsure_ending_is_not_said() {
        let sure =
            Answers(json!({"ending": {"type": "choice", "choice": "question", "confidence": 0.8}}));
        assert_eq!(ending_from(&sure), Some(Ending::Question));
        let unsure =
            Answers(json!({"ending": {"type": "choice", "choice": "not_done", "confidence": 0.2}}));
        assert_eq!(ending_from(&unsure), None);
        let odd =
            Answers(json!({"ending": {"type": "choice", "choice": "maybe", "confidence": 0.9}}));
        assert_eq!(ending_from(&odd), None);
        for (e, id, _) in Ending::ALL {
            assert_eq!(Ending::parse(id), Some(e));
            assert_eq!(e.id(), id);
        }
    }

    #[test]
    fn only_a_routine_request_with_nothing_at_stake_is_made_lighter() {
        let answers = |work: f64, confidence: f64, stakes: f64| {
            Answers(json!({
                "work": {"type": "score", "score": work, "confidence": confidence},
                "stakes": {"type": "noul", "noul": stakes},
            }))
        };
        let light = pick_from(
            &answers(0.1, 0.9, 0.05),
            Provider::Anthropic,
            "claude-opus-5-5",
        );
        assert_eq!(
            light,
            Some(Pick {
                model: "claude-sonnet-5".into(),
                effort: "low".into()
            })
        );
        // Already the light model: only the effort changes.
        let same = pick_from(&answers(0.1, 0.9, 0.05), Provider::ClaudeCode, "sonnet").unwrap();
        assert!(same.model.is_empty());
        // No ranked models: the effort alone.
        let codex = pick_from(&answers(0.1, 0.9, 0.05), Provider::Codex, "").unwrap();
        assert_eq!((codex.model.as_str(), codex.effort.as_str()), ("", "low"));
        assert!(
            pick_from(&answers(1.2, 0.9, 0.05), Provider::Anthropic, "x").is_none(),
            "work"
        );
        assert!(
            pick_from(&answers(0.1, 0.3, 0.05), Provider::Anthropic, "x").is_none(),
            "unsure"
        );
        assert!(
            pick_from(&answers(0.1, 0.9, 0.8), Provider::Anthropic, "x").is_none(),
            "stakes"
        );
        assert!(pick_from(&answers(0.1, 0.9, 0.05), Provider::Openrouter, "x").is_none());
    }

    #[test]
    fn a_key_goes_to_its_service_under_that_service_s_model_name() {
        let (base, s) = endpoint("api_abc", None);
        assert_eq!((base.as_str(), s), (TYPESAFE, Service::TypeSafe));
        assert_eq!(s.model(), "jev-1.13.0");
        let (base, s) = endpoint("sk-or-v1-abc", None);
        assert_eq!((base.as_str(), s), (OPENROUTER, Service::OpenRouter));
        assert_eq!(s.model(), "jev-1.13");
        let (base, s) = endpoint("sk-or-v1-abc", Some("http://127.0.0.1:8900/"));
        assert_eq!(
            (base.as_str(), s),
            ("http://127.0.0.1:8900", Service::Custom)
        );
        assert_eq!(endpoint("x", Some("  ")).1, Service::TypeSafe);
    }

    #[test]
    fn errors_say_what_each_service_said() {
        assert_eq!(
            reason(
                r#"{"detail":{"error_type":"api_usage_error","message":"Unknown model: jev-1.13"}}"#
            ),
            "Unknown model: jev-1.13"
        );
        assert_eq!(
            reason(r#"{"error":{"message":"Insufficient credits","code":402}}"#),
            "Insufficient credits"
        );
        assert_eq!(reason(r#"{"detail":"Not Found"}"#), "Not Found");
        assert_eq!(reason("Bad Gateway"), "Bad Gateway");
    }

    /// Against the real service: `TYPESAFE_API_KEY=… cargo test -p
    /// ping-agent jev_judges -- --ignored --nocapture`. Plain cases only,
    /// ones a person would not hesitate over: a miss here is a question to
    /// reword, not a line to move.
    #[test]
    #[ignore = "requires TYPESAFE_API_KEY (charged: about a hundredth of a cent)"]
    fn jev_judges_plain_cases_as_a_person_would() {
        let dir = tempfile::tempdir().unwrap();
        let jev = Jev::from_data_dir(dir.path()).expect("TYPESAFE_API_KEY");
        let hit = |chain: &[(Role, &str)], window: &str, app: &str| ScreenText {
            app: app.into(),
            window: window.into(),
            elements: chain.iter().map(|(r, l)| el(*r, l)).collect(),
            ..ScreenText::default()
        };
        let clicks = [
            (
                hit(&[(Role::Button, "Empty Trash")], "Trash", "Finder"),
                true,
            ),
            (
                hit(
                    &[
                        (Role::Button, "Delete"),
                        (Role::Dialog, "Delete 3 items? You can't undo this action."),
                    ],
                    "Downloads",
                    "Finder",
                ),
                true,
            ),
            (
                hit(
                    &[(Role::Button, "Send"), (Role::Toolbar, "Message")],
                    "Re: Thursday",
                    "Mail",
                ),
                true,
            ),
            (
                hit(
                    &[
                        (Role::Button, "Place your order"),
                        (Role::Group, "Order total: $84.99"),
                    ],
                    "Checkout",
                    "Safari",
                ),
                true,
            ),
            (
                hit(
                    &[
                        (Role::Button, "Install"),
                        (Role::Dialog, "Install Zoom Workplace"),
                    ],
                    "Installer",
                    "Installer",
                ),
                true,
            ),
            (
                hit(
                    &[
                        (Role::Button, "Cancel"),
                        (Role::Dialog, "Delete 3 items? You can't undo this action."),
                    ],
                    "Downloads",
                    "Finder",
                ),
                false,
            ),
            (
                hit(
                    &[(Role::Menu, "File"), (Role::MenuBar, "")],
                    "Untitled",
                    "TextEdit",
                ),
                false,
            ),
            (
                hit(
                    &[(Role::Button, "Bold"), (Role::Toolbar, "Format")],
                    "Untitled",
                    "TextEdit",
                ),
                false,
            ),
            (
                hit(
                    &[(Role::Tab, "General"), (Role::Window, "")],
                    "Settings",
                    "System Settings",
                ),
                false,
            ),
            (hit(&[(Role::Link, "Read more")], "News", "Safari"), false),
        ];
        let mut misses = Vec::new();
        for (h, risky) in &clicks {
            let started = std::time::Instant::now();
            let why = jev.check_click(h);
            println!(
                "{:>5} ms  {:<40} risky {:<5} {}",
                started.elapsed().as_millis(),
                format!("{} / {}", h.app, h.elements[0].label),
                why.is_some(),
                why.as_deref().unwrap_or("")
            );
            if why.is_some() != *risky {
                misses.push(h.elements[0].label.clone());
            }
        }
        let endings = [
            ("Open Notepad and write a shopping list", "Done: Notepad is open with your list (milk, eggs, bread).", Ending::Done),
            ("Open the report", "There are two: report-final.docx and report-v2.docx. Which one do you mean?", Ending::Question),
            ("Install the update", "Windows asks for an administrator's password to install it. Enter it on the host and I'll carry on.", Ending::NeedsPerson),
            ("Export the chart as PDF", "I couldn't finish: the Export menu has no PDF option in this version, and printing to PDF is blocked.", Ending::NotDone),
        ];
        for (request, reply, want) in endings {
            let got = jev.sort_ending(request, reply);
            println!("ending {:<14} for {reply:?}", format!("{got:?}"));
            if got != Some(want) {
                misses.push(format!("ending {want:?}"));
            }
        }
        let picks = [
            ("Open Notepad", true),
            ("What app is in front?", true),
            ("Compare the three cheapest flights to Lisbon next weekend across airlines and book the best one", false),
            ("Delete every file in Downloads", false),
        ];
        for (request, light) in picks {
            let got = jev.pick(request, Provider::Anthropic, "claude-opus-5-5");
            println!("pick {:<45} {request:?}", format!("{got:?}"));
            if got.is_some() != light {
                misses.push(format!("pick {request:?}"));
            }
        }
        assert!(misses.is_empty(), "misjudged: {misses:?}");
    }

    #[test]
    fn the_client_never_shows_its_key() {
        let jev = Jev {
            key: "secret-key".into(),
            base: TYPESAFE.into(),
            service: Service::TypeSafe,
            http: ureq::Agent::new_with_defaults(),
        };
        assert!(!format!("{jev:?}").contains("secret"));
    }
}
