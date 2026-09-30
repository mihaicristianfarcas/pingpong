//! Spike: prove a Windows Graphics Capture texture can be registered with CUDA
//! and converted to NV12 correctly.
//!
//! Resolves risk #2, the highest-uncertainty item in the project (plan Task 2).
//! Implements spec §8.4. Throwaway code -- the deliverables are:
//!   1. does `cuGraphicsD3D11RegisterResource` accept a WGC texture?
//!   2. is the NV12 conversion VISUALLY CORRECT? (open frame.png and look)
//!   3. the four `extern "C"` declarations Task 12 reuses verbatim (§8.2)
//!
//! The probe tries registration TWO ways and reports which worked:
//!   A. directly on the WGC texture (true zero-copy)
//!   B. after CopyResource into a texture we create with interop-friendly flags
//! WGC pool textures are not created by us, so their D3D11 misc flags are not
//! under our control; if A fails, knowing whether B works is the difference
//! between "redesign §8" and "pay one on-GPU copy".

use std::ffi::c_void;
use std::os::raw::c_uint;

use cudarc::driver::sys::{CUarray, CUresult, CUstream, CUtexObject};
use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_RESOURCE_MISC_SHARED, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

// ---------------------------------------------------------------------------
// CUDA-D3D11 graphics interop FFI. THIS BLOCK IS THE DELIVERABLE Task 12
// REUSES (spec §8.2): these four symbols live in cudaD3D11.h, not cuda.h, so
// cudarc does not bind them.
//
// NOTE (deviation from plan Task 2 Step 3): the plan writes
// `#[link(name = "nvcuda")]`. On MSVC there is no nvcuda.lib -- the import
// library for nvcuda.dll is cuda.lib. Linking "cuda" also reuses the search
// path cudarc's build script already sets up.
// ---------------------------------------------------------------------------

pub type CUgraphicsResource = *mut c_void;

#[link(name = "cuda")]
extern "C" {
    pub fn cuGraphicsD3D11RegisterResource(
        out: *mut CUgraphicsResource,
        d3d_resource: *mut c_void,
        flags: c_uint,
    ) -> CUresult;
    pub fn cuGraphicsMapResources(
        count: c_uint,
        res: *mut CUgraphicsResource,
        stream: CUstream,
    ) -> CUresult;
    pub fn cuGraphicsSubResourceGetMappedArray(
        out: *mut CUarray,
        res: CUgraphicsResource,
        array_index: c_uint,
        mip_level: c_uint,
    ) -> CUresult;
    pub fn cuGraphicsUnmapResources(
        count: c_uint,
        res: *mut CUgraphicsResource,
        stream: CUstream,
    ) -> CUresult;
}

pub const CU_GRAPHICS_REGISTER_FLAGS_NONE: c_uint = 0;

type Err = Box<dyn std::error::Error + Send + Sync>;

struct Probe {
    device: ID3D11Device,
    d3d_context: ID3D11DeviceContext,
    handled: bool,
}

impl GraphicsCaptureApiHandler for Probe {
    type Flags = ();
    type Error = Err;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Probe {
            device: ctx.device,
            d3d_context: ctx.device_context,
            handled: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.handled {
            capture_control.stop();
            return Ok(());
        }
        self.handled = true;

        // Report rather than propagate: a failure here IS the finding, and
        // returning Err would just abort the capture with a less clear message.
        if let Err(e) = self.probe(frame) {
            println!();
            println!("!!! PROBE FAILED: {e}");
            println!("!!! See the decision table in plan Task 2 Step 5.");
        }
        capture_control.stop();
        Ok(())
    }
}

