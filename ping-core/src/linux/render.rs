//! Presenting decoded pictures with wgpu (Vulkan, or GL) on Linux.
//!
//! Pictures arrive in system memory (see `pingpong_decode::ffmpeg`) and are
//! uploaded straight from the decode thread into one of a few textures of
//! ours; the render thread draws the newest, converting BT.709 (limited
//! range) to RGB as the other platforms' shaders do. V-Sync presents in
//! mailbox mode where the driver has it (no tearing, no queue), else FIFO
//! with a frame latency of one; without V-Sync, immediately.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use pingpong_decode::ffmpeg::{Layout as Planes, Picture};
use pingpong_proto::clock;

use crate::stats::StatsCollector;
use crate::stream::FrameTiming;

const SHADERS: &str = r#"
struct VOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };

@vertex fn v_full(@builtin(vertex_index) i: u32) -> VOut {
    let x = select(-1.0, 3.0, i == 1u);
    let y = select(-1.0, 3.0, i == 2u);
    var o: VOut;
    o.pos = vec4<f32>(x, y, 0.0, 1.0);
    o.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return o;
}

struct Video { chroma_offset: vec2<f32>, uv_scale: vec2<f32>, planar: u32, srgb: u32, pad0: u32, pad1: u32 };
@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var<uniform> video: Video;
@group(0) @binding(2) var tex_y: texture_2d<f32>;
@group(0) @binding(3) var tex_u: texture_2d<f32>;
@group(0) @binding(4) var tex_v: texture_2d<f32>;

// To the surface: as is, or linearised for an sRGB surface (which encodes
// again on write).
fn out(rgb: vec3<f32>, srgb: u32) -> vec3<f32> {
    if (srgb == 1u) {
        return select(pow((rgb + 0.055) / 1.055, vec3<f32>(2.4)), rgb / 12.92, rgb <= vec3<f32>(0.04045));
    }
    return rgb;
}

// BT.709, limited range -- the inverse of the host's converter. Chroma is
// sited left (H.264/HEVC type 0): sampled half a luma pixel to the right.
@fragment fn f_video(in: VOut) -> @location(0) vec4<f32> {
    let uv = in.uv * video.uv_scale;
    let cuv = uv + video.chroma_offset;
    var y = textureSample(tex_y, samp, uv).r;
    let u = textureSample(tex_u, samp, cuv);
    let v = textureSample(tex_v, samp, cuv).r;
    var c = u.rg;
    if (video.planar == 1u) {
        c = vec2<f32>(u.r, v);
    }
    y = (y - 16.0 / 255.0) * (255.0 / 219.0);
    let cb = (c.x - 128.0 / 255.0) * (255.0 / 224.0);
    let cr = (c.y - 128.0 / 255.0) * (255.0 / 224.0);
    let rgb = vec3<f32>(y + 1.5748 * cr, y - 0.1873 * cb - 0.4681 * cr, y + 1.8556 * cb);
    return vec4<f32>(out(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), video.srgb), 1.0);
}

struct Quad { rect: vec4<f32>, srgb: u32, pad0: u32, pad1: u32, pad2: u32 };
@group(0) @binding(0) var qsamp: sampler;
@group(0) @binding(1) var<uniform> quad: Quad;
@group(0) @binding(2) var tex: texture_2d<f32>;

@vertex fn v_quad(@builtin(vertex_index) i: u32) -> VOut {
    let uv = vec2<f32>(f32(i & 1u), f32(i >> 1u));
    var o: VOut;
    o.uv = uv;
    o.pos = vec4<f32>(mix(quad.rect.x, quad.rect.z, uv.x), mix(quad.rect.y, quad.rect.w, uv.y), 0.0, 1.0);
    return o;
}

@fragment fn f_quad(in: VOut) -> @location(0) vec4<f32> {
    let t = textureSample(tex, qsamp, in.uv);
    return vec4<f32>(out(t.rgb, quad.srgb), t.a);
}
"#;

/// The device decoding uploads and presentation share.
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub format: wgpu::TextureFormat,
    video_layout: wgpu::BindGroupLayout,
    quad_layout: wgpu::BindGroupLayout,
    video_pipeline: wgpu::RenderPipeline,
    quad_pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    /// Stands in for the third plane of an NV12 picture.
    dummy: wgpu::TextureView,
}

