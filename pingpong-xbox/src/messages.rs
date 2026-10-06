//! The message and control channels: JSON, one message per data channel
//! message.
//!
//! - **message** (`messageV1`): a handshake, then messages addressed to a
//!   path (`/streaming/characteristics/dimensionschanged`) whose content is
//!   itself JSON, as a string. The console opens transactions the client
//!   completes (a disconnect, a dialog). Greenlight's
//!   `packages/player/src/client/lib/channel/message.ts`.
//! - **control** (`controlV1`): the client authorizes itself, says which
//!   controllers are plugged in, and asks for keyframes (`.../control.ts`).
//!
//! The constants here are the web client's, which the console expects to
//! see; their meaning is noted where it is known.

use serde::Deserialize;
use serde_json::{json, Value};

/// The data channels, in the order the client opens them (the web client's
/// order: the console tells them apart by label, but nothing is gained by
/// differing).
pub const CHANNELS: [(&str, &str); 4] = [
    ("input", "1.0"),
    ("chat", "chatV1"),
    ("control", "controlV1"),
    ("message", "messageV1"),
];

/// The protocol versions the client offers with its SDP, per channel
/// (`xcloudapi.ts` `sendSdp`).
pub fn channel_versions() -> Value {
    json!({
        "chatConfiguration": {
            "bytesPerSample": 2,
            "expectedClipDurationMs": 20,
            "format": { "codec": "opus", "container": "webm" },
            "numChannels": 1,
            "sampleFrequencyHz": 24000
        },
        "chat": { "minVersion": 1, "maxVersion": 1 },
        "control": { "minVersion": 1, "maxVersion": 3 },
        "input": { "minVersion": 1, "maxVersion": 8 },
        "message": { "minVersion": 1, "maxVersion": 1 }
    })
}

/// The first message on the message channel.
pub fn handshake(id: &str) -> String {
    json!({ "type": "Handshake", "version": "messageV1", "id": id, "cv": "0" }).to_string()
}

/// A message to `target`, whose content is `data` as a JSON string.
pub fn message(id: &str, target: &str, data: &Value) -> String {
    json!({
        "type": "Message",
        "content": data.to_string(),
        "id": id,
        "target": target,
        "cv": ""
    })
    .to_string()
}

/// Ends the console's transaction `id` with `data`.
pub fn complete_transaction(id: &str, data: &Value) -> String {
    json!({ "type": "TransactionComplete", "content": data.to_string(), "id": id, "cv": "" })
        .to_string()
}

/// Turns down the console's transaction `id` (a dialog the client does not
/// show).
pub fn cancel_transaction(id: &str) -> String {
    json!({ "type": "ReceiverCancel", "content": "\"\"", "id": id, "cv": "" }).to_string()
}

/// What the client tells the console about itself once the handshake is
/// done: no system UI of its own (the console draws its dialogs and
/// keyboard into the picture), the install id, landscape, no touch, and the
/// size it shows the picture at.
pub fn client_configuration(
    install_id: &str,
    width: u32,
    height: u32,
) -> [(&'static str, Value); 6] {
    [
        (
            "/streaming/systemUi/configuration",
            json!({ "version": [0, 2, 0], "systemUis": [] }),
        ),
        (
            "/streaming/properties/clientappinstallidchanged",
            json!({ "clientAppInstallId": install_id }),
        ),
        (
            "/streaming/characteristics/orientationchanged",
            json!({ "orientation": 0 }),
        ),
        (
            "/streaming/characteristics/touchinputenabledchanged",
            json!({ "touchInputEnabled": false }),
        ),
        (
            "/streaming/characteristics/clientdevicecapabilities",
            json!({}),
        ),
        (
            "/streaming/characteristics/dimensionschanged",
            json!({
                "horizontal": width,
                "vertical": height,
                "preferredWidth": width,
                "preferredHeight": height,
                "safeAreaLeft": 0,
                "safeAreaTop": 0,
                "safeAreaRight": width,
                "safeAreaBottom": height,
                "supportsCustomResolution": true
            }),
        ),
    ]
}

/// What came in on the message channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incoming {
    HandshakeAck,
    /// The console ends the stream (another device took the console, it
    /// went to sleep, the game quit to the dashboard on xCloud).
    Disconnect {
        transaction: String,
    },
    /// The title being played changed.
    TitleInfo(String),
    /// A dialog for the client to show (only sent to clients that say they
    /// draw dialogs, which Ping does not).
    Dialog {
        transaction: String,
    },
    /// A transaction the client does not know: it is turned down, so the
    /// console does not wait on it.
    OtherTransaction {
        transaction: String,
        target: String,
    },
    Other,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    content: String,
}

/// Read a message from the console; `None` if it is not JSON of the
/// expected shape.
pub fn parse_incoming(d: &[u8]) -> Option<Incoming> {
    let e: Envelope = serde_json::from_slice(d).ok()?;
    Some(match (e.kind.as_str(), e.target.as_str()) {
        ("HandshakeAck", _) => Incoming::HandshakeAck,
        (_, "/streaming/sessionLifetimeManagement/serverInitiatedDisconnect") => {
            Incoming::Disconnect { transaction: e.id }
        }
        (_, "/streaming/properties/titleinfo") => Incoming::TitleInfo(e.content),
        ("TransactionStart", "/streaming/systemUi/messages/ShowMessageDialog") => {
            Incoming::Dialog { transaction: e.id }
        }
        ("TransactionStart", _) => Incoming::OtherTransaction {
            transaction: e.id,
            target: e.target,
        },
        _ => Incoming::Other,
    })
}

