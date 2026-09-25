use std::path::PathBuf;

use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_ProgramData, KF_FLAG_DEFAULT, SHGetKnownFolderPath};

/// `C:\ProgramData\DeskCam`: readable by the Frame Server (LocalService) and the user session.
pub fn data_dir() -> PathBuf {
    let base = unsafe {
        match SHGetKnownFolderPath(&FOLDERID_ProgramData, KF_FLAG_DEFAULT, None) {
            Ok(p) => {
                let s = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                s
            }
            Err(_) => String::new(),
        }
    };
    let base = if base.is_empty() { r"C:\ProgramData".to_owned() } else { base };
    PathBuf::from(base).join("DeskCam")
}

pub fn stream_file() -> PathBuf {
    data_dir().join("stream.bin")
}

pub fn config_file() -> PathBuf {
    data_dir().join("config.ini")
}
