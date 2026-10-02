//! A Mac host's sound in surround: a Core Audio process tap of an output
//! device, which hears what every app sends that device in the device's own
//! channels (its stream's format, Core Audio says), where ScreenCaptureKit
//! mixes the system's sound down to stereo whatever it is asked for (2
//! channels at 48 kHz when asked for 6 or 8, macOS 26.5). Taps are macOS
//! 14.2 and later, and need the System Audio Recording permission.
//!
//! Apps play surround only to a device that has the speakers for it: a
//! receiver on HDMI, or a loopback device such as BlackHole 16ch, which is
//! what Sunshine uses on a Mac. [`Route`] does for such a device what the
//! Windows host does for Steam Streaming Speakers: makes it the default
//! output for the session, its speakers set to the session's layout, and
//! puts both back after.
//!
//! The tap is read through a private aggregate device (the output device
//! for its clock, the tap as its input), by an IO proc on Core Audio's
//! real-time thread, which hands each buffer to the caller's sink as it is.
//! The tap can mute what it hears ([`Tap::start`]'s `mute`), as the Windows
//! host keeps the session's sound off the host's speakers unless the client
//! asks for it there too.

use std::ffi::{c_void, CStr};
use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::AllocAnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertyPreferredChannelLayout,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyTransportType,
    kAudioDeviceTransportTypeAggregate, kAudioDeviceTransportTypeVirtual,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
    kAudioHardwarePropertyTranslatePIDToProcessObject, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput,
    kAudioObjectSystemObject, kAudioSubDeviceUIDKey, kAudioSubTapDriftCompensationKey,
    kAudioSubTapUIDKey, kAudioTapPropertyFormat, AudioDeviceCreateIOProcID,
    AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap,
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectSetPropertyData, CATapDescription, CATapMuteBehavior,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};
use objc2_core_foundation::{CFDictionary, CFString};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};

use crate::channels;

/// `kAudioFormatLinearPCM`, `kAudioFormatFlagIsFloat`.
const LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const FLOAT: u32 = 1;

/// An output device.
#[derive(Debug, Clone)]
pub struct OutputDevice {
    pub id: AudioObjectID,
    pub uid: String,
    pub name: String,
    /// Its output channels, all streams together.
    pub channels: usize,
    /// A loopback driver (BlackHole, Loopback, Soundflower): nothing is
    /// heard from it, so the session's sound is not missed on the host.
    pub is_virtual: bool,
    pub rate: f64,
}

impl OutputDevice {
    /// Its channels' speakers, from its speaker configuration.
    pub fn labels(&self) -> Vec<u32> {
        let layout = get_bytes(
            self.id,
            kAudioDevicePropertyPreferredChannelLayout,
            kAudioObjectPropertyScopeOutput,
        )
        .unwrap_or_default();
        channels::labels_from_layout(&layout, self.channels)
    }

    /// The most channels a stream can carry from it (2, 6 or 8).
    pub fn surround_channels(&self) -> u8 {
        channels::surround_channels(&self.labels())
    }
}

fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// A property of fixed size: plain data (numbers, ids, a CFStringRef, an
/// AudioStreamBasicDescription).
fn get<T: Copy>(object: AudioObjectID, selector: u32, scope: u32) -> Option<T> {
    let addr = address(selector, scope);
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: `value` has room for a T and `size` says so; Core Audio
    // writes at most `size` bytes into it.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    // SAFETY: the T is plain data, all of it written (or zero) on success.
    (status == 0 && size as usize == std::mem::size_of::<T>())
        .then(|| unsafe { value.assume_init() })
}

/// A property of variable size, as bytes.
fn get_bytes(object: AudioObjectID, selector: u32, scope: u32) -> Option<Vec<u8>> {
    let addr = address(selector, scope);
    let mut size = 0u32;
    // SAFETY: the address is valid for the call; `size` receives a u32.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 || size == 0 {
        return None;
    }
    // Words, so the structures read from it are aligned.
    let mut words = vec![0u32; (size as usize).div_ceil(4)];
    // SAFETY: `words` holds at least `size` bytes.
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(words.as_mut_ptr())?.cast(),
        )
    };
    if status != 0 {
        return None;
    }
    let mut bytes: Vec<u8> = words.iter().flat_map(|w| w.to_ne_bytes()).collect();
    bytes.truncate(size as usize);
    Some(bytes)
}

