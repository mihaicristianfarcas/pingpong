//! Presenting decoded frames with Direct3D 11, the way Moonlight's Windows
//! renderer (`d3d11va.cpp`) does:
//!
//! - **V-Sync on** (default): a flip-model swap chain whose frame latency is
//!   one (its waitable object), each frame drawn as it is decoded once the
//!   chain has room, and shown on the next refresh. A full-screen window gets
//!   independent flip, straight to the display. No tearing.
//! - **Frame pacing** (opt-in): on each refresh of the display (DXGI's
//!   `WaitForVBlank`) the next frame is drawn, one per refresh.
//! - **V-Sync off**: presented at once, tearing allowed.
//!
//! Decoding (FFmpeg's D3D11VA) runs on the same device. Each decoded picture
//! is copied out of the decoder's texture array into one of a few textures
//! of ours at once -- a GPU copy of a few microseconds -- so the decoder's
//! pool is never held by the screen. The copy and every draw take the
//! device context's lock, which FFmpeg takes too.
//!
//! On top of the video: the statistics overlay, status and warnings. The
//! pointer is Windows' own (see `window`).

use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use pingpong_decode::d3d11va::{ContextLock, DecodedTexture};
use pingpong_proto::clock;
use windows::core::{Interface, PCSTR};
use windows::Win32::Foundation::{HANDLE, HMODULE, HWND, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP,
    D3D11_SRV_DIMENSION_TEXTURE2D, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_UNKNOWN,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGIFactory5, IDXGISwapChain1, IDXGISwapChain2,
    DXGI_FEATURE_PRESENT_ALLOW_TEARING, DXGI_FRAME_STATISTICS, DXGI_MWA_NO_ALT_ENTER, DXGI_PRESENT,
    DXGI_PRESENT_ALLOW_TEARING, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::WaitForSingleObjectEx;

use crate::stats::StatsCollector;
use crate::stream::FrameTiming;

const SHADERS: &str = r#"
struct VOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };

VOut v_full(uint id : SV_VertexID) {
    float2 p = float2(id == 1 ? 3.0 : -1.0, id == 2 ? 3.0 : -1.0);
    VOut o;
    o.pos = float4(p, 0.0, 1.0);
    o.uv = float2((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    return o;
}

Texture2D<float> luma : register(t0);
Texture2D<float2> chroma : register(t1);
SamplerState samp : register(s0);
cbuffer Video : register(b0) { float2 chroma_offset; float2 uv_scale; };

// BT.709, limited range -- the inverse of the host's converter. Chroma is
// sited left (H.264/HEVC type 0): sampled half a luma pixel to the right.
float4 f_video(VOut i) : SV_Target {
    float2 uv = i.uv * uv_scale;
    float y = luma.Sample(samp, uv);
    float2 c = chroma.Sample(samp, uv + chroma_offset);
    y = (y - 16.0 / 255.0) * (255.0 / 219.0);
    float cb = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
    float cr = (c.y - 128.0 / 255.0) * (255.0 / 224.0);
    float3 rgb = float3(y + 1.5748 * cr, y - 0.1873 * cb - 0.4681 * cr, y + 1.8556 * cb);
    return float4(saturate(rgb), 1.0);
}

cbuffer Quad : register(b0) { float4 rect; };

VOut v_quad(uint id : SV_VertexID) {
    float2 uv = float2(id & 1, id >> 1);
    VOut o;
    o.uv = uv;
    o.pos = float4(lerp(rect.x, rect.z, uv.x), lerp(rect.y, rect.w, uv.y), 0.0, 1.0);
    return o;
}

Texture2D<float4> tex : register(t0);

float4 f_quad(VOut i) : SV_Target { return tex.Sample(samp, i.uv); }
"#;

/// The Direct3D 11 device decoding and presentation share.
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub lock: Arc<ContextLock>,
}

// SAFETY: a Direct3D 11 device is free-threaded; its immediate context is
// only used under `lock` (and is multithread-protected besides).
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl Gpu {
    pub fn new() -> Result<Arc<Gpu>, String> {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| format!("no Direct3D 11 device: {e}"))?;
        }
        let (device, context): (ID3D11Device, ID3D11DeviceContext) = (
            device.ok_or("no Direct3D 11 device")?,
            context.ok_or("no Direct3D 11 context")?,
        );
        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            unsafe {
                let _ = mt.SetMultithreadProtected(true);
            }
        }
        Ok(Arc::new(Gpu {
            device,
            context,
            lock: Arc::new(ContextLock::default()),
        }))
    }
}

/// The drawable, as the window says it is.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    /// Pixels per 96 dpi point.
    pub scale: f64,
}

struct PoolSlot {
    texture: ID3D11Texture2D,
    luma: ID3D11ShaderResourceView,
    chroma: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
    busy: bool,
}

#[derive(Clone, Copy)]
struct Picture {
    slot: usize,
    timing: FrameTiming,
    decoded_at: u32,
    width: u32,
    height: u32,
}

