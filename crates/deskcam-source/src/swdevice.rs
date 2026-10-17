//! Windows 10 camera device for the Frame Server.
//!
//! Windows 10 has no `MFCreateVirtualCamera`, but its Frame Server already supports custom media
//! sources: a camera interface (`KSCATEGORY_VIDEO_CAMERA`) whose registry key names a COM class in
//! `CustomCaptureSourceClsid` is served by CoCreating that class in the service. Microsoft's docs
//! register the interface from a stub driver; a driverless software device (`SwDeviceCreate`)
//! works the same and needs no driver signing. Media Foundation and WinRT apps (AMD Privacy View,
//! the Camera app) then list the camera; DirectShow apps keep using `deskcam_dshow.dll`, because
//! the device is deliberately not registered under `KSCATEGORY_VIDEO`.
//!
//! The device persists (`SWDeviceLifetimeParentPresent`) until [`remove`]; both need administrator
//! rights and run from `regsvr32`. [`remove`] runs on every Windows version, so a device left from
//! Windows 10 goes away after an upgrade to 11.

use std::sync::mpsc;
use std::time::Duration;

use deskcam_proto::CLSID_STR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, CM_LOCATE_DEVNODE_PHANTOM, CM_Locate_DevNodeW, CM_MapCrToWin32Err,
    CM_Open_Device_Interface_KeyW, CM_Query_And_Remove_SubTreeW, CM_REMOVE_NO_RESTART, CM_REMOVE_UI_NOT_OK,
    CM_Uninstall_DevNode, CONFIGRET, CR_NO_SUCH_DEVNODE, CR_SUCCESS, RegDisposition_OpenAlways,
};
use windows::Win32::Devices::Enumeration::Pnp::{
    HSWDEVICE, SW_DEVICE_CREATE_INFO, SWDeviceCapabilitiesSilentInstall, SWDeviceLifetimeParentPresent,
    SwDeviceClose, SwDeviceCreate, SwDeviceInterfaceRegister, SwDeviceInterfaceSetState, SwDeviceSetLifetime,
    SwMemFree,
};
use windows::Win32::Devices::Properties::{DEVPROP_STORE_SYSTEM, DEVPROP_TYPE_STRING, DEVPROPCOMPKEY, DEVPROPERTY};
use windows::Win32::Foundation::{
    CloseHandle, DEVPROPKEY, E_FAIL, ERROR_NO_TOKEN, ERROR_NOT_ALL_ASSIGNED, ERROR_PRIVILEGE_NOT_HELD, ERROR_TIMEOUT,
    GetLastError, HANDLE, LUID, WIN32_ERROR,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_LOAD_DRIVER_NAME, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Registry::{HKEY, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegSetValueExW};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken};
use windows_core::{GUID, HRESULT, PCWSTR, w};

/// Device instance `SWD\DeskCam\DeskCam0`. Never change once installed: [`remove`] finds the
/// device by [`INSTANCE_ID`].
const ENUMERATOR: PCWSTR = w!("DeskCam");
const INSTANCE: PCWSTR = w!("DeskCam0");
const INSTANCE_ID: PCWSTR = w!("SWD\\DeskCam\\DeskCam0");
const HARDWARE_IDS: PCWSTR = w!("DeskCam\\VirtualCamera\0");
const REFERENCE: PCWSTR = w!("DeskCam");
const KSCATEGORY_VIDEO_CAMERA: GUID = GUID::from_u128(0xe5323777_f976_4f5b_9b55_b94699c46e44);
const DEVPKEY_DEVICEINTERFACE_FRIENDLYNAME: DEVPROPKEY =
    DEVPROPKEY { fmtid: GUID::from_u128(0x026e516e_b814_414b_83cd_856d6fef4822), pid: 2 };

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Owns the boxed sender, so a callback racing [`open`]'s timeout still has valid context. After
/// `SwDeviceClose` returns the callback is never called, so a timed-out create leaks at most the
/// sender instead of risking a double free.
unsafe extern "system" fn on_created(_device: HSWDEVICE, result: HRESULT, context: *const core::ffi::c_void, _id: PCWSTR) {
    let done = unsafe { Box::from_raw(context as *mut mpsc::Sender<HRESULT>) };
    let _ = done.send(result);
}

