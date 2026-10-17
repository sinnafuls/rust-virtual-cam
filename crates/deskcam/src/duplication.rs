//! DXGI Desktop Duplication of one monitor: the screen capture that draws no border on Windows 10
//! (Windows.Graphics.Capture can only drop its border from build 20348).
//!
//! Duplicated frames carry no cursor, so the pointer shape and position are tracked here and
//! [`Duplication::draw_cursor`] blends them into the caller's copy of the desktop on the CPU. That
//! keeps XOR (inverting) cursors exact, which a GPU blend state cannot express.

use windows::Win32::Foundation::{E_ACCESSDENIED, E_FAIL, POINT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows_core::Interface;

/// The adapter driving `hmon` and its output; duplication needs a device on that adapter.
pub fn find_output(hmon: HMONITOR) -> windows_core::Result<(IDXGIAdapter1, IDXGIOutput1)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    for a in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else { break };
        for o in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else { break };
            if unsafe { output.GetDesc()? }.Monitor == hmon {
                return Ok((adapter, output.cast()?));
            }
        }
    }
    Err(windows_core::Error::new(E_FAIL, "no DXGI output for this display"))
}

struct Pointer {
    visible: bool,
    pos: POINT,
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    shape: Vec<u8>,
}

pub struct Duplication {
    output: IDXGIOutput1,
    dup: Option<IDXGIOutputDuplication>,
    /// The desktop without cursor; Windows only reports what changed, so it is kept between frames.
    desk: ID3D11Texture2D,
    width: u32,
    height: u32,
    pointer: Pointer,
    /// CPU read/write copy of the desktop under the cursor.
    stage: Option<(ID3D11Texture2D, u32, u32)>,
}

fn duplicate(output: &IDXGIOutput1, device: &ID3D11Device) -> windows_core::Result<(IDXGIOutputDuplication, u32, u32)> {
    let dup = unsafe { output.DuplicateOutput(device)? };
    let desc = unsafe { dup.GetDesc() };
    // Rotated displays arrive unrotated; Windows.Graphics.Capture handles them instead.
    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
        return Err(windows_core::Error::new(E_FAIL, "rotated display"));
    }
    Ok((dup, desc.ModeDesc.Width, desc.ModeDesc.Height))
}

impl Duplication {
    pub fn new(device: &ID3D11Device, output: IDXGIOutput1) -> windows_core::Result<Duplication> {
        let (dup, width, height) = duplicate(&output, device)?;
        let desk = crate::capture::texture(
            device,
            width,
            height,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_DEFAULT,
            D3D11_BIND_FLAG(0),
            D3D11_CPU_ACCESS_FLAG(0),
        )?;
        Ok(Duplication {
            output,
            dup: Some(dup),
            desk,
            width,
            height,
            pointer: Pointer { visible: false, pos: POINT::default(), info: Default::default(), shape: Vec::new() },
            stage: None,
        })
    }

    pub fn desktop(&self) -> (&ID3D11Texture2D, u32, u32) {
        (&self.desk, self.width, self.height)
    }

