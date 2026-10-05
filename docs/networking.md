# Networking: reaching the host from anywhere

On the local network Ping finds Pong by mDNS and connects directly. Away from
home, pingpong connects the way Tailscale connects machines — addresses
exchanged through a rendezvous, NATs punched from both sides — but without
servers: the rendezvous is the public BitTorrent Mainline DHT, and nothing
needs to be run, forwarded or paid for.

## Ports

| Port | Protocol | What | Reachable from |
|---|---|---|---|
| 47800 | UDP | The tunnel: every stream, all paired clients | wherever clients are |
| 47801 | TCP | Pairing (PIN) and host information | the local network |
| 47802 | TCP (HTTPS) | Pong's web UI and API | the local network |

All three are settings on the host (**Settings > Network**, or `port`,
`pairing_port` and `web_port` in its `config.toml`). Pairing is deliberately
local: a device is paired once, at home, and then connects from anywhere.
The client's own tunnel port is chosen once and kept (`port` in its data
folder), so its NAT keeps giving it the same public port.

## What Moonlight + Sunshine do, and why it is not enough

Sunshine asks the router for a port mapping over UPnP (when its `upnp`
setting is on; it is off by default), and Moonlight connects
to the host's public address. That needs a router that speaks UPnP and has a
public address. Double NAT (a router behind the ISP's box) and carrier-grade
NAT are common, so a mapping is an optimisation, not the path. pingpong asks
for one too, and also punches.

## How it works

1. **Candidates.** Each side collects the addresses it might be reached at:
   its LAN addresses, the public address its NAT gives the tunnel's socket
   (STUN, on that same socket), and a port mapping if the router grants one.
2. **Rendezvous.** Each device has an Ed25519 key, and the host a 32-byte
   rendezvous secret; both are exchanged during PIN pairing, so only paired
   devices can read each other's records. Records are BEP 44 mutable items
   on the Mainline DHT, signed by the device and sealed with
   ChaCha20-Poly1305 under the host's secret. The DHT sees ciphertext under
   a key it cannot link to anyone.
   - The host publishes its candidates when they change, and every 30
     minutes (items expire after about two hours).
   - A client that wants to connect publishes an *intent*: its candidates,
     a timestamp and a nonce.
3. **Punching.** The client races handshake initiations to every host
   candidate (local ones first, remote ones 300 ms later). The host checks
   its paired clients' records every few seconds; on a fresh intent it sends
   initiations to the client's candidates, which opens its own NAT towards
   the client. Whichever handshake lands first wins, and WireGuard roaming
   keeps the path.
4. **Keeping it open.** The tunnel's keepalive holds the NAT mappings during
   a session; the host re-checks its public address by STUN every 25 s.

### The warm path

A cold connect through the DHT takes seconds (finding records costs a few
round trips, and the host notices an intent on its next check). So while
Ping is open it keeps each host's internet address cached, and publishes a
*presence* record per host; the host then sends a small packet every 20 s
towards each present client, keeping its own NAT open towards it. A client's
handshake then walks straight in, and the intent path remains the fallback.

### Port mapping

With internet access on, Pong also asks the router to forward the tunnel's
port: UPnP IGD first (an SSDP search, the WAN IP or PPP connection service,
`AddPortMapping` for UDP with a one-hour lease renewed at half), else NAT-PMP
(RFC 6886) to the default gateway. The mapping's public address joins the
published candidates, and is released when Pong stops or the setting is
turned off. A router whose own "public" address is private (behind another
NAT) is left alone: a mapping there would open nothing. Pong's window and
web UI show what happened under **Router**; **Settings > Network > Ask the
router to forward the port** turns it off.

## Limits

- **Symmetric NAT on both sides** (a new public port for every destination)
  defeats punching. Tailscale falls back to relays; pingpong has none.
  Forward UDP 47800 to the host on its router for a path that always works.
- **Cold connects take seconds** (measured 9.6–11.5 s across two NATs); warm
  ones do not. On the local network nothing of this is used.
- A router that does not loop traffic for its own public address back inside
  ("hairpinning") means a client on the same network as the host cannot
  test the internet path; `ping stream NAME --wan-only` needs the client on
  another network.

## Measured

- Joining the DHT: 3–5 s (Ping joins at launch, so a click does not wait).
  Publishing a record: ~4.7 s; resolving one: ~2.5 s.
- A laptop on a phone hotspot to a host at home, `--wan-only` (neither the
  LAN nor a VPN available): the tunnel came up directly between the two
  NATs in 9.6 and 11.5 s; 1080p60 at 10 Mbit/s streamed at 58–60 fps with no
  loss, a 40 ms round trip and 59 ms from the host's capture to the client's
  screen.
- Away from home, with the warm path: the first connect of the day took
  0.35 s (11.8 s before the warm path and handshake racing). The client
  re-races a fresh handshake along every known path each second; WireGuard
  alone retries after 5 s, to one address.

## Where it lives

| Piece | Code |
|---|---|
| STUN codec and NAT probe | `pingpong-nat/src/stun.rs`, `examples/stun.rs` |
| Rendezvous records (seal, sign, publish, resolve) | `pingpong-nat/src/rendezvous.rs`, `examples/dht.rs` |
| UPnP / NAT-PMP | `pingpong-nat/src/portmap.rs` |
| Keys exchanged in pairing | `pingpong-pairing/src/pair.rs` (`Extras`) |
| Host: publish, watch intents, punch, map the port | `pong/src/presence.rs` |
| Client: resolve, publish intent, race, presence | `ping-core/src/wan.rs`, `ping-core/src/stream/net.rs` |
