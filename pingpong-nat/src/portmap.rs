//! Asking the router to forward the tunnel's port, as Sunshine does (`upnp.cpp`): UPnP IGD
//! first, then NAT-PMP. With a mapping, a client anywhere reaches the host
//! at the router's public address, with no hole punching (and past NATs
//! that defeat it on the host's side).
//!
//! Best effort: many routers have neither, and one behind another NAT (the
//! ISP's box, carrier-grade NAT) grants a mapping that opens nothing -- its
//! "public" address is private. That is found out here and the mapping
//! given back, so only a mapping that helps is used.

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

const SSDP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
const SEARCH_FOR: Duration = Duration::from_millis(2500);
const HTTP_TIMEOUT: Duration = Duration::from_secs(4);
const NATPMP_PORT: u16 = 5351;
const DESCRIPTION: &str = "Pong (pingpong streaming)";

/// A port the router forwards to this host.
#[derive(Debug, Clone)]
pub struct Mapping {
    /// Where clients on the internet reach the tunnel.
    pub external: SocketAddr,
    pub internal_port: u16,
    pub lease: Duration,
    how: How,
}

#[derive(Debug, Clone)]
enum How {
    Upnp {
        control: String,
        service: String,
        local: Ipv4Addr,
    },
    NatPmp {
        gateway: Ipv4Addr,
    },
}

impl Mapping {
    /// "UPnP" or "NAT-PMP".
    pub fn protocol(&self) -> &'static str {
        match self.how {
            How::Upnp { .. } => "UPnP",
            How::NatPmp { .. } => "NAT-PMP",
        }
    }

    /// Ask again before the lease runs out (0: permanent, nothing to do).
    pub fn renew(&mut self) -> Result<(), String> {
        let lease = self.lease;
        match &self.how {
            How::Upnp {
                control,
                service,
                local,
            } => add_upnp(
                control,
                service,
                *local,
                self.internal_port,
                self.external.port(),
                lease.as_secs() as u32,
            )
            .map(|_| ()),
            How::NatPmp { gateway } => {
                let (ext, secs) = natpmp_map(
                    *gateway,
                    self.internal_port,
                    self.external.port(),
                    lease.as_secs() as u32,
                )?;
                self.external.set_port(ext);
                self.lease = Duration::from_secs(secs as u64);
                Ok(())
            }
        }
    }

    /// Give the port back.
    pub fn remove(&self) {
        let r = match &self.how {
            How::Upnp {
                control, service, ..
            } => soap(
                control,
                service,
                "DeletePortMapping",
                &[
                    ("NewRemoteHost", String::new()),
                    ("NewExternalPort", self.external.port().to_string()),
                    ("NewProtocol", "UDP".into()),
                ],
            )
            .map(|_| ()),
            How::NatPmp { gateway } => natpmp_map(*gateway, self.internal_port, 0, 0).map(|_| ()),
        };
        match r {
            Ok(()) => tracing::info!(
                protocol = self.protocol(),
                port = self.external.port(),
                "port mapping removed"
            ),
            Err(e) => tracing::debug!(error = e, "removing the port mapping"),
        }
    }
}

/// Map UDP `port` (the tunnel's) on the router for `lease`, at the same
/// port outside if it is free. Err says why not (no router that maps, or
/// one whose mapping would not reach the internet).
pub fn map(port: u16, lease: Duration) -> Result<Mapping, String> {
    let upnp = match map_upnp(port, lease) {
        Ok(m) => return usable(m),
        Err(e) => e,
    };
    let natpmp = match map_natpmp(port, lease) {
        Ok(m) => return usable(m),
        Err(e) => e,
    };
    Err(format!("UPnP: {upnp}; NAT-PMP: {natpmp}"))
}

/// A mapping whose outside address is private opens nothing: the router is
/// behind another NAT.
fn usable(m: Mapping) -> Result<Mapping, String> {
    if is_public(m.external.ip()) {
        return Ok(m);
    }
    let why = format!(
        "the router ({}) says its public address is {}: it is behind another NAT (the \
            ISP's box, or carrier-grade NAT), so a mapping on it would not reach the \
            internet",
        m.protocol(),
        m.external.ip()
    );
    m.remove();
    Err(why)
}

