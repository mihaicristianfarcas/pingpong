//! The desktop to what the encoder reads, on the GPU, with the colour math
//! Sunshine uses (`convert_*_ps.hlsl`):
//!
//! - SDR: BGRA → NV12, BT.709;
//! - HDR: FP16 scRGB (linear BT.709, 1.0 = 80 cd/m², what Desktop
//!   Duplication gives of an HDR desktop) → BT.2020 → PQ → P010, BT.2020
//!   matrix; an SDR frame meanwhile (a secure desktop) is placed at the
//!   display's SDR white;
//! - 4:4:4: BGRA → AYUV, chroma at full resolution (NVENC takes 8-bit
//!   4:4:4 from a D3D11 texture only packed; 10-bit 4:4:4 needs CUDA, as
//!   Sunshine's `nvenc_d3d11_native.cpp` says, so HDR is 4:2:0).
//!
//! An earlier version handed NVENC the BGRA desktop and let it convert
//! internally. That path writes no colour description into the stream and
//! subsamples chroma however the driver likes, and text came out soft with
//! coloured fringes. Sunshine does the conversion itself, and so does this:
//!
//! - Limited ("video") range and the matrix written into the VUI by the
//!   encoder, so the decoder's idea of the colours is never a guess.
//! - Chroma sited LEFT (H.264/HEVC `chroma_sample_loc_type` 0, the default a
//!   decoder assumes): each chroma sample averages two bilinear taps one luma
//!   pixel apart, which centres it on the even luma column and filters it
//!   [1 2 1]/4 horizontally instead of point-sampling.
//! - Y and UV are rendered as two passes into the two planes of one NV12
//!   texture, which NVENC then reads directly. Scaling, if the desktop and the
//!   stream differ, falls out of the bilinear sampler for free.

use windows::core::{s, Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_AYUV, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};

use crate::EncodeError;

const SHADERS: &str = r#"
cbuffer ColorMatrix : register(b0) {
    float4 vec_y;
    float4 vec_u;
    float4 vec_v;
    float2 range_y;
    float2 range_uv;
};
cbuffer Subsample : register(b1) {
    float2 subsample_offset;
    float2 pad;
};
cbuffer Signal : register(b2) {
    // 1: the output is HDR10 (BT.2020, PQ).
    float pq_out;
    // 1: the input is FP16 scRGB (linear, 1.0 = 80 cd/m2); 0: sRGB-encoded.
    float scrgb_in;
    // SDR white over 80 cd/m2: where an sRGB frame's white goes in HDR.
    float sdr_scale;
    float pad2;
};
Texture2D image : register(t0);
SamplerState smp : register(s0);

float3 srgb_to_linear(float3 c) {
    return c <= 0.04045 ? c / 12.92 : pow((c + 0.055) / 1.055, 2.4);
}

// SMPTE ST 2084: light (a fraction of 10000 cd/m2) to signal.
float3 pq(float3 l) {
    const float m1 = 0.1593017578125, m2 = 78.84375;
    const float c1 = 0.8359375, c2 = 18.8515625, c3 = 18.6875;
    float3 lm = pow(saturate(l), m1);
    return pow((c1 + c2 * lm) / (1.0 + c3 * lm), m2);
}

// A sample as the signal the matrix takes: sRGB as it is for SDR; for HDR,
// linear light in scRGB units, BT.709 -> BT.2020, then PQ.
float3 signal(float3 c) {
    if (pq_out < 0.5) return saturate(c);
    float3 lin = scrgb_in > 0.5 ? c : srgb_to_linear(saturate(c)) * sdr_scale;
    const float3x3 bt709_to_bt2020 = {
        0.6274, 0.3293, 0.0433,
        0.0691, 0.9195, 0.0114,
        0.0164, 0.0880, 0.8956,
    };
    return pq(max(mul(bt709_to_bt2020, lin), 0.0) * (80.0 / 10000.0));
}