fn texture(
    device: &wgpu::Device,
    w: u32,
    h: u32,
    format: wgpu::TextureFormat,
    label: &str,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: w.max(1),
            height: h.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

impl Gpu {
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, format: wgpu::TextureFormat) -> Gpu {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ping"),
            source: wgpu::ShaderSource::Wgsl(SHADERS.into()),
        });
        let entry = |binding: u32, ty: wgpu::BindingType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty,
            count: None,
        };
        let sampler_ty = wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering);
        let uniform_ty = wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        };
        let tex_ty = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let video_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video"),
            entries: &[
                entry(0, sampler_ty),
                entry(1, uniform_ty),
                entry(2, tex_ty),
                entry(3, tex_ty),
                entry(4, tex_ty),
            ],
        });
        let quad_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad"),
            entries: &[entry(0, sampler_ty), entry(1, uniform_ty), entry(2, tex_ty)],
        });
        let pipeline = |layout: &wgpu::BindGroupLayout,
                        vs: &str,
                        fs: &str,
                        topology: wgpu::PrimitiveTopology,
                        blend: Option<wgpu::BlendState>| {
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(fs),
                layout: Some(&pl),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some(vs),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(fs),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology,
                    cull_mode: None,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let video_pipeline = pipeline(
            &video_layout,
            "v_full",
            "f_video",
            wgpu::PrimitiveTopology::TriangleList,
            None,
        );
        let quad_pipeline = pipeline(
            &quad_layout,
            "v_quad",
            "f_quad",
            wgpu::PrimitiveTopology::TriangleStrip,
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("linear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let dummy = texture(&device, 1, 1, wgpu::TextureFormat::R8Unorm, "dummy")
            .create_view(&Default::default());
        Gpu {
            device,
            queue,
            format,
            video_layout,
            quad_layout,
            video_pipeline,
            quad_pipeline,
            sampler,
            dummy,
        }
    }

    fn srgb(&self) -> u32 {
        self.format.is_srgb() as u32
    }
}

/// The drawable, as the window says it is.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Layout {
    pub width: u32,
    pub height: u32,
    /// Pixels per point.
    pub scale: f64,
}

struct Slot {
    planar: bool,
    full_chroma: bool,
    width: u32,
    height: u32,
    _uniform: wgpu::Buffer,
    bind: wgpu::BindGroup,
    busy: bool,
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: Option<wgpu::Texture>,
}

#[derive(Clone, Copy)]
struct Shown {
    slot: usize,
    timing: FrameTiming,
    decoded_at: u32,
    width: u32,
    height: u32,
}

#[derive(Default)]
struct Frames {
    slots: Vec<Option<Slot>>,
    pending: Option<Shown>,
    current: Option<Shown>,
}

/// Our textures: the one on screen, the one waiting, the one being filled.
const POOL: usize = 3;

/// What the rest of the client hands the render thread.
pub struct RenderShared {
    pub gpu: Arc<Gpu>,
    frames: Mutex<Frames>,
    timings: Mutex<Vec<FrameTiming>>,
    frame_ready: Condvar,
    overlay_enabled: AtomicBool,
    status: Mutex<Option<String>>,
    notice: Mutex<Option<String>>,
    warning: Mutex<Option<String>>,
    dirty: AtomicBool,
    layout: Mutex<Layout>,
    stop: AtomicBool,
    stats: Arc<StatsCollector>,
    pub vsync: bool,
    snapshot: Mutex<Option<(std::path::PathBuf, u32)>>,
}