#[derive(Default)]
struct Frames {
    slots: Vec<Option<PoolSlot>>,
    pending: VecDeque<Picture>,
    current: Option<Picture>,
}

impl Frames {
    fn release(&mut self, slot: usize) {
        if let Some(Some(s)) = self.slots.get_mut(slot) {
            s.busy = false;
        }
    }
}

/// Frames the pacing queue holds at most (older ones are dropped).
const PACING_QUEUE_MAX: usize = 3;
/// Our textures: the pacing queue, the one on screen, the one being filled.
const POOL: usize = PACING_QUEUE_MAX + 2;
/// Refreshes a pacing queue may stand before it is skipped to the newest.
const PACING_BACKLOG_TICKS: u32 = 10;

/// What the rest of the client hands the render thread.
pub struct RenderShared {
    pub gpu: Arc<Gpu>,
    frames: Mutex<Frames>,
    timings: Mutex<Vec<FrameTiming>>,
    frame_ready: Condvar,
    new_frame: AtomicBool,
    overlay_enabled: AtomicBool,
    status: Mutex<Option<String>>,
    notice: Mutex<Option<String>>,
    warning: Mutex<Option<String>>,
    dirty: AtomicBool,
    layout: Mutex<Layout>,
    stop: AtomicBool,
    stats: Arc<StatsCollector>,
    vsync: bool,
    paced: bool,
    vsync_ticks: Mutex<u64>,
    vsync_tick: Condvar,
    /// A test snapshot of the screen: where, and after how many frames.
    snapshot: Mutex<Option<(std::path::PathBuf, u32)>>,
}

impl RenderShared {
    pub fn new(
        gpu: Arc<Gpu>,
        stats: Arc<StatsCollector>,
        vsync: bool,
        paced: bool,
    ) -> Arc<RenderShared> {
        // PING_TEST_SNAPSHOT=PATH[@FRAMES]: save what is on screen as a PNG
        // after that many frames (default 120), for tests without eyes.
        let snapshot = std::env::var("PING_TEST_SNAPSHOT")
            .ok()
            .map(|v| match v.rsplit_once('@') {
                Some((p, n)) if n.parse::<u32>().is_ok() => (p.into(), n.parse().unwrap()),
                _ => (v.into(), 120),
            });
        let mut frames = Frames::default();
        frames.slots.resize_with(POOL, || None);
        Arc::new(RenderShared {
            gpu,
            frames: Mutex::new(frames),
            timings: Mutex::new(Vec::with_capacity(16)),
            frame_ready: Condvar::new(),
            new_frame: AtomicBool::new(false),
            overlay_enabled: AtomicBool::new(false),
            status: Mutex::new(None),
            notice: Mutex::new(None),
            warning: Mutex::new(None),
            dirty: AtomicBool::new(true),
            layout: Mutex::new(Layout::default()),
            stop: AtomicBool::new(false),
            stats,
            vsync,
            paced: paced && vsync,
            vsync_ticks: Mutex::new(0),
            vsync_tick: Condvar::new(),
            snapshot: Mutex::new(snapshot),
        })
    }

    fn poke(&self) {
        self.dirty.store(true, Ordering::Release);
        self.frame_ready.notify_one();
    }

    /// Remember a frame's network timing until it comes out of the decoder.
    pub fn expect(&self, timing: FrameTiming) {
        let mut t = self.timings.lock();
        if t.len() >= 16 {
            t.remove(0);
        }
        t.push(timing);
    }

    /// A decoded picture (on the decoder's thread): copy it into one of our
    /// textures and queue it. Without pacing the newest replaces any not yet
    /// shown; with it, frames queue to be shown one per refresh.
    pub fn push_picture(&self, pic: &DecodedTexture<'_>) {
        let timing = {
            let t = self.timings.lock();
            t.iter()
                .rev()
                .find(|t| t.frame_id == pic.tag)
                .copied()
                .unwrap_or_default()
        };
        self.stats
            .decoded(pic.decoded_at_us.wrapping_sub(timing.reassembled_us));
        let (w, h) = ((pic.width + 1) & !1, (pic.height + 1) & !1);
        {
            let _ctx = self.gpu.lock.lock();
            let mut f = self.frames.lock();
            if !self.paced {
                while let Some(p) = f.pending.pop_front() {
                    f.release(p.slot);
                }
            } else if f.pending.len() >= PACING_QUEUE_MAX {
                if let Some(p) = f.pending.pop_front() {
                    f.release(p.slot);
                }
            }
            let Some(slot) = f
                .slots
                .iter()
                .position(|s| s.as_ref().is_none_or(|s| !s.busy))
            else {
                tracing::warn!("no free texture for a decoded frame");
                return;
            };
            if f.slots[slot]
                .as_ref()
                .is_none_or(|s| s.width != w || s.height != h)
            {
                match self.texture(w, h) {
                    Ok(s) => f.slots[slot] = Some(s),
                    Err(e) => {
                        tracing::error!(error = %e, "could not make a texture for the video");
                        return;
                    }
                }
            }
            let s = f.slots[slot].as_mut().expect("made above");
            let region = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            unsafe {
                self.gpu.context.CopySubresourceRegion(
                    &s.texture,
                    0,
                    0,
                    0,
                    0,
                    pic.texture,
                    pic.index,
                    Some(&region),
                );
            }
            s.busy = true;
            f.pending.push_back(Picture {
                slot,
                timing,
                decoded_at: pic.decoded_at_us,
                width: pic.width,
                height: pic.height,
            });
        }
        self.new_frame.store(true, Ordering::Release);
        self.frame_ready.notify_one();
    }

