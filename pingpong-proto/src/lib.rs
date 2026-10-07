//! The pingpong wire protocol: datagram headers, frame packetization and
//! Reed-Solomon FEC, reassembly, frame pacing, the control, input, audio
//! and clipboard messages that ride beside the video, and the Annex-B framing
//! of the video itself.
//!
//! This crate performs no I/O and has no platform or GPU dependencies, so
//! everything in it is unit-tested, property-tested and fuzzed on any machine.
//!
//! Section references in this crate (`v1 design §5.1`, `v2 design §4.3`) are
//! to the original design documents in `docs/design/`.

pub mod annexb;
pub mod audio;
pub mod clip;
pub mod clock;
pub mod control;
pub mod fec;
pub mod gamepad;
pub mod header;
pub mod input;
pub mod mackeys;
pub mod pacer;
pub mod packetize;
pub mod permission;
pub mod reassemble;
pub mod repeat;
pub mod telemetry;
pub mod video;

/// Bytes of header on every datagram. Occupies the inner IPv4 header's
/// positions (v1 design §5).
pub const HEADER_LEN: usize = 20;

/// Media payload bytes per datagram at the default 1280-byte path MTU
/// (v1 design §6). Must be even: Reed-Solomon rejects odd shard sizes.
pub const PAYLOAD_LEN: usize = 1180;

/// Largest inner packet we build for any path.
pub const MAX_DATAGRAM: usize = HEADER_LEN + PAYLOAD_LEN;

/// Video shard bytes on the local network, where the path's MTU is 1500
/// (Moonlight uses 1392 there): the outer datagram is then exactly 1500
/// bytes over IPv6 (40 + 8 UDP + 32 WireGuard + 20 header + 1400), and a
/// frame needs a sixth fewer datagrams than at `PAYLOAD_LEN`. Marked per
/// datagram by the header's `lan_shards` flag; only a client that says it
/// understands it (`control::flags::LAN_SHARDS`) is sent them.
pub const LAN_PAYLOAD_LEN: usize = 1400;

const _: () = assert!(
    LAN_PAYLOAD_LEN.is_multiple_of(2),
    "Reed-Solomon shard sizes must be even"
);

const _: () = assert!(
    PAYLOAD_LEN.is_multiple_of(2),
    "Reed-Solomon shard sizes must be even"
);
