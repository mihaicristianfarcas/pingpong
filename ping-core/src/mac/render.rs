//! Presenting decoded frames with Metal, the way Moonlight's macOS renderer
//! (`vt_metal.mm`) does:
//!
//! - **V-Sync on** (default): `displaySyncEnabled = YES`, each frame drawn
//!   the moment it is decoded and shown on the next refresh. No tearing.
//! - **Frame pacing** (opt-in): Moonlight's pacer. On each refresh of the
//!   display (a `CVDisplayLink` tick) the newest frame is drawn and shown at
//!   the next one: even pacing, frames never queue, for about half a refresh
//!   more latency. (A `CAMetalDisplayLink` did this first, but it hands out
//!   its drawable a refresh or more before the one it targets: 20 ms at
//!   120 Hz, 29 ms at 60 Hz, decode to glass.)
//! - **V-Sync off**: `displaySyncEnabled = NO`, frames drawn as they are
//!   decoded without waiting for a refresh. Lowest latency, tears.
//!
//! Zero-copy: VideoToolbox's CVPixelBuffer is bound to Metal through
//! `CVMetalTextureCache`. The buffer and its texture wrappers are held until
//! the command buffer completes -- releasing them at `commit` (as an earlier
//! version did) lets VideoToolbox recycle the IOSurface into the next decode
//! while the GPU is still sampling it.
//!
//! On top of the video: the client-drawn cursor (lag-free: drawn where the
//! local pointer is, in the host application's shape) and the statistics
//! overlay.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFRetained, CGSize};
use objc2_core_video::{
    CVDisplayLink, CVImageBuffer, CVMetalTexture, CVMetalTextureCache, CVMetalTextureGetTexture,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidthOfPlane,
};
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_quartz_core::{CACurrentMediaTime, CAEDRMetadata, CAMetalDrawable, CAMetalLayer};
use parking_lot::{Condvar, Mutex};
use pingpong_decode::DecodedFrame;
use pingpong_proto::clock;
use pingpong_proto::control::{CursorShape, HdrMetadata};

use crate::stats::StatsCollector;
use crate::stream::FrameTiming;

const SHADERS: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VOut { float4 pos [[position]]; float2 uv; };

vertex VOut v_full(uint vid [[vertex_id]]) {
    float2 p[3] = { float2(-1.0, -1.0), float2(3.0, -1.0), float2(-1.0, 3.0) };
    VOut o;
    o.pos = float4(p[vid], 0.0, 1.0);
    o.uv = float2((p[vid].x + 1.0) * 0.5, (1.0 - p[vid].y) * 0.5);
    return o;
}

struct VideoParams {
    // Where chroma is sampled from: half a luma pixel right for 4:2:0
    // sited left (H.264/HEVC type 0), nothing for 4:4:4.
    float2 chroma_offset;
    // A sample, normalized, back to its code value: 255 for 8-bit; 65535/64
    // for 10-bit held in the top bits of 16 (x420, x444).
    float code_scale;
    // Limited range: black and the span of Y, the middle and span of Cb/Cr.
    float y_black, y_range, c_mid, c_range;
    // The matrix's coefficients: R = Y + cr_r Cr, G = Y + cb_g Cb + cr_g Cr,
    // B = Y + cb_b Cb.
    float cr_r, cb_g, cr_g, cb_b;
};

