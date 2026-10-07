//! What a paired device may do on a host: Apollo's client permissions
//! (`crypto.h`, `PERM`), fitted to what a pingpong session carries.
//!
//! The host keeps a set per paired device and enforces it as Apollo does,
//! where each thing arrives: a session or a watch is refused without
//! [`VIEW`] (or [`WATCH`]), input of a kind the device may not send is
//! dropped (`input.cpp`'s `passthrough`), the clipboard moves only the ways
//! it may, and taking [`VIEW`] away ends a running session (`stream.cpp`'s
//! `update_device_info`). The host tells the client its set in the
//! session's ack and again when it changes, so the client can say what is
//! held back and why.
//!
//! On the wire and in the host's files the set is a bitmask; in files and
//! the web API it is written as names (`"view"`, `"keyboard"`), so a file
//! reads plainly and a name a newer host wrote is skipped, not misread.
//!
//! Apollo has no AI agents. An agent's set uses the same bits for seeing
//! and for the keyboard and mouse, and one of its own ([`UNWATCHED`]); the
//! ones a person's device has beyond those mean nothing for an agent,
//! whose rules forbid them anyway (docs/ai-agents.md).

/// See the screen and hear the sound: Apollo's `view`. Without it the
/// device is paired but turned away.
pub const VIEW: u16 = 1;
/// Type on the host's keyboard: keys and text (Apollo's `input_kbd`).
pub const KEYBOARD: u16 = 1 << 1;
/// Move, click and scroll the host's mouse (Apollo's `input_mouse`).
pub const MOUSE: u16 = 1 << 2;
/// Play with game controllers (Apollo's `input_controller`).
pub const CONTROLLER: u16 = 1 << 3;
/// What is copied on the host can be pasted on the device (Apollo's
/// `clipboard_read`).
pub const CLIPBOARD_READ: u16 = 1 << 4;
/// What is copied on the device can be pasted on the host (Apollo's
/// `clipboard_set`).
pub const CLIPBOARD_WRITE: u16 = 1 << 5;
/// Start an app with the session, such as Steam Big Picture (Apollo's
/// `launch`).
pub const LAUNCH: u16 = 1 << 6;
/// Take over the host while another device streams (where the host lets
/// devices take over at all). Apollo shares a running session between
/// clients instead of handing it over, so it has no such permission.
pub const TAKE_OVER: u16 = 1 << 7;
/// Watch an AI agent's session; with the keyboard or mouse, take over from
/// it; pause, hand back and stop it.
pub const WATCH: u16 = 1 << 8;
/// An agent acts while nobody watches its session. Without it the agent's
/// input is held until a person watches.
pub const UNWATCHED: u16 = 1 << 9;

/// Every permission, with its name in files and the web API.
pub const NAMES: [(u16, &str); 10] = [
    (VIEW, "view"),
    (KEYBOARD, "keyboard"),
    (MOUSE, "mouse"),
    (CONTROLLER, "controller"),
    (CLIPBOARD_READ, "clipboard_read"),
    (CLIPBOARD_WRITE, "clipboard_write"),
    (LAUNCH, "launch"),
    (TAKE_OVER, "take_over"),
    (WATCH, "watch"),
    (UNWATCHED, "unwatched"),
];

/// What a person's device can be allowed.
const PERSON: u16 = VIEW
    | KEYBOARD
    | MOUSE
    | CONTROLLER
    | CLIPBOARD_READ
    | CLIPBOARD_WRITE
    | LAUNCH
    | TAKE_OVER
    | WATCH;
/// What an agent can be allowed: an agent never shares the clipboard,
/// starts apps, watches or takes over a person (docs/ai-agents.md).
const AGENT: u16 = VIEW | KEYBOARD | MOUSE | UNWATCHED;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Permissions(u16);

impl Permissions {
    pub const NONE: Permissions = Permissions(0);
    /// Everything a person's device can be allowed: what the first device
    /// paired with a host gets, as Apollo gives its first client `_all`.
    pub const PERSON_ALL: Permissions = Permissions(PERSON);
    /// See, and use the keyboard, mouse and controllers; no clipboard, no
    /// apps, no taking over, no watching agents.
    pub const PERSON_CONTROL: Permissions = Permissions(VIEW | KEYBOARD | MOUSE | CONTROLLER);
    /// See only: what every later device gets, as Apollo's `_default`
    /// (`view | list`; pingpong has no app list to hide).
    pub const SEE_ONLY: Permissions = Permissions(VIEW);
    /// An agent that sees and acts, watched or not: what an agent gets
    /// when it pairs.
    pub const AGENT_ALL: Permissions = Permissions(AGENT);
    /// An agent that acts only while a person watches.
    pub const AGENT_WATCHED: Permissions = Permissions(VIEW | KEYBOARD | MOUSE);

