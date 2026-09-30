//! macOS playback through the default output AudioUnit. It follows the
//! system's default device (AirPods connecting, say) and converts our 48 kHz
//! float stream to whatever the device runs at.

use std::ffi::c_void;

use crate::player::{Feeder, Output};
use pingpong_proto::audio::SAMPLE_RATE;

type OSStatus = i32;
type AudioComponent = *mut c_void;
type AudioUnit = *mut c_void;

#[repr(C)]
struct AudioComponentDescription {
    component_type: u32,
    component_sub_type: u32,
    component_manufacturer: u32,
    component_flags: u32,
    component_flags_mask: u32,
}

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

type RenderCallback = unsafe extern "C" fn(
    ref_con: *mut c_void,
    action_flags: *mut u32,
    time_stamp: *const c_void,
    bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus;

#[repr(C)]
struct AURenderCallbackStruct {
    input_proc: RenderCallback,
    input_proc_ref_con: *mut c_void,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(
        component: AudioComponent,
        desc: *const AudioComponentDescription,
    ) -> AudioComponent;
    fn AudioComponentInstanceNew(component: AudioComponent, out: *mut AudioUnit) -> OSStatus;
    fn AudioComponentInstanceDispose(unit: AudioUnit) -> OSStatus;
    fn AudioUnitSetProperty(
        unit: AudioUnit,
        id: u32,
        scope: u32,
        element: u32,
        data: *const c_void,
        size: u32,
    ) -> OSStatus;
    fn AudioUnitInitialize(unit: AudioUnit) -> OSStatus;
    fn AudioUnitUninitialize(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStart(unit: AudioUnit) -> OSStatus;
    fn AudioOutputUnitStop(unit: AudioUnit) -> OSStatus;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const K_AUDIO_UNIT_TYPE_OUTPUT: u32 = fourcc(b"auou");
const K_AUDIO_UNIT_SUB_TYPE_DEFAULT_OUTPUT: u32 = fourcc(b"def ");
const K_AUDIO_UNIT_MANUFACTURER_APPLE: u32 = fourcc(b"appl");
const K_AUDIO_FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm");
const K_AUDIO_FORMAT_FLAG_IS_FLOAT: u32 = 1;
const K_AUDIO_FORMAT_FLAG_IS_PACKED: u32 = 8;
const K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT: u32 = 8;
const K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK: u32 = 23;
const K_AUDIO_UNIT_PROPERTY_AUDIO_CHANNEL_LAYOUT: u32 = 19;
/// kAudioChannelLayoutTag_UseChannelDescriptions.
const K_LAYOUT_USE_DESCRIPTIONS: u32 = 0;
const K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE: u32 = fourcc(b"fsiz");
const K_AUDIO_UNIT_SCOPE_GLOBAL: u32 = 0;
const K_AUDIO_UNIT_SCOPE_INPUT: u32 = 1;

/// Device I/O buffer we ask for: 5 ms at 48 kHz, down from the default ~11.
const IO_FRAMES: u32 = 240;

pub struct CoreAudioOutput {
    unit: AudioUnit,
    feeder: *mut Feeder,
}

// The unit and feeder are only touched again on drop, after the unit stops.
unsafe impl Send for CoreAudioOutput {}

impl Output for CoreAudioOutput {}

unsafe extern "C" fn render(
    ref_con: *mut c_void,
    _flags: *mut u32,
    _ts: *const c_void,
    _bus: u32,
    frames: u32,
    data: *mut AudioBufferList,
) -> OSStatus {
    let feeder = unsafe { &mut *(ref_con as *mut Feeder) };
    let list = unsafe { &mut *data };
    let buf = &mut list.buffers[0];
    let len = (frames as usize * feeder.channels()).min(buf.data_byte_size as usize / 4);
    let out = unsafe { std::slice::from_raw_parts_mut(buf.data as *mut f32, len) };
    feeder.fill(out);
    0
}

fn check(what: &str, status: OSStatus) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("{what} failed (OSStatus {status})"))
    }
}