/// Creates the device, or re-attaches to the persistent one; `description` may update its name.
/// Closing the handle of a new device (default `SWDeviceLifetimeHandle`) unplugs it again, while a
/// re-attached device keeps its `SWDeviceLifetimeParentPresent` lifetime.
fn open(description: &str) -> windows_core::Result<HSWDEVICE> {
    let description = wide(description);
    let info = SW_DEVICE_CREATE_INFO {
        cbSize: size_of::<SW_DEVICE_CREATE_INFO>() as u32,
        pszInstanceId: INSTANCE,
        pszzHardwareIds: HARDWARE_IDS,
        CapabilityFlags: SWDeviceCapabilitiesSilentInstall.0 as u32,
        pszDeviceDescription: PCWSTR(description.as_ptr()),
        ..Default::default()
    };
    let (done, created) = mpsc::channel::<HRESULT>();
    let done = Box::into_raw(Box::new(done));
    let device = unsafe {
        SwDeviceCreate(ENUMERATOR, w!("HTREE\\ROOT\\0"), &info, None, Some(on_created), Some(done as *const _))
    };
    let device = match device {
        Ok(device) => device,
        Err(e) => {
            // No callback is coming; take the sender back.
            drop(unsafe { Box::from_raw(done) });
            return Err(e);
        }
    };
    // The handle is only usable once the callback has run.
    let result = created.recv_timeout(Duration::from_secs(30)).unwrap_or(ERROR_TIMEOUT.to_hresult());
    if result.is_err() {
        unsafe { SwDeviceClose(device) };
        return Err(result.into());
    }
    Ok(device)
}

fn set_interface_values(path: PCWSTR, name: &str) -> windows_core::Result<()> {
    let mut key = HKEY::default();
    let cr = unsafe { CM_Open_Device_Interface_KeyW(path, KEY_SET_VALUE.0, RegDisposition_OpenAlways, &mut key, 0) };
    if cr != CR_SUCCESS {
        return Err(cr_error(cr));
    }
    let set = |value_name: PCWSTR, value: &str| {
        let value = wide(value);
        let bytes = unsafe { std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2) };
        unsafe { RegSetValueExW(key, value_name, None, REG_SZ, Some(bytes)).ok() }
    };
    let result = set(w!("CustomCaptureSourceClsid"), CLSID_STR).and_then(|()| set(w!("FriendlyName"), name));
    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

fn register_interface(device: HSWDEVICE, name: &str) -> windows_core::Result<()> {
    let name_w = wide(name);
    let friendly = DEVPROPERTY {
        CompKey: DEVPROPCOMPKEY {
            Key: DEVPKEY_DEVICEINTERFACE_FRIENDLYNAME,
            Store: DEVPROP_STORE_SYSTEM,
            LocaleName: PCWSTR::null(),
        },
        Type: DEVPROP_TYPE_STRING,
        BufferSize: (name_w.len() * 2) as u32,
        Buffer: name_w.as_ptr() as *mut _,
    };
    let path = unsafe { SwDeviceInterfaceRegister(device, &KSCATEGORY_VIDEO_CAMERA, REFERENCE, Some(&[friendly]), false)? };
    // Enabled only after the key names the media source, so no app sees a camera it cannot open.
    let result = set_interface_values(PCWSTR(path.0), name)
        .and_then(|()| unsafe { SwDeviceInterfaceSetState(device, PCWSTR(path.0), true) });
    unsafe { SwMemFree(path.0 as *const _) };
    result
}

/// Adds (or renames) the persistent camera device named `name`. On failure a new device is
/// unplugged again by closing its handle; an existing one stays installed.
pub fn install(name: &str) -> windows_core::Result<()> {
    let device = open(name)?;
    let result = register_interface(device, name)
        .and_then(|()| unsafe { SwDeviceSetLifetime(device, SWDeviceLifetimeParentPresent) });
    unsafe { SwDeviceClose(device) };
    result
}