    fn texture(&self, width: u32, height: u32) -> windows::core::Result<PoolSlot> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let device = &self.gpu.device;
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.expect("created");
        let view = |format: DXGI_FORMAT| -> windows::core::Result<ID3D11ShaderResourceView> {
            let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                    },
                },
            };
            let mut srv = None;
            unsafe { device.CreateShaderResourceView(&texture, Some(&desc), Some(&mut srv))? };
            Ok(srv.expect("created"))
        };
        let (luma, chroma) = (view(DXGI_FORMAT_R8_UNORM)?, view(DXGI_FORMAT_R8G8_UNORM)?);
        Ok(PoolSlot {
            texture,
            luma,
            chroma,
            width,
            height,
            busy: false,
        })
    }

    /// Show or hide the statistics overlay (Ctrl+Alt+Shift+S).
    pub fn toggle_overlay(&self) -> bool {
        let on = !self.overlay_enabled.fetch_xor(true, Ordering::AcqRel);
        self.poke();
        on
    }

    pub fn set_overlay(&self, on: bool) {
        self.overlay_enabled.store(on, Ordering::Release);
        self.poke();
    }

    /// Show `text` until the first frame (None: nothing).
    pub fn set_status(&self, text: Option<String>) {
        *self.status.lock() = text;
        self.poke();
    }

    /// Show a connection warning in the corner until cleared (None).
    pub fn set_warning(&self, text: Option<String>) {
        *self.warning.lock() = text;
        self.poke();
    }

    /// Show `text` over the picture until cleared (None).
    pub fn set_notice(&self, text: Option<String>) {
        *self.notice.lock() = text;
        self.poke();
    }

    pub fn set_layout(&self, l: Layout) {
        let mut cur = self.layout.lock();
        if *cur != l {
            *cur = l;
            drop(cur);
            self.poke();
        }
    }

    /// Where the picture is drawn in the drawable, and the picture's own
    /// size: the window maps the pointer with them.
    pub fn video_rect(&self) -> Option<VideoPlacement> {
        let f = self.frames.lock();
        let p = f.current.or_else(|| f.pending.back().copied())?;
        let l = *self.layout.lock();
        Some((
            fit(
                l.width as f64,
                l.height as f64,
                p.width as f64,
                p.height as f64,
            ),
            (p.width as f64, p.height as f64),
        ))
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.frame_ready.notify_all();
        self.vsync_tick.notify_all();
    }
}

/// Where the picture sits in the drawable (x, y, w, h), and the picture's
/// own size (w, h).
pub type VideoPlacement = ((f64, f64, f64, f64), (f64, f64));

/// `sw`x`sh` fitted inside `dw`x`dh`, centred, aspect kept: (x, y, w, h).
fn fit(dw: f64, dh: f64, sw: f64, sh: f64) -> (f64, f64, f64, f64) {
    if sw <= 0.0 || sh <= 0.0 {
        return (0.0, 0.0, dw, dh);
    }
    let s = (dw / sw).min(dh / sh);
    let (w, h) = ((sw * s).round(), (sh * s).round());
    (((dw - w) / 2.0).round(), ((dh - h) / 2.0).round(), w, h)
}

struct Overlay {
    text: String,
    view: ID3D11ShaderResourceView,
    width: f64,
    height: f64,
}

struct Renderer {
    gpu: Arc<Gpu>,
    swapchain: IDXGISwapChain1,
    waitable: Option<HANDLE>,
    flags: DXGI_SWAP_CHAIN_FLAG,
    tearing: bool,
    rtv: Option<ID3D11RenderTargetView>,
    size: (u32, u32),
    vs_full: ID3D11VertexShader,
    ps_video: ID3D11PixelShader,
    vs_quad: ID3D11VertexShader,
    ps_quad: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    blend: ID3D11BlendState,
    /// No culling: the full-screen triangle and the quads face either way.
    raster: ID3D11RasterizerState,
    video_cb: ID3D11Buffer,
    quad_cb: ID3D11Buffer,
    status: Option<Overlay>,
    notice: Option<Overlay>,
    warning: Option<Overlay>,
    stats_overlay: Option<Overlay>,
    overlay_at: Option<Instant>,
    backlog: u32,
    /// Presented frames waiting for DXGI to say when they reached the
    /// screen: (present count, decoded at, captured at on the host,
    /// presented at).
    presents: VecDeque<(u32, u32, u32, u32)>,
    qpc_per_us: f64,
    shown: u32,
    stats_unknown_logged: bool,
    /// The last (refresh count, QPC time) DXGI reported, and the refresh
    /// period two of them give.
    last_sync: Option<(u32, i64)>,
    refresh_us: f64,
}

