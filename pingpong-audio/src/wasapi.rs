//! Host capture (Windows), as Sunshine does it: WASAPI loopback of a render
//! endpoint, converted by the audio engine to 48 kHz float at the channel
//! count we stream. And client playback ([`open_output`]).
//!
//! Unless the client wants sound on the host too, the session's audio goes to
//! a virtual sink (Steam Streaming Speakers, as Apollo prefers) made the
//! default device for its duration, so the PC's own speakers stay quiet; the
//! previous defaults come back when capture stops. A silent stream is kept
//! playing into the captured endpoint so loopback delivers a continuous clock
//! even when nothing else plays.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pingpong_proto::audio::{FRAME_SAMPLES, SAMPLE_RATE};
use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCommunications, eConsole, eMultimedia, eRender, ERole, IAudioCaptureClient, IAudioClient,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
    WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

/// Virtual sinks we will route the session's audio to, best first.
const VIRTUAL_SINKS: &[&str] = &["Steam Streaming Speakers"];
/// 100 ns units.
const BUFFER_HNS: i64 = 200_000;
const SILENCE_BUFFER_HNS: i64 = 1_000_000;

mod policy {
    #![allow(non_snake_case)]
    use windows::core::{HRESULT, PCWSTR};
    use windows::Win32::Media::Audio::{ERole, WAVEFORMATEX};

    // The undocumented interface Windows' own sound settings use to change the
    // default device (Sunshine uses it the same way). Only the vtable order matters.
    #[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    pub unsafe trait IPolicyConfig: windows::core::IUnknown {
        fn GetMixFormat(&self, device: PCWSTR, format: *mut *mut WAVEFORMATEX) -> HRESULT;
        fn GetDeviceFormat(
            &self,
            device: PCWSTR,
            default: i32,
            format: *mut *mut WAVEFORMATEX,
        ) -> HRESULT;
        fn ResetDeviceFormat(&self, device: PCWSTR) -> HRESULT;
        fn SetDeviceFormat(
            &self,
            device: PCWSTR,
            endpoint: *const WAVEFORMATEX,
            mix: *const WAVEFORMATEX,
        ) -> HRESULT;
        fn GetProcessingPeriod(
            &self,
            device: PCWSTR,
            default: i32,
            period: *mut i64,
            min: *mut i64,
        ) -> HRESULT;
        fn SetProcessingPeriod(&self, device: PCWSTR, period: *const i64) -> HRESULT;
        fn GetShareMode(&self, device: PCWSTR, mode: *mut std::ffi::c_void) -> HRESULT;
        fn SetShareMode(&self, device: PCWSTR, mode: *const std::ffi::c_void) -> HRESULT;
        fn GetPropertyValue(
            &self,
            device: PCWSTR,
            fx: i32,
            key: *const std::ffi::c_void,
            value: *mut std::ffi::c_void,
        ) -> HRESULT;
        fn SetPropertyValue(
            &self,
            device: PCWSTR,
            fx: i32,
            key: *const std::ffi::c_void,
            value: *const std::ffi::c_void,
        ) -> HRESULT;
        fn SetDefaultEndpoint(&self, device: PCWSTR, role: ERole) -> HRESULT;
        fn SetEndpointVisibility(&self, device: PCWSTR, visible: i32) -> HRESULT;
    }

    // The macro's methods are private to this module.
    pub fn set_default_endpoint(p: &IPolicyConfig, device: PCWSTR, role: ERole) -> HRESULT {
        unsafe { p.SetDefaultEndpoint(device, role) }
    }

    pub fn set_device_format(
        p: &IPolicyConfig,
        device: PCWSTR,
        endpoint: *const WAVEFORMATEX,
        mix: *const WAVEFORMATEX,
    ) -> HRESULT {
        unsafe { p.SetDeviceFormat(device, endpoint, mix) }
    }
}
use policy::IPolicyConfig;

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub channels: u8,
    /// Route the session's audio to a virtual sink (host speakers silent).
    /// Off: capture whatever the host's default device plays, and it keeps
    /// playing there too.
    pub virtual_sink: bool,
    /// Where the previous default devices are written while the virtual sink
    /// stands in for them, so a crash does not leave the PC silent (see
    /// [`restore_stale`]).
    pub state_path: Option<std::path::PathBuf>,
}

struct Com;
impl Com {
    fn init() -> Com {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        Com
    }
}
impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