fn cr_error(cr: CONFIGRET) -> windows_core::Error {
    WIN32_ERROR(unsafe { CM_MapCrToWin32Err(cr, E_FAIL.0 as u32) }).to_hresult().into()
}

/// `SeLoadDriverPrivilege` enabled on the calling thread's effective token while alive.
/// `CM_Query_And_Remove_SubTreeW` requires it; elevated administrators hold it disabled. Drop
/// restores the previous state and closes the token.
struct LoadDriverPrivilege {
    token: HANDLE,
    /// What `AdjustTokenPrivileges` changed; empty when the privilege was already enabled.
    previous: TOKEN_PRIVILEGES,
}

impl LoadDriverPrivilege {
    fn enable() -> windows_core::Result<Self> {
        let access = TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY;
        let mut token = HANDLE::default();
        // An impersonating thread is checked against its own token, not the process token.
        if let Err(e) = unsafe { OpenThreadToken(GetCurrentThread(), access, true, &mut token) } {
            if e.code() != ERROR_NO_TOKEN.to_hresult() {
                return Err(e);
            }
            unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut token)? };
        }
        let mut guard = Self { token, previous: TOKEN_PRIVILEGES::default() };
        let mut luid = LUID::default();
        unsafe { LookupPrivilegeValueW(PCWSTR::null(), SE_LOAD_DRIVER_NAME, &mut luid)? };
        let enable = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        let mut len = 0u32;
        unsafe {
            AdjustTokenPrivileges(
                token,
                false,
                Some(&raw const enable),
                size_of::<TOKEN_PRIVILEGES>() as u32,
                Some(&raw mut guard.previous),
                Some(&raw mut len),
            )?;
        }
        // Success with ERROR_NOT_ALL_ASSIGNED: the token does not hold the privilege at all.
        if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
            return Err(windows_core::Error::new(
                ERROR_PRIVILEGE_NOT_HELD.to_hresult(),
                "removing the DeskCam camera device needs SeLoadDriverPrivilege; run regsvr32 from an elevated administrator prompt",
            ));
        }
        Ok(guard)
    }
}

impl Drop for LoadDriverPrivilege {
    fn drop(&mut self) {
        unsafe {
            if self.previous.PrivilegeCount != 0 {
                let _ = AdjustTokenPrivileges(self.token, false, Some(&raw const self.previous), 0, None, None);
            }
            let _ = CloseHandle(self.token);
        }
    }
}

/// Uninstalls the camera device if it exists (present or not); a missing device is success.
/// Goes through PnP rather than `SwDeviceCreate`, which would first create the device.
pub fn remove() -> windows_core::Result<()> {
    let mut devinst = 0;
    let cr = unsafe { CM_Locate_DevNodeW(&mut devinst, INSTANCE_ID, CM_LOCATE_DEVNODE_PHANTOM) };
    if cr == CR_NO_SUCH_DEVNODE {
        return Ok(());
    }
    if cr != CR_SUCCESS {
        return Err(cr_error(cr));
    }
    let (mut status, mut problem) = Default::default();
    // Only a present device has a status; it must be stopped before it can be uninstalled.
    if unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, devinst, 0) } == CR_SUCCESS {
        let _privilege = LoadDriverPrivilege::enable()?;
        let cr = unsafe {
            CM_Query_And_Remove_SubTreeW(devinst, None, None, CM_REMOVE_UI_NOT_OK | CM_REMOVE_NO_RESTART)
        };
        if cr != CR_SUCCESS {
            return Err(cr_error(cr));
        }
    }
    let cr = unsafe { CM_Uninstall_DevNode(devinst, 0) };
    if cr == CR_SUCCESS || cr == CR_NO_SUCH_DEVNODE { Ok(()) } else { Err(cr_error(cr)) }
}
