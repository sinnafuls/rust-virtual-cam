//! The media source handed to the Frame Server. Mirrors smourier/VCamSample `MediaSource`.

use std::sync::{Mutex, MutexGuard};

use deskcam_proto::StreamInfo;
use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, ERROR_SET_NOT_FOUND, S_OK};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, IKsControl_Impl, KSCAMERAPROFILE_HighFrameRate, KSCAMERAPROFILE_Legacy, KSIDENTIFIER,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{BOOL, ComObject, GUID, HRESULT, IUnknown, Interface, Ref, implement, w};

use crate::attrs::{forward_attributes, new_store};
use crate::media_stream::{self, MediaStream};
use crate::{ObjGuard, trace};

struct SourceState {
    queue: Option<IMFMediaEventQueue>,
    descriptor: Option<IMFPresentationDescriptor>,
}

#[implement(IMFMediaSourceEx, IMFAttributes, IMFGetService, IKsControl, IMFSampleAllocatorControl)]
pub struct MediaSource {
    attrs: IMFAttributes,
    inner: Mutex<SourceState>,
    stream: ComObject<MediaStream>,
    _guard: ObjGuard,
}

forward_attributes!(MediaSource_Impl, attrs);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub(crate) fn ks_not_found(bytes_returned: *mut u32) -> windows_core::Result<()> {
    if !bytes_returned.is_null() {
        unsafe { *bytes_returned = 0 };
    }
    Err(HRESULT::from_win32(ERROR_SET_NOT_FOUND.0).into())
}

pub fn create(info: StreamInfo, activator_attrs: &IMFAttributes) -> windows_core::Result<IMFMediaSource> {
    let attrs = new_store()?;
    unsafe {
        activator_attrs.CopyAllItems(&attrs)?;
        let profiles = MFCreateSensorProfileCollection()?;
        let legacy = MFCreateSensorProfile(&KSCAMERAPROFILE_Legacy, 0, None)?;
        legacy.AddProfileFilter(0, w!("((RES==;FRT<=30,1;SUT==))"))?;
        profiles.AddProfile(&legacy)?;
        let high = MFCreateSensorProfile(&KSCAMERAPROFILE_HighFrameRate, 0, None)?;
        high.AddProfileFilter(0, w!("((RES==;FRT>=60,1;SUT==))"))?;
        profiles.AddProfile(&high)?;
        attrs.SetUnknown(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION, &profiles)?;
    }
    let stream = media_stream::new(info, 0)?;
    let descriptor = unsafe { MFCreatePresentationDescriptor(Some(&[Some(stream.descriptor())]))? };
    let queue = unsafe { MFCreateEventQueue()? };
    let source = ComObject::new(MediaSource {
        attrs,
        inner: Mutex::new(SourceState { queue: Some(queue), descriptor: Some(descriptor) }),
        stream: stream.clone(),
        _guard: ObjGuard::new(),
    });
    let source: IMFMediaSource = source.to_interface::<IMFMediaSourceEx>().cast()?;
    trace(&format!("media source created {}x{}@{}", info.width, info.height, info.fps));
    Ok(source)
}

impl MediaSource_Impl {
    fn queue(&self) -> windows_core::Result<IMFMediaEventQueue> {
        lock(&self.inner).queue.clone().ok_or(MF_E_SHUTDOWN.into())
    }

    fn descriptor(&self) -> windows_core::Result<IMFPresentationDescriptor> {
        lock(&self.inner).descriptor.clone().ok_or(MF_E_SHUTDOWN.into())
    }
}

fn time_variant() -> PROPVARIANT {
    PROPVARIANT::from(unsafe { MFGetSystemTime() })
}

impl IMFMediaEventGenerator_Impl for MediaSource_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows_core::Result<IMFMediaEvent> {
        let queue = self.queue()?;
        unsafe { queue.GetEvent(flags.0) }
    }

    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> windows_core::Result<()> {
        let queue = self.queue()?;
        unsafe { queue.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }

    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows_core::Result<IMFMediaEvent> {
        let queue = self.queue()?;
        unsafe { queue.EndGetEvent(result.as_ref()) }
    }

    fn QueueEvent(&self, met: u32, ext: *const GUID, hr: HRESULT, value: *const PROPVARIANT) -> windows_core::Result<()> {
        let queue = self.queue()?;
        unsafe { queue.QueueEventParamVar(met, ext, hr, value) }
    }
}

