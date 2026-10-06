//! ICE candidates as the streaming service exchanges them, and the
//! console's Teredo addresses.
//!
//! The client posts its candidates and reads the console's back, as JSON
//! with the SDP candidate line in `candidate` (`xcloudapi.ts` `sendIce`).
//! A console reachable over the internet has an IPv6 Teredo address among
//! its candidates (`2001:0::/32`); its NAT's public IPv4 address and port
//! are inside it, obscured, and the client adds them as candidates of their
//! own, as Greenlight does (`teredo.ts`, `ice.ts`): the address at the
//! mapped port, and at 9002, the port consoles stream from.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The port Xbox consoles stream from.
pub const CONSOLE_PORT: u16 = 9002;

/// One candidate, as the service carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidate {
    /// The SDP line, with or without its `a=` (the console's have it).
    pub candidate: String,
    #[serde(default)]
    pub sdp_mid: Option<String>,
    /// A number from browsers, a string from the console: kept as it came.
    #[serde(default)]
    pub sdp_m_line_index: Option<Value>,
}

impl IceCandidate {
    /// One of ours, on the first m-line (which carries every transport:
    /// the session is bundled).
    pub fn local(sdp: String) -> IceCandidate {
        IceCandidate {
            candidate: sdp,
            sdp_mid: Some("0".into()),
            sdp_m_line_index: Some(Value::from(0)),
        }
    }

    /// The candidate line without its `a=`, as SDP parsers take it; `None`
    /// for the end-of-candidates marker.
    pub fn line(&self) -> Option<&str> {
        let line = self.candidate.trim();
        let line = line.strip_prefix("a=").unwrap_or(line);
        (line.starts_with("candidate:")).then_some(line)
    }

    /// The candidate's transport address.
    pub fn address(&self) -> Option<SocketAddr> {
        let mut f = self.line()?.split_whitespace();
        let ip = f.nth(4)?.parse().ok()?;
        let port = f.next()?.parse().ok()?;
        Some(SocketAddr::new(ip, port))
    }
}

/// The service's answer: the candidates as a JSON string.
pub fn parse_exchange(exchange_response: &str) -> Result<Vec<IceCandidate>, String> {
    serde_json::from_str(exchange_response)
        .map_err(|e| format!("the console's network addresses were not readable: {e}"))
}

/// The public IPv4 address and port inside a Teredo address (RFC 4380 §4:
/// the port and address are stored inverted).
pub fn teredo_client(addr: Ipv6Addr) -> Option<SocketAddrV4> {
    let s = addr.segments();
    if s[0] != 0x2001 || s[1] != 0 {
        return None;
    }
    let port = !s[5];
    let ip = !(((s[6] as u32) << 16) | s[7] as u32);
    Some(SocketAddrV4::new(Ipv4Addr::from(ip), port))
}

/// The console's candidates, with the IPv4 candidates its Teredo addresses
/// stand for added before them (they are tried as well as the Teredo ones,
/// which need a Teredo client this computer does not have).
pub fn with_teredo(remote: &[IceCandidate]) -> Vec<String> {
    let mut lines = Vec::new();
    for c in remote {
        let Some(line) = c.line() else {
            continue;
        };
        if let Some(SocketAddr::V6(a)) = c.address() {
            if let Some(v4) = teredo_client(*a.ip()) {
                // Lowest priority: they are guesses beside the console's own.
                for (foundation, port) in [(10, CONSOLE_PORT), (11, v4.port())] {
                    lines.push(format!(
                        "candidate:{foundation} 1 UDP 1 {} {port} typ host",
                        v4.ip()
                    ));
                }
            }
        }
        lines.push(line.to_owned());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn teredo_addresses_hold_the_public_address_inverted() {
        // RFC 4380 §4's example: client 192.0.2.45:40000 behind a cone NAT.
        let a: Ipv6Addr = "2001:0:4136:e378:8000:63bf:3fff:fdd2".parse().unwrap();
        assert_eq!(teredo_client(a), Some("192.0.2.45:40000".parse().unwrap()));
        assert_eq!(teredo_client("2001:db8::1".parse().unwrap()), None);
        assert_eq!(teredo_client("fe80::1".parse().unwrap()), None);
    }

    #[test]
    fn the_consoles_candidates_parse_with_their_a_equals() {
        let json = r#"[
            {"candidate":"a=candidate:1 1 UDP 100 192.168.1.20 9002 typ host ","messageType":"iceCandidate","sdpMLineIndex":"0","sdpMid":"0"},
            {"candidate":"a=candidate:2 1 UDP 1 2001:0:4136:e378:8000:63bf:3fff:fdd2 9002 typ host ","sdpMLineIndex":"0","sdpMid":"0"},
            {"candidate":"a=end-of-candidates","sdpMLineIndex":"0","sdpMid":"0"}
        ]"#;
        let remote = parse_exchange(json).unwrap();
        assert_eq!(remote.len(), 3);
        assert_eq!(
            remote[0].address(),
            Some("192.168.1.20:9002".parse().unwrap())
        );
        assert_eq!(remote[2].line(), None);
        let lines = with_teredo(&remote);
        assert_eq!(
            lines,
            vec![
                "candidate:1 1 UDP 100 192.168.1.20 9002 typ host".to_string(),
                "candidate:10 1 UDP 1 192.0.2.45 9002 typ host".into(),
                "candidate:11 1 UDP 1 192.0.2.45 40000 typ host".into(),
                "candidate:2 1 UDP 1 2001:0:4136:e378:8000:63bf:3fff:fdd2 9002 typ host".into(),
            ]
        );
    }

    #[test]
    fn our_candidates_go_out_on_the_first_m_line() {
        let c = IceCandidate::local("candidate:1 1 udp 2130706431 10.0.0.2 50000 typ host".into());
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["sdpMid"], "0");
        assert_eq!(v["sdpMLineIndex"], 0);
        assert!(v["candidate"].as_str().unwrap().starts_with("candidate:"));
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse_exchange("{").is_err());
        let odd = IceCandidate {
            candidate: "candidate:1 1 UDP".into(),
            sdp_mid: None,
            sdp_m_line_index: None,
        };
        assert_eq!(odd.address(), None);
        assert_eq!(with_teredo(&[odd]).len(), 1);
    }
}
