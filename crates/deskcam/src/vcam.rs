//! Registers the session-lifetime virtual camera backed by `deskcam_source.dll`.

use deskcam_proto::CLSID_STR;
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MFCreateVirtualCamera, MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime_Session,
    MFVirtualCameraType_SoftwareCameraSource,
};
use windows_core::HSTRING;

use crate::log::log;

pub struct VirtualCamera(IMFVirtualCamera);

impl VirtualCamera {
    pub fn create(name: &str) -> Result<VirtualCamera, String> {
        let camera = unsafe {
            MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_CurrentUser,
                &HSTRING::from(name),
                &HSTRING::from(CLSID_STR),
                None,
            )
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
