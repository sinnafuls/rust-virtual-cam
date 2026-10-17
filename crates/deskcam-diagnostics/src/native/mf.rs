//! Media Foundation, the way UWP/MF apps (Windows Camera, WebView2 on Windows 11, ...) find
//! cameras: `MFEnumDeviceSources` for video capture devices.
//!
//! Every MF entry point is resolved with `LoadLibraryExW(SYSTEM32)` + `GetProcAddress`, so the
//! diagnostics executable starts even on Windows N editions without the Media Feature Pack.
//! `MFCreateVirtualCamera` is only looked up, never called: no camera is created or registered.
//!
//! Windows 10 exposes a driverless Frame Server camera alongside its DirectShow filter.
//! Both platforms should enumerate a matching MF camera unless Windows 11 is forced to DirectShow.
//! Enumerate without activating a camera: a friendly-name match alone cannot prove identity.

use deskcam_proto::{CLSID_STR, os, paths};
use std::ffi::c_void;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_VERSION, MFSTARTUP_FULL,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_core::{GUID, HRESULT, HSTRING, Interface, PCSTR, PWSTR};

use super::{Report, hr, step};

type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
type ShutdownFn = unsafe extern "system" fn() -> HRESULT;
type CreateAttributesFn = unsafe extern "system" fn(*mut *mut c_void, u32) -> HRESULT;
type EnumDeviceSourcesFn =
    unsafe extern "system" fn(*mut c_void, *mut *mut *mut c_void, *mut u32) -> HRESULT;
type RawFn = unsafe extern "system" fn() -> isize;

/// An export of a System32 DLL, or why it is unavailable.
fn export(r: &mut Report, dll: &str, name: &str) -> Result<RawFn, String> {
    r.stage(&format!(
        "LoadLibraryExW({dll}, LOAD_LIBRARY_SEARCH_SYSTEM32) + GetProcAddress({name})"
    ));
    let module = unsafe { LoadLibraryExW(&HSTRING::from(dll), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|e| format!("{dll} cannot be loaded: {}", hr(&e)))?;
    let cname = format!("{name}\0");
    match unsafe { GetProcAddress(module, PCSTR(cname.as_ptr())) } {
        Some(f) => Ok(f),
        None => {
            let e = windows_core::Error::from_thread();
            Err(format!("{dll} has no export {name}: {}", hr(&e)))
        }
    }
}

fn mf_hint(code: u32) -> &'static str {
    match code {
        0x8007_0005 => {
            " - likely: camera access denied (Settings > Privacy > Camera, desktop apps)"
        }
        0xC00D_ABE0 => " - likely: device invalidated (camera removed while opening)",
        0xC00D_ABE1 => " - likely: camera in exclusive use by another application",
        0xC00D_36E3 => " - likely: Media Foundation version mismatch",
        0x8007_0002 | 0x8007_007E => {
            " - likely: a Media Foundation component is missing (Media Feature Pack)"
        }
        0x8007_0490 => {
            " - likely: device no longer present (virtual camera removed: deskcam.exe exited?)"
        }
        _ => "",
    }
}

fn hr_mf(e: &windows_core::Error) -> String {
    format!("{}{}", hr(e), mf_hint(e.code().0 as u32))
}

