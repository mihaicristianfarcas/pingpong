//! A run's control folder: how the person running an agent reaches whatever
//! does the actions -- this process (an API key's run), or the MCP server
//! that Codex or Claude Code started, in a process of its own (and, in
//! tests, a container of its own). Files, so every one of them can see it:
//!
//! - `pause`: while it exists, actions wait;
//! - `ask-N.json`: a step waiting for the person's go-ahead
//!   (`{"n": N, "action": "type \"rm -rf build\"", "why": "…", "from": "rule"}`):
//!   a risky action a rule or clef caught (`rule`), one the model asked
//!   about (`model`), any action when every one is to be approved
//!   (`every`), or a screen with personal information the model is to see
//!   (`screen`, whatever the approvals: see `judge`);
//! - `answer-N`: `yes` or `no`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::providers::{Approvals, Ask};

const POLL: Duration = Duration::from_millis(200);
/// A yes to the model's own request covers the risky actions that carry it
/// out, for a while: the person is not asked twice for one step.
const COVER_FOR: Duration = Duration::from_secs(120);
const COVER_ACTIONS: u32 = 3;

pub struct Control {
    dir: PathBuf,
    approvals: Approvals,
    next: u32,
    /// What a yes to the model's request still covers: until when, and how
    /// many risky actions.
    covered: Option<(Instant, u32)>,
}

impl Control {
    pub fn new(dir: PathBuf, approvals: Approvals) -> Control {
        let _ = std::fs::create_dir_all(&dir);
        // Numbered on from the clock: a turn's questions never share a
        // number with an earlier turn's in the same folder.
        let next = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| (d.as_millis() % 1_000_000_000) as u32)
            .unwrap_or(0);
        Control {
            dir,
            approvals,
            next,
            covered: None,
        }
    }

    pub fn approvals(&self) -> Approvals {
        self.approvals
    }

    /// Before an input action: wait while paused, and for a yes when it is
    /// to be approved (`risk`: why a rule thinks it risky). `stop` says if
    /// the run must stop meanwhile (and why); `max_wait` is how long a
    /// person is waited for.
    pub fn gate(
        &mut self,
        action: &str,
        risk: Option<&str>,
        stop: &dyn Fn() -> Option<String>,
        max_wait: Duration,
    ) -> Result<(), String> {
        let started = Instant::now();
        while self.dir.join("pause").exists() {
            if let Some(why) = stop() {
                return Err(why);
            }
            if started.elapsed() > max_wait {
                return Err(
                    "The person running you paused you and has not resumed you. \
                        Stop here and report where you are."
                        .into(),
                );
            }
            std::thread::sleep(POLL);
        }
        let (why, from) = match (self.approvals, risk) {
            (Approvals::Every, r) => (r.unwrap_or_default().to_string(), "every"),
            (Approvals::Risky, Some(r)) if !self.covered() => (r.to_string(), "rule"),
            _ => return Ok(()),
        };
        if self.ask(
            &Ask {
                what: action.to_string(),
                why,
            },
            from,
            stop,
            max_wait,
        )? {
            Ok(())
        } else {
            Err(format!(
                "The person said no to {action}. Don't do it: do it another way, \
                    or stop and report."
            ))
        }
    }

    /// The model asks before a step (`ask_approval`): true if the person
    /// said yes. With approvals off, yes at once.
    pub fn request(
        &mut self,
        ask: &Ask,
        stop: &dyn Fn() -> Option<String>,
        max_wait: Duration,
    ) -> Result<bool, String> {
        if self.approvals == Approvals::Off {
            return Ok(true);
        }
        let yes = self.ask(ask, "model", stop, max_wait)?;
        self.covered = yes.then(|| (Instant::now() + COVER_FOR, COVER_ACTIONS));
        Ok(yes)
    }

    /// Whether the model may see a screen clef finds personal information
    /// on (see `judge`): asked whatever the approvals, since it is about
    /// what leaves the computer, not what the agent does. Nobody answering
    /// in time is a no; only a stop is an error.
    pub fn show(
        &mut self,
        ask: &Ask,
        stop: &dyn Fn() -> Option<String>,
        max_wait: Duration,
    ) -> Result<bool, String> {
        match self.ask(ask, "screen", stop, max_wait) {
            Err(why) if stop().is_some() => Err(why),
            answer => Ok(answer.unwrap_or(false)),
        }
    }

    /// A yes to the model's request still covers a risky action (using it).
    fn covered(&mut self) -> bool {
        match &mut self.covered {
            Some((until, left)) if Instant::now() < *until && *left > 0 => {
                *left -= 1;
                true
            }
            _ => {
                self.covered = None;
                false
            }
        }
    }

    fn ask(
        &mut self,
        ask: &Ask,
        from: &str,
        stop: &dyn Fn() -> Option<String>,
        max_wait: Duration,
    ) -> Result<bool, String> {
        self.next += 1;
        let n = self.next;
        let started = Instant::now();
        let file = self.dir.join(format!("ask-{n}.json"));
        let answer = self.dir.join(format!("answer-{n}"));
        // An answer to an earlier turn's question of the same number, come
        // too late, is not this one's.
        let _ = std::fs::remove_file(&answer);
        let body = serde_json::json!({"n": n, "action": ask.what, "why": ask.why, "from": from});
        std::fs::write(&file, body.to_string()).map_err(|e| e.to_string())?;
        tracing::info!(
            n,
            from,
            action = ask.what,
            why = ask.why,
            "waiting for the person's yes"
        );
        let result = loop {
            if let Ok(a) = std::fs::read_to_string(&answer) {
                let yes = a.trim() == "yes";
                tracing::info!(
                    n,
                    yes,
                    secs = started.elapsed().as_secs(),
                    "the person answered"
                );
                break Ok(yes);
            }
            if let Some(why) = stop() {
                break Err(why);
            }
            if started.elapsed() > max_wait {
                break Err(format!(
                    "Nobody answered whether to {} (waited {} minutes). Don't do it: stop \
                        here and report that it needs the person's go-ahead.",
                    ask.what,
                    started.elapsed().as_secs() / 60
                ));
            }
            std::thread::sleep(POLL);
        };
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_file(&answer);
        result
    }
}