impl IMFMediaSource_Impl for MediaSource_Impl {
    fn GetCharacteristics(&self) -> windows_core::Result<u32> {
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> windows_core::Result<IMFPresentationDescriptor> {
        unsafe { self.descriptor()?.Clone() }
    }

    fn Start(
        &self,
        pd: Ref<IMFPresentationDescriptor>,
        time_format: *const GUID,
        _start: *const PROPVARIANT,
    ) -> windows_core::Result<()> {
        let pd = pd.ok()?;
        if !time_format.is_null() && unsafe { *time_format } != GUID::zeroed() {
            return Err(E_INVALIDARG.into());
        }
        let queue = self.queue()?;
        let ours = self.descriptor()?;
        let count = unsafe { pd.GetStreamDescriptorCount()? };
        if count != 1 {
            return Err(E_INVALIDARG.into());
        }
        let mut selected = BOOL::default();
        let mut desc = None;
        unsafe { pd.GetStreamDescriptorByIndex(0, &mut selected, &mut desc)? };
        let desc = desc.ok_or(windows_core::Error::from(E_POINTER))?;
        if unsafe { desc.GetStreamIdentifier()? } != 0 {
            return Err(E_INVALIDARG.into());
        }
        let running = self.stream.state() != MF_STREAM_STATE_STOPPED;
        if selected.as_bool() && !running {
            unsafe {
                ours.SelectStream(0)?;
                // Stream → source reference exists only while streaming (Shutdown clears it), so an
                // activated-but-never-started source is freed as soon as its owner releases it.
                let this = windows_core::IUnknownImpl::to_object(self);
                self.stream.set_source(this.to_interface::<IMFMediaSourceEx>().cast()?);
                let stream: IMFMediaStream2 = self.stream.to_interface();
                queue.QueueEventParamUnk(MENewStream.0 as u32, &GUID::zeroed(), S_OK, &stream)?;
                let ty = desc.GetMediaTypeHandler()?.GetCurrentMediaType()?;
                self.stream.start(Some(ty))?;
            }
        } else if !selected.as_bool() && running {
            unsafe { ours.DeselectStream(0)? };
            self.stream.stop()?;
        }
        unsafe {
            queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, &time_variant())?;
        }
        Ok(())
    }

    fn Stop(&self) -> windows_core::Result<()> {
        let queue = self.queue()?;
        let ours = self.descriptor()?;
        if self.stream.state() != MF_STREAM_STATE_STOPPED {
            self.stream.stop()?;
        }
        unsafe {
            ours.DeselectStream(0)?;
            queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), S_OK, &time_variant())?;
        }
        Ok(())
    }

    fn Pause(&self) -> windows_core::Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }

    fn Shutdown(&self) -> windows_core::Result<()> {
        let queue = {
            let mut inner = lock(&self.inner);
            inner.descriptor = None;
            inner.queue.take()
        };
        let queue = queue.ok_or(windows_core::Error::from(MF_E_SHUTDOWN))?;
        unsafe {
            let _ = queue.Shutdown();
        }
        self.stream.shutdown();
        trace("media source shut down");
        Ok(())
    }
}

impl IMFMediaSourceEx_Impl for MediaSource_Impl {
    fn GetSourceAttributes(&self) -> windows_core::Result<IMFAttributes> {
        Ok(self.attrs.clone())
    }

    fn GetStreamAttributes(&self, id: u32) -> windows_core::Result<IMFAttributes> {
        if id != 0 {
            return Err(E_INVALIDARG.into());
        }
        Ok(self.stream.attributes())
    }

    fn SetD3DManager(&self, _manager: Ref<IUnknown>) -> windows_core::Result<()> {
        // Samples stay in system memory; the frames come from shared memory anyway.
        Ok(())
    }
}

impl IMFGetService_Impl for MediaSource_Impl {
    fn GetService(&self, _service: *const GUID, _riid: *const GUID, _ppv: *mut *mut core::ffi::c_void) -> windows_core::Result<()> {
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IMFSampleAllocatorControl_Impl for MediaSource_Impl {
    fn SetDefaultAllocator(&self, stream_id: u32, allocator: Ref<IUnknown>) -> windows_core::Result<()> {
        if stream_id != 0 {
            return Err(E_INVALIDARG.into());
        }
        let allocator = allocator.ok()?.cast::<IMFVideoSampleAllocatorEx>()?;
        self.stream.set_allocator(allocator);
        Ok(())
    }

    fn GetAllocatorUsage(
        &self,
        stream_id: u32,
        input_id: *mut u32,
        usage: *mut MFSampleAllocatorUsage,
    ) -> windows_core::Result<()> {
        if input_id.is_null() || usage.is_null() {
            return Err(E_POINTER.into());
        }
        if stream_id != 0 {
            return Err(E_INVALIDARG.into());
        }
        unsafe {
            *input_id = stream_id;
            *usage = MFSampleAllocatorUsage_UsesProvidedAllocator;
        }
        Ok(())
    }
}

impl IKsControl_Impl for MediaSource_Impl {
    fn KsProperty(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut core::ffi::c_void, _dl: u32, ret: *mut u32) -> windows_core::Result<()> {
        ks_not_found(ret)
    }

    fn KsMethod(&self, _m: *const KSIDENTIFIER, _ml: u32, _d: *mut core::ffi::c_void, _dl: u32, ret: *mut u32) -> windows_core::Result<()> {
        ks_not_found(ret)
    }

    fn KsEvent(&self, _e: *const KSIDENTIFIER, _el: u32, _d: *mut core::ffi::c_void, _dl: u32, ret: *mut u32) -> windows_core::Result<()> {
        ks_not_found(ret)
    }
}