fn set<T>(object: AudioObjectID, selector: u32, scope: u32, value: &T) -> Result<(), i32> {
    set_raw(
        object,
        selector,
        scope,
        NonNull::from(value).cast(),
        std::mem::size_of::<T>(),
    )
}

fn set_raw(
    object: AudioObjectID,
    selector: u32,
    scope: u32,
    data: NonNull<c_void>,
    len: usize,
) -> Result<(), i32> {
    let addr = address(selector, scope);
    // SAFETY: `data` points at `len` readable bytes.
    let status = unsafe {
        AudioObjectSetPropertyData(
            object,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            len as u32,
            data,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(status)
    }
}

/// A CFString property, as a String (Core Audio hands over a CFStringRef:
/// read as the pointer it is).
fn get_string(object: AudioObjectID, selector: u32) -> Option<String> {
    let s: *const CFString = get(object, selector, kAudioObjectPropertyScopeGlobal)
        .map(|p: usize| p as *const CFString)?;
    // SAFETY: Core Audio hands over a retained CFString (the caller
    // releases it): taken over, then released when dropped.
    let s = unsafe { objc2_core_foundation::CFRetained::from_raw(NonNull::new(s.cast_mut())?) };
    Some(s.to_string())
}

fn output_channels(id: AudioObjectID) -> usize {
    let Some(bytes) = get_bytes(
        id,
        kAudioDevicePropertyStreamConfiguration,
        kAudioObjectPropertyScopeOutput,
    ) else {
        return 0;
    };
    // An AudioBufferList: a count, then (aligned to 8) that many buffers of
    // { channels: u32, bytes: u32, data: pointer }.
    let word = |i: usize| -> usize {
        bytes
            .get(i * 4..i * 4 + 4)
            .map_or(0, |b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as usize)
    };
    let buffers = word(0);
    let first = std::mem::offset_of!(AudioBufferList, mBuffers) / 4;
    let stride = std::mem::size_of::<AudioBuffer>() / 4;
    (0..buffers).map(|b| word(first + b * stride)).sum()
}

fn device(id: AudioObjectID) -> Option<OutputDevice> {
    let channels = output_channels(id);
    if channels == 0 {
        return None;
    }
    let transport: u32 = get(
        id,
        kAudioDevicePropertyTransportType,
        kAudioObjectPropertyScopeGlobal,
    )
    .unwrap_or(0);
    Some(OutputDevice {
        id,
        uid: get_string(id, kAudioDevicePropertyDeviceUID)?,
        name: get_string(id, kAudioObjectPropertyName).unwrap_or_default(),
        channels,
        is_virtual: transport == kAudioDeviceTransportTypeVirtual,
        rate: get(
            id,
            kAudioDevicePropertyNominalSampleRate,
            kAudioObjectPropertyScopeGlobal,
        )
        .unwrap_or(0.0),
    })
}

/// Every device that plays sound, aggregates (ours among them) aside.
pub fn output_devices() -> Vec<OutputDevice> {
    let system = kAudioObjectSystemObject as AudioObjectID;
    let Some(bytes) = get_bytes(
        system,
        kAudioHardwarePropertyDevices,
        kAudioObjectPropertyScopeGlobal,
    ) else {
        return Vec::new();
    };
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_ne_bytes(*b))
        .filter(|&id| {
            get::<u32>(
                id,
                kAudioDevicePropertyTransportType,
                kAudioObjectPropertyScopeGlobal,
            ) != Some(kAudioDeviceTransportTypeAggregate)
        })
        .filter_map(device)
        .collect()
}

/// The device the system plays to.
pub fn default_output() -> Option<OutputDevice> {
    let id: AudioObjectID = get(
        kAudioObjectSystemObject as AudioObjectID,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )?;
    device(id)
}

/// The session's sound sent to a device for as long as this lives: the
/// default output, its speakers set to the session's layout. Both are put
/// back when dropped.
pub struct Route {
    previous: Option<AudioObjectID>,
    layout: Option<(AudioObjectID, Vec<u8>)>,
}

