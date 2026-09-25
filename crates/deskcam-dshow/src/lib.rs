//! `deskcam_dshow.dll`: DirectShow capture filter for the DeskCam camera on Windows 10.
//!
//! Windows 10 has no API for adding a Media Foundation virtual camera, so this DLL is registered
//! under `CLSID_VideoInputDeviceCategory` instead. That makes DeskCam show up as a webcam in
//! DirectShow-based apps (Discord, OBS, Chrome/Edge/Firefox, Zoom, Teams). Unlike the Windows 11
//! media source, it is loaded **inside each consumer process**, which is why both a 64-bit and a
//! 32-bit build are installed. Frames come from the `Local\` section that `deskcam.exe` creates in
//! the user's session (see `deskcam_proto::layout`).
//!
//! Object layout: [`filter::Filter`] (`IBaseFilter`) owns one [`pin::OutputPin`] (`IPin`,
//! `IAMStreamConfig`, `IKsPropertySet`); a delivery thread in [`stream`] pushes one sample per
//! frame interval to the downstream pin while the graph is paused or running.

// COM methods take raw pointers by contract; their signatures come from the `windows` crate
// traits and cannot be `unsafe fn`.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod enums;
pub mod filter;
pub mod format;
pub mod pin;
mod stream;

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use deskcam_proto::{DSHOW_CLSID, DSHOW_CLSID_STR, com, paths, stream_info};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_INVALIDARG, E_POINTER, HINSTANCE, HMODULE, S_FALSE, S_OK,
};
use windows::Win32::Media::DirectShow::{
    IFilterMapper2, MERIT_DO_NOT_USE, REGFILTER2, REGFILTER2_0, REGFILTER2_0_0, REGFILTERPINS, REGPINTYPES,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_FilterMapper2, CLSID_VideoInputDeviceCategory, MEDIASUBTYPE_NV12, MEDIATYPE_Video,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, IClassFactory,
    IClassFactory_Impl,
};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_core::{BOOL, GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement};


/// Friendly name used by `regsvr32` without `/i:"name"`.
pub const DEFAULT_NAME: &str = "DeskCam";

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static OBJECTS: AtomicUsize = AtomicUsize::new(0);

/// Keeps `DllCanUnloadNow` returning `S_FALSE` while any object or thread of this DLL is alive.
pub(crate) struct ObjGuard;

impl ObjGuard {
    pub(crate) fn new() -> Self {
        OBJECTS.fetch_add(1, Ordering::AcqRel);
        ObjGuard
    }
}

impl Drop for ObjGuard {
    fn drop(&mut self) {
        OBJECTS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Debug output, visible with Sysinternals DebugView.
pub(crate) fn trace(msg: &str) {
    let text = HSTRING::from(format!("[deskcam-dshow] {msg}\n"));
    unsafe { OutputDebugStringW(&text) };
}

#[unsafe(no_mangle)]
extern "system" fn DllMain(hinst: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(hinst.0, Ordering::Release);
        unsafe {
            let _ = DisableThreadLibraryCalls(HMODULE(hinst.0));
        }
    }
    true.into()
}

#[implement(IClassFactory)]
struct ClassFactory {
    _guard: ObjGuard,
}

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> windows_core::Result<()> {
        if ppv.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *ppv = std::ptr::null_mut() };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        // The size the running app produces; stream.bin is written before the section exists.
        let info = stream_info::load_or_default(&paths::stream_file());
        let filter = filter::create(info).inspect_err(|e| trace(&format!("filter create failed: {e}")))?;
        let unknown: IUnknown = filter.to_interface();
        unsafe { unknown.query(riid, ppv).ok() }
    }

    fn LockServer(&self, lock: BOOL) -> windows_core::Result<()> {
        if lock.as_bool() {
            OBJECTS.fetch_add(1, Ordering::AcqRel);
        } else {
            OBJECTS.fetch_sub(1, Ordering::AcqRel);
        }
        Ok(())
    }
}

#[unsafe(no_mangle)]
unsafe extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    unsafe {
        *ppv = std::ptr::null_mut();
        if *rclsid != DSHOW_CLSID {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = ClassFactory { _guard: ObjGuard::new() }.into();
        factory.query(riid, ppv)
    }
}

