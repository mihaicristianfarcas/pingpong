//! The D3D11 device capture, conversion and NVENC all share.
//!
//! One device, one immediate context, one GPU queue: the captured texture is
//! converted and encoded without ever crossing an API or device boundary.

use windows::core::{s, Interface};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIDevice1, IDXGIFactory1,
    IDXGIOutput, DXGI_ADAPTER_DESC1,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::CaptureError;

/// A D3D11 device on the adapter that scans out one particular display.
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput,
    pub adapter_luid: LUID,
    pub vendor_id: u32,
}

// SAFETY: D3D11 devices are free-threaded; the immediate context is not, and is
// only ever used from the session's encode thread (the struct is moved there,
// never shared).
unsafe impl Send for Gpu {}

fn platform(what: &str) -> impl Fn(windows::core::Error) -> CaptureError + '_ {
    move |e| CaptureError::Platform(format!("{what}: {e}"))
}

fn utf16_name(raw: &[u16]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..end])
}

impl Gpu {
    /// Open a device on whichever adapter owns the display named `gdi_name`
    /// (e.g. `\\.\DISPLAY3`).
    ///
    /// For a virtual (IddCx) display this is the adapter the driver renders on,
    /// which DXGI lists the display's output under.
    pub fn for_output(gdi_name: &str) -> Result<Gpu, CaptureError> {
        declare_dpi_aware();
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1() }.map_err(platform("CreateDXGIFactory1"))?;

        let mut a = 0;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
            a += 1;
            let mut o = 0;
            while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
                o += 1;
                let desc = unsafe { output.GetDesc() }.map_err(platform("IDXGIOutput::GetDesc"))?;
                if utf16_name(&desc.DeviceName).eq_ignore_ascii_case(gdi_name) {
                    return Gpu::create(adapter, output);
                }
            }
        }
        Err(CaptureError::NoSuchOutput(gdi_name.to_string()))
    }

    /// Every output DXGI can see, as `(gdi_name, adapter description)`. For
    /// diagnostics: which name to pass when a display is not found.
    pub fn list_outputs() -> Vec<(String, String)> {
        let mut out = Vec::new();
        let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
            return out;
        };
        let mut a = 0;
        while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
            a += 1;
            let adapter_name = unsafe { adapter.GetDesc1() }
                .map(|d| utf16_name(&d.Description))
                .unwrap_or_default();
            let mut o = 0;
            while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
                o += 1;
                if let Ok(desc) = unsafe { output.GetDesc() } {
                    out.push((utf16_name(&desc.DeviceName), adapter_name.clone()));
                }
            }
        }
        out
    }

    fn create(adapter: IDXGIAdapter1, output: IDXGIOutput) -> Result<Gpu, CaptureError> {
        let desc: DXGI_ADAPTER_DESC1 =
            unsafe { adapter.GetDesc1() }.map_err(platform("IDXGIAdapter1::GetDesc1"))?;

        let mut device = None;
        let mut context = None;
        let levels: [D3D_FEATURE_LEVEL; 2] = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        unsafe {
            D3D11CreateDevice(
                &adapter
                    .cast::<IDXGIAdapter>()
                    .map_err(platform("IDXGIAdapter"))?,
                // UNKNOWN is mandatory when an explicit adapter is passed.
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(platform("D3D11CreateDevice"))?;
        let device: ID3D11Device =
            device.ok_or_else(|| CaptureError::Platform("no device".into()))?;
        let context = context.ok_or_else(|| CaptureError::Platform("no context".into()))?;

        let gpu = Gpu {
            device,
            context,
            adapter,
            output,
            adapter_luid: desc.AdapterLuid,
            vendor_id: desc.VendorId,
        };
        gpu.raise_priority();
        Ok(gpu)
    }

    /// Keep capture and encode on the GPU's fast path while a game saturates it.
    ///
    /// Without this, capture+encode queue behind the game's own rendering: an
    /// earlier version measured 190 ms per frame with CS2 running, against
    /// ~7 ms idle. Apollo
    /// avoids it the same way: the process's GPU scheduling class goes to
    /// REALTIME (HIGH on NVIDIA with hardware-accelerated scheduling, where
    /// realtime is known to hang the encoder), and the device asks for the top
    /// GPU thread priority. Both need admin or SYSTEM; failure is logged, not
    /// fatal.
    fn raise_priority(&self) {
        const REALTIME: i32 = 5; // D3DKMT_SCHEDULINGPRIORITYCLASS_REALTIME
        const HIGH: i32 = 4; // D3DKMT_SCHEDULINGPRIORITYCLASS_HIGH
        type SetClass = unsafe extern "system" fn(isize, i32) -> i32;

        let hags = hags_enabled();
        let class = if self.vendor_id == 0x10DE && hags {
            HIGH
        } else {
            REALTIME
        };
        unsafe {
            if let Ok(gdi32) = LoadLibraryA(s!("gdi32.dll")) {
                if let Some(f) =
                    GetProcAddress(gdi32, s!("D3DKMTSetProcessSchedulingPriorityClass"))
                {
                    let f: SetClass = std::mem::transmute(f);
                    let status = f(GetCurrentProcess().0 as isize, class);
                    if status != 0 {
                        tracing::warn!(
                            status,
                            "could not raise GPU scheduling class (needs admin)"
                        );
                    } else {
                        tracing::info!(
                            class = if class == REALTIME {
                                "realtime"
                            } else {
                                "high"
                            },
                            hags,
                            "GPU scheduling class raised"
                        );
                    }
                }
            }

            if let Ok(dxgi) = self.device.cast::<IDXGIDevice>() {
                // 0x4000001E is the absolute maximum; relative 7 is the fallback.
                if dxgi.SetGPUThreadPriority(0x4000_001E).is_err() {
                    let _ = dxgi.SetGPUThreadPriority(7);
                }
            }
            if let Ok(dxgi1) = self.device.cast::<IDXGIDevice1>() {
                let _ = dxgi1.SetMaximumFrameLatency(1);
            }
        }
    }
}

/// Per-monitor-v2 DPI awareness, which `DuplicateOutput1` requires and which
/// makes every coordinate this process reads a real pixel. Idempotent; a
/// failure means something already chose the process's awareness.
pub fn declare_dpi_aware() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
}

/// Whether hardware-accelerated GPU scheduling is on. Read from the registry
/// rather than D3DKMT so no undocumented structs are needed.
fn hags_enabled() -> bool {
    use windows::Win32::System::Registry::{RegGetValueA, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueA(
            HKEY_LOCAL_MACHINE,
            s!("SYSTEM\\CurrentControlSet\\Control\\GraphicsDrivers"),
            s!("HwSchMode"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok() && value == 2
}
