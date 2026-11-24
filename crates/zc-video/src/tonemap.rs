//! HDR → SDR for captured frames.
//!
//! With Windows HDR on, an 8-bit capture comes out far too bright: the desktop is composed in
//! scRGB (1.0 = 80 nits) with SDR white raised to the user's "SDR content brightness" (often 200
//! nits or more), and an 8-bit surface clips everything above 1.0. Instead, capture the 16-bit
//! float scRGB surface and draw it into a BGRA8 texture with a pixel shader that scales SDR white
//! back to 1.0, rolls the brightest highlights off softly, and applies the sRGB curve. The video
//! processor then converts that texture to NV12 exactly as it does for an SDR display.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BUFFER_DESC, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_VIEWPORT, ID3D11Buffer,
    ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView, ID3D11Texture2D,
    ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::{PCSTR, s};
use zc_core::ports::SourceError;
use zc_win::error::WinResultExt;

/// scRGB's 1.0 is this many nits.
const SCRGB_WHITE_NITS: f32 = 80.0;

/// A full-screen triangle, and the HDR → SDR pixel shader.
const HLSL: &str = r"
cbuffer Params : register(b0) { float scale; float3 pad; };
Texture2D<float4> src : register(t0);
float4 vs(uint id : SV_VertexID) : SV_Position {
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}
// Linear up to the knee, then an exponential roll-off, so SDR white stays (nearly) white and
// brighter highlights squeeze into the last bit of range instead of clipping flat.
float3 shoulder(float3 x) {
    float knee = 0.85, k = 0.065;
    return min(x, knee) + (1 - knee) * (1 - exp(-max(x - knee, 0) / k));
}
float3 oetf(float3 l) {
    return l <= 0.0031308 ? 12.92 * l : 1.055 * pow(l, 1 / 2.4) - 0.055;
}
float4 ps(float4 pos : SV_Position) : SV_Target {
    float3 c = max(src.Load(int3(pos.xy, 0)).rgb, 0) * scale;
    return float4(oetf(shoulder(c)), 1);
}
";

/// Draws scRGB frames as sRGB BGRA8 frames, on the GPU.
#[derive(Debug)]
pub struct ToneMapper {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    params: ID3D11Buffer,
    /// The output texture and its render-target view; remade when the size changes.
    target: Option<(ID3D11Texture2D, ID3D11RenderTargetView, (u32, u32))>,
}

/// The bytes a blob holds.
fn blob_bytes(blob: &ID3DBlob) -> Vec<u8> {
    // SAFETY: a blob's pointer and size describe its own bytes.
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
    .to_vec()
}

/// Compiles one entry point of [`HLSL`].
fn compile(entry: PCSTR, profile: PCSTR) -> Result<Vec<u8>, SourceError> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source slice and both out-parameters outlive the call.
    let result = unsafe {
        D3DCompile(
            HLSL.as_ptr().cast(),
            HLSL.len(),
            None,
            None,
            None,
            entry,
            profile,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let detail = errors.map(|b| String::from_utf8_lossy(&blob_bytes(&b)).into_owned());
        return Err(SourceError::Other(format!(
            "tone-map shader: {e} {}",
            detail.unwrap_or_default()
        )));
    }
    code.map(|b| blob_bytes(&b))
        .ok_or_else(|| SourceError::Other("the shader compiler returned nothing".into()))
}

impl ToneMapper {
    /// Compiles the shaders.
    ///
    /// # Errors
    /// The shader compiler (`d3dcompiler_47.dll`) or the device refused.
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext) -> Result<Self, SourceError> {
        let vs = compile(s!("vs"), s!("vs_5_0"))?;
        let ps = compile(s!("ps"), s!("ps_5_0"))?;
        let mut vertex = None;
        let mut pixel = None;
        let mut params = None;
        let buffer = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        // SAFETY: valid bytecode and descriptors; every out-parameter outlives its call.
        unsafe {
            device
                .CreateVertexShader(&vs, None, Some(&mut vertex))
                .context("CreateVertexShader")?;
            device.CreatePixelShader(&ps, None, Some(&mut pixel)).context("CreatePixelShader")?;
            device.CreateBuffer(&buffer, None, Some(&mut params)).context("CreateBuffer")?;
        }
        let missing = || SourceError::Other("the device returned no shader".into());
        Ok(Self {
            device: device.clone(),
            context: context.clone(),
            vertex: vertex.ok_or_else(missing)?,
            pixel: pixel.ok_or_else(missing)?,
            params: params.ok_or_else(missing)?,
            target: None,
        })
    }

    /// Draws `source` (a float scRGB frame) into the BGRA8 output and returns it. `sdr_white` is
    /// Windows' SDR white level in nits. The output is reused: copy it before the next call if
    /// it must outlive that.
    ///
    /// # Errors
    /// Resource creation failed.
    pub fn map(
        &mut self,
        source: &ID3D11Texture2D,
        sdr_white: f32,
    ) -> Result<&ID3D11Texture2D, SourceError> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a valid out-parameter.
        unsafe { source.GetDesc(&mut desc) };
        let size = (desc.Width, desc.Height);
        if self.target.as_ref().is_none_or(|t| t.2 != size) {
            let target = D3D11_TEXTURE2D_DESC {
                Width: size.0,
                Height: size.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            let mut texture = None;
            let mut view = None;
            // SAFETY: valid descriptor and out-parameters.
            unsafe {
                self.device
                    .CreateTexture2D(&target, None, Some(&mut texture))
                    .context("CreateTexture2D (tone-mapped)")?;
                let made =
                    texture.as_ref().ok_or_else(|| SourceError::Other("no texture".into()))?;
                self.device
                    .CreateRenderTargetView(made, None, Some(&mut view))
                    .context("CreateRenderTargetView")?;
            }
            if let (Some(texture), Some(view)) = (texture, view) {
                self.target = Some((texture, view, size));
            }
        }
        let Some((texture, view, _)) = &self.target else {
            return Err(SourceError::Other("no tone-map target".into()));
        };
        let scale = [SCRGB_WHITE_NITS / sdr_white.max(SCRGB_WHITE_NITS), 0.0, 0.0, 0.0];
        let mut input = None;
        let viewport = D3D11_VIEWPORT {
            Width: size.0 as f32,
            Height: size.1 as f32,
            MaxDepth: 1.0,
            ..Default::default()
        };
        // SAFETY: plain pipeline calls on a multithread-protected device; the views and buffer
        // are alive for the draw, and the bindings are cleared again so the video processor
        // can read the target afterwards.
        unsafe {
            self.device
                .CreateShaderResourceView(source, None, Some(&mut input))
                .context("CreateShaderResourceView")?;
            self.context.UpdateSubresource(&self.params, 0, None, scale.as_ptr().cast(), 0, 0);
            self.context.IASetInputLayout(None);
            self.context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShader(&self.pixel, None);
            self.context.PSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
            self.context.PSSetShaderResources(0, Some(&[input]));
            self.context.RSSetViewports(Some(&[viewport]));
            self.context.OMSetRenderTargets(Some(&[Some(view.clone())]), None);
            self.context.Draw(3, 0);
            self.context.PSSetShaderResources(0, Some(&[None]));
            self.context.OMSetRenderTargets(None, None);
        }
        Ok(texture)
    }
}