/// Open the default output device and start pulling from `feeder`.
pub fn open(feeder: Feeder) -> Result<Box<dyn Output>, String> {
    let channels = feeder.channels() as u32;
    let desc = AudioComponentDescription {
        component_type: K_AUDIO_UNIT_TYPE_OUTPUT,
        component_sub_type: K_AUDIO_UNIT_SUB_TYPE_DEFAULT_OUTPUT,
        component_manufacturer: K_AUDIO_UNIT_MANUFACTURER_APPLE,
        component_flags: 0,
        component_flags_mask: 0,
    };
    unsafe {
        let component = AudioComponentFindNext(std::ptr::null_mut(), &desc);
        if component.is_null() {
            return Err("no default audio output".into());
        }
        let mut unit: AudioUnit = std::ptr::null_mut();
        check(
            "AudioComponentInstanceNew",
            AudioComponentInstanceNew(component, &mut unit),
        )?;
        let feeder = Box::into_raw(Box::new(feeder));
        let out = CoreAudioOutput { unit, feeder };

        let asbd = AudioStreamBasicDescription {
            sample_rate: SAMPLE_RATE as f64,
            format_id: K_AUDIO_FORMAT_LINEAR_PCM,
            format_flags: K_AUDIO_FORMAT_FLAG_IS_FLOAT | K_AUDIO_FORMAT_FLAG_IS_PACKED,
            bytes_per_packet: 4 * channels,
            frames_per_packet: 1,
            bytes_per_frame: 4 * channels,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        };
        check(
            "setting the stream format",
            AudioUnitSetProperty(
                unit,
                K_AUDIO_UNIT_PROPERTY_STREAM_FORMAT,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
                &asbd as *const _ as *const c_void,
                std::mem::size_of::<AudioStreamBasicDescription>() as u32,
            ),
        )?;
        // Say which speaker each channel is for. Without it the output keeps
        // the first channels the device has and drops the rest -- on stereo
        // speakers, the centre channel with the dialogue in it; with it,
        // CoreAudio downmixes (or spatializes) properly.
        if let Some(layout) = channel_layout(channels) {
            let status = AudioUnitSetProperty(
                unit,
                K_AUDIO_UNIT_PROPERTY_AUDIO_CHANNEL_LAYOUT,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
                layout.as_ptr() as *const c_void,
                (layout.len() * 4) as u32,
            );
            if status != 0 {
                tracing::warn!(
                    status,
                    channels,
                    "could not set the surround channel layout"
                );
            } else {
                tracing::debug!(channels, "surround channel layout set");
            }
        }
        let cb = AURenderCallbackStruct {
            input_proc: render,
            input_proc_ref_con: feeder as *mut c_void,
        };
        check(
            "setting the render callback",
            AudioUnitSetProperty(
                unit,
                K_AUDIO_UNIT_PROPERTY_SET_RENDER_CALLBACK,
                K_AUDIO_UNIT_SCOPE_INPUT,
                0,
                &cb as *const _ as *const c_void,
                std::mem::size_of::<AURenderCallbackStruct>() as u32,
            ),
        )?;
        let frames = IO_FRAMES;
        let status = AudioUnitSetProperty(
            unit,
            K_AUDIO_DEVICE_PROPERTY_BUFFER_FRAME_SIZE,
            K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            &frames as *const u32 as *const c_void,
            4,
        );
        if status != 0 {
            tracing::debug!(status, "could not shrink the output buffer");
        }
        check("AudioUnitInitialize", AudioUnitInitialize(unit))?;
        check("AudioOutputUnitStart", AudioOutputUnitStart(unit))?;
        Ok(Box::new(out))
    }
}

/// An AudioChannelLayout (as u32 words) naming each channel in Windows'
/// order, which the stream keeps (see `opus::layout`).
fn channel_layout(channels: u32) -> Option<Vec<u32>> {
    // kAudioChannelLabel_*: Left 1, Right 2, Center 3, LFEScreen 4,
    // LeftSurround 5, RightSurround 6, RearSurroundLeft 33, RearSurroundRight 34.
    let labels: &[u32] = match channels {
        // FL FR FC LFE BL BR: 5.1's back pair plays as the surrounds.
        6 => &[1, 2, 3, 4, 5, 6],
        // FL FR FC LFE BL BR SL SR.
        8 => &[1, 2, 3, 4, 33, 34, 5, 6],
        _ => return None,
    };
    // Tag, bitmap, count, then per channel: label, flags, 3 float coordinates.
    let mut words = vec![K_LAYOUT_USE_DESCRIPTIONS, 0, labels.len() as u32];
    for &label in labels {
        words.extend_from_slice(&[label, 0, 0, 0, 0]);
    }
    Some(words)
}

impl Drop for CoreAudioOutput {
    fn drop(&mut self) {
        unsafe {
            AudioOutputUnitStop(self.unit);
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
            drop(Box::from_raw(self.feeder));
        }
    }
}
