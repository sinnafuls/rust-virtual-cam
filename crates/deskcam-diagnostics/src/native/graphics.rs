//! Prerequisites of deskcam.exe's desktop capture: Windows.Graphics.Capture support,
//! `IGraphicsCaptureItemInterop::CreateForMonitor`, and a hardware Direct3D 11 device.
//!
//! Capture is never started: capture items, a frame pool and a session object are created and
//! closed without `StartCapture`, so no frames are produced and no capture border appears.
//! Optional newer APIs (cursor toggle, borderless capture, update interval) are only queried
//! through `ApiInformation`; Windows 10 lacking them is reported, not failed. Limitations: the
//! device is created on the default adapter like deskcam.exe does; cross-adapter monitors rely
//! on Windows' own copy, which is not exercised without starting capture.

use deskcam_proto::os;
use windows::Foundation::Metadata::ApiInformation;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{E_FAIL, HMODULE, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL,
    D3D_FEATURE_LEVEL_10_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND,
    IDXGIAdapter, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows_core::{BOOL, HSTRING, Interface};

use super::{Report, hr, step, wide_to_string};

const SESSION: &str = "Windows.Graphics.Capture.GraphicsCaptureSession";
/// `MONITORINFOF_PRIMARY`.
const PRIMARY: u32 = 1;

pub(super) fn run(r: &mut Report) {
    let build = os::build();
    if build >= os::MIN_BUILD {
        r.pass(format!(
            "Windows build {build} >= {} (Windows 10 1903, first with CreateForMonitor)",
            os::MIN_BUILD
        ));
    } else {
        r.fail(format!(
            "Windows build {build} < {} (Windows 10 1903): monitor capture is unavailable",
            os::MIN_BUILD
        ));
    }
    let winrt = step(r, "RoInitialize(RO_INIT_MULTITHREADED)", || unsafe {
        RoInitialize(RO_INIT_MULTITHREADED)
    })
    .is_some();
    let interop = if winrt {
        capture_api(r)
    } else {
        r.skip("Windows.Graphics.Capture checks: the Windows Runtime could not be initialized");
        None
    };
    adapters(r);
    let d3d = device(r);
    monitors(r, interop.as_ref(), d3d.as_ref());
}

/// `ApiInformation`, `IsSupported` and the monitor interop factory.
fn capture_api(r: &mut Report) -> Option<IGraphicsCaptureItemInterop> {
    api_information(r);
    r.stage("GraphicsCaptureSession::IsSupported()");
    match GraphicsCaptureSession::IsSupported() {
        Ok(true) => r.pass("GraphicsCaptureSession::IsSupported() = true"),
        Ok(false) => r.fail("GraphicsCaptureSession::IsSupported() = false: Windows.Graphics.Capture is unavailable on this system"),
        Err(e) => r.fail(format!("GraphicsCaptureSession::IsSupported(): {}", hr(&e))),
    }
    let interop = step(
        r,
        "RoGetActivationFactory(GraphicsCaptureItem) as IGraphicsCaptureItemInterop",
        || windows_core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>(),
    );
    if interop.is_some() {
        r.pass("IGraphicsCaptureItemInterop available (CreateForMonitor entry point)");
    }
    interop
}

/// What `ApiInformation` reports for the capture APIs deskcam.exe uses; only the session type
/// itself is required.
fn api_information(r: &mut Report) {
    r.stage("Windows.Foundation.Metadata.ApiInformation queries");
    let contract = HSTRING::from("Windows.Foundation.UniversalApiContract");
    let highest = (1..=30u16).rev().find(|&v| {
        matches!(
            ApiInformation::IsApiContractPresentByMajor(&contract, v),
            Ok(true)
        )
    });
    match highest {
        Some(v) => r.info(format!("UniversalApiContract v{v} present (8 = 1903, 10 = 2004, 12 = build 20348, 14 = Windows 11)")),
        None => r.warn("ApiInformation reports no UniversalApiContract version"),
    }
    match ApiInformation::IsTypePresent(&HSTRING::from(SESSION)) {
        Ok(true) => r.pass(format!("{SESSION} type present")),
        Ok(false) => r.fail(format!("{SESSION} type absent")),
        Err(e) => r.warn(format!(
            "ApiInformation::IsTypePresent({SESSION}): {}",
            hr(&e)
        )),
    }
    let optional = [
        (
            "IsCursorCaptureEnabled",
            "cursor on/off setting (Windows 10 2004+); without it the cursor is always drawn",
        ),
        (
            "IsBorderRequired",
            "borderless capture (build 20348+); without it Windows shows the yellow capture border",
        ),
        (
            "MinUpdateInterval",
            "OS-side frame-rate cap (recent Windows 11); without it DeskCam paces frames itself",
        ),
    ];
    for (property, meaning) in optional {
        match ApiInformation::IsPropertyPresent(&HSTRING::from(SESSION), &HSTRING::from(property)) {
            Ok(present) => r.info(format!(
                "Optional {property}: {} - {meaning}",
                if present { "present" } else { "absent" }
            )),
            Err(e) => r.warn(format!(
                "ApiInformation::IsPropertyPresent({property}): {}",
                hr(&e)
            )),
        }
    }
    match ApiInformation::IsTypePresent(&HSTRING::from(
        "Windows.Graphics.Capture.GraphicsCaptureAccess",
    )) {
        Ok(present) => r.info(format!(
            "Optional GraphicsCaptureAccess: {} (borderless access request; not requested here)",
            if present { "present" } else { "absent" }
        )),
        Err(e) => r.warn(format!(
            "ApiInformation::IsTypePresent(GraphicsCaptureAccess): {}",
            hr(&e)
        )),
    }
}

fn adapter_line(d: &DXGI_ADAPTER_DESC1) -> String {
    format!(
        "'{}' vendor 0x{:04X} device 0x{:04X}, {} MiB dedicated{}",
        wide_to_string(&d.Description),
        d.VendorId,
        d.DeviceId,
        d.DedicatedVideoMemory / (1024 * 1024),
        if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            ", software"
        } else {
            ""
        }
    )
}