impl RenderShared {
    pub fn new(gpu: Arc<Gpu>, stats: Arc<StatsCollector>, vsync: bool) -> Arc<RenderShared> {
        // PING_TEST_SNAPSHOT=PATH[@FRAMES]: save what is drawn as a PNG after
        // that many frames (default 120), for tests without eyes.
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
            overlay_enabled: AtomicBool::new(false),
            status: Mutex::new(None),
            notice: Mutex::new(None),
            warning: Mutex::new(None),
            dirty: AtomicBool::new(true),
            layout: Mutex::new(Layout::default()),
            stop: AtomicBool::new(false),
            stats,
            vsync,
            snapshot: Mutex::new(snapshot),
        })
    }

    fn poke(&self) {
        self.dirty.store(true, Ordering::Release);
        self.frame_ready.notify_one();
    }

    pub fn expect(&self, timing: FrameTiming) {
        let mut t = self.timings.lock();
        if t.len() >= 16 {
            t.remove(0);
        }
        t.push(timing);
    }

    /// A decoded picture (on the decode thread): upload it into one of our
    /// textures, the newest replacing any not yet shown.
    pub fn push_picture(&self, pic: &Picture<'_>) {
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
        let planar = !matches!(pic.layout, Planes::Nv12 { .. });
        let full_chroma = matches!(pic.layout, Planes::I444 { .. });
        let mut f = self.frames.lock();
        if let Some(p) = f.pending.take() {
            if let Some(Some(s)) = f.slots.get_mut(p.slot) {
                s.busy = false;
            }
        }
        let Some(slot) = f
            .slots
            .iter()
            .position(|s| s.as_ref().is_none_or(|s| !s.busy))
        else {
            return;
        };
        if f.slots[slot].as_ref().is_none_or(|s| {
            s.width != pic.width
                || s.height != pic.height
                || s.planar != planar
                || s.full_chroma != full_chroma
        }) {
            f.slots[slot] = Some(self.slot(pic.width, pic.height, planar, full_chroma));
        }
        let s = f.slots[slot].as_mut().expect("made above");
        let (cw, ch) = if full_chroma {
            (pic.width, pic.height)
        } else {
            (pic.width.div_ceil(2), pic.height.div_ceil(2))
        };
        let q = &self.gpu.queue;
        let upload = |t: &wgpu::Texture, data: &[u8], stride: usize, w: u32, h: u32| {
            q.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: t,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(stride as u32),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        };
        match &pic.layout {
            Planes::Nv12 { y, uv } => {
                upload(&s.y, y.data, y.stride, pic.width, pic.height);
                upload(&s.u, uv.data, uv.stride, cw, ch);
            }
            Planes::I420 { y, u, v } | Planes::I444 { y, u, v } => {
                upload(&s.y, y.data, y.stride, pic.width, pic.height);
                upload(&s.u, u.data, u.stride, cw, ch);
                if let Some(vt) = &s.v {
                    upload(vt, v.data, v.stride, cw, ch);
                }
            }
        }
        s.busy = true;
        f.pending = Some(Shown {
            slot,
            timing,
            decoded_at: pic.decoded_at_us,
            width: pic.width,
            height: pic.height,
        });
        drop(f);
        self.frame_ready.notify_one();
    }

    fn slot(&self, width: u32, height: u32, planar: bool, full_chroma: bool) -> Slot {
        let d = &self.gpu.device;
        let (cw, ch) = if full_chroma {
            (width, height)
        } else {
            (width.div_ceil(2), height.div_ceil(2))
        };
        let y = texture(d, width, height, wgpu::TextureFormat::R8Unorm, "luma");
        let u = texture(
            d,
            cw,
            ch,
            if planar {
                wgpu::TextureFormat::R8Unorm
            } else {
                wgpu::TextureFormat::Rg8Unorm
            },
            "chroma",
        );
        let v = planar.then(|| texture(d, cw, ch, wgpu::TextureFormat::R8Unorm, "chroma v"));
        let uniform = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("video"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        // Chroma sited left, half a luma pixel off -- when it is half size.
        let chroma_offset = if full_chroma { 0.0 } else { 0.5 / width as f32 };
        let params: [u32; 8] = [
            chroma_offset.to_bits(),
            0f32.to_bits(),
            1f32.to_bits(),
            1f32.to_bits(),
            planar as u32,
            self.gpu.srgb(),
            0,
            0,
        ];
        self.gpu.queue.write_buffer(
            &uniform,
            0,
            &params
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<u8>>(),
        );
        let (yv, uv) = (
            y.create_view(&Default::default()),
            u.create_view(&Default::default()),
        );
        let vv = v.as_ref().map(|t| t.create_view(&Default::default()));
        let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("video"),
            layout: &self.gpu.video_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&self.gpu.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: uniform.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&yv),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&uv),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(
                        vv.as_ref().unwrap_or(&self.gpu.dummy),
                    ),
                },
            ],
        });
        Slot {
            planar,
            full_chroma,
            width,
            height,
            _uniform: uniform,
            bind,
            busy: false,
            y,
            u,
            v,
        }
    }

    pub fn toggle_overlay(&self) -> bool {
        let on = !self.overlay_enabled.fetch_xor(true, Ordering::AcqRel);
        self.poke();
        on
    }

    pub fn set_overlay(&self, on: bool) {
        self.overlay_enabled.store(on, Ordering::Release);
        self.poke();
    }

    pub fn set_status(&self, text: Option<String>) {
        *self.status.lock() = text;
        self.poke();
    }

    pub fn set_warning(&self, text: Option<String>) {
        *self.warning.lock() = text;
        self.poke();
    }

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

    /// Where the picture is drawn, and its own size (w, h).
    pub fn video_rect(&self) -> Option<(Rect, (f64, f64))> {
        let f = self.frames.lock();
        let p = f.current.or(f.pending)?;
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
    }
}