impl Route {
    /// Send the system's sound to `to`, its speakers set to `channels` (6
    /// or 8): what is not so already.
    pub fn to(to: &OutputDevice, channels: u8) -> Route {
        let mut route = Route {
            previous: None,
            layout: None,
        };
        let system = kAudioObjectSystemObject as AudioObjectID;
        let current: Option<AudioObjectID> = get(
            system,
            kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal,
        );
        if current != Some(to.id) {
            match set(
                system,
                kAudioHardwarePropertyDefaultOutputDevice,
                kAudioObjectPropertyScopeGlobal,
                &to.id,
            ) {
                Ok(()) => {
                    tracing::info!(device = to.name, "the session's sound goes to it");
                    route.previous = current;
                }
                Err(status) => {
                    tracing::warn!(
                        device = to.name,
                        status,
                        "could not make it the default output"
                    )
                }
            }
        }
        // Its speakers exactly as the session's layout (a loopback device's
        // own numbering, such as BlackHole's 1..16, has no rear pair).
        if !channels::is_layout(&to.labels(), channels) {
            let old = get_bytes(
                to.id,
                kAudioDevicePropertyPreferredChannelLayout,
                kAudioObjectPropertyScopeOutput,
            );
            let mut layout = channels::surround_layout(channels, to.channels);
            // Core Audio reads it as an AudioChannelLayout: aligned words.
            let len = layout.len();
            layout.resize(len.div_ceil(4) * 4, 0);
            let words: Vec<u32> = layout
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| u32::from_ne_bytes(*b))
                .collect();
            match NonNull::new(words.as_ptr().cast_mut()).map(|p| {
                set_raw(
                    to.id,
                    kAudioDevicePropertyPreferredChannelLayout,
                    kAudioObjectPropertyScopeOutput,
                    p.cast(),
                    len,
                )
            }) {
                Some(Ok(())) => {
                    tracing::info!(
                        device = to.name,
                        channels,
                        "its speakers set for the session"
                    );
                    route.layout = old.map(|o| (to.id, o));
                }
                _ => tracing::warn!(
                    device = to.name,
                    channels,
                    "could not set its speakers; set them in Audio MIDI Setup (Configure Speakers)"
                ),
            }
        }
        route
    }
}

impl Drop for Route {
    fn drop(&mut self) {
        if let Some((id, old)) = self.layout.take() {
            let words: Vec<u32> = old
                .chunks(4)
                .map(|b| {
                    let mut w = [0u8; 4];
                    w[..b.len()].copy_from_slice(b);
                    u32::from_ne_bytes(w)
                })
                .collect();
            if let Some(p) = NonNull::new(words.as_ptr().cast_mut()) {
                let _ = set_raw(
                    id,
                    kAudioDevicePropertyPreferredChannelLayout,
                    kAudioObjectPropertyScopeOutput,
                    p.cast(),
                    old.len(),
                );
            }
        }
        if let Some(previous) = self.previous.take() {
            match set(
                kAudioObjectSystemObject as AudioObjectID,
                kAudioHardwarePropertyDefaultOutputDevice,
                kAudioObjectPropertyScopeGlobal,
                &previous,
            ) {
                Ok(()) => tracing::info!("the default output is back"),
                Err(status) => tracing::warn!(status, "the default output could not be put back"),
            }
        }
    }
}

/// Core Audio's object for process `pid`, for a tap to name; `None` while
/// the process has not used Core Audio.
fn process_object(pid: i32) -> Option<AudioObjectID> {
    let addr = address(
        kAudioHardwarePropertyTranslatePIDToProcessObject,
        kAudioObjectPropertyScopeGlobal,
    );
    let mut object: AudioObjectID = 0;
    let mut size = std::mem::size_of::<AudioObjectID>() as u32;
    // SAFETY: the qualifier is the pid (an i32), the answer an object id.
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&addr),
            std::mem::size_of::<i32>() as u32,
            (&pid as *const i32).cast(),
            NonNull::from(&mut size),
            NonNull::from(&mut object).cast(),
        )
    };
    (status == 0 && object != 0).then_some(object)
}

/// What a tap hands its sink: interleaved float frames of `channels`.
pub type TapSink = Box<dyn FnMut(&[f32]) + Send>;

/// What the IO proc needs (owned by the tap, freed after the proc is gone).
struct Reader {
    sink: TapSink,
    channels: usize,
    /// Non-interleaved buffers, interleaved (grown once, then reused).
    scratch: Vec<f32>,
}

/// A running capture of an output device.
pub struct Tap {
    tap: AudioObjectID,
    aggregate: AudioObjectID,
    proc_id: AudioDeviceIOProcID,
    reader: *mut Reader,
    /// Channels a frame, and frames a second, of what the sink gets.
    pub channels: usize,
    pub rate: f64,
}

// SAFETY: the Core Audio objects are ids, usable from any thread; `reader`
// is only touched by the IO proc until the proc is destroyed, then freed.
unsafe impl Send for Tap {}