/// An address the internet can reach (not private, shared, loopback,
/// link-local or unspecified).
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || v4.is_documentation()
                // 100.64.0.0/10: carrier-grade NAT.
                || (o[0] == 100 && (o[1] & 0xc0) == 64))
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

// --- UPnP IGD ---------------------------------------------------------------

fn map_upnp(port: u16, lease: Duration) -> Result<Mapping, String> {
    let location = discover()?;
    let (control, service) = igd_service(&location)?;
    let router = url_host(&control).ok_or("a control URL without a host")?;
    let local = local_ip_towards(router)?;
    let external_ip: IpAddr = soap(&control, &service, "GetExternalIPAddress", &[])
        .ok()
        .and_then(|body| xml_value(&body, "NewExternalIPAddress"))
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    // Nothing to map on a router without a public address of its own.
    if !is_public(external_ip) {
        let said = if external_ip.is_unspecified() {
            "none".to_string()
        } else {
            external_ip.to_string()
        };
        return Err(format!(
            "the router has no public address ({said}): it is behind \
                another NAT (the ISP's box, or carrier-grade NAT)"
        ));
    }
    // Another device may hold the port: the next few are as good.
    let mut last = String::new();
    for external in port..port.saturating_add(4) {
        match add_upnp(
            &control,
            &service,
            local,
            port,
            external,
            lease.as_secs() as u32,
        ) {
            Ok(secs) => {
                return Ok(Mapping {
                    external: SocketAddr::new(external_ip, external),
                    internal_port: port,
                    lease: Duration::from_secs(secs as u64),
                    how: How::Upnp {
                        control,
                        service,
                        local,
                    },
                })
            }
            // 718: ConflictInMappingEntry.
            Err(e) if e.contains("718") => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// Add (or refresh) the mapping; the lease granted, in seconds.
fn add_upnp(
    control: &str,
    service: &str,
    local: Ipv4Addr,
    internal: u16,
    external: u16,
    lease: u32,
) -> Result<u32, String> {
    let args = |lease: u32| {
        vec![
            ("NewRemoteHost", String::new()),
            ("NewExternalPort", external.to_string()),
            ("NewProtocol", "UDP".to_string()),
            ("NewInternalPort", internal.to_string()),
            ("NewInternalClient", local.to_string()),
            ("NewEnabled", "1".to_string()),
            ("NewPortMappingDescription", DESCRIPTION.to_string()),
            ("NewLeaseDuration", lease.to_string()),
        ]
    };
    match soap(control, service, "AddPortMapping", &args(lease)) {
        Ok(_) => Ok(lease),
        // 725: OnlyPermanentLeasesSupported.
        Err(e) if e.contains("725") => {
            soap(control, service, "AddPortMapping", &args(0)).map(|_| 0)
        }
        Err(e) => Err(e),
    }
}

/// The first Internet Gateway Device that answers an SSDP search: its
/// description's URL.
fn discover() -> Result<String, String> {
    // From the address that reaches the router, and to the router itself
    // as well as the group: a host with several networks (a VPN, virtual
    // adapters) may send the group's search out of another.
    let gateway = default_gateway();
    let local = gateway
        .and_then(|g| local_ip_towards(g).ok())
        .unwrap_or(Ipv4Addr::UNSPECIFIED);
    let sock = UdpSocket::bind(SocketAddrV4::new(local, 0))
        .or_else(|_| UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))
        .map_err(|e| e.to_string())?;
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| e.to_string())?;
    let targets = [
        "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
        "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
        "urn:schemas-upnp-org:service:WANIPConnection:1",
    ];
    let search = |st: &str| {
        format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nST: {st}\r\nMAN: \
                \"ssdp:discover\"\r\nMX: 2\r\n\r\n"
        )
    };
    let started = Instant::now();
    let mut buf = [0u8; 2048];
    let mut sent = 0;
    while started.elapsed() < SEARCH_FOR {
        // Twice, in case a datagram is lost.
        if sent < 2 && started.elapsed() >= Duration::from_millis(700) * sent {
            for st in targets {
                let _ = sock.send_to(search(st).as_bytes(), SSDP);
                if let Some(g) = gateway {
                    let _ = sock.send_to(search(st).as_bytes(), SocketAddrV4::new(g, 1900));
                }
            }
            sent += 1;
        }
        let Ok((n, _)) = sock.recv_from(&mut buf) else {
            continue;
        };
        let text = String::from_utf8_lossy(&buf[..n]);
        if let Some(loc) = header(&text, "location") {
            return Ok(loc.to_string());
        }
    }
    Err("no router answered a UPnP search".into())
}