    pub const fn from_bits(bits: u16) -> Permissions {
        Permissions(bits)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Everything in `bits` is allowed.
    pub const fn allows(self, bits: u16) -> bool {
        self.0 & bits == bits
    }

    /// Any of `bits` is allowed.
    pub const fn allows_any(self, bits: u16) -> bool {
        self.0 & bits != 0
    }

    pub const fn with(self, bits: u16, on: bool) -> Permissions {
        if on {
            Permissions(self.0 | bits)
        } else {
            Permissions(self.0 & !bits)
        }
    }

    /// What can be allowed to a device of this kind.
    pub const fn possible(agent: bool) -> Permissions {
        if agent {
            Permissions(AGENT)
        } else {
            Permissions(PERSON)
        }
    }

    /// Only what a device of this kind can be allowed.
    pub const fn fit(self, agent: bool) -> Permissions {
        Permissions(self.0 & Self::possible(agent).0)
    }

    /// What a device gets when it pairs. A person's: everything when no
    /// other person's device is paired, else see only, as Apollo grants
    /// its first client `_all` and later ones `_default`
    /// (`nvhttp.cpp`, `clientpairingsecret`). An agent's: see and act, as
    /// agents could before permissions; the person pairing it sees it is
    /// an agent, and can choose less.
    pub const fn on_pairing(agent: bool, people_paired: usize) -> Permissions {
        if agent {
            Self::AGENT_ALL
        } else if people_paired == 0 {
            Self::PERSON_ALL
        } else {
            Self::SEE_ONLY
        }
    }

    /// The names of what is allowed, in [`NAMES`] order.
    pub fn names(self) -> Vec<&'static str> {
        NAMES
            .iter()
            .filter(|(bit, _)| self.allows(*bit))
            .map(|(_, name)| *name)
            .collect()
    }

    /// The set these names make. Names this version does not know (a
    /// newer host's) are left out: a permission not understood is not
    /// granted.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Permissions {
        let mut bits = 0;
        for name in names {
            if let Some((bit, _)) = NAMES.iter().find(|(_, n)| *n == name) {
                bits |= bit;
            }
        }
        Permissions(bits)
    }

    /// Like [`Permissions::from_names`], but an unknown name is an error,
    /// for what a person typed (`all`, `none` and `see-only` are sets).
    pub fn parse(text: &str) -> Result<Permissions, String> {
        let mut bits = 0;
        for name in text.split([',', ' ']).filter(|n| !n.is_empty()) {
            bits |= match name {
                "all" => PERSON | AGENT,
                "none" => 0,
                "see-only" => VIEW,
                _ => match NAMES.iter().find(|(_, n)| *n == name) {
                    Some((bit, _)) => *bit,
                    None => {
                        return Err(format!(
                            "unknown permission {name:?}: use all, none, see-only, or {}",
                            NAMES.map(|(_, n)| n).join(", ")
                        ))
                    }
                },
            };
        }
        Ok(Permissions(bits))
    }
}

impl serde::Serialize for Permissions {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.names())
    }
}

impl<'de> serde::Deserialize<'de> for Permissions {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Permissions, D::Error> {
        let names = Vec::<String>::deserialize(d)?;
        Ok(Permissions::from_names(names.iter().map(String::as_str)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_person_gets_everything_and_later_ones_see_only() {
        assert_eq!(Permissions::on_pairing(false, 0), Permissions::PERSON_ALL);
        assert_eq!(Permissions::on_pairing(false, 1), Permissions::SEE_ONLY);
        // Agents do not count, and get what they could do before.
        assert_eq!(Permissions::on_pairing(true, 0), Permissions::AGENT_ALL);
        assert_eq!(Permissions::on_pairing(true, 3), Permissions::AGENT_ALL);
    }

    #[test]
    fn names_round_trip_and_unknown_ones_grant_nothing() {
        for p in [
            Permissions::PERSON_ALL,
            Permissions::PERSON_CONTROL,
            Permissions::SEE_ONLY,
            Permissions::AGENT_ALL,
            Permissions::NONE,
        ] {
            assert_eq!(Permissions::from_names(p.names()), p);
        }
        assert_eq!(
            Permissions::from_names(["view", "teleport"]),
            Permissions::SEE_ONLY
        );
        let json = serde_json::to_string(&Permissions::PERSON_CONTROL).unwrap();
        assert_eq!(json, r#"["view","keyboard","mouse","controller"]"#);
        let back: Permissions = serde_json::from_str(r#"["mouse","view","fly"]"#).unwrap();
        assert_eq!(back, Permissions::from_bits(VIEW | MOUSE));
    }

    #[test]
    fn what_a_person_types_is_checked() {
        assert_eq!(
            Permissions::parse("view,keyboard mouse").unwrap(),
            Permissions::from_bits(VIEW | KEYBOARD | MOUSE)
        );
        assert_eq!(
            Permissions::parse("all").unwrap().fit(false),
            Permissions::PERSON_ALL
        );
        assert_eq!(
            Permissions::parse("all").unwrap().fit(true),
            Permissions::AGENT_ALL
        );
        assert_eq!(Permissions::parse("none").unwrap(), Permissions::NONE);
        assert!(Permissions::parse("view,root").is_err());
    }

    #[test]
    fn an_agent_can_be_allowed_only_what_its_rules_permit() {
        let p = Permissions::PERSON_ALL.with(UNWATCHED, true).fit(true);
        assert_eq!(p, Permissions::AGENT_ALL);
        assert!(!p.allows_any(CLIPBOARD_READ | CLIPBOARD_WRITE | WATCH | TAKE_OVER));
        assert!(!Permissions::AGENT_ALL.fit(false).allows(UNWATCHED));
    }
}
