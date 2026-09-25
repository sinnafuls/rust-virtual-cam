//! Windows.Graphics.Capture of one monitor, scaled/letterboxed and converted to NV12
//! (BT.601 limited range) on the GPU, then read back into the shared ring.

use deskcam_proto::{FrameRing, StreamInfo};
use windows::Foundation::TimeSpan;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCOMPILE_OPTIMIZATION_LEVEL3, D3DCompile};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows_core::{Interface, PCSTR, s};

use crate::log::log;

const SHADER: &str = r#"
Texture2D src : register(t0);
SamplerState samp : register(s0);
cbuffer Params : register(b0) { float2 tap; float2 pad; };
struct VsOut { float4 pos : SV_Position; float2 uv : TEXCOORD0; };
VsOut vs_main(uint id : SV_VertexID) {
    VsOut o; float2 uv = float2((id << 1) & 2, id & 2);
    o.pos = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1); o.uv = uv; return o;
}
float3 fetch(float2 uv) {
    return 0.25 * (src.Sample(samp, uv + float2(-tap.x, -tap.y)).rgb + src.Sample(samp, uv + float2(tap.x, -tap.y)).rgb
                 + src.Sample(samp, uv + float2(-tap.x,  tap.y)).rgb + src.Sample(samp, uv + float2(tap.x,  tap.y)).rgb);
}
float ps_y(VsOut i) : SV_Target { return 0.0627451 + dot(fetch(i.uv), float3(0.256788, 0.504129, 0.0979059)); }
float2 ps_uv(VsOut i) : SV_Target {
    float3 c = fetch(i.uv);
    return float2(0.501961 + dot(c, float3(-0.148223, -0.290993, 0.439216)),
                  0.501961 + dot(c, float3(0.439216, -0.367788, -0.0714274)));
}
"#;

/// Letterbox rectangle `(x, y, w, h)` of a `sw`×`sh` source inside `dw`×`dh`; all even.
pub fn fit_rect(sw: u32, sh: u32, dw: u32, dh: u32) -> (u32, u32, u32, u32) {
    let (sw, sh, dw, dh) = (sw.max(1) as u64, sh.max(1) as u64, dw as u64, dh as u64);
    let (w, h) = if sw * dh >= sh * dw { (dw, (sh * dw / sw) & !1) } else { ((sw * dh / sh) & !1, dh) };
    let (w, h) = (w.max(2), h.max(2));
    ((((dw - w) / 2) & !1) as u32, (((dh - h) / 2) & !1) as u32, w as u32, h as u32)
}

#[repr(C)]
struct Params {
    tap: [f32; 2],
    pad: [f32; 2],
}

struct Source {
    tex: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
}

pub struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    d3d: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    pool_size: SizeInt32,
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    params: ID3D11Buffer,
    y_rt: ID3D11Texture2D,
    y_rtv: ID3D11RenderTargetView,
    uv_rt: ID3D11Texture2D,
    uv_rtv: ID3D11RenderTargetView,
    y_stage: ID3D11Texture2D,
    uv_stage: ID3D11Texture2D,
    src: Option<Source>,
    out: StreamInfo,
}

fn compile(entry: PCSTR, target: PCSTR) -> windows_core::Result<ID3DBlob> {
    let mut code = None;
    let mut errors = None;
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr() as *const _,
            SHADER.len(),
            s!("deskcam"),
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let msg = errors
            .map(|b| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()))
                    .into_owned()
            })
            .unwrap_or_default();
        return Err(windows_core::Error::new(e.code(), format!("shader compile failed: {msg}")));
    }
    code.ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_FAIL))
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }
}

fn texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    usage: D3D11_USAGE,
    bind: D3D11_BIND_FLAG,
    cpu: D3D11_CPU_ACCESS_FLAG,
) -> windows_core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: cpu.0 as u32,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    tex.ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_FAIL))
}

fn rtv(device: &ID3D11Device, tex: &ID3D11Texture2D) -> windows_core::Result<ID3D11RenderTargetView> {
    let mut view = None;
    unsafe { device.CreateRenderTargetView(tex, None, Some(&mut view))? };
    view.ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_FAIL))
}

