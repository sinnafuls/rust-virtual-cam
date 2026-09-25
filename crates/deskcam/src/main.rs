#![windows_subsystem = "windows"]
//! DeskCam: streams a monitor as a webcam from a tray-icon background app. Windows 11 uses the
//! Media Foundation virtual camera; Windows 10 uses a DirectShow capture filter (see `backend`).
//!
//! `deskcam.exe` runs the app; `deskcam.exe stop` asks a running instance to exit.

mod backend;
mod capture;
mod config;
mod log;
mod monitor;
mod tray;
mod vcam;
mod worker;

use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, LPARAM, WPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_CLOSE};
use windows_core::{PCWSTR, w};

fn main() {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("stop") => {
            let code = match unsafe { FindWindowW(tray::WINDOW_CLASS, PCWSTR::null()) } {
                Ok(hwnd) if !hwnd.is_invalid() => {
                    let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
                    0
                }
                _ => 1,
            };
            std::process::exit(code);
        }
        Some(_) => std::process::exit(2),
    }

    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    // Held for the process lifetime; a second instance exits quietly.
    let _instance = unsafe { CreateMutexW(None, true, w!("Local\\DeskCam.Instance")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return;
    }
    log::init();
    log::log!("DeskCam {} starting", env!("CARGO_PKG_VERSION"));
    if let Err(e) = tray::run() {
        log::log!("tray failed: {e}");
        std::process::exit(1);
    }
}