pub(super) fn run(r: &mut Report) {
    let win11 = os::has_mf_virtual_camera();
    virtual_camera_api(r, win11);

    r.stage("CoInitializeEx(COINIT_MULTITHREADED)");
    if let Err(e) = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok() {
        r.fail(format!("CoInitializeEx: {}", hr(&e)));
        return;
    }
    let mfplat = (
        export(r, "mfplat.dll", "MFStartup"),
        export(r, "mfplat.dll", "MFShutdown"),
        export(r, "mfplat.dll", "MFCreateAttributes"),
    );
    let (startup, shutdown, create_attributes) = match mfplat {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
        (a, b, c) => {
            let why = [a.err(), b.err(), c.err()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("; ");
            r.fail(format!(
                "Media Foundation platform unavailable: {why}. Windows N/KN editions need the Media Feature Pack; deskcam.exe calls MFStartup at start and cannot run without it"
            ));
            r.skip("MF device enumeration: Media Foundation is not available");
            return;
        }
    };
    let startup: StartupFn = unsafe { std::mem::transmute::<RawFn, StartupFn>(startup) };
    let shutdown: ShutdownFn = unsafe { std::mem::transmute::<RawFn, ShutdownFn>(shutdown) };
    let create_attributes: CreateAttributesFn =
        unsafe { std::mem::transmute::<RawFn, CreateAttributesFn>(create_attributes) };

    r.stage("MFStartup(MF_VERSION, MFSTARTUP_FULL)");
    if let Err(e) = unsafe { startup(MF_VERSION, MFSTARTUP_FULL) }.ok() {
        r.fail(format!(
            "MFStartup: {} (deskcam.exe needs this at start)",
            hr_mf(&e)
        ));
        return;
    }
    r.pass("MFStartup succeeded (Media Foundation platform present)");

    enumerate(r, win11, create_attributes);

    r.stage("MFShutdown");
    if let Err(e) = unsafe { shutdown() }.ok() {
        r.warn(format!("MFShutdown: {}", hr(&e)));
    }
}

/// Looks up (never calls) the Windows 11 virtual camera API the MF backend needs.
fn virtual_camera_api(r: &mut Report, win11: bool) {
    match export(r, "mfsensorgroup.dll", "MFCreateVirtualCamera") {
        Ok(_) if win11 => r.pass("mfsensorgroup.dll exports MFCreateVirtualCamera (looked up only; no camera created)"),
        Ok(_) => r.info("mfsensorgroup.dll exports MFCreateVirtualCamera although the build is below 22000"),
        Err(why) if win11 => r.fail(format!("MFCreateVirtualCamera unavailable on Windows 11: {why}; the MF backend cannot create the camera")),
        Err(why) => r.info(format!("MFCreateVirtualCamera absent, as expected before Windows 11 ({why}); DeskCam registers a driverless Frame Server camera and DirectShow filters instead")),
    }
}

fn attr_string(act: &IMFActivate, key: &GUID) -> Option<String> {
    let (mut p, mut len) = (PWSTR::null(), 0u32);
    unsafe { act.GetAllocatedString(key, &mut p, &mut len) }.ok()?;
    let s = unsafe { p.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s
}

/// Read only the settings needed for discovery, with the same bounded reader as inventory.
fn configured_camera(r: &mut Report) -> (String, bool) {
    let path = paths::config_file();
    let bytes = match crate::inventory::read_bounded(&path, 16 * 1024 + 1) {
        Ok(bytes) if bytes.len() <= 16 * 1024 => bytes,
        Ok(_) => {
            r.warn("config.ini exceeds 16 KiB; cannot infer the MF camera name/backend");
            return ("DeskCam".into(), true);
        }
        Err(e) => {
            r.info(format!(
                "{} not readable ({e}); assuming default name/backend",
                path.display()
            ));
            return ("DeskCam".into(), true);
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let (backend, name) = crate::inventory::camera_keys(&text);
    let expects_mf = !os::has_mf_virtual_camera()
        || !backend.is_some_and(|b| matches!(b.to_ascii_lowercase().as_str(), "dshow" | "directshow"));
    (name.unwrap_or_else(|| "DeskCam".into()), expects_mf)
}

fn enumerate(r: &mut Report, win11: bool, create_attributes: CreateAttributesFn) {
    let enum_sources = match export(r, "mf.dll", "MFEnumDeviceSources") {
        Ok(f) => unsafe { std::mem::transmute::<RawFn, EnumDeviceSourcesFn>(f) },
        Err(why) => {
            r.fail(format!("MFEnumDeviceSources unavailable: {why}"));
            return;
        }
    };
    let Some(attrs) = step(
        r,
        "MFCreateAttributes + SetGUID(SOURCE_TYPE = VIDCAP)",
        || unsafe {
            let mut raw = std::ptr::null_mut();
            create_attributes(&mut raw, 1).ok()?;
            let attrs = IMFAttributes::from_raw(raw);
            attrs.SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )?;
            Ok(attrs)
        },
    ) else {
        return;
    };
    let Some(devices) = step(r, "MFEnumDeviceSources(video capture)", || unsafe {
        let (mut list, mut count) = (std::ptr::null_mut::<*mut c_void>(), 0u32);
        enum_sources(attrs.as_raw(), &mut list, &mut count).ok()?;
        let mut out = Vec::with_capacity(count as usize);
        if !list.is_null() {
            for i in 0..count as usize {
                let p = *list.add(i);
                if !p.is_null() {
                    out.push(IMFActivate::from_raw(p));
                }
            }
            CoTaskMemFree(Some(list as *const c_void));
        }
        Ok(out)
    }) else {
        return;
    };

    let (name, expects_mf) = configured_camera(r);
    r.info(format!(
        "Looking for DeskCam by configured name '{name}' (MF does not expose the source CLSID {CLSID_STR} to apps)"
    ));
    let mut found = None;
    let virtual_name = format!("{name} (Windows Virtual Camera)");
    for (i, act) in devices.iter().enumerate() {
        let friendly = attr_string(act, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
            .unwrap_or_else(|| "<no name>".into());
        let link = attr_string(
            act,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
        )
        .unwrap_or_default();
        let is_deskcam = friendly == name || friendly == virtual_name;
        let software = link.to_ascii_lowercase().starts_with(r"\\?\swd#");
        r.info(format!(
            "Camera {}: '{friendly}'{}{} | {link}",
            i + 1,
            if is_deskcam {
                " [name matches DeskCam configuration; identity unverified]"
            } else {
                ""
            },
            if software { " [software device]" } else { "" }
        ));
        if is_deskcam && found.is_none() {
            found = Some(friendly);
        }
    }
    r.info(format!(
        "{} Media Foundation video capture device(s)",
        devices.len()
    ));

    match found {
        Some(friendly) => r.info(format!(
            "MF lists a camera matching DeskCam's configured name: '{friendly}' (name match only)"
        )),
        None if expects_mf => r.fail(format!(
            "No MF camera matching '{name}': {}",
            if win11 {
                "leave deskcam.exe running with the MF backend and check its log for startup errors"
            } else {
                "reinstall DeskCam to register its software camera and deskcam_source.dll; check the Frame Server service"
            }
        )),
        None => r.info(
            "No matching MF camera: expected for the DirectShow backend; MF-only apps cannot discover that filter",
        ),
    }
    r.info("Enumeration only: no MF camera was activated. DirectShow sample delivery is checked separately by CLSID.");
}
