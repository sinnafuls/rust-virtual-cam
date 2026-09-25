//! The single output pin: connection and allocator negotiation (`IPin`), format selection
//! (`IAMStreamConfig`) and the capture pin category apps look for (`IKsPropertySet`).

use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{E_FAIL, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK, SIZE};
use windows::Win32::Media::DirectShow::{
    ALLOCATOR_PROPERTIES, AMPROPERTY_PIN_CATEGORY, E_PROP_ID_UNSUPPORTED, E_PROP_SET_UNSUPPORTED, IAMStreamConfig,
    IAMStreamConfig_Impl, IBaseFilter, IEnumMediaTypes, IFilterGraph, IMemAllocator, IMemInputPin, IPin, IPin_Impl,
    PIN_DIRECTION, PIN_INFO, PINDIR_OUTPUT, State_Stopped, VFW_E_ALREADY_CONNECTED, VFW_E_INVALIDMEDIATYPE,
    VFW_E_NO_ACCEPTABLE_TYPES, VFW_E_NOT_CONNECTED, VFW_E_NOT_STOPPED, VIDEO_STREAM_CONFIG_CAPS,
};
use windows::Win32::Media::KernelStreaming::{IKsPropertySet, IKsPropertySet_Impl};
use windows::Win32::Media::MediaFoundation::{
    AM_MEDIA_TYPE, AMPROPSETID_Pin, CLSID_MemoryAllocator, FORMAT_VideoInfo, PIN_CATEGORY_CAPTURE,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemAlloc};
use windows_core::{GUID, HRESULT, IUnknownImpl, Interface, OutRef, PWSTR, Ref, Weak, implement};

use crate::ObjGuard;
use crate::enums::EnumMediaTypes;
use crate::filter::{Connection, Core};
use crate::format::{Format, VideoType, free_format_block};

/// Pin id and name. Apps find the pin by category, not by name.
pub const PIN_ID: &str = "Capture";
/// Samples the allocator should hold at least, so a slow consumer does not stall delivery.
const MIN_BUFFERS: i32 = 3;
/// `KSPROPERTY_SUPPORT_GET`.
const SUPPORT_GET: u32 = 1;

struct FilterRef(Option<Weak<IBaseFilter>>);
// Only used to hand out a counted `IBaseFilter` from `QueryPinInfo`; guarded by the mutex.
unsafe impl Send for FilterRef {}

#[implement(IPin, IAMStreamConfig, IKsPropertySet)]
pub struct OutputPin {
    core: Arc<Core>,
    filter: Mutex<FilterRef>,
    _guard: ObjGuard,
}

impl OutputPin {
    pub(crate) fn new(core: Arc<Core>) -> OutputPin {
        OutputPin { core, filter: Mutex::new(FilterRef(None)), _guard: ObjGuard::new() }
    }

    pub(crate) fn set_filter(&self, filter: Weak<IBaseFilter>) {
        self.filter.lock().unwrap_or_else(|p| p.into_inner()).0 = Some(filter);
    }

    fn filter(&self) -> Option<IBaseFilter> {
        self.filter.lock().unwrap_or_else(|p| p.into_inner()).0.as_ref().and_then(|w| w.upgrade())
    }

    /// Types we can produce: our advertised size, any of our formats, any sane frame rate.
    fn acceptable(&self, vt: &VideoType) -> bool {
        vt.width == self.core.info.width && vt.height == self.core.info.height && (1..=240).contains(&vt.fps)
    }

    /// Parses a caller's type and fills in our frame rate when it left it unspecified.
    fn accept(&self, mt: *const AM_MEDIA_TYPE) -> Option<VideoType> {
        let mut vt = unsafe { VideoType::parse(mt) }?;
        if vt.fps == 0 {
            vt.fps = self.core.info.fps;
        }
        self.acceptable(&vt).then_some(vt)
    }
}

