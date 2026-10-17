//! DirectShow, the way Windows 10 desktop apps find and open a camera: the system device
//! enumerator lists `CLSID_VideoInputDeviceCategory`, the app binds the moniker, builds a capture
//! graph and receives samples.
//!
//! DeskCam is identified by the filter CLSID stored with the category instance (`CLSID`
//! property / moniker display name), never by its friendly name, which users can change.
//! Activation is tested twice: through the moniker (what apps do) and with a plain
//! `CoCreateInstance` of the CLSID, so "not in the camera category" and "cannot load the DLL"
//! are told apart. This opens DeskCam for a few seconds, which makes deskcam.exe capture the
//! desktop (the Windows 10 yellow capture border may flash). Samples are reduced to counts and
//! luma statistics in memory; no pixels are kept or written.

#![allow(non_snake_case)] // COM method names

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use deskcam_proto::layout::Namespace;
use deskcam_proto::{DSHOW_CLSID, DSHOW_CLSID_STR, com, os};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HMODULE, S_OK, WIN32_ERROR};
use windows::Win32::Media::DirectShow::{
    IAMStreamConfig, IBaseFilter, ICaptureGraphBuilder2, ICreateDevEnum, IGraphBuilder,
    IMediaControl, IMediaEvent, PIN_INFO, PINDIR_OUTPUT,
};
use windows::Win32::Media::MediaFoundation::{
    AM_MEDIA_TYPE, CLSID_CaptureGraphBuilder2, CLSID_FilterGraph, CLSID_SystemDeviceEnum,
    CLSID_VideoInputDeviceCategory, FORMAT_VideoInfo, FORMAT_VideoInfo2, MEDIASUBTYPE_I420,
    MEDIASUBTYPE_IYUV, MEDIASUBTYPE_NV12, MEDIASUBTYPE_RGB24, MEDIASUBTYPE_RGB32,
    MEDIASUBTYPE_YUY2, MEDIATYPE_Video, PIN_CATEGORY_CAPTURE, VIDEOINFOHEADER, VIDEOINFOHEADER2,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CreateBindCtx, IEnumMoniker, IMoniker, IPersist,
};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleHandleExW,
};
use windows::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::Variant::VARIANT;
use windows_core::{
    BOOL, BSTR, ComObject, GUID, HRESULT, HSTRING, IUnknown, IUnknown_Vtbl, Interface, PCWSTR,
    implement, interface, w,
};

use super::{
    Luma, Report, RingWatch, describe_ring, fourcc_or_guid, hint_suffix, hr, load_stream_info,
    open_section, sample_ring, sparse_hash, step, wide_to_string, win32,
};

/// qedit.dll; still shipped with Windows 10/11 though its header left the SDK.
const CLSID_SAMPLE_GRABBER: GUID = GUID::from_u128(0xc1f400a0_3f08_11d3_9f0b_006008039e37);
const CLSID_NULL_RENDERER: GUID = GUID::from_u128(0xc1f400a4_3f08_11d3_9f0b_006008039e37);
/// `REGDB_E_CLASSNOTREG`.
const CLASS_NOT_REGISTERED: u32 = 0x8004_0154;
/// How long samples are observed once the graph runs.
const OBSERVE: Duration = Duration::from_secs(6);

#[interface("6b652fff-11fe-4fce-92ad-0266b5d7c78f")]
unsafe trait ISampleGrabber: IUnknown {
    fn SetOneShot(&self, one_shot: BOOL) -> HRESULT;
    fn SetMediaType(&self, mt: *const AM_MEDIA_TYPE) -> HRESULT;
    fn GetConnectedMediaType(&self, mt: *mut AM_MEDIA_TYPE) -> HRESULT;
    fn SetBufferSamples(&self, buffer: BOOL) -> HRESULT;
    fn GetCurrentBuffer(&self, size: *mut i32, buffer: *mut i32) -> HRESULT;
    fn GetCurrentSample(&self, sample: *mut *mut c_void) -> HRESULT;
    fn SetCallback(&self, callback: *mut c_void, which: i32) -> HRESULT;
}

#[interface("0579154a-2b53-4994-b0d0-e773148eff85")]
unsafe trait ISampleGrabberCB: IUnknown {
    fn SampleCB(&self, time: f64, sample: *mut c_void) -> HRESULT;
    fn BufferCB(&self, time: f64, buffer: *mut u8, len: i32) -> HRESULT;
}