#[unsafe(no_mangle)]
extern "system" fn DllCanUnloadNow() -> HRESULT {
    if OBJECTS.load(Ordering::Acquire) == 0 { S_OK } else { S_FALSE }
}

fn with_mapper(f: impl FnOnce(&IFilterMapper2) -> windows_core::Result<()>) -> windows_core::Result<()> {
    let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let result = unsafe { CoCreateInstance::<_, IFilterMapper2>(&CLSID_FilterMapper2, None, CLSCTX_INPROC_SERVER) }
        .and_then(|mapper| f(&mapper));
    if init.is_ok() {
        unsafe { CoUninitialize() };
    }
    result
}

/// Registers the COM class and the video capture device entry apps enumerate.
fn register(name: &str) -> windows_core::Result<()> {
    let path = com::module_path(HMODULE(MODULE.load(Ordering::Acquire)));
    com::register_inproc_server(DSHOW_CLSID_STR, name, &path)?;
    with_mapper(|mapper| unsafe {
        // Replace a previous registration (possibly under another name).
        let _ = mapper.UnregisterFilter(&CLSID_VideoInputDeviceCategory, PCWSTR::null(), &DSHOW_CLSID);
        let any_filter = GUID::zeroed();
        let types = REGPINTYPES { clsMajorType: &MEDIATYPE_Video, clsMinorType: &MEDIASUBTYPE_NV12 };
        let pin = REGFILTERPINS {
            strName: PWSTR::null(),
            bRendered: false.into(),
            bOutput: true.into(),
            bZero: false.into(),
            bMany: false.into(),
            clsConnectsToFilter: &any_filter,
            strConnectsToPin: PCWSTR::null(),
            nMediaTypes: 1,
            lpMediaType: &types,
        };
        let reg = REGFILTER2 {
            dwVersion: 1,
            dwMerit: MERIT_DO_NOT_USE.0 as u32,
            Anonymous: REGFILTER2_0 { Anonymous1: REGFILTER2_0_0 { cPins: 1, rgPins: &pin } },
        };
        // A null instance name makes the CLSID the instance key, which UnregisterFilter matches.
        mapper.RegisterFilter(
            &DSHOW_CLSID,
            &HSTRING::from(name),
            None,
            &CLSID_VideoInputDeviceCategory,
            PCWSTR::null(),
            &reg,
        )
    })
}

fn unregister() -> windows_core::Result<()> {
    let _ = with_mapper(|mapper| unsafe {
        mapper.UnregisterFilter(&CLSID_VideoInputDeviceCategory, PCWSTR::null(), &DSHOW_CLSID)
    });
    com::unregister_inproc_server(DSHOW_CLSID_STR)
}

fn to_hresult(result: windows_core::Result<()>) -> HRESULT {
    match result {
        Ok(()) => S_OK,
        Err(e) => {
            trace(&format!("registration failed: {e}"));
            e.code()
        }
    }
}

/// `regsvr32 deskcam_dshow.dll`: registers as [`DEFAULT_NAME`].
#[unsafe(no_mangle)]
extern "system" fn DllRegisterServer() -> HRESULT {
    to_hresult(register(DEFAULT_NAME))
}

#[unsafe(no_mangle)]
extern "system" fn DllUnregisterServer() -> HRESULT {
    to_hresult(unregister())
}

/// `regsvr32 /n /i:"Camera name" deskcam_dshow.dll` registers under a custom name; `/u /n /i`
/// unregisters.
#[unsafe(no_mangle)]
unsafe extern "system" fn DllInstall(install: BOOL, cmdline: PCWSTR) -> HRESULT {
    if !install.as_bool() {
        return to_hresult(unregister());
    }
    let name = if cmdline.is_null() { String::new() } else { unsafe { cmdline.to_string() }.unwrap_or_default() };
    let name = match name.trim().trim_matches('"').trim() {
        "" => DEFAULT_NAME,
        n if n.chars().count() <= 64 => n,
        _ => return E_INVALIDARG,
    };
    to_hresult(register(name))
}