/// The control channel's messages.
pub mod control {
    use serde_json::json;

    /// The key the web client authorizes itself with: the same for every
    /// client, so it says which kind of client this is, not who.
    const ACCESS_KEY: &str = "4BDB3609-C1F1-4195-9B37-FEFF45DA8B8E";

    pub fn authorization() -> String {
        json!({ "message": "authorizationRequest", "accessKey": ACCESS_KEY }).to_string()
    }

    /// Controller `index` was plugged in, or unplugged.
    pub fn gamepad_changed(index: u8, added: bool) -> String {
        json!({ "message": "gamepadChanged", "gamepadIndex": index, "wasAdded": added }).to_string()
    }

    /// Ask for a keyframe: `idr`, a full one, as after a loss the decoder
    /// cannot recover from.
    pub fn keyframe_request(idr: bool) -> String {
        json!({ "message": "videoKeyframeRequested", "ifrRequested": idr }).to_string()
    }
}

/// What the mock console reads on the control channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlMessage {
    Authorization { key: String },
    GamepadChanged { index: u8, added: bool },
    KeyframeRequest { idr: bool },
    Other(String),
}

pub fn parse_control(d: &[u8]) -> Option<ControlMessage> {
    let v: Value = serde_json::from_slice(d).ok()?;
    let msg = v.get("message")?.as_str()?;
    Some(match msg {
        "authorizationRequest" => ControlMessage::Authorization {
            key: v.get("accessKey")?.as_str()?.to_owned(),
        },
        "gamepadChanged" => ControlMessage::GamepadChanged {
            index: u8::try_from(v.get("gamepadIndex")?.as_u64()?).ok()?,
            added: v.get("wasAdded")?.as_bool()?,
        },
        "videoKeyframeRequested" => ControlMessage::KeyframeRequest {
            idr: v.get("ifrRequested")?.as_bool()?,
        },
        other => ControlMessage::Other(other.to_owned()),
    })
}

/// What the mock console reads on the message channel: the type, the
/// target and the content of each message.
pub fn parse_envelope(d: &[u8]) -> Option<(String, String, String, String)> {
    let e: Envelope = serde_json::from_slice(d).ok()?;
    Some((e.kind, e.id, e.target, e.content))
}

/// The console's side of the message channel, as the mock console writes
/// it.
pub mod console {
    use serde_json::json;

    pub fn handshake_ack(id: &str) -> String {
        json!({ "type": "HandshakeAck", "id": id, "cv": "0" }).to_string()
    }

    pub fn disconnect(id: &str) -> String {
        json!({
            "type": "TransactionStart",
            "id": id,
            "target": "/streaming/sessionLifetimeManagement/serverInitiatedDisconnect",
            "content": "{\"reason\":\"Mock\"}",
            "cv": ""
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_messages_content_is_json_inside_a_string() {
        let m = message("id-1", "/streaming/x", &json!({ "a": 1 }));
        let v: Value = serde_json::from_str(&m).unwrap();
        assert_eq!(v["type"], "Message");
        assert_eq!(v["target"], "/streaming/x");
        assert_eq!(v["content"], "{\"a\":1}");
        let (kind, id, target, content) = parse_envelope(m.as_bytes()).unwrap();
        assert_eq!((kind.as_str(), id.as_str()), ("Message", "id-1"));
        assert_eq!(
            (target.as_str(), content.as_str()),
            ("/streaming/x", "{\"a\":1}")
        );
    }

    #[test]
    fn the_console_ends_a_stream_with_a_transaction() {
        let d = console::disconnect("t-9");
        assert_eq!(
            parse_incoming(d.as_bytes()),
            Some(Incoming::Disconnect {
                transaction: "t-9".into()
            })
        );
        assert_eq!(
            parse_incoming(console::handshake_ack("h").as_bytes()),
            Some(Incoming::HandshakeAck)
        );
        let other = r#"{"type":"TransactionStart","id":"q","target":"/streaming/new"}"#;
        assert_eq!(
            parse_incoming(other.as_bytes()),
            Some(Incoming::OtherTransaction {
                transaction: "q".into(),
                target: "/streaming/new".into()
            })
        );
        assert_eq!(parse_incoming(b"not json"), None);
        assert_eq!(parse_incoming(b"{\"no\":\"type\"}"), None);
    }

    #[test]
    fn the_dimensions_say_the_size_the_picture_is_shown_at() {
        let config = client_configuration("install", 2560, 1440);
        let (target, dims) = &config[5];
        assert_eq!(*target, "/streaming/characteristics/dimensionschanged");
        assert_eq!(dims["horizontal"], 2560);
        assert_eq!(dims["safeAreaBottom"], 1440);
        assert_eq!(config[1].1["clientAppInstallId"], "install");
    }

    #[test]
    fn control_messages_round_trip() {
        assert_eq!(
            parse_control(control::gamepad_changed(2, false).as_bytes()),
            Some(ControlMessage::GamepadChanged {
                index: 2,
                added: false
            })
        );
        assert_eq!(
            parse_control(control::keyframe_request(true).as_bytes()),
            Some(ControlMessage::KeyframeRequest { idr: true })
        );
        assert!(matches!(
            parse_control(control::authorization().as_bytes()),
            Some(ControlMessage::Authorization { .. })
        ));
        assert_eq!(
            parse_control(br#"{"message":"gamepadChanged","gamepadIndex":300,"wasAdded":true}"#),
            None
        );
    }
}
