//! Game controllers on Linux (gilrs over evdev: Xbox, PlayStation, Switch
//! Pro and most others, mapped by SDL's database), mirrored to the host as
//! Xbox 360 pads, slot for slot in the order they connect. The host's rumble
//! plays as force feedback, the heavy motor as the strong effect.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use gilrs::ff::{BaseEffect, BaseEffectType, EffectBuilder, Replay, Ticks};
use gilrs::{Axis, Button, GamepadId, Gilrs};
use pingpong_proto::control::Control;
use pingpong_proto::gamepad::{button, GamepadState, MAX_PADS};

use crate::pad::{PadMouse, PadSlot, Rumble, RUMBLE_TIMEOUT};

const POLL: Duration = Duration::from_millis(4);
const IDLE_POLL: Duration = Duration::from_millis(100);

pub struct Gamepads {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Gamepads {
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
                .spawn(move || match Gilrs::new() {
                    Ok(gilrs) => poll_loop(gilrs, &stop, &send, &rumble, mouse.as_ref()),
                    Err(e) => tracing::info!(error = %e, "no controllers (gilrs)"),
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

struct Slot {
    id: GamepadId,
    pad: PadSlot,
    effect: Option<gilrs::ff::Effect>,
    rumble_at: Option<Instant>,
}

fn axis(v: f32) -> i16 {
    (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

fn read(g: &gilrs::Gamepad<'_>) -> GamepadState {
    let mut buttons = 0u32;
    for (b, bit) in [
        (Button::DPadUp, button::DPAD_UP),
        (Button::DPadDown, button::DPAD_DOWN),
        (Button::DPadLeft, button::DPAD_LEFT),
        (Button::DPadRight, button::DPAD_RIGHT),
        (Button::Start, button::START),
        (Button::Select, button::BACK),
        (Button::LeftThumb, button::LEFT_THUMB),
        (Button::RightThumb, button::RIGHT_THUMB),
        (Button::LeftTrigger, button::LEFT_SHOULDER),
        (Button::RightTrigger, button::RIGHT_SHOULDER),
        (Button::Mode, button::GUIDE),
        (Button::South, button::A),
        (Button::East, button::B),
        (Button::West, button::X),
        (Button::North, button::Y),
    ] {
        if g.is_pressed(b) {
            buttons |= bit;
        }
    }
    let trigger = |b: Button| {
        (g.button_data(b).map_or(0.0, |d| d.value()).clamp(0.0, 1.0) * 255.0).round() as u8
    };
    GamepadState {
        index: 0,
        seq: 0,
        connected: true,
        buttons,
        left_trigger: trigger(Button::LeftTrigger2),
        right_trigger: trigger(Button::RightTrigger2),
        left_x: axis(g.value(Axis::LeftStickX)),
        left_y: axis(g.value(Axis::LeftStickY)),
        right_x: axis(g.value(Axis::RightStickX)),
        right_y: axis(g.value(Axis::RightStickY)),
    }
}

fn rumble_effect(
    gilrs: &mut Gilrs,
    id: GamepadId,
    low: u16,
    high: u16,
) -> Option<gilrs::ff::Effect> {
    let replay = Replay {
        play_for: Ticks::from_ms(RUMBLE_TIMEOUT.as_millis() as u32),
        ..Default::default()
    };
    let effect = EffectBuilder::new()
        .add_effect(BaseEffect {
            kind: BaseEffectType::Strong { magnitude: low },
            scheduling: replay,
            ..Default::default()
        })
        .add_effect(BaseEffect {
            kind: BaseEffectType::Weak { magnitude: high },
            scheduling: replay,
            ..Default::default()
        })
        .gamepads(&[id])
        .finish(gilrs)
        .ok()?;
    effect.play().ok()?;
    Some(effect)
}

fn poll_loop(
    mut gilrs: Gilrs,
    stop: &AtomicBool,
    send: &dyn Fn(Control),
    rumble: &Receiver<Rumble>,
    mouse: Option<&PadMouse>,
) {
    let mut slots: [Option<Slot>; MAX_PADS] = Default::default();
    let mut seq = [0u16; MAX_PADS];
    let mut last_poll = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // Events keep gilrs' state current; the state is read below.
        while gilrs.next_event().is_some() {}
        let now = Instant::now();
        let dt = now.duration_since(last_poll).as_secs_f64().min(0.05);
        last_poll = now;

        let connected: Vec<GamepadId> = gilrs
            .gamepads()
            .filter(|(_, g)| g.is_connected())
            .map(|(id, _)| id)
            .collect();
        for (i, slot) in slots.iter_mut().enumerate() {
            if slot.as_ref().is_some_and(|s| !connected.contains(&s.id)) {
                let mut s = slot.take().expect("checked");
                send(Control::Gamepad(s.pad.disconnect(
                    i as u8,
                    &mut seq[i],
                    mouse,
                )));
                tracing::info!(slot = i, "controller disconnected");
            }
        }
        for id in connected {
            let index = match slots
                .iter()
                .position(|s| s.as_ref().is_some_and(|s| s.id == id))
            {
                Some(i) => i,
                None => {
                    let Some(free) = slots.iter().position(Option::is_none) else {
                        continue;
                    };
                    tracing::info!(
                        slot = free,
                        controller = gilrs.gamepad(id).name(),
                        "controller connected"
                    );
                    slots[free] = Some(Slot {
                        id,
                        pad: PadSlot::new(now),
                        effect: None,
                        rumble_at: None,
                    });
                    free
                }
            };
            let state = read(&gilrs.gamepad(id));
            let slot = slots[index].as_mut().expect("assigned above");
            if let Some(msg) = slot
                .pad
                .update(index as u8, state, &mut seq[index], now, dt, mouse)
            {
                send(Control::Gamepad(msg));
            }
        }
        // The host's rumble.
        while let Ok((index, low, high)) = rumble.try_recv() {
            let Some(slot) = slots.get_mut(index as usize).and_then(Option::as_mut) else {
                continue;
            };
            slot.effect = if low != 0 || high != 0 {
                rumble_effect(&mut gilrs, slot.id, low, high)
            } else {
                None
            };
            slot.rumble_at = (low != 0 || high != 0).then_some(now);
        }
        for slot in slots.iter_mut().flatten() {
            if slot
                .rumble_at
                .is_some_and(|t| now.duration_since(t) > RUMBLE_TIMEOUT)
            {
                slot.effect = None;
                slot.rumble_at = None;
            }
        }
        std::thread::sleep(if slots.iter().all(Option::is_none) {
            IDLE_POLL
        } else {
            POLL
        });
    }
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