fn header<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// The WAN IP (or PPP) connection service in the device's description:
/// its control URL (absolute) and type.
fn igd_service(location: &str) -> Result<(String, String), String> {
    let (status, body) = http(location, "GET", &[], "")?;
    if status != 200 {
        return Err(format!("the router's description: HTTP {status}"));
    }
    let base = xml_value(&body, "URLBase")
        .map(|b| b.trim().trim_end_matches('/').to_string())
        .unwrap_or_else(|| origin(location));
    for block in body.split("<service>").skip(1) {
        let block = block.split("</service>").next().unwrap_or_default();
        let Some(ty) = xml_value(block, "serviceType") else {
            continue;
        };
        if !(ty.contains("WANIPConnection") || ty.contains("WANPPPConnection")) {
            continue;
        }
        let Some(url) = xml_value(block, "controlURL") else {
            continue;
        };
        let url = url.trim();
        let control = if url.starts_with("http://") {
            url.to_string()
        } else {
            format!("{base}/{}", url.trim_start_matches('/'))
        };
        return Ok((control, ty.trim().to_string()));
    }
    Err("the router offers no WAN connection service".into())
}

/// Call `action` on the service; the response body, or its error code.
fn soap(
    control: &str,
    service: &str,
    action: &str,
    args: &[(&str, String)],
) -> Result<String, String> {
    let mut inner = String::new();
    for (k, v) in args {
        inner.push_str(&format!("<{k}>{}</{k}>", xml_escape(v)));
    }
    let body = format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
            s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{service}\">{inner}</u:{action}></s:Body></s:Envelope>\r\n"
    );
    let soap_action = format!("\"{service}#{action}\"");
    let headers = [
        ("Content-Type", "text/xml; charset=\"utf-8\""),
        ("SOAPAction", soap_action.as_str()),
    ];
    let (status, resp) = http(control, "POST", &headers, &body)?;
    if status == 200 {
        return Ok(resp);
    }
    let code = xml_value(&resp, "errorCode").unwrap_or_default();
    let desc = xml_value(&resp, "errorDescription").unwrap_or_default();
    Err(
        format!("{action}: HTTP {status} {} {}", code.trim(), desc.trim())
            .trim()
            .to_string(),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The text of the first `<name>` element (any namespace prefix).
fn xml_value(xml: &str, name: &str) -> Option<String> {
    let mut rest = xml;
    loop {
        let open = rest.find('<')?;
        rest = &rest[open + 1..];
        let end = rest.find('>')?;
        let tag = &rest[..end];
        let local = tag.split_whitespace().next().unwrap_or_default();
        let local = local.rsplit(':').next().unwrap_or(local);
        if local == name && !tag.ends_with('/') {
            let body = &rest[end + 1..];
            let close = body.find("</")?;
            return Some(body[..close].to_string());
        }
    }
}

fn origin(url: &str) -> String {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    let host = rest.split('/').next().unwrap_or(rest);
    format!("http://{host}")
}

fn url_host(url: &str) -> Option<Ipv4Addr> {
    let rest = url.strip_prefix("http://")?;
    let host = rest.split('/').next()?;
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    host.parse().ok()
}

/// A plain HTTP/1.1 request: (status, body).
fn http(
    url: &str,
    method: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Result<(u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("not an http URL: {url}"))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let addr = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    let addr = addr
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or("no address")?;
    let mut s =
        TcpStream::connect_timeout(&addr, HTTP_TIMEOUT).map_err(|e| format!("{addr}: {e}"))?;
    s.set_read_timeout(Some(HTTP_TIMEOUT))
        .map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(HTTP_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {hostport}\r\nConnection: \
            close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    let _ = s.take(1 << 20).read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, content) = text
        .split_once("\r\n\r\n")
        .ok_or("an incomplete HTTP answer")?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or("an HTTP answer without a status")?;
    let chunked =
        header(head, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    Ok((
        status,
        if chunked {
            unchunk(content)
        } else {
            content.to_string()
        },
    ))
}

fn unchunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16) else {
            break;
        };
        if n == 0 || rest.len() < n {
            break;
        }
        out.push_str(&rest[..n]);
        s = rest[n..].trim_start_matches("\r\n");
    }
    out
}