/// Where the luma bytes of a negotiated sample are.
#[derive(Clone, Copy)]
enum Layout {
    /// NV12 / I420 / IYUV: a `w*h` Y plane first.
    Planar420 {
        y_len: usize,
    },
    /// YUY2: Y in every even byte.
    Yuy2 {
        len: usize,
    },
    Unknown,
}

/// Sample statistics only; the callback never keeps pixel data.
struct Stats {
    layout: Layout,
    count: u32,
    first: Option<Instant>,
    last: Option<Instant>,
    last_len: usize,
    black: u32,
    non_black: u32,
    distinct: u32,
    last_hash: Option<u64>,
    last_luma: Option<Luma>,
}

#[implement(ISampleGrabberCB)]
struct Sink(Mutex<Stats>);

impl Sink {
    fn stats(&self) -> MutexGuard<'_, Stats> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ISampleGrabberCB_Impl for Sink_Impl {
    unsafe fn SampleCB(&self, _time: f64, _sample: *mut c_void) -> HRESULT {
        HRESULT(0)
    }

    unsafe fn BufferCB(&self, _time: f64, buffer: *mut u8, len: i32) -> HRESULT {
        let mut s = self.stats();
        let now = Instant::now();
        s.first.get_or_insert(now);
        s.last = Some(now);
        s.count += 1;
        s.last_len = len.max(0) as usize;
        if buffer.is_null() || len <= 0 {
            return HRESULT(0);
        }
        let bytes = unsafe { std::slice::from_raw_parts(buffer, len as usize) };
        let hash = sparse_hash(bytes);
        if s.last_hash != Some(hash) {
            s.distinct += 1;
            s.last_hash = Some(hash);
        }
        let luma = match s.layout {
            Layout::Planar420 { y_len } => Luma::of(&bytes[..y_len.min(bytes.len())], 7),
            Layout::Yuy2 { len } => Luma::of(&bytes[..len.min(bytes.len())], 14),
            Layout::Unknown => None,
        };
        if let Some(l) = luma {
            if l.is_fallback_black() {
                s.black += 1;
            } else {
                s.non_black += 1;
            }
        }
        s.last_luma = luma;
        HRESULT(0)
    }
}

pub(super) fn run(r: &mut Report) {
    r.stage("CoInitializeEx(COINIT_MULTITHREADED)");
    if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        r.fail(format!("CoInitializeEx: {}", hr(&e)));
        return;
    }
    let win11 = os::has_mf_virtual_camera();
    registry(r);
    let found = enumerate(r);
    let direct = activate_direct(r);

    let deskcam: Vec<&Device> = found.iter().flatten().filter(|d| d.is_deskcam()).collect();
    if deskcam.len() > 1 {
        r.warn(format!(
            "{} category entries carry the DeskCam CLSID; apps may list duplicates",
            deskcam.len()
        ));
    }
    let mut bound = None;
    if let Some(dev) = deskcam.first() {
        r.pass(format!(
            "DeskCam is listed by the system device enumerator as '{}' (identified by CLSID {DSHOW_CLSID_STR})",
            dev.name
        ));
        r.stage(&format!(
            "IMoniker::BindToObject(IBaseFilter) for '{}' (what apps do)",
            dev.name
        ));
        let res: windows_core::Result<IBaseFilter> =
            unsafe { dev.moniker.BindToObject(None, None) };
        match res {
            Ok(f) => {
                r.pass(format!(
                    "Moniker bound to a filter object{}",
                    loaded_from(f.as_raw())
                ));
                bound = Some(f);
            }
            Err(e) => r.fail(format!(
                "Moniker for '{}' is listed but cannot be activated: {}{}",
                dev.name,
                hr(&e),
                hint_suffix(&e)
            )),
        }
    } else if found.is_some() {
        match &direct {
            Ok(_) if win11 => r.warn(
                "DeskCam's DirectShow class loads but is not in the video capture category; on Windows 11 the MF backend is normally used instead",
            ),
            Ok(_) => r.fail(
                "DeskCam's DirectShow class loads, but it is not registered in the video capture category: apps cannot list it (re-run install.ps1)",
            ),
            Err(code) if *code == CLASS_NOT_REGISTERED && win11 => {
                r.warn("DeskCam is not registered for DirectShow; expected on Windows 11 with the MF backend (default install)");
            }
            Err(_) => r.fail(format!(
                "DeskCam is not listed by the system device enumerator in this {}-bit process (no entry with CLSID {DSHOW_CLSID_STR})",
                usize::BITS
            )),
        }
    }

    // Prefer the instance apps would get; fall back to the directly created one so streaming is
    // still tested when only the category entry is missing.
    let filter = bound.or_else(|| direct.ok());
    let Some(filter) = filter else {
        r.skip("Pin, media type and sample delivery checks: no DeskCam filter instance could be created");
        return;
    };
    report_pins(r, &filter);
    stream(r, &filter);
}