// Limited range Y'CbCr -> R'G'B', the inverse of the host's converter:
// BT.709 for SDR; for HDR BT.2020, the result still PQ-encoded, which is
// what an HDR layer (BT.2100 PQ) takes.
fragment float4 f_video(VOut in [[stage_in]],
                        texture2d<float> luma [[texture(0)]],
                        texture2d<float> chroma [[texture(1)]],
                        constant VideoParams &p [[buffer(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float y = luma.sample(s, in.uv).r * p.code_scale;
    float2 c = chroma.sample(s, in.uv + p.chroma_offset).rg * p.code_scale;
    y = (y - p.y_black) / p.y_range;
    float cb = (c.x - p.c_mid) / p.c_range;
    float cr = (c.y - p.c_mid) / p.c_range;
    float3 rgb = float3(y + p.cr_r * cr, y + p.cb_g * cb + p.cr_g * cr, y + p.cb_b * cb);
    return float4(saturate(rgb), 1.0);
}

struct Quad { float4 rect; };

vertex VOut v_quad(uint vid [[vertex_id]], constant Quad &q [[buffer(0)]]) {
    float2 uv[4] = { float2(0.0, 0.0), float2(1.0, 0.0), float2(0.0, 1.0), float2(1.0, 1.0) };
    VOut o;
    o.uv = uv[vid];
    o.pos = float4(mix(q.rect.x, q.rect.z, uv[vid].x), mix(q.rect.y, q.rect.w, uv[vid].y), 0.0, 1.0);
    return o;
}

fragment float4 f_quad(VOut in [[stage_in]], texture2d<float> t [[texture(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    return t.sample(s, in.uv);
}

// SMPTE ST 2084 (PQ): absolute light, as a fraction of 10000 cd/m2, to signal.
float3 pq_encode(float3 l) {
    const float m1 = 0.1593017578125, m2 = 78.84375;
    const float c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
    float3 lm = pow(clamp(l, 0.0, 1.0), m1);
    return pow((c1 + c2 * lm) / (1.0 + c3 * lm), m2);
}

// An sRGB overlay (text, statistics) on an HDR layer: its white at the
// stream's SDR white, in BT.2020 and PQ like the picture beneath; blended
// premultiplied, as in SDR.
fragment float4 f_quad_hdr(VOut in [[stage_in]], texture2d<float> t [[texture(0)]],
                           constant float &sdr_white [[buffer(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float4 c = t.sample(s, in.uv);
    if (c.a <= 0.0) return float4(0.0);
    float3 e = c.rgb / c.a;
    float3 lin = select(pow((e + 0.055) / 1.055, 2.4), e / 12.92, e <= 0.04045);
    const float3x3 bt709_to_bt2020 = float3x3(float3(0.6274, 0.0691, 0.0164),
                                             float3(0.3293, 0.9195, 0.0880),
                                             float3(0.0433, 0.0114, 0.8956));
    float3 pq = pq_encode(bt709_to_bt2020 * lin * (sdr_white / 10000.0));
    return float4(pq * c.a, c.a);
}
"#;

/// A cursor image: premultiplied RGBA8, in pixels, with its hotspot.
#[derive(Clone)]
pub struct CursorImage {
    pub width: usize,
    pub height: usize,
    pub hot_x: f32,
    pub hot_y: f32,
    pub rgba: Vec<u8>,
}

pub use crate::pointer::CursorDraw;

/// Where the picture goes in the drawable.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Layout {
    pub drawable_w: f64,
    pub drawable_h: f64,
    /// Pixels at the top of the drawable that must stay clear (the notch).
    pub top_inset: f64,
    /// In a window (not full screen): the window server composites the
    /// frame, so it reaches the glass a refresh later (see `glass_limit`).
    pub windowed: bool,
}

/// What the rest of the client hands the render thread.
pub struct RenderShared {
    /// Decoded frames not yet shown: the newest only, or with frame pacing
    /// a short queue shown one per refresh.
    pending: Mutex<VecDeque<(DecodedFrame, FrameTiming)>>,
    timings: Mutex<Vec<FrameTiming>>,
    frame_ready: Condvar,
    new_frame: AtomicBool,
    cursor: Mutex<CursorDraw>,
    overlay_enabled: AtomicBool,
    /// What the stream is doing before the first frame ("Connecting…"),
    /// shown centred until video arrives, as Moonlight's progress dialog.
    status: Mutex<Option<String>>,
    /// Shown over the picture mid-stream ("Reconnecting…").
    notice: Mutex<Option<String>>,
    /// A connection warning, in the top-right corner.
    warning: Mutex<Option<String>>,
    dirty: AtomicBool,
    layout: Mutex<Layout>,
    stop: AtomicBool,
    stats: Arc<StatsCollector>,
    vsync: bool,
    /// Frame pacing: present on the display's refresh (see `VsyncClock`).
    paced: bool,
    vsync_ticks: Mutex<u64>,
    vsync_tick: Condvar,
    /// Drawables committed and not yet on the glass, and the signal that
    /// one got there. See `wait_for_glass`.
    glass: Arc<(Mutex<u32>, Condvar)>,
    /// The stream is HDR, with this metadata: the layer is BT.2100 PQ.
    hdr: Mutex<Option<HdrMetadata>>,
}

impl RenderShared {
    pub fn new(stats: Arc<StatsCollector>, vsync: bool, paced: bool) -> Arc<RenderShared> {
        Arc::new(RenderShared {
            pending: Mutex::new(VecDeque::with_capacity(PACING_QUEUE_MAX + 1)),
            timings: Mutex::new(Vec::with_capacity(16)),
            frame_ready: Condvar::new(),
            new_frame: AtomicBool::new(false),
            cursor: Mutex::new(CursorDraw {
                visible: false,
                absolute: false,
                x: 0.0,
                y: 0.0,
                shape: CursorShape::Arrow,
            }),
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
            glass: Arc::new((Mutex::new(0), Condvar::new())),
            hdr: Mutex::new(None),
        })
    }

    /// The stream is HDR (or no longer): the layer follows at the next draw.
    /// Until the host says more, its display is taken for a 1000 cd/m²
    /// BT.2020 one with SDR white at BT.2408's 203.
    pub fn set_hdr(&self, on: bool) {
        let mut hdr = self.hdr.lock();
        match (on, hdr.is_some()) {
            (true, false) => *hdr = Some(HdrMetadata::bt2020(1000, 203)),
            (false, true) => *hdr = None,
            _ => return,
        }
        self.dirty.store(true, Ordering::Release);
    }

    /// The host's HDR metadata, while the stream is HDR.
    pub fn set_hdr_metadata(&self, m: HdrMetadata) {
        let mut hdr = self.hdr.lock();
        if hdr.is_some_and(|h| h != m) {
            *hdr = Some(m);
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// Drawn frames that may wait for a refresh at once: one, full screen. In
    /// a window the window server composites each frame, up to two refreshes
    /// later, and only the layer's own three drawables keep up: at 120 fps a
    /// limit of two showed 80 frames a second, one showed 40 of 60 at 60.
    /// An HDR layer is composited full screen too (the window server maps
    /// it onto the display's headroom): with a limit of one, 58-104 of 118
    /// frames a second were shown at 3024x1890@120; with the layer's three,
    /// 116-120.
    fn glass_limit(&self) -> u32 {
        if self.layout.lock().windowed || self.hdr.lock().is_some() {
            DRAWABLES
        } else {
            GLASS_QUEUE_MAX
        }
    }

    /// With V-Sync, wait until fewer than `glass_limit` drawn frames are
    /// waiting for a refresh, so that the next one drawn is the next shown.
    ///
    /// The layer would otherwise queue up to its three drawables: when the
    /// stream's rate matches the display's (120 fps on a 120 Hz panel), a
    /// burst fills the queue and nothing ever drains it, each frame then
    /// shown two refreshes late (29 ms decode to glass at 120 Hz, against
    /// 10 ms at 60 fps, where the display drains it).
    fn wait_for_glass(&self) {
        if !self.vsync {
            return;
        }
        let limit = self.glass_limit();
        let (queued, reached_glass) = &*self.glass;
        let mut queued = queued.lock();
        while *queued >= limit {
            if reached_glass.wait_for(&mut queued, GLASS_WAIT).timed_out() {
                // A drawable the window server dropped unshown may never
                // report back: do not wait on it again.
                *queued = 0;
                break;
            }
        }
    }

    /// Remember a frame's network timing until it comes out of the decoder.
    pub fn expect(&self, timing: FrameTiming) {
        let mut t = self.timings.lock();
        if t.len() >= 16 {
            t.remove(0);
        }
        t.push(timing);
    }

    /// Called from VideoToolbox's callback. Without pacing the newest frame
    /// replaces any not yet shown (the older one would only be late); with
    /// it, frames queue to be shown one per refresh.
    pub fn push_frame(&self, frame: DecodedFrame) {
        let id = frame.capture_ts_us;
        let timing = {
            let t = self.timings.lock();
            t.iter()
                .rev()
                .find(|t| t.frame_id == id)
                .copied()
                .unwrap_or_default()
        };
        self.stats
            .decoded(frame.decoded_at_us.wrapping_sub(timing.reassembled_us));
        {
            let mut q = self.pending.lock();
            if !self.paced {
                q.clear();
            } else if q.len() >= PACING_QUEUE_MAX {
                q.pop_front();
            }
            q.push_back((frame, timing));
        }
        self.new_frame.store(true, Ordering::Release);
        self.frame_ready.notify_one();
    }

    pub fn set_cursor(&self, c: CursorDraw) {
        let mut cur = self.cursor.lock();
        if *cur != c {
            *cur = c;
            self.dirty.store(true, Ordering::Release);
            self.frame_ready.notify_one();
        }
    }

    pub fn cursor(&self) -> CursorDraw {
        *self.cursor.lock()
    }

    /// Show or hide the statistics overlay (Ctrl+Alt+Shift+S).
    pub fn toggle_overlay(&self) -> bool {
        let on = !self.overlay_enabled.fetch_xor(true, Ordering::AcqRel);
        self.dirty.store(true, Ordering::Release);
        self.frame_ready.notify_one();
        on
    }

    /// Show `text` until the first frame (None: nothing).
    pub fn set_status(&self, text: Option<String>) {
        *self.status.lock() = text;
        self.dirty.store(true, Ordering::Release);
        self.frame_ready.notify_one();
    }

    /// Show a connection warning in the corner until cleared (None).
    pub fn set_warning(&self, text: Option<String>) {
        *self.warning.lock() = text;
        self.dirty.store(true, Ordering::Release);
        self.frame_ready.notify_one();
    }

    /// Show `text` over the picture until cleared (None).
    pub fn set_notice(&self, text: Option<String>) {
        *self.notice.lock() = text;
        self.dirty.store(true, Ordering::Release);
        self.frame_ready.notify_one();
    }

    pub fn set_overlay(&self, on: bool) {
        self.overlay_enabled.store(on, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
        self.frame_ready.notify_one();
    }

    pub fn set_layout(&self, l: Layout) {
        let mut cur = self.layout.lock();
        if *cur != l {
            *cur = l;
            self.dirty.store(true, Ordering::Release);
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.frame_ready.notify_all();
    }
}

type Tex = Retained<ProtocolObject<dyn MTLTexture>>;

struct GpuCursor {
    texture: Tex,
    width: f32,
    height: f32,
    hot_x: f32,
    hot_y: f32,
}

/// The layer's format for SDR, and for HDR: 10 bits a channel, PQ-encoded
/// BT.2020 (the layer's colour space says so), as Moonlight's HDR renderer
/// (`vt_metal.mm`).
const SDR_FORMAT: MTLPixelFormat = MTLPixelFormat::BGRA8Unorm;
const HDR_FORMAT: MTLPixelFormat = MTLPixelFormat::BGR10A2Unorm;

/// What a decoded picture is, from its pixel format: 8-bit (`420v`,
/// `444v`) or 10-bit (`x420`, `x444`: HDR, BT.2020), 4:2:0 or 4:4:4.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Picture {
    ten_bit: bool,
}

impl Picture {
    fn of(image: &CVImageBuffer) -> Picture {
        let format = CVPixelBufferGetPixelFormatType(image);
        Picture {
            ten_bit: matches!(&format.to_be_bytes(), b"x420" | b"x444" | b"x422"),
        }
    }

    fn luma_format(self) -> MTLPixelFormat {
        if self.ten_bit {
            MTLPixelFormat::R16Unorm
        } else {
            MTLPixelFormat::R8Unorm
        }
    }

    fn chroma_format(self) -> MTLPixelFormat {
        if self.ten_bit {
            MTLPixelFormat::RG16Unorm
        } else {
            MTLPixelFormat::RG8Unorm
        }
    }

    /// The shader's `VideoParams`. Chroma as wide as luma is 4:4:4: sampled
    /// where it is; else half a luma pixel right (sited left).
    fn params(self, luma_width: usize, chroma_width: usize) -> [f32; 12] {
        let offset = if chroma_width >= luma_width {
            0.0
        } else {
            0.5 / luma_width.max(1) as f32
        };
        // (code scale, Y black, Y span, C middle, C span), then the matrix:
        // BT.709 for 8-bit SDR, BT.2020 (non-constant luminance) for HDR.
        let (range, matrix) = if self.ten_bit {
            (
                [65535.0 / 64.0, 64.0, 876.0, 512.0, 896.0],
                [1.4746, -0.164553, -0.571353, 1.8814],
            )
        } else {
            (
                [255.0, 16.0, 219.0, 128.0, 224.0],
                [1.5748, -0.1873, -0.4681, 1.8556],
            )
        };
        [
            offset, 0.0, range[0], range[1], range[2], range[3], range[4], matrix[0], matrix[1],
            matrix[2], matrix[3], 0.0,
        ]
    }
}

/// The layer set up for an HDR stream with `hdr`'s metadata, or for SDR.
/// macOS tone-maps by the metadata, as with Moonlight (`vt_metal.mm`), which
/// maps 203 cd/m² to the display's SDR white (EDR 1.0); here the host's own
/// SDR white goes there, so its desktop is as bright as this one's, and
/// brighter goes into the display's headroom. (A screenshot is macOS's SDR
/// rendition, tone-mapped into no headroom at all: white comes out grey in
/// it, whatever the metadata says the peak is.)
fn configure_layer(layer: &CAMetalLayer, hdr: Option<HdrMetadata>) {
    use objc2_core_graphics::{kCGColorSpaceITUR_2100_PQ, CGColorSpace};
    use objc2_foundation::NSData;
    match hdr {
        Some(m) => {
            layer.setPixelFormat(HDR_FORMAT);
            // SAFETY: a CoreGraphics constant.
            let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceITUR_2100_PQ }));
            layer.setColorspace(space.as_deref());
            layer.setWantsExtendedDynamicRangeContent(true);
            let (display, content) = edr_metadata(m);
            let white = if m.sdr_white > 0 { m.sdr_white } else { 203 };
            let edr = CAEDRMetadata::HDR10MetadataWithDisplayInfo_contentInfo_opticalOutputScale(
                Some(&NSData::with_bytes(&display)),
                Some(&NSData::with_bytes(&content)),
                white as f32,
            );
            layer.setEDRMetadata(Some(&edr));
            tracing::info!(
                max_nits = m.max_luminance,
                sdr_white = white,
                "the stream is HDR: the layer is BT.2100 PQ"
            );
        }
        None => {
            layer.setEDRMetadata(None);
            layer.setWantsExtendedDynamicRangeContent(false);
            layer.setColorspace(None);
            layer.setPixelFormat(SDR_FORMAT);
        }
    }
}

/// The metadata as Core Animation takes it: the mastering display's colour
/// volume and the content's light levels, each as its SEI message would
/// carry it (big-endian; primaries green, blue, red; luminance in
/// 0.0001 cd/m²).
fn edr_metadata(m: HdrMetadata) -> ([u8; 24], [u8; 4]) {
    let mut display = [0u8; 24];
    let [r, g, b] = m.primaries;
    let values = [
        g[0],
        g[1],
        b[0],
        b[1],
        r[0],
        r[1],
        m.white_point[0],
        m.white_point[1],
    ];
    for (i, v) in values.iter().enumerate() {
        display[i * 2..i * 2 + 2].copy_from_slice(&v.to_be_bytes());
    }
    display[16..20].copy_from_slice(&(m.max_luminance as u32 * 10_000).to_be_bytes());
    display[20..24].copy_from_slice(&(m.min_luminance as u32).to_be_bytes());
    let mut content = [0u8; 4];
    content[..2].copy_from_slice(&m.max_cll.to_be_bytes());
    content[2..].copy_from_slice(&m.max_fall.to_be_bytes());
    (display, content)
}

/// Everything that must outlive a command buffer. The textures are dropped
/// before the cache they came from: releasing one reaches into the cache,
/// and the renderer's own reference can be gone by then (a command buffer
/// completing just after the renderer stopped crashed in
/// `CVMetalTextureCache::bufferBackingNotInUse`).
struct KeepAlive {
    _frame: DecodedFrame,
    _textures: [CFRetained<CVMetalTexture>; 2],
    _cache: CFRetained<CVMetalTextureCache>,
}

pub struct Renderer {
    layer: Retained<CAMetalLayer>,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    video: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    quad: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// The same, drawing into an HDR layer.
    video_hdr: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    quad_hdr: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    /// What the layer is set up for: HDR with this metadata, or SDR.
    hdr: Option<HdrMetadata>,
    cache: CFRetained<CVMetalTextureCache>,
    cursors: HashMap<u8, GpuCursor>,
    overlay: Option<(String, Tex, f32, f32)>,
    overlay_at: Option<std::time::Instant>,
    status: Option<(String, Tex, f32, f32)>,
    notice: Option<(String, Tex, f32, f32)>,
    /// Refreshes in a row the pacing queue has held more than one frame.
    backlog: u32,
    warning: Option<(String, Tex, f32, f32)>,
    current: Option<(DecodedFrame, FrameTiming)>,
    scale: f64,
}

fn pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    library: &ProtocolObject<dyn MTLLibrary>,
    vertex: &str,
    fragment: &str,
    blend: bool,
    format: MTLPixelFormat,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let desc = MTLRenderPipelineDescriptor::new();
    let v = library
        .newFunctionWithName(&NSString::from_str(vertex))
        .ok_or("missing vertex function")?;
    let f = library
        .newFunctionWithName(&NSString::from_str(fragment))
        .ok_or("missing fragment function")?;
    desc.setVertexFunction(Some(&v));
    desc.setFragmentFunction(Some(&f));
    let color = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
    color.setPixelFormat(format);
    if blend {
        color.setBlendingEnabled(true);
        color.setSourceRGBBlendFactor(MTLBlendFactor::One);
        color.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        color.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        color.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    }
    device
        .newRenderPipelineStateWithDescriptor_error(&desc)
        .map_err(|e| format!("pipeline: {e}"))
}

impl Renderer {
    pub fn new(
        layer: Retained<CAMetalLayer>,
        cursors: &[(CursorShape, CursorImage)],
        vsync: bool,
        scale: f64,
    ) -> Result<Renderer, String> {
        let device = MTLCreateSystemDefaultDevice().ok_or("no Metal device")?;
        layer.setDevice(Some(&device));
        layer.setPixelFormat(SDR_FORMAT);
        layer.setFramebufferOnly(true);
        layer.setDisplaySyncEnabled(vsync);
        // How many of them may queue for the glass is `wait_for_glass`'s.
        layer.setMaximumDrawableCount((if vsync { DRAWABLES } else { 2 }) as usize);
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADERS), None)
            .map_err(|e| format!("shaders: {e}"))?;
        let video = pipeline(&device, &library, "v_full", "f_video", false, SDR_FORMAT)?;
        let quad = pipeline(&device, &library, "v_quad", "f_quad", true, SDR_FORMAT)?;
        let video_hdr = pipeline(&device, &library, "v_full", "f_video", false, HDR_FORMAT)?;
        let quad_hdr = pipeline(&device, &library, "v_quad", "f_quad_hdr", true, HDR_FORMAT)?;
        let queue = device.newCommandQueue().ok_or("no command queue")?;

        let mut cache: *mut CVMetalTextureCache = std::ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCache::create(
                None,
                None,
                &device,
                None,
                NonNull::new(&mut cache as *mut _).unwrap(),
            )
        };
        if status != 0 || cache.is_null() {
            return Err(format!("CVMetalTextureCacheCreate: {status}"));
        }
        let cache = unsafe { CFRetained::from_raw(NonNull::new(cache).unwrap()) };

        let mut renderer = Renderer {
            layer,
            device,
            queue,
            video,
            quad,
            video_hdr,
            quad_hdr,
            hdr: None,
            cache,
            cursors: HashMap::new(),
            overlay: None,
            overlay_at: None,
            status: None,
            notice: None,
            backlog: 0,
            warning: None,
            current: None,
            scale,
        };
        for (shape, img) in cursors {
            if let Some(texture) = renderer.upload(img.width, img.height, &img.rgba) {
                renderer.cursors.insert(
                    *shape as u8,
                    GpuCursor {
                        texture,
                        width: img.width as f32,
                        height: img.height as f32,
                        hot_x: img.hot_x,
                        hot_y: img.hot_y,
                    },
                );
            }
        }
        Ok(renderer)
    }

    fn upload(&self, width: usize, height: usize, rgba: &[u8]) -> Option<Tex> {
        if width == 0 || height == 0 {
            return None;
        }
        let desc = unsafe {
            MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                MTLPixelFormat::RGBA8Unorm,
                width,
                height,
                false,
            )
        };
        desc.setUsage(MTLTextureUsage::ShaderRead);
        let tex = self.device.newTextureWithDescriptor(&desc)?;
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width,
                height,
                depth: 1,
            },
        };
        unsafe {
            tex.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::new(rgba.as_ptr() as *mut c_void).unwrap(),
                width * 4,
            );
        }
        Some(tex)
    }

    fn plane(
        &self,
        image: &CVImageBuffer,
        plane: usize,
        format: MTLPixelFormat,
    ) -> Option<CFRetained<CVMetalTexture>> {
        let width = CVPixelBufferGetWidthOfPlane(image, plane);
        let height = CVPixelBufferGetHeightOfPlane(image, plane);
        let mut texture: *mut CVMetalTexture = std::ptr::null_mut();
        let status = unsafe {
            CVMetalTextureCache::create_texture_from_image(
                None,
                &self.cache,
                image,
                None,
                format,
                width,
                height,
                plane,
                NonNull::new(&mut texture as *mut _).unwrap(),
            )
        };
        if status != 0 {
            return None;
        }
        NonNull::new(texture).map(|t| unsafe { CFRetained::from_raw(t) })
    }

    /// Draw the current state into `drawable` and present it.
    /// `shown`: a new frame being presented, as (decoded at, our clock;
    /// captured at, the host's), whose trip to the glass is measured when the
    /// display reports it.
    fn draw(
        &mut self,
        drawable: &ProtocolObject<dyn CAMetalDrawable>,
        shared: &RenderShared,
        target_time: Option<f64>,
        shown: Option<(u32, u32)>,
    ) {
        // An HDR stream (or no longer): the layer follows, for the drawables
        // after this one.
        let hdr = *shared.hdr.lock();
        if hdr != self.hdr {
            configure_layer(&self.layer, hdr);
            self.hdr = hdr;
        }
        let layout = *shared.layout.lock();
        let target = drawable.texture();
        let (dw, dh) = (target.width() as f64, target.height() as f64);

        let pass = MTLRenderPassDescriptor::new();
        let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
        attachment.setTexture(Some(&target));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        attachment.setClearColor(MTLClearColor {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 1.0,
        });
        let Some(cmd) = self.queue.commandBuffer() else {
            return;
        };
        let Some(enc) = cmd.renderCommandEncoderWithDescriptor(&pass) else {
            return;
        };

        // The drawable's format says which pipelines draw into it (the layer
        // may have changed format since it was taken).
        let hdr_target = target.pixelFormat() == HDR_FORMAT;
        let (video_pipeline, quad_pipeline) = if hdr_target {
            (&self.video_hdr, &self.quad_hdr)
        } else {
            (&self.video, &self.quad)
        };
        let sdr_white: f32 = self.hdr.map_or(
            203.0,
            |m| if m.sdr_white > 0 { m.sdr_white } else { 203 } as f32,
        );

        let mut keep: Option<KeepAlive> = None;
        let mut video_rect = (0.0, 0.0, dw, dh);
        if let Some((frame, _)) = &self.current {
            let image: &CVImageBuffer = unsafe { &*(frame.pixel_buffer as *const CVImageBuffer) };
            let picture = Picture::of(image);
            if let (Some(luma), Some(chroma)) = (
                self.plane(image, 0, picture.luma_format()),
                self.plane(image, 1, picture.chroma_format()),
            ) {
                if let (Some(lt), Some(ct)) = (
                    CVMetalTextureGetTexture(&luma),
                    CVMetalTextureGetTexture(&chroma),
                ) {
                    // Fit inside the area below the notch, preserving aspect.
                    let avail_y = layout.top_inset.min(dh);
                    let (aw, ah) = (dw, dh - avail_y);
                    let (vw, vh) = (frame.width as f64, frame.height as f64);
                    let s = (aw / vw).min(ah / vh);
                    let (w, h) = ((vw * s).round(), (vh * s).round());
                    let x = ((aw - w) / 2.0).round();
                    let y = avail_y + ((ah - h) / 2.0).round();
                    video_rect = (x, y, w, h);
                    enc.setViewport(MTLViewport {
                        originX: x,
                        originY: y,
                        width: w,
                        height: h,
                        znear: 0.0,
                        zfar: 1.0,
                    });
                    enc.setRenderPipelineState(video_pipeline);
                    let params = picture.params(lt.width(), ct.width());
                    unsafe {
                        enc.setFragmentBytes_length_atIndex(
                            NonNull::new(params.as_ptr() as *mut c_void).unwrap(),
                            std::mem::size_of_val(&params),
                            0,
                        );
                        enc.setFragmentTexture_atIndex(Some(&lt), 0);
                        enc.setFragmentTexture_atIndex(Some(&ct), 1);
                        enc.drawPrimitives_vertexStart_vertexCount(
                            MTLPrimitiveType::Triangle,
                            0,
                            3,
                        );
                    }
                    keep = Some(KeepAlive {
                        _frame: frame.clone_ref(),
                        _textures: [luma, chroma],
                        _cache: self.cache.clone(),
                    });
                }
            }
        }

        // Overlays in full-drawable NDC.
        enc.setViewport(MTLViewport {
            originX: 0.0,
            originY: 0.0,
            width: dw,
            height: dh,
            znear: 0.0,
            zfar: 1.0,
        });
        let ndc = |x: f64, y: f64| -> (f32, f32) {
            ((x / dw * 2.0 - 1.0) as f32, (1.0 - y / dh * 2.0) as f32)
        };
        let quad = |tex: &Tex, x: f64, y: f64, w: f64, h: f64| {
            let (x0, y0) = ndc(x, y);
            let (x1, y1) = ndc(x + w, y + h);
            let rect: [f32; 4] = [x0, y0, x1, y1];
            enc.setRenderPipelineState(quad_pipeline);
            unsafe {
                enc.setVertexBytes_length_atIndex(
                    NonNull::new(rect.as_ptr() as *mut c_void).unwrap(),
                    16,
                    0,
                );
                if hdr_target {
                    enc.setFragmentBytes_length_atIndex(
                        NonNull::new(&sdr_white as *const f32 as *mut c_void).unwrap(),
                        4,
                        0,
                    );
                }
                enc.setFragmentTexture_atIndex(Some(tex), 0);
                enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
            }
        };

        let cursor = *shared.cursor.lock();
        if cursor.visible && self.current.is_some() {
            let (vx, vy, vw, vh) = video_rect;
            let (sw, sh) = self
                .current
                .as_ref()
                .map(|(f, _)| (f.width as f64, f.height as f64))
                .unwrap();
            let c = self
                .cursors
                .get(&(cursor.shape as u8))
                .or_else(|| self.cursors.get(&(CursorShape::Arrow as u8)));
            if let Some(c) = c {
                let px = vx + cursor.x as f64 * vw / sw;
                let py = vy + cursor.y as f64 * vh / sh;
                quad(
                    &c.texture,
                    px - c.hot_x as f64,
                    py - c.hot_y as f64,
                    c.width as f64,
                    c.height as f64,
                );
            }
        }

        if self.current.is_none() {
            let text = shared.status.lock().clone();
            match text {
                Some(text) => {
                    if self.status.as_ref().map(|s| &s.0) != Some(&text) {
                        self.status = super::text::rasterize(&text, self.scale).and_then(|b| {
                            self.upload(b.width, b.height, &b.rgba)
                                .map(|t| (text.clone(), t, b.width as f32, b.height as f32))
                        });
                    }
                    if let Some((_, tex, w, h)) = &self.status {
                        let (w, h) = (*w as f64, *h as f64);
                        quad(
                            tex,
                            ((dw - w) / 2.0).round(),
                            ((dh - h) / 2.0).round(),
                            w,
                            h,
                        );
                    }
                }
                None => self.status = None,
            }
        } else {
            self.status = None;
            let text = shared.notice.lock().clone();
            match text {
                Some(text) => {
                    if self.notice.as_ref().map(|s| &s.0) != Some(&text) {
                        self.notice = super::text::rasterize(&text, self.scale).and_then(|b| {
                            self.upload(b.width, b.height, &b.rgba)
                                .map(|t| (text.clone(), t, b.width as f32, b.height as f32))
                        });
                    }
                    if let Some((_, tex, w, h)) = &self.notice {
                        let (w, h) = (*w as f64, *h as f64);
                        quad(
                            tex,
                            ((dw - w) / 2.0).round(),
                            ((dh - h) / 2.0).round(),
                            w,
                            h,
                        );
                    }
                }
                None => self.notice = None,
            }
        }

        if shared.overlay_enabled.load(Ordering::Acquire) {
            // Refreshed once a second, like the stats window it shows.
            if self
                .overlay_at
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
            {
                self.overlay_at = Some(std::time::Instant::now());
                let text = shared.stats.snapshot().overlay_text();
                if self.overlay.as_ref().map(|o| &o.0) != Some(&text) {
                    self.overlay = super::text::rasterize(&text, self.scale).and_then(|b| {
                        self.upload(b.width, b.height, &b.rgba)
                            .map(|t| (text.clone(), t, b.width as f32, b.height as f32))
                    });
                }
            }
            if let Some((_, tex, w, h)) = &self.overlay {
                let margin = 12.0 * self.scale;
                quad(tex, margin, layout.top_inset + margin, *w as f64, *h as f64);
            }
        } else {
            self.overlay = None;
            self.overlay_at = None;
        }

        let warning = shared.warning.lock().clone();
        match warning {
            Some(text) if self.current.is_some() => {
                if self.warning.as_ref().map(|s| &s.0) != Some(&text) {
                    self.warning = super::text::rasterize(&text, self.scale).and_then(|b| {
                        self.upload(b.width, b.height, &b.rgba)
                            .map(|t| (text.clone(), t, b.width as f32, b.height as f32))
                    });
                }
                if let Some((_, tex, w, h)) = &self.warning {
                    let margin = 12.0 * self.scale;
                    quad(
                        tex,
                        dw - *w as f64 - margin,
                        layout.top_inset + margin,
                        *w as f64,
                        *h as f64,
                    );
                }
            }
            _ => self.warning = None,
        }

        enc.endEncoding();
        // The display link has already scheduled its drawable for the target
        // vsync; presentAtTime is refused (CAMetalDrawableInvalidOperation).
        let _ = target_time;
        if shared.vsync {
            let glass = shared.glass.clone();
            *glass.0.lock() += 1;
            let block = RcBlock::new(move |_d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
                let (queued, reached_glass) = &*glass;
                let mut queued = queued.lock();
                *queued = queued.saturating_sub(1);
                reached_glass.notify_all();
            });
            unsafe { drawable.addPresentedHandler(RcBlock::as_ptr(&block)) };
        }
        if let Some((decoded_at, captured_at)) = shown {
            // Decoded -> on the glass, as the display reports it: includes the
            // wait for the refresh and, in a window, the compositor.
            let stats = shared.stats.clone();
            let block = RcBlock::new(move |d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
                let at = unsafe { d.as_ref() }.presentedTime();
                if at > 0.0 {
                    let ago_us = ((CACurrentMediaTime() - at).max(0.0) * 1e6) as u32;
                    let on_glass = clock::now_us().wrapping_sub(ago_us);
                    stats.presented(on_glass.wrapping_sub(decoded_at));
                    if let Some(captured) = stats.host_to_local(captured_at) {
                        let us = on_glass.wrapping_sub(captured) as i32;
                        if (0..1_000_000).contains(&us) {
                            stats.end_to_end(us as u32);
                        }
                    }
                }
            });
            unsafe { drawable.addPresentedHandler(RcBlock::as_ptr(&block)) };
        }
        cmd.presentDrawable(ProtocolObject::from_ref(drawable));
        if let Some(keep) = keep {
            let keep = std::cell::Cell::new(Some(keep));
            let block = RcBlock::new(move |_cb: NonNull<ProtocolObject<dyn MTLCommandBuffer>>| {
                drop(keep.take());
            });
            unsafe { cmd.addCompletedHandler(RcBlock::as_ptr(&block)) };
        }
        cmd.commit();
        self.cache.flush(0);
    }

    /// The statistics overlay is on and its once-a-second refresh is due.
    fn overlay_due(&self, shared: &RenderShared) -> bool {
        shared.overlay_enabled.load(Ordering::Acquire)
            && self
                .overlay_at
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1))
    }

    /// Something to draw: a frame, a change, or the overlay's refresh.
    fn has_work(&self, shared: &RenderShared) -> bool {
        shared.new_frame.load(Ordering::Acquire)
            || shared.dirty.load(Ordering::Acquire)
            || self.overlay_due(shared)
    }

    fn render(
        &mut self,
        drawable: &ProtocolObject<dyn CAMetalDrawable>,
        shared: &RenderShared,
        target_time: Option<f64>,
    ) {
        let fresh = if shared.new_frame.load(Ordering::Acquire) {
            self.take_frame(shared)
        } else {
            None
        };
        let overlay_due = self.overlay_due(shared);
        let dirty = shared.dirty.swap(false, Ordering::AcqRel) || overlay_due;
        if fresh.is_none() && !dirty {
            return;
        }
        let presented = fresh
            .as_ref()
            .map(|(f, t)| (f.decoded_at_us, t.captured_us));
        if let Some(f) = fresh {
            self.current = Some(f);
        }
        self.draw(drawable, shared, target_time, presented);
    }

    /// The frame to show now. Paced, Moonlight's way: the oldest waiting, one
    /// per refresh, so frames that network jitter delivered together are
    /// still each shown; but a queue that stands for `PACING_BACKLOG_TICKS`
    /// is latency, and is skipped to its newest frame.
    fn take_frame(&mut self, shared: &RenderShared) -> Option<(DecodedFrame, FrameTiming)> {
        let mut q = shared.pending.lock();
        let frame = if shared.paced {
            self.backlog = if q.len() > 1 { self.backlog + 1 } else { 0 };
            if self.backlog >= PACING_BACKLOG_TICKS {
                self.backlog = 0;
                while q.len() > 1 {
                    q.pop_front();
                }
            }
            q.pop_front()
        } else {
            let newest = q.pop_back();
            q.clear();
            newest
        };
        shared.new_frame.store(!q.is_empty(), Ordering::Release);
        frame
    }

    /// Keep the layer's drawable in step with the layout the window published.
    fn sync_size(&self, shared: &RenderShared) {
        let l = *shared.layout.lock();
        if l.drawable_w > 0.0 && l.drawable_h > 0.0 {
            let cur = self.layer.drawableSize();
            if cur.width != l.drawable_w || cur.height != l.drawable_h {
                self.layer.setDrawableSize(CGSize {
                    width: l.drawable_w,
                    height: l.drawable_h,
                });
            }
        }
    }
}

