//! Game controllers on the Mac (GameController framework: Xbox, PlayStation,
//! Switch Pro and MFi pads), mirrored to the host as Xbox 360 pads the way
//! Moonlight maps them.
//!
//! A background thread polls every connected controller; a controller's whole
//! state goes out when it changes, and again every 100 ms so a lost packet is
//! corrected without waiting for the next change. The host's rumble plays
//! through CoreHaptics the way SDL (and so Moonlight) does it: the heavy motor
//! on the left handle, the light one on the right.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::AllocAnyThread;
use objc2_core_haptics::{
    CHHapticDynamicParameter, CHHapticDynamicParameterIDHapticIntensityControl, CHHapticEngine,
    CHHapticEvent, CHHapticEventParameter, CHHapticEventParameterIDHapticIntensity,
    CHHapticEventParameterIDHapticSharpness, CHHapticEventTypeHapticContinuous, CHHapticPattern,
    CHHapticPatternPlayer,
};
use objc2_foundation::NSArray;
use objc2_game_controller::{
    GCController, GCControllerButtonInput, GCDevice, GCExtendedGamepad, GCHapticDurationInfinite,
    GCHapticsLocalityDefault, GCHapticsLocalityLeftHandle, GCHapticsLocalityRightHandle,
    GCSystemGestureState,
};
use pingpong_proto::control::Control;
use pingpong_proto::gamepad::{button, GamepadState, MAX_PADS};

use crate::pad::{PadMouse, PadSlot, Rumble, RUMBLE_TIMEOUT};

const POLL: Duration = Duration::from_millis(4);
/// With no controller connected: look for one this often (a 4 ms poll
/// would wake the Mac 250 times a second for nothing).
const IDLE_POLL: Duration = Duration::from_millis(100);

struct Slot {
    controller: usize,
    pad: PadSlot,
    haptics: Option<Haptics>,
    rumble_at: Option<Instant>,
}

/// One continuous haptic event, its intensity driven by the host's motor.
struct Motor {
    _engine: Retained<CHHapticEngine>,
    player: Retained<ProtocolObject<dyn CHHapticPatternPlayer>>,
    playing: bool,
}

impl Motor {
    fn new(
        controller: &GCController,
        locality: &objc2_game_controller::GCHapticsLocality,
    ) -> Option<Motor> {
        unsafe {
            // macOS 11+ has this; the generated binding only exposes it for iOS.
            let Some(haptics) = controller.haptics() else {
                tracing::debug!("the controller offers no haptics");
                return None;
            };
            let engine: Option<Retained<CHHapticEngine>> =
                objc2::msg_send![&*haptics, createEngineWithLocality: locality];
            let Some(engine) = engine else {
                tracing::debug!(locality = %locality, "no haptic engine for this locality");
                return None;
            };
            if let Err(e) = engine.startAndReturnError() {
                tracing::warn!(error = %e, "the controller's haptic engine did not start");
                return None;
            }
            let params = NSArray::from_retained_slice(&[
                CHHapticEventParameter::initWithParameterID_value(
                    CHHapticEventParameter::alloc(),
                    CHHapticEventParameterIDHapticIntensity,
                    1.0,
                ),
                CHHapticEventParameter::initWithParameterID_value(
                    CHHapticEventParameter::alloc(),
                    CHHapticEventParameterIDHapticSharpness,
                    1.0,
                ),
            ]);
            let event = CHHapticEvent::initWithEventType_parameters_relativeTime_duration(
                CHHapticEvent::alloc(),
                CHHapticEventTypeHapticContinuous,
                &params,
                0.0,
                GCHapticDurationInfinite as f64,
            );
            let pattern = CHHapticPattern::initWithEvents_parameters_error(
                CHHapticPattern::alloc(),
                &NSArray::from_retained_slice(&[event]),
                &NSArray::new(),
            )
            .ok()?;
            // A plain player, as SDL makes: an advanced one is refused for a
            // controller ("Couldn't communicate with a helper application").
            let player = match engine.createPlayerWithPattern_error(&pattern) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "no haptic player");
                    return None;
                }
            };
            Some(Motor {
                _engine: engine,
                player,
                playing: false,
            })
        }
    }

    fn set(&mut self, intensity: f32) {
        unsafe {
            if intensity <= 0.0 {
                if self.playing {
                    let _ = self.player.stopAtTime_error(0.0);
                    self.playing = false;
                }
                return;
            }
            if !self.playing {
                match self.player.startAtTime_error(0.0) {
                    Ok(()) => self.playing = true,
                    Err(e) => tracing::warn!(error = %e, "rumble did not start"),
                }
            }
            let p = CHHapticDynamicParameter::initWithParameterID_value_relativeTime(
                CHHapticDynamicParameter::alloc(),
                CHHapticDynamicParameterIDHapticIntensityControl,
                intensity.min(1.0),
                0.0,
            );
            let _ = self
                .player
                .sendParameters_atTime_error(&NSArray::from_retained_slice(&[p]), 0.0);
        }
    }
}