fn take_pwstr(p: PWSTR) -> String {
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn device_id(d: &IMMDevice) -> windows::core::Result<String> {
    unsafe { d.GetId().map(take_pwstr) }
}

fn friendly_name(d: &IMMDevice) -> String {
    unsafe {
        d.OpenPropertyStore(STGM_READ)
            .and_then(|store| store.GetValue(&PKEY_Device_FriendlyName))
            .and_then(|v| PropVariantToStringAlloc(&v).map(take_pwstr))
            .unwrap_or_default()
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn set_defaults(previous: &[(ERole, String)]) -> Result<(), String> {
    let config: IPolicyConfig =
        unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }
            .map_err(|e| e.to_string())?;
    for (role, id) in previous {
        let w = wide(id);
        let hr = policy::set_default_endpoint(&config, PCWSTR(w.as_ptr()), *role);
        if hr.is_err() {
            return Err(format!("SetDefaultEndpoint: {hr:?}"));
        }
    }
    Ok(())
}

/// Give the virtual sink `channels` speakers, as Windows' sound settings
/// would ("Configure"): what plays into it is then mixed for them -- a game
/// renders 7.1 only to a 7.1 device. Only the virtual sink, which exists for
/// streaming, is changed, so there is nothing to put back afterwards.
fn set_speakers(device_id: &str, channels: u8) -> Result<(), String> {
    use windows::Win32::Media::KernelStreaming::KSDATAFORMAT_SUBTYPE_PCM;
    let config: IPolicyConfig =
        unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }
            .map_err(|e| e.to_string())?;
    let w = wide(device_id);
    let mix = format(channels);
    let pcm = |bits: u16, valid: u16| {
        let mut f = format(channels);
        f.Format.wBitsPerSample = bits;
        f.Format.nBlockAlign = bits / 8 * channels as u16;
        f.Format.nAvgBytesPerSec = SAMPLE_RATE * f.Format.nBlockAlign as u32;
        f.Samples.wValidBitsPerSample = valid;
        f.SubFormat = KSDATAFORMAT_SUBTYPE_PCM;
        f
    };
    // The device (endpoint) format as the sound settings offer it, best
    // first; the mix is always float.
    let mut last = windows::core::HRESULT(0);
    for endpoint in [pcm(32, 24), pcm(16, 16), mix] {
        let hr = policy::set_device_format(
            &config,
            PCWSTR(w.as_ptr()),
            &endpoint as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX,
            &mix as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX,
        );
        if hr.is_ok() {
            return Ok(());
        }
        last = hr;
    }
    Err(format!("SetDeviceFormat: {last:?}"))
}

fn write_state(path: &std::path::Path, previous: &[(ERole, String)]) {
    let text: String = previous
        .iter()
        .map(|(role, id)| format!("{}\t{id}\n", role.0))
        .collect();
    if let Err(e) = std::fs::write(path, text) {
        tracing::warn!(error = %e, "could not record the default audio devices");
    }
}

fn read_state(path: &std::path::Path) -> Option<Vec<(ERole, String)>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter_map(|l| l.split_once('\t'))
            .filter_map(|(r, id)| Some((ERole(r.parse().ok()?), id.to_string())))
            .collect(),
    )
}

/// A previous run died with the virtual sink standing in as the default
/// device: put the real defaults back.
pub fn restore_stale(path: &std::path::Path) {
    let Some(previous) = read_state(path) else {
        return;
    };
    let _com = Com::init();
    match set_defaults(&previous) {
        Ok(()) => tracing::info!("default audio device restored after an unclean exit"),
        Err(e) => tracing::warn!(error = %e, "could not restore the default audio device"),
    }
    let _ = std::fs::remove_file(path);
}

/// Puts the previous default render devices back when dropped.
struct DefaultSwitch {
    previous: Vec<(ERole, String)>,
    state_path: Option<std::path::PathBuf>,
}