    /// Waits up to `timeout_ms` for the desktop or the pointer to change; `Ok(true)` when either
    /// did. While the secure desktop (UAC, lock screen) is up, duplication is unavailable and this
    /// keeps the last image.
    pub fn acquire(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext, timeout_ms: u32) -> windows_core::Result<bool> {
        let dup = match &self.dup {
            Some(d) => d.clone(),
            None => match duplicate(&self.output, device) {
                Ok((d, w, h)) if (w, h) == (self.width, self.height) => {
                    self.dup = Some(d.clone());
                    d
                }
                // A mode change: start over at the new size.
                Ok(_) => return Err(windows_core::Error::new(DXGI_ERROR_ACCESS_LOST, "display mode changed")),
                Err(e) if e.code() == E_ACCESSDENIED || e.code() == DXGI_ERROR_UNSUPPORTED => {
                    std::thread::sleep(std::time::Duration::from_millis(timeout_ms.into()));
                    return Ok(false);
                }
                Err(e) => return Err(e),
            },
        };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.dup = None;
                return Ok(false);
            }
            Err(e) => return Err(e),
        }
        let result = self.take_frame(&dup, context, &info, resource);
        unsafe {
            let _ = dup.ReleaseFrame();
        }
        result.map(|()| info.LastPresentTime != 0 || info.LastMouseUpdateTime != 0)
    }

    fn take_frame(
        &mut self,
        dup: &IDXGIOutputDuplication,
        context: &ID3D11DeviceContext,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> windows_core::Result<()> {
        if info.LastPresentTime != 0
            && let Some(resource) = resource
        {
            let tex: ID3D11Texture2D = resource.cast()?;
            unsafe { context.CopyResource(&self.desk, &tex) };
        }
        if info.LastMouseUpdateTime != 0 {
            self.pointer.visible = info.PointerPosition.Visible.as_bool();
            self.pointer.pos = info.PointerPosition.Position;
        }
        if info.PointerShapeBufferSize > 0 {
            let mut shape = vec![0u8; info.PointerShapeBufferSize as usize];
            let (mut needed, mut shape_info) = (0u32, DXGI_OUTDUPL_POINTER_SHAPE_INFO::default());
            unsafe {
                dup.GetFramePointerShape(shape.len() as u32, shape.as_mut_ptr() as *mut _, &mut needed, &mut shape_info)?
            };
            self.pointer.shape = shape;
            self.pointer.info = shape_info;
        }
        Ok(())
    }

    /// Blends the pointer into `target` (a copy of [`Self::desktop`], same size).
    pub fn draw_cursor(&mut self, device: &ID3D11Device, context: &ID3D11DeviceContext, target: &ID3D11Texture2D) -> windows_core::Result<()> {
        let p = &self.pointer;
        if !p.visible || p.shape.is_empty() {
            return Ok(());
        }
        let monochrome = p.info.Type == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32;
        let (cw, ch) = (p.info.Width as i32, if monochrome { p.info.Height as i32 / 2 } else { p.info.Height as i32 });
        // Clip the cursor rectangle to the desktop.
        let (x0, y0) = (p.pos.x.max(0), p.pos.y.max(0));
        let (x1, y1) = ((p.pos.x + cw).min(self.width as i32), (p.pos.y + ch).min(self.height as i32));
        if x0 >= x1 || y0 >= y1 {
            return Ok(());
        }
        let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
        let stage = match &self.stage {
            Some((tex, sw, sh)) if *sw >= w && *sh >= h => tex.clone(),
            _ => {
                let (sw, sh) = (w.max(64), h.max(64));
                let tex = crate::capture::texture(
                    device,
                    sw,
                    sh,
                    windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_USAGE_STAGING,
                    D3D11_BIND_FLAG(0),
                    D3D11_CPU_ACCESS_FLAG(D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0),
                )?;
                self.stage = Some((tex.clone(), sw, sh));
                tex
            }
        };
        let region = D3D11_BOX { left: x0 as u32, top: y0 as u32, front: 0, right: x1 as u32, bottom: y1 as u32, back: 1 };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context.CopySubresourceRegion(&stage, 0, 0, 0, 0, target, 0, Some(&region));
            context.Map(&stage, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut mapped))?;
        }
        let pitch = mapped.RowPitch as usize;
        let pixels = unsafe { std::slice::from_raw_parts_mut(mapped.pData as *mut u8, pitch * h as usize) };
        let (sx, sy) = ((x0 - p.pos.x) as usize, (y0 - p.pos.y) as usize);
        blend(pixels, pitch, w as usize, h as usize, &p.shape, &p.info, sx, sy);
        unsafe {
            context.Unmap(&stage, 0);
            let all = D3D11_BOX { left: 0, top: 0, front: 0, right: w, bottom: h, back: 1 };
            context.CopySubresourceRegion(target, 0, x0 as u32, y0 as u32, 0, &stage, 0, Some(&all));
        }
        Ok(())
    }
}

