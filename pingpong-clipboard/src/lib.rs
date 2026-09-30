//! Sharing the clipboard between the two ends of a session: what is copied
//! on one (text, an image, files and folders) can be pasted on the other.
//!
//! Each end runs a [`ClipSync`] for as long as the session does. It looks at
//! its own clipboard a few times a second -- the system's change count on a
//! Mac and on Windows, what is on it elsewhere -- and sends each new copy to
//! the other end over the session's tunnel (`pingpong_proto::clip`: chunks,
//! acknowledged and resent, under a rate cap so the video goes first). What
//! arrives goes on the clipboard here: text and images as they are, files
//! into a folder of this end's, put on the clipboard as files (a paste in
//! Finder or Explorer copies them from there).
//!
//! Left alone: copies a password manager marks as concealed, and copies
//! larger than `clip::MAX_TRANSFER` (files beyond it are not sent; the text
//! or image of the copy still is).

mod board;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use pingpong_proto::clip::{self, Incoming, Item, Msg, Outgoing};

pub use board::Board;

/// How often the clipboard is looked at.
const LOOK_EVERY: Duration = Duration::from_millis(400);
/// The worker's pace while a transfer runs.
const TICK: Duration = Duration::from_millis(5);
/// Looks at a changed clipboard before what is on it counts as unshareable.
const READ_TRIES: u32 = 10;
/// Copies done lately, whose late chunks are answered "all in".
const DONE_KEPT: usize = 8;

pub struct Options {
    /// Where files copied on the other end land: a folder of this end's,
    /// emptied as the sharing starts.
    pub files_dir: PathBuf,
    /// Bytes per second at most.
    pub rate: u64,
    /// Send what is on the clipboard now (text or an image, not files) as
    /// the sharing starts: a client does, so what was copied before the
    /// stream can be pasted in it.
    pub offer_current: bool,
    /// The other end, for the log.
    pub peer: String,
}

/// What a stream's clipboard sharing moves at most per second: half the
/// video's bitrate, within bounds.
pub fn rate_for(bitrate_kbps: u32) -> u64 {
    (bitrate_kbps as u64 * 1000 / 8 / 2).clamp(2 << 20, 25 << 20)
}

/// Clipboard sharing for one session, on a thread of its own.
pub struct ClipSync {
    inbox: Sender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ClipSync {
    /// Start sharing; `send` puts a packet (header and body) on the tunnel.
    pub fn start(opts: Options, send: impl Fn(&[u8]) + Send + 'static) -> ClipSync {
        let (inbox, rx) = crossbeam_channel::bounded(8192);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("clipboard".into())
                .spawn(move || Worker::new(opts, Box::new(send)).run(rx, &stop))
                .map_err(|e| tracing::warn!(error = %e, "cannot start clipboard sharing"))
                .ok()
        };
        ClipSync {
            inbox,
            stop,
            thread,
        }
    }

    /// A clipboard packet from the other end (a control body for which
    /// `clip::is_clip`).
    pub fn deliver(&self, body: &[u8]) {
        let _ = self.inbox.try_send(body.to_vec());
    }
}

impl Drop for ClipSync {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Sends one message to the other end, over the session's tunnel.
type SendFn = Box<dyn Fn(&[u8]) + Send>;

struct Worker {
    opts: Options,
    send: SendFn,
    board: Board,
    outgoing: Option<Outgoing>,
    incoming: Option<Incoming>,
    /// Copies received lately: (id, chunks), to answer their retransmits.
    done: std::collections::VecDeque<(u32, u32)>,
    /// The clipboard's stamp when last looked at, and after this end last
    /// wrote to it (not sent back).
    seen: Option<u64>,
    next_id: u32,
    started: Option<Instant>,
    /// Looks at a changed clipboard that found nothing readable yet.
    misses: u32,
    /// A digest of the last copy sent or received: the same copy again is
    /// not sent (both ends on one clipboard would bounce it forever).
    last: Option<u64>,
}

impl Worker {
    fn new(opts: Options, send: SendFn) -> Worker {
        let _ = std::fs::remove_dir_all(&opts.files_dir);
        let next_id = rand_core::RngCore::next_u32(&mut rand_core::OsRng);
        Worker {
            board: Board::open(),
            opts,
            send,
            outgoing: None,
            incoming: None,
            done: Default::default(),
            seen: None,
            next_id,
            started: None,
            misses: 0,
            last: None,
        }
    }

