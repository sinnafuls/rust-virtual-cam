//! Display enumeration and selection.

use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplayMonitors, EnumDisplaySettingsW, GetMonitorInfoW, HDC, HMONITOR,
    MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows_core::{BOOL, PCWSTR};

use crate::config::MonitorSel;

#[derive(Clone, Debug)]
pub struct MonitorInfo {
    pub hmon: HMONITOR,
    /// N of `\\.\DISPLAYN`.
    pub number: u32,
    pub primary: bool,
    pub rect: RECT,
    pub refresh_hz: u32,
}

unsafe extern "system" fn collect(hmon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
    let out = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    if unsafe { GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO) }.as_bool() {
        let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
        let device = String::from_utf16_lossy(&info.szDevice[..len]);
        let number = device.rsplit("DISPLAY").next().and_then(|n| n.parse().ok()).unwrap_or(0);
        let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
        let hz = if unsafe { EnumDisplaySettingsW(PCWSTR(info.szDevice.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool() {
            mode.dmDisplayFrequency
        } else {
            0
        };
        out.push(MonitorInfo {
            hmon,
            number,
            primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            rect: info.monitorInfo.rcMonitor,
            refresh_hz: if hz <= 1 { 60 } else { hz },
        });
    }
    true.into()
}

pub fn enumerate() -> Vec<MonitorInfo> {
    let mut out: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut out as *mut _ as isize));
    }
    out.sort_by_key(|m| m.number);
    out
}

pub fn select(monitors: &[MonitorInfo], sel: MonitorSel) -> Result<&MonitorInfo, String> {
    let found = match sel {
        MonitorSel::Primary => monitors.iter().find(|m| m.primary),
        MonitorSel::Display(n) => monitors.iter().find(|m| m.number == n),
    };
    found.ok_or_else(|| {
        let list: Vec<String> = monitors.iter().map(|m| m.number.to_string()).collect();
        format!("monitor {sel:?} not found; available displays: {}", list.join(", "))
    })
}

pub fn describe(m: &MonitorInfo) -> String {
    format!(
        "display {}{}: {}x{} at ({}, {}), {} Hz",
        m.number,
        if m.primary { " (primary)" } else { "" },
        m.rect.right - m.rect.left,
        m.rect.bottom - m.rect.top,
        m.rect.left,
        m.rect.top,
        m.refresh_hz
    )
}