fn key(k: &CStr) -> Retained<NSString> {
    NSString::from_str(k.to_str().unwrap_or_default())
}

fn dictionary(entries: &[(&CStr, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<Retained<NSString>> = entries.iter().map(|(k, _)| key(k)).collect();
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<&AnyObject> = entries.iter().map(|(_, v)| *v).collect();
    NSDictionary::from_slices(&keys, &values)
}

impl Tap {
    /// Hear what apps play to `device`, in its channels: every app's, or
    /// only those of the processes `only` names (a loopback test's signal,
    /// without its own client's playback). `mute` keeps it off the device
    /// while the tap is read.
    pub fn start(
        device: &OutputDevice,
        mute: bool,
        only: &[i32],
        sink: TapSink,
    ) -> Result<Tap, String> {
        if AnyClass::get(c"CATapDescription").is_none() {
            return Err("capturing sound in surround needs macOS 14.2 or later".into());
        }
        let processes: Vec<Retained<NSNumber>> = only
            .iter()
            .filter_map(|&pid| process_object(pid))
            .map(NSNumber::new_u32)
            .collect();
        if processes.len() != only.len() {
            return Err(format!("not all of {only:?} play sound"));
        }
        let processes: Vec<&NSNumber> = processes.iter().map(|n| &**n).collect();
        let processes = NSArray::from_slice(&processes);
        let uid = NSString::from_str(&device.uid);
        // SAFETY: CATapDescription exists (checked above); every argument
        // is a valid object.
        let description = unsafe {
            let d = if only.is_empty() {
                CATapDescription::initExcludingProcesses_andDeviceUID_withStream(
                    CATapDescription::alloc(),
                    &processes,
                    &uid,
                    0,
                )
            } else {
                CATapDescription::initWithProcesses_andDeviceUID_withStream(
                    CATapDescription::alloc(),
                    &processes,
                    &uid,
                    0,
                )
            };
            d.setName(&NSString::from_str("Pong"));
            d.setPrivate(true);
            d.setMuteBehavior(if mute {
                CATapMuteBehavior::MutedWhenTapped
            } else {
                CATapMuteBehavior::Unmuted
            });
            d
        };
        let mut tap: AudioObjectID = 0;
        // SAFETY: a valid description and somewhere to put the id.
        let status = unsafe { AudioHardwareCreateProcessTap(Some(&description), &mut tap) };
        if status != 0 || tap == 0 {
            return Err(format!("could not tap {} ({status})", device.name));
        }
        let mut this = Tap {
            tap,
            aggregate: 0,
            proc_id: None,
            reader: std::ptr::null_mut(),
            channels: 0,
            rate: 0.0,
        };
        let format: AudioStreamBasicDescription = get(
            tap,
            kAudioTapPropertyFormat,
            kAudioObjectPropertyScopeGlobal,
        )
        .ok_or("the tap has no format")?;
        if format.mFormatID != LINEAR_PCM
            || format.mFormatFlags & FLOAT == 0
            || format.mBitsPerChannel != 32
        {
            return Err(format!(
                "the tap's format is not 32-bit float (flags {:#x})",
                format.mFormatFlags
            ));
        }
        this.channels = format.mChannelsPerFrame as usize;
        this.rate = format.mSampleRate;

        // SAFETY: the description lives; UUID is a plain getter.
        let tap_uid = unsafe { description.UUID().UUIDString() }.to_string();
        let yes = NSNumber::new_bool(true);
        let no = NSNumber::new_bool(false);
        let device_uid = NSString::from_str(&device.uid);
        let sub_device = dictionary(&[(kAudioSubDeviceUIDKey, &device_uid)]);
        let tap_uid_ns = NSString::from_str(&tap_uid);
        let sub_tap = dictionary(&[
            (kAudioSubTapUIDKey, &tap_uid_ns),
            (kAudioSubTapDriftCompensationKey, &yes),
        ]);
        let name = NSString::from_str("Pong capture");
        let uid = NSString::from_str(&format!("dev.pingpong.Pong.tap.{}", std::process::id()));
        let sub_devices = NSArray::from_slice(&[&*sub_device]);
        let sub_taps = NSArray::from_slice(&[&*sub_tap]);
        let aggregate = dictionary(&[
            (kAudioAggregateDeviceNameKey, &name),
            (kAudioAggregateDeviceUIDKey, &uid),
            (kAudioAggregateDeviceMainSubDeviceKey, &device_uid),
            (kAudioAggregateDeviceIsPrivateKey, &yes),
            (kAudioAggregateDeviceIsStackedKey, &no),
            (kAudioAggregateDeviceTapAutoStartKey, &yes),
            (kAudioAggregateDeviceSubDeviceListKey, &sub_devices),
            (kAudioAggregateDeviceTapListKey, &sub_taps),
        ]);
        // SAFETY: NSDictionary is toll-free bridged to CFDictionary.
        let cf: &CFDictionary = unsafe { &*(Retained::as_ptr(&aggregate) as *const CFDictionary) };
        let mut id: AudioObjectID = 0;
        // SAFETY: a valid description and somewhere to put the id.
        let status = unsafe { AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut id)) };
        if status != 0 || id == 0 {
            return Err(format!("could not make the capture device ({status})"));
        }
        this.aggregate = id;
        // Buffers of about 5 ms, the stream's packet (best effort: the
        // device may keep its own).
        let frames = (this.rate * 0.005).round().max(64.0) as u32;
        let _ = set(
            id,
            kAudioDevicePropertyBufferFrameSize,
            kAudioObjectPropertyScopeGlobal,
            &frames,
        );

        this.reader = Box::into_raw(Box::new(Reader {
            sink,
            channels: this.channels,
            scratch: Vec::with_capacity(frames as usize * 4 * this.channels),
        }));
        let mut proc_id: AudioDeviceIOProcID = None;
        // SAFETY: `reader` lives until after the proc is destroyed (Drop).
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                id,
                Some(read_tap),
                this.reader.cast(),
                NonNull::from(&mut proc_id),
            )
        };
        if status != 0 || proc_id.is_none() {
            return Err(format!("could not read the capture device ({status})"));
        }
        this.proc_id = proc_id;
        // SAFETY: the device and proc were made above.
        let status = unsafe { AudioDeviceStart(id, proc_id) };
        if status != 0 {
            return Err(format!("could not start the capture ({status})"));
        }
        tracing::info!(
            device = device.name,
            channels = this.channels,
            rate = this.rate,
            muted = mute,
            "tapping the output"
        );
        Ok(this)
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // SAFETY: each id is ours and destroyed once; the proc is stopped
        // and destroyed before the reader it uses is freed.
        unsafe {
            if self.aggregate != 0 {
                if self.proc_id.is_some() {
                    AudioDeviceStop(self.aggregate, self.proc_id);
                    AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
                }
                AudioHardwareDestroyAggregateDevice(self.aggregate);
            }
            if !self.reader.is_null() {
                drop(Box::from_raw(self.reader));
            }
            AudioHardwareDestroyProcessTap(self.tap);
        }
    }
}

