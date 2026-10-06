//! The account's consoles, and commands to them, through Xbox Live's
//! remote management service (`xccs.xboxlive.com`, which the Xbox app's
//! "remote features" use): the list Greenlight shows
//! (`xbox-webapi`'s `smartglass.js`), and power on and off.
//!
//! A console in "sleep" (the Xbox's standby power mode) wakes for a
//! command; one powered off at the wall, or in energy-saving mode, does
//! not.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{Auth, AuthError};
use crate::http::Body;

const HOST: &str = "xccs.xboxlive.com";

/// A console, as the account sees it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Console {
    /// The console's id: what a stream is started with.
    pub id: String,
    pub name: String,
    /// `XboxOne`, `XboxOneS`, `XboxOneX`, `XboxSeriesS`, `XboxSeriesX`, …
    #[serde(default)]
    pub console_type: String,
    /// `On`, `ConnectedStandby` (asleep, wakes for a stream), `Off`, or
    /// `Unknown`.
    #[serde(default)]
    pub power_state: String,
    /// "Remote features" are on: it can be woken and commanded.
    #[serde(default)]
    pub remote_management_enabled: bool,
    /// "Allow game streaming to other devices" is on.
    #[serde(default)]
    pub console_streaming_enabled: bool,
    /// The service thinks the console is on another network than this
    /// computer.
    #[serde(default)]
    pub out_of_home_warning: bool,
}

impl Console {
    /// The kind of console, in words.
    pub fn model(&self) -> &str {
        match self.console_type.as_str() {
            "XboxOne" => "Xbox One",
            "XboxOneS" => "Xbox One S",
            "XboxOneX" => "Xbox One X",
            "XboxSeriesS" => "Xbox Series S",
            "XboxSeriesX" => "Xbox Series X",
            "" => "Xbox",
            other => other,
        }
    }

    pub fn is_on(&self) -> bool {
        self.power_state == "On"
    }

    /// Asleep, and so woken by a command or a stream.
    pub fn is_asleep(&self) -> bool {
        self.power_state == "ConnectedStandby"
    }

    /// Why a stream from this console would fail before it starts, if one
    /// would.
    pub fn cannot_stream(&self) -> Option<&'static str> {
        if !self.console_streaming_enabled {
            return Some(
                "Turn on remote play on the console: Settings > Devices & connections > \
                 Remote features > Enable remote features.",
            );
        }
        if self.power_state == "Off" {
            return Some(
                "The console is off. Set its power mode to Sleep (Settings > General > Power \
                 options) so it can be woken from here.",
            );
        }
        None
    }

    /// In a line, for lists.
    pub fn state(&self) -> &'static str {
        match self.power_state.as_str() {
            "On" => "On",
            "ConnectedStandby" => "Asleep",
            "Off" => "Off",
            _ => "Unknown",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    error_code: String,
    #[serde(default)]
    error_message: Option<String>,
}

#[derive(Deserialize)]
struct ListAnswer {
    status: Status,
    #[serde(default)]
    result: Vec<Console>,
}

/// A command to a console.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    WakeUp,
    TurnOff,
    Reboot,
}

impl Command {
    fn kind_and_name(self) -> (&'static str, &'static str) {
        match self {
            Command::WakeUp => ("Power", "WakeUp"),
            Command::TurnOff => ("Power", "TurnOff"),
            Command::Reboot => ("Power", "Reboot"),
        }
    }
}

fn headers(auth: &mut Auth) -> Result<String, AuthError> {
    Ok(auth.web_token()?.authorization())
}

/// The account's consoles.
pub fn list(auth: &mut Auth) -> Result<Vec<Console>, AuthError> {
    let authorization = headers(auth)?;
    let answer = auth.http.ok(
        "GET",
        HOST,
        "/lists/devices?queryCurrentDevice=false&includeStorageDevices=false",
        &[
            ("Authorization", &authorization),
            ("x-xbl-contract-version", "4"),
            ("skillplatform", "RemoteManagement"),
            ("Accept-Language", "en-US"),
        ],
        Body::None,
    )?;
    parse_list(&answer.body)
}

/// Read the console list.
pub fn parse_list(body: &str) -> Result<Vec<Console>, AuthError> {
    let a: ListAnswer = serde_json::from_str(body)
        .map_err(|e| AuthError::Failed(format!("The console list could not be read: {e}")))?;
    if a.status.error_code != "OK" {
        return Err(AuthError::Failed(format!(
            "The console list was refused: {}",
            a.status.error_message.unwrap_or(a.status.error_code)
        )));
    }
    Ok(a.result)
}

/// Send `command` to the console `id`.
pub fn send(auth: &mut Auth, id: &str, command: Command) -> Result<(), AuthError> {
    let authorization = headers(auth)?;
    let (kind, name) = command.kind_and_name();
    let body = json!({
        "destination": "Xbox",
        "type": kind,
        "command": name,
        "sessionId": crate::uuid_v4(),
        "sourceId": "com.microsoft.smartglass",
        "parameters": [],
        "linkedXboxId": id,
    });
    let answer = auth.http.ok(
        "POST",
        HOST,
        "/commands",
        &[
            ("Authorization", &authorization),
            ("x-xbl-contract-version", "4"),
            ("skillplatform", "RemoteManagement"),
        ],
        Body::Json(&body),
    )?;
    let status = serde_json::from_str::<Value>(&answer.body)
        .ok()
        .and_then(|v| {
            v.get("status")?
                .get("errorCode")?
                .as_str()
                .map(str::to_owned)
        });
    match status.as_deref() {
        Some("OK") | None => Ok(()),
        Some(code) => Err(AuthError::Failed(format!(
            "The console did not take the command ({code})."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_reads_the_services_fields() {
        let body = r#"{"status":{"errorCode":"OK","errorMessage":null},"result":[
            {"id":"F4001234ABCD","name":"Living room","locale":"en-US","region":"","consoleType":"XboxSeriesX",
             "powerState":"ConnectedStandby","digitalAssistantRemoteControlEnabled":false,
             "remoteManagementEnabled":true,"consoleStreamingEnabled":true,"wirelessWarning":false,
             "outOfHomeWarning":false,"storageDevices":[]}],"agentUserId":null}"#;
        let list = parse_list(body).unwrap();
        assert_eq!(list.len(), 1);
        let c = &list[0];
        assert_eq!(c.id, "F4001234ABCD");
        assert_eq!(c.model(), "Xbox Series X");
        assert!(c.is_asleep());
        assert_eq!(c.state(), "Asleep");
        assert_eq!(c.cannot_stream(), None);
    }

    #[test]
    fn a_console_without_remote_play_says_how_to_turn_it_on() {
        let c = Console {
            id: "x".into(),
            name: "x".into(),
            console_type: "XboxOne".into(),
            power_state: "On".into(),
            remote_management_enabled: true,
            console_streaming_enabled: false,
            out_of_home_warning: false,
        };
        assert!(c.cannot_stream().unwrap().contains("Remote features"));
    }

    #[test]
    fn a_refused_list_is_an_error() {
        let body = r#"{"status":{"errorCode":"Forbidden","errorMessage":"no"},"result":[]}"#;
        assert!(parse_list(body).is_err());
        assert!(parse_list("[]").is_err());
    }
}
