//! DXGI Desktop Duplication of one output, with the pointer drawn in.
//!
//! Duplication hands the pointer over beside the desktop image, never in it;
//! it is drawn into a copy (`overlay`), as Apollo does (`display_vram.cpp`).
//! So there are two textures: the desktop as duplicated, kept clean for the
//! pointer's next move, and the picture the encoder reads, the desktop with
//! the pointer on it. A pointer that moves over a still desktop makes a new
//! picture; one that is hidden makes none.
//!
//! For an HDR session the duplication asks for FP16 first, as Sunshine does
//! (`display_base.cpp`): an HDR desktop then comes as scRGB (linear BT.709,
//! 1.0 = 80 cd/m²), and the converter takes it to PQ. An SDR desktop still
//! comes as BGRA.

use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, GENERIC_ALL};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
    DXGI_ERROR_INVALID_CALL, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
    DF_ALLOWOTHERACCOUNTHOOK,
};

use crate::gpu::Gpu;
use crate::overlay::PointerOverlay;
use crate::pointer::{PointerImage, PointerPlace, ShapeKind};
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
    /// Our copy of the latest desktop image, without the pointer.
    desktop: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
    /// The desktop with the pointer drawn in: what the encoder reads. Stable
    /// across frames so the converter binds it once; recreated (with
    /// `desktop`) only when the desktop size changes.
    picture: Option<(ID3D11Texture2D, ID3D11RenderTargetView)>,
    width: u32,
    height: u32,
    /// Consecutive failed re-duplications, to keep a stuck desktop from
    /// spinning the thread.
    failures: u32,
    /// Where duplication last said the pointer is, and whether it shows.
    pointer: PointerPlace,
    /// Draws it; made at the first shape. `None` after it could not be made:
    /// the picture then goes without a pointer rather than without a frame.
    overlay: Option<PointerOverlay>,
    overlay_failed: bool,
    /// Where `GetFramePointerShape` writes, kept between shapes.
    shape_buffer: Vec<u8>,
    /// Duplicate in FP16 where the desktop is HDR.
    hdr: bool,
    /// SDR white over 80 cd/m²: the pointer's white on an HDR desktop.
    sdr_scale: f32,
    /// The surfaces' format: the duplicated frames'.
    format: DXGI_FORMAT,
}

impl DdaCapture {
    pub fn new(gpu: Gpu) -> DdaCapture {
        DdaCapture {
            gpu,
            dup: None,
            holding: false,
            desktop: None,
            picture: None,
            width: 0,
            height: 0,
            failures: 0,
            pointer: PointerPlace::default(),
            overlay: None,
            overlay_failed: false,
            shape_buffer: Vec::new(),
            hdr: false,
            sdr_scale: 1.0,
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
        }
    }