/// HKCR as this process sees it (WOW6432Node for 32-bit). Read-only; facts only, the
/// functional checks below decide pass or fail.
fn registry(r: &mut Report) {
    r.stage("registry: DirectShow class and category entries (read-only, this process' view)");
    let key = format!("CLSID\\{DSHOW_CLSID_STR}\\InprocServer32");
    match reg_sz(&key, PCWSTR::null()) {
        Ok(path) => {
            let exists = std::path::Path::new(&path).is_file();
            let model =
                reg_sz(&key, w!("ThreadingModel")).unwrap_or_else(|e| format!("<{}>", win32(e)));
            let msg =
                format!("HKCR\\{key} = {path} (ThreadingModel {model}); file present: {exists}");
            if exists { r.info(msg) } else { r.warn(msg) }
        }
        Err(e) if e == ERROR_FILE_NOT_FOUND => r.info(format!(
            "HKCR\\{key} not present in the {}-bit registry view",
            usize::BITS
        )),
        Err(e) => r.warn(format!("HKCR\\{key}: {}", win32(e))),
    }
    match com::registered_dshow_name() {
        Some(name) => r.info(format!(
            "Video capture category instance {DSHOW_CLSID_STR}: FriendlyName '{name}'"
        )),
        None => r.info(format!(
            "Video capture category instance {DSHOW_CLSID_STR}: not present or unreadable"
        )),
    }
}

fn reg_sz(subkey: &str, value: PCWSTR) -> Result<String, WIN32_ERROR> {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            &HSTRING::from(subkey),
            value,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return Err(status);
    }
    Ok(wide_to_string(&buf[..(size as usize / 2).min(buf.len())]))
}

struct Device {
    name: String,
    clsid: Option<String>,
    display: Option<String>,
    moniker: IMoniker,
}

impl Device {
    fn is_deskcam(&self) -> bool {
        self.clsid
            .as_deref()
            .is_some_and(|c| c.trim().eq_ignore_ascii_case(DSHOW_CLSID_STR))
            || self
                .display
                .as_deref()
                .is_some_and(|d| d.to_ascii_uppercase().contains(DSHOW_CLSID_STR))
    }
}

fn read_prop(bag: &IPropertyBag, name: PCWSTR) -> Option<String> {
    let mut v = VARIANT::default();
    unsafe { bag.Read(name, &mut v, None) }.ok()?;
    BSTR::try_from(&v).ok().map(|b| b.to_string())
}