/// Sets up the sample allocator with the downstream input pin: its allocator if it offers one,
/// otherwise the standard memory allocator.
fn negotiate_allocator(peer: &IPin, vt: &VideoType) -> windows_core::Result<Connection> {
    let input: IMemInputPin = peer.cast()?;
    let bytes = vt.frame_bytes() as i32;
    let mut request = unsafe { input.GetAllocatorRequirements() }.unwrap_or_default();
    request.cBuffers = request.cBuffers.max(MIN_BUFFERS);
    request.cbBuffer = bytes;
    request.cbAlign = request.cbAlign.max(1);
    request.cbPrefix = request.cbPrefix.max(0);
    let setup = |allocator: IMemAllocator| -> windows_core::Result<IMemAllocator> {
        let actual: ALLOCATOR_PROPERTIES = unsafe { allocator.SetProperties(&request)? };
        if actual.cbBuffer < bytes || actual.cBuffers < 1 {
            return Err(E_FAIL.into());
        }
        unsafe { input.NotifyAllocator(&allocator, false)? };
        Ok(allocator)
    };
    let allocator = match unsafe { input.GetAllocator() }.and_then(&setup) {
        Ok(a) => a,
        Err(_) => setup(unsafe { CoCreateInstance(&CLSID_MemoryAllocator, None, CLSCTX_INPROC_SERVER)? })?,
    };
    Ok(Connection { peer: peer.clone(), input, allocator })
}

fn co_task_string(s: &str) -> windows_core::Result<PWSTR> {
    let wide: Vec<u16> = s.encode_utf16().chain(Some(0)).collect();
    let p = unsafe { CoTaskMemAlloc(wide.len() * 2) } as *mut u16;
    if p.is_null() {
        return Err(E_OUTOFMEMORY.into());
    }
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len()) };
    Ok(PWSTR(p))
}

impl IPin_Impl for OutputPin_Impl {
    fn Connect(&self, receive: Ref<IPin>, pmt: *const AM_MEDIA_TYPE) -> windows_core::Result<()> {
        let receive = receive.ok()?;
        let preferred = {
            let st = self.core.lock();
            if st.filter_state != State_Stopped {
                return Err(VFW_E_NOT_STOPPED.into());
            }
            if st.conn.is_some() {
                return Err(VFW_E_ALREADY_CONNECTED.into());
            }
            st.vtype
        };
        // A fully specified acceptable type is used as is; otherwise try ours, preferred first.
        let candidates = match self.accept(pmt) {
            Some(vt) => vec![vt],
            None => VideoType::offered(preferred).into_iter().filter(|vt| unsafe { vt.admitted_by(pmt) }).collect(),
        };
        let me: IPin = self.to_interface();
        let mut last: HRESULT = VFW_E_NO_ACCEPTABLE_TYPES;
        // No lock is held while calling the peer: it calls back into this pin.
        for vt in candidates {
            let mut mt = AM_MEDIA_TYPE::default();
            unsafe { vt.write_to(&mut mt)? };
            let result = unsafe { receive.ReceiveConnection(&me, &mt) };
            unsafe { free_format_block(&mut mt) };
            if let Err(e) = result {
                last = e.code();
                continue;
            }
            match negotiate_allocator(receive, &vt) {
                Ok(conn) => {
                    let mut st = self.core.lock();
                    st.vtype = vt;
                    st.type_changed = false;
                    st.conn = Some(conn);
                    return Ok(());
                }
                Err(e) => {
                    last = e.code();
                    let _ = unsafe { receive.Disconnect() };
                }
            }
        }
        Err(last.into())
    }

    fn ReceiveConnection(&self, _connector: Ref<IPin>, _pmt: *const AM_MEDIA_TYPE) -> windows_core::Result<()> {
        // Output pins initiate connections; they never receive them.
        Err(E_UNEXPECTED.into())
    }

    fn Disconnect(&self) -> windows_core::Result<()> {
        let mut st = self.core.lock();
        if st.filter_state != State_Stopped {
            return Err(VFW_E_NOT_STOPPED.into());
        }
        if let Some(conn) = st.conn.take() {
            let _ = unsafe { conn.allocator.Decommit() };
        }
        Ok(())
    }

