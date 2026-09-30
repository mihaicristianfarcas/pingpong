//! Clipboard sharing: what one side of a session copied, sent to the other.
//!
//! Control packets (`kind = 3`) with opcodes of their own, which peers that
//! predate them ignore (`Control::decode` refuses them). A copy is one
//! transfer: its bytes (encoded [`Item`]s) cut into chunks that the receiver
//! acknowledges selectively, and that the sender resends until every one is
//! in. Control messages have no FEC or retransmit of their own, and a
//! clipboard must arrive whole or not at all. A newer copy cancels the
//! transfer in flight.
//!
//! ```text
//! Data    op u8 | id u32 | total u32 | offset u32 | bytes          sender -> receiver
//! Ack     op u8 | id u32 | upto u32  | bits…                        receiver -> sender
//! Cancel  op u8 | id u32                                            either way
//! ```
//!
//! `upto` counts the chunks received in order from the first; bit `i` of
//! `bits` (LSB first) says chunk `upto + 1 + i` is in too.

use std::time::{Duration, Instant};

use crate::header::{Header, Kind};
use crate::HEADER_LEN;

const OP_DATA: u8 = 40;
const OP_ACK: u8 = 41;
const OP_CANCEL: u8 = 42;

/// Payload bytes per data packet: a datagram of at most `MAX_DATAGRAM`.
pub const CHUNK: usize = 1152;
/// The largest copy shared (files included).
pub const MAX_TRANSFER: usize = 256 << 20;
/// Chunks a sender keeps in flight at most, and so the most an ack covers.
const MAX_WINDOW: usize = 1024;
const ACK_BITS_BYTES: usize = MAX_WINDOW / 8;
const _: () = assert!(HEADER_LEN + 13 + CHUNK <= crate::MAX_DATAGRAM);
const _: () = assert!(HEADER_LEN + 9 + ACK_BITS_BYTES <= crate::MAX_DATAGRAM);

/// A transfer that moves nothing for this long is given up.
pub const STALL: Duration = Duration::from_secs(20);

/// Whether a control body is clipboard sharing's.
pub fn is_clip(body: &[u8]) -> bool {
    matches!(body.first(), Some(&(OP_DATA | OP_ACK | OP_CANCEL)))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Msg<'a> {
    Data {
        id: u32,
        total: u32,
        offset: u32,
        bytes: &'a [u8],
    },
    Ack {
        id: u32,
        upto: u32,
        bits: &'a [u8],
    },
    Cancel {
        id: u32,
    },
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Hostile input: anything malformed is `None`.
pub fn decode(body: &[u8]) -> Option<Msg<'_>> {
    let id = u32_at(body, 1)?;
    match body[0] {
        OP_DATA => {
            let (total, offset) = (u32_at(body, 5)?, u32_at(body, 9)?);
            let bytes = &body[13..];
            // Only an empty copy has an empty chunk.
            if (bytes.is_empty() && total != 0)
                || bytes.len() > CHUNK
                || total as usize > MAX_TRANSFER
                || !(offset as usize).is_multiple_of(CHUNK)
            {
                return None;
            }
            if offset as usize + bytes.len() > total as usize {
                return None;
            }
            Some(Msg::Data {
                id,
                total,
                offset,
                bytes,
            })
        }
        OP_ACK => {
            let bits = &body[9..];
            (bits.len() <= ACK_BITS_BYTES).then_some(Msg::Ack {
                id,
                upto: u32_at(body, 5)?,
                bits,
            })
        }
        OP_CANCEL => Some(Msg::Cancel { id }),
        _ => None,
    }
}

/// A whole control packet (header and body) around `body`.
fn packet(body: &[u8]) -> Vec<u8> {
    let len = HEADER_LEN + body.len();
    let header = Header {
        keyframe: false,
        recovery: false,
        lan_shards: false,
        frame_end: false,
        kind: Kind::Control,
        total_len: len as u16,
        fragment_idx: 0,
        data_shards: 0,
        parity_shards: 0,
        fec_block_idx: 0,
        frame_len: 0,
        capture_ts_us: crate::clock::now_us(),
        frame_id: 0,
    };
    let mut out = vec![0u8; len];
    let mut h = [0u8; HEADER_LEN];
    header
        .encode(&mut h)
        .expect("control header is always valid");
    out[..HEADER_LEN].copy_from_slice(&h);
    out[HEADER_LEN..].copy_from_slice(body);
    out
}