fn display_name(moniker: &IMoniker) -> Option<String> {
    let ctx = unsafe { CreateBindCtx(0) }.ok()?;
    let p = unsafe { moniker.GetDisplayName(&ctx, None) }.ok()?;
    let s = unsafe { p.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s
}

/// Every video capture device as apps see it. `None` when enumeration itself failed.
fn enumerate(r: &mut Report) -> Option<Vec<Device>> {
    let dev_enum: ICreateDevEnum =
        step(r, "CoCreateInstance(CLSID_SystemDeviceEnum)", || unsafe {
            CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)
        })?;
    let mut monikers: Option<IEnumMoniker> = None;
    step(
        r,
        "ICreateDevEnum::CreateClassEnumerator(CLSID_VideoInputDeviceCategory)",
        || unsafe {
            dev_enum.CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut monikers, 0)
        },
    )?;
    let mut out = Vec::new();
    let Some(monikers) = monikers else {
        r.info("Video capture category is empty (S_FALSE): no DirectShow cameras at all");
        return Some(out);
    };
    loop {
        r.stage("IEnumMoniker::Next (video capture category)");
        let mut next = [None];
        let code = unsafe { monikers.Next(&mut next, None) };
        if code != S_OK {
            if code.is_err() {
                r.warn(format!(
                    "IEnumMoniker::Next stopped early: {}",
                    hr(&code.into())
                ));
            }
            break;
        }
        let Some(moniker) = next[0].take() else { break };
        let display = display_name(&moniker);
        r.stage(&format!(
            "IMoniker::BindToStorage(IPropertyBag) for {}",
            display.as_deref().unwrap_or("<no display name>")
        ));
        let bag: windows_core::Result<IPropertyBag> = unsafe { moniker.BindToStorage(None, None) };
        let (name, clsid, path) = match &bag {
            Ok(bag) => (
                read_prop(bag, w!("FriendlyName")).unwrap_or_else(|| "<no FriendlyName>".into()),
                read_prop(bag, w!("CLSID")),
                read_prop(bag, w!("DevicePath")),
            ),
            Err(e) => {
                r.warn(format!(
                    "Cannot read properties of {}: {}",
                    display.as_deref().unwrap_or("<moniker>"),
                    hr(e)
                ));
                ("<unreadable>".into(), None, None)
            }
        };
        let dev = Device {
            name,
            clsid,
            display,
            moniker,
        };
        r.info(format!(
            "Camera {}: '{}'{} CLSID {} | {}{}",
            out.len() + 1,
            dev.name,
            if dev.is_deskcam() { " [DeskCam]" } else { "" },
            dev.clsid.as_deref().unwrap_or("<none>"),
            dev.display.as_deref().unwrap_or("<no display name>"),
            path.map(|p| format!(" | DevicePath {p}"))
                .unwrap_or_default()
        ));
        out.push(dev);
    }
    r.info(format!(
        "{} DirectShow video capture device(s) in this {}-bit process",
        out.len(),
        usize::BITS
    ));
    Some(out)
}

/// `CoCreateInstance(DSHOW_CLSID)`; `Err` carries the HRESULT for the discovery verdict.
fn activate_direct(r: &mut Report) -> Result<IBaseFilter, u32> {
    r.stage("CoCreateInstance(DSHOW_CLSID, CLSCTX_INPROC_SERVER) without the category (loads deskcam_dshow.dll)");
    match unsafe { CoCreateInstance::<_, IBaseFilter>(&DSHOW_CLSID, None, CLSCTX_INPROC_SERVER) } {
        Ok(f) => {
            r.pass(format!(
                "Direct activation of {DSHOW_CLSID_STR} succeeded{}",
                loaded_from(f.as_raw())
            ));
            match f.cast::<IPersist>().and_then(|p| unsafe { p.GetClassID() }) {
                Ok(id) if id == DSHOW_CLSID => {}
                Ok(id) => r.warn(format!(
                    "Created object reports class {id:?}, not {DSHOW_CLSID_STR}"
                )),
                Err(e) => r.warn(format!("IPersist::GetClassID: {}", hr(&e))),
            }
            Ok(f)
        }
        Err(e) => {
            let code = e.code().0 as u32;
            if code == CLASS_NOT_REGISTERED && os::has_mf_virtual_camera() {
                r.info(format!(
                    "Direct activation of {DSHOW_CLSID_STR}: {} (not registered)",
                    hr(&e)
                ));
            } else {
                r.fail(format!(
                    "Direct activation of {DSHOW_CLSID_STR}: {}{}",
                    hr(&e),
                    hint_suffix(&e)
                ));
            }
            Err(code)
        }
    }
}

/// ` from <dll path>` of the module implementing a COM object, read from its vtable address.
fn loaded_from(obj: *mut c_void) -> String {
    if obj.is_null() {
        return String::new();
    }
    let vtable = unsafe { *(obj as *const *const c_void) };
    let mut module = HMODULE::default();
    let flags =
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    match unsafe { GetModuleHandleExW(flags, PCWSTR(vtable as *const u16), &mut module) } {
        Ok(()) => format!(" (implemented by {})", com::module_path(module)),
        Err(_) => String::new(),
    }
}

