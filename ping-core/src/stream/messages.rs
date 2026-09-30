//! What the person streaming is told: why a session ended or was refused,
//! what a host's warning means, and who drives an agent's session.

use pingpong_proto::control::{agent_state, AckStatus, AgentState, EndReason};

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
    let what = if held.is_empty() {
        "working".to_string()
    } else {
        held.join(", ")
    };
    format!("Watching the agent on {host} ({what}). Ctrl+Alt+Shift+T takes over.")
}

/// What a refused session means, for the person who asked for it.
pub(super) fn refusal(status: AckStatus) -> String {
    match status {
        AckStatus::NothingToWatch => "No AI agent is working on the host, so there is \
            nothing to watch."
            .into(),
        AckStatus::AgentNotAllowed => {
            "The host turned the agent away: a person is using it, agents are off there, \
                or this agent's access is off (Pong's web UI, Devices)."
                .into()
        }
        AckStatus::Busy => "The host is streaming to another device.".into(),
        AckStatus::NoCodec => "The host cannot encode video in a format this Mac can play.".into(),
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
        _ => format!("The agent's session on {host} ended."),
    }
}