/// x, y, w, h.
pub type Rect = (f64, f64, f64, f64);

fn fit(dw: f64, dh: f64, sw: f64, sh: f64) -> Rect {
    if sw <= 0.0 || sh <= 0.0 {
        return (0.0, 0.0, dw, dh);
    }
    let s = (dw / sw).min(dh / sh);
    let (w, h) = ((sw * s).round(), (sh * s).round());
    (((dw - w) / 2.0).round(), ((dh - h) / 2.0).round(), w, h)
}

struct Overlay {
    text: String,
    _texture: wgpu::Texture,
    uniform: wgpu::Buffer,
    bind: wgpu::BindGroup,
    width: f64,
    height: f64,
}

fn overlay(gpu: &Gpu, slot: Option<Overlay>, text: &str, scale: f64) -> Option<Overlay> {
    if let Some(o) = slot.filter(|o| o.text == text) {
        return Some(o);
    }
    let b = super::text::rasterize(text, scale)?;
    let d = &gpu.device;
    let t = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("overlay"),
        size: wgpu::Extent3d {
            width: b.width as u32,
            height: b.height as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &t,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &b.rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(b.width as u32 * 4),
            rows_per_image: Some(b.height as u32),
        },
        wgpu::Extent3d {
            width: b.width as u32,
            height: b.height as u32,
            depth_or_array_layers: 1,
        },
    );
    let uniform = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("quad"),
        size: 32,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let view = t.create_view(&Default::default());
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("overlay"),
        layout: &gpu.quad_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Sampler(&gpu.sampler),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: uniform.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: wgpu::BindingResource::TextureView(&view),
            },
        ],
    });
    Some(Overlay {
        text: text.to_string(),
        _texture: t,
        uniform,
        bind,
        width: b.width as f64,
        height: b.height as f64,
    })
}

struct Renderer {
    shared: Arc<RenderShared>,
    /// None: drawing offscreen only (`offscreen_png`).
    surface: Option<(wgpu::Surface<'static>, wgpu::SurfaceConfiguration)>,
    size: (u32, u32),
    status: Option<Overlay>,
    notice: Option<Overlay>,
    warning: Option<Overlay>,
    stats_overlay: Option<Overlay>,
    overlay_at: Option<Instant>,
    shown: u32,
}

impl Renderer {
    fn sync_size(&mut self) {
        let l = *self.shared.layout.lock();
        if l.width > 0 && l.height > 0 && (l.width, l.height) != self.size {
            self.size = (l.width, l.height);
            if let Some((surface, config)) = &mut self.surface {
                config.width = l.width;
                config.height = l.height;
                surface.configure(&self.shared.gpu.device, config);
            }
        }
    }

    /// The newest picture waiting becomes the one shown.
    fn take_frame(&mut self) -> Option<Shown> {
        let mut f = self.shared.frames.lock();
        let fresh = f.pending.take();
        if let Some(p) = fresh {
            if let Some(old) = f.current.replace(p) {
                if old.slot != p.slot {
                    if let Some(Some(s)) = f.slots.get_mut(old.slot) {
                        s.busy = false;
                    }
                }
            }
        }
        fresh
    }

