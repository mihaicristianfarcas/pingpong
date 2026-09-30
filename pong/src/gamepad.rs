//! The client's controllers as virtual Xbox 360 pads (ViGEmBus), as Apollo
//! does. One thread owns the pads: plugging one in takes a few milliseconds,
//! which the receive thread must not wait for. Games' rumble goes back to the
//! client, repeated while a motor runs so a lost packet cannot leave it on.

use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;
use pingpong_proto::control::Control;
use pingpong_proto::gamepad::{GamepadGate, GamepadState, MAX_PADS};
use pingpong_transport::{Endpoint, Peer};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

const RUMBLE_REPEAT: Duration = Duration::from_millis(200);
/// The client repeats each controller's state every 100 ms; one this late
/// has lost its connection. Its pad is centred, so a held trigger or a
/// pushed stick does not keep acting in the game.
const STALE_AFTER: Duration = Duration::from_secs(1);

pub struct Pads {
    tx: Option<Sender<GamepadState>>,
    thread: Option<JoinHandle<()>>,
}

impl Pads {
    pub fn start(endpoint: Arc<Endpoint>, peer: Arc<Peer>) -> Pads {
        let (tx, rx) = crossbeam_channel::bounded(256);
        let thread = std::thread::Builder::new()
            .name("gamepads".into())
            .spawn(move || run(rx, endpoint, peer))
            .ok();
        Pads {
            tx: Some(tx),
            thread,
        }
    }

    /// A state from the client (never blocks the receive thread).
    pub fn apply(&self, state: GamepadState) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(state);
        }
    }
}

impl Drop for Pads {
    /// Unplugs every pad (the session is over).
    fn drop(&mut self) {
        drop(self.tx.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Pad {
    target: Xbox360Wired<Arc<Client>>,
    rumble: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Rumble {
    low: u16,
    high: u16,
    sent: Option<Instant>,
}

fn run(rx: Receiver<GamepadState>, endpoint: Arc<Endpoint>, peer: Arc<Peer>) {
    let mut client: Option<Arc<Client>> = None;
    let mut bus_missing = false;
    let mut pads: [Option<Pad>; MAX_PADS] = Default::default();
    let mut gate = GamepadGate::new();
    // When each pad last heard from the client; None once centred.
    let mut heard: [Option<Instant>; MAX_PADS] = [None; MAX_PADS];
    let rumble: Arc<Mutex<[Rumble; MAX_PADS]>> = Arc::default();

    let send_rumble = |index: usize, r: &mut Rumble| {
        let msg = Control::Rumble {
            index: index as u8,
            low: r.low,
            high: r.high,
        };
        crate::session::send_control(&endpoint, &peer, msg);
        r.sent = Some(Instant::now());
    };

    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(state) => {
                if !gate.admit(&state) {
                    continue;
                }
                let i = state.index as usize;
                if !state.connected {
                    if pads[i].take().is_some() {
                        tracing::info!(slot = i, "virtual pad unplugged");
                    }
                    continue;
                }
                if pads[i].is_none() {
                    if client.is_none() && !bus_missing {
                        match Client::connect() {
                            Ok(c) => client = Some(Arc::new(c)),
                            Err(e) => {
                                bus_missing = true;
                                tracing::warn!(error = ?e, "ViGEmBus is not available; \
                                    controllers will not work");
                            }
                        }
                    }
                    let Some(c) = &client else { continue };
                    match plug(c.clone(), i, rumble.clone(), endpoint.clone(), peer.clone()) {
                        Ok(pad) => {
                            tracing::info!(slot = i, "virtual Xbox 360 pad plugged in");
                            pads[i] = Some(pad);
                        }
                        Err(e) => {
                            tracing::warn!(slot = i, error = ?e, "could not plug in a virtual pad");
                            continue;
                        }
                    }
                }
                if let Some(pad) = &mut pads[i] {
                    heard[i] = Some(Instant::now());
                    let report = XGamepad {
                        buttons: XButtons(state.buttons as u16),
                        left_trigger: state.left_trigger,
                        right_trigger: state.right_trigger,
                        thumb_lx: state.left_x,
                        thumb_ly: state.left_y,
                        thumb_rx: state.right_x,
                        thumb_ry: state.right_y,
                    };
                    if let Err(e) = pad.target.update(&report) {
                        tracing::debug!(slot = i, error = ?e, "pad update failed");
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        for (i, pad) in pads.iter_mut().enumerate() {
            let Some(pad) = pad else { continue };
            if heard[i].is_some_and(|t| t.elapsed() > STALE_AFTER) {
                heard[i] = None;
                tracing::info!(slot = i, "controller went quiet; centring its pad");
                if let Err(e) = pad.target.update(&XGamepad::default()) {
                    tracing::debug!(slot = i, error = ?e, "pad update failed");
                }
            }
        }
        // Keep a running motor's state flowing to the client.
        let mut r = rumble.lock();
        for (i, m) in r.iter_mut().enumerate() {
            if (m.low != 0 || m.high != 0) && m.sent.is_none_or(|t| t.elapsed() >= RUMBLE_REPEAT) {
                send_rumble(i, m);
            }
        }
    }
    // Pads unplug as they drop; stop any rumble the client is still playing.
    let mut r = rumble.lock();
    for (i, m) in r.iter_mut().enumerate() {
        if pads[i].is_some() && (m.low != 0 || m.high != 0) {
            *m = Rumble::default();
            send_rumble(i, m);
        }
    }
    drop(pads);
}

fn plug(
    client: Arc<Client>,
    index: usize,
    rumble: Arc<Mutex<[Rumble; MAX_PADS]>>,
    endpoint: Arc<Endpoint>,
    peer: Arc<Peer>,
) -> Result<Pad, vigem_client::Error> {
    let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
    target.plugin()?;
    target.wait_ready()?;
    let rumble = match target.request_notification() {
        Ok(n) => Some(n.spawn_thread(move |_, note| {
            // ViGEm reports motors as 0..=255.
            let (low, high) = (note.large_motor as u16 * 257, note.small_motor as u16 * 257);
            tracing::debug!(slot = index, low, high, "rumble from the game");
            let mut r = rumble.lock();
            let m = &mut r[index];
            if (m.low, m.high) != (low, high) {
                *m = Rumble {
                    low,
                    high,
                    sent: Some(Instant::now()),
                };
                crate::session::send_control(
                    &endpoint,
                    &peer,
                    Control::Rumble {
                        index: index as u8,
                        low,
                        high,
                    },
                );
            }
        })),
        Err(e) => {
            tracing::debug!(error = ?e, "no rumble notifications");
            None
        }
    };
    Ok(Pad { target, rumble })
}

impl Drop for Pad {
    fn drop(&mut self) {
        let _ = self.target.unplug();
        // The notification thread ends once its target is gone.
        if let Some(t) = self.rumble.take() {
            let _ = t.join();
        }
    }
}
