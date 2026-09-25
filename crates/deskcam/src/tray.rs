//! Notification-area icon: status tooltip, error balloons and the Open config / Open log /
//! Restart / Exit menu. Runs on the main thread; the worker runs on its own thread.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use deskcam_proto::paths;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    ShellExecuteW, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_core::{HSTRING, PCWSTR, w};

use crate::log::{log, log_path};
use crate::worker::{self, Status};
use crate::config;

pub const WINDOW_CLASS: PCWSTR = w!("DeskCamTray");
const WM_TRAY: u32 = WM_APP + 1;
const WM_STATUS: u32 = WM_APP + 2;
const ID_OPEN_CONFIG: usize = 1;
const ID_OPEN_LOG: usize = 2;
const ID_RESTART: usize = 3;
const ID_EXIT: usize = 4;

struct App {
    hwnd: HWND,
    taskbar_created: u32,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    status: Arc<Mutex<Status>>,
    last_error_shown: Option<String>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn copy_wide<const N: usize>(dst: &mut [u16; N], text: &str) {
    let wide: Vec<u16> = text.encode_utf16().take(N - 1).collect();
    dst[..wide.len()].copy_from_slice(&wide);
    dst[wide.len()] = 0;
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
}

fn add_icon(hwnd: HWND, tip: &str) {
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default();
    copy_wide(&mut nid.szTip, tip);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    }
}

fn current_status(app: &App) -> Status {
    app.status.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn set_status(app: &App, status: Status) {
    *app.status.lock().unwrap_or_else(|p| p.into_inner()) = status;
    unsafe {
        let _ = PostMessageW(Some(app.hwnd), WM_STATUS, WPARAM(0), LPARAM(0));
    }
}

fn start_worker(app: &mut App) {
    set_status(app, Status::Starting);
    match config::load() {
        Err(msg) => {
            log!("{msg}");
            set_status(app, Status::Error(msg));
        }
        Ok(cfg) => {
            log!("config: {cfg:?}");
            let stop = Arc::new(AtomicBool::new(false));
            let status = app.status.clone();
            let hwnd = app.hwnd.0 as isize;
            app.stop = stop.clone();
            app.worker = Some(worker::spawn(cfg, stop, move |s| {
                *status.lock().unwrap_or_else(|p| p.into_inner()) = s;
                unsafe {
                    let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_STATUS, WPARAM(0), LPARAM(0));
                }
            }));
        }
    }
}

fn stop_worker(app: &mut App) {
    app.stop.store(true, Ordering::Release);
    if let Some(handle) = app.worker.take() {
        let _ = handle.join();
    }
}

fn on_status(app: &mut App) {
    let status = current_status(app);
    let mut nid = icon_data(app.hwnd);
    nid.uFlags = NIF_TIP;
    copy_wide(&mut nid.szTip, &status.to_string());
    match &status {
        Status::Error(msg) if app.last_error_shown.as_deref() != Some(msg) => {
            nid.uFlags |= NIF_INFO;
            nid.dwInfoFlags = NIIF_ERROR;
            copy_wide(&mut nid.szInfoTitle, "DeskCam");
            copy_wide(&mut nid.szInfo, msg);
            app.last_error_shown = Some(msg.clone());
        }
        Status::Error(_) => {}
        _ => app.last_error_shown = None,
    }
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

fn open(path: &std::path::Path) {
    unsafe {
        ShellExecuteW(None, w!("open"), &HSTRING::from(path.as_os_str()), None, None, SW_SHOWNORMAL);
    }
}

fn show_menu(hwnd: HWND, status: &str) -> usize {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, &HSTRING::from(status));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_CONFIG, w!("Open config"));
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN_LOG, w!("Open log"));
        let _ = AppendMenuW(menu, MF_STRING, ID_RESTART, w!("Restart"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT, w!("Exit"));
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD, pt.x, pt.y, None, hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    }
}

/// Runs `f` with the app state. The borrow is never held across calls that pump messages
/// (menus, DestroyWindow), so re-entrant window messages cannot hit an active borrow.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| cell.try_borrow_mut().ok().and_then(|mut guard| guard.as_mut().map(f)))
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_STATUS => {
            with_app(on_status);
        }
        WM_TRAY => {
            let event = (lparam.0 & 0xFFFF) as u32;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                let Some(status) = with_app(|app| current_status(app).to_string()) else { return LRESULT(0) };
                match show_menu(hwnd, &status) {
                    ID_OPEN_CONFIG => open(&paths::config_file()),
                    ID_OPEN_LOG => open(&log_path()),
                    ID_RESTART => {
                        log!("restart requested");
                        with_app(|app| {
                            stop_worker(app);
                            start_worker(app);
                        });
                    }
                    ID_EXIT => unsafe {
                        let _ = DestroyWindow(hwnd);
                    },
                    _ => {}
                }
            }
        }
        WM_CLOSE => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        WM_DESTROY => {
            with_app(stop_worker);
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &icon_data(hwnd));
                PostQuitMessage(0);
            }
        }
        m if m != 0 && with_app(|app| app.taskbar_created == m) == Some(true) => {
            let tip = with_app(|app| current_status(app).to_string()).unwrap_or_default();
            add_icon(hwnd, &tip);
        }
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    LRESULT(0)
}

pub fn run() -> windows_core::Result<()> {
    unsafe {
        let hinstance = GetModuleHandleW(None)?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: WINDOW_CLASS,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(windows_core::Error::from_thread());
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            WINDOW_CLASS,
            w!("DeskCam"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        )?;
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        APP.with(|cell| {
            *cell.borrow_mut() = Some(App {
                hwnd,
                taskbar_created,
                stop: Arc::new(AtomicBool::new(false)),
                worker: None,
                status: Arc::new(Mutex::new(Status::Starting)),
                last_error_shown: None,
            })
        });
        add_icon(hwnd, &Status::Starting.to_string());
        APP.with(|cell| {
            if let Some(app) = cell.borrow_mut().as_mut() {
                start_worker(app);
            }
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    log!("exiting");
    Ok(())
}