/// This host's address on the network that reaches `to`.
fn local_ip_towards(to: Ipv4Addr) -> Result<Ipv4Addr, String> {
    let s =
        UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
    s.connect(SocketAddrV4::new(to, 1900))
        .map_err(|e| e.to_string())?;
    match s.local_addr().map_err(|e| e.to_string())?.ip() {
        IpAddr::V4(v4) => Ok(v4),
        IpAddr::V6(_) => Err("no IPv4 address towards the router".into()),
    }
}

// --- NAT-PMP (RFC 6886) -----------------------------------------------------

fn map_natpmp(port: u16, lease: Duration) -> Result<Mapping, String> {
    let gateway = default_gateway().ok_or("no default gateway found")?;
    let public = natpmp_public(gateway)?;
    let (external, secs) = natpmp_map(gateway, port, port, lease.as_secs() as u32)?;
    Ok(Mapping {
        external: SocketAddr::new(IpAddr::V4(public), external),
        internal_port: port,
        lease: Duration::from_secs(secs as u64),
        how: How::NatPmp { gateway },
    })
}

/// One request, retried with the RFC's back-off (250 ms, doubling) for a
/// few seconds; the answer.
fn natpmp_ask(gateway: Ipv4Addr, req: &[u8], answer_op: u8) -> Result<Vec<u8>, String> {
    let s =
        UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).map_err(|e| e.to_string())?;
    s.connect(SocketAddrV4::new(gateway, NATPMP_PORT))
        .map_err(|e| e.to_string())?;
    let mut wait = Duration::from_millis(250);
    let mut buf = [0u8; 64];
    for _ in 0..4 {
        s.send(req).map_err(|e| e.to_string())?;
        s.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            let Ok(n) = s.recv(&mut buf) else { break };
            if n >= 4 && buf[0] == 0 && buf[1] == answer_op {
                let result = u16::from_be_bytes([buf[2], buf[3]]);
                if result != 0 {
                    return Err(match result {
                        1 => "unsupported version".into(),
                        2 => "refused (turned off on the router)".into(),
                        3 => "network failure (the router has no public address)".into(),
                        4 => "out of resources".into(),
                        5 => "unsupported request".into(),
                        n => format!("error {n}"),
                    });
                }
                return Ok(buf[..n].to_vec());
            }
        }
        wait *= 2;
    }
    Err("the router did not answer".into())
}

fn natpmp_public(gateway: Ipv4Addr) -> Result<Ipv4Addr, String> {
    let a = natpmp_ask(gateway, &[0, 0], 128)?;
    let ip: [u8; 4] = a
        .get(8..12)
        .ok_or("a short answer")?
        .try_into()
        .map_err(|_| "a short answer")?;
    Ok(Ipv4Addr::from(ip))
}

