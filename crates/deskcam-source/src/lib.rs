//! `deskcam_source.dll`: Media Foundation media source for the DeskCam virtual camera.
//!
//! The Windows Camera Frame Server (LocalService, session 0) loads this DLL through the
//! HKLM-registered CLSID and pulls NV12/RGB32 samples from it. Frames come from the app via
//! the shared section described in `deskcam_proto::layout`.
//!
//! Windows 11 adds the camera at runtime with `MFCreateVirtualCamera`. On Windows 10 registering
//! this DLL also installs a software camera device that points the Frame Server at it
//! (`swdevice`), so Media Foundation and WinRT apps can use DeskCam there too.

pub mod activator;
mod attrs;
pub mod media_source;
pub mod media_stream;
mod swdevice;

use std::ffi::c_void;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use deskcam_proto::{CLSID, CLSID_STR, os, paths, stream_info};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_INVALIDARG, E_POINTER, ERROR_FILE_NOT_FOUND, HINSTANCE,
    HMODULE, S_FALSE, S_OK,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleFileNameW};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW,
    RegDeleteTreeW, RegOpenKeyExW, RegSetValueExW,
};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows_core::{BOOL, GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, Ref, implement};

/// Camera name of the Windows 10 device when `regsvr32` gets no `/i:"name"`.
pub const DEFAULT_NAME: &str = "DeskCam";

static MODULE: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static OBJECTS: AtomicUsize = AtomicUsize::new(0);

/// Keeps `DllCanUnloadNow` returning `S_FALSE` while any COM object of this DLL is alive.
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

/// Debug output, visible with Sysinternals DebugView ("Capture Global Win32" for the service).
pub(crate) fn trace(msg: &str) {
    let text = HSTRING::from(format!("[deskcam-source] {msg}\n"));
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
        let info = stream_info::load_or_default(&paths::stream_file());
        let activator = activator::create(info).inspect_err(|e| trace(&format!("activator create failed: {e}")))?;
        unsafe { activator.query(riid, ppv).ok() }
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
        if *rclsid != CLSID {
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

fn clsid_key() -> String {
    format!("Software\\Classes\\CLSID\\{CLSID_STR}")
}

fn module_path() -> String {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(Some(HMODULE(MODULE.load(Ordering::Acquire))), &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..len])
}

fn set_string(key: HKEY, name: PCWSTR, value: &str) -> windows_core::Result<()> {
    let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    unsafe { RegSetValueExW(key, name, None, REG_SZ, Some(bytes)).ok() }
}

/// Registers the class under HKLM; the Frame Server does not read per-user registrations.
fn register_class() -> windows_core::Result<()> {
    let path = module_path();
    let subkey = HSTRING::from(format!("{}\\InprocServer32", clsid_key()));
    let mut key = HKEY::default();
    let result = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            &subkey,
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()
        .and_then(|()| {
            let r = set_string(key, PCWSTR::null(), &path)
                .and_then(|()| set_string(key, windows_core::w!("ThreadingModel"), "Both"));
            let _ = RegCloseKey(key);
            r
        })
    };
    result
}

/// Whether the class key exists; unreadable counts as existing so rollback never deletes it.
fn class_exists() -> bool {
    let mut key = HKEY::default();
    let status = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, &HSTRING::from(clsid_key()), None, KEY_READ, &mut key) };
    if status.is_ok() {
        unsafe {
            let _ = RegCloseKey(key);
        }
    }
    status != ERROR_FILE_NOT_FOUND
}

fn delete_class() -> windows_core::Result<()> {
    let status = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, &HSTRING::from(clsid_key())) };
    if status.is_ok() || status == ERROR_FILE_NOT_FOUND { Ok(()) } else { Err(status.to_hresult().into()) }
}

fn register(name: &str) -> windows_core::Result<()> {
    if os::has_mf_virtual_camera() {
        register_class()?;
        // Windows 11 adds the camera at runtime; a device left from Windows 10 would list it twice.
        return swdevice::remove();
    }
    // A failed first install leaves no class registration behind; an existing one (a previous
    // install being renamed or updated) is kept, as is its still-installed device.
    let existed = class_exists();
    let result = register_class().and_then(|()| swdevice::install(name));
    if result.is_err()
        && !existed
        && let Err(e) = delete_class()
    {
        trace(&format!("rolling back the class registration failed: {e}"));
    }
    result
}

fn unregister() -> windows_core::Result<()> {
    swdevice::remove()?;
    delete_class()
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

/// `regsvr32 deskcam_source.dll`; on Windows 10 the camera is named [`DEFAULT_NAME`].
#[unsafe(no_mangle)]
extern "system" fn DllRegisterServer() -> HRESULT {
    to_hresult(register(DEFAULT_NAME))
}

#[unsafe(no_mangle)]
extern "system" fn DllUnregisterServer() -> HRESULT {
    to_hresult(unregister())
}

/// `regsvr32 /n /i:"Camera name" deskcam_source.dll` names the Windows 10 camera; `/u /n /i`
/// unregisters. Windows 11 takes the name from the app at runtime.
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