pub fn cancel_packet(id: u32) -> Vec<u8> {
    let mut b = vec![OP_CANCEL];
    b.extend_from_slice(&id.to_le_bytes());
    packet(&b)
}

/// "Every chunk is in": the answer to a copy's late retransmits.
pub fn full_ack_packet(id: u32, chunks: u32) -> Vec<u8> {
    let mut b = vec![OP_ACK];
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(&chunks.to_le_bytes());
    packet(&b)
}

fn data_packet(id: u32, total: u32, offset: u32, bytes: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(13 + bytes.len());
    b.push(OP_DATA);
    for v in [id, total, offset] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(bytes);
    packet(&b)
}

fn chunks_of(total: usize) -> usize {
    total.div_ceil(CHUNK).max(1)
}

/// Sending one copy: chunks go out within a window that grows while acks
/// come back and halves when chunks are lost, under a rate cap (the stream's
/// video goes first); a chunk not acknowledged in time goes again.
pub struct Outgoing {
    pub id: u32,
    data: Vec<u8>,
    acked: Vec<bool>,
    /// Every chunk below this is acknowledged.
    contiguous: usize,
    /// When each chunk was last sent, and whether it was sent more than once
    /// (its ack then says nothing about the round trip).
    sent: Vec<Option<(Instant, bool)>>,
    /// The first chunk never sent.
    next: usize,
    in_flight: usize,
    cwnd: f64,
    ssthresh: f64,
    srtt: Option<Duration>,
    /// No second window cut before this (one per round trip).
    cut_until: Instant,
    rate: f64,
    tokens: f64,
    refilled: Instant,
    progressed: Instant,
}

impl Outgoing {
    /// `rate`: bytes per second at most.
    pub fn new(id: u32, data: Vec<u8>, rate: u64, now: Instant) -> Outgoing {
        let n = chunks_of(data.len());
        Outgoing {
            id,
            data,
            acked: vec![false; n],
            contiguous: 0,
            sent: vec![None; n],
            next: 0,
            in_flight: 0,
            cwnd: 32.0,
            ssthresh: MAX_WINDOW as f64,
            srtt: None,
            cut_until: now,
            rate: rate.max(64 * 1024) as f64,
            tokens: 64.0 * 1024.0,
            refilled: now,
            progressed: now,
        }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn done(&self) -> bool {
        self.contiguous >= self.acked.len()
    }

    /// Nothing acknowledged for too long: the other side is gone or refuses.
    pub fn stalled(&self, now: Instant) -> bool {
        now.duration_since(self.progressed) > STALL
    }

    /// Bytes acknowledged so far.
    pub fn acked_bytes(&self) -> usize {
        (self.acked.iter().filter(|&&a| a).count() * CHUNK).min(self.data.len())
    }

    fn rto(&self) -> Duration {
        self.srtt.map_or(Duration::from_millis(300), |s| {
            (s * 3).clamp(Duration::from_millis(60), Duration::from_secs(2))
        })
    }

    fn chunk(&self, i: usize) -> &[u8] {
        let start = i * CHUNK;
        &self.data[start..(start + CHUNK).min(self.data.len())]
    }

    /// The packets to send now.
    pub fn poll(&mut self, now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let dt = now.duration_since(self.refilled).as_secs_f64();
        self.refilled = now;
        self.tokens = (self.tokens + dt * self.rate).min((self.rate / 20.0).max(64.0 * 1024.0));
        let total = self.data.len() as u32;
        // Chunks whose ack is overdue go first.
        let rto = self.rto();
        let mut lost = false;
        for i in self.contiguous..self.next {
            if self.tokens < CHUNK as f64 {
                break;
            }
            match self.sent[i] {
                Some((at, _)) if !self.acked[i] && now.duration_since(at) >= rto => {
                    lost = true;
                    self.sent[i] = Some((now, true));
                    self.tokens -= CHUNK as f64;
                    out.push(data_packet(
                        self.id,
                        total,
                        (i * CHUNK) as u32,
                        self.chunk(i),
                    ));
                }
                _ => {}
            }
        }
        if lost && now >= self.cut_until {
            self.ssthresh = (self.cwnd / 2.0).max(8.0);
            self.cwnd = self.ssthresh;
            self.cut_until = now + self.rto();
        }
        while self.next < self.acked.len()
            && self.in_flight < self.cwnd as usize
            && self.next - self.contiguous < MAX_WINDOW
        {
            if self.tokens < CHUNK as f64 {
                break;
            }
            let i = self.next;
            self.next += 1;
            self.in_flight += 1;
            self.sent[i] = Some((now, false));
            self.tokens -= CHUNK as f64;
            out.push(data_packet(
                self.id,
                total,
                (i * CHUNK) as u32,
                self.chunk(i),
            ));
        }
        out
    }

    pub fn on_ack(&mut self, upto: u32, bits: &[u8], now: Instant) {
        let upto = (upto as usize).min(self.acked.len());
        let mut newly = 0usize;
        let mut sample = None;
        let mut mark = |i: usize, this: &mut Outgoing| {
            if i < this.acked.len() && i < this.next && !this.acked[i] {
                this.acked[i] = true;
                this.in_flight = this.in_flight.saturating_sub(1);
                newly += 1;
                if let Some((at, false)) = this.sent[i] {
                    sample = Some(now.duration_since(at));
                }
            }
        };
        for i in self.contiguous..upto {
            mark(i, self);
        }
        for (byte, b) in bits.iter().enumerate() {
            for bit in 0..8 {
                if b & (1 << bit) != 0 {
                    mark(upto + 1 + byte * 8 + bit, self);
                }
            }
        }
        while self.contiguous < self.acked.len() && self.acked[self.contiguous] {
            self.contiguous += 1;
        }
        if newly > 0 {
            self.progressed = now;
            if let Some(s) = sample {
                self.srtt = Some(self.srtt.map_or(s, |r| (r * 7 + s) / 8));
            }
            let n = newly as f64;
            self.cwnd = if self.cwnd < self.ssthresh {
                self.cwnd + n
            } else {
                self.cwnd + n / self.cwnd
            };
            self.cwnd = self.cwnd.min(MAX_WINDOW as f64);
        }
    }
}

/// Receiving one copy.
pub struct Incoming {
    pub id: u32,
    data: Vec<u8>,
    have: Vec<bool>,
    contiguous: usize,
    received: usize,
    /// Something arrived since the last ack.
    pub dirty: bool,
    pub last: Instant,
}

impl Incoming {
    /// None when it is too large to take.
    pub fn new(id: u32, total: u32, now: Instant) -> Option<Incoming> {
        let total = total as usize;
        if total > MAX_TRANSFER {
            return None;
        }
        let n = chunks_of(total);
        Some(Incoming {
            id,
            data: vec![0; total],
            have: vec![false; n],
            contiguous: 0,
            received: 0,
            dirty: false,
            last: now,
        })
    }

