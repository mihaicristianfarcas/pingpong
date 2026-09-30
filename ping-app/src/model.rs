//! The host list, kept fresh the way Moonlight's is: browse the local network
//! and ask each paired host whether it is up, every few seconds; and the
//! pairing attempts, wakes and streams started from it.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ping_core::pair::{self, Discovered, Reachability};
use ping_core::store::{self, Hosts};
use pingpong_pairing::pair::Cancel;

const POLL_EVERY: Duration = Duration::from_secs(4);
/// With Ping in the background: nobody is looking at the list, and each
/// poll is a new mDNS browse and a probe of every host.
const POLL_EVERY_IN_BACKGROUND: Duration = Duration::from_secs(30);
const DISCOVER_FOR: Duration = Duration::from_millis(1200);
/// One browse can miss a host that is there, so a host stays listed until it
/// has been unseen this long.
const FORGET_AFTER: Duration = Duration::from_secs(30);
/// How long a woken host's card says "Waking…" at most.
const WAKE_FOR: Duration = Duration::from_secs(60);

/// A host this computer has paired with.
#[derive(Debug, Clone, PartialEq)]
pub struct Paired {
    pub name: String,
    /// The short id discovery announces.
    pub id: String,
    /// Its X25519 key: how the core names it.
    pub key: String,
    pub address: String,
    pub local_address: Option<String>,
    /// It announced how to wake it.
    pub can_wake: bool,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub name: String,
    pub paired: Option<Paired>,
    pub found: Option<Discovered>,
    /// None: not known yet.
    pub online: Option<bool>,
}

impl Item {
    pub fn is_paired(&self) -> bool {
        self.paired.is_some()
    }

    /// A Mac running Pong (known while it is on the network).
    pub fn is_mac(&self) -> bool {
        self.found.as_ref().is_some_and(|f| f.os == "macos")
    }

    /// Paired, and it announced how to wake it.
    pub fn can_wake(&self) -> bool {
        self.paired.as_ref().is_some_and(|p| p.can_wake)
    }

    /// Asleep (or off), but it can be woken: a click wakes it, as in Moonlight.
    pub fn offline_but_wakeable(&self) -> bool {
        self.can_wake() && self.online == Some(false)
    }

    /// The host's address and one of its ports, from discovery or else from
    /// the paired address (whose tunnel port the others follow).
    fn address_at(&self, found_port: impl Fn(&Discovered) -> u16, offset: u16) -> Option<String> {
        if let Some(f) = &self.found {
            return Some(join(f.address, found_port(f)));
        }
        let p = self.paired.as_ref()?;
        let (host, port) = split(p.local_address.as_deref().unwrap_or(&p.address))?;
        Some(join_str(&host, port.wrapping_add(offset)))
    }

    /// `HOST:PORT` of the host's pairing listener.
    pub fn pairing_address(&self) -> Option<String> {
        self.address_at(|f| f.pairing_port, 1)
    }

    /// Pong's web UI (where the PIN is typed).
    pub fn web_url(&self) -> Option<String> {
        self.address_at(|f| f.web_port, 2)
            .map(|a| format!("https://{a}"))
    }
}

fn join(ip: IpAddr, port: u16) -> String {
    join_str(&ip.to_string(), port)
}

