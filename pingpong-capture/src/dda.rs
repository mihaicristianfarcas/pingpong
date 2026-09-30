//! DXGI Desktop Duplication of one output.

use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, GENERIC_ALL};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_INVALID_CALL, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
    DF_ALLOWOTHERACCOUNTHOOK,
};

use crate::gpu::Gpu;
use crate::{CaptureError, Grab};

/// Attach the calling thread to whatever desktop currently receives input.
///
/// This is what makes the secure desktop capturable: when a UAC prompt or the
/// lock screen comes up, input moves to `Winlogon`, duplication reports
/// `ACCESS_LOST`, and re-duplicating from a thread attached to `Winlogon`
/// captures it. Only SYSTEM can open `Winlogon`; as a normal user this fails and
/// capture waits out the prompt on the last frame.
///
/// Must be called on a thread that owns no windows or hooks, which is why the
/// capture runs on its own thread.
pub fn sync_thread_desktop() -> bool {
    unsafe {
        match OpenInputDesktop(
            DF_ALLOWOTHERACCOUNTHOOK,
            false,
            DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0),
        ) {
            Ok(desk) => {
                let ok = SetThreadDesktop(desk).is_ok();
                let _ = CloseDesktop(desk);
                ok
            }
            Err(_) => false,
        }
    }
}

pub struct DdaCapture {
    gpu: Gpu,
    dup: Option<IDXGIOutputDuplication>,
    /// A frame is held until just before the next acquire, as Microsoft's
    /// duplication sample does: DWM keeps presenting into its own surface
    /// meanwhile, and the copy below is already queued on our context.
    holding: bool,
    /// Our copy of the latest desktop image. Stable across frames so the
    /// converter binds it once; recreated only when the desktop size changes.
    texture: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    /// Consecutive failed re-duplications, to keep a stuck desktop from
    /// spinning the thread.
    failures: u32,
}