struct VsOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

// One triangle that covers the viewport: (0,0) (2,0) (0,2) in uv.
VsOut vs_main(uint id : SV_VertexID) {
    VsOut o;
    float2 t = float2((id << 1) & 2, id & 2);
    o.pos = float4(t.x * 2.0 - 1.0, 1.0 - t.y * 2.0, 0.0, 1.0);
    o.uv = t;
    return o;
}

float ps_y(VsOut i) : SV_Target {
    float3 rgb = signal(image.Sample(smp, i.uv).rgb);
    return dot(vec_y.xyz, rgb) * range_y.x + range_y.y;
}

// 4:4:4, packed for an AYUV texture written through an R8G8B8A8 view:
// bytes V, U, Y, A.
float4 ps_ayuv(VsOut i) : SV_Target {
    float3 rgb = signal(image.Sample(smp, i.uv).rgb);
    float y = dot(vec_y.xyz, rgb) * range_y.x + range_y.y;
    float u = (dot(vec_u.xyz, rgb) + vec_u.w) * range_uv.x + range_uv.y;
    float v = (dot(vec_v.xyz, rgb) + vec_v.w) * range_uv.x + range_uv.y;
    return float4(v, u, y, 1.0);
}

float2 ps_uv(VsOut i) : SV_Target {
    // At half resolution each output pixel centre lands between luma pixels
    // (2i, 2i+1): that tap is the 2x2 average. The second tap, one luma pixel
    // left, averages (2i-1, 2i). Together: centred on luma column 2i -- left
    // siting -- with a [1 2 1]/4 horizontal filter.
    float3 a = image.Sample(smp, i.uv).rgb;
    float3 b = image.Sample(smp, float2(i.uv.x - subsample_offset.x, i.uv.y)).rgb;
    float3 rgb = signal((a + b) * 0.5);
    float u = dot(vec_u.xyz, rgb) + vec_u.w;
    float v = dot(vec_v.xyz, rgb) + vec_v.w;
    return float2(u * range_uv.x + range_uv.y, v * range_uv.x + range_uv.y);
}
"#;

/// Constant buffer layout matching `ColorMatrix` (padded to 64 bytes).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct ColorMatrix {
    vec_y: [f32; 4],
    vec_u: [f32; 4],
    vec_v: [f32; 4],
    range_y: [f32; 2],
    range_uv: [f32; 2],
}

/// Limited range for the matrix with `kr`, `kb`: BT.709 (0.2126, 0.0722)
/// for SDR, BT.2020 (0.2627, 0.0593) for HDR. 8-bit maps Y to 16..235 and
/// Cb/Cr to 16..240 out of 255; 10-bit to 64..940 and 64..960 out of 1023,
/// written in the top ten bits of sixteen (P010).
fn limited(kr: f32, kb: f32, ten_bit: bool) -> ColorMatrix {
    let kg = 1.0 - kr - kb;
    let (range_y, range_uv) = if ten_bit {
        let unit = 64.0 / 65535.0;
        ([876.0 * unit, 64.0 * unit], [896.0 * unit, 64.0 * unit])
    } else {
        ([219.0 / 255.0, 16.0 / 255.0], [224.0 / 255.0, 16.0 / 255.0])
    };
    ColorMatrix {
        vec_y: [kr, kg, kb, 0.0],
        vec_u: [-0.5 * kr / (1.0 - kb), -0.5 * kg / (1.0 - kb), 0.5, 0.5],
        vec_v: [0.5, -0.5 * kg / (1.0 - kr), -0.5 * kb / (1.0 - kr), 0.5],
        range_y,
        range_uv,
    }
}

/// What the converter writes, for the encoder to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// SDR 4:2:0, BT.709.
    Nv12,
    /// HDR10 4:2:0: BT.2020, PQ, 10-bit.
    P010,
    /// SDR 4:4:4, BT.709, packed.
    Ayuv,
}

