//! The pointer drawn into the captured desktop on the GPU (`pointer` says
//! why, decodes the shapes and states the rule).
//!
//! One draw per picture: a quad over the pointer's rectangle on the picture
//! (a copy of the desktop), whose pixel shader reads the desktop pixel under
//! it and the shape's two layers and writes `pointer::composite` of them.
//! Reading the desktop in the shader rather than blending against the target
//! makes the XOR exact: D3D11 has no XOR blend for a BGRA target (logic ops
//! want an integer format), where Apollo approximates it with an inverting
//! blend (`display_vram.cpp`).
//!
//! On an HDR desktop (FP16 scRGB, linear) the shape's sRGB colours are made
//! linear and put at the desktop's SDR white; XOR, meaningless on light
//! levels, inverts against that white instead.

use windows::core::{s, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use crate::pointer::PointerImage;
use crate::CaptureError;

const SHADERS: &str = r#"
cbuffer Place : register(b0) {
    int2 origin; // the shape's top-left on the desktop
    int2 size;   // the shape's size; its XOR layer starts size.y rows down
    float sdr_scale; // HDR: SDR white over 80 cd/m2
    float3 pad;
};
Texture2D<float4> desktop : register(t0);
Texture2D<float4> shape : register(t1);

// One triangle covering the viewport, which is the shape's rectangle.
float4 vs_main(uint id : SV_VertexID) : SV_Position {
    float2 t = float2((id << 1) & 2, id & 2);
    return float4(t.x * 2.0 - 1.0, 1.0 - t.y * 2.0, 0.0, 1.0);
}

float4 ps_main(float4 pos : SV_Position) : SV_Target {
    int2 p = int2(pos.xy);
    int2 c = p - origin;
    float4 screen = desktop.Load(int3(p, 0));
    float4 blend = shape.Load(int3(c, 0));
    uint3 x = uint3(round(shape.Load(int3(c.x, c.y + size.y, 0)).rgb * 255.0));
    float3 mixed = lerp(screen.rgb, blend.rgb, blend.a);
    uint3 rgb = uint3(round(saturate(mixed) * 255.0)) ^ x;
    return float4(float3(rgb) / 255.0, 1.0);
}

float4 ps_hdr(float4 pos : SV_Position) : SV_Target {
    int2 p = int2(pos.xy);
    int2 c = p - origin;
    float4 screen = desktop.Load(int3(p, 0));
    float4 blend = shape.Load(int3(c, 0));
    float3 x = shape.Load(int3(c.x, c.y + size.y, 0)).rgb;
    float3 e = blend.rgb;
    float3 lin = (e <= 0.04045 ? e / 12.92 : pow((e + 0.055) / 1.055, 2.4)) * sdr_scale;
    float3 mixed = lerp(screen.rgb, lin, blend.a);
    float3 inverted = max(sdr_scale - mixed, 0.0);
    return float4(x > 0.5 ? inverted : mixed, 1.0);
}
"#;

/// The constant buffer `Place`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Place {
    origin: [i32; 2],
    size: [i32; 2],
    sdr_scale: f32,
    pad: [f32; 3],
}

fn d3d(what: &'static str) -> impl Fn(windows::core::Error) -> CaptureError {
    move |e| CaptureError::Platform(format!("{what}: {e}"))
}

fn compile(entry: &str, target: &str) -> Result<ID3DBlob, CaptureError> {
    let entry_c = format!("{entry}\0");
    let target_c = format!("{target}\0");
    let mut code = None;
    let mut errors = None;
    // SAFETY: the source, names and out-pointers are valid for the call; the
    // names are NUL-terminated above.
    let result = unsafe {
        D3DCompile(
            SHADERS.as_ptr() as *const _,
            SHADERS.len(),
            s!("pingpong-pointer"),
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
            .map(|b: ID3DBlob| String::from_utf8_lossy(blob_bytes(&b)).into_owned())
            .unwrap_or_default();
        return Err(CaptureError::Platform(format!(
            "D3DCompile({entry}): {e} {detail}"
        )));
    }
    code.ok_or_else(|| CaptureError::Platform(format!("D3DCompile({entry}) produced nothing")))
}

fn blob_bytes(b: &ID3DBlob) -> &[u8] {
    // SAFETY: a blob's pointer and size describe its own buffer, which lives
    // as long as the blob borrowed here.
    unsafe { std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()) }
}

/// A pointer shape on the GPU.
struct Shape {
    view: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
}

pub struct PointerOverlay {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    ps_hdr: ID3D11PixelShader,
    place: ID3D11Buffer,
    shape: Option<Shape>,
}

