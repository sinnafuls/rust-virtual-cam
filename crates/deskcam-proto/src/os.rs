//! Windows version checks that decide which camera backend can work.

use std::sync::OnceLock;

use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

/// First Windows 11 build; `MFCreateVirtualCamera` exists from here on.
pub const WIN11_BUILD: u32 = 22000;
/// Windows 10 1903: first build with `IGraphicsCaptureItemInterop::CreateForMonitor`.
pub const MIN_BUILD: u32 = 18362;
/// First build with `GraphicsCaptureAccess` / `IsBorderRequired` (UniversalApiContract 12).
pub const BORDERLESS_BUILD: u32 = 20348;

// `GetVersionExW` reports 6.2 to unmanifested processes; `RtlGetVersion` always tells the truth.
windows_core::link!("ntdll.dll" "system" fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32);

/// The running OS build number (e.g. 19045 for Windows 10 22H2, 22631 for Windows 11 23H2).
pub fn build() -> u32 {
    static BUILD: OnceLock<u32> = OnceLock::new();
    *BUILD.get_or_init(|| {
        let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
        match unsafe { RtlGetVersion(&mut info) } {
            0 => info.dwBuildNumber,
            _ => 0,
        }
    })
}

/// Whether the Media Foundation virtual camera API (Windows 11) is available.
pub fn has_mf_virtual_camera() -> bool {
    build() >= WIN11_BUILD
}
