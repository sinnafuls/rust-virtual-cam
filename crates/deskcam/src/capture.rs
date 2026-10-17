//! Capture of one monitor (Windows.Graphics.Capture, or Desktop Duplication where WGC would draw
//! a border on screen), scaled/letterboxed and converted to NV12 (BT.601 limited range) on the
//! GPU, then read back into the shared ring.
//!
//! Cost model: nothing runs until Windows signals a new desktop frame (`FrameArrived`), the
//! pipeline state is set once, the letterbox bars are cleared only when the geometry changes,
//! and the device runs at the lowest GPU scheduling priority so games and the compositor win.

use std::sync::Arc;

use deskcam_proto::{FrameRing, StreamInfo, os};
use windows::Foundation::{TimeSpan, TypedEventHandler};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind, GraphicsCaptureItem,
    GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{CloseHandle, E_FAIL, HANDLE, HMODULE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows_core::Interface;

use crate::config::CaptureSel;
use crate::duplication::{self, Duplication};
use crate::log::log;

// Bytecode compiled from src/nv12.hlsl by build.rs.
const VS_MAIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vs_main.dxbc"));
const PS_Y: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ps_y.dxbc"));
const PS_UV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ps_uv.dxbc"));

/// Lowest D3D GPU thread priority: our conversion yields to anything the user is looking at.
const GPU_PRIORITY: i32 = -7;

/// Letterbox rectangle `(x, y, w, h)` of a `sw`×`sh` source inside `dw`×`dh`; all even.
pub fn fit_rect(sw: u32, sh: u32, dw: u32, dh: u32) -> (u32, u32, u32, u32) {
    let (sw, sh, dw, dh) = (sw.max(1) as u64, sh.max(1) as u64, dw as u64, dh as u64);
    let (w, h) = if sw * dh >= sh * dw { (dw, (sh * dw / sw) & !1) } else { ((sw * dh / sh) & !1, dh) };
    let (w, h) = (w.max(2), h.max(2));
    ((((dw - w) / 2) & !1) as u32, (((dh - h) / 2) & !1) as u32, w as u32, h as u32)
}

/// Matches `cbuffer Params` in nv12.hlsl.
#[repr(C)]
struct Params {
    tap_y: [f32; 2],
    tap_uv: [f32; 2],
}

struct Source {
    tex: ID3D11Texture2D,
    width: u32,
    height: u32,
}

/// Auto-reset event signalled by `FrameArrived`. Shared with the handler so the handle
/// outlives any callback still in flight.
struct FrameEvent(HANDLE);

// SAFETY: an event handle may be signalled and waited on from any thread.
unsafe impl Send for FrameEvent {}
unsafe impl Sync for FrameEvent {}

impl Drop for FrameEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Windows.Graphics.Capture session; Windows signals `frame_event` for every new desktop frame.
struct Wgc {
    d3d: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_token: i64,
    frame_event: Arc<FrameEvent>,
    pool_size: SizeInt32,
}

impl Drop for Wgc {
    fn drop(&mut self) {
        let _ = self.pool.RemoveFrameArrived(self.frame_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// Desktop Duplication: `wait_frame` already acquires, so it hands its outcome to `tick`.
struct Dup {
    dup: Duplication,
    cursor: bool,
    fresh: bool,
    error: Option<windows_core::Error>,
}

enum Grab {
    Wgc(Wgc),
    Dup(Dup),
}

pub struct Capture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    grab: Grab,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    params: ID3D11Buffer,
    y_rt: ID3D11Texture2D,
    y_rtv: ID3D11RenderTargetView,
    uv_rt: ID3D11Texture2D,
    uv_rtv: ID3D11RenderTargetView,
    y_stage: ID3D11Texture2D,
    uv_stage: ID3D11Texture2D,
    /// The staging textures hold the last converted frame, so a reader that joins on a static
    /// screen gets it without a recapture or reconversion.
    converted: bool,
    src: Option<Source>,
    /// Letterbox rectangle of the last frame; bars are re-cleared only when it changes.
    fit: Option<(u32, u32, u32, u32)>,
    out: StreamInfo,
}

fn fail() -> windows_core::Error {
    windows_core::Error::from(E_FAIL)
}

pub(crate) fn texture(
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
    tex.ok_or_else(fail)
}

fn rtv(device: &ID3D11Device, tex: &ID3D11Texture2D) -> windows_core::Result<ID3D11RenderTargetView> {
    let mut view = None;
    unsafe { device.CreateRenderTargetView(tex, None, Some(&mut view))? };
    view.ok_or_else(fail)
}

/// A device on `adapter` (Desktop Duplication needs the one driving the display), or the default.
fn create_device(adapter: Option<&IDXGIAdapter>) -> windows_core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let (mut device, mut context) = (None, None);
    let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
    unsafe {
        D3D11CreateDevice(
            adapter,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?
    };
    let device = device.ok_or_else(fail)?;
    if let Err(e) = unsafe { device.cast::<IDXGIDevice>()?.SetGPUThreadPriority(GPU_PRIORITY) } {
        log!("GPU priority unavailable: {e}");
    }
    Ok((device, context.ok_or_else(fail)?))
}

/// (Re)creates the shader input texture when the captured size changes.
fn ensure_source(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &mut Option<Source>,
    width: u32,
    height: u32,
) -> windows_core::Result<()> {
    if src.as_ref().is_some_and(|s| s.width == width && s.height == height) {
        return Ok(());
    }
    let tex = texture(
        device,
        width,
        height,
        DXGI_FORMAT_B8G8R8A8_UNORM,
        D3D11_USAGE_DEFAULT,
        D3D11_BIND_SHADER_RESOURCE,
        D3D11_CPU_ACCESS_FLAG(0),
    )?;
    let mut srv = None;
    unsafe {
        device.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
        // Stays bound; copies into the texture do not conflict with an SRV binding.
        context.PSSetShaderResources(0, Some(&[Some(srv.ok_or_else(fail)?)]));
    }
    *src = Some(Source { tex, width, height });
    Ok(())
}

impl Capture {
    pub fn new(hmon: HMONITOR, out: StreamInfo, fps: u32, cursor: bool, sel: CaptureSel) -> windows_core::Result<Capture> {
        // Windows.Graphics.Capture draws a border on screen until it can be turned off (build 20348).
        let duplicate = match sel {
            CaptureSel::Auto => os::build() < os::BORDERLESS_BUILD,
            CaptureSel::Wgc => false,
            CaptureSel::Duplication => true,
        };
        if duplicate {
            match Self::with_duplication(hmon, out, cursor) {
                Ok(c) => {
                    log!("capturing with Desktop Duplication");
                    return Ok(c);
                }
                Err(e) if sel == CaptureSel::Auto => {
                    log!("Desktop Duplication unavailable ({e}); using Windows.Graphics.Capture")
                }
                Err(e) => return Err(e),
            }
        }
        Self::with_wgc(hmon, out, fps, cursor)
    }

    fn with_duplication(hmon: HMONITOR, out: StreamInfo, cursor: bool) -> windows_core::Result<Capture> {
        let (adapter, output) = duplication::find_output(hmon)?;
        let (device, context) = create_device(Some(&adapter.cast()?))?;
        let dup = Duplication::new(&device, output)?;
        Self::build(device, context, Grab::Dup(Dup { dup, cursor, fresh: false, error: None }), out)
    }

    fn with_wgc(hmon: HMONITOR, out: StreamInfo, fps: u32, cursor: bool) -> windows_core::Result<Capture> {
        let (device, context) = create_device(None)?;
        let d3d: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&device.cast::<IDXGIDevice>()?)? }.cast()?;

        let interop = windows_core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(hmon)? };
        let pool_size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, pool_size)?;
        let frame_event = Arc::new(FrameEvent(unsafe { CreateEventW(None, false, false, None)? }));
        let handler_event = frame_event.clone();
        let frame_token = pool.FrameArrived(&TypedEventHandler::new(move |_, _| {
            unsafe { SetEvent(handler_event.0) }
        }))?;
        let session = pool.CreateCaptureSession(&item)?;
        // IsCursorCaptureEnabled needs Windows 10 2004; older builds always draw the cursor.
        if let Err(e) = session.SetIsCursorCaptureEnabled(cursor) {
            log!("cursor setting unavailable: {e}");
        }
        // Windows 10 has no borderless capture: the yellow border shows on screen (not in frames).
        if os::build() >= os::BORDERLESS_BUILD {
            let borderless = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)
                .and_then(|op| op.join())
                .and_then(|_| session.SetIsBorderRequired(false));
            if let Err(e) = borderless {
                log!("borderless capture unavailable: {e}");
            }
        }
        // Windows copies at most `fps` desktop frames per second for us.
        if let Err(e) = session.SetMinUpdateInterval(TimeSpan { Duration: 10_000_000 / fps.max(1) as i64 }) {
            log!("MinUpdateInterval unavailable: {e}");
        }
        let wgc = Wgc { d3d, pool, session, frame_token, frame_event, pool_size };
        let capture = Self::build(device, context, Grab::Wgc(wgc), out)?;
        if let Grab::Wgc(w) = &capture.grab {
            w.session.StartCapture()?;
        }
        Ok(capture)
    }

    /// The conversion pipeline shared by both capture methods.
    fn build(device: ID3D11Device, context: ID3D11DeviceContext, grab: Grab, out: StreamInfo) -> windows_core::Result<Capture> {
        let (mut vs, mut ps_y, mut ps_uv, mut sampler, mut params) = (None, None, None, None, None);
        unsafe {
            device.CreateVertexShader(VS_MAIN, None, Some(&mut vs))?;
            device.CreatePixelShader(PS_Y, None, Some(&mut ps_y))?;
            device.CreatePixelShader(PS_UV, None, Some(&mut ps_uv))?;
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
        let (vs, sampler, params) = (vs.ok_or_else(fail)?, sampler.ok_or_else(fail)?, params.ok_or_else(fail)?);
        // Pipeline state that never changes: set once. Nothing else uses this context.
        unsafe {
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&vs, None);
            context.PSSetSamplers(0, Some(&[Some(sampler)]));
            context.PSSetConstantBuffers(0, Some(&[Some(params.clone())]));
        }

        let (w, h) = (out.width, out.height);
        let none = D3D11_CPU_ACCESS_FLAG(0);
        let y_rt = texture(&device, w, h, DXGI_FORMAT_R8_UNORM, D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET, none)?;
        let uv_rt = texture(&device, w / 2, h / 2, DXGI_FORMAT_R8G8_UNORM, D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET, none)?;
        let y_stage = texture(&device, w, h, DXGI_FORMAT_R8_UNORM, D3D11_USAGE_STAGING, D3D11_BIND_FLAG(0), D3D11_CPU_ACCESS_READ)?;
        let uv_stage =
            texture(&device, w / 2, h / 2, DXGI_FORMAT_R8G8_UNORM, D3D11_USAGE_STAGING, D3D11_BIND_FLAG(0), D3D11_CPU_ACCESS_READ)?;
        let y_rtv = rtv(&device, &y_rt)?;
        let uv_rtv = rtv(&device, &uv_rt)?;

        Ok(Capture {
            ps_y: ps_y.ok_or_else(fail)?,
            ps_uv: ps_uv.ok_or_else(fail)?,
            params,
            device,
            context,
            grab,
            y_rt,
            y_rtv,
            uv_rt,
            uv_rtv,
            y_stage,
            uv_stage,
            converted: false,
            src: None,
            fit: None,
            out,
        })
    }

    /// Blocks until Windows delivers a new desktop frame or `timeout_ms` passes.
    pub fn wait_frame(&mut self, timeout_ms: u32) -> bool {
        match &mut self.grab {
            Grab::Wgc(w) => unsafe { WaitForSingleObject(w.frame_event.0, timeout_ms) == WAIT_OBJECT_0 },
            Grab::Dup(d) => {
                match d.dup.acquire(&self.device, &self.context, timeout_ms) {
                    Ok(changed) => d.fresh |= changed,
                    Err(e) => d.error = Some(e),
                }
                d.fresh || d.error.is_some()
            }
        }
    }

    fn pass(&self, rtv: &ID3D11RenderTargetView, ps: &ID3D11PixelShader, clear: Option<[f32; 4]>, vp: [f32; 4]) {
        let ctx = &self.context;
        unsafe {
            if let Some(color) = clear {
                ctx.ClearRenderTargetView(rtv, &color);
            }
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

    /// Copies the newest Windows.Graphics.Capture frame into the source texture; its size, or
    /// `None` when the screen produced no new frame.
    fn grab_wgc(
        w: &mut Wgc,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &mut Option<Source>,
    ) -> windows_core::Result<Option<(u32, u32)>> {
        let mut latest = None;
        while let Ok(frame) = w.pool.TryGetNextFrame() {
            latest = Some(frame);
        }
        let Some(frame) = latest else { return Ok(None) };

        let content = frame.ContentSize()?;
        let (cw, ch) = (content.Width.max(1) as u32, content.Height.max(1) as u32);
        let surface_tex: ID3D11Texture2D =
            unsafe { frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface()? };
        let mut surface_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { surface_tex.GetDesc(&mut surface_desc) };
        let (cw, ch) = (cw.min(surface_desc.Width), ch.min(surface_desc.Height));
        ensure_source(device, context, src, cw, ch)?;
        let src = src.as_ref().ok_or_else(fail)?;
        unsafe {
            context.CopySubresourceRegion(
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
        if content.Width != w.pool_size.Width || content.Height != w.pool_size.Height {
            w.pool.Recreate(&w.d3d, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, content)?;
            w.pool_size = content;
        }
        Ok(Some((cw, ch)))
    }

    /// Copies the duplicated desktop plus cursor into the source texture.
    fn grab_dup(
        d: &mut Dup,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &mut Option<Source>,
    ) -> windows_core::Result<Option<(u32, u32)>> {
        if let Some(e) = d.error.take() {
            return Err(e);
        }
        if !std::mem::take(&mut d.fresh) {
            return Ok(None);
        }
        let (desk, w, h) = d.dup.desktop();
        ensure_source(device, context, src, w, h)?;
        let src = src.as_ref().ok_or_else(fail)?;
        unsafe { context.CopyResource(&src.tex, desk) };
        if d.cursor {
            d.dup.draw_cursor(device, context, &src.tex)?;
        }
        Ok(Some((w, h)))
    }

    /// Publishes the newest captured frame into every ring; `Ok(false)` when the screen produced no
    /// new frame.
    pub fn tick<'r>(&mut self, rings: impl IntoIterator<Item = &'r FrameRing>) -> windows_core::Result<bool> {
        let grabbed = match &mut self.grab {
            Grab::Wgc(w) => Self::grab_wgc(w, &self.device, &self.context, &mut self.src)?,
            Grab::Dup(d) => Self::grab_dup(d, &self.device, &self.context, &mut self.src)?,
        };
        let Some((cw, ch)) = grabbed else { return Ok(false) };

        let (w, h) = (self.out.width, self.out.height);
        let fit = fit_rect(cw, ch, w, h);
        let (x, y, vw, vh) = fit;
        let (fx, fy, fw, fh) = (x as f32, y as f32, vw as f32, vh as f32);
        // Bars outside the viewport keep their black until the geometry changes.
        let changed = self.fit != Some(fit);
        if changed {
            let params = Params { tap_y: [0.25 / fw, 0.25 / fh], tap_uv: [0.5 / fw, 0.5 / fh] };
            unsafe { self.context.UpdateSubresource(&self.params, 0, None, &params as *const _ as *const _, 0, 0) };
            self.fit = Some(fit);
        }
        let clear_y = changed.then_some([16.0 / 255.0, 0.0, 0.0, 0.0]);
        let clear_uv = changed.then_some([128.0 / 255.0, 128.0 / 255.0, 0.0, 0.0]);
        self.pass(&self.y_rtv, &self.ps_y, clear_y, [fx, fy, fw, fh]);
        self.pass(&self.uv_rtv, &self.ps_uv, clear_uv, [fx / 2.0, fy / 2.0, fw / 2.0, fh / 2.0]);
        let ctx = &self.context;
        unsafe {
            ctx.OMSetRenderTargets(None, None);
            ctx.CopyResource(&self.y_stage, &self.y_rt);
            ctx.CopyResource(&self.uv_stage, &self.uv_rt);
        }
        self.publish_stages(rings)?;
        self.converted = true;
        Ok(true)
    }

    /// Publishes the last converted frame again into `rings`, straight from the staging textures;
    /// `Ok(false)` when no frame has been converted yet.
    pub fn republish<'r>(&self, rings: impl IntoIterator<Item = &'r FrameRing>) -> windows_core::Result<bool> {
        if !self.converted {
            return Ok(false);
        }
        self.publish_stages(rings)?;
        Ok(true)
    }

    /// Copies the staged NV12 planes into a fresh slot of every ring.
    fn publish_stages<'r>(&self, rings: impl IntoIterator<Item = &'r FrameRing>) -> windows_core::Result<()> {
        let ctx = &self.context;
        let mut ym = D3D11_MAPPED_SUBRESOURCE::default();
        let mut uvm = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { ctx.Map(&self.y_stage, 0, D3D11_MAP_READ, 0, Some(&mut ym))? };
        if let Err(e) = unsafe { ctx.Map(&self.uv_stage, 0, D3D11_MAP_READ, 0, Some(&mut uvm)) } {
            unsafe { ctx.Unmap(&self.y_stage, 0) };
            return Err(e);
        }
        let (w, h) = (self.out.width as usize, self.out.height as usize);
        for ring in rings {
            ring.publish(|slot| {
                let (y_plane, uv_plane) = slot.split_at_mut(w * h);
                copy_plane(y_plane, ym.pData as *const u8, ym.RowPitch as usize, w, h);
                copy_plane(uv_plane, uvm.pData as *const u8, uvm.RowPitch as usize, w, h / 2);
            });
        }
        unsafe {
            ctx.Unmap(&self.uv_stage, 0);
            ctx.Unmap(&self.y_stage, 0);
        }
        Ok(())
    }
}

/// Copies `rows` rows of `width` bytes from a mapped texture with `pitch` into a packed plane.
fn copy_plane(dst: &mut [u8], src: *const u8, pitch: usize, width: usize, rows: usize) {
    if pitch == width {
        dst[..width * rows].copy_from_slice(unsafe { std::slice::from_raw_parts(src, width * rows) });
        return;
    }
    for (r, row) in dst.chunks_exact_mut(width).take(rows).enumerate() {
        row.copy_from_slice(unsafe { std::slice::from_raw_parts(src.add(r * pitch), width) });
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