/// Frees a media type's format block and `pUnk` (strmbase `FreeMediaType`).
///
/// # Safety
/// `mt` must hold a `CoTaskMemAlloc`ed format block or null.
unsafe fn free_media_type(mt: &mut AM_MEDIA_TYPE) {
    if !mt.pbFormat.is_null() {
        unsafe { CoTaskMemFree(Some(mt.pbFormat as *const c_void)) };
        mt.pbFormat = std::ptr::null_mut();
    }
    mt.cbFormat = 0;
    unsafe { ManuallyDrop::drop(&mut mt.pUnk) };
    mt.pUnk = ManuallyDrop::new(None);
}

/// Frees a `CoTaskMemAlloc`ed media type (strmbase `DeleteMediaType`).
///
/// # Safety
/// `mt` must be null or a `CoTaskMemAlloc`ed media type that owns its format block.
unsafe fn delete_media_type(mt: *mut AM_MEDIA_TYPE) {
    if let Some(m) = unsafe { mt.as_mut() } {
        unsafe {
            free_media_type(m);
            CoTaskMemFree(Some(mt as *const c_void));
        }
    }
}

fn subtype_name(g: &GUID) -> String {
    match *g {
        MEDIASUBTYPE_RGB32 => "RGB32".into(),
        MEDIASUBTYPE_RGB24 => "RGB24".into(),
        _ => fourcc_or_guid(g),
    }
}

/// Width, height and fps from a `VIDEOINFOHEADER`/`VIDEOINFOHEADER2` format block.
fn video_dims(m: &AM_MEDIA_TYPE) -> Option<(i32, i32, f64)> {
    if m.pbFormat.is_null() {
        return None;
    }
    let (bmi, avg) = if m.formattype == FORMAT_VideoInfo
        && m.cbFormat as usize >= size_of::<VIDEOINFOHEADER>()
    {
        let v = unsafe { (m.pbFormat as *const VIDEOINFOHEADER).read_unaligned() };
        (v.bmiHeader, v.AvgTimePerFrame)
    } else if m.formattype == FORMAT_VideoInfo2
        && m.cbFormat as usize >= size_of::<VIDEOINFOHEADER2>()
    {
        let v = unsafe { (m.pbFormat as *const VIDEOINFOHEADER2).read_unaligned() };
        (v.bmiHeader, v.AvgTimePerFrame)
    } else {
        return None;
    };
    let fps = if avg > 0 {
        10_000_000.0 / avg as f64
    } else {
        0.0
    };
    Some((bmi.biWidth, bmi.biHeight, fps))
}

fn describe_media_type(m: &AM_MEDIA_TYPE) -> String {
    let major = if m.majortype == MEDIATYPE_Video {
        "Video".to_owned()
    } else {
        format!("{:?}", m.majortype)
    };
    let format = match video_dims(m) {
        Some((w, h, fps)) => format!("{w}x{h} @ {fps:.2} fps"),
        None => format!("format {:?} ({} bytes)", m.formattype, m.cbFormat),
    };
    format!(
        "{major} {} {format}, sample size {}",
        subtype_name(&m.subtype),
        m.lSampleSize
    )
}

fn layout_of(m: &AM_MEDIA_TYPE) -> Layout {
    let Some((w, h, _)) = video_dims(m) else {
        return Layout::Unknown;
    };
    let pixels = w.unsigned_abs() as usize * h.unsigned_abs() as usize;
    match m.subtype {
        MEDIASUBTYPE_NV12 | MEDIASUBTYPE_I420 | MEDIASUBTYPE_IYUV => {
            Layout::Planar420 { y_len: pixels }
        }
        MEDIASUBTYPE_YUY2 => Layout::Yuy2 { len: pixels * 2 },
        _ => Layout::Unknown,
    }
}

