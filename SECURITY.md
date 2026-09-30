# Security

pingpong gives another computer your screen, keyboard and mouse, so its
security matters as much as its latency. This page says how to report a
problem, and what pingpong does and does not protect against.

## Reporting a vulnerability

Please report vulnerabilities privately, through GitHub's **private
vulnerability reporting** (the repository's **Security** tab, "Report a
vulnerability"), not in a public issue. Include what an attacker needs
(network position, a paired device, local access), what they gain, and how
to reproduce it. You will get an answer, and credit in the fix if you want
it.

Only the latest code on the default branch is supported.

## The model

**Transport.** Every packet between Ping and Pong travels inside a
WireGuard tunnel (pq-boringtun) whose handshake combines X25519 with
ML-KEM-768 and authenticates both peers with static keys: a recording of the
traffic stays closed to a future quantum computer. A host accepts handshakes
only from paired clients' keys. Keys rotate every two minutes.

**Pairing.** Devices trust each other only after a PIN pairing on the local
network: SPAKE2 over a four-digit PIN, with an ML-KEM-768 exchange beside it,
then each side's tunnel keys sent under the result. An attacker must be in
the middle *while* the pairing happens, and guess the PIN at the first try,
to intervene; a recording of the pairing does not help later. The pairing
port (TCP 47801) serves the local network only.

**The host's web UI and API** (HTTPS on TCP 47802) use a self-signed
certificate the host makes for itself, and require the admin account the
first visitor creates (or, from Pong's own window, a token the host writes
for its own user). It is meant for the local network; do not forward the
port.

**Secrets at rest.** On macOS and Linux, each device's private keys, the
agent's API keys and the host's tokens are written readable only by their
owner (0600). On Windows, the host's web tokens are restricted to SYSTEM and
Administrators; other files rely on their folder's permissions (Ping's in
the user's `%APPDATA%`; the host's in `C:\ProgramData\Pong`, which does not
restrict reading by default). Whoever can read a device's keys can act as
that device: unpair a device you lost.

**The host's privileges.** On Windows, Pong runs as SYSTEM so it can
capture and control the secure desktop (UAC prompts, the lock screen), as
Sunshine and Apollo do. Clipboard files are read and written as the
signed-in user, not as SYSTEM.

**AI agents** are clients of their own, held to rules the host enforces:
never over a person, never on a secure screen, only with the access the
host granted. [docs/ai-agents.md](docs/ai-agents.md#threat-model) has their
threat model.

## Out of scope

- A compromised client or host: whoever controls either end controls the
  session.
- Denial of service on the local network, or by a paired device.
- Traffic analysis: an observer can see that two machines exchange a
  stream, and roughly at what rate.