/// Frames the pacing queue holds at most (older ones are dropped).
const PACING_QUEUE_MAX: usize = 3;
/// The layer's drawables with V-Sync: three, which the window server needs
/// to take a full-screen layer straight to the display.
const DRAWABLES: u32 = 3;
/// Drawn frames that may wait for a refresh at once, full screen (see
/// `wait_for_glass`). On a refresh tick (frame pacing) one more: the frame
/// shown at that refresh reports being on the glass only just after it.
const GLASS_QUEUE_MAX: u32 = 1;
/// Longer than a refresh at any rate the stream runs at.
const GLASS_WAIT: Duration = Duration::from_millis(40);
/// Refreshes a pacing queue may stand before it is skipped to the newest.
const PACING_BACKLOG_TICKS: u32 = 10;

// ---------------------------------------------------------------------------
// Frame pacing's clock: the display's refresh, as Moonlight's pacer uses it.

/// Ticks `RenderShared::vsync` once per refresh of the main display.
struct VsyncClock {
    link: CFRetained<CVDisplayLink>,
    shared: *const RenderShared,
}

impl VsyncClock {
    // CVDisplayLink is deprecated for NSView/NSScreen display links, which
    // need an Objective-C target on a run loop; this needs only a tick.
    #[allow(deprecated)]
    fn start(shared: Arc<RenderShared>) -> Option<VsyncClock> {
        unsafe {
            let mut raw: *mut CVDisplayLink = std::ptr::null_mut();
            if CVDisplayLink::create_with_active_cg_displays(NonNull::from(&mut raw)) != 0 {
                return None;
            }
            let link = CFRetained::from_raw(NonNull::new(raw)?);
            link.set_current_cg_display(objc2_core_graphics::CGMainDisplayID());
            let shared = Arc::into_raw(shared);
            link.set_output_callback(Some(on_vsync), shared as *mut c_void);
            if link.start() != 0 {
                drop(Arc::from_raw(shared));
                return None;
            }
            Some(VsyncClock { link, shared })
        }
    }
}