impl DefaultSwitch {
    fn to(
        enumerator: &IMMDeviceEnumerator,
        target_id: &str,
        state_path: Option<std::path::PathBuf>,
    ) -> Result<DefaultSwitch, String> {
        let roles = [eConsole, eMultimedia, eCommunications];
        let previous: Vec<(ERole, String)> = roles
            .iter()
            .filter_map(|&role| {
                let id = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, role) }
                    .and_then(|d| device_id(&d))
                    .ok()?;
                Some((role, id))
            })
            .collect();
        if previous.iter().all(|(_, id)| id == target_id) {
            // Already the default everywhere: nothing to switch or restore.
            return Ok(DefaultSwitch {
                previous: Vec::new(),
                state_path: None,
            });
        }
        if let Some(p) = &state_path {
            write_state(p, &previous);
        }
        let targets: Vec<(ERole, String)> =
            roles.iter().map(|&r| (r, target_id.to_string())).collect();
        if let Err(e) = set_defaults(&targets) {
            let _ = set_defaults(&previous);
            if let Some(p) = &state_path {
                let _ = std::fs::remove_file(p);
            }
            return Err(e);
        }
        Ok(DefaultSwitch {
            previous,
            state_path,
        })
    }
}

impl Drop for DefaultSwitch {
    fn drop(&mut self) {
        if self.previous.is_empty() {
            return;
        }
        match set_defaults(&self.previous) {
            Ok(()) => tracing::info!("default audio device restored"),
            Err(e) => tracing::warn!(error = %e, "could not restore the default audio device"),
        }
        if let Some(p) = &self.state_path {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn find_virtual_sink(enumerator: &IMMDeviceEnumerator) -> Option<(IMMDevice, String)> {
    let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }.ok()?;
    let count = unsafe { devices.GetCount() }.ok()?;
    let all: Vec<(IMMDevice, String)> = (0..count)
        .filter_map(|i| unsafe { devices.Item(i) }.ok())
        .map(|d| {
            let name = friendly_name(&d);
            (d, name)
        })
        .collect();
    VIRTUAL_SINKS
        .iter()
        .find_map(|want| all.iter().find(|(_, name)| name.contains(want)).cloned())
}

fn format(channels: u8) -> WAVEFORMATEXTENSIBLE {
    let block = 4 * channels as u16;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels as u16,
            nSamplesPerSec: SAMPLE_RATE,
            nAvgBytesPerSec: SAMPLE_RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: (std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
                - std::mem::size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: match channels {
            1 => 0x4,
            2 => 0x3,
            6 => 0x3F,
            8 => 0x63F,
            _ => 0,
        },
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// Keeps silence playing into the endpoint so loopback never stalls.
struct SilenceKeeper {
    client: IAudioClient,
    render: IAudioRenderClient,
    frames: u32,
}

impl SilenceKeeper {
    fn start(
        device: &IMMDevice,
        fmt: &WAVEFORMATEXTENSIBLE,
    ) -> windows::core::Result<SilenceKeeper> {
        unsafe {
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                SILENCE_BUFFER_HNS,
                0,
                &fmt.Format,
                None,
            )?;
            let frames = client.GetBufferSize()?;
            let render: IAudioRenderClient = client.GetService()?;
            let keeper = SilenceKeeper {
                client,
                render,
                frames,
            };
            keeper.top_up();
            keeper.client.Start()?;
            Ok(keeper)
        }
    }

    fn top_up(&self) {
        unsafe {
            let Ok(padding) = self.client.GetCurrentPadding() else {
                return;
            };
            let free = self.frames.saturating_sub(padding);
            if free > 0 && self.render.GetBuffer(free).is_ok() {
                let _ = self
                    .render
                    .ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32);
            }
        }
    }
}

impl Drop for SilenceKeeper {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

struct Event(HANDLE);
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Capture until `stop`, calling `on_frame` with each 5 ms block of
/// interleaved 48 kHz float samples. Survives device changes by reopening.
pub fn run(
    cfg: &CaptureConfig,
    stop: &AtomicBool,
    mut on_frame: impl FnMut(&[f32]),
) -> Result<(), String> {
    let _com = Com::init();
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| e.to_string())?;

    let mut _switch = None;
    let mut source: Option<IMMDevice> = None;
    if cfg.virtual_sink {
        match find_virtual_sink(&enumerator) {
            Some((device, name)) => {
                let id = device_id(&device).map_err(|e| e.to_string())?;
                match set_speakers(&id, cfg.channels) {
                    Ok(()) => tracing::info!(
                        sink = name,
                        channels = cfg.channels,
                        "virtual sink set to the stream's speakers"
                    ),
                    Err(e) => {
                        tracing::warn!(sink = name, channels = cfg.channels, error = %e, "could not set the virtual sink's speakers")
                    }
                }
                match DefaultSwitch::to(&enumerator, &id, cfg.state_path.clone()) {
                    Ok(s) => {
                        tracing::info!(sink = name, "session audio routed to the virtual sink");
                        _switch = Some(s);
                        source = Some(device);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not make the virtual sink the default")
                    }
                }
            }
            None => {
                tracing::warn!("no virtual audio sink found; capturing the host's default device")
            }
        }
    }

    let mut backoff = Duration::from_millis(200);
    while !stop.load(Ordering::Relaxed) {
        let device = match &source {
            Some(d) => d.clone(),
            None => match unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) } {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "no audio output device");
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            },
        };
        let started = Instant::now();
        match capture(&device, cfg.channels, stop, &mut on_frame) {
            Ok(()) => break,
            Err(e) => {
                tracing::warn!(device = friendly_name(&device), error = %e, "audio capture interrupted; reopening");
                if started.elapsed() > Duration::from_secs(5) {
                    backoff = Duration::from_millis(200);
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        }
    }
    Ok(())
}

fn capture(
    device: &IMMDevice,
    channels: u8,
    stop: &AtomicBool,
    on_frame: &mut impl FnMut(&[f32]),
) -> windows::core::Result<()> {
    let ch = channels as usize;
    let fmt = format(channels);
    let silence = SilenceKeeper::start(device, &fmt);
    if let Err(e) = &silence {
        tracing::debug!(error = %e, "silence keeper unavailable");
    }
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let base = AUDCLNT_STREAMFLAGS_LOOPBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let event = Event(CreateEventW(None, false, false, None)?);
        let evented = client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                base | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_HNS,
                0,
                &fmt.Format,
                None,
            )
            .is_ok();
        let client = if evented {
            client.SetEventHandle(event.0)?;
            client
        } else {
            // Older Windows: loopback without event callbacks; poll.
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                base,
                BUFFER_HNS,
                0,
                &fmt.Format,
                None,
            )?;
            client
        };
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        tracing::info!(
            device = friendly_name(device),
            channels,
            evented,
            "audio capture started"
        );

        let frame_len = FRAME_SAMPLES * ch;
        let mut pending: Vec<f32> = Vec::with_capacity(frame_len * 8);
        let mut last_top_up = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            if evented {
                let _ = WaitForSingleObject(event.0, 10) == WAIT_OBJECT_0;
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
            loop {
                let n = capture.GetNextPacketSize()?;
                if n == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let count = frames as usize * ch;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    pending.resize(pending.len() + count, 0.0);
                } else {
                    pending
                        .extend_from_slice(std::slice::from_raw_parts(data as *const f32, count));
                }
                capture.ReleaseBuffer(frames)?;
            }
            let mut offset = 0;
            while pending.len() - offset >= frame_len {
                on_frame(&pending[offset..offset + frame_len]);
                offset += frame_len;
            }
            pending.drain(..offset);
            if last_top_up.elapsed() >= Duration::from_millis(20) {
                last_top_up = Instant::now();
                if let Ok(s) = &silence {
                    s.top_up();
                }
            }
        }
        let _ = client.Stop();
    }
    Ok(())
}

