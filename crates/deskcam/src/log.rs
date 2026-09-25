//! Minimal file logger: `%LOCALAPPDATA%\DeskCam\deskcam.log`, truncated at startup.

use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

static LOG: LazyLock<Option<(Mutex<File>, Instant)>> = LazyLock::new(|| {
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    File::create(&path).ok().map(|f| (Mutex::new(f), Instant::now()))
});

pub fn log_path() -> PathBuf {
    let base = unsafe {
        match SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            }
            Err(_) => std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into()),
        }
    };
    PathBuf::from(base).join("DeskCam").join("deskcam.log")
}

/// Creates (truncates) the log file now instead of on the first message.
pub fn init() {
    LazyLock::force(&LOG);
}

pub fn write(msg: &str) {
    if let Some((file, start)) = LOG.as_ref() {
        let mut f = file.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(f, "{:>9.3} {msg}", start.elapsed().as_secs_f64());
        let _ = f.flush();
    }
}

macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(&format!($($t)*)) };
}

pub(crate) use log;