/// A controller's motors: left and right handles, or one for the whole pad.
struct Haptics {
    left: Option<Motor>,
    right: Option<Motor>,
}

impl Haptics {
    fn new(controller: &GCController) -> Haptics {
        let (left, right) = unsafe {
            (
                Motor::new(controller, GCHapticsLocalityLeftHandle),
                Motor::new(controller, GCHapticsLocalityRightHandle),
            )
        };
        if left.is_some() || right.is_some() {
            tracing::info!(
                left = left.is_some(),
                right = right.is_some(),
                "controller rumble ready"
            );
            return Haptics { left, right };
        }
        let whole = unsafe { Motor::new(controller, GCHapticsLocalityDefault) };
        tracing::info!(
            motor = whole.is_some(),
            "controller rumble ready (one motor)"
        );
        Haptics {
            left: whole,
            right: None,
        }
    }

    fn set(&mut self, low: u16, high: u16) {
        let (low, high) = (low as f32 / 65535.0, high as f32 / 65535.0);
        match (&mut self.left, &mut self.right) {
            (Some(l), Some(r)) => {
                l.set(low);
                r.set(high);
            }
            (Some(m), None) | (None, Some(m)) => m.set(low.max(high)),
            (None, None) => {}
        }
    }
}

/// The host's rumble for controller `index`: (index, low, high).
pub type RumbleSender = crossbeam_channel::Sender<Rumble>;