impl Probe {
    fn probe(&mut self, frame: &mut Frame) -> Result<(), Err> {
        let width = frame.width();
        let height = frame.height();
        println!("captured frame: {width}x{height}");

        let cu_ctx = CudaContext::new(0)?;
        let stream = cu_ctx.default_stream();
        println!("CUDA context created on device 0");

        // --- attempt A: register the WGC texture directly (true zero-copy) ---
        let wgc_texture: &ID3D11Texture2D = unsafe { frame.as_raw_texture() };
        let wgc_ptr = wgc_texture.as_raw();

        let mut resource: CUgraphicsResource = std::ptr::null_mut();
        let direct_rc = unsafe {
            cuGraphicsD3D11RegisterResource(&mut resource, wgc_ptr, CU_GRAPHICS_REGISTER_FLAGS_NONE)
        };

        // Keep the staging texture alive for as long as `resource` refers to it.
        let _staging;
        let path;
        if direct_rc == CUresult::CUDA_SUCCESS {
            println!("[A] cuGraphicsD3D11RegisterResource on the WGC texture: SUCCESS");
            println!("    -> true zero-copy is available");
            path = "direct WGC texture (zero-copy)";
            _staging = None;
        } else {
            println!(
                "[A] cuGraphicsD3D11RegisterResource on the WGC texture: FAILED {direct_rc:?}"
            );
            println!("    -> trying [B] CopyResource into an interop-flagged texture");

            let staging = self.make_interop_texture(width, height)?;
            unsafe {
                self.d3d_context.CopyResource(&staging, wgc_texture);
                self.d3d_context.Flush();
            }
            let staging_ptr = staging.as_raw();
            let rc = unsafe {
                cuGraphicsD3D11RegisterResource(
                    &mut resource,
                    staging_ptr,
                    CU_GRAPHICS_REGISTER_FLAGS_NONE,
                )
            };
            if rc != CUresult::CUDA_SUCCESS {
                return Err(format!(
                    "[B] registration ALSO failed: {rc:?}. Neither the WGC texture nor \
                     an interop-flagged copy could be registered -- decision D6 needs \
                     revisiting (plan Task 2 Step 5)."
                )
                .into());
            }
            println!("[B] registration after CopyResource: SUCCESS");
            println!("    -> zero-copy from the WGC texture is NOT available;");
            println!("       one on-GPU CopyResource is required. Task 12 must budget it.");
            path = "CopyResource into interop texture";
            _staging = Some(staging);
        }

        // --- allocate NV12 planes; pitches equal width ---
        let y_len = (width * height) as usize;
        let uv_len = y_len / 2;
        let mut y_plane = stream.alloc_zeros::<u8>(y_len)?;
        let mut uv_plane = stream.alloc_zeros::<u8>(uv_len)?;

        // --- load the kernel ---
        // PTX is precompiled by build.rs with nvcc, exactly as §8.3 requires of
        // the real encoder. (Runtime NVRTC was tried first and aborts on this
        // CUDA/cudarc combination -- see the note in build.rs.)
        let ptx = cudarc::nvrtc::Ptx::from_src(include_str!(concat!(env!("OUT_DIR"), "/nv12.ptx")));
        let module = cu_ctx.load_module(ptx)?;
        let func = module.load_function("bgra_to_nv12")?;

        // One thread per 2x2 pixel quad.
        let bx = ((width / 2) + 15) / 16;
        let by = ((height / 2) + 15) / 16;
        let cfg = LaunchConfig {
            grid_dim: (bx, by, 1),
            block_dim: (16, 16, 1),
            shared_mem_bytes: 0,
        };

        let w_i = width as i32;
        let h_i = height as i32;
        let y_pitch = width as i32;
        let uv_pitch = width as i32;

        // Repeat the per-frame GPU work to get a STEADY-STATE cost. A single
        // cold pass is dominated by CUDA context creation and PTX JIT and says
        // nothing about the per-frame budget Task 12 needs (§12).
        //
        // The conversion is re-run over the same captured texture rather than
        // waiting for new frames on purpose: WGC delivery is present-driven, so
        // a static desktop delivers nothing at all (§7.2.1), and this isolates
        // the CUDA cost from capture arrival anyway.
        const ITERATIONS: usize = 200;
        let mut timings_us: Vec<u128> = Vec::with_capacity(ITERATIONS);
        let mut first_pass_planes = None;

        for i in 0..ITERATIONS {
            let t0 = std::time::Instant::now();

            // On the CopyResource path this copy is part of the per-frame cost.
            if let Some(staging) = _staging.as_ref() {
                unsafe { self.d3d_context.CopyResource(staging, wgc_texture) };
            }

            let rc = unsafe { cuGraphicsMapResources(1, &mut resource, std::ptr::null_mut()) };
            if rc != CUresult::CUDA_SUCCESS {
                return Err(format!("cuGraphicsMapResources failed: {rc:?}").into());
            }
            let mut array: CUarray = std::ptr::null_mut();
            let rc = unsafe { cuGraphicsSubResourceGetMappedArray(&mut array, resource, 0, 0) };
            if rc != CUresult::CUDA_SUCCESS {
                return Err(format!("cuGraphicsSubResourceGetMappedArray failed: {rc:?}").into());
            }
            // Recreated per frame: the mapped CUarray is only valid between map
            // and unmap, so the texture object cannot be cached across frames.
            let tex_object = unsafe { create_texture_object(array)? };

            unsafe {
                stream
                    .launch_builder(&func)
                    .arg(&tex_object)
                    .arg(&mut y_plane)
                    .arg(&mut uv_plane)
                    .arg(&w_i)
                    .arg(&h_i)
                    .arg(&y_pitch)
                    .arg(&uv_pitch)
                    .launch(cfg)?;
            }
            stream.synchronize()?;

            unsafe {
                cudarc::driver::sys::cuTexObjectDestroy(tex_object);
                let rc = cuGraphicsUnmapResources(1, &mut resource, std::ptr::null_mut());
                if rc != CUresult::CUDA_SUCCESS {
                    println!("warning: cuGraphicsUnmapResources failed: {rc:?}");
                }
            }

            timings_us.push(t0.elapsed().as_micros());

            // Read back once, from the first pass, for the PNG.
            if i == 0 {
                let y_host = stream.memcpy_dtov(&y_plane)?;
                let uv_host = stream.memcpy_dtov(&uv_plane)?;
                first_pass_planes = Some((y_host, uv_host));
            }
        }

        let (y_host, uv_host) = first_pass_planes.expect("first pass read back");

        let cold_us = timings_us[0];
        let mut sorted = timings_us.clone();
        sorted.sort_unstable();
        let pick = |p: f64| {
            sorted[(((p / 100.0) * sorted.len() as f64).ceil() as usize - 1).min(sorted.len() - 1)]
        };
        println!();
        println!("per-frame copy+map+convert over {ITERATIONS} iterations (no host readback):");
        println!("  cold first pass : {:.3} ms", cold_us as f64 / 1000.0);
        println!(
            "  p50 / p95 / p99 : {:.3} / {:.3} / {:.3} ms",
            pick(50.0) as f64 / 1000.0,
            pick(95.0) as f64 / 1000.0,
            pick(99.0) as f64 / 1000.0
        );
        println!(
            "  min / max       : {:.3} / {:.3} ms",
            sorted[0] as f64 / 1000.0,
            sorted[sorted.len() - 1] as f64 / 1000.0
        );
        println!("  (at {width}x{height}; spec §12 budgets <1 ms for BGRA->NV12)");

        // --- NV12 -> RGB on the CPU and write the PNG ---
        write_png(&y_host, &uv_host, width, height)?;
        println!();
        println!("wrote frame.png via: {path}");
        println!("*** NOW OPEN frame.png AND LOOK AT IT -- that is the actual test. ***");
        println!("    correct desktop image  -> tiled-layout handling is right, proceed");
        println!("    blocky mosaic          -> reading the texture as linear, re-read §8.1");
        println!("    near-noise             -> delta colour compression may be active");
        println!("    right shapes, wrong colours -> kernel maths/channel order, not interop");
        Ok(())
    }