    /// Capture an HDR desktop as it is (FP16 scRGB), its SDR white at
    /// `sdr_white_nits`. Before the first `grab`.
    pub fn set_hdr(&mut self, hdr: bool, sdr_white_nits: u16) {
        self.hdr = hdr;
        self.sdr_scale = sdr_white_nits.max(1) as f32 / 80.0;
        self.reset();
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// The latest picture -- the desktop with the pointer drawn in -- once
    /// one has been captured.
    pub fn texture(&self) -> Option<&ID3D11Texture2D> {
        self.picture.as_ref().map(|(t, _)| t)
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
                if self.hdr {
                    o5.DuplicateOutput1(
                        &self.gpu.device,
                        0,
                        &[DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM],
                    )
                } else {
                    o5.DuplicateOutput1(&self.gpu.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
                }
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
                    format = desc.ModeDesc.Format.0,
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

    /// Wait up to `timeout_ms` for the desktop to present or the pointer to
    /// change, and make a new picture ([`DdaCapture::texture`]) if either did.
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

        let dup = self.dup.clone().expect("duplicated above");
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

        let pointer_moved = self.read_pointer(&dup, &info);
        // LastPresentTime == 0 means only the pointer changed: the desktop
        // image did not, unless there is none yet. The resource is still the
        // desktop's, and on a still desktop (the lock screen) nothing else
        // may come.
        let desktop_changed = info.LastPresentTime != 0 || self.desktop.is_none();
        if desktop_changed {
            if let Some(resource) = resource {
                self.copy_desktop(&resource)?;
            }
        }
        if self.desktop.is_none() || !(desktop_changed || pointer_moved) {
            return Ok(Grab::Timeout);
        }
        self.compose();
        Ok(Grab::Frame)
    }

    /// Take the duplicated frame into `desktop`.
    fn copy_desktop(&mut self, resource: &IDXGIResource) -> Result<(), CaptureError> {
        let frame: ID3D11Texture2D = resource
            .cast()
            .map_err(|e| CaptureError::Platform(format!("frame is not a texture: {e}")))?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: writes one descriptor we own.
        unsafe { frame.GetDesc(&mut desc) };
        if self.desktop.is_none()
            || desc.Width != self.width
            || desc.Height != self.height
            || desc.Format != self.format
        {
            self.format = desc.Format;
            self.create_surfaces(desc.Width, desc.Height)?;
        }
        let (desktop, _) = self.desktop.as_ref().expect("created above");
        // SAFETY: both textures are of the same format and size, on this
        // device.
        unsafe { self.gpu.context.CopyResource(desktop, &frame) };
        Ok(())
    }

    /// What this frame says about the pointer: a new shape, a new place.
    /// True when the picture changes for it.
    fn read_pointer(
        &mut self,
        dup: &IDXGIOutputDuplication,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> bool {
        let mut redraw = false;
        if info.PointerShapeBufferSize > 0 {
            let size = info.PointerShapeBufferSize as usize;
            if self.shape_buffer.len() < size {
                self.shape_buffer.resize(size, 0);
            }
            let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
            let mut written = 0u32;
            // SAFETY: the buffer holds at least `size` bytes, as told; the
            // out-pointers are locals.
            let got = unsafe {
                dup.GetFramePointerShape(
                    size as u32,
                    self.shape_buffer.as_mut_ptr() as *mut _,
                    &mut written,
                    &mut shape,
                )
            };
            match got {
                Ok(()) => {
                    let data = &self.shape_buffer[..(written as usize).min(size)];
                    let image = ShapeKind::from_dxgi(shape.Type).and_then(|kind| {
                        PointerImage::decode(kind, shape.Width, shape.Height, shape.Pitch, data)
                    });
                    match image {
                        Some(image) => {
                            if let Some(overlay) = self.overlay() {
                                match overlay.set_shape(&image) {
                                    Ok(()) => redraw = self.pointer.visible,
                                    Err(e) => tracing::warn!(error = %e, "pointer shape"),
                                }
                            }
                        }
                        None => tracing::debug!(
                            kind = shape.Type,
                            width = shape.Width,
                            height = shape.Height,
                            "a pointer shape that cannot be drawn"
                        ),
                    }
                }
                Err(e) => tracing::debug!(error = %e, "GetFramePointerShape"),
            }
        }
        // Zero: the pointer has not moved or changed visibility since the
        // last frame, so the last place still holds.
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            let next = PointerPlace {
                visible: p.Visible.as_bool(),
                x: p.Position.x,
                y: p.Position.y,
            };
            redraw |= self.pointer.redraw_for(next);
            self.pointer = next;
        }
        redraw
    }

    /// The overlay, made the first time it is needed.
    fn overlay(&mut self) -> Option<&mut PointerOverlay> {
        if self.overlay.is_none() && !self.overlay_failed {
            match PointerOverlay::new(&self.gpu.device, &self.gpu.context) {
                Ok(o) => self.overlay = Some(o),
                Err(e) => {
                    tracing::warn!(error = %e, "cannot draw the pointer into the picture");
                    self.overlay_failed = true;
                }
            }
        }
        self.overlay.as_mut()
    }

    /// The picture: the desktop, and the pointer on it where it shows.
    fn compose(&self) {
        let (Some((desktop, desktop_view)), Some((picture, target))) =
            (&self.desktop, &self.picture)
        else {
            return;
        };
        // SAFETY: both textures are of the same format and size, on this
        // device.
        unsafe { self.gpu.context.CopyResource(picture, desktop) };
        if let (true, Some(overlay)) = (self.pointer.visible, &self.overlay) {
            let hdr = (self.format == DXGI_FORMAT_R16G16B16A16_FLOAT).then_some(self.sdr_scale);
            overlay.draw(desktop_view, target, self.pointer.x, self.pointer.y, hdr);
        }
    }

    /// Start from a black image of `width` x `height`: the lock screen, drawn
    /// before duplication began, may present nothing until someone touches
    /// it. Grabs replace it as soon as the desktop presents.
    pub fn blank(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        self.create_surfaces(width, height)?;
        let (picture, target) = self.picture.as_ref().expect("created above");
        let (desktop, _) = self.desktop.as_ref().expect("created above");
        // SAFETY: the view is the picture's own; both textures are ours.
        unsafe {
            self.gpu
                .context
                .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0]);
            self.gpu.context.CopyResource(desktop, picture);
        }
        Ok(())
    }

    /// The desktop copy and the picture, at `width` x `height`.
    fn create_surfaces(&mut self, width: u32, height: u32) -> Result<(), CaptureError> {
        let desktop = self.create_texture(width, height)?;
        let picture = self.create_texture(width, height)?;
        let (mut view, mut target) = (None, None);
        // SAFETY: views of textures just made on this device, with the
        // bindings they were made with.
        unsafe {
            self.gpu
                .device
                .CreateShaderResourceView(&desktop, None, Some(&mut view))
                .map_err(|e| CaptureError::Platform(format!("CreateShaderResourceView: {e}")))?;
            self.gpu
                .device
                .CreateRenderTargetView(&picture, None, Some(&mut target))
                .map_err(|e| CaptureError::Platform(format!("CreateRenderTargetView: {e}")))?;
        }
        let empty = || CaptureError::Platform("a view came back empty".into());
        self.desktop = Some((desktop, view.ok_or_else(empty)?));
        self.picture = Some((picture, target.ok_or_else(empty)?));
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
            Format: self.format,
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