impl DdaCapture {
    pub fn new(gpu: Gpu) -> DdaCapture {
        DdaCapture {
            gpu,
            dup: None,
            holding: false,
            texture: None,
            width: 0,
            height: 0,
            failures: 0,
        }
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// The latest desktop image, once one has been captured.
    pub fn texture(&self) -> Option<&ID3D11Texture2D> {
        self.texture.as_ref()
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn duplicate(&mut self) -> Result<(), CaptureError> {
        sync_thread_desktop();
        let output = &self.gpu.output;
        // DuplicateOutput1 is the fast path, but it refuses (DXGI_ERROR_UNSUPPORTED)
        // any process that is not per-monitor-v2 DPI aware; `Gpu::for_output`
        // declares that, and the plain call is kept as a fallback.
        let v1 = match output.cast::<IDXGIOutput5>() {
            Ok(o5) => unsafe {
                o5.DuplicateOutput1(&self.gpu.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
            },
            Err(e) => Err(e),
        };
        let result = match v1 {
            Ok(dup) => Ok(dup),
            Err(e) if e.code() == E_ACCESSDENIED => Err(e),
            Err(_) => {
                let o1: IDXGIOutput1 = output
                    .cast()
                    .map_err(|e| CaptureError::Platform(format!("IDXGIOutput1: {e}")))?;
                unsafe { o1.DuplicateOutput(&self.gpu.device) }
            }
        };
        match result {
            Ok(dup) => {
                let desc = unsafe { dup.GetDesc() };
                tracing::info!(
                    width = desc.ModeDesc.Width,
                    height = desc.ModeDesc.Height,
                    refresh = desc.ModeDesc.RefreshRate.Numerator as f64
                        / desc.ModeDesc.RefreshRate.Denominator.max(1) as f64,
                    "desktop duplication started"
                );
                self.dup = Some(dup);
                self.failures = 0;
                Ok(())
            }
            Err(e) => {
                self.failures += 1;
                if e.code() == E_ACCESSDENIED {
                    Err(CaptureError::Unavailable(format!("DuplicateOutput: {e}")))
                } else {
                    Err(CaptureError::Unavailable(format!(
                        "DuplicateOutput: {e} ({:#x})",
                        e.code().0
                    )))
                }
            }
        }
    }

    fn release(&mut self) {
        if self.holding {
            if let Some(dup) = &self.dup {
                let _ = unsafe { dup.ReleaseFrame() };
            }
            self.holding = false;
        }
    }

    /// Drop the duplication; the next `grab` re-attaches to the input desktop
    /// and duplicates again.
    pub fn reset(&mut self) {
        self.release();
        self.dup = None;
    }

    /// Wait up to `timeout_ms` for the desktop to present, and copy it into
    /// [`DdaCapture::texture`] if it did.
    ///
    /// Errors are all recoverable by calling again: a lost duplication is
    /// re-established on the next call. `Unavailable` means the desktop cannot
    /// be duplicated right now; the caller keeps encoding the last frame.
    pub fn grab(&mut self, timeout_ms: u32) -> Result<Grab, CaptureError> {
        if self.dup.is_none() {
            if self.failures > 0 {
                // Back off while a desktop switch or mode change settles.
                std::thread::sleep(std::time::Duration::from_millis(
                    (self.failures as u64 * 20).min(200),
                ));
            }
            self.duplicate()?;
        }
        self.release();

        let dup = self.dup.as_ref().expect("duplicated above");
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Grab::Timeout),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST || e.code() == DXGI_ERROR_INVALID_CALL => {
                // Desktop switch (UAC, lock screen), mode change, or fullscreen
                // exclusive app. Re-duplicate on the next call.
                tracing::info!(
                    code = format!("{:#x}", e.code().0),
                    "desktop duplication lost; re-attaching"
                );
                self.dup = None;
                return Ok(Grab::Timeout);
            }
            Err(e) => {
                self.dup = None;
                return Err(CaptureError::Platform(format!("AcquireNextFrame: {e}")));
            }
        }
        self.holding = true;

        // LastPresentTime == 0 means only the pointer moved: nothing to encode,
        // unless there is no image yet. The resource is still the desktop's,
        // and on a still desktop (the lock screen) nothing else may come.
        if info.LastPresentTime == 0 && self.texture.is_some() {
            return Ok(Grab::Timeout);
        }
        let Some(resource) = resource else {
            return Ok(Grab::Timeout);
        };
        let frame: ID3D11Texture2D = resource
            .cast()
            .map_err(|e| CaptureError::Platform(format!("frame is not a texture: {e}")))?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { frame.GetDesc(&mut desc) };
        if self.texture.is_none() || desc.Width != self.width || desc.Height != self.height {
            self.texture = Some(self.create_texture(desc.Width, desc.Height)?);
            self.width = desc.Width;
            self.height = desc.Height;
        }
        unsafe {
            self.gpu
                .context
                .CopyResource(self.texture.as_ref().expect("created above"), &frame);
        }
        Ok(Grab::Frame)
    }

    /// Start from a black image of `width` x `height`: the lock screen, drawn
    /// before duplication began, may present nothing until someone touches
    /// it. Grabs replace it as soon as the desktop presents.
    pub fn blank(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        let tex = self.create_texture(width, height)?;
        unsafe {
            let mut rtv = None;
            self.gpu
                .device
                .CreateRenderTargetView(&tex, None, Some(&mut rtv))
                .map_err(|e| CaptureError::Platform(format!("CreateRenderTargetView: {e}")))?;
            if let Some(rtv) = rtv {
                self.gpu
                    .context
                    .ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            }
        }
        self.texture = Some(tex);
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn create_texture(&self, width: u32, height: u32) -> Result<ID3D11Texture2D, CaptureError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { self.gpu.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .map_err(|e| CaptureError::Platform(format!("CreateTexture2D: {e}")))?;
        tex.ok_or_else(|| CaptureError::Platform("CreateTexture2D returned nothing".into()))
    }
}

impl Drop for DdaCapture {
    fn drop(&mut self) {
        self.release();
    }
}