/// Output pins and the media types they offer, as a format-picking app sees them.
fn report_pins(r: &mut Report, filter: &IBaseFilter) {
    let Some(pins) = step(r, "IBaseFilter::EnumPins", || unsafe { filter.EnumPins() }) else {
        return;
    };
    let mut outputs = 0;
    loop {
        r.stage("IEnumPins::Next");
        let mut next = [None];
        if unsafe { pins.Next(&mut next, None) } != S_OK {
            break;
        }
        let Some(pin) = next[0].take() else { break };
        let mut info = PIN_INFO::default();
        let name = match unsafe { pin.QueryPinInfo(&mut info) } {
            Ok(()) => {
                unsafe { ManuallyDrop::drop(&mut info.pFilter) };
                wide_to_string(&info.achName)
            }
            Err(e) => format!("<QueryPinInfo {}>", hr(&e)),
        };
        match unsafe { pin.QueryDirection() } {
            Ok(dir) if dir == PINDIR_OUTPUT => {}
            Ok(_) => {
                r.info(format!(
                    "Input pin '{name}' (unexpected on a capture source)"
                ));
                continue;
            }
            Err(e) => {
                r.warn(format!("Pin '{name}': QueryDirection {}", hr(&e)));
                continue;
            }
        }
        outputs += 1;
        let Some(types) = step(
            r,
            &format!("IPin::EnumMediaTypes on output pin '{name}'"),
            || unsafe { pin.EnumMediaTypes() },
        ) else {
            continue;
        };
        let mut count = 0;
        while count < 64 {
            let mut mt = [std::ptr::null_mut()];
            let mut fetched = 0u32;
            if unsafe { types.Next(&mut mt, Some(&mut fetched)) } != S_OK
                || fetched == 0
                || mt[0].is_null()
            {
                break;
            }
            count += 1;
            r.info(format!(
                "Pin '{name}' type {count}: {}",
                describe_media_type(unsafe { &*mt[0] })
            ));
            unsafe { delete_media_type(mt[0]) };
        }
        if count == 0 {
            r.fail(format!("Output pin '{name}' offers no media types"));
        } else {
            r.pass(format!("Output pin '{name}' offers {count} media type(s)"));
        }
        match pin.cast::<IAMStreamConfig>() {
            Ok(config) => {
                let (mut caps, mut size) = (0i32, 0i32);
                match unsafe { config.GetNumberOfCapabilities(&mut caps, &mut size) } {
                    Ok(()) => r.info(format!(
                        "IAMStreamConfig: {caps} capabilities (caps struct {size} bytes)"
                    )),
                    Err(e) => r.warn(format!(
                        "IAMStreamConfig::GetNumberOfCapabilities: {}",
                        hr(&e)
                    )),
                }
                match unsafe { config.GetFormat() } {
                    Ok(mt) => {
                        if let Some(m) = unsafe { mt.as_ref() } {
                            r.info(format!(
                                "IAMStreamConfig current format: {}",
                                describe_media_type(m)
                            ));
                        }
                        unsafe { delete_media_type(mt) };
                    }
                    Err(e) => r.warn(format!("IAMStreamConfig::GetFormat: {}", hr(&e))),
                }
            }
            Err(e) => r.warn(format!(
                "Output pin '{name}' has no IAMStreamConfig ({}); apps cannot choose a format",
                hr(&e)
            )),
        }
    }
    if outputs == 0 {
        r.fail("DeskCam filter exposes no output pin");
    }
}