    /// Draw and present if there is anything new. Returns whether a new
    /// frame was shown.
    fn frame(&mut self) -> bool {
        let shared = self.shared.clone();
        self.sync_size();
        let fresh = self.take_frame();
        let overlay_due = shared.overlay_enabled.load(Ordering::Acquire)
            && self
                .overlay_at
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        let dirty = shared.dirty.swap(false, Ordering::AcqRel) || overlay_due;
        if fresh.is_none() && !dirty {
            return false;
        }
        let Some((surface, config)) = &self.surface else {
            return false;
        };
        let texture = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                surface.configure(&shared.gpu.device, config);
                shared.dirty.store(true, Ordering::Release);
                return false;
            }
            _ => {
                shared.dirty.store(true, Ordering::Release);
                return false;
            }
        };
        let view = texture.texture.create_view(&Default::default());
        self.draw(&view, overlay_due);
        let presented_at = clock::now_us();
        shared.gpu.queue.present(texture);
        if let Some(p) = fresh {
            self.shown += 1;
            shared
                .stats
                .presented(presented_at.wrapping_sub(p.decoded_at));
            if let Some(captured) = shared.stats.host_to_local(p.timing.captured_us) {
                let us = presented_at.wrapping_sub(captured) as i32;
                if (0..1_000_000).contains(&us) {
                    shared.stats.end_to_end(us as u32);
                }
            }
            self.snapshot_if_due();
        }
        fresh.is_some()
    }

    fn draw(&mut self, target: &wgpu::TextureView, overlay_due: bool) {
        let shared = self.shared.clone();
        let gpu = &shared.gpu;
        let layout = *shared.layout.lock();
        let scale = if layout.scale > 0.0 {
            layout.scale
        } else {
            1.0
        };
        let (dw, dh) = (self.size.0 as f64, self.size.1 as f64);
        let current = shared.frames.lock().current;

        // Overlays first (uploads), then one pass.
        if current.is_none() {
            self.notice = None;
            self.status = shared
                .status
                .lock()
                .clone()
                .and_then(|t| overlay(gpu, self.status.take(), &t, scale));
        } else {
            self.status = None;
            self.notice = shared
                .notice
                .lock()
                .clone()
                .and_then(|t| overlay(gpu, self.notice.take(), &t, scale));
        }
        if shared.overlay_enabled.load(Ordering::Acquire) {
            if overlay_due {
                self.overlay_at = Some(Instant::now());
                let text = shared.stats.snapshot().overlay_text();
                self.stats_overlay = overlay(gpu, self.stats_overlay.take(), &text, scale);
            }
        } else {
            self.stats_overlay = None;
            self.overlay_at = None;
        }
        let warning = shared.warning.lock().clone().filter(|_| current.is_some());
        self.warning = warning.and_then(|t| overlay(gpu, self.warning.take(), &t, scale));

        let margin = 12.0 * scale;
        let mut quads: Vec<(&Overlay, f64, f64)> = Vec::new();
        for o in [&self.status, &self.notice].into_iter().flatten() {
            quads.push((
                o,
                ((dw - o.width) / 2.0).round(),
                ((dh - o.height) / 2.0).round(),
            ));
        }
        if let Some(o) = &self.stats_overlay {
            quads.push((o, margin, margin));
        }
        if let Some(o) = &self.warning {
            quads.push((o, dw - o.width - margin, margin));
        }
        for (o, x, y) in &quads {
            let ndc =
                |px: f64, py: f64| ((px / dw * 2.0 - 1.0) as f32, (1.0 - py / dh * 2.0) as f32);
            let (x0, y0) = ndc(*x, *y);
            let (x1, y1) = ndc(x + o.width, y + o.height);
            let params: [u32; 8] = [
                x0.to_bits(),
                y0.to_bits(),
                x1.to_bits(),
                y1.to_bits(),
                gpu.srgb(),
                0,
                0,
                0,
            ];
            gpu.queue.write_buffer(
                &o.uniform,
                0,
                &params
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<u8>>(),
            );
        }

        let frames = shared.frames.lock();
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("stream"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let Some(p) = current {
                if let Some(Some(s)) = frames.slots.get(p.slot) {
                    let (x, y, w, h) = fit(dw, dh, p.width as f64, p.height as f64);
                    pass.set_viewport(x as f32, y as f32, w as f32, h as f32, 0.0, 1.0);
                    pass.set_pipeline(&gpu.video_pipeline);
                    pass.set_bind_group(0, &s.bind, &[]);
                    pass.draw(0..3, 0..1);
                }
            }
            pass.set_viewport(0.0, 0.0, dw as f32, dh as f32, 0.0, 1.0);
            pass.set_pipeline(&gpu.quad_pipeline);
            for (o, _, _) in &quads {
                pass.set_bind_group(0, &o.bind, &[]);
                pass.draw(0..4, 0..1);
            }
        }
        drop(frames);
        gpu.queue.submit([encoder.finish()]);
    }

    /// PING_TEST_SNAPSHOT: what was just drawn, drawn again into a texture we
    /// can read, as a PNG.
    fn snapshot_if_due(&mut self) {
        let due = {
            let mut s = self.shared.snapshot.lock();
            match s.as_ref() {
                Some((_, after)) if self.shown >= *after => s.take(),
                _ => None,
            }
        };
        let Some((path, _)) = due else { return };
        if let Err(e) = self.save_png(&path) {
            tracing::warn!(error = e, "test snapshot not saved");
        }
    }

    /// Draw the current state into a texture we can read, and save it.
    fn save_png(&mut self, path: &std::path::Path) -> Result<(), String> {
        let gpu = self.shared.gpu.clone();
        let (w, h) = self.size;
        let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("snapshot"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.draw(&target.create_view(&Default::default()), false);
        let row = (w * 4).div_ceil(256) * 256;
        let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("snapshot"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        gpu.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
        if !matches!(rx.recv(), Ok(Ok(()))) {
            return Err("not read back".into());
        }
        let data = buffer
            .slice(..)
            .get_mapped_range()
            .map_err(|e| format!("not mapped: {e:?}"))?;
        let bgra = matches!(
            gpu.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            for px in data[y * row as usize..y * row as usize + w as usize * 4]
                .as_chunks::<4>()
                .0
            {
                if bgra {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
                } else {
                    rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
            }
        }
        drop(data);
        let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(f), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()
            .and_then(|mut wr| wr.write_image_data(&rgba))
            .map_err(|e| e.to_string())?;
        tracing::info!(path = %path.display(), width = w, height = h, "test snapshot saved");
        Ok(())
    }
}

/// Present mode for the stream: V-Sync without a queue where the driver
/// allows it.
pub fn present_mode(caps: &wgpu::SurfaceCapabilities, vsync: bool) -> wgpu::PresentMode {
    let has = |m| caps.present_modes.contains(&m);
    if vsync {
        if has(wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        }
    } else if has(wgpu::PresentMode::Immediate) {
        wgpu::PresentMode::Immediate
    } else if has(wgpu::PresentMode::Mailbox) {
        wgpu::PresentMode::Mailbox
    } else {
        wgpu::PresentMode::Fifo
    }
}

/// Run the renderer until `shared.stop()`. Call on a dedicated thread.
pub fn run(
    shared: Arc<RenderShared>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
) {
    surface.configure(&shared.gpu.device, &config);
    tracing::info!(format = ?config.format, present_mode = ?config.present_mode, "renderer ready");
    let size = (config.width, config.height);
    let mut r = Renderer {
        shared: shared.clone(),
        surface: Some((surface, config)),
        size,
        status: None,
        notice: None,
        warning: None,
        stats_overlay: None,
        overlay_at: None,
        shown: 0,
    };
    while !shared.stop.load(Ordering::Acquire) {
        {
            let mut f = shared.frames.lock();
            if f.pending.is_none() && !shared.dirty.load(Ordering::Acquire) {
                shared
                    .frame_ready
                    .wait_for(&mut f, Duration::from_millis(50));
            }
        }
        r.frame();
    }
}

/// Draw the newest picture handed to `shared` (and its overlays) at
/// `width`x`height`, offscreen, into a PNG: checks without a window.
pub fn offscreen_png(
    shared: Arc<RenderShared>,
    width: u32,
    height: u32,
    path: &std::path::Path,
) -> Result<(), String> {
    let mut r = Renderer {
        shared: shared.clone(),
        surface: None,
        size: (width, height),
        status: None,
        notice: None,
        warning: None,
        stats_overlay: None,
        overlay_at: None,
        shown: 0,
    };
    r.take_frame();
    r.save_png(path)
}