/// Pause the run whose control folder is `dir`, or let it go on.
pub fn set_paused(dir: &Path, paused: bool) {
    let p = dir.join("pause");
    if paused {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(p, "");
    } else {
        let _ = std::fs::remove_file(p);
    }
}

/// Steps waiting for an answer: (number, what and why).
pub fn pending(dir: &Path) -> Vec<(u32, Ask)> {
    let mut out: Vec<(u32, Ask)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let n: u32 = name
                .strip_prefix("ask-")?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            if dir.join(format!("answer-{n}")).exists() {
                return None;
            }
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            let what = v["action"].as_str()?.to_string();
            Some((
                n,
                Ask {
                    what,
                    why: v["why"].as_str().unwrap_or_default().to_string(),
                },
            ))
        })
        .collect();
    out.sort_by_key(|(n, _)| *n);
    out
}

pub fn answer(dir: &Path, n: u32, yes: bool) {
    let _ = std::fs::write(
        dir.join(format!("answer-{n}")),
        if yes { "yes" } else { "no" },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_secs(120);

    /// Answer the asks in `dir` in turn with `answers`, as the person would.
    fn answerer(dir: PathBuf, answers: Vec<bool>) -> std::thread::JoinHandle<Vec<Ask>> {
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            for yes in answers {
                loop {
                    if let Some((n, ask)) = pending(&dir).into_iter().next() {
                        seen.push(ask);
                        answer(&dir, n, yes);
                        while dir.join(format!("answer-{n}")).exists() {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            seen
        })
    }

    #[test]
    fn every_action_waits_for_its_answer_and_a_pause() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_path_buf();
        let mut c = Control::new(d.clone(), Approvals::Every);
        let person = answerer(d.clone(), vec![false, true]);
        let none = || None;
        assert!(c
            .gate("left_click (1, 1)", None, &none, WAIT)
            .unwrap_err()
            .contains("said no"));
        c.gate("left_click (2, 2)", None, &none, WAIT).unwrap();
        let seen = person.join().unwrap();
        assert_eq!(
            seen.iter().map(|a| a.what.as_str()).collect::<Vec<_>>(),
            ["left_click (1, 1)", "left_click (2, 2)"]
        );
        // A pause holds the next action until lifted.
        set_paused(&d, true);
        let lift = {
            let d = d.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(300));
                set_paused(&d, false);
            })
        };
        let t = Instant::now();
        c.approvals = Approvals::Off;
        c.gate("left_click (3, 3)", None, &none, WAIT).unwrap();
        assert!(
            t.elapsed() >= Duration::from_millis(250),
            "{:?}",
            t.elapsed()
        );
        lift.join().unwrap();
        assert!(pending(&d).is_empty());
    }

    #[test]
    fn risky_actions_wait_and_routine_ones_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_path_buf();
        let mut c = Control::new(d.clone(), Approvals::Risky);
        let none = || None;
        // Routine: at once, nothing asked.
        c.gate("left_click (1, 1)", None, &none, WAIT).unwrap();
        assert!(pending(&d).is_empty());
        let person = answerer(d.clone(), vec![true, true]);
        c.gate(
            "type \"rm -rf build\"",
            Some("It types a command…"),
            &none,
            WAIT,
        )
        .unwrap();
        // The model's own request, then the risky action that carries it
        // out: asked once.
        assert!(c
            .request(
                &Ask {
                    what: "Empty the Recycle Bin".into(),
                    why: "to free space".into()
                },
                &none,
                WAIT
            )
            .unwrap());
        c.gate("key shift+Delete", Some("Shift+Delete…"), &none, WAIT)
            .unwrap();
        let seen = person.join().unwrap();
        assert_eq!(seen[0].why, "It types a command…");
        assert_eq!(seen[1].what, "Empty the Recycle Bin");
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn with_approvals_off_nothing_waits() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Control::new(dir.path().to_path_buf(), Approvals::Off);
        let none = || None;
        c.gate("type \"rm -rf /\"", Some("…"), &none, WAIT).unwrap();
        assert!(c
            .request(
                &Ask {
                    what: "anything".into(),
                    why: String::new()
                },
                &none,
                WAIT
            )
            .unwrap());
    }

    #[test]
    fn a_screen_is_asked_about_whatever_the_approvals_and_silence_is_a_no() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_path_buf();
        let mut c = Control::new(d.clone(), Approvals::Off);
        let ask = Ask {
            what: "Show the model this screen".into(),
            why: "Clef finds payment or bank details on it.".into(),
        };
        let none = || None;
        let person = answerer(d.clone(), vec![true]);
        assert!(c.show(&ask, &none, WAIT).unwrap());
        assert_eq!(person.join().unwrap(), vec![ask.clone()]);
        assert!(!c.show(&ask, &none, Duration::from_millis(50)).unwrap());
        assert!(pending(&d).is_empty());
        let e = c.show(&ask, &|| Some("Stopped.".into()), WAIT).unwrap_err();
        assert_eq!(e, "Stopped.");
    }

    #[test]
    fn a_stopped_run_does_not_wait() {
        let dir = tempfile::tempdir().unwrap();
        set_paused(dir.path(), true);
        let mut c = Control::new(dir.path().to_path_buf(), Approvals::Off);
        let e = c
            .gate("key Return", None, &|| Some("Stopped.".into()), WAIT)
            .unwrap_err();
        assert_eq!(e, "Stopped.");
        let mut c = Control::new(dir.path().to_path_buf(), Approvals::Risky);
        set_paused(dir.path(), false);
        let e = c
            .gate(
                "key shift+Delete",
                Some("…"),
                &|| Some("Stopped.".into()),
                WAIT,
            )
            .unwrap_err();
        assert_eq!(e, "Stopped.");
    }
}
