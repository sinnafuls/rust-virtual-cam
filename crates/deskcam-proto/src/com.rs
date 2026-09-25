//! HKLM registration helpers for in-process COM servers, and the DirectShow device lookup the
//! app uses to report what the Windows 10 camera is registered as.

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HMODULE};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegGetValueW, RegSetValueExW,
};
use windows_core::{HSTRING, PCWSTR, w};

use crate::DSHOW_CLSID_STR;

/// `CLSID_VideoInputDeviceCategory`, the DirectShow category apps enumerate for cameras.
const VIDEO_INPUT_CATEGORY: &str = "{860BB310-5D01-11D0-BD3B-00A0C911CE86}";

pub fn module_path(module: HMODULE) -> String {
    let mut buf = [0u16; 1024];
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..len])
}

fn clsid_key(clsid: &str) -> String {
    format!("Software\\Classes\\CLSID\\{clsid}")
}

fn set_string(key: HKEY, name: PCWSTR, value: &str) -> windows_core::Result<()> {
    let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2) };
    unsafe { RegSetValueExW(key, name, None, REG_SZ, Some(bytes)).ok() }
}

fn with_key(subkey: &str, f: impl FnOnce(HKEY) -> windows_core::Result<()>) -> windows_core::Result<()> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(subkey),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
        .ok()?;
    }
    let result = f(key);
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

/// Writes `HKLM\Software\Classes\CLSID\{clsid}` with `InprocServer32 = path`, `ThreadingModel = Both`.
/// A 32-bit caller lands in the WOW6432Node view, which is what 32-bit consumers read.
pub fn register_inproc_server(clsid: &str, description: &str, path: &str) -> windows_core::Result<()> {
    with_key(&clsid_key(clsid), |key| set_string(key, PCWSTR::null(), description))?;
    with_key(&format!("{}\\InprocServer32", clsid_key(clsid)), |key| {
        set_string(key, PCWSTR::null(), path)?;
        set_string(key, w!("ThreadingModel"), "Both")
    })
}

pub fn unregister_inproc_server(clsid: &str) -> windows_core::Result<()> {
    let status = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, &HSTRING::from(clsid_key(clsid))) };
    if status.is_ok() || status == ERROR_FILE_NOT_FOUND { Ok(()) } else { Err(status.to_hresult().into()) }
}

/// Friendly name of the registered DirectShow camera, or `None` when the filter is not registered
/// (in this process' registry view).
pub fn registered_dshow_name() -> Option<String> {
    let subkey = HSTRING::from(format!("CLSID\\{VIDEO_INPUT_CATEGORY}\\Instance\\{DSHOW_CLSID_STR}"));
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            &subkey,
            w!("FriendlyName"),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let len = (size as usize / 2).saturating_sub(1);
    Some(String::from_utf16_lossy(&buf[..len]))
}