/// Source → Sample Grabber → Null Renderer through the capture graph builder, like apps do.
fn stream(r: &mut Report, source: &IBaseFilter) {
    let info = load_stream_info(r);
    let section = open_section(r, Namespace::Local, info);
    if let Some(s) = &section {
        let snap = sample_ring(&s.ring);
        r.info(format!("Before streaming: {}", describe_ring(&snap)));
        if !snap.writer_alive {
            r.warn("deskcam.exe is not updating the frame section (not running, hung, or using another backend)");
        }
    }

    let Some(graph) = step(r, "CoCreateInstance(CLSID_FilterGraph)", || unsafe {
        CoCreateInstance::<_, IGraphBuilder>(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER)
    }) else {
        return;
    };
    let Some(builder) = step(
        r,
        "CoCreateInstance(CLSID_CaptureGraphBuilder2)",
        || unsafe {
            CoCreateInstance::<_, ICaptureGraphBuilder2>(
                &CLSID_CaptureGraphBuilder2,
                None,
                CLSCTX_INPROC_SERVER,
            )
        },
    ) else {
        return;
    };
    let Some(grabber_filter) = step(
        r,
        "CoCreateInstance(Sample Grabber, qedit.dll)",
        || unsafe {
            CoCreateInstance::<_, IBaseFilter>(&CLSID_SAMPLE_GRABBER, None, CLSCTX_INPROC_SERVER)
        },
    ) else {
        r.info("Without qedit.dll's Sample Grabber the streaming test cannot run; DeskCam itself does not need it");
        return;
    };
    let Some(renderer) = step(r, "CoCreateInstance(Null Renderer, qedit.dll)", || unsafe {
        CoCreateInstance::<_, IBaseFilter>(&CLSID_NULL_RENDERER, None, CLSCTX_INPROC_SERVER)
    }) else {
        return;
    };
    let Some(grabber) = step(r, "QueryInterface(ISampleGrabber)", || {
        grabber_filter.cast::<ISampleGrabber>()
    }) else {
        return;
    };
    let sink = ComObject::new(Sink(Mutex::new(Stats {
        layout: Layout::Unknown,
        count: 0,
        first: None,
        last: None,
        last_len: 0,
        black: 0,
        non_black: 0,
        distinct: 0,
        last_hash: None,
        last_luma: None,
    })));
    let sink_cb: ISampleGrabberCB = sink.to_interface();

    let built = step(
        r,
        "graph: SetFiltergraph + AddFilter(DeskCam, Sample Grabber, Null Renderer)",
        || unsafe {
            builder.SetFiltergraph(&graph)?;
            graph.AddFilter(source, w!("DeskCam"))?;
            graph.AddFilter(&grabber_filter, w!("Sample Grabber"))?;
            graph.AddFilter(&renderer, w!("Null Renderer"))
        },
    );
    if built.is_none() {
        return;
    }
    // Any video subtype: the graph negotiates what DeskCam offers first, as a default app would.
    let want = AM_MEDIA_TYPE {
        majortype: MEDIATYPE_Video,
        ..Default::default()
    };
    let configured = step(
        r,
        "ISampleGrabber: SetMediaType(Video) + SetBufferSamples(FALSE) + SetCallback(BufferCB)",
        || unsafe {
            grabber.SetMediaType(&want).ok()?;
            grabber.SetBufferSamples(false.into()).ok()?;
            grabber.SetCallback(sink_cb.as_raw(), 1).ok()
        },
    );
    if configured.is_none() {
        return;
    }
    let rendered = step(
        r,
        "ICaptureGraphBuilder2::RenderStream(PIN_CATEGORY_CAPTURE, Video) (pin connection / negotiation)",
        || unsafe {
            builder.RenderStream(
                Some(&PIN_CATEGORY_CAPTURE),
                &MEDIATYPE_Video,
                source,
                &grabber_filter,
                &renderer,
            )
        },
    );
    if rendered.is_none() {
        let _ = unsafe { grabber.SetCallback(std::ptr::null_mut(), 1) };
        return;
    }
    r.pass("Capture pin connected to Sample Grabber and Null Renderer");
    let mut connected = AM_MEDIA_TYPE::default();
    r.stage("ISampleGrabber::GetConnectedMediaType");
    match unsafe { grabber.GetConnectedMediaType(&mut connected) }.ok() {
        Ok(()) => {
            r.info(format!("Negotiated: {}", describe_media_type(&connected)));
            sink.stats().layout = layout_of(&connected);
            unsafe { free_media_type(&mut connected) };
        }
        Err(e) => r.warn(format!("ISampleGrabber::GetConnectedMediaType: {}", hr(&e))),
    }

    let Some(control) = step(r, "QueryInterface(IMediaControl)", || {
        graph.cast::<IMediaControl>()
    }) else {
        let _ = unsafe { grabber.SetCallback(std::ptr::null_mut(), 1) };
        return;
    };
    let events = graph.cast::<IMediaEvent>().ok();
    if step(r, "IMediaControl::Run", || unsafe { control.Run() }).is_none() {
        drain_events(r, events.as_ref());
        let _ = unsafe { grabber.SetCallback(std::ptr::null_mut(), 1) };
        return;
    }
    r.stage("IMediaControl::GetState(3000 ms)");
    match unsafe { control.GetState(3000) } {
        Ok(2) => r.info("Graph state: running"),
        Ok(state) => r.warn(format!("Graph state {state} (2 = running) after 3 s")),
        Err(e) => r.warn(format!("IMediaControl::GetState: {}", hr(&e))),
    }

    r.stage(&format!(
        "observe samples for {} s (nothing is saved)",
        OBSERVE.as_secs()
    ));
    let start = Instant::now();
    let mut watch = RingWatch::default();
    let mut next_report = Duration::from_secs(1);
    while start.elapsed() < OBSERVE {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(s) = &section {
            watch.add(&sample_ring(&s.ring));
        }
        if start.elapsed() >= next_report {
            next_report += Duration::from_secs(1);
            let count = sink.stats().count;
            let ring = if section.is_some() {
                format!("; section: {}", watch.summary())
            } else {
                String::new()
            };
            r.info(format!(
                "t={:.1}s: {count} samples{ring}",
                start.elapsed().as_secs_f64()
            ));
        }
        drain_events(r, events.as_ref());
    }
    r.stage("IMediaControl::Stop");
    if let Err(e) = unsafe { control.Stop() } {
        r.warn(format!("IMediaControl::Stop: {}", hr(&e)));
    }
    let _ = unsafe { grabber.SetCallback(std::ptr::null_mut(), 1) };
    drain_events(r, events.as_ref());
    verdict(r, &sink.stats(), section.is_some(), &watch);
}