impl Capture {
    pub fn new(hmon: HMONITOR, out: StreamInfo, fps: u32, cursor: bool) -> windows_core::Result<Capture> {
        let (mut device, mut context) = (None, None);
        unsafe {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?
        };
        let device = device.ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        let context = context.ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)? }.cast()?;

        let interop = windows_core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(hmon)? };
        let pool_size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, pool_size)?;
        let session = pool.CreateCaptureSession(&item)?;
        // IsCursorCaptureEnabled needs Windows 10 2004; older builds always draw the cursor.
        if let Err(e) = session.SetIsCursorCaptureEnabled(cursor) {
            log!("cursor setting unavailable: {e}");
        }
        // Windows 10 has no borderless capture: the yellow border shows on screen (not in frames).
        if deskcam_proto::os::build() >= deskcam_proto::os::BORDERLESS_BUILD {
            let borderless = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
                .and_then(|op| op.join())
                .and_then(|_| session.SetIsBorderRequired(false));
            if let Err(e) = borderless {
                log!("borderless capture unavailable: {e}");
            }
        }
        if let Err(e) = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000_000 / fps.max(1) as i64 }) {
            log!("MinUpdateInterval unavailable: {e}");
        }

        let vs_blob = compile(s!("vs_main"), s!("vs_4_0"))?;
        let ps_y_blob = compile(s!("ps_y"), s!("ps_4_0"))?;
        let ps_uv_blob = compile(s!("ps_uv"), s!("ps_4_0"))?;
        let (mut vs, mut ps_y, mut ps_uv, mut sampler, mut params) = (None, None, None, None, None);
        unsafe {
            device.CreateVertexShader(blob_bytes(&vs_blob), None, Some(&mut vs))?;
            device.CreatePixelShader(blob_bytes(&ps_y_blob), None, Some(&mut ps_y))?;
            device.CreatePixelShader(blob_bytes(&ps_uv_blob), None, Some(&mut ps_uv))?;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: size_of::<Params>() as u32,
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut params),
            )?;
        }
        let fail = || windows_core::Error::from(windows::Win32::Foundation::E_FAIL);
        let (w, h) = (out.width, out.height);
        let none = D3D11_CPU_ACCESS_FLAG(0);
        let y_rt = texture(&device, w, h, DXGI_FORMAT_R8_UNORM, D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET, none)?;
        let uv_rt = texture(&device, w / 2, h / 2, DXGI_FORMAT_R8G8_UNORM, D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET, none)?;
        let y_stage = texture(&device, w, h, DXGI_FORMAT_R8_UNORM, D3D11_USAGE_STAGING, D3D11_BIND_FLAG(0), D3D11_CPU_ACCESS_READ)?;
        let uv_stage =
            texture(&device, w / 2, h / 2, DXGI_FORMAT_R8G8_UNORM, D3D11_USAGE_STAGING, D3D11_BIND_FLAG(0), D3D11_CPU_ACCESS_READ)?;
        let y_rtv = rtv(&device, &y_rt)?;
        let uv_rtv = rtv(&device, &uv_rt)?;

        session.StartCapture()?;
        Ok(Capture {
            vs: vs.ok_or_else(fail)?,
            ps_y: ps_y.ok_or_else(fail)?,
            ps_uv: ps_uv.ok_or_else(fail)?,
            sampler: sampler.ok_or_else(fail)?,
            params: params.ok_or_else(fail)?,
            device,
            context,
            d3d,
            pool,
            session,
            pool_size,
            y_rt,
            y_rtv,
            uv_rt,
            uv_rtv,
            y_stage,
            uv_stage,
            src: None,
            out,
        })
    }

    fn ensure_source(&mut self, width: u32, height: u32) -> windows_core::Result<()> {
        if self.src.as_ref().is_some_and(|s| s.width == width && s.height == height) {
            return Ok(());
        }
        let tex = texture(
            &self.device,
            width,
            height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_SHADER_RESOURCE,
            D3D11_CPU_ACCESS_FLAG(0),
        )?;
        let mut srv = None;
        unsafe { self.device.CreateShaderResourceView(&tex, None, Some(&mut srv))? };
        let srv = srv.ok_or_else(|| windows_core::Error::from(windows::Win32::Foundation::E_FAIL))?;
        self.src = Some(Source { tex, srv, width, height });
        Ok(())
    }

    fn pass(&self, rtv: &ID3D11RenderTargetView, ps: &ID3D11PixelShader, clear: [f32; 4], tap: [f32; 2], vp: [f32; 4]) {
        let ctx = &self.context;
        let params = Params { tap, pad: [0.0; 2] };
        unsafe {
            ctx.ClearRenderTargetView(rtv, &clear);
            ctx.UpdateSubresource(&self.params, 0, None, &params as *const _ as *const _, 0, 0);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: vp[0],
                TopLeftY: vp[1],
                Width: vp[2],
                Height: vp[3],
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.PSSetShader(ps, None);
            ctx.Draw(3, 0);
        }
    }

    /// Publishes the newest captured frame; `Ok(false)` when the screen produced no new frame.
    pub fn tick(&mut self, ring: &FrameRing) -> windows_core::Result<bool> {
        let mut latest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            latest = Some(frame);
        }
        let Some(frame) = latest else { return Ok(false) };

        let content = frame.ContentSize()?;
        let (cw, ch) = (content.Width.max(1) as u32, content.Height.max(1) as u32);
        let surface_tex: ID3D11Texture2D =
            unsafe { frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()? };
        let mut surface_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { surface_tex.GetDesc(&mut surface_desc) };
        let (cw, ch) = (cw.min(surface_desc.Width), ch.min(surface_desc.Height));
        self.ensure_source(cw, ch)?;
        let src = self.src.as_ref().expect("source texture ensured");
        unsafe {
            self.context.CopySubresourceRegion(
                &src.tex,
                0,
                0,
                0,
                0,
                &surface_tex,
                0,
                Some(&D3D11_BOX { left: 0, top: 0, front: 0, right: cw, bottom: ch, back: 1 }),
            );
        }
        frame.Close()?;
        if content.Width != self.pool_size.Width || content.Height != self.pool_size.Height {
            self.pool.Recreate(&self.d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)?;
            self.pool_size = content;
        }

        let (w, h) = (self.out.width, self.out.height);
        let (x, y, vw, vh) = fit_rect(cw, ch, w, h);
        let ctx = &self.context;
        unsafe {
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(src.srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.params.clone())]));
        }
        let (fx, fy, fw, fh) = (x as f32, y as f32, vw as f32, vh as f32);
        self.pass(&self.y_rtv, &self.ps_y, [16.0 / 255.0, 0.0, 0.0, 0.0], [0.25 / fw, 0.25 / fh], [fx, fy, fw, fh]);
        self.pass(
            &self.uv_rtv,
            &self.ps_uv,
            [128.0 / 255.0, 128.0 / 255.0, 0.0, 0.0],
            [0.5 / fw, 0.5 / fh],
            [fx / 2.0, fy / 2.0, fw / 2.0, fh / 2.0],
        );
        unsafe {
            ctx.OMSetRenderTargets(None, None);
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.CopyResource(&self.y_stage, &self.y_rt);
            ctx.CopyResource(&self.uv_stage, &self.uv_rt);
        }

        let mut ym = D3D11_MAPPED_SUBRESOURCE::default();
        let mut uvm = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { ctx.Map(&self.y_stage, 0, D3D11_MAP_READ, 0, Some(&mut ym))? };
        if let Err(e) = unsafe { ctx.Map(&self.uv_stage, 0, D3D11_MAP_READ, 0, Some(&mut uvm)) } {
            unsafe { ctx.Unmap(&self.y_stage, 0) };
            return Err(e);
        }
        let (w, h) = (w as usize, h as usize);
        ring.publish(|slot| {
            for r in 0..h {
                let src = unsafe { std::slice::from_raw_parts((ym.pData as *const u8).add(r * ym.RowPitch as usize), w) };
                slot[r * w..][..w].copy_from_slice(src);
            }
            for r in 0..h / 2 {
                let src = unsafe { std::slice::from_raw_parts((uvm.pData as *const u8).add(r * uvm.RowPitch as usize), w) };
                slot[w * h + r * w..][..w].copy_from_slice(src);
            }
        });
        unsafe {
            ctx.Unmap(&self.uv_stage, 0);
            ctx.Unmap(&self.y_stage, 0);
        }
        Ok(true)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

#[cfg(test)]
mod tests {
    use super::fit_rect;

    #[test]
    fn letterbox() {
        assert_eq!(fit_rect(2560, 1440, 1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(fit_rect(1920, 1200, 1920, 1080), (96, 0, 1728, 1080));
        assert_eq!(fit_rect(3440, 1440, 1920, 1080), (0, 138, 1920, 802));
        assert_eq!(fit_rect(1280, 1024, 1920, 1080), (284, 0, 1350, 1080));
    }
}