fn adapters(r: &mut Report) {
    let Some(factory) = step(r, "CreateDXGIFactory1", || unsafe {
        CreateDXGIFactory1::<IDXGIFactory1>()
    }) else {
        return;
    };
    for i in 0..16 {
        r.stage(&format!("IDXGIFactory1::EnumAdapters1({i})"));
        match unsafe { factory.EnumAdapters1(i) } {
            Ok(adapter) => match unsafe { adapter.GetDesc1() } {
                Ok(d) => r.info(format!("DXGI adapter {i}: {}", adapter_line(&d))),
                Err(e) => r.warn(format!("DXGI adapter {i}: GetDesc1 {}", hr(&e))),
            },
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => {
                r.warn(format!("IDXGIFactory1::EnumAdapters1({i}): {}", hr(&e)));
                break;
            }
        }
    }
}

fn create_device(
    driver: D3D_DRIVER_TYPE,
) -> windows_core::Result<(ID3D11Device, D3D_FEATURE_LEVEL)> {
    let (mut device, mut level) = (None, D3D_FEATURE_LEVEL::default());
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            None,
        )?
    };
    device.map(|d| (d, level)).ok_or_else(|| E_FAIL.into())
}

fn level_name(l: D3D_FEATURE_LEVEL) -> String {
    format!("{}_{}", (l.0 >> 12) & 0xF, (l.0 >> 8) & 0xF)
}

/// The device deskcam.exe creates (default adapter, hardware, BGRA) and its WinRT wrapper.
fn device(r: &mut Report) -> Option<IDirect3DDevice> {
    r.stage("D3D11CreateDevice(default adapter, HARDWARE, BGRA_SUPPORT)");
    let (device, level) = match create_device(D3D_DRIVER_TYPE_HARDWARE) {
        Ok(v) => v,
        Err(e) => {
            r.fail(format!(
                "D3D11CreateDevice(HARDWARE, BGRA): {} - deskcam.exe cannot capture",
                hr(&e)
            ));
            r.stage("D3D11CreateDevice(WARP, BGRA_SUPPORT) for comparison");
            match create_device(D3D_DRIVER_TYPE_WARP) {
                Ok((_, l)) => r.info(format!("WARP (software) device works at feature level {}: the GPU driver is the problem", level_name(l))),
                Err(e) => r.info(format!("WARP device also fails: {}", hr(&e))),
            }
            return None;
        }
    };
    let adapter = device
        .cast::<IDXGIDevice>()
        .and_then(|d| unsafe { d.GetAdapter() })
        .and_then(|a| a.cast::<IDXGIAdapter1>())
        .and_then(|a| unsafe { a.GetDesc1() });
    let adapter_text = match &adapter {
        Ok(d) => adapter_line(d),
        Err(e) => format!("<adapter unknown: {}>", hr(e)),
    };
    if level.0 >= D3D_FEATURE_LEVEL_10_0.0 {
        r.pass(format!(
            "Hardware D3D11 device: feature level {} on {adapter_text}",
            level_name(level)
        ));
    } else {
        r.fail(format!(
            "Hardware D3D11 device only reaches feature level {} on {adapter_text}; DeskCam's shaders need 10_0",
            level_name(level)
        ));
    }
    if adapter.is_ok_and(|d| d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0) {
        r.warn("The default adapter is a software renderer (Microsoft Basic Render Driver?): install the GPU driver");
    }
    step(
        r,
        "CreateDirect3D11DeviceFromDXGIDevice (WinRT IDirect3DDevice)",
        || {
            let dxgi = device.cast::<IDXGIDevice>()?;
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }?.cast::<IDirect3DDevice>()
        },
    )
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    list.push(monitor);
    true.into()
}