/// Friendly names of the active render devices (diagnostics).
pub fn outputs() -> Vec<String> {
    let _com = Com::init();
    let Ok(enumerator) = (unsafe {
        CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
    }) else {
        return Vec::new();
    };
    let Ok(devices) = (unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) })
    else {
        return Vec::new();
    };
    let count = unsafe { devices.GetCount() }.unwrap_or(0);
    (0..count)
        .filter_map(|i| unsafe { devices.Item(i) }.ok())
        .map(|d| friendly_name(&d))
        .collect()
}

// ---------------------------------------------------------------------------
// Client playback (Ping on Windows): the default output device in shared
// mode, the audio engine converting our 48 kHz float to its own format, fed
// from the player's ring on the engine's event. Follows the default device
// when the user switches (headphones plugged in), as Moonlight's SDL does.

/// 100 ns units: the engine's period, about. The player's ring is the
/// jitter buffer; this only has to cover one wake-up.
const PLAYBACK_BUFFER_HNS: i64 = 100_000;

struct Playback {
    stop: std::sync::Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl crate::player::Output for Playback {}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Play `feeder` on the default output device until the returned handle is
/// dropped.
pub fn open_output(
    feeder: crate::player::Feeder,
) -> Result<Box<dyn crate::player::Output>, String> {
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("ping-wasapi".into())
            .spawn(move || playback_loop(feeder, &stop, ready_tx))
            .map_err(|e| e.to_string())?
    };
    let playback = Playback {
        stop,
        thread: Some(thread),
    };
    match ready_rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(())) => Ok(Box::new(playback)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the audio device did not start".into()),
    }
}