/// Map UDP `internal` (lifetime 0: remove it); the port outside and the
/// lifetime granted.
fn natpmp_map(
    gateway: Ipv4Addr,
    internal: u16,
    external: u16,
    lifetime: u32,
) -> Result<(u16, u32), String> {
    let mut req = vec![0, 1, 0, 0];
    req.extend_from_slice(&internal.to_be_bytes());
    req.extend_from_slice(&external.to_be_bytes());
    req.extend_from_slice(&lifetime.to_be_bytes());
    let a = natpmp_ask(gateway, &req, 129)?;
    let ext = u16::from_be_bytes(
        a.get(10..12)
            .ok_or("a short answer")?
            .try_into()
            .map_err(|_| "a short answer")?,
    );
    let secs = u32::from_be_bytes(
        a.get(12..16)
            .ok_or("a short answer")?
            .try_into()
            .map_err(|_| "a short answer")?,
    );
    Ok((ext, secs))
}

/// The router: the default route's next hop.
pub fn default_gateway() -> Option<Ipv4Addr> {
    #[cfg(target_os = "linux")]
    {
        let table = std::fs::read_to_string("/proc/net/route").ok()?;
        for line in table.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() > 2 && f[1] == "00000000" {
                let g = u32::from_str_radix(f[2], 16).ok()?;
                if g != 0 {
                    return Some(Ipv4Addr::from(g.to_le_bytes()));
                }
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/sbin/route")
            .args(["-n", "get", "default"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines()
            .find_map(|l| l.trim().strip_prefix("gateway:"))
            .and_then(|g| g.trim().parse().ok())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window flashes on the user's desktop.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("route")
            .args(["print", "-4", "0.0.0.0"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines().find_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 3 && f[0] == "0.0.0.0" && f[1] == "0.0.0.0")
                .then(|| f[2].parse().ok())
                .flatten()
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_addresses() {
        for private in [
            "192.168.0.1",
            "10.1.2.3",
            "172.16.5.4",
            "100.64.1.1",
            "100.127.255.255",
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in ["1.1.1.1", "8.8.8.8", "100.128.0.1", "2606:4700:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
        assert!(!is_public("fd00::1".parse().unwrap()));
        assert!(!is_public("fe80::1".parse().unwrap()));
    }

    #[test]
    fn descriptions_and_answers_are_read() {
        let desc = r#"<?xml version="1.0"?><root><URLBase>http://192.168.0.1:1900/</URLBase><device><serviceList>
            <service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/ctl/L3F</controlURL></service>
            </serviceList><deviceList><device><serviceList>
            <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
            <controlURL>/ctl/IPConn</controlURL></service></serviceList></device></deviceList></device></root>"#;
        assert_eq!(
            xml_value(desc, "URLBase").as_deref(),
            Some("http://192.168.0.1:1900/")
        );
        let answer = r#"<s:Envelope><s:Body><u:GetExternalIPAddressResponse xmlns:u="x"><NewExternalIPAddress>203.0.113.7</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>"#;
        assert_eq!(
            xml_value(answer, "NewExternalIPAddress").as_deref(),
            Some("203.0.113.7")
        );
        let fault = "<s:Fault><detail><UPnPError><errorCode>718</errorCode><errorDescription>ConflictInMappingEntry</errorDescription></UPnPError></detail></s:Fault>";
        assert_eq!(xml_value(fault, "errorCode").as_deref(), Some("718"));
        assert_eq!(
            url_host("http://192.168.0.1:1900/ctl/IPConn"),
            Some(Ipv4Addr::new(192, 168, 0, 1))
        );
        assert_eq!(
            origin("http://192.168.0.1:1900/rootDesc.xml"),
            "http://192.168.0.1:1900"
        );
        assert_eq!(
            unchunk("5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"),
            "hello world"
        );
        assert_eq!(
            header("HTTP/1.1 200 OK\r\nLOCATION: http://x/y\r\n", "location"),
            Some("http://x/y")
        );
    }

    /// Against this network's router, when asked: PINGPONG_PORTMAP_TEST=1.
    #[test]
    fn maps_on_this_network() {
        if std::env::var("PINGPONG_PORTMAP_TEST").is_err() {
            return;
        }
        println!("gateway: {:?}", default_gateway());
        match map(47899, Duration::from_secs(120)) {
            Ok(m) => {
                println!("mapped: {m:?}");
                m.remove();
            }
            Err(e) => println!("no mapping: {e}"),
        }
    }
}