    pub fn total(&self) -> usize {
        self.data.len()
    }

    pub fn received_bytes(&self) -> usize {
        (self.received * CHUNK).min(self.data.len())
    }

    /// Take a chunk; false if it does not belong (another size, a bad offset).
    pub fn on_data(&mut self, total: u32, offset: u32, bytes: &[u8], now: Instant) -> bool {
        if total as usize != self.data.len() {
            return false;
        }
        let i = offset as usize / CHUNK;
        let start = i * CHUNK;
        let len = (self.data.len() - start).min(CHUNK);
        if start >= self.data.len().max(1) || bytes.len() != len {
            return false;
        }
        self.dirty = true;
        self.last = now;
        if !self.have[i] {
            self.have[i] = true;
            self.received += 1;
            self.data[start..start + len].copy_from_slice(bytes);
            while self.contiguous < self.have.len() && self.have[self.contiguous] {
                self.contiguous += 1;
            }
        }
        true
    }

    pub fn complete(&self) -> bool {
        self.contiguous >= self.have.len()
    }

    pub fn ack_packet(&mut self) -> Vec<u8> {
        self.dirty = false;
        let mut b = vec![OP_ACK];
        b.extend_from_slice(&self.id.to_le_bytes());
        b.extend_from_slice(&(self.contiguous as u32).to_le_bytes());
        let mut bits = [0u8; ACK_BITS_BYTES];
        let mut last = 0;
        for (k, i) in (self.contiguous + 1..self.have.len())
            .take(MAX_WINDOW)
            .enumerate()
        {
            if self.have[i] {
                bits[k / 8] |= 1 << (k % 8);
                last = k / 8 + 1;
            }
        }
        b.extend_from_slice(&bits[..last]);
        packet(&b)
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

/// What was copied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Text(String),
    /// An image, as PNG.
    Png(Vec<u8>),
    /// A file, at a relative path ('/' between its parts) under the copy.
    File {
        path: String,
        data: Vec<u8>,
    },
    /// A folder (so an empty one comes across too).
    Dir {
        path: String,
    },
}

const VERSION: u8 = 1;

pub fn encode_items(items: &[Item]) -> Vec<u8> {
    let mut out = vec![VERSION];
    for item in items {
        let (kind, path, data): (u8, &str, &[u8]) = match item {
            Item::Text(t) => (1, "", t.as_bytes()),
            Item::Png(p) => (2, "", p),
            Item::File { path, data } => (3, path, data),
            Item::Dir { path } => (4, path, &[]),
        };
        out.push(kind);
        out.extend_from_slice(&(path.len() as u16).to_le_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
    }
    out
}

/// A relative path that stays under the folder it lands in, with names any
/// system takes; None if it would not.
pub fn safe_path(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for part in path.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains(['\\', '\0'])
            || part.len() > 255
        {
            return None;
        }
        // Windows refuses these in names (':' also names a drive or a
        // stream), and trailing dots and spaces.
        let clean: String = part
            .chars()
            .map(|c| {
                if matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 32 {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let clean = clean.trim_end_matches(['.', ' ']);
        if clean.is_empty() {
            return None;
        }
        parts.push(clean.to_string());
    }
    if parts.is_empty() || parts.len() > 64 {
        return None;
    }
    Some(parts.join("/"))
}

/// Hostile input: anything malformed (or a path that would leave its
/// folder) is `None`.
pub fn decode_items(bytes: &[u8]) -> Option<Vec<Item>> {
    if bytes.first() != Some(&VERSION) {
        return None;
    }
    let mut at = 1;
    let mut items = Vec::new();
    while at < bytes.len() {
        let kind = bytes[at];
        let plen = u16::from_le_bytes(bytes.get(at + 1..at + 3)?.try_into().ok()?) as usize;
        let path = std::str::from_utf8(bytes.get(at + 3..at + 3 + plen)?).ok()?;
        at += 3 + plen;
        let dlen = u32_at(bytes, at)? as usize;
        let data = bytes.get(at + 4..at + 4 + dlen)?;
        at += 4 + dlen;
        items.push(match kind {
            1 => Item::Text(String::from_utf8(data.to_vec()).ok()?),
            2 => Item::Png(data.to_vec()),
            3 => Item::File {
                path: safe_path(path)?,
                data: data.to_vec(),
            },
            4 => Item::Dir {
                path: safe_path(path)?,
            },
            _ => return None,
        });
    }
    Some(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(p: &[u8]) -> &[u8] {
        &p[HEADER_LEN..]
    }

    /// Move `data` from an `Outgoing` to an `Incoming` over a link that
    /// drops one packet in `drop_every` at random (both ways); returns the
    /// rounds.
    fn transfer(data: Vec<u8>, drop_every: u32) -> (Vec<u8>, usize) {
        let mut now = Instant::now();
        let mut out = Outgoing::new(7, data.clone(), 50_000_000, now);
        let mut inc: Option<Incoming> = None;
        let mut rng = 0x9E37_79B9u32;
        let mut lose = move || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            drop_every > 0 && rng.is_multiple_of(drop_every)
        };
        let mut rounds = 0;
        while !out.done() {
            rounds += 1;
            assert!(rounds < 100_000, "stuck");
            now += Duration::from_millis(5);
            for p in out.poll(now) {
                if lose() {
                    continue;
                }
                assert!(is_clip(body(&p)));
                match decode(body(&p)).unwrap() {
                    Msg::Data {
                        id,
                        total,
                        offset,
                        bytes,
                    } => {
                        let i = inc.get_or_insert_with(|| Incoming::new(id, total, now).unwrap());
                        assert!(i.on_data(total, offset, bytes, now));
                    }
                    other => panic!("{other:?}"),
                }
            }
            if let Some(i) = inc.as_mut().filter(|i| i.dirty) {
                let ack = i.ack_packet();
                if lose() {
                    continue;
                }
                match decode(body(&ack)).unwrap() {
                    Msg::Ack { id, upto, bits } => {
                        assert_eq!(id, 7);
                        out.on_ack(upto, bits, now);
                    }
                    other => panic!("{other:?}"),
                }
            }
        }
        let inc = inc.unwrap();
        assert!(inc.complete());
        (inc.into_bytes(), rounds)
    }

    #[test]
    fn a_copy_arrives_whole() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let (got, _) = transfer(data.clone(), 0);
        assert_eq!(got, data);
    }

    #[test]
    fn a_copy_arrives_whole_over_a_lossy_link() {
        let data: Vec<u8> = (0..500_000u32).map(|i| (i * 13 % 253) as u8).collect();
        for drop_every in [3, 7, 50] {
            let (got, _) = transfer(data.clone(), drop_every);
            assert_eq!(got, data, "dropping every {drop_every}th");
        }
    }

    #[test]
    fn small_and_empty_copies() {
        for len in [0usize, 1, CHUNK - 1, CHUNK, CHUNK + 1] {
            let data = vec![9u8; len];
            let (got, _) = transfer(data.clone(), 0);
            assert_eq!(got, data, "{len} bytes");
        }
    }

    #[test]
    fn the_rate_cap_holds() {
        // 1 MB at 1 MB/s takes about a second of 5 ms rounds.
        let data = vec![1u8; 1 << 20];
        let mut now = Instant::now();
        let mut out = Outgoing::new(1, data, 1 << 20, now);
        let mut sent = 0usize;
        for _ in 0..100 {
            now += Duration::from_millis(5);
            sent += out.poll(now).len() * CHUNK;
            // Everything acked at once: only the cap limits.
            out.on_ack(out.next as u32, &[], now);
        }
        // Half a second at 1 MB/s, plus the first burst.
        assert!(sent < (1 << 20) / 2 + 128 * 1024, "{sent}");
        assert!(sent > (1 << 20) / 3, "{sent}");
    }

    #[test]
    fn hostile_packets_are_refused() {
        assert_eq!(decode(&[OP_DATA]), None);
        assert_eq!(
            decode(&[OP_DATA, 1, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0]),
            None,
            "no bytes"
        );
        let mut too_far = vec![OP_DATA, 1, 0, 0, 0, 10, 0, 0, 0, 0, 0, 0, 0];
        too_far.extend_from_slice(&[0; 11]);
        assert_eq!(decode(&too_far), None, "past its total");
        let mut odd = vec![OP_DATA, 1, 0, 0, 0, 0, 0, 1, 0, 5, 0, 0, 0];
        odd.push(1);
        assert_eq!(decode(&odd), None, "an offset between chunks");
        let huge = [OP_DATA, 1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 1];
        assert_eq!(decode(&huge), None, "too large");
        assert!(Incoming::new(1, (MAX_TRANSFER + 1) as u32, Instant::now()).is_none());
        // A chunk of the wrong size for its place.
        let mut i = Incoming::new(1, (CHUNK * 2) as u32, Instant::now()).unwrap();
        assert!(!i.on_data((CHUNK * 2) as u32, 0, &[1; 10], Instant::now()));
        assert!(!i.on_data(5, 0, &[1; 5], Instant::now()));
        assert_eq!(
            decode(&[OP_CANCEL, 3, 0, 0, 0]),
            Some(Msg::Cancel { id: 3 })
        );
        assert!(!is_clip(&[2]) && !is_clip(&[]));
    }

    #[test]
    fn items_round_trip() {
        let items = vec![
            Item::Text("héllo".into()),
            Item::Png(vec![1, 2, 3]),
            Item::Dir {
                path: "folder".into(),
            },
            Item::File {
                path: "folder/a.txt".into(),
                data: b"abc".to_vec(),
            },
            Item::File {
                path: "empty".into(),
                data: vec![],
            },
        ];
        assert_eq!(decode_items(&encode_items(&items)), Some(items));
        assert_eq!(decode_items(&[]), None);
        assert_eq!(decode_items(&[9]), None);
    }

    #[test]
    fn paths_stay_in_their_folder() {
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a//b",
            "C:\\x",
            "a\\..\\b",
            ".",
            "a/./b",
        ] {
            assert_eq!(safe_path(bad), None, "{bad}");
            let items = vec![Item::File {
                path: bad.into(),
                data: vec![],
            }];
            assert_eq!(decode_items(&encode_items(&items)), None, "{bad}");
        }
        assert_eq!(safe_path("a/b c/d.txt").as_deref(), Some("a/b c/d.txt"));
        assert_eq!(safe_path("what?.txt").as_deref(), Some("what_.txt"));
        assert_eq!(safe_path("C:x").as_deref(), Some("C_x"));
        assert_eq!(safe_path("dots...").as_deref(), Some("dots"));
    }
}