fn playback_loop(
    mut feeder: crate::player::Feeder,
    stop: &AtomicBool,
    ready: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let _com = Com::init();
    // Tell the scheduler this thread feeds the audio engine.
    let mut task = 0u32;
    let _mmcss = unsafe {
        windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(
            windows::core::w!("Pro Audio"),
            &mut task,
        )
    };
    let enumerator: IMMDeviceEnumerator =
        match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
            Ok(e) => e,
            Err(e) => {
                let _ = ready.send(Err(e.to_string()));
                return;
            }
        };
    let mut ready = Some(ready);
    let mut backoff = Duration::from_millis(200);
    while !stop.load(Ordering::Relaxed) {
        let device = match unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) } {
            Ok(d) => d,
            Err(e) => {
                if let Some(r) = ready.take() {
                    let _ = r.send(Err(format!("no audio output device: {e}")));
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let id = device_id(&device).unwrap_or_default();
        let started = Instant::now();
        match play(&enumerator, &device, &id, &mut feeder, stop, &mut ready) {
            Ok(()) => {}
            Err(e) => {
                if let Some(r) = ready.take() {
                    let _ = r.send(Err(e.to_string()));
                    return;
                }
                tracing::warn!(device = friendly_name(&device), error = %e, "audio playback interrupted; reopening");
                if started.elapsed() > Duration::from_secs(5) {
                    backoff = Duration::from_millis(200);
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(2));
            }
        }
    }
}

/// Play on `device` until `stop`, or until the default device is another
/// (Ok: the caller reopens on the new one).
fn play(
    enumerator: &IMMDeviceEnumerator,
    device: &IMMDevice,
    id: &str,
    feeder: &mut crate::player::Feeder,
    stop: &AtomicBool,
    ready: &mut Option<std::sync::mpsc::Sender<Result<(), String>>>,
) -> windows::core::Result<()> {
    let channels = feeder.channels();
    let fmt = format(channels as u8);
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            PLAYBACK_BUFFER_HNS,
            0,
            &fmt.Format,
            None,
        )?;
        let event = Event(CreateEventW(None, false, false, None)?);
        client.SetEventHandle(event.0)?;
        let frames_total = client.GetBufferSize()?;
        let render: IAudioRenderClient = client.GetService()?;
        let mut scratch = vec![0f32; frames_total as usize * channels];
        // Start on silence: the first callback comes a period in.
        let data = render.GetBuffer(frames_total)?;
        std::ptr::write_bytes(data, 0, frames_total as usize * channels * 4);
        render.ReleaseBuffer(frames_total, 0)?;
        client.Start()?;
        tracing::info!(
            device = friendly_name(device),
            channels,
            buffer_frames = frames_total,
            "audio playback started"
        );
        if let Some(r) = ready.take() {
            let _ = r.send(Ok(()));
        }
        let mut checked = Instant::now();
        let result = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            let _ = WaitForSingleObject(event.0, 100);
            let padding = match client.GetCurrentPadding() {
                Ok(p) => p,
                Err(e) => break Err(e),
            };
            let free = frames_total.saturating_sub(padding);
            if free > 0 {
                let out = &mut scratch[..free as usize * channels];
                feeder.fill(out);
                let data = match render.GetBuffer(free) {
                    Ok(d) => d,
                    Err(e) => break Err(e),
                };
                std::ptr::copy_nonoverlapping(out.as_ptr() as *const u8, data, out.len() * 4);
                if let Err(e) = render.ReleaseBuffer(free, 0) {
                    break Err(e);
                }
            }
            // The user picked another output: follow it.
            if checked.elapsed() >= Duration::from_secs(1) {
                checked = Instant::now();
                let now = enumerator
                    .GetDefaultAudioEndpoint(eRender, eConsole)
                    .ok()
                    .and_then(|d| device_id(&d).ok());
                if now.is_some_and(|now| now != id) {
                    tracing::info!("default audio output changed; following it");
                    break Ok(());
                }
            }
        };
        let _ = client.Stop();
        result
    }
}