/// Logs pending graph events; errors (EC_ERRORABORT and friends) fail with their HRESULT.
fn drain_events(r: &mut Report, events: Option<&IMediaEvent>) {
    let Some(events) = events else { return };
    loop {
        let (mut code, mut p1, mut p2) = (0i32, 0isize, 0isize);
        if unsafe { events.GetEvent(&mut code, &mut p1, &mut p2, 0) }.is_err() {
            break;
        }
        let name = match code {
            0x01 => Some("EC_COMPLETE"),
            0x03 => Some("EC_ERRORABORT"),
            0x06 => Some("EC_STREAM_ERROR_STOPPED"),
            0x07 => Some("EC_STREAM_ERROR_STILLPLAYING"),
            0x1F => Some("EC_DEVICE_LOST"),
            0x45 => Some("EC_ERRORABORTEX"),
            _ => None,
        };
        match (code, name) {
            (0x03 | 0x06 | 0x07 | 0x45, Some(n)) => {
                r.fail(format!("Graph event {n}: HRESULT 0x{:08X}", p1 as u32))
            }
            (_, Some(n)) => r.warn(format!("Graph event {n} (param 0x{:X})", p1 as usize)),
            _ => r.info(format!(
                "Graph event 0x{code:X} (params 0x{:X}, 0x{:X})",
                p1 as usize, p2 as usize
            )),
        }
        let _ = unsafe { events.FreeEventParams(code, p1, p2) };
    }
}

fn verdict(r: &mut Report, s: &Stats, section_seen: bool, watch: &RingWatch) {
    if s.count == 0 {
        r.fail(format!(
            "Graph ran but the Sample Grabber received no samples in {} s",
            OBSERVE.as_secs()
        ));
    } else {
        let fps = match (s.first, s.last) {
            (Some(a), Some(b)) if s.count > 1 && b > a => {
                (s.count - 1) as f64 / (b - a).as_secs_f64()
            }
            _ => 0.0,
        };
        r.pass(format!(
            "{} samples delivered ({fps:.1} fps measured, last {} bytes, {} distinct contents)",
            s.count, s.last_len, s.distinct
        ));
    }
    let luma = s.last_luma.map(|l| format!(" ({l})")).unwrap_or_default();
    r.info(format!(
        "Samples: {} black fallback, {} with content{luma}",
        s.black, s.non_black
    ));
    if section_seen {
        r.info(format!(
            "Frame section during streaming: {}",
            watch.summary()
        ));
    }
    if s.count == 0 {
        return;
    }
    let analyzed = s.black + s.non_black > 0;
    if watch.frames > 0 && !analyzed {
        r.pass("deskcam.exe published frames while DeskCam was open (sample content of this format not analyzed)");
    } else if watch.frames > 0 && s.non_black > 0 {
        r.pass("Desktop frames reached the consumer: deskcam.exe published frames and samples are not the black fallback");
    } else if watch.frames > 0 {
        r.warn("deskcam.exe published frames but every sample was video black: the desktop may be black, or the filter reads a different section size than the app writes");
    } else if watch.alive > 0 {
        r.fail("deskcam.exe is alive but published no frame while DeskCam was open: capture did not start (see the graphics probe and DeskCam log), or the filter could not open the section for writing its reader heartbeat");
    } else if s.non_black > 0 {
        r.warn(
            "Samples have content but the frame section could not be observed from this process",
        );
    } else {
        r.fail("Samples are DeskCam's black fallback only: deskcam.exe is not running or not streaming in this desktop session");
    }
}