/// Capture items for every monitor, plus a frame pool and session that are never started.
fn monitors(
    r: &mut Report,
    interop: Option<&IGraphicsCaptureItemInterop>,
    d3d: Option<&IDirect3DDevice>,
) {
    r.stage("EnumDisplayMonitors");
    let mut list: Vec<HMONITOR> = Vec::new();
    let ok = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut list as *mut Vec<HMONITOR> as isize),
        )
    };
    if !ok.as_bool() {
        r.warn(format!(
            "EnumDisplayMonitors: {}",
            hr(&windows_core::Error::from_thread())
        ));
    }
    if list.is_empty() {
        r.fail("No monitors in this session (disconnected remote session or no active display?)");
        return;
    }
    for (i, &hmon) in list.iter().enumerate() {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
        let device = if unsafe {
            GetMonitorInfoW(hmon, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO)
        }
        .as_bool()
        {
            let m = &info.monitorInfo;
            let device = wide_to_string(&info.szDevice);
            r.info(format!(
                "Monitor {}: {device} ({},{})-({},{}){}",
                i + 1,
                m.rcMonitor.left,
                m.rcMonitor.top,
                m.rcMonitor.right,
                m.rcMonitor.bottom,
                if m.dwFlags & PRIMARY != 0 {
                    " primary"
                } else {
                    ""
                }
            ));
            device
        } else {
            r.warn(format!(
                "Monitor {}: GetMonitorInfoW {}",
                i + 1,
                hr(&windows_core::Error::from_thread())
            ));
            format!("monitor {}", i + 1)
        };
        let Some(interop) = interop else {
            r.skip(format!(
                "CreateForMonitor({device}): interop factory unavailable"
            ));
            continue;
        };
        let stage = format!("IGraphicsCaptureItemInterop::CreateForMonitor({device})");
        r.stage(&stage);
        let item: GraphicsCaptureItem = match unsafe { interop.CreateForMonitor(hmon) } {
            Ok(item) => item,
            Err(e) => {
                r.fail(format!("{stage}: {}", hr(&e)));
                continue;
            }
        };
        let size = item.Size();
        let name = item
            .DisplayName()
            .map(|n| n.to_string())
            .unwrap_or_default();
        match &size {
            Ok(s) => r.pass(format!(
                "Capture item for {device}: '{name}' {}x{}",
                s.Width, s.Height
            )),
            Err(e) => r.warn(format!(
                "Capture item for {device} created but Size() failed: {}",
                hr(e)
            )),
        }
        let (Some(d3d), Ok(size)) = (d3d, size) else {
            r.skip(format!(
                "Frame pool/session for {device}: no Direct3D device or item size"
            ));
            continue;
        };
        let stage = format!(
            "Direct3D11CaptureFramePool::CreateFreeThreaded + CreateCaptureSession({device}), not started"
        );
        r.stage(&stage);
        let created = Direct3D11CaptureFramePool::CreateFreeThreaded(
            d3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .and_then(|pool| {
            let session = pool.CreateCaptureSession(&item);
            if let Ok(s) = &session {
                let _ = s.Close();
            }
            let _ = pool.Close();
            session.map(|_| ())
        });
        match created {
            Ok(()) => r.pass(format!(
                "Frame pool and capture session created for {device} (capture not started)"
            )),
            Err(e) => r.fail(format!("{stage}: {}", hr(&e))),
        }
    }
}