pub struct Gamepads {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Gamepads {
    /// A channel for the host's rumble, made before the stream so its events
    /// can feed it.
    pub fn rumble_channel() -> (crossbeam_channel::Sender<Rumble>, Receiver<Rumble>) {
        crate::pad::rumble_channel()
    }

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
                .spawn(move || {
                    let home = super::hid::HomeButtons::start();
                    poll_loop(&stop, &send, &rumble, mouse.as_ref(), &home)
                })
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

fn pressed(b: &GCControllerButtonInput) -> bool {
    unsafe { b.isPressed() }
}

fn axis(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn read(pad: &GCExtendedGamepad) -> GamepadState {
    unsafe {
        let mut buttons = 0u32;
        let mut set = |bit: u32, on: bool| {
            if on {
                buttons |= bit;
            }
        };
        let dpad = pad.dpad();
        set(button::DPAD_UP, pressed(&dpad.up()));
        set(button::DPAD_DOWN, pressed(&dpad.down()));
        set(button::DPAD_LEFT, pressed(&dpad.left()));
        set(button::DPAD_RIGHT, pressed(&dpad.right()));
        set(button::A, pressed(&pad.buttonA()));
        set(button::B, pressed(&pad.buttonB()));
        set(button::X, pressed(&pad.buttonX()));
        set(button::Y, pressed(&pad.buttonY()));
        set(button::LEFT_SHOULDER, pressed(&pad.leftShoulder()));
        set(button::RIGHT_SHOULDER, pressed(&pad.rightShoulder()));
        set(button::START, pressed(&pad.buttonMenu()));
        set(
            button::BACK,
            pad.buttonOptions().is_some_and(|b| pressed(&b)),
        );
        set(button::GUIDE, pad.buttonHome().is_some_and(|b| pressed(&b)));
        set(
            button::LEFT_THUMB,
            pad.leftThumbstickButton().is_some_and(|b| pressed(&b)),
        );
        set(
            button::RIGHT_THUMB,
            pad.rightThumbstickButton().is_some_and(|b| pressed(&b)),
        );
        let (l, r) = (pad.leftThumbstick(), pad.rightThumbstick());
        GamepadState {
            index: 0,
            seq: 0,
            connected: true,
            buttons,
            left_trigger: (pad.leftTrigger().value().clamp(0.0, 1.0) * 255.0).round() as u8,
            right_trigger: (pad.rightTrigger().value().clamp(0.0, 1.0) * 255.0).round() as u8,
            left_x: axis(l.xAxis().value()),
            left_y: axis(l.yAxis().value()),
            right_x: axis(r.xAxis().value()),
            right_y: axis(r.yAxis().value()),
        }
    }
}

/// The Home and Share buttons open system overlays by default; while
/// streaming they belong to the host, as in Moonlight. Every button bound to
/// a gesture is claimed, on the main thread, as SDL does. macOS 26 ignores
/// this for the Home button: its Games overlay opens and takes the focus,
/// the press never reaching the app (Moonlight, which also reads the
/// controller's raw HID reports, gets it and the overlay both).
fn claim_system_buttons() {
    dispatch2::DispatchQueue::main().exec_async(|| unsafe {
        for c in GCController::controllers().iter() {
            for b in c.physicalInputProfile().buttons().allValues().iter() {
                if !b.isBoundToSystemGesture() {
                    continue;
                }
                b.setPreferredSystemGestureState(GCSystemGestureState::Disabled);
                tracing::debug!(button = ?b.localizedName().map(|n| n.to_string()), "claimed from the system");
            }
        }
    });
}

fn poll_loop(
    stop: &AtomicBool,
    send: &dyn Fn(Control),
    rumble: &Receiver<Rumble>,
    mouse: Option<&PadMouse>,
    home: &super::hid::HomeButtons,
) {
    let mut slots: [Option<Slot>; MAX_PADS] = Default::default();
    let mut seq = [0u16; MAX_PADS];
    let mut last_poll = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // A pool per poll: GameController hands back autoreleased objects
        // (the controller list, each element read), and a thread without one
        // keeps them all until it exits -- measured as ~0.5 MB a minute.
        objc2::rc::autoreleasepool(|_| {
            let controllers: Vec<Retained<GCController>> =
                unsafe { GCController::controllers() }.to_vec();
            let now = Instant::now();
            let dt = now.duration_since(last_poll).as_secs_f64().min(0.05);
            last_poll = now;

            // Controllers that went away: unplug their pads.
            for (i, slot) in slots.iter_mut().enumerate() {
                let Some(s) = slot else { continue };
                if !controllers
                    .iter()
                    .any(|c| Retained::as_ptr(c) as usize == s.controller)
                {
                    send(Control::Gamepad(s.pad.disconnect(
                        i as u8,
                        &mut seq[i],
                        mouse,
                    )));
                    tracing::info!(slot = i, "controller disconnected");
                    *slot = None;
                }
            }

            for c in &controllers {
                let Some(pad) = (unsafe { c.extendedGamepad() }) else {
                    continue;
                };
                let id = Retained::as_ptr(c) as usize;
                let index = match slots
                    .iter()
                    .position(|s| s.as_ref().is_some_and(|s| s.controller == id))
                {
                    Some(i) => i,
                    None => {
                        let Some(free) = slots.iter().position(Option::is_none) else {
                            continue;
                        };
                        claim_system_buttons();
                        let name = unsafe { c.vendorName() }
                            .map(|n| n.to_string())
                            .unwrap_or_default();
                        tracing::info!(slot = free, controller = name, "controller connected");
                        slots[free] = Some(Slot {
                            controller: id,
                            pad: PadSlot::new(now),
                            haptics: None,
                            rumble_at: None,
                        });
                        free
                    }
                };
                let slot = slots[index].as_mut().expect("assigned above");
                let mut state = read(&pad);
                // The Home button macOS keeps (see `hid`): the only
                // controller's, or with several, the first's.
                if home.pressed() && (controllers.len() == 1 || index == 0) {
                    state.buttons |= button::GUIDE;
                }
                if let Some(msg) =
                    slot.pad
                        .update(index as u8, state, &mut seq[index], now, dt, mouse)
                {
                    send(Control::Gamepad(msg));
                }
            }
            // The host's rumble.
            while let Ok((index, low, high)) = rumble.try_recv() {
                tracing::debug!(index, low, high, "host rumble");
                let Some(slot) = slots.get_mut(index as usize).and_then(Option::as_mut) else {
                    continue;
                };
                let Some(c) = controllers
                    .iter()
                    .find(|c| Retained::as_ptr(c) as usize == slot.controller)
                else {
                    continue;
                };
                slot.haptics
                    .get_or_insert_with(|| Haptics::new(c))
                    .set(low, high);
                slot.rumble_at = (low != 0 || high != 0).then_some(now);
            }
            for slot in slots.iter_mut().flatten() {
                if slot
                    .rumble_at
                    .is_some_and(|t| now.duration_since(t) > RUMBLE_TIMEOUT)
                {
                    if let Some(h) = &mut slot.haptics {
                        h.set(0, 0);
                    }
                    slot.rumble_at = None;
                }
            }
        });
        std::thread::sleep(if slots.iter().all(Option::is_none) {
            IDLE_POLL
        } else {
            POLL
        });
    }
    // Leave nothing held on the host.
    for (i, slot) in slots.iter().enumerate() {
        if slot.is_some() {
            send(Control::Gamepad(GamepadState {
                index: i as u8,
                seq: seq[i].wrapping_add(1),
                connected: false,
                ..Default::default()
            }));
        }
    }
}