    fn run(mut self, rx: Receiver<Vec<u8>>, stop: &AtomicBool) {
        tracing::info!(peer = self.opts.peer, "sharing the clipboard");
        self.seen = self.board.stamp();
        if self.opts.offer_current && !self.board.concealed() {
            if let Some(items) = self.board.read(false) {
                self.offer(items);
            }
        }
        let mut last_look = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let busy = self.outgoing.is_some() || self.incoming.is_some();
            let wait = if busy {
                TICK
            } else {
                LOOK_EVERY.saturating_sub(last_look.elapsed()).max(TICK)
            };
            match rx.recv_timeout(wait) {
                Ok(body) => {
                    self.on_packet(&body);
                    while let Ok(body) = rx.try_recv() {
                        self.on_packet(&body);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
            let now = Instant::now();
            self.receive_tick(now);
            self.send_tick(now);
            if last_look.elapsed() >= LOOK_EVERY {
                last_look = now;
                self.look();
            }
        }
        if let Some(o) = self.outgoing.take() {
            (self.send)(&clip::cancel_packet(o.id));
        }
        tracing::info!(peer = self.opts.peer, "clipboard sharing over");
    }

    /// Something new on this end's clipboard: send it.
    fn look(&mut self) {
        let stamp = self.board.stamp();
        if stamp.is_none() || stamp == self.seen {
            return;
        }
        if self.board.concealed() {
            self.seen = stamp;
            tracing::info!("a concealed copy (a password manager's): not shared");
            return;
        }
        match self.board.read(true) {
            Some(items) => {
                self.seen = stamp;
                self.misses = 0;
                self.offer(items);
            }
            // The app copying may still hold the clipboard (Windows opens
            // it to one program at a time): look again, a few times.
            None => {
                self.misses += 1;
                if self.misses >= READ_TRIES {
                    tracing::info!(
                        why = self.board.why_not(),
                        "the clipboard changed, but holds nothing to share"
                    );
                    self.seen = stamp;
                    self.misses = 0;
                }
            }
        }
    }

    fn offer(&mut self, items: Vec<Item>) {
        let bytes = clip::encode_items(&items);
        let digest = digest(&bytes);
        if self.last == Some(digest) {
            return;
        }
        self.last = Some(digest);
        if bytes.len() > clip::MAX_TRANSFER {
            tracing::info!(bytes = bytes.len(), "a copy too large to share");
            return;
        }
        if let Some(old) = self.outgoing.take() {
            (self.send)(&clip::cancel_packet(old.id));
        }
        self.next_id = self.next_id.wrapping_add(1);
        tracing::info!(
            peer = self.opts.peer,
            what = describe(&items),
            bytes = bytes.len(),
            "sending a copy"
        );
        self.started = Some(Instant::now());
        self.outgoing = Some(Outgoing::new(
            self.next_id,
            bytes,
            self.opts.rate,
            Instant::now(),
        ));
    }

    fn on_packet(&mut self, body: &[u8]) {
        let now = Instant::now();
        match clip::decode(body) {
            Some(Msg::Data {
                id,
                total,
                offset,
                bytes,
            }) => {
                if let Some(&(_, chunks)) = self.done.iter().find(|(d, _)| *d == id) {
                    // Done here, but the sender missed the last ack.
                    (self.send)(&clip::full_ack_packet(id, chunks));
                    return;
                }
                if self.incoming.as_ref().is_none_or(|i| i.id != id) {
                    // A new copy supersedes one still arriving.
                    match Incoming::new(id, total, now) {
                        Some(i) => self.incoming = Some(i),
                        None => {
                            (self.send)(&clip::cancel_packet(id));
                            return;
                        }
                    }
                }
                if let Some(i) = self.incoming.as_mut() {
                    i.on_data(total, offset, bytes, now);
                }
            }
            Some(Msg::Ack { id, upto, bits }) => {
                if let Some(o) = self.outgoing.as_mut().filter(|o| o.id == id) {
                    o.on_ack(upto, bits, now);
                }
            }
            Some(Msg::Cancel { id }) => {
                if self.incoming.as_ref().is_some_and(|i| i.id == id) {
                    self.incoming = None;
                }
                if self.outgoing.as_ref().is_some_and(|o| o.id == id) {
                    tracing::info!(peer = self.opts.peer, "the other end refused the copy");
                    self.outgoing = None;
                }
            }
            None => {}
        }
    }

    fn receive_tick(&mut self, now: Instant) {
        let Some(i) = self.incoming.as_mut() else {
            return;
        };
        if i.dirty {
            (self.send)(&i.ack_packet());
        }
        if i.complete() {
            let i = self.incoming.take().expect("there");
            let (id, chunks) = (i.id, i.total().div_ceil(clip::CHUNK).max(1) as u32);
            self.done.push_back((id, chunks));
            if self.done.len() > DONE_KEPT {
                self.done.pop_front();
            }
            let bytes = i.into_bytes();
            self.last = Some(digest(&bytes));
            match clip::decode_items(&bytes) {
                Some(items) => {
                    tracing::info!(
                        peer = self.opts.peer,
                        what = describe(&items),
                        "a copy arrived"
                    );
                    if let Err(e) = self
                        .board
                        .write(items, &self.opts.files_dir.join(id.to_string()))
                    {
                        tracing::warn!(error = e, "could not put the copy on the clipboard");
                    }
                    // What this end wrote is not sent back.
                    self.seen = self.board.stamp();
                }
                None => tracing::warn!("a copy arrived that could not be read"),
            }
        } else if now.duration_since(i.last) > clip::STALL {
            tracing::info!(peer = self.opts.peer, "a copy stopped arriving");
            self.incoming = None;
        }
    }

    fn send_tick(&mut self, now: Instant) {
        let Some(o) = self.outgoing.as_mut() else {
            return;
        };
        for p in o.poll(now) {
            (self.send)(&p);
        }
        if o.done() {
            tracing::info!(
                bytes = o.len(),
                ms = self.started.map_or(0, |s| s.elapsed().as_millis() as u64),
                "the copy arrived on the other end"
            );
            self.outgoing = None;
        } else if o.stalled(now) {
            tracing::info!(
                acked = o.acked_bytes(),
                bytes = o.len(),
                "the other end stopped taking the copy"
            );
            self.outgoing = None;
        }
    }
}

fn digest(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// What a copy holds, for the log (never its contents).
fn describe(items: &[Item]) -> String {
    let files = items
        .iter()
        .filter(|i| matches!(i, Item::File { .. }))
        .count();
    let dirs = items
        .iter()
        .filter(|i| matches!(i, Item::Dir { .. }))
        .count();
    let mut out = Vec::new();
    for i in items {
        match i {
            Item::Text(t) => out.push(format!("text ({} characters)", t.chars().count())),
            Item::Png(p) => out.push(format!("an image ({} KB)", p.len() / 1024)),
            _ => {}
        }
    }
    if files + dirs > 0 {
        out.push(format!("{files} files, {dirs} folders"));
    }
    out.join(", ")
}

/// Files and folders under `paths`, as items at paths relative to where
/// each top-level one sits; None if they total more than `max` bytes.
pub fn gather(paths: &[PathBuf], max: usize) -> Option<Vec<Item>> {
    let mut items = Vec::new();
    let mut total = 0usize;
    for p in paths {
        let name = p.file_name()?.to_string_lossy().to_string();
        walk(p, &name, &mut items, &mut total, max)?;
    }
    Some(items)
}

fn walk(p: &Path, rel: &str, items: &mut Vec<Item>, total: &mut usize, max: usize) -> Option<()> {
    let meta = std::fs::symlink_metadata(p).ok()?;
    if meta.file_type().is_symlink() {
        return Some(());
    }
    if meta.is_dir() {
        items.push(Item::Dir {
            path: rel.to_string(),
        });
        let mut entries: Vec<_> = std::fs::read_dir(p).ok()?.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let child = format!("{rel}/{}", e.file_name().to_string_lossy());
            walk(&e.path(), &child, items, total, max)?;
        }
    } else if meta.is_file() {
        *total += meta.len() as usize;
        if *total > max {
            return None;
        }
        items.push(Item::File {
            path: rel.to_string(),
            data: std::fs::read(p).ok()?,
        });
    }
    Some(())
}

/// Write the files and folders among `items` under `dir` (a fresh folder);
/// the top-level ones, in order, to put on the clipboard.
pub fn land(items: &[Item], dir: &Path) -> Result<Vec<PathBuf>, String> {
    // Older copies' files go: the clipboard now holds this one.
    if let Some(parent) = dir.parent() {
        for e in std::fs::read_dir(parent).into_iter().flatten().flatten() {
            if e.path() != dir {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut top: Vec<PathBuf> = Vec::new();
    for item in items {
        let (path, file) = match item {
            Item::File { path, data } => (path, Some(data)),
            Item::Dir { path } => (path, None),
            _ => continue,
        };
        // Checked when decoded; checked again, as this writes to disk.
        let Some(path) = clip::safe_path(path) else {
            continue;
        };
        let full = dir.join(&path);
        match file {
            Some(data) => {
                if let Some(p) = full.parent() {
                    std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                std::fs::write(&full, data).map_err(|e| format!("{}: {e}", full.display()))?;
            }
            None => std::fs::create_dir_all(&full).map_err(|e| e.to_string())?,
        }
        let first = dir.join(path.split('/').next().unwrap_or(&path));
        if !top.contains(&first) {
            top.push(first);
        }
    }
    Ok(top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_are_gathered_and_landed_whole() {
        let src = tempfile::tempdir().unwrap();
        let a = src.path().join("notes.txt");
        std::fs::write(&a, b"hello").unwrap();
        let d = src.path().join("Project");
        std::fs::create_dir_all(d.join("src/empty")).unwrap();
        std::fs::write(d.join("src/main.rs"), b"fn main() {}").unwrap();
        let items = gather(&[a, d.clone()], 1 << 20).unwrap();
        assert_eq!(
            items,
            vec![
                Item::File {
                    path: "notes.txt".into(),
                    data: b"hello".to_vec()
                },
                Item::Dir {
                    path: "Project".into()
                },
                Item::Dir {
                    path: "Project/src".into()
                },
                Item::Dir {
                    path: "Project/src/empty".into()
                },
                Item::File {
                    path: "Project/src/main.rs".into(),
                    data: b"fn main() {}".to_vec()
                },
            ]
        );
        // Too large: nothing.
        assert!(gather(&[d], 4).is_none());

        let out = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(out.path().join("old")).unwrap();
        let dir = out.path().join("42");
        let top = land(&items, &dir).unwrap();
        assert_eq!(top, vec![dir.join("notes.txt"), dir.join("Project")]);
        assert_eq!(
            std::fs::read(dir.join("Project/src/main.rs")).unwrap(),
            b"fn main() {}"
        );
        assert!(dir.join("Project/src/empty").is_dir());
        assert!(!out.path().join("old").exists(), "older copies go");
    }

    #[test]
    fn rates_follow_the_stream() {
        assert_eq!(rate_for(100_000), 6_250_000);
        assert_eq!(rate_for(1_000), 2 << 20);
        assert_eq!(rate_for(1_000_000), 25 << 20);
    }
}