impl PointerOverlay {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<PointerOverlay, CaptureError> {
        let vs_blob = compile("vs_main", "vs_5_0")?;
        let ps_blob = compile("ps_main", "ps_5_0")?;
        let ps_hdr_blob = compile("ps_hdr", "ps_5_0")?;
        let (mut vs, mut ps, mut ps_hdr, mut place) = (None, None, None, None);
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: std::mem::size_of::<Place>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        // SAFETY: the bytecode slices are the compiled blobs; each out-pointer
        // is an Option we own.
        unsafe {
            device
                .CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))
                .map_err(d3d("CreateVertexShader(pointer)"))?;
            device
                .CreatePixelShader(blob_bytes(&ps_blob), None, Some(&mut ps))
                .map_err(d3d("CreatePixelShader(pointer)"))?;
            device
                .CreatePixelShader(blob_bytes(&ps_hdr_blob), None, Some(&mut ps_hdr))
                .map_err(d3d("CreatePixelShader(pointer, HDR)"))?;
            device
                .CreateBuffer(&desc, None, Some(&mut place))
                .map_err(d3d("CreateBuffer(pointer)"))?;
        }
        let made = || CaptureError::Platform("a pointer resource came back empty".into());
        Ok(PointerOverlay {
            device: device.clone(),
            context: context.clone(),
            vs: vs.ok_or_else(made)?,
            ps: ps.ok_or_else(made)?,
            ps_hdr: ps_hdr.ok_or_else(made)?,
            place: place.ok_or_else(made)?,
            shape: None,
        })
    }

    /// The pointer's new shape. A new texture each time: shapes change a few
    /// times a second at most (over a link, over text), never per frame.
    pub fn set_shape(&mut self, image: &PointerImage) -> Result<(), CaptureError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: image.width,
            Height: image.height * 2,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: image.pixels.as_ptr() as *const _,
            SysMemPitch: image.width * 4,
            SysMemSlicePitch: 0,
        };
        let (mut texture, mut view) = (None::<ID3D11Texture2D>, None);
        // SAFETY: `init` points at `image.pixels`, which holds exactly
        // width x 2*height BGRA pixels at that pitch and outlives the call.
        unsafe {
            self.device
                .CreateTexture2D(&desc, Some(&init), Some(&mut texture))
                .map_err(d3d("CreateTexture2D(pointer)"))?;
            let texture = texture.ok_or_else(|| {
                CaptureError::Platform("CreateTexture2D(pointer) returned nothing".into())
            })?;
            self.device
                .CreateShaderResourceView(&texture, None, Some(&mut view))
                .map_err(d3d("CreateShaderResourceView(pointer)"))?;
        }
        self.shape = view.map(|view| Shape {
            view,
            width: image.width,
            height: image.height,
        });
        Ok(())
    }

    /// Draw the pointer with its top-left at (`x`, `y`) into `target`, a
    /// copy of the desktop `desktop` shows. Off the edges is clipped.
    /// `hdr`: the desktop is FP16 scRGB, with SDR white this many times 80
    /// cd/m².
    pub fn draw(
        &self,
        desktop: &ID3D11ShaderResourceView,
        target: &ID3D11RenderTargetView,
        x: i32,
        y: i32,
        hdr: Option<f32>,
    ) {
        let Some(shape) = &self.shape else { return };
        let place = Place {
            origin: [x, y],
            size: [shape.width as i32, shape.height as i32],
            sdr_scale: hdr.unwrap_or(1.0),
            pad: [0.0; 3],
        };
        // The viewport may hang off the target's edges: the rasterizer clips
        // to the target.
        let viewport = D3D11_VIEWPORT {
            TopLeftX: x as f32,
            TopLeftY: y as f32,
            Width: shape.width as f32,
            Height: shape.height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let ctx = &self.context;
        // SAFETY: every resource bound here is owned by this overlay or the
        // capture, on the device this context belongs to; `place` is a
        // 32-byte value matching the buffer's size.
        unsafe {
            ctx.UpdateSubresource(
                &self.place,
                0,
                None,
                &place as *const Place as *const _,
                0,
                0,
            );
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(
                if hdr.is_some() {
                    &self.ps_hdr
                } else {
                    &self.ps
                },
                None,
            );
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.place.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(desktop.clone()), Some(shape.view.clone())]));
            ctx.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            ctx.RSSetViewports(Some(&[viewport]));
            ctx.Draw(3, 0);
            // Unbound, so the converter can read the picture next.
            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&[None, None]));
        }
    }
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
            ("ps_main", "ps_5_0"),
            ("ps_hdr", "ps_5_0"),
        ] {
            if let Err(e) = compile(entry, target) {
                panic!("{entry}: {e}");
            }
        }
    }

    #[test]
    fn the_place_is_two_shader_registers() {
        assert_eq!(std::mem::size_of::<Place>(), 32);
    }
}