impl Drop for VsyncClock {
    #[allow(deprecated)]
    fn drop(&mut self) {
        self.link.stop();
        unsafe { drop(Arc::from_raw(self.shared)) };
    }
}

unsafe extern "C-unwind" fn on_vsync(
    _link: NonNull<CVDisplayLink>,
    _now: NonNull<objc2_core_video::CVTimeStamp>,
    _output: NonNull<objc2_core_video::CVTimeStamp>,
    _flags: objc2_core_video::CVOptionFlags,
    _flags_out: NonNull<objc2_core_video::CVOptionFlags>,
    ctx: *mut c_void,
) -> objc2_core_video::CVReturn {
    let shared = unsafe { &*(ctx as *const RenderShared) };
    *shared.vsync_ticks.lock() += 1;
    shared.vsync_tick.notify_one();
    0
}

/// Run the renderer until `shared.stop()`. Call on a dedicated thread.
pub fn run(
    layer: Retained<CAMetalLayer>,
    shared: Arc<RenderShared>,
    cursors: Vec<(CursorShape, CursorImage)>,
    scale: f64,
) {
    let renderer = match Renderer::new(layer.clone(), &cursors, shared.vsync, scale) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "renderer failed to start");
            return;
        }
    };
    let clock = if shared.paced {
        VsyncClock::start(shared.clone())
    } else {
        None
    };
    if shared.paced && clock.is_none() {
        tracing::warn!("no display clock; presenting frames as they arrive");
    }
    if clock.is_some() {
        // Frame pacing: on each refresh, show the newest frame. It reaches
        // the glass at the next one, so a frame waits half a refresh on
        // average for its tick, and frames arriving together never queue.
        let mut renderer = renderer;
        let mut seen = 0u64;
        while !shared.stop.load(Ordering::Acquire) {
            {
                let mut ticks = shared.vsync_ticks.lock();
                if *ticks == seen {
                    shared
                        .vsync_tick
                        .wait_for(&mut ticks, Duration::from_millis(50));
                }
                seen = *ticks;
            }
            autoreleasepool(|_| {
                renderer.sync_size(&shared);
                let due = renderer.has_work(&shared);
                // Besides the frame going on the glass at this refresh, as many
                // still waiting as `glass_limit`: this tick's would only queue
                // behind them.
                let clear = *shared.glass.0.lock() < shared.glass_limit() + 1;
                if due && clear {
                    if let Some(drawable) = renderer.layer.nextDrawable() {
                        renderer.render(&drawable, &shared, None);
                    }
                }
            });
        }
    } else {
        let mut renderer = renderer;
        while !shared.stop.load(Ordering::Acquire) {
            {
                let mut guard = shared.pending.lock();
                if guard.is_empty() && !shared.dirty.load(Ordering::Acquire) {
                    shared
                        .frame_ready
                        .wait_for(&mut guard, Duration::from_millis(50));
                }
            }
            // A drawable only for something to draw (not every timeout).
            if !renderer.has_work(&shared) {
                continue;
            }
            // Before taking the frame, so that it is the newest when drawn.
            shared.wait_for_glass();
            autoreleasepool(|_| {
                renderer.sync_size(&shared);
                if let Some(drawable) = renderer.layer.nextDrawable() {
                    renderer.render(&drawable, &shared, None);
                }
            });
        }
    }
}