fn join_str(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn split(address: &str) -> Option<(String, u16)> {
    let (host, port) = address.rsplit_once(':')?;
    Some((
        host.trim_matches(|c| c == '[' || c == ']').to_string(),
        port.parse().ok()?,
    ))
}

struct Poll {
    found: Vec<Discovered>,
    reach: Vec<Reachability>,
}

pub struct Model {
    pub items: Vec<Item>,
    pub searching: bool,
    /// Hosts sent Wake-on-LAN and not up yet: id → until when to say so.
    waking: HashMap<String, Instant>,
    seen: HashMap<String, (Discovered, Instant)>,
    /// Paired hosts' keys → whether they answered.
    reach: HashMap<String, bool>,
    dir: PathBuf,
    poll_now: Sender<()>,
    polled: Receiver<Poll>,
    /// While streaming, nothing polls (the network is the stream's).
    paused: Arc<AtomicBool>,
    /// Ping is the app in front: the list is polled at `POLL_EVERY`.
    foreground: Arc<AtomicBool>,
}

impl Model {
    /// Starts polling; `wake_ui` is called whenever there is news.
    pub fn new(wake_ui: impl Fn() + Send + 'static) -> Model {
        let dir = store::data_dir();
        let (poll_now, trigger) = crossbeam_channel::bounded::<()>(1);
        let (report, polled) = crossbeam_channel::unbounded();
        let paused = Arc::new(AtomicBool::new(false));
        let foreground = Arc::new(AtomicBool::new(true));
        {
            let (dir, paused, foreground) = (dir.clone(), paused.clone(), foreground.clone());
            std::thread::Builder::new()
                .name("ping-poll".into())
                .spawn(move || loop {
                    if !paused.load(Ordering::Relaxed) {
                        let found = pair::discover(&dir, DISCOVER_FOR).unwrap_or_default();
                        let reach = pair::probe(&dir);
                        if report.send(Poll { found, reach }).is_err() {
                            return;
                        }
                        wake_ui();
                    }
                    let every = if foreground.load(Ordering::Relaxed) {
                        POLL_EVERY
                    } else {
                        POLL_EVERY_IN_BACKGROUND
                    };
                    match trigger.recv_timeout(every) {
                        Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                    }
                })
                .expect("spawning the poll thread");
        }
        let mut model = Model {
            items: Vec::new(),
            searching: true,
            waking: HashMap::new(),
            seen: HashMap::new(),
            reach: HashMap::new(),
            dir,
            poll_now,
            polled,
            paused,
            foreground,
        };
        model.rebuild();
        model
    }

    /// Take in what the poll thread found. Returns whether anything did.
    pub fn update(&mut self) -> bool {
        let mut any = false;
        while let Ok(p) = self.polled.try_recv() {
            any = true;
            let now = Instant::now();
            for h in p.found {
                self.seen.insert(h.id.clone(), (h, now));
            }
            self.seen
                .retain(|_, (_, at)| now.duration_since(*at) < FORGET_AFTER);
            self.reach.clear();
            for r in p.reach {
                *self.reach.entry(r.x25519).or_default() |= r.via.is_some();
            }
            // Found on the network: it is up even if the probe raced it.
            let paired = Hosts::load(&self.dir);
            for (h, _) in self.seen.values().filter(|(h, _)| h.paired) {
                if let Some(k) = paired
                    .list()
                    .iter()
                    .find(|k| k.public().is_some_and(|p| p.short_id() == h.id))
                {
                    self.reach.insert(k.x25519.clone(), true);
                }
            }
            self.searching = false;
        }
        if any {
            self.rebuild();
        }
        let before = self.waking.len();
        let now = Instant::now();
        let online: Vec<String> = self
            .items
            .iter()
            .filter(|i| i.online == Some(true))
            .map(|i| i.id.clone())
            .collect();
        self.waking
            .retain(|id, until| now < *until && !online.contains(id));
        any || before != self.waking.len()
    }

    /// Re-read the paired hosts and merge in what the network says.
    pub fn rebuild(&mut self) {
        let hosts = Hosts::load(&self.dir);
        let mut list: Vec<Item> = hosts
            .list()
            .iter()
            .map(|h| {
                let id = h.public().map(|p| p.short_id()).unwrap_or_default();
                Item {
                    found: self.seen.get(&id).map(|(f, _)| f.clone()),
                    online: self.reach.get(&h.x25519).copied(),
                    name: h.name.clone(),
                    paired: Some(Paired {
                        name: h.name.clone(),
                        id: id.clone(),
                        key: h.x25519.clone(),
                        address: h.address.clone(),
                        local_address: h.local_address.clone(),
                        can_wake: !h.wake.is_empty(),
                    }),
                    id,
                }
            })
            .collect();
        for (f, _) in self.seen.values() {
            if !list.iter().any(|i| i.id == f.id) {
                list.push(Item {
                    id: f.id.clone(),
                    name: f.name.clone(),
                    paired: None,
                    found: Some(f.clone()),
                    online: Some(true),
                });
            }
        }
        list.sort_by(|a, b| (!a.is_paired(), &a.name).cmp(&(!b.is_paired(), &b.name)));
        // Hosts coming, going, waking up or going away.
        for item in &list {
            match self.items.iter().find(|i| i.id == item.id) {
                None => {
                    tracing::info!(host = item.name, paired = item.is_paired(), online = ?item.online, "host listed")
                }
                Some(old) if old.online != item.online && item.online.is_some() => {
                    tracing::info!(host = item.name, online = ?item.online, "host state")
                }
                _ => {}
            }
        }
        for old in &self.items {
            if !list.iter().any(|i| i.id == old.id) {
                tracing::info!(host = old.name, "host no longer listed");
            }
        }
        self.items = list;
    }

    /// Look again now.
    pub fn refresh(&mut self) {
        self.searching = true;
        let _ = self.poll_now.try_send(());
    }

    /// Ping came to the front (poll now, then often) or went behind (poll
    /// seldom).
    pub fn set_foreground(&self, foreground: bool) {
        if self.foreground.swap(foreground, Ordering::Relaxed) != foreground && foreground {
            let _ = self.poll_now.try_send(());
        }
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_waking(&self, item: &Item) -> bool {
        self.waking.contains_key(&item.id)
    }

    /// Moonlight's "Wake PC": send the magic packets, then watch for the
    /// host for up to a minute, its card saying so meanwhile.
    pub fn wake(&mut self, item: &Item) -> Result<(), String> {
        let key = item
            .paired
            .as_ref()
            .map(|p| p.key.clone())
            .ok_or("That host is not paired.")?;
        let hosts = Hosts::load(&self.dir);
        let host = hosts
            .list()
            .iter()
            .find(|h| h.x25519 == key)
            .ok_or("That host is not paired.")?;
        ping_core::wake::wake(host)?;
        self.waking
            .insert(item.id.clone(), Instant::now() + WAKE_FOR);
        self.refresh();
        Ok(())
    }

    /// Forget a paired host.
    pub fn remove(&mut self, item: &Item) {
        let mut hosts = Hosts::load(&self.dir);
        if let Some(p) = &item.paired {
            if let Err(e) = hosts.remove(&p.name) {
                tracing::warn!(error = %e, "host not removed");
            }
        }
        self.rebuild();
    }
}

// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum PairState {
    Connecting,
    /// The host is showing the prompt for the PIN.
    Waiting,
    /// Paired with the host of this name.
    Paired(String),
    Failed(String),
}

/// One pairing attempt: this computer shows `pin`; the user types it into
/// Pong's web UI.
pub struct Pairing {
    pub title: String,
    pub web_url: Option<String>,
    pub pin: String,
    pub state: PairState,
    cancel: Cancel,
    events: Receiver<PairState>,
}

impl Pairing {
    /// Pair this device's AI agent (its own identity) instead of Ping.
    pub fn start_as(
        title: String,
        address: String,
        web_url: Option<String>,
        agent: bool,
        wake_ui: impl Fn() + Send + 'static,
    ) -> Pairing {
        let pin = pingpong_pairing::pair::new_pin();
        let cancel = Cancel::default();
        let (tx, events) = crossbeam_channel::unbounded();
        {
            let (pin, cancel) = (pin.clone(), cancel.clone());
            std::thread::Builder::new()
                .name("ping-pair".into())
                .spawn(move || {
                    let dir = store::data_dir();
                    let waiting = {
                        let tx = tx.clone();
                        move || {
                            let _ = tx.send(PairState::Waiting);
                        }
                    };
                    let waiting = {
                        let wake_ui = &wake_ui;
                        move || {
                            waiting();
                            wake_ui();
                        }
                    };
                    let result = pair::resolve(&address).and_then(|addr| {
                        if agent {
                            pair::pair_agent(&dir, addr, &pin, &cancel, waiting)
                        } else {
                            pair::pair_with(
                                &dir,
                                addr,
                                &pair::client_name(),
                                &pin,
                                &cancel,
                                waiting,
                            )
                        }
                    });
                    let _ = tx.send(match result {
                        Ok(host) => PairState::Paired(host.name),
                        // Cancelled: the sheet is gone already.
                        Err(_) if cancel.is_cancelled() => return,
                        Err(e) => PairState::Failed(e),
                    });
                    wake_ui();
                })
                .expect("spawning the pairing thread");
        }
        Pairing {
            title,
            web_url,
            pin,
            state: PairState::Connecting,
            cancel,
            events,
        }
    }

    /// Take in the pairing's progress. Returns true once it has just paired.
    pub fn update(&mut self) -> bool {
        let mut paired = false;
        while let Ok(s) = self.events.try_recv() {
            paired |= matches!(s, PairState::Paired(_));
            self.state = s;
        }
        paired
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paired(address: &str, local: Option<&str>) -> Item {
        Item {
            id: "abcd".into(),
            name: "gaming-pc".into(),
            paired: Some(Paired {
                name: "gaming-pc".into(),
                id: "abcd".into(),
                key: "k".into(),
                address: address.into(),
                local_address: local.map(Into::into),
                can_wake: true,
            }),
            found: None,
            online: Some(false),
        }
    }

    #[test]
    fn ports_follow_the_tunnel_port_without_discovery() {
        let item = paired("192.168.1.30:47800", None);
        assert_eq!(
            item.pairing_address().as_deref(),
            Some("192.168.1.30:47801")
        );
        assert_eq!(
            item.web_url().as_deref(),
            Some("https://192.168.1.30:47802")
        );
        let v6 = paired("[fe80::1]:47800", Some("[fe80::2]:47800"));
        assert_eq!(v6.pairing_address().as_deref(), Some("[fe80::2]:47801"));
        assert!(item.offline_but_wakeable());
    }
}