    /// A texture we own, created with flags that permit CUDA registration.
    fn make_interop_texture(&self, width: u32, height: u32) -> Result<ID3D11Texture2D, Err> {
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
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut texture))?;
        }
        texture.ok_or_else(|| "CreateTexture2D returned null".into())
    }
}

/// Wrap a CUarray in a texture object using POINT filtering and UNNORMALIZED
/// coordinates -- the kernel indexes in pixels.
unsafe fn create_texture_object(array: CUarray) -> Result<CUtexObject, Err> {
    use cudarc::driver::sys::{
        CUaddress_mode, CUfilter_mode, CUresourcetype, CUDA_RESOURCE_DESC, CUDA_TEXTURE_DESC,
    };

    let mut res_desc: CUDA_RESOURCE_DESC = std::mem::zeroed();
    res_desc.resType = CUresourcetype::CU_RESOURCE_TYPE_ARRAY;
    res_desc.res.array.hArray = array;

    let mut tex_desc: CUDA_TEXTURE_DESC = std::mem::zeroed();
    tex_desc.addressMode = [
        CUaddress_mode::CU_TR_ADDRESS_MODE_CLAMP,
        CUaddress_mode::CU_TR_ADDRESS_MODE_CLAMP,
        CUaddress_mode::CU_TR_ADDRESS_MODE_CLAMP,
    ];
    tex_desc.filterMode = CUfilter_mode::CU_TR_FILTER_MODE_POINT;
    // flags = 0: CU_TRSF_NORMALIZED_COORDINATES deliberately NOT set.
    tex_desc.flags = 0;

    let mut tex_object: CUtexObject = 0;
    let rc = cudarc::driver::sys::cuTexObjectCreate(
        &mut tex_object,
        &res_desc,
        &tex_desc,
        std::ptr::null(),
    );
    if rc != CUresult::CUDA_SUCCESS {
        return Err(format!("cuTexObjectCreate failed: {rc:?}").into());
    }
    Ok(tex_object)
}

