//! Registers the session-lifetime virtual camera backed by `deskcam_source.dll` (Windows 11).
//!
//! `MFCreateVirtualCamera` only exists on Windows 11. The `windows` crate imports functions at
//! load time, so calling it directly would stop `deskcam.exe` from starting at all on Windows 10.
//! It is resolved at runtime instead.

use std::ffi::c_void;

use deskcam_proto::CLSID_STR;
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MFVirtualCameraAccess, MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime,
    MFVirtualCameraLifetime_Session, MFVirtualCameraType, MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows_core::{GUID, HRESULT, HSTRING, Interface, PCWSTR, s, w};

use crate::log::log;

type CreateVirtualCameraFn = unsafe extern "system" fn(
    MFVirtualCameraType,
    MFVirtualCameraLifetime,
    MFVirtualCameraAccess,
    PCWSTR,
    PCWSTR,
    *const GUID,
    u32,
    *mut *mut c_void,
) -> HRESULT;

/// `MFCreateVirtualCamera` from mfsensorgroup.dll, or `None` before Windows 11. The module stays
/// loaded for the process lifetime because the camera object lives in it.
fn create_fn() -> Option<CreateVirtualCameraFn> {
    unsafe {
        let module = LoadLibraryExW(w!("mfsensorgroup.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?;
        let proc = GetProcAddress(module, s!("MFCreateVirtualCamera"))?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateVirtualCameraFn>(proc))
    }
}

pub struct VirtualCamera(IMFVirtualCamera);

impl VirtualCamera {
    pub fn create(name: &str) -> Result<VirtualCamera, String> {
        let Some(create) = create_fn() else {
            return Err("This Windows version has no MFCreateVirtualCamera (Windows 11 only). Use backend = auto or dshow.".into());
        };
        let camera = unsafe {
            let (name, clsid) = (HSTRING::from(name), HSTRING::from(CLSID_STR));
            let mut raw = std::ptr::null_mut();
            create(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                PCWSTR(name.as_ptr()),
                PCWSTR(clsid.as_ptr()),
                std::ptr::null(),
                0,
                &mut raw,
            )
            .ok()
            .map(|()| IMFVirtualCamera::from_raw(raw))
            .and_then(|cam| cam.Start(None).map(|()| cam))
        };
        camera.map(VirtualCamera).map_err(|e| {
            format!(
                "Virtual camera failed to start (0x{:08X}). Is DeskCam installed? Run scripts\\install.ps1 as administrator.",
                e.code().0 as u32
            )
        })
    }
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        // No Shutdown(): a second shutdown of the source prevents removal (VCamSample note).
        if let Err(e) = unsafe { self.0.Remove() } {
            log!("virtual camera remove failed: {e}");
        }
    }
}
