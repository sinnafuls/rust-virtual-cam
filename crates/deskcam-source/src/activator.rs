//! `IMFActivate` returned by the class factory; hands out the single media source.

use std::ffi::c_void;
use std::sync::Mutex;

use deskcam_proto::{CLSID, StreamInfo};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFActivate_Impl, IMFAttributes, IMFMediaSource, MF_E_SHUTDOWN,
    MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES, MFT_TRANSFORM_CLSID_Attribute,
};
use windows_core::{GUID, Interface, implement};

use crate::attrs::{forward_attributes, new_store};
use crate::{ObjGuard, media_source};

#[implement(IMFActivate)]
pub struct Activator {
    attrs: IMFAttributes,
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
    let source = media_source::create(info, &attrs)?;
    Ok(Activator { attrs, source: Mutex::new(Some(source)), _guard: ObjGuard::new() }.into())
}

impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> windows_core::Result<()> {
        let source = self.source.lock().unwrap_or_else(|p| p.into_inner()).clone();
        match source {
            Some(source) => unsafe { source.query(riid, ppv).ok() },
            None => Err(MF_E_SHUTDOWN.into()),
        }
    }

    fn ShutdownObject(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn DetachObject(&self) -> windows_core::Result<()> {
        *self.source.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(())
    }
}