/// NV12 -> RGB with nearest-neighbour chroma upsampling. Spike quality on
/// purpose: this exists so a human can eyeball the result.
fn write_png(y: &[u8], uv: &[u8], width: u32, height: u32) -> Result<(), Err> {
    let w = width as usize;
    let h = height as usize;
    let mut rgb = vec![0u8; w * h * 3];

    for row in 0..h {
        for col in 0..w {
            let yy = y[row * w + col] as f32;
            let cx = col / 2;
            let cy = row / 2;
            let u = uv[cy * w + cx * 2] as f32;
            let v = uv[cy * w + cx * 2 + 1] as f32;

            // Inverse of the kernel's BT.709 limited-range transform.
            let yf = (yy - 16.0) / 219.0;
            let uf = (u - 128.0) / 224.0;
            let vf = (v - 128.0) / 224.0;

            let r = yf + 1.5748 * vf;
            let g = yf - 0.1873 * uf - 0.4681 * vf;
            let b = yf + 1.8556 * uf;

            let o = (row * w + col) * 3;
            rgb[o] = (r.clamp(0.0, 1.0) * 255.0) as u8;
            rgb[o + 1] = (g.clamp(0.0, 1.0) * 255.0) as u8;
            rgb[o + 2] = (b.clamp(0.0, 1.0) * 255.0) as u8;
        }
    }

    image::save_buffer(
        "frame.png",
        &rgb,
        width,
        height,
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(())
}

fn main() {
    let monitor = Monitor::primary().expect("no primary monitor");
    println!("capturing primary monitor, waiting for one frame...");

    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        (),
    );

    Probe::start(settings).expect("capture failed");
    println!("done");
}
