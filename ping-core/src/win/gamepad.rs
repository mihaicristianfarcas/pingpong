//! Game controllers on Windows (XInput: Xbox pads and whatever Steam Input
//! or DS4Windows presents as one), mirrored to the host as Xbox 360 pads,
//! slot for slot. The Guide button comes from XInputGetStateEx (ordinal 100),
//! which XInputGetState hides; the host's rumble goes to XInputSetState.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use pingpong_proto::control::Control;
use pingpong_proto::gamepad::{GamepadState, MAX_PADS};
use windows::core::w;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Input::XboxController::{
    XInputGetState, XInputSetState, XINPUT_STATE, XINPUT_VIBRATION,
};

use crate::pad::{PadMouse, PadSlot, Rumble, RUMBLE_TIMEOUT};

const POLL: Duration = Duration::from_millis(4);
/// With no controller connected: look for one this often (asking XInput
/// about an empty slot is slow, and 250 times a second is waste).
const IDLE_POLL: Duration = Duration::from_millis(250);
const ERROR_SUCCESS: u32 = 0;
/// XInput slots: at most four.
const SLOTS: usize = 4;

type GetStateEx = unsafe extern "system" fn(u32, *mut XINPUT_STATE) -> u32;

/// XInputGetStateEx, by ordinal: the same as XInputGetState, with the Guide
/// button (0x0400).
fn get_state_ex() -> Option<GetStateEx> {
    unsafe {
        let lib = LoadLibraryW(w!("xinput1_4.dll")).ok()?;
        // Ordinal 100: a pointer-sized integer where the name would be.
        let f = GetProcAddress(lib, windows::core::PCSTR(100 as *const u8))?;
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            GetStateEx,
        >(f))
    }
}

pub struct Gamepads {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Gamepads {
    /// `mouse`: let a controller drive the pointer when its Start is held
    /// (Moonlight's gamepad mouse emulation); None leaves it off.
    pub fn start(
        send: impl Fn(Control) + Send + 'static,
        rumble: Receiver<Rumble>,
        mouse: Option<PadMouse>,
    ) -> Gamepads {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("ping-gamepads".into())
                .spawn(move || poll_loop(&stop, &send, &rumble, mouse.as_ref()))
                .ok()
        };
        Gamepads { stop, thread }
    }
}

impl Drop for Gamepads {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Slot {
    pad: PadSlot,
    rumble_at: Option<Instant>,
}

fn read(get: Option<GetStateEx>, index: u32) -> Option<GamepadState> {
    let mut s = XINPUT_STATE::default();
    let status = unsafe {
        match get {
            Some(f) => f(index, &mut s),
            None => XInputGetState(index, &mut s),
        }
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let g = s.Gamepad;
    // XInput's button bits are the protocol's.
    Some(GamepadState {
        index: index as u8,
        seq: 0,
        connected: true,
        buttons: g.wButtons.0 as u32,
        left_trigger: g.bLeftTrigger,
        right_trigger: g.bRightTrigger,
        left_x: g.sThumbLX,
        left_y: g.sThumbLY,
        right_x: g.sThumbRX,
        right_y: g.sThumbRY,
    })
}

fn set_rumble(index: u8, low: u16, high: u16) {
    let v = XINPUT_VIBRATION {
        wLeftMotorSpeed: low,
        wRightMotorSpeed: high,
    };
    unsafe {
        XInputSetState(index as u32, &v);
    }
}

fn poll_loop(
    stop: &AtomicBool,
    send: &dyn Fn(Control),
    rumble: &Receiver<Rumble>,
    mouse: Option<&PadMouse>,
) {
    let get = get_state_ex();
    if get.is_none() {
        tracing::info!("XInputGetStateEx unavailable: the Guide button stays local");
    }
    let mut slots: [Option<Slot>; SLOTS] = Default::default();
    let mut seq = [0u16; MAX_PADS];
    let mut last_poll = Instant::now();
    let mut last_scan = Instant::now() - IDLE_POLL;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let dt = now.duration_since(last_poll).as_secs_f64().min(0.05);
        last_poll = now;
        // Empty slots are looked at less often: XInput is slow to say no.
        let scan = now.duration_since(last_scan) >= IDLE_POLL;
        if scan {
            last_scan = now;
        }
        for i in 0..SLOTS {
            if slots[i].is_none() && !scan {
                continue;
            }
            match read(get, i as u32) {
                Some(state) => {
                    let slot = slots[i].get_or_insert_with(|| {
                        tracing::info!(slot = i, "controller connected");
                        Slot {
                            pad: PadSlot::new(now),
                            rumble_at: None,
                        }
                    });
                    if let Some(msg) = slot.pad.update(i as u8, state, &mut seq[i], now, dt, mouse)
                    {
                        send(Control::Gamepad(msg));
                    }
                }
                None => {
                    if let Some(mut s) = slots[i].take() {
                        send(Control::Gamepad(s.pad.disconnect(
                            i as u8,
                            &mut seq[i],
                            mouse,
                        )));
                        tracing::info!(slot = i, "controller disconnected");
                    }
                }
            }
        }
        // The host's rumble.
        while let Ok((index, low, high)) = rumble.try_recv() {
            tracing::debug!(index, low, high, "host rumble");
            let Some(slot) = slots.get_mut(index as usize).and_then(Option::as_mut) else {
                continue;
            };
            set_rumble(index, low, high);
            slot.rumble_at = (low != 0 || high != 0).then_some(now);
        }
        for (i, slot) in slots.iter_mut().enumerate() {
            if let Some(s) = slot {
                if s.rumble_at
                    .is_some_and(|t| now.duration_since(t) > RUMBLE_TIMEOUT)
                {
                    set_rumble(i as u8, 0, 0);
                    s.rumble_at = None;
                }
            }
        }
        std::thread::sleep(if slots.iter().all(Option::is_none) {
            IDLE_POLL
        } else {
            POLL
        });
    }
    // Leave nothing held on the host, and no motor running here.
    for (i, slot) in slots.iter().enumerate() {
        if slot.is_some() {
            set_rumble(i as u8, 0, 0);
            send(Control::Gamepad(GamepadState {
                index: i as u8,
                seq: seq[i].wrapping_add(1),
                connected: false,
                ..Default::default()
            }));
        }
    }
}