    fn ConnectedTo(&self) -> windows_core::Result<IPin> {
        self.core.lock().conn.as_ref().map(|c| c.peer.clone()).ok_or_else(|| VFW_E_NOT_CONNECTED.into())
    }

    fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> windows_core::Result<()> {
        if pmt.is_null() {
            return Err(E_POINTER.into());
        }
        let st = self.core.lock();
        if st.conn.is_none() {
            unsafe { pmt.write(AM_MEDIA_TYPE::default()) };
            return Err(VFW_E_NOT_CONNECTED.into());
        }
        unsafe { st.vtype.write_to(pmt) }
    }

    fn QueryPinInfo(&self, info: *mut PIN_INFO) -> windows_core::Result<()> {
        if info.is_null() {
            return Err(E_POINTER.into());
        }
        let mut out = PIN_INFO { pFilter: std::mem::ManuallyDrop::new(self.filter()), dir: PINDIR_OUTPUT, achName: [0; 128] };
        for (d, s) in out.achName.iter_mut().zip(PIN_ID.encode_utf16()) {
            *d = s;
        }
        unsafe { info.write(out) };
        Ok(())
    }

    fn QueryDirection(&self) -> windows_core::Result<PIN_DIRECTION> {
        Ok(PINDIR_OUTPUT)
    }

    fn QueryId(&self) -> windows_core::Result<PWSTR> {
        co_task_string(PIN_ID)
    }

    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        if self.accept(pmt).is_some() { S_OK } else { S_FALSE }
    }

    fn EnumMediaTypes(&self) -> windows_core::Result<IEnumMediaTypes> {
        let preferred = self.core.lock().vtype;
        Ok(EnumMediaTypes::new(VideoType::offered(preferred)).into())
    }

    fn QueryInternalConnections(&self, _pins: OutRef<IPin>, _count: *mut u32) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EndOfStream(&self) -> windows_core::Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn BeginFlush(&self) -> windows_core::Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn EndFlush(&self) -> windows_core::Result<()> {
        Err(E_UNEXPECTED.into())
    }

    fn NewSegment(&self, _start: i64, _stop: i64, _rate: f64) -> windows_core::Result<()> {
        Ok(())
    }
}

impl IAMStreamConfig_Impl for OutputPin_Impl {
    fn SetFormat(&self, pmt: *const AM_MEDIA_TYPE) -> windows_core::Result<()> {
        let vt = if pmt.is_null() {
            VideoType::new(Format::Nv12, self.core.info)
        } else {
            self.accept(pmt).ok_or(windows_core::Error::from(VFW_E_INVALIDMEDIATYPE))?
        };
        let (peer, graph) = {
            let mut st = self.core.lock();
            if st.filter_state != State_Stopped {
                return Err(VFW_E_NOT_STOPPED.into());
            }
            match &st.conn {
                None => {
                    st.vtype = vt;
                    return Ok(());
                }
                Some(_) if st.vtype == vt => return Ok(()),
                Some(conn) => (conn.peer.clone(), st.graph),
            }
        };
        // Connected with another type: switch only if the peer takes it, then let the graph
        // reconnect us (our Connect offers the new type first).
        let mut mt = AM_MEDIA_TYPE::default();
        unsafe { vt.write_to(&mut mt)? };
        let accepted = unsafe { peer.QueryAccept(&mt) } == S_OK;
        unsafe { free_format_block(&mut mt) };
        if !accepted {
            return Err(VFW_E_INVALIDMEDIATYPE.into());
        }
        {
            let mut st = self.core.lock();
            st.vtype = vt;
            st.type_changed = true;
        }
        if let Some(graph) = unsafe { IFilterGraph::from_raw_borrowed(&graph) } {
            let me: IPin = self.to_interface();
            unsafe { graph.Reconnect(&me)? };
        }
        Ok(())
    }

    fn GetFormat(&self) -> windows_core::Result<*mut AM_MEDIA_TYPE> {
        self.core.lock().vtype.alloc()
    }