impl Output {
    pub fn for_stream(hdr: bool, yuv444: bool) -> Output {
        match (hdr, yuv444) {
            (true, _) => Output::P010,
            (false, true) => Output::Ayuv,
            (false, false) => Output::Nv12,
        }
    }

    fn format(self) -> DXGI_FORMAT {
        match self {
            Output::Nv12 => DXGI_FORMAT_NV12,
            Output::P010 => DXGI_FORMAT_P010,
            Output::Ayuv => DXGI_FORMAT_AYUV,
        }
    }
}

/// The constant buffer `Signal`.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Signal {
    pq_out: f32,
    scrgb_in: f32,
    sdr_scale: f32,
    pad: f32,
}

fn d3d(what: &'static str) -> impl Fn(windows::core::Error) -> EncodeError {
    move |e| EncodeError::D3d(format!("{what}: {e}"))
}

fn compile(entry: &str, target: &str) -> Result<ID3DBlob, EncodeError> {
    let entry_c = format!("{entry}\0");
    let target_c = format!("{target}\0");
    let mut code = None;
    let mut errors = None;
    let result = unsafe {
        D3DCompile(
            SHADERS.as_ptr() as *const _,
            SHADERS.len(),
            s!("pingpong-convert"),
            None,
            None,
            PCSTR(entry_c.as_ptr()),
            PCSTR(target_c.as_ptr()),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let detail = errors
            .map(|b: ID3DBlob| unsafe {
                let bytes = std::slice::from_raw_parts(
                    b.GetBufferPointer() as *const u8,
                    b.GetBufferSize(),
                );
                String::from_utf8_lossy(bytes).into_owned()
            })
            .unwrap_or_default();
        return Err(EncodeError::D3d(format!(
            "D3DCompile({entry}): {e} {detail}"
        )));
    }
    code.ok_or_else(|| EncodeError::D3d(format!("D3DCompile({entry}) produced nothing")))
}

fn blob_bytes(b: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

pub struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    ps_ayuv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    cb_color: ID3D11Buffer,
    cb_subsample: ID3D11Buffer,
    /// The `Signal` for an sRGB source and for an scRGB one.
    cb_signal: [ID3D11Buffer; 2],
    output: ID3D11Texture2D,
    kind: Output,
    /// Luma (or the whole packed picture), and chroma for 4:2:0.
    rtv_y: ID3D11RenderTargetView,
    rtv_uv: Option<ID3D11RenderTargetView>,
    width: u32,
    height: u32,
    /// Shader view of the source, keyed by the texture it was made for.
    source: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
}

// SAFETY: moved to the encode thread and used only there.
unsafe impl Send for Converter {}

impl Converter {
    /// A converter writing `width`×`height` pictures of `kind`; both even.
    /// `sdr_white_nits`: where an SDR frame's white goes in an HDR picture.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        kind: Output,
        sdr_white_nits: u16,
    ) -> Result<Converter, EncodeError> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(EncodeError::Unsupported(format!(
                "odd picture size {width}x{height}"
            )));
        }
        let vs_blob = compile("vs_main", "vs_5_0")?;
        let ps_y_blob = compile("ps_y", "ps_5_0")?;
        let ps_uv_blob = compile("ps_uv", "ps_5_0")?;
        let ps_ayuv_blob = compile("ps_ayuv", "ps_5_0")?;

        let mut vs = None;
        let mut ps_y = None;
        let mut ps_uv = None;
        let mut ps_ayuv = None;
        unsafe {
            device
                .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))
                .map_err(d3d("CreateVertexShader"))?;
            device
                .CreatePixelShader(blob_bytes(&ps_y_blob), None, Some(&mut ps_y))
                .map_err(d3d("CreatePixelShader(y)"))?;
            device
                .CreatePixelShader(blob_bytes(&ps_uv_blob), None, Some(&mut ps_uv))
                .map_err(d3d("CreatePixelShader(uv)"))?;
            device
                .CreatePixelShader(blob_bytes(&ps_ayuv_blob), None, Some(&mut ps_ayuv))
                .map_err(d3d("CreatePixelShader(ayuv)"))?;
        }

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
            AddressW: D3D11_TEXTURE_ADDRESS_WRAP,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        unsafe { device.CreateSamplerState(&sampler_desc, Some(&mut sampler)) }
            .map_err(d3d("CreateSamplerState"))?;

        let hdr = kind == Output::P010;
        let color = if hdr {
            limited(0.2627, 0.0593, true)
        } else {
            limited(0.2126, 0.0722, false)
        };
        let cb_color = constant_buffer(device, &color)?;
        let subsample: [f32; 4] = [1.0 / width as f32, 1.0 / height as f32, 0.0, 0.0];
        let cb_subsample = constant_buffer(device, &subsample)?;
        let signal = |scrgb_in: bool| Signal {
            pq_out: if hdr { 1.0 } else { 0.0 },
            scrgb_in: if scrgb_in { 1.0 } else { 0.0 },
            sdr_scale: sdr_white_nits.max(1) as f32 / 80.0,
            pad: 0.0,
        };
        let cb_signal = [
            constant_buffer(device, &signal(false))?,
            constant_buffer(device, &signal(true))?,
        ];

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: kind.format(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut output = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut output)) }
            .map_err(d3d("CreateTexture2D(converter output)"))?;
        let output: ID3D11Texture2D = output.expect("CreateTexture2D succeeded");

        // The view's format selects the plane: R8 (R16) is luma, R8G8
        // (R16G16) the interleaved chroma plane at half resolution; AYUV is
        // written whole through an R8G8B8A8 view.
        let rtv = |format| -> Result<ID3D11RenderTargetView, EncodeError> {
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                },
            };
            let mut view = None;
            unsafe { device.CreateRenderTargetView(&output, Some(&desc), Some(&mut view)) }
                .map_err(d3d("CreateRenderTargetView"))?;
            Ok(view.expect("CreateRenderTargetView succeeded"))
        };
        let (rtv_y, rtv_uv) = match kind {
            Output::Nv12 => (
                rtv(DXGI_FORMAT_R8_UNORM)?,
                Some(rtv(DXGI_FORMAT_R8G8_UNORM)?),
            ),
            Output::P010 => (
                rtv(DXGI_FORMAT_R16_UNORM)?,
                Some(rtv(DXGI_FORMAT_R16G16_UNORM)?),
            ),
            Output::Ayuv => (rtv(DXGI_FORMAT_R8G8B8A8_UNORM)?, None),
        };

        Ok(Converter {
            device: device.clone(),
            context: context.clone(),
            vs: vs.expect("created"),
            ps_y: ps_y.expect("created"),
            ps_uv: ps_uv.expect("created"),
            ps_ayuv: ps_ayuv.expect("created"),
            sampler: sampler.expect("created"),
            cb_color,
            cb_subsample,
            cb_signal,
            output,
            kind,
            rtv_y,
            rtv_uv,
            width,
            height,
            source: None,
        })
    }

    /// The texture every `convert` writes into. Register it with the
    /// encoder once.
    pub fn output(&self) -> &ID3D11Texture2D {
        &self.output
    }

    /// What the output is.
    pub fn kind(&self) -> Output {
        self.kind
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Convert `source` (BGRA, or FP16 scRGB from an HDR desktop; any size)
    /// into the output.
    pub fn convert(&mut self, source: &ID3D11Texture2D) -> Result<(), EncodeError> {
        let stale = match &self.source {
            Some((tex, _)) => tex.as_raw() != source.as_raw(),
            None => true,
        };
        if stale {
            let mut srv = None;
            unsafe {
                self.device
                    .CreateShaderResourceView(source, None, Some(&mut srv))
            }
            .map_err(d3d("CreateShaderResourceView"))?;
            self.source = Some((source.clone(), srv.expect("created")));
        }
        let srv = &self.source.as_ref().expect("set above").1;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: writes one descriptor we own.
        unsafe { source.GetDesc(&mut desc) };
        let scrgb = desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
        let signal = &self.cb_signal[scrgb as usize];

        let ctx = &self.context;
        unsafe {
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetConstantBuffers(
                0,
                Some(&[
                    Some(self.cb_color.clone()),
                    Some(self.cb_subsample.clone()),
                    Some(signal.clone()),
                ]),
            );

            // Luma (or the packed 4:4:4 picture), full resolution.
            ctx.OMSetRenderTargets(Some(&[Some(self.rtv_y.clone())]), None);
            ctx.RSSetViewports(Some(&[viewport(self.width, self.height)]));
            let first = if self.kind == Output::Ayuv {
                &self.ps_ayuv
            } else {
                &self.ps_y
            };
            ctx.PSSetShader(first, None);
            ctx.Draw(3, 0);

            // Chroma, half resolution.
            if let Some(rtv_uv) = &self.rtv_uv {
                ctx.OMSetRenderTargets(Some(&[Some(rtv_uv.clone())]), None);
                ctx.RSSetViewports(Some(&[viewport(self.width / 2, self.height / 2)]));
                ctx.PSSetShader(&self.ps_uv, None);
                ctx.Draw(3, 0);
            }

            // Unbind, so the next capture copy into the source never races a
            // bound view and NVENC sees an unbound output.
            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }
}

fn viewport(width: u32, height: u32) -> D3D11_VIEWPORT {
    D3D11_VIEWPORT {
        TopLeftX: 0.0,
        TopLeftY: 0.0,
        Width: width as f32,
        Height: height as f32,
        MinDepth: 0.0,
        MaxDepth: 1.0,
    }
}

fn constant_buffer<T: Copy>(device: &ID3D11Device, value: &T) -> Result<ID3D11Buffer, EncodeError> {
    let size = std::mem::size_of::<T>().div_ceil(16) * 16;
    let mut bytes = vec![0u8; size];
    unsafe {
        std::ptr::copy_nonoverlapping(
            value as *const T as *const u8,
            bytes.as_mut_ptr(),
            std::mem::size_of::<T>(),
        );
    }
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: size as u32,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
        StructureByteStride: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: bytes.as_ptr() as *const _,
        SysMemPitch: 0,
        SysMemSlicePitch: 0,
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, Some(&init), Some(&mut buffer)) }
        .map_err(d3d("CreateBuffer"))?;
    Ok(buffer.expect("CreateBuffer succeeded"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// D3DCompile needs no GPU: a shader that does not compile fails here
    /// rather than at a session's start.
    #[test]
    fn every_shader_compiles() {
        for (entry, target) in [
            ("vs_main", "vs_5_0"),
            ("ps_y", "ps_5_0"),
            ("ps_uv", "ps_5_0"),
            ("ps_ayuv", "ps_5_0"),
        ] {
            if let Err(e) = compile(entry, target) {
                panic!("{entry}: {e}");
            }
        }
    }

    #[test]
    fn ten_bit_limited_range_lands_in_the_top_ten_bits() {
        let m = limited(0.2627, 0.0593, true);
        // Y' = 1 is code 940, written as 940 << 6 of 65535.
        let white = (1.0 * m.range_y[0] + m.range_y[1]) * 65535.0;
        assert!((white - (940 << 6) as f32).abs() < 0.5, "{white}");
        let black = m.range_y[1] * 65535.0;
        assert!((black - (64 << 6) as f32).abs() < 0.5, "{black}");
        // Neutral chroma (0.5) is code 512.
        let mid = (0.5 * m.range_uv[0] + m.range_uv[1]) * 65535.0;
        assert!((mid - (512 << 6) as f32).abs() < 0.5, "{mid}");
    }
}