/// The IO proc, on Core Audio's real-time thread: the tap's buffers to the
/// sink, without allocating or waiting. The aggregate's input is the output
/// device's own input streams (a loopback driver has some), then the tap's:
/// the tap is the last buffers.
unsafe extern "C-unwind" fn read_tap(
    _device: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    // SAFETY: `client` is the Reader made for this proc, alive until the
    // proc is destroyed; `input` is Core Audio's buffer list for this cycle,
    // with `mNumberBuffers` buffers laid out after its count.
    unsafe {
        let Some(reader) = client.cast::<Reader>().as_mut() else {
            return 0;
        };
        let list = input.as_ptr();
        let count = (*list).mNumberBuffers as usize;
        let buffers = std::slice::from_raw_parts((*list).mBuffers.as_ptr(), count);
        let as_floats = |b: &AudioBuffer| -> &[f32] {
            if b.mData.is_null() {
                &[]
            } else {
                std::slice::from_raw_parts(b.mData.cast::<f32>(), b.mDataByteSize as usize / 4)
            }
        };
        let channels = reader.channels;
        match buffers.last() {
            // Interleaved: one buffer with every channel.
            Some(last) if last.mNumberChannels as usize == channels => {
                (reader.sink)(as_floats(last));
            }
            // One buffer a channel: the last `channels`, interleaved.
            _ if count >= channels && channels > 0 => {
                let planes = &buffers[count - channels..];
                let frames = planes.iter().map(|b| as_floats(b).len()).min().unwrap_or(0);
                reader.scratch.clear();
                for f in 0..frames {
                    for p in planes {
                        reader.scratch.push(as_floats(p)[f]);
                    }
                }
                (reader.sink)(&reader.scratch);
            }
            _ => {}
        }
    }
    0
}