    fn GetNumberOfCapabilities(&self, count: *mut i32, size: *mut i32) -> windows_core::Result<()> {
        if count.is_null() || size.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe {
            *count = crate::format::FORMATS.len() as i32;
            *size = size_of::<VIDEO_STREAM_CONFIG_CAPS>() as i32;
        }
        Ok(())
    }

    fn GetStreamCaps(&self, index: i32, pmt: *mut *mut AM_MEDIA_TYPE, caps: *mut u8) -> windows_core::Result<()> {
        if pmt.is_null() || caps.is_null() {
            return Err(E_POINTER.into());
        }
        let default = VideoType::new(Format::Nv12, self.core.info);
        let offered = VideoType::offered(default);
        let vt = match usize::try_from(index) {
            Ok(i) if i < offered.len() => offered[i],
            Ok(_) => return Err(windows_core::Error::from_hresult(S_FALSE)),
            Err(_) => return Err(E_INVALIDARG.into()),
        };
        let size = SIZE { cx: vt.width as i32, cy: vt.height as i32 };
        let bits = vt.bit_rate() as i32;
        let info = VIDEO_STREAM_CONFIG_CAPS {
            guid: FORMAT_VideoInfo,
            VideoStandard: 0,
            InputSize: size,
            MinCroppingSize: size,
            MaxCroppingSize: size,
            CropGranularityX: 1,
            CropGranularityY: 1,
            CropAlignX: 1,
            CropAlignY: 1,
            MinOutputSize: size,
            MaxOutputSize: size,
            OutputGranularityX: 1,
            OutputGranularityY: 1,
            StretchTapsX: 0,
            StretchTapsY: 0,
            ShrinkTapsX: 0,
            ShrinkTapsY: 0,
            MinFrameInterval: vt.interval(),
            MaxFrameInterval: VideoType { fps: 1, ..vt }.interval(),
            MinBitsPerSecond: VideoType { fps: 1, ..vt }.bit_rate() as i32,
            MaxBitsPerSecond: bits,
        };
        unsafe {
            *pmt = vt.alloc()?;
            (caps as *mut VIDEO_STREAM_CONFIG_CAPS).write_unaligned(info);
        }
        Ok(())
    }
}

impl IKsPropertySet_Impl for OutputPin_Impl {
    fn Set(
        &self,
        _set: *const GUID,
        _id: u32,
        _instance: *const core::ffi::c_void,
        _instance_len: u32,
        _data: *const core::ffi::c_void,
        _data_len: u32,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Get(
        &self,
        set: *const GUID,
        id: u32,
        _instance: *const core::ffi::c_void,
        _instance_len: u32,
        data: *mut core::ffi::c_void,
        data_len: u32,
        returned: *mut u32,
    ) -> windows_core::Result<()> {
        check_category(set, id)?;
        if data.is_null() {
            return Err(E_POINTER.into());
        }
        if (data_len as usize) < size_of::<GUID>() {
            return Err(E_UNEXPECTED.into());
        }
        unsafe {
            (data as *mut GUID).write_unaligned(PIN_CATEGORY_CAPTURE);
            if !returned.is_null() {
                *returned = size_of::<GUID>() as u32;
            }
        }
        Ok(())
    }

    fn QuerySupported(&self, set: *const GUID, id: u32) -> windows_core::Result<u32> {
        check_category(set, id).map(|()| SUPPORT_GET)
    }
}

/// Only `AMPROPSETID_Pin` / `AMPROPERTY_PIN_CATEGORY` is supported.
fn check_category(set: *const GUID, id: u32) -> windows_core::Result<()> {
    match unsafe { set.as_ref() } {
        None => Err(E_POINTER.into()),
        Some(s) if *s != AMPROPSETID_Pin => Err(E_PROP_SET_UNSUPPORTED.into()),
        Some(_) if id != AMPROPERTY_PIN_CATEGORY.0 as u32 => Err(E_PROP_ID_UNSUPPORTED.into()),
        Some(_) => Ok(()),
    }
}