fn compile(entry: &str, target: &str) -> Result<Vec<u8>, String> {
    let entry = std::ffi::CString::new(entry).unwrap();
    let target = std::ffi::CString::new(target).unwrap();
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = unsafe {
        D3DCompile(
            SHADERS.as_ptr() as *const c_void,
            SHADERS.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(entry.as_ptr() as *const u8),
            PCSTR(target.as_ptr() as *const u8),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let blob_bytes = |b: &ID3DBlob| unsafe {
        std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec()
    };
    match (result, code) {
        (Ok(()), Some(c)) => Ok(blob_bytes(&c)),
        (r, _) => Err(format!(
            "shader {}: {:?} {}",
            entry.to_string_lossy(),
            r.err(),
            errors
                .map(|e| String::from_utf8_lossy(&blob_bytes(&e)).into_owned())
                .unwrap_or_default()
        )),
    }
}

impl Renderer {
    fn new(hwnd: HWND, shared: &RenderShared) -> Result<Renderer, String> {
        let gpu = shared.gpu.clone();
        let device = &gpu.device;
        fn e(what: &'static str) -> impl Fn(windows::core::Error) -> String {
            move |err| format!("{what}: {err}")
        }
        unsafe {
            let dxgi: IDXGIDevice = device.cast().map_err(e("IDXGIDevice"))?;
            let factory: IDXGIFactory2 = dxgi
                .GetAdapter()
                .and_then(|a| a.GetParent())
                .map_err(e("DXGI factory"))?;
            let mut allow = windows::core::BOOL(0);
            let tearing = factory
                .cast::<IDXGIFactory5>()
                .and_then(|f| {
                    f.CheckFeatureSupport(
                        DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                        &mut allow as *mut _ as *mut c_void,
                        std::mem::size_of::<windows::core::BOOL>() as u32,
                    )
                })
                .is_ok()
                && allow.as_bool();
            let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
            if tearing {
                flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
            }
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: 0,
                Height: 0,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 3,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: flags.0 as u32,
            };
            let swapchain = factory
                .CreateSwapChainForHwnd(device, hwnd, &desc, None, None)
                .map_err(e("swap chain"))?;
            // Alt+Enter is ours (nothing), not DXGI's exclusive full screen.
            let _ = factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
            let waitable = swapchain.cast::<IDXGISwapChain2>().ok().and_then(|s2| {
                s2.SetMaximumFrameLatency(1).ok()?;
                let h = s2.GetFrameLatencyWaitableObject();
                (!h.is_invalid()).then_some(h)
            });

            let vs = |code: Vec<u8>| -> Result<ID3D11VertexShader, String> {
                let mut s = None;
                device
                    .CreateVertexShader(&code, None, Some(&mut s))
                    .map_err(e("vertex shader"))?;
                Ok(s.unwrap())
            };
            let ps = |code: Vec<u8>| -> Result<ID3D11PixelShader, String> {
                let mut s = None;
                device
                    .CreatePixelShader(&code, None, Some(&mut s))
                    .map_err(e("pixel shader"))?;
                Ok(s.unwrap())
            };
            let vs_full = vs(compile("v_full", "vs_4_0")?)?;
            let ps_video = ps(compile("f_video", "ps_4_0")?)?;
            let vs_quad = vs(compile("v_quad", "vs_4_0")?)?;
            let ps_quad = ps(compile("f_quad", "ps_4_0")?)?;

            let sampler_desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                ..Default::default()
            };
            let mut sampler = None;
            device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(e("sampler"))?;

            // Premultiplied alpha, for the overlays.
            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device
                .CreateBlendState(&blend_desc, Some(&mut blend))
                .map_err(e("blend state"))?;

            let raster_desc = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            };
            let mut raster = None;
            device
                .CreateRasterizerState(&raster_desc, Some(&mut raster))
                .map_err(e("rasterizer state"))?;

            let cbuffer = || -> Result<ID3D11Buffer, String> {
                let desc = D3D11_BUFFER_DESC {
                    ByteWidth: 16,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                };
                let mut b = None;
                device
                    .CreateBuffer(&desc, None, Some(&mut b))
                    .map_err(e("constant buffer"))?;
                Ok(b.unwrap())
            };
            let mut freq = 0i64;
            let _ = QueryPerformanceFrequency(&mut freq);

            Ok(Renderer {
                gpu: gpu.clone(),
                swapchain,
                waitable,
                flags,
                tearing,
                rtv: None,
                size: (0, 0),
                vs_full,
                ps_video,
                vs_quad,
                ps_quad,
                sampler: sampler.unwrap(),
                blend: blend.unwrap(),
                raster: raster.unwrap(),
                video_cb: cbuffer()?,
                quad_cb: cbuffer()?,
                status: None,
                notice: None,
                warning: None,
                stats_overlay: None,
                overlay_at: None,
                backlog: 0,
                presents: VecDeque::new(),
                qpc_per_us: freq.max(1) as f64 / 1e6,
                shown: 0,
                stats_unknown_logged: false,
                last_sync: None,
                refresh_us: 1e6 / 60.0,
            })
        }
    }

    /// Keep the swap chain's buffers the window's size.
    fn sync_size(&mut self, shared: &RenderShared) {
        let l = *shared.layout.lock();
        if l.width == 0 || l.height == 0 || (l.width, l.height) == self.size {
            return;
        }
        self.rtv = None;
        unsafe {
            self.gpu.context.OMSetRenderTargets(None, None);
            if let Err(err) =
                self.swapchain
                    .ResizeBuffers(0, l.width, l.height, DXGI_FORMAT_UNKNOWN, self.flags)
            {
                tracing::warn!(error = %err, "swap chain resize");
                return;
            }
        }
        self.size = (l.width, l.height);
    }

    fn target(&mut self) -> Option<ID3D11RenderTargetView> {
        if self.rtv.is_none() {
            unsafe {
                let buffer: ID3D11Texture2D = self.swapchain.GetBuffer(0).ok()?;
                let mut rtv = None;
                self.gpu
                    .device
                    .CreateRenderTargetView(&buffer, None, Some(&mut rtv))
                    .ok()?;
                self.rtv = rtv;
                if self.size == (0, 0) {
                    let mut d = D3D11_TEXTURE2D_DESC::default();
                    buffer.GetDesc(&mut d);
                    self.size = (d.Width, d.Height);
                }
            }
        }
        self.rtv.clone()
    }

    /// Upload a text overlay (or keep the one already up for that text).
    fn overlay(
        device: &ID3D11Device,
        slot: Option<Overlay>,
        text: &str,
        scale: f64,
    ) -> Option<Overlay> {
        if let Some(o) = slot.filter(|o| o.text == text) {
            return Some(o);
        }
        let b = super::text::rasterize(text, scale)?;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: b.width as u32,
            Height: b.height as u32,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let data = D3D11_SUBRESOURCE_DATA {
            pSysMem: b.rgba.as_ptr() as *const c_void,
            SysMemPitch: (b.width * 4) as u32,
            SysMemSlicePitch: 0,
        };
        unsafe {
            let mut tex = None;
            device
                .CreateTexture2D(&desc, Some(&data), Some(&mut tex))
                .ok()?;
            let mut view = None;
            device
                .CreateShaderResourceView(&tex?, None, Some(&mut view))
                .ok()?;
            Some(Overlay {
                text: text.to_string(),
                view: view?,
                width: b.width as f64,
                height: b.height as f64,
            })
        }
    }

    /// The frame to show now. Paced, Moonlight's way: the oldest waiting, one
    /// per refresh, so frames that network jitter delivered together are
    /// still each shown; but a queue that stands for `PACING_BACKLOG_TICKS`
    /// is latency, and is skipped to its newest frame.
    fn take_frame(&mut self, shared: &RenderShared) -> Option<Picture> {
        let mut f = shared.frames.lock();
        let pic = if shared.paced {
            self.backlog = if f.pending.len() > 1 {
                self.backlog + 1
            } else {
                0
            };
            if self.backlog >= PACING_BACKLOG_TICKS {
                self.backlog = 0;
                while f.pending.len() > 1 {
                    let p = f.pending.pop_front().expect("more than one");
                    f.release(p.slot);
                }
            }
            f.pending.pop_front()
        } else {
            let newest = f.pending.pop_back();
            while let Some(p) = f.pending.pop_front() {
                f.release(p.slot);
            }
            newest
        };
        shared
            .new_frame
            .store(!f.pending.is_empty(), Ordering::Release);
        if let Some(p) = pic {
            if let Some(old) = f.current.replace(p) {
                f.release(old.slot);
            }
        }
        pic
    }

    /// Draw and present, if there is anything new to show (or `force`: the
    /// swap chain's room was taken, and only a present gives it back).
    /// Returns whether a new frame was presented.
    fn frame(&mut self, shared: &RenderShared, force: bool) -> bool {
        let gpu = self.gpu.clone();
        self.measure_presents(shared);
        let ctx_lock = gpu.lock.lock();
        self.sync_size(shared);
        let fresh = if shared.new_frame.load(Ordering::Acquire) {
            self.take_frame(shared)
        } else {
            None
        };
        let overlay_due = shared.overlay_enabled.load(Ordering::Acquire)
            && self
                .overlay_at
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        let dirty = shared.dirty.swap(false, Ordering::AcqRel) || overlay_due || force;
        if fresh.is_none() && !dirty {
            return false;
        }
        let Some(rtv) = self.target() else {
            return false;
        };
        let layout = *shared.layout.lock();
        let scale = if layout.scale > 0.0 {
            layout.scale
        } else {
            1.0
        };
        let (dw, dh) = (self.size.0 as f64, self.size.1 as f64);
        let ctx = &gpu.context;
        let current = shared.frames.lock().current;
        unsafe {
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.ClearRenderTargetView(&rtv, &[0.0, 0.0, 0.0, 1.0]);
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.RSSetState(&self.raster);
            ctx.IASetInputLayout(None);

            if let Some(p) = current {
                let f = shared.frames.lock();
                if let Some(Some(s)) = f.slots.get(p.slot) {
                    let (x, y, w, h) = fit(dw, dh, p.width as f64, p.height as f64);
                    ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                        TopLeftX: x as f32,
                        TopLeftY: y as f32,
                        Width: w as f32,
                        Height: h as f32,
                        MinDepth: 0.0,
                        MaxDepth: 1.0,
                    }]));
                    let params: [f32; 4] = [
                        0.5 / s.width as f32,
                        0.0,
                        p.width as f32 / s.width as f32,
                        p.height as f32 / s.height as f32,
                    ];
                    ctx.UpdateSubresource(
                        &self.video_cb,
                        0,
                        None,
                        params.as_ptr() as *const c_void,
                        0,
                        0,
                    );
                    ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
                    ctx.VSSetShader(&self.vs_full, None);
                    ctx.PSSetShader(&self.ps_video, None);
                    ctx.PSSetConstantBuffers(0, Some(&[Some(self.video_cb.clone())]));
                    ctx.PSSetShaderResources(
                        0,
                        Some(&[Some(s.luma.clone()), Some(s.chroma.clone())]),
                    );
                    ctx.OMSetBlendState(None, None, 0xFFFF_FFFF);
                    ctx.Draw(3, 0);
                }
            }

            // Overlays, in full-drawable coordinates.
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: dw as f32,
                Height: dh as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.VSSetShader(&self.vs_quad, None);
            ctx.PSSetShader(&self.ps_quad, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.quad_cb.clone())]));
            ctx.OMSetBlendState(&self.blend, None, 0xFFFF_FFFF);
            let quad = |o: &Overlay, x: f64, y: f64| {
                let ndc =
                    |px: f64, py: f64| ((px / dw * 2.0 - 1.0) as f32, (1.0 - py / dh * 2.0) as f32);
                let (x0, y0) = ndc(x, y);
                let (x1, y1) = ndc(x + o.width, y + o.height);
                let rect: [f32; 4] = [x0, y0, x1, y1];
                ctx.UpdateSubresource(&self.quad_cb, 0, None, rect.as_ptr() as *const c_void, 0, 0);
                ctx.PSSetShaderResources(0, Some(&[Some(o.view.clone())]));
                ctx.Draw(4, 0);
            };
            let margin = 12.0 * scale;
            let device = &gpu.device;

            // Centred: the status before the first frame, a notice after.
            if current.is_none() {
                self.notice = None;
                self.status = shared
                    .status
                    .lock()
                    .clone()
                    .and_then(|t| Self::overlay(device, self.status.take(), &t, scale));
            } else {
                self.status = None;
                self.notice = shared
                    .notice
                    .lock()
                    .clone()
                    .and_then(|t| Self::overlay(device, self.notice.take(), &t, scale));
            }
            for o in [&self.status, &self.notice].into_iter().flatten() {
                quad(
                    o,
                    ((dw - o.width) / 2.0).round(),
                    ((dh - o.height) / 2.0).round(),
                );
            }

            if shared.overlay_enabled.load(Ordering::Acquire) {
                if overlay_due {
                    self.overlay_at = Some(Instant::now());
                    let text = shared.stats.snapshot().overlay_text();
                    self.stats_overlay =
                        Self::overlay(device, self.stats_overlay.take(), &text, scale);
                }
                if let Some(o) = &self.stats_overlay {
                    quad(o, margin, margin);
                }
            } else {
                self.stats_overlay = None;
                self.overlay_at = None;
            }

            let warning = shared.warning.lock().clone().filter(|_| current.is_some());
            self.warning =
                warning.and_then(|t| Self::overlay(device, self.warning.take(), &t, scale));
            if let Some(o) = &self.warning {
                quad(o, dw - o.width - margin, margin);
            }
            ctx.PSSetShaderResources(0, Some(&[None, None]));

            let (interval, flags) = if shared.vsync {
                (1, DXGI_PRESENT(0))
            } else {
                (
                    0,
                    if self.tearing {
                        DXGI_PRESENT_ALLOW_TEARING
                    } else {
                        DXGI_PRESENT(0)
                    },
                )
            };
            // Not under the lock: a present can wait a refresh (or longer,
            // for a monitor asleep), and decoding must not wait with it.
            drop(ctx_lock);
            let presented_at = clock::now_us();
            let hr = self.swapchain.Present(interval, flags);
            if hr.is_err() {
                tracing::warn!(error = ?hr, "present");
            }
            if let Some(p) = fresh {
                self.shown += 1;
                let count = self.swapchain.GetLastPresentCount().unwrap_or(0);
                self.presents
                    .push_back((count, p.decoded_at, p.timing.captured_us, presented_at));
                if self.presents.len() > 8 {
                    if let Some((_, decoded_at, captured_at, at)) = self.presents.pop_front() {
                        Self::measured(shared, decoded_at, captured_at, at);
                    }
                }
                self.snapshot_if_due(shared);
            }
        }
        fresh.is_some()
    }

    /// A frame on the screen at `on_glass`: decode → glass, and capture →
    /// glass.
    fn measured(shared: &RenderShared, decoded_at: u32, captured_at: u32, on_glass: u32) {
        shared.stats.presented(on_glass.wrapping_sub(decoded_at));
        if let Some(captured) = shared.stats.host_to_local(captured_at) {
            let us = on_glass.wrapping_sub(captured) as i32;
            if (0..1_000_000).contains(&us) {
                shared.stats.end_to_end(us as u32);
            }
        }
    }

    /// When DXGI reports a presented frame reached the screen, measure it
    /// then; when it cannot say (a composed window), at its present.
    fn measure_presents(&mut self, shared: &RenderShared) {
        if self.presents.is_empty() {
            return;
        }
        let mut st = DXGI_FRAME_STATISTICS::default();
        let result = unsafe { self.swapchain.GetFrameStatistics(&mut st) };
        let known = result.is_ok() && st.SyncQPCTime != 0;
        if !known && !self.stats_unknown_logged {
            self.stats_unknown_logged = true;
            tracing::debug!(error = ?result.err(), "no frame statistics from DXGI; timing \
                frames at their present");
        }
        if !known {
            for (_, decoded_at, captured_at, at) in std::mem::take(&mut self.presents) {
                Self::measured(shared, decoded_at, captured_at, at);
            }
            return;
        }
        // The refresh period, from two reports.
        if let Some((refresh, qpc)) = self.last_sync {
            if st.SyncRefreshCount > refresh && st.SyncQPCTime > qpc {
                let us = (st.SyncQPCTime - qpc) as f64
                    / self.qpc_per_us
                    / (st.SyncRefreshCount - refresh) as f64;
                if (2_000.0..60_000.0).contains(&us) {
                    self.refresh_us = us;
                }
            }
        }
        self.last_sync = Some((st.SyncRefreshCount, st.SyncQPCTime));
        let mut now_qpc = 0i64;
        unsafe {
            let _ = QueryPerformanceCounter(&mut now_qpc);
        }
        let now = clock::now_us();
        let synced_ago_us = (now_qpc - st.SyncQPCTime).max(0) as f64 / self.qpc_per_us;
        while let Some(&(count, decoded_at, captured_at, at)) = self.presents.front() {
            if count > st.PresentCount {
                break;
            }
            self.presents.pop_front();
            // DXGI times the newest frame on the glass; one presented k
            // presents before it went up about k refreshes earlier (a present
            // a refresh, with V-Sync and a frame latency of one).
            let k = (st.PresentCount - count) as f64;
            let on_glass = now.wrapping_sub((synced_ago_us + k * self.refresh_us) as u32);
            // Never before its own present.
            let on_glass = if (on_glass.wrapping_sub(at) as i32) < 0 {
                at
            } else {
                on_glass
            };
            Self::measured(shared, decoded_at, captured_at, on_glass);
        }
    }

    /// PING_TEST_SNAPSHOT: the back buffer just presented, as a PNG.
    fn snapshot_if_due(&mut self, shared: &RenderShared) {
        let mut due = shared.snapshot.lock();
        let Some((path, after)) = due.as_ref() else {
            return;
        };
        if self.shown < *after {
            return;
        }
        let path = path.clone();
        *due = None;
        drop(due);
        match self.read_back() {
            Ok((w, h, rgba)) => {
                let result = std::fs::File::create(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|f| {
                        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
                        enc.set_color(png::ColorType::Rgba);
                        enc.set_depth(png::BitDepth::Eight);
                        enc.write_header()
                            .and_then(|mut wr| wr.write_image_data(&rgba))
                            .map_err(|e| e.to_string())
                    });
                match result {
                    Ok(()) => {
                        tracing::info!(path = %path.display(), width = w, height = h, "test snapshot saved")
                    }
                    Err(e) => tracing::warn!(error = e, "test snapshot not saved"),
                }
            }
            Err(e) => tracing::warn!(error = %e, "test snapshot not read back"),
        }
    }

    fn read_back(&self) -> windows::core::Result<(u32, u32, Vec<u8>)> {
        let _ctx = self.gpu.lock.lock();
        unsafe {
            let buffer: ID3D11Texture2D = self.swapchain.GetBuffer(0)?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            buffer.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.MiscFlags = 0;
            let mut staging = None;
            self.gpu
                .device
                .CreateTexture2D(&desc, None, Some(&mut staging))?;
            let staging = staging.expect("created");
            let ctx = &self.gpu.context;
            ctx.CopyResource(&staging, &buffer);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
            let (w, h) = (desc.Width as usize, desc.Height as usize);
            let mut rgba = vec![0u8; w * h * 4];
            for y in 0..h {
                let row = std::slice::from_raw_parts(
                    (map.pData as *const u8).add(y * map.RowPitch as usize),
                    w * 4,
                );
                for x in 0..w {
                    let (b, g, r) = (row[x * 4], row[x * 4 + 1], row[x * 4 + 2]);
                    rgba[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[r, g, b, 255]);
                }
            }
            ctx.Unmap(&staging, 0);
            Ok((desc.Width, desc.Height, rgba))
        }
    }
}

