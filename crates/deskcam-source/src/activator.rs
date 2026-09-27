//! `IMFActivate` returned by the class factory. The media source (and its stream) is built on
//! the first `ActivateObject`, not up front: the Frame Server and its monitor create activators
//! just to read attributes, and those must cost nothing.

use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};

use deskcam_proto::{CLSID, StreamInfo};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFActivate_Impl, IMFAttributes, IMFMediaSource, MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES,
    MFT_TRANSFORM_CLSID_Attribute,
};
use windows_core::{GUID, Interface, implement};

use crate::attrs::{forward_attributes, new_store};
use crate::{ObjGuard, media_source};

#[implement(IMFActivate)]
pub struct Activator {
    attrs: IMFAttributes,
    info: StreamInfo,
    source: Mutex<Option<IMFMediaSource>>,
    _guard: ObjGuard,
}

forward_attributes!(Activator_Impl, attrs);

pub fn create(info: StreamInfo) -> windows_core::Result<IMFActivate> {
    let attrs = new_store()?;
    unsafe {
        attrs.SetUINT32(&MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES, 1)?;
        attrs.SetGUID(&MFT_TRANSFORM_CLSID_Attribute, &CLSID)?;
    }
    Ok(Activator { attrs, info, source: Mutex::new(None), _guard: ObjGuard::new() }.into())
}

impl Activator {
    fn slot(&self) -> MutexGuard<'_, Option<IMFMediaSource>> {
        self.source.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> windows_core::Result<()> {
        let source = {
            let mut slot = self.slot();
            match &*slot {
                Some(source) => source.clone(),
                None => slot.insert(media_source::create(self.info, &self.attrs)?).clone(),
            }
        };
        unsafe { source.query(riid, ppv).ok() }
    }

    /// Shuts the created source down; a later `ActivateObject` creates a fresh one.
    fn ShutdownObject(&self) -> windows_core::Result<()> {
        if let Some(source) = self.slot().take() {
            // Already shut down by the pipeline is fine.
            let _ = unsafe { source.Shutdown() };
        }
        Ok(())
    }

    /// Releases the created source without shutting it down (the caller owns it now).
    fn DetachObject(&self) -> windows_core::Result<()> {
        self.slot().take();
        Ok(())
    }
}
