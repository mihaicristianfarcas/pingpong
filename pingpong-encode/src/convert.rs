//! BGRA → NV12 on the GPU, with the colour math Apollo uses.
//!
//! An earlier version handed NVENC the BGRA desktop and let it convert
//! internally. That path writes no colour description into the stream and
//! subsamples chroma however the driver likes, and text came out soft with
//! coloured fringes. Apollo does the conversion itself, and so does this:
//!
//! - BT.709 matrix, limited ("video") range, written into the VUI by the encoder
//!   so the decoder's idea of the colours is never a guess.
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
    DXGI_FORMAT_NV12, DXGI_FORMAT_R8G8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
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
Texture2D image : register(t0);
SamplerState smp : register(s0);

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
    float3 rgb = saturate(image.Sample(smp, i.uv).rgb);
    return dot(vec_y.xyz, rgb) * range_y.x + range_y.y;
}

float2 ps_uv(VsOut i) : SV_Target {
    // At half resolution each output pixel centre lands between luma pixels
    // (2i, 2i+1): that tap is the 2x2 average. The second tap, one luma pixel
    // left, averages (2i-1, 2i). Together: centred on luma column 2i -- left
    // siting -- with a [1 2 1]/4 horizontal filter.
    float3 a = image.Sample(smp, i.uv).rgb;
    float3 b = image.Sample(smp, float2(i.uv.x - subsample_offset.x, i.uv.y)).rgb;
    float3 rgb = saturate((a + b) * 0.5);
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

/// BT.709, limited range. Kr/Kb from ITU-R BT.709; the range maps Y to 16..235
/// and Cb/Cr to 16..240 out of 255.
fn bt709_limited() -> ColorMatrix {
    let (kr, kb) = (0.2126f32, 0.0722f32);
    let kg = 1.0 - kr - kb;
    ColorMatrix {
        vec_y: [kr, kg, kb, 0.0],
        vec_u: [-0.5 * kr / (1.0 - kb), -0.5 * kg / (1.0 - kb), 0.5, 0.5],
        vec_v: [0.5, -0.5 * kg / (1.0 - kr), -0.5 * kb / (1.0 - kr), 0.5],
        range_y: [219.0 / 255.0, 16.0 / 255.0],
        range_uv: [224.0 / 255.0, 16.0 / 255.0],
    }
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
    sampler: ID3D11SamplerState,
    cb_color: ID3D11Buffer,
    cb_subsample: ID3D11Buffer,
    output: ID3D11Texture2D,
    rtv_y: ID3D11RenderTargetView,
    rtv_uv: ID3D11RenderTargetView,
    width: u32,
    height: u32,
    /// Shader view of the source, keyed by the texture it was made for.
    source: Option<(ID3D11Texture2D, ID3D11ShaderResourceView)>,
}

// SAFETY: moved to the encode thread and used only there.
unsafe impl Send for Converter {}

impl Converter {
    /// A converter writing `width`×`height` NV12. Both must be even.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Converter, EncodeError> {
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(EncodeError::Unsupported(format!(
                "odd NV12 size {width}x{height}"
            )));
        }
        let vs_blob = compile("vs_main", "vs_5_0")?;
        let ps_y_blob = compile("ps_y", "ps_5_0")?;
        let ps_uv_blob = compile("ps_uv", "ps_5_0")?;

        let mut vs = None;
        let mut ps_y = None;
        let mut ps_uv = None;
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

        let color = bt709_limited();
        let cb_color = constant_buffer(device, &color)?;
        let subsample: [f32; 4] = [1.0 / width as f32, 1.0 / height as f32, 0.0, 0.0];
        let cb_subsample = constant_buffer(device, &subsample)?;

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
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut output = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut output)) }
            .map_err(d3d("CreateTexture2D(NV12)"))?;
        let output: ID3D11Texture2D = output.expect("CreateTexture2D succeeded");

        // The view's format selects the plane: R8 is luma, R8G8 is the
        // interleaved chroma plane at half resolution.
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
        let rtv_y = rtv(DXGI_FORMAT_R8_UNORM)?;
        let rtv_uv = rtv(DXGI_FORMAT_R8G8_UNORM)?;

        Ok(Converter {
            device: device.clone(),
            context: context.clone(),
            vs: vs.expect("created"),
            ps_y: ps_y.expect("created"),
            ps_uv: ps_uv.expect("created"),
            sampler: sampler.expect("created"),
            cb_color,
            cb_subsample,
            output,
            rtv_y,
            rtv_uv,
            width,
            height,
            source: None,
        })
    }

    /// The NV12 texture every `convert` writes into. Register it with the
    /// encoder once.
    pub fn output(&self) -> &ID3D11Texture2D {
        &self.output
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Convert `source` (BGRA, any size) into the NV12 output.
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

        let ctx = &self.context;
        unsafe {
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetConstantBuffers(
                0,
                Some(&[Some(self.cb_color.clone()), Some(self.cb_subsample.clone())]),
            );

            // Luma, full resolution.
            ctx.OMSetRenderTargets(Some(&[Some(self.rtv_y.clone())]), None);
            ctx.RSSetViewports(Some(&[viewport(self.width, self.height)]));
            ctx.PSSetShader(&self.ps_y, None);
            ctx.Draw(3, 0);

            // Chroma, half resolution.
            ctx.OMSetRenderTargets(Some(&[Some(self.rtv_uv.clone())]), None);
            ctx.RSSetViewports(Some(&[viewport(self.width / 2, self.height / 2)]));
            ctx.PSSetShader(&self.ps_uv, None);
            ctx.Draw(3, 0);

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
