//! What the person streaming is told: why a session ended or was refused,
//! what a host's warning means, what the host's permissions for this device
//! hold back, and who drives an agent's session.

use pingpong_proto::control::{agent_state, AckStatus, AgentState, EndReason};
use pingpong_proto::permission::{self, Permissions};

/// Where on the host what a device may do is set.
const WHERE_TO_ALLOW: &str = "in Pong on the host, under Devices";

/// What a host's warning means, for the person streaming.
pub(super) fn host_warning(code: u8, host: &str) -> Option<String> {
    use pingpong_proto::control::host_warning;
    match code {
        host_warning::INPUT_BLOCKED => Some(format!(
            "{host} is ignoring your keyboard and mouse: on that Mac, allow Pong in \
                System Settings > Privacy & Security > Accessibility"
        )),
        host_warning::CAPTURE_BLOCKED => Some(format!(
            "{host} is not allowed to record its screen: on that Mac, allow Pong in \
                System Settings > Privacy & Security > Screen & System Audio Recording, then try again"
        )),
        _ => None,
    }
}

/// What a watcher is told about the agent's session.
pub(super) fn watch_notice(state: AgentState, host: &str) -> String {
    if state.flags & agent_state::TAKEN_OVER != 0 {
        return "You have the keyboard and mouse. Ctrl+Alt+Shift+T hands them back to the agent."
            .into();
    }
    let mut held = Vec::new();
    if state.flags & agent_state::PAUSED != 0 {
        held.push("paused");
    }
    if state.flags & agent_state::SECURE_DESKTOP != 0 {
        held.push("waiting for a person on a secure screen");
    }
    if state.flags & agent_state::LOCAL_INPUT != 0 {
        held.push("waiting while someone uses the host");
    }
    if state.flags & agent_state::VIEW_ONLY != 0 {
        held.push("allowed to look only");
    }
    if state.flags & agent_state::UNWATCHED != 0 {
        held.push("waiting for someone to watch");
    }
    let what = if held.is_empty() {
        "working".to_string()
    } else {
        held.join(", ")
    };
    format!("Watching the agent on {host} ({what}). Ctrl+Alt+Shift+T takes over.")
}

/// What the host's permissions for this device hold back of a person's
/// stream: the input it ignores. None when it takes the keyboard and mouse
/// (controllers alone are not worth a word to someone who may have none).
pub(super) fn held_back(bits: u16, host: &str) -> Option<String> {
    let p = Permissions::from_bits(bits);
    if p.allows(permission::KEYBOARD | permission::MOUSE) {
        return None;
    }
    let ignored: Vec<&str> = [
        (permission::KEYBOARD, "keyboard"),
        (permission::MOUSE, "mouse"),
        (permission::CONTROLLER, "controllers"),
    ]
    .into_iter()
    .filter(|(bit, _)| !p.allows(*bit))
    .map(|(_, what)| what)
    .collect();
    let what = match ignored.as_slice() {
        [] => return None,
        [one] => one.to_string(),
        [a, b] => format!("{a} and {b}"),
        [a, b, c] => format!("{a}, {b} and {c}"),
        _ => unreachable!("three kinds of input"),
    };
    Some(format!(
        "{host} ignores your {what}: this device may not use them there. That is set \
            {WHERE_TO_ALLOW}."
    ))
}

/// What a refused session means, for the person who asked for it (`watch`:
/// they asked to watch an agent).
pub(super) fn refusal(status: AckStatus, watch: bool, host: &str) -> String {
    match status {
        AckStatus::NotAllowed if watch => format!(
            "{host} does not let this device watch AI agents. That can be allowed \
                {WHERE_TO_ALLOW}."
        ),
        AckStatus::NotAllowed => {
            format!("{host} does not let this device stream. That can be allowed {WHERE_TO_ALLOW}.")
        }
        AckStatus::AppNotAllowed => format!(
            "{host} does not let this device start apps. Stream the desktop instead, or \
                allow it {WHERE_TO_ALLOW}."
        ),
        AckStatus::Other => format!(
            "{host} turned the session down for a reason this version of Ping does not \
                know. Update Ping."
        ),
        AckStatus::NothingToWatch => "No AI agent is working on the host, so there is \
            nothing to watch."
            .into(),
        AckStatus::AgentNotAllowed => format!(
            "{host} turned the agent away: a person is using it, agents are off there, or \
                this agent may not see its screen (its permissions, {WHERE_TO_ALLOW})."
        ),
        AckStatus::Busy => "The host is streaming to another device.".into(),
        AckStatus::NoCodec => {
            "The host cannot encode video in a format this computer can play.".into()
        }
        AckStatus::VddUnavailable => {
            "The host could not create its virtual display. Try again; if it keeps \
                failing, restart Pong on the host."
                .into()
        }
        AckStatus::ModeUnsupported => "The host cannot show this resolution and frame rate.".into(),
        AckStatus::Failed => "The host could not start capturing its screen. Try again; if \
            it keeps failing, restart Pong on the host."
            .into(),
        AckStatus::Ok => String::new(),
    }
}

/// Why the host ended a person's session.
pub(super) fn end_text(reason: EndReason, host: &str) -> String {
    match reason {
        EndReason::Replaced => "Another client took over the host.".to_string(),
        EndReason::HostInUse => {
            format!("Someone is using {host} itself; the session ended so they have it.")
        }
        EndReason::Shutdown => {
            format!("{host} is shutting down, restarting, or Pong was stopped on it.")
        }
        EndReason::Error => "The host stopped the stream after an error.".to_string(),
        EndReason::NotAllowed => format!(
            "{host} no longer lets this device stream: what it may do there changed \
                {WHERE_TO_ALLOW}."
        ),
        _ => "The host ended the session.".to_string(),
    }
}

/// Why the host ended the agent's session a watcher was watching.
pub(super) fn watcher_end_text(reason: EndReason, host: &str) -> String {
    match reason {
        EndReason::Replaced => {
            format!("A person started streaming {host}; the agent's session is over.")
        }
        EndReason::Shutdown => {
            format!("{host} is shutting down, restarting, or Pong was stopped on it.")
        }
        EndReason::Error => "The host stopped the agent's session after an error.".to_string(),
        EndReason::NotAllowed => format!(
            "{host} no longer lets this device watch AI agents: that changed {WHERE_TO_ALLOW}."
        ),
        _ => format!("The agent's session on {host} ended."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_host_ignores_is_named() {
        let see_only = Permissions::SEE_ONLY.bits();
        assert_eq!(
            held_back(see_only, "gaming-pc").unwrap(),
            "gaming-pc ignores your keyboard, mouse and controllers: this device may not use \
                them there. That is set in Pong on the host, under Devices."
        );
        let no_mouse = Permissions::PERSON_ALL
            .with(permission::MOUSE, false)
            .bits();
        assert!(held_back(no_mouse, "pc")
            .unwrap()
            .starts_with("pc ignores your mouse:"));
        assert_eq!(held_back(Permissions::PERSON_CONTROL.bits(), "pc"), None);
        let no_pads = Permissions::PERSON_ALL.with(permission::CONTROLLER, false);
        assert_eq!(held_back(no_pads.bits(), "pc"), None);
    }
}