/// Wait until the swap chain has room for a frame (frame latency one), so
/// the frame drawn next is the next shown. False when it timed out.
fn wait_for_room(waitable: Option<HANDLE>, ms: u32) -> bool {
    match waitable {
        Some(h) => unsafe { WaitForSingleObjectEx(h, ms, true) == WAIT_OBJECT_0 },
        None => true,
    }
}

/// Ticks `RenderShared::vsync_ticks` on each refresh of the display the
/// window is on.
struct VBlank {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl VBlank {
    fn start(swapchain: &IDXGISwapChain1, shared: Arc<RenderShared>) -> Option<VBlank> {
        let output = unsafe { swapchain.GetContainingOutput() }.ok()?;
        struct SendOutput(windows::Win32::Graphics::Dxgi::IDXGIOutput);
        // SAFETY: DXGI outputs are free-threaded.
        unsafe impl Send for SendOutput {}
        let output = SendOutput(output);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("ping-vblank".into())
                .spawn(move || {
                    let output = output;
                    while !stop.load(Ordering::Relaxed) {
                        if unsafe { output.0.WaitForVBlank() }.is_err() {
                            std::thread::sleep(Duration::from_millis(8));
                        }
                        *shared.vsync_ticks.lock() += 1;
                        shared.vsync_tick.notify_one();
                    }
                })
                .ok()?
        };
        Some(VBlank {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for VBlank {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Run the renderer until `shared.stop()`. Call on a dedicated thread.
pub fn run(hwnd: isize, shared: Arc<RenderShared>) {
    let mut r = match Renderer::new(HWND(hwnd as *mut c_void), &shared) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "renderer failed to start");
            return;
        }
    };
    tracing::info!(
        vsync = shared.vsync,
        paced = shared.paced,
        tearing = r.tearing,
        waitable = r.waitable.is_some(),
        "renderer ready"
    );
    let vblank = if shared.paced {
        VBlank::start(&r.swapchain, shared.clone())
    } else {
        None
    };
    if shared.paced && vblank.is_none() {
        tracing::warn!("no display clock; presenting frames as they arrive");
    }
    let mut seen = 0u64;
    while !shared.stop.load(Ordering::Acquire) {
        if vblank.is_some() {
            // Frame pacing: on each refresh, the next frame.
            {
                let mut ticks = shared.vsync_ticks.lock();
                if *ticks == seen {
                    shared
                        .vsync_tick
                        .wait_for(&mut ticks, Duration::from_millis(50));
                }
                seen = *ticks;
            }
            let due =
                shared.new_frame.load(Ordering::Acquire) || shared.dirty.load(Ordering::Acquire);
            // Room for it, or the refresh passes it by.
            if due && wait_for_room(r.waitable, 0) {
                r.frame(&shared, true);
            }
        } else {
            {
                let mut f = shared.frames.lock();
                if f.pending.is_empty() && !shared.dirty.load(Ordering::Acquire) {
                    shared
                        .frame_ready
                        .wait_for(&mut f, Duration::from_millis(50));
                }
            }
            let overlay_due = shared.overlay_enabled.load(Ordering::Acquire)
                && r.overlay_at
                    .is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
            let due = shared.new_frame.load(Ordering::Acquire)
                || shared.dirty.load(Ordering::Acquire)
                || overlay_due;
            if !due {
                continue;
            }
            // Before taking the frame, so that it is the newest when drawn.
            let waited = shared.vsync && wait_for_room(r.waitable, 100);
            r.frame(&shared, waited);
        }
    }
    drop(vblank);
    // The textures go with the device's last user.
    let _ctx = shared.gpu.lock.lock();
    let mut f = shared.frames.lock();
    f.pending.clear();
    f.current = None;
    f.slots.iter_mut().for_each(|s| *s = None);
}