/// Blends a `w`×`h` window of the pointer shape, starting at shape pixel (`sx`, `sy`), into BGRA
/// `dst` rows of `pitch` bytes.
#[allow(clippy::too_many_arguments)]
fn blend(dst: &mut [u8], pitch: usize, w: usize, h: usize, shape: &[u8], info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, sx: usize, sy: usize) {
    let spitch = info.Pitch as usize;
    let mask_rows = info.Height as usize / 2;
    for y in 0..h {
        let row = &mut dst[y * pitch..y * pitch + w * 4];
        for x in 0..w {
            let px = &mut row[x * 4..x * 4 + 4];
            let (cx, cy) = (sx + x, sy + y);
            match info.Type {
                t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 as u32 => {
                    let bit = 0x80u8 >> (cx % 8);
                    let and = shape[cy * spitch + cx / 8] & bit != 0;
                    let xor = shape[(cy + mask_rows) * spitch + cx / 8] & bit != 0;
                    for c in &mut px[..3] {
                        *c = (if and { *c } else { 0 }) ^ (if xor { 0xFF } else { 0 });
                    }
                }
                t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 as u32 => {
                    let s = &shape[cy * spitch + cx * 4..cy * spitch + cx * 4 + 4];
                    for c in 0..3 {
                        px[c] = if s[3] == 0 { s[c] } else { px[c] ^ s[c] };
                    }
                }
                _ => {
                    let s = &shape[cy * spitch + cx * 4..cy * spitch + cx * 4 + 4];
                    let a = s[3] as u32;
                    for c in 0..3 {
                        px[c] = ((s[c] as u32 * a + px[c] as u32 * (255 - a) + 127) / 255) as u8;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(ty: DXGI_OUTDUPL_POINTER_SHAPE_TYPE, w: u32, h: u32, pitch: u32) -> DXGI_OUTDUPL_POINTER_SHAPE_INFO {
        DXGI_OUTDUPL_POINTER_SHAPE_INFO { Type: ty.0 as u32, Width: w, Height: h, Pitch: pitch, HotSpot: POINT::default() }
    }

    #[test]
    fn color_cursor_alpha_blends() {
        let mut dst = [100u8, 100, 100, 255, 100, 100, 100, 255];
        let shape = [200u8, 0, 50, 255, 200, 0, 50, 0];
        blend(&mut dst, 8, 2, 1, &shape, &info(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, 2, 1, 8), 0, 0);
        assert_eq!(&dst[..3], &[200, 0, 50]);
        assert_eq!(&dst[4..7], &[100, 100, 100]);
    }

    #[test]
    fn monochrome_and_xor_masks() {
        // Two pixels: AND keeps / XOR off (transparent), AND clears / XOR on (white).
        let shape = [0b1000_0000u8, 0b0100_0000];
        let mut dst = [30u8, 40, 50, 255, 30, 40, 50, 255];
        blend(&mut dst, 8, 2, 1, &shape, &info(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME, 2, 2, 1), 0, 0);
        assert_eq!(&dst[..3], &[30, 40, 50]);
        assert_eq!(&dst[4..7], &[255, 255, 255]);
    }

    #[test]
    fn masked_color_replaces_or_inverts() {
        let shape = [10u8, 20, 30, 0, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut dst = [1u8, 2, 3, 255, 1, 2, 3, 255];
        blend(&mut dst, 8, 2, 1, &shape, &info(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR, 2, 1, 8), 0, 0);
        assert_eq!(&dst[..3], &[10, 20, 30]);
        assert_eq!(&dst[4..7], &[254, 253, 252]);
    }

    #[test]
    fn clipped_window_reads_shape_offset() {
        let mut shape = vec![0u8; 2 * 2 * 4];
        shape[(2 + 1) * 4..(2 + 1) * 4 + 4].copy_from_slice(&[9, 8, 7, 255]);
        let mut dst = [0u8; 4];
        blend(&mut dst, 4, 1, 1, &shape, &info(DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, 2, 2, 8), 1, 1);
        assert_eq!(&dst[..3], &[9, 8, 7]);
    }
}
