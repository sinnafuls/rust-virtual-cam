//! Read-only inventory of everything DeskCam depends on outside its own process: Windows build and
//! edition, this process' token, related running processes, the shared `config.ini` / `stream.bin`,
//! installed files, COM / DirectShow / Media Foundation registrations in both registry views
//! (machine-wide and per-user), DirectShow core classes, the Frame Server services, Media
//! Foundation / DirectShow system files and the camera / screen capture privacy settings.
//!
//! Nothing is written, registered, started or downloaded. No DLL is loaded for execution: PE
//! headers and export tables are parsed from bounded file reads, and version resources are read with
//! `GetFileVersionInfoW`, which maps the file as data without running its code.
//!
//! The report contains file paths, the account name and SID, and process image paths.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use deskcam_proto::{CLSID_STR, DEFAULT_STREAM, DSHOW_CLSID_STR, StreamInfo, os, paths};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS,
    ERROR_SERVICE_DOES_NOT_EXIST, GetLastError, HANDLE, HLOCAL, LocalFree, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, LookupAccountSidW, PSID,
    SID_NAME_USE, TOKEN_ELEVATION, TOKEN_ELEVATION_TYPE, TOKEN_INFORMATION_CLASS,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER, TokenElevation, TokenElevationType,
    TokenIntegrityLevel, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY,
    REG_BINARY, REG_DWORD, REG_EXPAND_SZ, REG_MULTI_SZ, REG_QWORD, REG_SAM_FLAGS, REG_SZ,
    REG_VALUE_TYPE, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QUERY_SERVICE_CONFIGW, QueryServiceConfig2W,
    QueryServiceConfigW, QueryServiceStatusEx, SC_HANDLE, SC_MANAGER_CONNECT,
    SC_STATUS_PROCESS_INFO, SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
    SERVICE_DELAYED_AUTO_START_INFO, SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS,
    SERVICE_STATUS_PROCESS,
};
use windows::Win32::System::SystemInformation::{
    GetNativeSystemInfo, GetSystemDirectoryW, GetSystemWindowsDirectoryW, GetSystemWow64DirectoryW,
    IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE_UNKNOWN, SYSTEM_INFO,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, IsWow64Process, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
};
use windows_core::{BOOL, GUID, HSTRING, PCWSTR, PWSTR, w};

// ---------------------------------------------------------------------------------------------
// Report output
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Level {
    Pass,
    Warn,
    Fail,
    Info,
    Skip,
}

fn emit(level: Level, msg: impl fmt::Display) {
    let tag = match level {
        Level::Pass => "PASS",
        Level::Warn => "WARN",
        Level::Fail => "FAIL",
        Level::Info => "INFO",
        Level::Skip => "SKIP",
    };
    println!("[{tag}] {msg}");
}

macro_rules! pass { ($($t:tt)*) => { emit(Level::Pass, format_args!($($t)*)) } }
macro_rules! warn { ($($t:tt)*) => { emit(Level::Warn, format_args!($($t)*)) } }
macro_rules! fail { ($($t:tt)*) => { emit(Level::Fail, format_args!($($t)*)) } }
macro_rules! info { ($($t:tt)*) => { emit(Level::Info, format_args!($($t)*)) } }
macro_rules! skip { ($($t:tt)*) => { emit(Level::Skip, format_args!($($t)*)) } }

fn heading(title: &str) {
    println!();
    println!("== {title} ==");
}

/// Win32 error with its code and system message; the two everyday cases get a plain prefix.
fn win32(e: WIN32_ERROR) -> String {
    let hr = e.to_hresult();
    let prefix = if e == ERROR_FILE_NOT_FOUND {
        "missing: "
    } else if e == ERROR_ACCESS_DENIED {
        "access denied: "
    } else {
        ""
    };
    format!(
        "{prefix}Win32 error {} (HRESULT 0x{:08X}): {}",
        e.0,
        hr.0 as u32,
        hr.message()
    )
}

fn hr(e: &windows_core::Error) -> String {
    let code = e.code();
    let prefix = if code == ERROR_FILE_NOT_FOUND.to_hresult() {
        "missing: "
    } else if code == ERROR_ACCESS_DENIED.to_hresult() {
        "access denied: "
    } else {
        ""
    };
    format!("{prefix}HRESULT 0x{:08X}: {}", code.0 as u32, e.message())
}

fn io_err(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => format!("missing: {e}"),
        io::ErrorKind::PermissionDenied => format!("access denied: {e}"),
        _ => e.to_string(),
    }
}

fn last_error() -> String {
    win32(unsafe { GetLastError() })
}

/// `YYYY-MM-DD hh:mm:ss UTC` from seconds since the Unix epoch.
fn utc(secs: i64) -> String {
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

fn fmt_time(t: SystemTime) -> String {
    let secs = match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    };
    let age = match SystemTime::now().duration_since(t) {
        Ok(d) => format!("{}s ago", d.as_secs()),
        Err(_) => "in the future".into(),
    };
    format!("{} ({age})", utc(secs))
}

fn filetime(ticks: u64) -> String {
    utc((ticks / 10_000_000) as i64 - 11_644_473_600)
}

// ---------------------------------------------------------------------------------------------
// Registry (read-only)
// ---------------------------------------------------------------------------------------------

/// Registry view: what 64-bit and 32-bit (WOW64) processes see.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Bit64,
    Bit32,
}

const VIEWS: [View; 2] = [View::Bit64, View::Bit32];

impl View {
    fn flag(self) -> REG_SAM_FLAGS {
        match self {
            View::Bit64 => KEY_WOW64_64KEY,
            View::Bit32 => KEY_WOW64_32KEY,
        }
    }

    fn label(self) -> &'static str {
        match self {
            View::Bit64 => "64-bit view",
            View::Bit32 => "32-bit view (WOW6432Node)",
        }
    }

    /// PE machine a DLL registered in this view must have (DeskCam ships x64 and x86 builds).
    fn machine(self) -> u16 {
        match self {
            View::Bit64 => 0x8664,
            View::Bit32 => 0x014c,
        }
    }
}

#[derive(Clone, Copy)]
struct Hive {
    root: HKEY,
    name: &'static str,
}

const HKLM: Hive = Hive {
    root: HKEY_LOCAL_MACHINE,
    name: "HKLM",
};
const HKCU: Hive = Hive {
    root: HKEY_CURRENT_USER,
    name: "HKCU",
};

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn key_label(hive: Hive, path: &str, view: View) -> String {
    format!("{}\\{path} [{}]", hive.name, view.label())
}

fn open_key(hive: Hive, path: &str, view: View) -> Result<Key, WIN32_ERROR> {
    let mut key = HKEY::default();
    let status = unsafe {
        RegOpenKeyExW(
            hive.root,
            &HSTRING::from(path),
            None,
            KEY_READ | view.flag(),
            &mut key,
        )
    };
    if status.is_ok() {
        Ok(Key(key))
    } else {
        Err(status)
    }
}

enum RegValue {
    /// `REG_SZ`, or `REG_EXPAND_SZ` (`true`) shown unexpanded.
    Str(String, bool),
    Dword(u32),
    Qword(u64),
    Binary(Vec<u8>),
    MultiStr(Vec<String>),
    Other(u32, usize),
}

impl RegValue {
    /// String content, with `%VAR%` expanded for `REG_EXPAND_SZ`.
    fn text(&self) -> Option<String> {
        match self {
            RegValue::Str(s, false) => Some(s.clone()),
            RegValue::Str(s, true) => Some(expand_env(s)),
            _ => None,
        }
    }

    fn dword(&self) -> Option<u32> {
        match self {
            RegValue::Dword(v) => Some(*v),
            _ => None,
        }
    }
}

impl fmt::Display for RegValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegValue::Str(s, false) => write!(f, "\"{s}\" (REG_SZ)"),
            RegValue::Str(s, true) => write!(
                f,
                "\"{s}\" (REG_EXPAND_SZ, expands to \"{}\")",
                expand_env(s)
            ),
            RegValue::Dword(v) => write!(f, "{v} (REG_DWORD 0x{v:08X})"),
            RegValue::Qword(v) => write!(f, "{v} (REG_QWORD)"),
            RegValue::Binary(b) => write!(f, "{} bytes (REG_BINARY)", b.len()),
            RegValue::MultiStr(v) => write!(f, "{v:?} (REG_MULTI_SZ)"),
            RegValue::Other(ty, len) => write!(f, "{len} bytes of registry type {ty}"),
        }
    }
}

const MAX_REG_VALUE: u32 = 1 << 20;

fn utf16z(bytes: &[u8]) -> String {
    let wide: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// Reads one value; `name = ""` is the key's default value.
fn query(key: &Key, name: &str) -> Result<RegValue, WIN32_ERROR> {
    let name = HSTRING::from(name);
    let mut ty = REG_VALUE_TYPE::default();
    let mut size = 0u32;
    let status =
        unsafe { RegQueryValueExW(key.0, &name, None, Some(&mut ty), None, Some(&mut size)) };
    if status.is_err() {
        return Err(status);
    }
    // The value can grow between calls; retry a few times, then report ERROR_MORE_DATA.
    let mut attempts = 0;
    let buf = loop {
        if size > MAX_REG_VALUE || attempts == 4 {
            return Err(ERROR_MORE_DATA);
        }
        attempts += 1;
        let mut buf = vec![0u8; size as usize + 2];
        let mut got = buf.len() as u32;
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                &name,
                None,
                Some(&mut ty),
                Some(buf.as_mut_ptr()),
                Some(&mut got),
            )
        };
        if status == ERROR_MORE_DATA {
            size = got;
            continue;
        }
        if status.is_err() {
            return Err(status);
        }
        buf.truncate(got as usize);
        break buf;
    };
    Ok(match ty {
        REG_SZ => RegValue::Str(utf16z(&buf), false),
        REG_EXPAND_SZ => RegValue::Str(utf16z(&buf), true),
        REG_DWORD if buf.len() >= 4 => {
            RegValue::Dword(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
        }
        REG_QWORD if buf.len() >= 8 => RegValue::Qword(u64::from_le_bytes([
            buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7],
        ])),
        REG_BINARY => RegValue::Binary(buf),
        REG_MULTI_SZ => {
            let wide: Vec<u16> = buf
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            RegValue::MultiStr(
                wide.split(|&c| c == 0)
                    .filter(|s| !s.is_empty())
                    .map(String::from_utf16_lossy)
                    .collect(),
            )
        }
        other => RegValue::Other(other.0, buf.len()),
    })
}

fn read_value(hive: Hive, path: &str, view: View, name: &str) -> Result<RegValue, WIN32_ERROR> {
    query(&open_key(hive, path, view)?, name)
}

/// Up to `limit` subkey names and the total number of subkeys.
fn subkeys(key: &Key, limit: usize) -> Result<(Vec<String>, usize), WIN32_ERROR> {
    let mut names = Vec::new();
    let mut index = 0u32;
    loop {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if status == ERROR_NO_MORE_ITEMS {
            return Ok((names, index as usize));
        }
        if status.is_err() {
            return Err(status);
        }
        if names.len() < limit {
            names.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        index += 1;
    }
}

/// Expands `%NAME%` from this process' environment; unknown names stay as written.
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => {
                let name = &after[..end];
                match std::env::var(name) {
                    Ok(v) if !name.is_empty() => out.push_str(&v),
                    _ => {
                        out.push('%');
                        out.push_str(name);
                        out.push('%');
                    }
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy()
        .trim_matches('"')
        .eq_ignore_ascii_case(b.to_string_lossy().trim_matches('"'))
}

// ---------------------------------------------------------------------------------------------
// Files (bounded reads, never executed)
// ---------------------------------------------------------------------------------------------

pub(crate) fn read_bounded(path: &Path, max: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    File::open(path)?.take(max).read_to_end(&mut out)?;
    Ok(out)
}

fn machine_name(m: u16) -> String {
    match m {
        0x014c => "x86 (I386)".into(),
        0x8664 => "x64 (AMD64)".into(),
        0xAA64 => "ARM64".into(),
        0x01c4 => "ARM (ARMNT)".into(),
        0 => "unknown".into(),
        other => format!("machine 0x{other:04X}"),
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..at.checked_add(2)?)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at.checked_add(4)?)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// `IMAGE_FILE_HEADER.Machine` from the start of a PE file.
fn pe_machine(head: &[u8]) -> Result<u16, String> {
    if !head.starts_with(b"MZ") {
        return Err("not a PE file (no MZ header)".into());
    }
    let pe = u32_at(head, 0x3c).ok_or("truncated DOS header")? as usize;
    if head.get(pe..pe.saturating_add(4)) != Some(&b"PE\0\0"[..]) {
        return Err(format!(
            "no PE signature at offset 0x{pe:X} within the first {} bytes",
            head.len()
        ));
    }
    u16_at(head, pe + 4).ok_or_else(|| "truncated COFF header".into())
}

/// Whether the PE image exports `wanted` by name, from the file bytes.
fn pe_exports(data: &[u8], wanted: &str) -> Result<bool, String> {
    pe_machine(data)?;
    let found = (|| -> Option<bool> {
        let pe = u32_at(data, 0x3c)? as usize;
        let sections_count = u16_at(data, pe + 6)? as usize;
        let opt_size = u16_at(data, pe + 20)? as usize;
        let opt = pe + 24;
        let (dirs_count, dirs) = match u16_at(data, opt)? {
            0x10b => (u32_at(data, opt + 92)?, opt + 96),
            0x20b => (u32_at(data, opt + 108)?, opt + 112),
            _ => return None,
        };
        if dirs_count == 0 {
            return Some(false);
        }
        let export_rva = u32_at(data, dirs)?;
        if export_rva == 0 {
            return Some(false);
        }
        let sections = opt + opt_size;
        let to_offset = |rva: u32| -> Option<usize> {
            for i in 0..sections_count {
                let s = sections + i * 40;
                let (vsize, va, raw_size, raw_ptr) = (
                    u32_at(data, s + 8)?,
                    u32_at(data, s + 12)?,
                    u32_at(data, s + 16)?,
                    u32_at(data, s + 20)?,
                );
                let span = u64::from(vsize.max(raw_size));
                if u64::from(rva) >= u64::from(va) && u64::from(rva) < u64::from(va) + span {
                    return Some((rva - va) as usize + raw_ptr as usize);
                }
            }
            None
        };
        let dir = to_offset(export_rva)?;
        let count = u32_at(data, dir + 24)? as usize;
        let names = to_offset(u32_at(data, dir + 32)?)?;
        for i in 0..count.min(1 << 16) {
            let at = to_offset(u32_at(data, names + i * 4)?)?;
            let name = data.get(at..)?;
            let end = name.iter().position(|&b| b == 0)?;
            if &name[..end] == wanted.as_bytes() {
                return Some(true);
            }
        }
        Some(false)
    })();
    found.ok_or_else(|| "malformed or truncated PE export table".into())
}

/// File version from the version resource (mapped as data by the system, no code runs).
fn file_version(path: &Path) -> Result<String, String> {
    let wide = HSTRING::from(path);
    let size = unsafe { GetFileVersionInfoSizeW(&wide, None) };
    if size == 0 {
        return Err(format!("no version resource ({})", last_error()));
    }
    if size > 4 * 1024 * 1024 {
        return Err(format!(
            "version resource is too large ({size} bytes; limit 4 MiB)"
        ));
    }
    let mut buf = vec![0u8; size as usize];
    unsafe { GetFileVersionInfoW(&wide, None, size, buf.as_mut_ptr().cast()) }
        .map_err(|e| format!("GetFileVersionInfoW: {}", hr(&e)))?;
    let mut ptr = std::ptr::null_mut();
    let mut len = 0u32;
    let ok = unsafe { VerQueryValueW(buf.as_ptr().cast(), w!("\\"), &mut ptr, &mut len) };
    if !ok.as_bool() || ptr.is_null() || (len as usize) < size_of::<VS_FIXEDFILEINFO>() {
        return Err("version resource has no VS_FIXEDFILEINFO".into());
    }
    let info = unsafe { std::ptr::read_unaligned(ptr as *const VS_FIXEDFILEINFO) };
    Ok(format!(
        "{}.{}.{}.{}",
        info.dwFileVersionMS >> 16,
        info.dwFileVersionMS & 0xFFFF,
        info.dwFileVersionLS >> 16,
        info.dwFileVersionLS & 0xFFFF
    ))
}

#[derive(Clone, Copy)]
enum Expect {
    /// Any PE file.
    Pe,
    /// A PE file for this machine.
    Machine(u16),
}

/// Reports one file; `missing` is the level used when it does not exist. Returns whether it exists.
fn report_file(label: &str, path: &Path, expect: Expect, missing: Level) -> bool {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            let level = if e.kind() == io::ErrorKind::NotFound {
                missing
            } else {
                Level::Fail
            };
            emit(
                level,
                format_args!("{label}: {}: {}", path.display(), io_err(&e)),
            );
            return false;
        }
    };
    if !meta.is_file() {
        fail!(
            "{label}: {} exists but is not a regular file",
            path.display()
        );
        return false;
    }
    let modified = meta
        .modified()
        .map(fmt_time)
        .unwrap_or_else(|e| format!("unknown ({e})"));
    let mut details = format!("{} bytes, modified {modified}", meta.len());
    let mut level = Level::Pass;
    match read_bounded(path, 4096)
        .map_err(|e| io_err(&e))
        .and_then(|head| pe_machine(&head))
    {
        Ok(machine) => {
            details.push_str(&format!(", {}", machine_name(machine)));
            if let Expect::Machine(want) = expect
                && machine != want
            {
                details.push_str(&format!(" but {} is required here", machine_name(want)));
                level = Level::Fail;
            }
        }
        Err(e) => {
            details.push_str(&format!(", PE header unreadable: {e}"));
            level = Level::Fail;
        }
    }
    match file_version(path) {
        Ok(v) => details.push_str(&format!(", file version {v}")),
        Err(e) => details.push_str(&format!(", file version unavailable: {e}")),
    }
    emit(
        level,
        format_args!("{label}: {} ({details})", path.display()),
    );
    true
}

// ---------------------------------------------------------------------------------------------
// Facts shared between sections
// ---------------------------------------------------------------------------------------------

struct Proc {
    exe: String,
    label: &'static str,
    pid: u32,
    session: Result<u32, String>,
    /// `Ok(true)` for a 32-bit process under WOW64.
    wow64: Result<bool, String>,
}

#[derive(Default)]
struct Facts {
    /// Real OS build (0 = unknown).
    build: u32,
    os_64: bool,
    /// This process runs under WOW64 (System32 is redirected for it).
    wow64: bool,
    /// Native System32 as reachable from this process.
    sys_native: Option<PathBuf>,
    sys_wow64: Option<PathBuf>,
    install_dir: Option<PathBuf>,
    /// `backend` from `config.ini`, lowercased; `None` = not set (auto).
    backend: Option<String>,
    camera_name: String,
    our_session: Option<u32>,
    procs: Vec<Proc>,
}

const DISCORD: [&str; 4] = [
    "discord.exe",
    "discordptb.exe",
    "discordcanary.exe",
    "discorddevelopment.exe",
];

impl Facts {
    /// Windows 10 uses DirectShow alongside its Frame Server camera; Windows 11 can force it.
    fn dshow_expected(&self) -> bool {
        self.build < os::WIN11_BUILD
            || matches!(self.backend.as_deref(), Some("dshow" | "directshow"))
    }

    fn mf_expected(&self) -> bool {
        self.build < os::WIN11_BUILD || !self.dshow_expected()
    }

    fn running<'a>(&'a self, names: &'a [&str]) -> impl Iterator<Item = &'a Proc> + 'a {
        self.procs
            .iter()
            .filter(move |p| names.iter().any(|n| p.exe.eq_ignore_ascii_case(n)))
    }

    fn discord_32bit(&self) -> bool {
        self.running(&DISCORD).any(|p| matches!(p.wow64, Ok(true)))
    }

    /// Maps a registered `...\System32\...` path to where this process can see the file for `view`.
    fn resolve(&self, path: &Path, view: View) -> PathBuf {
        let text = path.to_string_lossy().trim_matches('"').to_owned();
        let lower = text.to_ascii_lowercase();
        let system32 =
            std::env::var("SystemRoot").map(|r| format!("{}\\system32\\", r.to_ascii_lowercase()));
        if let Ok(prefix) = system32
            && lower.starts_with(&prefix)
        {
            let dir = match view {
                View::Bit64 => self.sys_native.as_ref(),
                View::Bit32 => self.sys_wow64.as_ref(),
            };
            if let Some(dir) = dir {
                return dir.join(&text[prefix.len()..]);
            }
        }
        PathBuf::from(text)
    }
}

// ---------------------------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------------------------

/// Full read-only inventory on stdout (everything except the app log; see [`logs`]).
pub fn run() {
    println!("DeskCam inventory (read-only: nothing is written, registered or started).");
    println!("This section lists file paths, the account name/SID and process image paths.");
    let mut f = Facts {
        camera_name: "DeskCam".into(),
        ..Default::default()
    };
    os_section(&mut f);
    token_section(&mut f);
    process_section(&mut f);
    shared_data_section(&mut f);
    installed_section(&mut f);
    com_section(&f);
    category_section(&f);
    dshow_core_section(&f);
    mf_section(&f);
    services_section(&f);
    system_files_section(&f);
    privacy_section();
    cross_check_section(&f);
}

/// The tail (at most 64 KiB) of `%LOCALAPPDATA%\DeskCam\deskcam.log` on stdout.
pub fn logs() {
    const MAX_TAIL: u64 = 64 * 1024;
    heading("DeskCam log");
    let base = match known_folder(&FOLDERID_LocalAppData) {
        Ok(p) => p,
        Err(e) => {
            fail!("SHGetKnownFolderPath(LocalAppData): {e}");
            match std::env::var("LOCALAPPDATA") {
                Ok(v) => {
                    warn!("using %LOCALAPPDATA% = {v} instead (the app falls back the same way)");
                    PathBuf::from(v)
                }
                Err(e) => {
                    fail!("%LOCALAPPDATA% is not set either ({e}); cannot locate deskcam.log");
                    return;
                }
            }
        }
    };
    let path = base.join("DeskCam").join("deskcam.log");
    info!(
        "{}: deskcam.exe truncates this file at every start, so it covers only its latest run under this account",
        path.display()
    );
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            warn!(
                "{}: {} (DeskCam may not have run under this account, or the log was removed)",
                path.display(),
                io_err(&e)
            );
            return;
        }
        Err(e) => {
            fail!("{}: {}", path.display(), io_err(&e));
            return;
        }
    };
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(e) => {
            fail!("{}: metadata: {}", path.display(), io_err(&e));
            return;
        }
    };
    let len = meta.len();
    let modified = meta
        .modified()
        .map(fmt_time)
        .unwrap_or_else(|e| format!("unknown ({e})"));
    info!("size {len} bytes, modified {modified}");
    let start = len.saturating_sub(MAX_TAIL);
    let mut tail = Vec::new();
    if let Err(e) = file
        .seek(SeekFrom::Start(start))
        .and_then(|_| file.by_ref().take(MAX_TAIL).read_to_end(&mut tail))
    {
        fail!("{}: read: {}", path.display(), io_err(&e));
        return;
    }
    let mut text = String::from_utf8_lossy(&tail).into_owned();
    if start > 0 {
        // The first line is cut mid-way; drop it so every shown line is complete.
        let cut = text.find('\n').map_or(text.len(), |i| i + 1);
        text.drain(..cut);
        warn!(
            "log TRUNCATED: showing the last {} of {len} bytes; the first {} bytes and the partial line after them are omitted",
            text.len(),
            start
        );
    } else {
        info!("log complete: all {len} bytes shown");
    }
    let lines: Vec<&str> = text.lines().collect();
    let flagged = lines
        .iter()
        .filter(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("error") || l.contains("fail") || l.contains("warning")
        })
        .count();
    info!(
        "{} lines shown, {flagged} mention error/fail/warning",
        lines.len()
    );
    for line in lines {
        println!("    log| {line}");
    }
}

// ---------------------------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------------------------

fn known_folder(id: &GUID) -> Result<PathBuf, String> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).map_err(|e| hr(&e))?;
        let s = p.to_string();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
            .map_err(|e| format!("path is not valid UTF-16: {e}"))
    }
}

fn system_dir(get: impl FnOnce(&mut [u16]) -> u32) -> Result<PathBuf, String> {
    let mut buf = [0u16; 520];
    let n = get(&mut buf) as usize;
    if n == 0 || n >= buf.len() {
        return Err(last_error());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buf[..n])))
}

/// Use the older architecture APIs so diagnostics can explain the minimum-build failure even
/// on Windows 10 releases predating IsWow64Process2 (1709). DeskCam itself still needs 1903.
fn process_architecture(
    process: HANDLE,
) -> windows_core::Result<(IMAGE_FILE_MACHINE, IMAGE_FILE_MACHINE)> {
    let mut info = SYSTEM_INFO::default();
    unsafe { GetNativeSystemInfo(&mut info) };
    let native = IMAGE_FILE_MACHINE(
        match unsafe { info.Anonymous.Anonymous.wProcessorArchitecture }.0 {
            0 => 0x014c,
            5 => 0x01c4,
            9 => 0x8664,
            12 => 0xaa64,
            _ => 0,
        },
    );
    let mut wow64 = BOOL::default();
    unsafe { IsWow64Process(process, &mut wow64)? };
    let emulated = if wow64.as_bool() {
        IMAGE_FILE_MACHINE(0x014c)
    } else {
        IMAGE_FILE_MACHINE_UNKNOWN
    };
    Ok((emulated, native))
}

fn os_section(f: &mut Facts) {
    heading("Windows version and architecture");
    let rtl = os::build();
    const CV: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let (mut edition, mut product, mut reg_build) = (None, None, None);
    match open_key(HKLM, CV, View::Bit64) {
        Err(e) => fail!("{}: {}", key_label(HKLM, CV, View::Bit64), win32(e)),
        Ok(key) => {
            for name in [
                "ProductName",
                "EditionID",
                "InstallationType",
                "DisplayVersion",
                "ReleaseId",
                "CurrentBuildNumber",
                "UBR",
                "CurrentMajorVersionNumber",
                "CurrentMinorVersionNumber",
            ] {
                match query(&key, name) {
                    Ok(v) => {
                        info!("HKLM\\{CV}\\{name} = {v}");
                        match name {
                            "ProductName" => product = v.text(),
                            "EditionID" => edition = v.text(),
                            "CurrentBuildNumber" => {
                                reg_build = v.text().and_then(|s| s.trim().parse::<u32>().ok())
                            }
                            _ => {}
                        }
                    }
                    Err(e) if e == ERROR_FILE_NOT_FOUND => {
                        info!("HKLM\\{CV}\\{name}: absent (not written by every Windows build)")
                    }
                    Err(e) => warn!("HKLM\\{CV}\\{name}: {}", win32(e)),
                }
            }
        }
    }

    f.build = if rtl != 0 {
        info!("RtlGetVersion build (real, not compatibility-shimmed): {rtl}");
        if let Some(reg) = reg_build
            && reg != rtl
        {
            warn!(
                "registry CurrentBuildNumber {reg} differs from RtlGetVersion {rtl}; RtlGetVersion is used"
            );
        }
        rtl
    } else {
        fail!("RtlGetVersion failed; the real build is unknown");
        match reg_build {
            Some(b) => {
                warn!("using registry CurrentBuildNumber {b} instead");
                b
            }
            None => {
                fail!(
                    "registry CurrentBuildNumber unavailable too; backend expectations below assume Windows 10"
                );
                0
            }
        }
    };
    let build = f.build;
    if build == 0 {
        // Already reported.
    } else if build < os::MIN_BUILD {
        fail!(
            "build {build} is older than Windows 10 1903 (build {}): DeskCam's screen capture needs {} or newer",
            os::MIN_BUILD,
            os::MIN_BUILD
        );
    } else if build < os::WIN11_BUILD {
        pass!(
            "Windows 10 build {build} (>= {}): supported. MFCreateVirtualCamera needs build {}+; backend = auto or dshow feeds DirectShow and the installed Frame Server software camera",
            os::MIN_BUILD,
            os::WIN11_BUILD
        );
    } else {
        pass!(
            "Windows 11 build {build} (>= {}): backend = auto uses the Media Foundation virtual camera",
            os::WIN11_BUILD
        );
    }
    if build != 0 && build < os::BORDERLESS_BUILD {
        info!(
            "build < {}: the yellow screen-capture border cannot be disabled (cosmetic)",
            os::BORDERLESS_BUILD
        );
    }
    if build >= os::WIN11_BUILD
        && let Some(p) = &product
        && p.contains("Windows 10")
    {
        info!(
            "ProductName says '{p}' on build {build}: Windows 11 keeps the Windows 10 ProductName; the build number is authoritative"
        );
    }
    match &edition {
        Some(e) if e.ends_with('N') => warn!(
            "EditionID '{e}' is an N edition: Media Foundation and Windows Media components are absent unless the Media Feature Pack is installed (see the system file checks)"
        ),
        Some(e) => info!("EditionID '{e}' is not an N edition"),
        None => warn!(
            "EditionID unknown; cannot tell whether this is an N edition without Media Foundation"
        ),
    }

    match process_architecture(unsafe { GetCurrentProcess() }) {
        Ok((process_machine, native_machine)) => {
            f.wow64 = process_machine != IMAGE_FILE_MACHINE_UNKNOWN;
            f.os_64 = matches!(native_machine.0, 0x8664 | 0xAA64);
            info!(
                "OS native machine: {}; this diagnostics process: {} build{}",
                machine_name(native_machine.0),
                std::env::consts::ARCH,
                if f.wow64 {
                    format!(
                        ", running under WOW64 as {}",
                        machine_name(process_machine.0)
                    )
                } else {
                    String::new()
                }
            );
            match native_machine.0 {
                0x8664 => pass!("64-bit x64 Windows"),
                0xAA64 => warn!(
                    "ARM64 Windows: DeskCam's x64 binaries run emulated, and ARM64-native apps cannot load the x64 or x86 DirectShow filter"
                ),
                _ => fail!("32-bit Windows: DeskCam needs 64-bit Windows"),
            }
        }
        Err(e) => {
            f.os_64 = cfg!(target_pointer_width = "64");
            fail!(
                "Process architecture query: {}; assuming {}-bit Windows from this build",
                hr(&e),
                if f.os_64 { 64 } else { 32 }
            );
        }
    }

    let windir = system_dir(|b| unsafe { GetSystemWindowsDirectoryW(Some(b)) });
    if let Err(e) = &windir {
        fail!("GetSystemWindowsDirectoryW: {e}");
    }
    f.sys_native = if f.wow64 {
        windir.ok().map(|w| w.join("Sysnative"))
    } else {
        system_dir(|b| unsafe { GetSystemDirectoryW(Some(b)) })
            .map_err(|e| fail!("GetSystemDirectoryW: {e}"))
            .ok()
    };
    f.sys_wow64 = if f.os_64 {
        system_dir(|b| unsafe { GetSystemWow64DirectoryW(Some(b)) })
            .map_err(|e| fail!("GetSystemWow64DirectoryW: {e}"))
            .ok()
    } else {
        None
    };
    if let Some(d) = &f.sys_native {
        info!(
            "native system directory (as seen by this process): {}",
            d.display()
        );
    }
    if let Some(d) = &f.sys_wow64 {
        info!("32-bit system directory: {}", d.display());
    }
}

fn token_info(token: HANDLE, class: TOKEN_INFORMATION_CLASS) -> Result<Vec<u64>, String> {
    let mut needed = 0u32;
    // The size probe fails with ERROR_INSUFFICIENT_BUFFER by design.
    let _ = unsafe { GetTokenInformation(token, class, None, 0, &mut needed) };
    if needed == 0 {
        return Err(last_error());
    }
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // TokenElevation requires its exact structure size, not the aligned allocation's padding.
    unsafe {
        GetTokenInformation(
            token,
            class,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .map_err(|e| hr(&e))?;
    Ok(buf)
}

fn sid_text(sid: PSID) -> String {
    let string = unsafe {
        let mut p = PWSTR::null();
        match ConvertSidToStringSidW(sid, &mut p) {
            Ok(()) => {
                let s = p.to_string().unwrap_or_default();
                LocalFree(Some(HLOCAL(p.0.cast())));
                s
            }
            Err(e) => format!("<ConvertSidToStringSidW: {}>", hr(&e)),
        }
    };
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
    let mut usage = SID_NAME_USE::default();
    let account = unsafe {
        LookupAccountSidW(
            PCWSTR::null(),
            sid,
            Some(PWSTR(name.as_mut_ptr())),
            &mut name_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut usage,
        )
    }
    .map(|()| {
        format!(
            "{}\\{}",
            String::from_utf16_lossy(&domain[..domain_len as usize]),
            String::from_utf16_lossy(&name[..name_len as usize])
        )
    })
    .unwrap_or_else(|e| format!("<LookupAccountSidW: {}>", hr(&e)));
    format!("{account} ({string})")
}

fn token_section(f: &mut Facts) {
    heading("This process and account");
    let pid = unsafe { GetCurrentProcessId() };
    let mut session = 0u32;
    match unsafe { ProcessIdToSessionId(pid, &mut session) } {
        Ok(()) => f.our_session = Some(session),
        Err(e) => fail!("ProcessIdToSessionId({pid}): {}", hr(&e)),
    }
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    let console_text = if console == u32::MAX {
        "none attached".to_owned()
    } else {
        console.to_string()
    };
    info!(
        "diagnostics PID {pid}, session {}, active console session {console_text}",
        f.our_session.map_or("unknown".into(), |s| s.to_string())
    );
    if let Some(s) = f.our_session
        && s != console
    {
        info!(
            "this is not the console session (e.g. Remote Desktop): the Windows 10 frame section is per session"
        );
    }

    let mut token = HANDLE::default();
    if let Err(e) = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } {
        fail!("OpenProcessToken: {}", hr(&e));
        return;
    }
    match token_info(token, TokenUser) {
        Ok(buf) => {
            let user = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
            info!("account: {}", sid_text(user.User.Sid));
        }
        Err(e) => fail!("GetTokenInformation(TokenUser): {e}"),
    }
    let elevated = match token_info(token, TokenElevation) {
        Ok(buf) => {
            let e = unsafe { &*(buf.as_ptr() as *const TOKEN_ELEVATION) };
            Some(e.TokenIsElevated != 0)
        }
        Err(e) => {
            fail!("GetTokenInformation(TokenElevation): {e}");
            None
        }
    };
    match token_info(token, TokenElevationType) {
        Ok(buf) => {
            let t = unsafe { *(buf.as_ptr() as *const TOKEN_ELEVATION_TYPE) };
            let name = match t.0 {
                1 => "default (no split token: UAC off, standard user or built-in Administrator)",
                2 => "full (elevated)",
                3 => "limited (UAC-filtered)",
                _ => "unknown",
            };
            info!("elevation type {}: {name}", t.0);
        }
        Err(e) => fail!("GetTokenInformation(TokenElevationType): {e}"),
    }
    match token_info(token, TokenIntegrityLevel) {
        Ok(buf) => {
            let label = unsafe { &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
            let rid = unsafe {
                let count = *GetSidSubAuthorityCount(label.Label.Sid);
                if count == 0 {
                    None
                } else {
                    Some(*GetSidSubAuthority(label.Label.Sid, u32::from(count) - 1))
                }
            };
            let name = match rid {
                Some(0x0000) => "untrusted",
                Some(0x1000) => "low",
                Some(0x2000) => "medium",
                Some(0x2100) => "medium plus",
                Some(0x3000) => "high",
                Some(0x4000) => "system",
                _ => "other",
            };
            info!("integrity level: {name} (RID 0x{:X})", rid.unwrap_or(0));
        }
        Err(e) => fail!("GetTokenInformation(TokenIntegrityLevel): {e}"),
    }
    unsafe {
        let _ = CloseHandle(token);
    }
    match elevated {
        Some(true) => warn!(
            "running elevated: HKCU, %LOCALAPPDATA% and per-user results describe the account above. If UAC asked for a different administrator's password, they do not describe the signed-in user; elevated processes also ignore per-user COM registrations. Prefer running unelevated."
        ),
        Some(false) => pass!("running unelevated, like deskcam.exe and camera apps normally do"),
        None => {}
    }
}

fn process_section(f: &mut Facts) {
    heading("Related running processes (names, PIDs, sessions; no command lines)");
    const WATCH: &[(&str, &str)] = &[
        ("deskcam.exe", "DeskCam app"),
        ("discord.exe", "Discord"),
        ("discordptb.exe", "Discord PTB"),
        ("discordcanary.exe", "Discord Canary"),
        ("discorddevelopment.exe", "Discord Development"),
        ("amdprivacyview.exe", "AMD Privacy View"),
        ("amdprivacyviewservice.exe", "AMD Privacy View service"),
        ("amd privacy view.exe", "AMD Privacy View"),
        ("radeonsoftware.exe", "AMD Software"),
        ("amdrsserv.exe", "AMD Radeon Settings service"),
        ("amdrssrcext.exe", "AMD ReLive/recording extension"),
        ("amdow.exe", "AMD overlay"),
        ("atieclxx.exe", "AMD External Events client"),
        ("atiesrxx.exe", "AMD External Events service"),
        ("amdfendrsr.exe", "AMD crash defender"),
    ];
    const MAX_SHOWN: usize = 64;
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(h) => h,
        Err(e) => {
            fail!("CreateToolhelp32Snapshot: {}", hr(&e));
            return;
        }
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut next = unsafe { Process32FirstW(snapshot, &mut entry) };
    let (mut total, mut extra) = (0usize, 0usize);
    while next.is_ok() {
        total += 1;
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        let exe = String::from_utf16_lossy(&entry.szExeFile[..len]);
        if let Some(&(_, label)) = WATCH.iter().find(|(n, _)| exe.eq_ignore_ascii_case(n)) {
            if f.procs.len() >= MAX_SHOWN {
                extra += 1;
            } else {
                f.procs.push(inspect_process(
                    exe,
                    label,
                    entry.th32ProcessID,
                    entry.th32ParentProcessID,
                ));
            }
        }
        next = unsafe { Process32NextW(snapshot, &mut entry) };
    }
    if let Err(e) = next
        && e.code() != ERROR_NO_MORE_FILES_HR
    {
        warn!("Process32NextW stopped early: {}", hr(&e));
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    info!(
        "{total} processes in the snapshot; {} related ones listed{}",
        f.procs.len(),
        if extra > 0 {
            format!(", {extra} more not listed")
        } else {
            String::new()
        }
    );

    let deskcam: Vec<&Proc> = f.running(&["deskcam.exe"]).collect();
    match deskcam.len() {
        0 => warn!(
            "deskcam.exe is not running: Windows 10 camera apps get black frames, and the Windows 11 session camera does not exist"
        ),
        1 => pass!("deskcam.exe is running (PID {})", deskcam[0].pid),
        n => warn!("{n} deskcam.exe processes are running; normally there is one per user session"),
    }
    if f.running(&DISCORD).next().is_none() {
        info!("Discord is not running");
    }
    let amd = f
        .procs
        .iter()
        .filter(|p| p.label.starts_with("AMD"))
        .count();
    if amd > 0 {
        info!(
            "{amd} AMD software processes are running; listed for correlation only, their presence is not evidence of a fault"
        );
    }
}

/// `ERROR_NO_MORE_FILES` as returned by the Toolhelp enumerators.
const ERROR_NO_MORE_FILES_HR: windows_core::HRESULT = windows_core::HRESULT::from_win32(18);

fn inspect_process(exe: String, label: &'static str, pid: u32, ppid: u32) -> Proc {
    let mut session = 0u32;
    let session = unsafe { ProcessIdToSessionId(pid, &mut session) }
        .map(|()| session)
        .map_err(|e| hr(&e));
    let (wow64, image) = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
    {
        Err(e) => (
            Err(format!("OpenProcess: {}", hr(&e))),
            Err(format!("OpenProcess: {}", hr(&e))),
        ),
        Ok(h) => {
            let wow64 = process_architecture(h)
                .map(|(machine, _)| machine != IMAGE_FILE_MACHINE_UNKNOWN)
                .map_err(|e| format!("Process architecture query: {}", hr(&e)));
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let image = unsafe {
                QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len)
            }
            .map(|()| String::from_utf16_lossy(&buf[..len as usize]))
            .map_err(|e| format!("QueryFullProcessImageNameW: {}", hr(&e)));
            unsafe {
                let _ = CloseHandle(h);
            }
            (wow64, image)
        }
    };
    let session_text = match &session {
        Ok(s) => s.to_string(),
        Err(e) => format!("unknown ({e})"),
    };
    let arch = match &wow64 {
        Ok(true) => "32-bit (WOW64)".to_owned(),
        Ok(false) => "native 64-bit".to_owned(),
        Err(e) => format!("bitness unknown ({e})"),
    };
    let image_text = match &image {
        Ok(p) => p.clone(),
        Err(e) => format!("image path unknown ({e})"),
    };
    info!("{label}: {exe} PID {pid} (parent {ppid}), session {session_text}, {arch}, {image_text}");
    Proc {
        exe,
        label,
        pid,
        session,
        wow64,
    }
}

fn shared_data_section(f: &mut Facts) {
    const MAX_CONFIG: u64 = 16 * 1024;
    heading("Shared data (config.ini, stream.bin)");
    let dir = paths::data_dir();
    match fs::metadata(&dir) {
        Ok(m) if m.is_dir() => pass!("data directory {} exists", dir.display()),
        Ok(_) => fail!("{} exists but is not a directory", dir.display()),
        Err(e) => warn!(
            "data directory {}: {} (created by the installer or the first app start)",
            dir.display(),
            io_err(&e)
        ),
    }

    let config = paths::config_file();
    match File::open(&config) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            warn!(
                "{}: {} (deskcam.exe writes defaults on first start: backend = auto, name = DeskCam)",
                config.display(),
                io_err(&e)
            )
        }
        Err(e) => fail!(
            "{}: {} (deskcam.exe cannot start without reading it)",
            config.display(),
            io_err(&e)
        ),
        Ok(file) => {
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            let mut bytes = Vec::new();
            match file.take(MAX_CONFIG).read_to_end(&mut bytes) {
                Err(e) => fail!("{}: read: {}", config.display(), io_err(&e)),
                Ok(_) => {
                    info!(
                        "{}: {len} bytes; deskcam.exe validates every key at startup and logs errors to deskcam.log",
                        config.display()
                    );
                    if len > MAX_CONFIG {
                        warn!(
                            "config.ini TRUNCATED: showing the first {MAX_CONFIG} of {len} bytes"
                        );
                    }
                    let text = match std::str::from_utf8(&bytes) {
                        Ok(t) => t.to_owned(),
                        Err(e) => {
                            fail!(
                                "config.ini is not valid UTF-8 ({e}); deskcam.exe refuses to read it"
                            );
                            String::from_utf8_lossy(&bytes).into_owned()
                        }
                    };
                    for line in text.lines() {
                        println!("    config| {line}");
                    }
                    let (backend, name) = camera_keys(&text);
                    f.backend = backend.map(|b| b.to_ascii_lowercase());
                    if let Some(n) = name {
                        f.camera_name = n;
                    }
                }
            }
        }
    }
    let build = f.build;
    let resolved = if build < os::WIN11_BUILD {
        "DirectShow plus a Frame Server software camera"
    } else if f.dshow_expected() {
        "DirectShow"
    } else {
        "Media Foundation"
    };
    match f.backend.as_deref() {
        None => pass!("backend not set (auto): resolves to {resolved} on build {build}"),
        Some("auto") => pass!("backend = auto: resolves to {resolved} on build {build}"),
        Some(b @ ("mf" | "mediafoundation")) if build < os::WIN11_BUILD => fail!(
            "backend = {b} on Windows 10 build {build}: MFCreateVirtualCamera does not exist, so DeskCam cannot start its camera. Set backend = auto or dshow"
        ),
        Some(b @ ("mf" | "mediafoundation")) => {
            pass!("backend = {b}: Media Foundation on build {build}")
        }
        Some(b @ ("dshow" | "directshow")) if build >= os::WIN11_BUILD => warn!(
            "backend = {b} forces the DirectShow filter on Windows 11: no Media Foundation camera is created, so only apps that enumerate DirectShow devices list DeskCam"
        ),
        Some(b @ ("dshow" | "directshow")) => pass!(
            "backend = {b}: DirectShow plus a Frame Server software camera on build {build}"
        ),
        Some(b) => {
            fail!("backend = '{b}' is not auto, mf or dshow; deskcam.exe rejects this config.ini")
        }
    }
    info!("camera name used for comparisons: '{}'", f.camera_name);

    let stream = paths::stream_file();
    match File::open(&stream) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => warn!(
            "{}: {} (deskcam.exe writes it at startup; until then readers use the default {}x{}@{})",
            stream.display(),
            io_err(&e),
            DEFAULT_STREAM.width,
            DEFAULT_STREAM.height,
            DEFAULT_STREAM.fps
        ),
        Err(e) => fail!("{}: {}", stream.display(), io_err(&e)),
        Ok(file) => {
            let meta = file.metadata();
            let mut bytes = Vec::new();
            match file.take(64).read_to_end(&mut bytes) {
                Err(e) => fail!("{}: read: {}", stream.display(), io_err(&e)),
                Ok(_) => {
                    let modified = meta
                        .as_ref()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .map_or("unknown".into(), fmt_time);
                    match StreamInfo::from_bytes(&bytes) {
                        Some(s) => pass!(
                            "{}: valid, {}x{} at {} fps, modified {modified}",
                            stream.display(),
                            s.width,
                            s.height,
                            s.fps
                        ),
                        None => fail!(
                            "{}: invalid record ({} bytes read, modified {modified}); readers fall back to the default {}x{}@{}",
                            stream.display(),
                            bytes.len(),
                            DEFAULT_STREAM.width,
                            DEFAULT_STREAM.height,
                            DEFAULT_STREAM.fps
                        ),
                    }
                }
            }
        }
    }
    let tmp = stream.with_extension("bin.tmp");
    if tmp.exists() {
        info!(
            "{} exists: leftover from an interrupted write (harmless)",
            tmp.display()
        );
    }
}

/// `backend` and `name` from the `[camera]` section (last occurrence wins, like the app).
pub(crate) fn camera_keys(text: &str) -> (Option<String>, Option<String>) {
    let (mut backend, mut name) = (None, None);
    let mut in_camera = false;
    for raw in text.trim_start_matches('\u{feff}').lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_camera = section.trim().eq_ignore_ascii_case("camera");
            continue;
        }
        if !in_camera {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            match key.trim().to_ascii_lowercase().as_str() {
                "backend" => backend = Some(value.trim().to_owned()),
                "name" => name = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }
    (backend, name)
}

fn installed_section(f: &mut Facts) {
    heading("Installed files");
    let program_files = match std::env::var("ProgramW6432") {
        Ok(p) => {
            info!("Program Files (64-bit) from %ProgramW6432%: {p}");
            Some(PathBuf::from(p))
        }
        Err(e) => {
            info!("%ProgramW6432% unavailable ({e}); using FOLDERID_ProgramFiles");
            known_folder(&FOLDERID_ProgramFiles)
                .map_err(|e| fail!("SHGetKnownFolderPath(ProgramFiles): {e}"))
                .ok()
        }
    };
    let Some(program_files) = program_files else {
        return;
    };
    let dir = program_files.join("DeskCam");
    f.install_dir = Some(dir.clone());
    match fs::metadata(&dir) {
        Ok(m) if m.is_dir() => pass!("install directory {} exists", dir.display()),
        Ok(_) => fail!("{} exists but is not a directory", dir.display()),
        Err(e) => fail!(
            "install directory {}: {} (DeskCam is not installed with the installer or install.ps1)",
            dir.display(),
            io_err(&e)
        ),
    }
    let (dshow, mf) = (f.dshow_expected(), f.mf_expected());
    let need = |expected: bool| if expected { Level::Fail } else { Level::Info };
    report_file(
        "app",
        &dir.join("deskcam.exe"),
        Expect::Machine(0x8664),
        Level::Fail,
    );
    report_file(
        "DirectShow filter x64",
        &dir.join("deskcam_dshow.dll"),
        Expect::Machine(0x8664),
        need(dshow),
    );
    let x86_missing = if dshow && f.discord_32bit() {
        Level::Fail
    } else if dshow {
        Level::Warn
    } else {
        Level::Info
    };
    report_file(
        "DirectShow filter x86",
        &dir.join("x86").join("deskcam_dshow.dll"),
        Expect::Machine(0x014c),
        x86_missing,
    );
    report_file(
        "Media Foundation source",
        &dir.join("deskcam_source.dll"),
        Expect::Machine(0x8664),
        need(mf),
    );
    report_file(
        "Inno Setup uninstaller",
        &dir.join("unins000.exe"),
        Expect::Pe,
        Level::Info,
    );

    for sub in [dir.clone(), dir.join("x86")] {
        if let Ok(entries) = fs::read_dir(&sub) {
            for entry in entries.flatten().take(256) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.contains(".old-") {
                    info!(
                        "leftover renamed DLL {} (still loaded by an app during a reinstall; harmless)",
                        entry.path().display()
                    );
                }
            }
        }
    }

    const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{6E095512-BE9A-4490-8A21-CF843DFCC74C}_is1";
    let mut found = false;
    for view in VIEWS {
        match open_key(HKLM, UNINSTALL, view) {
            Ok(key) => {
                found = true;
                for name in ["DisplayVersion", "InstallLocation", "InstallDate"] {
                    match query(&key, name) {
                        Ok(v) => info!("{}\\{name} = {v}", key_label(HKLM, UNINSTALL, view)),
                        Err(e) => {
                            info!("{}\\{name}: {}", key_label(HKLM, UNINSTALL, view), win32(e))
                        }
                    }
                }
            }
            Err(e) if e == ERROR_FILE_NOT_FOUND => {}
            Err(e) => warn!("{}: {}", key_label(HKLM, UNINSTALL, view), win32(e)),
        }
    }
    if !found {
        info!(
            "no Inno Setup uninstall entry: not installed with DeskCam-Setup (scripts\\install.ps1 leaves none)"
        );
    }

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    match read_value(HKCU, RUN, View::Bit64, "DeskCam") {
        Ok(v) => {
            info!("autostart HKCU\\{RUN}\\DeskCam = {v}");
            if let Some(cmd) = v.text() {
                let exe = PathBuf::from(cmd.trim().trim_matches('"'));
                if !exe.is_file() {
                    warn!("autostart target {} does not exist", exe.display());
                }
            }
        }
        Err(e) if e == ERROR_FILE_NOT_FOUND => {
            info!("no autostart entry HKCU\\{RUN}\\DeskCam (deskcam.exe must be started by hand)")
        }
        Err(e) => warn!("HKCU\\{RUN}\\DeskCam: {}", win32(e)),
    }
}

/// One `CLSID\{clsid}` registration in one hive and view. Returns whether the class key exists.
fn report_inproc(
    f: &Facts,
    hive: Hive,
    view: View,
    clsid: &str,
    label: &str,
    expected_dll: Option<&Path>,
    missing: Level,
) -> bool {
    let path = format!("Software\\Classes\\CLSID\\{clsid}");
    let where_ = key_label(hive, &path, view);
    let key = match open_key(hive, &path, view) {
        Ok(k) => k,
        Err(e) => {
            let level = if e == ERROR_FILE_NOT_FOUND {
                missing
            } else {
                Level::Warn
            };
            emit(level, format_args!("{label}: {where_}: {}", win32(e)));
            return false;
        }
    };
    match query(&key, "") {
        Ok(v) => info!("{label}: {where_} (default) = {v}"),
        Err(e) => info!("{label}: {where_} (default): {}", win32(e)),
    }
    let server_path = format!("{path}\\InprocServer32");
    let server = match open_key(hive, &server_path, view) {
        Ok(k) => k,
        Err(e) => {
            fail!(
                "{label}: {}: {} (class key without a server: CoCreateInstance fails)",
                key_label(hive, &server_path, view),
                win32(e)
            );
            return true;
        }
    };
    match query(&server, "ThreadingModel") {
        Ok(v) if v.text().is_some_and(|t| t.eq_ignore_ascii_case("Both")) => {
            pass!("{label}: ThreadingModel = {v}")
        }
        Ok(v) => warn!("{label}: ThreadingModel = {v}; DeskCam registers 'Both'"),
        Err(e) => warn!(
            "{label}: ThreadingModel: {} (DeskCam registers 'Both'; without it COM loads the DLL single-threaded)",
            win32(e)
        ),
    }
    match query(&server, "") {
        Err(e) => fail!(
            "{label}: {} (default): {}",
            key_label(hive, &server_path, view),
            win32(e)
        ),
        Ok(v) => match v.text().filter(|t| !t.trim().is_empty()) {
            None => fail!("{label}: InprocServer32 (default) = {v}: not a DLL path"),
            Some(dll) => {
                let dll = PathBuf::from(dll.trim().trim_matches('"'));
                info!("{label}: InprocServer32 = {v}");
                if let Some(expected) = expected_dll {
                    if same_path(&dll, expected) {
                        pass!("{label}: registered DLL is the installed one");
                    } else {
                        warn!(
                            "{label}: registered DLL {} is not the installed location {}",
                            dll.display(),
                            expected.display()
                        );
                    }
                }
                report_file(
                    &format!("{label} DLL"),
                    &f.resolve(&dll, view),
                    Expect::Machine(view.machine()),
                    Level::Fail,
                );
            }
        },
    }
    true
}

fn com_section(f: &Facts) {
    heading("COM registrations (machine and per-user, both registry views)");
    let dir = f.install_dir.clone().unwrap_or_default();
    let (dshow, mf) = (f.dshow_expected(), f.mf_expected());
    let x86_level = if !dshow {
        Level::Info
    } else if f.discord_32bit() {
        Level::Fail
    } else {
        Level::Warn
    };
    if dshow && f.discord_32bit() {
        info!(
            "a 32-bit Discord process is running: it reads only the 32-bit view and needs the x86 filter"
        );
    }
    for view in VIEWS {
        if view == View::Bit32 && !f.os_64 {
            skip!("32-bit view: this is not 64-bit Windows");
            continue;
        }
        let (expected, missing) = match view {
            View::Bit64 => (
                dir.join("deskcam_dshow.dll"),
                if dshow { Level::Fail } else { Level::Info },
            ),
            View::Bit32 => (dir.join("x86").join("deskcam_dshow.dll"), x86_level),
        };
        let label = format!("DirectShow filter {DSHOW_CLSID_STR} [{}]", view.label());
        let present = report_inproc(
            f,
            HKLM,
            view,
            DSHOW_CLSID_STR,
            &label,
            Some(expected.as_path()),
            missing,
        );
        if present && !dshow {
            warn!(
                "{label}: registered although this setup uses Media Foundation; apps may list DeskCam twice, once without frames"
            );
        }
    }
    let label = format!(
        "Media Foundation source {CLSID_STR} [{}]",
        View::Bit64.label()
    );
    let present = report_inproc(
        f,
        HKLM,
        View::Bit64,
        CLSID_STR,
        &label,
        Some(dir.join("deskcam_source.dll").as_path()),
        if mf { Level::Fail } else { Level::Info },
    );
    if present && !mf {
        info!(
            "{label}: registered but unused because Windows 11 is forced to backend = dshow"
        );
    }
    if f.os_64 {
        let path = format!("Software\\Classes\\CLSID\\{CLSID_STR}");
        match open_key(HKLM, &path, View::Bit32) {
            Ok(_) => info!(
                "{}: present; the Frame Server is 64-bit and never reads it",
                key_label(HKLM, &path, View::Bit32)
            ),
            Err(e) if e == ERROR_FILE_NOT_FOUND => {}
            Err(e) => warn!("{}: {}", key_label(HKLM, &path, View::Bit32), win32(e)),
        }
    }

    info!(
        "per-user (HKCU) class registrations override HKLM for unelevated processes (the merged HKCR view)"
    );
    for view in VIEWS {
        if view == View::Bit32 && !f.os_64 {
            continue;
        }
        for (clsid, name) in [
            (DSHOW_CLSID_STR, "DirectShow filter"),
            (CLSID_STR, "Media Foundation source"),
        ] {
            let path = format!("Software\\Classes\\CLSID\\{clsid}");
            match open_key(HKCU, &path, view) {
                Err(e) if e == ERROR_FILE_NOT_FOUND => {
                    pass!(
                        "{name}: no per-user override at {}",
                        key_label(HKCU, &path, view)
                    )
                }
                Err(e) => warn!("{name}: {}: {}", key_label(HKCU, &path, view), win32(e)),
                Ok(_) => {
                    warn!(
                        "{name}: per-user override exists at {}; it shadows the machine registration",
                        key_label(HKCU, &path, view)
                    );
                    report_inproc(
                        f,
                        HKCU,
                        view,
                        clsid,
                        &format!("{name} (per-user)"),
                        None,
                        Level::Warn,
                    );
                }
            }
        }
    }
}

const VIDEO_INPUT_CATEGORY: &str = "{860BB310-5D01-11D0-BD3B-00A0C911CE86}";

fn category_section(f: &Facts) {
    heading("DirectShow video input device category");
    info!(
        "this category lists software (registry) DirectShow capture filters only; hardware webcams are enumerated through their drivers and never appear here"
    );
    let instances = format!("Software\\Classes\\CLSID\\{VIDEO_INPUT_CATEGORY}\\Instance");
    let dshow = f.dshow_expected();
    for hive in [HKLM, HKCU] {
        for view in VIEWS {
            if view == View::Bit32 && !f.os_64 {
                continue;
            }
            let where_ = key_label(hive, &instances, view);
            let key = match open_key(hive, &instances, view) {
                Ok(k) => k,
                Err(e) if e == ERROR_FILE_NOT_FOUND && hive.name == "HKCU" => {
                    pass!("{where_}: absent (no per-user camera entries)");
                    continue;
                }
                Err(e) => {
                    let level = if hive.name == "HKLM" && dshow {
                        Level::Fail
                    } else {
                        Level::Warn
                    };
                    emit(level, format_args!("{where_}: {}", win32(e)));
                    continue;
                }
            };
            let (names, total) = match subkeys(&key, 64) {
                Ok(v) => v,
                Err(e) => {
                    fail!("{where_}: RegEnumKeyExW: {}", win32(e));
                    continue;
                }
            };
            info!(
                "{where_}: {total} entries{}",
                if total > names.len() {
                    format!(" ({} listed)", names.len())
                } else {
                    String::new()
                }
            );
            let mut ours = false;
            for sub in &names {
                let entry = format!("{instances}\\{sub}");
                let friendly = read_value(hive, &entry, view, "FriendlyName")
                    .ok()
                    .and_then(|v| v.text());
                let clsid = read_value(hive, &entry, view, "CLSID")
                    .ok()
                    .and_then(|v| v.text());
                let is_ours = sub.eq_ignore_ascii_case(DSHOW_CLSID_STR);
                info!(
                    "  entry {sub}: FriendlyName {:?}, CLSID {:?}{}",
                    friendly.as_deref().unwrap_or("<missing>"),
                    clsid.as_deref().unwrap_or("<missing>"),
                    if is_ours { " (DeskCam)" } else { "" }
                );
                if is_ours {
                    ours = true;
                    report_deskcam_instance(f, hive, view, &entry);
                } else if friendly
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(&f.camera_name))
                {
                    warn!(
                        "  another filter is also named '{}' (CLSID {:?}); apps show two cameras with that name",
                        f.camera_name, clsid
                    );
                }
            }
            if !ours {
                // Not among the listed entries: look it up directly (the listing is capped).
                let entry = format!("{instances}\\{DSHOW_CLSID_STR}");
                match open_key(hive, &entry, view) {
                    Ok(_) => {
                        info!("  entry {DSHOW_CLSID_STR} (DeskCam, beyond the listed entries)");
                        report_deskcam_instance(f, hive, view, &entry);
                    }
                    Err(e) => {
                        let level = match (hive.name, view) {
                            ("HKLM", View::Bit64) if dshow => Level::Fail,
                            ("HKLM", View::Bit32) if dshow && f.discord_32bit() => Level::Fail,
                            ("HKLM", View::Bit32) if dshow => Level::Warn,
                            _ => Level::Info,
                        };
                        let bits = match view {
                            View::Bit64 => "64-bit",
                            View::Bit32 => "32-bit",
                        };
                        emit(
                            level,
                            format_args!(
                                "{where_}\\{DSHOW_CLSID_STR}: {}; {bits} apps do not list DeskCam from here",
                                win32(e)
                            ),
                        );
                    }
                }
            }
        }
    }
}

fn report_deskcam_instance(f: &Facts, hive: Hive, view: View, entry: &str) {
    let where_ = key_label(hive, entry, view);
    match read_value(hive, entry, view, "FriendlyName") {
        Ok(v) => match v.text() {
            Some(n) if n == f.camera_name => pass!("  FriendlyName = {v} matches config.ini"),
            Some(n) => warn!(
                "  FriendlyName '{n}' differs from config.ini name '{}'; re-run the installer to apply it",
                f.camera_name
            ),
            None => fail!("  FriendlyName = {v} is not a string"),
        },
        Err(e) => fail!(
            "  {where_}\\FriendlyName: {} (devenum needs it to list the camera)",
            win32(e)
        ),
    }
    match read_value(hive, entry, view, "CLSID") {
        Ok(v)
            if v.text()
                .is_some_and(|c| c.eq_ignore_ascii_case(DSHOW_CLSID_STR)) =>
        {
            pass!("  CLSID = {v}")
        }
        Ok(v) => fail!("  CLSID = {v}, expected {DSHOW_CLSID_STR}"),
        Err(e) => fail!("  {where_}\\CLSID: {}", win32(e)),
    }
    match read_value(hive, entry, view, "FilterData") {
        Ok(RegValue::Binary(b)) if b.len() >= 8 => {
            let version = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            let merit = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
            pass!(
                "  FilterData present: {} bytes, format version {version}, merit 0x{merit:08X}",
                b.len()
            );
        }
        Ok(v) => warn!("  FilterData = {v}: unexpected content"),
        Err(e) => warn!("  {where_}\\FilterData: {}", win32(e)),
    }
}

fn dshow_core_section(f: &Facts) {
    heading("DirectShow core classes");
    const CORE: [(&str, &str, &str); 3] = [
        (
            "{62BE5D10-60EB-11D0-BD3B-00A0C911CE86}",
            "System Device Enumerator",
            "devenum.dll",
        ),
        (
            "{CDA42200-BD88-11D0-BD4E-00A0C911CE86}",
            "Filter Mapper 2",
            "quartz.dll",
        ),
        (
            "{E436EBB3-524F-11CE-9F53-0020AF0BA770}",
            "Filter Graph",
            "quartz.dll",
        ),
    ];
    for view in VIEWS {
        if view == View::Bit32 && !f.os_64 {
            continue;
        }
        for (clsid, name, dll) in CORE {
            let path = format!("Software\\Classes\\CLSID\\{clsid}\\InprocServer32");
            let where_ = key_label(HKLM, &path, view);
            match read_value(HKLM, &path, view, "") {
                Err(e) => fail!(
                    "{name}: {where_}: {} (DirectShow device enumeration breaks)",
                    win32(e)
                ),
                Ok(v) => match v.text() {
                    Some(t)
                        if Path::new(t.trim())
                            .file_name()
                            .is_some_and(|n| n.eq_ignore_ascii_case(dll)) =>
                    {
                        let file = f.resolve(Path::new(t.trim()), view);
                        if file.is_file() {
                            pass!(
                                "{name} [{}]: {v} -> {} exists",
                                view.label(),
                                file.display()
                            );
                        } else {
                            fail!(
                                "{name} [{}]: {v} -> {} does not exist",
                                view.label(),
                                file.display()
                            );
                        }
                    }
                    _ => fail!(
                        "{name} [{}]: InprocServer32 = {v}, expected {dll}",
                        view.label()
                    ),
                },
            }
        }
    }
}

fn mf_section(f: &Facts) {
    heading("Media Foundation virtual camera support");
    let build = f.build;
    match &f.sys_native {
        None => fail!("native system directory unknown; cannot inspect mfsensorgroup.dll"),
        Some(dir) => {
            let dll = dir.join("mfsensorgroup.dll");
            const MAX_PE: u64 = 32 << 20;
            match read_bounded(&dll, MAX_PE + 1) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    let level = if build >= os::WIN11_BUILD {
                        Level::Fail
                    } else {
                        Level::Info
                    };
                    emit(level, format_args!("{}: {}", dll.display(), io_err(&e)));
                }
                Err(e) => fail!("{}: {}", dll.display(), io_err(&e)),
                Ok(data) if data.len() as u64 > MAX_PE => warn!(
                    "{}: larger than {MAX_PE} bytes; export table not inspected",
                    dll.display()
                ),
                Ok(data) => match pe_exports(&data, "MFCreateVirtualCamera") {
                    Ok(true) if build >= os::WIN11_BUILD => {
                        pass!("{} exports MFCreateVirtualCamera", dll.display())
                    }
                    Ok(true) => info!(
                        "{} exports MFCreateVirtualCamera although build {build} is below {}",
                        dll.display(),
                        os::WIN11_BUILD
                    ),
                    Ok(false) if build >= os::WIN11_BUILD => fail!(
                        "{} does not export MFCreateVirtualCamera on build {build}",
                        dll.display()
                    ),
                    Ok(false) => info!(
                        "{} does not export MFCreateVirtualCamera: expected on Windows 10, DeskCam uses a driverless Frame Server camera plus DirectShow",
                        dll.display()
                    ),
                    Err(e) => fail!("{}: {e}", dll.display()),
                },
            }
        }
    }
    const PLATFORM: &str = r"SOFTWARE\Microsoft\Windows Media Foundation\Platform";
    for view in VIEWS {
        if view == View::Bit32 && !f.os_64 {
            continue;
        }
        let where_ = key_label(HKLM, PLATFORM, view);
        match read_value(HKLM, PLATFORM, view, "EnableFrameServerMode") {
            Err(e) if e == ERROR_FILE_NOT_FOUND => {
                pass!("{where_}\\EnableFrameServerMode absent: default (Frame Server mode on)")
            }
            Err(e) => warn!("{where_}\\EnableFrameServerMode: {}", win32(e)),
            Ok(v) if v.dword() == Some(0) => warn!(
                "{where_}\\EnableFrameServerMode = {v}: Media Foundation capture apps of this bitness bypass the Frame Server (a known compatibility tweak); DeskCam's Media Foundation camera requires the Frame Server on both Windows versions"
            ),
            Ok(v) => info!("{where_}\\EnableFrameServerMode = {v}"),
        }
    }
}

struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseServiceHandle(self.0);
        }
    }
}

fn services_section(f: &Facts) {
    heading("Windows Camera Frame Server services (read-only query)");
    info!(
        "Windows 11 serves the DeskCam virtual camera through FrameServer; the Windows 10 DirectShow filter is loaded inside each app and does not use it"
    );
    let scm = match unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) } {
        Ok(h) => Some(ScHandle(h)),
        Err(e) => {
            fail!(
                "OpenSCManagerW(SC_MANAGER_CONNECT): {}; falling back to the services registry",
                hr(&e)
            );
            None
        }
    };
    for name in ["FrameServer", "FrameServerMonitor"] {
        let disabled_level = if name == "FrameServer" && f.mf_expected() {
            Level::Fail
        } else {
            Level::Warn
        };
        let queried = scm
            .as_ref()
            .is_some_and(|scm| query_service(scm, name, f, disabled_level));
        if !queried {
            service_registry(name, f, disabled_level);
        }
    }
}

/// Returns `false` when the registry fallback should run.
fn query_service(scm: &ScHandle, name: &str, f: &Facts, disabled_level: Level) -> bool {
    let svc = match unsafe {
        OpenServiceW(
            scm.0,
            &HSTRING::from(name),
            SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
        )
    } {
        Ok(h) => ScHandle(h),
        Err(e) if e.code() == ERROR_SERVICE_DOES_NOT_EXIST.to_hresult() => {
            if name == "FrameServerMonitor" && f.build < os::WIN11_BUILD {
                skip!("{name}: not installed (only exists on newer builds)");
            } else {
                emit(
                    if name == "FrameServer" {
                        Level::Fail
                    } else {
                        Level::Warn
                    },
                    format_args!("{name}: service does not exist ({})", hr(&e)),
                );
            }
            return true;
        }
        Err(e) => {
            warn!(
                "{name}: OpenServiceW: {}; falling back to the services registry",
                hr(&e)
            );
            return false;
        }
    };
    let mut needed = 0u32;
    let _ = unsafe { QueryServiceConfigW(svc.0, None, 0, &mut needed) };
    let mut buf = vec![0u64; (needed as usize).div_ceil(8).max(1)];
    match unsafe {
        QueryServiceConfigW(
            svc.0,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 8) as u32,
            &mut needed,
        )
    } {
        Ok(()) => {
            let cfg = unsafe { &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW) };
            let text = |p: PWSTR| {
                if p.is_null() {
                    String::new()
                } else {
                    unsafe { p.to_string().unwrap_or_default() }
                }
            };
            let mut delayed_buf = [0u8; 16];
            let mut delayed_needed = 0u32;
            let delayed = unsafe {
                QueryServiceConfig2W(
                    svc.0,
                    SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                    Some(&mut delayed_buf[..]),
                    &mut delayed_needed,
                )
            }
            .map(|()| {
                unsafe {
                    std::ptr::read_unaligned(
                        delayed_buf.as_ptr() as *const SERVICE_DELAYED_AUTO_START_INFO
                    )
                }
                .fDelayedAutostart
                .as_bool()
            })
            .unwrap_or(false);
            let start = cfg.dwStartType.0;
            info!(
                "{name}: \"{}\", start type {} ({}{}), account {}, binary {}",
                text(cfg.lpDisplayName),
                start,
                start_type_name(start),
                if delayed { ", delayed" } else { "" },
                text(cfg.lpServiceStartName),
                text(cfg.lpBinaryPathName)
            );
            if start == 4 {
                emit(
                    disabled_level,
                    format_args!(
                        "{name} is DISABLED; Media Foundation camera access through the Frame Server cannot start"
                    ),
                );
            }
        }
        Err(e) => warn!("{name}: QueryServiceConfigW: {}", hr(&e)),
    }
    let mut status = [0u8; size_of::<SERVICE_STATUS_PROCESS>()];
    let mut status_needed = 0u32;
    match unsafe {
        QueryServiceStatusEx(
            svc.0,
            SC_STATUS_PROCESS_INFO,
            Some(&mut status[..]),
            &mut status_needed,
        )
    } {
        Ok(()) => {
            let s = unsafe {
                std::ptr::read_unaligned(status.as_ptr() as *const SERVICE_STATUS_PROCESS)
            };
            let state = match s.dwCurrentState.0 {
                1 => "stopped (normal: it starts on demand when a camera is opened)",
                2 => "start pending",
                3 => "stop pending",
                4 => "running",
                5 => "continue pending",
                6 => "pause pending",
                7 => "paused",
                _ => "unknown",
            };
            info!(
                "{name}: state {} {state}, PID {}, last exit code {} (service-specific {})",
                s.dwCurrentState.0, s.dwProcessId, s.dwWin32ExitCode, s.dwServiceSpecificExitCode
            );
        }
        Err(e) => warn!("{name}: QueryServiceStatusEx: {}", hr(&e)),
    }
    true
}

fn start_type_name(start: u32) -> &'static str {
    match start {
        0 => "boot",
        1 => "system",
        2 => "automatic",
        3 => "manual/demand",
        4 => "disabled",
        _ => "unknown",
    }
}

fn service_registry(name: &str, f: &Facts, disabled_level: Level) {
    let path = format!("SYSTEM\\CurrentControlSet\\Services\\{name}");
    let where_ = key_label(HKLM, &path, View::Bit64);
    let key = match open_key(HKLM, &path, View::Bit64) {
        Ok(k) => k,
        Err(e)
            if e == ERROR_FILE_NOT_FOUND
                && name == "FrameServerMonitor"
                && f.build < os::WIN11_BUILD =>
        {
            skip!("{name}: not installed (only exists on newer builds)");
            return;
        }
        Err(e) => {
            fail!("{name}: {where_}: {}", win32(e));
            return;
        }
    };
    for value in ["Start", "ImagePath", "ObjectName", "DelayedAutostart"] {
        match query(&key, value) {
            Ok(v) => {
                info!("{where_}\\{value} = {v}");
                if value == "Start" && v.dword() == Some(4) {
                    emit(
                        disabled_level,
                        format_args!("{name} is DISABLED (registry Start = 4)"),
                    );
                }
            }
            Err(e) => info!("{where_}\\{value}: {}", win32(e)),
        }
    }
    info!("{name}: run state unknown without Service Control Manager access");
}

fn system_files_section(f: &Facts) {
    heading("Media Foundation and DirectShow system files");
    const NATIVE: [(&str, &str); 10] = [
        ("quartz.dll", "DirectShow runtime"),
        ("devenum.dll", "DirectShow device enumerator"),
        ("qcap.dll", "DirectShow capture graph builder"),
        ("ksproxy.ax", "kernel-streaming proxy"),
        ("mfplat.dll", "Media Foundation platform"),
        ("mf.dll", "Media Foundation"),
        ("mfreadwrite.dll", "Media Foundation source reader"),
        ("mfcore.dll", "Media Foundation core"),
        ("mfsensorgroup.dll", "Media Foundation sensor groups"),
        ("frameserver.dll", "Camera Frame Server"),
    ];
    let level_for = |file: &str| match file {
        "quartz.dll" | "devenum.dll" => Level::Fail,
        "mfsensorgroup.dll" | "frameserver.dll" if f.mf_expected() => Level::Fail,
        "mfplat.dll" | "mf.dll" | "mfreadwrite.dll" | "mfcore.dll" if f.mf_expected() => {
            Level::Fail
        }
        _ => Level::Warn,
    };
    let mut mf_missing = false;
    for (dir, files, view) in [
        (f.sys_native.as_ref(), &NATIVE[..], "native"),
        (f.sys_wow64.as_ref(), &NATIVE[..7], "32-bit"),
    ] {
        let Some(dir) = dir else {
            skip!("{view} system directory unavailable");
            continue;
        };
        for (file, role) in files {
            let exists = report_file(
                &format!("{role} ({view})"),
                &dir.join(file),
                Expect::Pe,
                level_for(file),
            );
            if !exists && file.starts_with("mf") {
                mf_missing = true;
            }
        }
    }
    if mf_missing {
        warn!(
            "Media Foundation files are missing: typical of Windows N editions without the Media Feature Pack. Many camera apps and browsers use Media Foundation for capture and fail or show no cameras without it"
        );
    }
}

fn privacy_section() {
    heading("Camera and screen-capture privacy settings");
    info!(
        "an absent key means no explicit choice was stored: it is not a denial (the Windows default applies)"
    );
    info!(
        "policy and consent values show configuration only, not proof of what a given app is allowed: Windows enforces them per app at access time"
    );
    info!(
        "the camera switches gate the Windows camera stack (driver and Frame Server cameras); whether they affect a DirectShow filter loaded inside the app is not shown by this registry evidence"
    );
    const STORE: &str =
        r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore";
    for cap in [
        "webcam",
        "graphicsCaptureProgrammatic",
        "graphicsCaptureWithoutBorder",
    ] {
        for (hive, sub, meaning) in [
            (HKLM, "", "device-wide switch"),
            (HKCU, "", "user's 'let apps access' switch"),
            (
                HKCU,
                "\\NonPackaged",
                "user's 'let desktop apps access' switch",
            ),
        ] {
            let path = format!("{STORE}\\{cap}{sub}");
            let where_ = format!("{}\\{path}\\Value ({meaning})", hive.name);
            match read_value(hive, &path, View::Bit64, "Value") {
                Ok(v) => match v.text().as_deref() {
                    Some("Allow") => pass!("{where_} = {v}"),
                    Some("Deny") => warn!("{where_} = {v}"),
                    _ => info!("{where_} = {v}"),
                },
                Err(e) if e == ERROR_FILE_NOT_FOUND => info!("{where_}: absent"),
                Err(e) => warn!("{where_}: {}", win32(e)),
            }
        }
    }

    // Desktop apps that used the camera: only DeskCam/Discord entries are named.
    let nonpackaged = format!("{STORE}\\webcam\\NonPackaged");
    match open_key(HKCU, &nonpackaged, View::Bit64) {
        Err(e) if e == ERROR_FILE_NOT_FOUND => {
            info!("HKCU\\{nonpackaged}: absent (no desktop app has used a camera yet)")
        }
        Err(e) => warn!("HKCU\\{nonpackaged}: {}", win32(e)),
        Ok(key) => match subkeys(&key, 4096) {
            Err(e) => warn!("HKCU\\{nonpackaged}: RegEnumKeyExW: {}", win32(e)),
            Ok((names, total)) => {
                let mut shown = 0;
                for name in &names {
                    let lower = name.to_ascii_lowercase();
                    if !(lower.contains("discord") || lower.contains("deskcam")) {
                        continue;
                    }
                    shown += 1;
                    let entry = format!("{nonpackaged}\\{name}");
                    let times: Vec<String> = ["LastUsedTimeStart", "LastUsedTimeStop"]
                        .iter()
                        .map(|v| match read_value(HKCU, &entry, View::Bit64, v) {
                            Ok(RegValue::Qword(t)) if t > 0 => format!("{v} {}", filetime(t)),
                            Ok(other) => format!("{v} {other}"),
                            Err(e) => format!("{v} {}", win32(e)),
                        })
                        .collect();
                    info!("camera usage record {name}: {}", times.join(", "));
                }
                info!(
                    "{total} desktop-app camera usage records; {shown} for Discord/DeskCam shown, others not listed"
                );
            }
        },
    }

    const APP_PRIVACY: &str = r"SOFTWARE\Policies\Microsoft\Windows\AppPrivacy";
    for hive in [HKLM, HKCU] {
        for value in [
            "LetAppsAccessCamera",
            "LetAppsAccessGraphicsCaptureProgrammatic",
            "LetAppsAccessGraphicsCaptureWithoutBorder",
        ] {
            let where_ = format!("{}\\{APP_PRIVACY}\\{value}", hive.name);
            match read_value(hive, APP_PRIVACY, View::Bit64, value) {
                Ok(v) => match v.dword() {
                    Some(0) => info!("{where_} = {v}: policy leaves the choice to the user"),
                    Some(1) => pass!("{where_} = {v}: policy forces allow"),
                    Some(2) => warn!(
                        "{where_} = {v}: policy forces DENY (per-app exception lists may still allow some apps)"
                    ),
                    _ => warn!("{where_} = {v}: unexpected value"),
                },
                Err(e) if e == ERROR_FILE_NOT_FOUND => info!("{where_}: absent (no policy)"),
                Err(e) => warn!("{where_}: {}", win32(e)),
            }
        }
    }
    const CAMERA_POLICY: &str = r"SOFTWARE\Policies\Microsoft\Camera";
    for hive in [HKLM, HKCU] {
        let where_ = format!("{}\\{CAMERA_POLICY}\\AllowCamera", hive.name);
        match read_value(hive, CAMERA_POLICY, View::Bit64, "AllowCamera") {
            Ok(v) if v.dword() == Some(0) => {
                warn!("{where_} = {v}: camera use is disabled by policy")
            }
            Ok(v) => info!("{where_} = {v}"),
            Err(e) if e == ERROR_FILE_NOT_FOUND => info!("{where_}: absent (no policy)"),
            Err(e) => warn!("{where_}: {}", win32(e)),
        }
    }
}

fn cross_check_section(f: &Facts) {
    heading("Cross-checks");
    let deskcam_sessions: Vec<u32> = f
        .running(&["deskcam.exe"])
        .filter_map(|p| p.session.as_ref().ok().copied())
        .collect();
    if let Some(ours) = f.our_session
        && !deskcam_sessions.is_empty()
        && !deskcam_sessions.contains(&ours)
    {
        info!(
            "deskcam.exe runs in session(s) {deskcam_sessions:?}, this diagnostics run is in session {ours}"
        );
    }
    if !f.dshow_expected() {
        skip!(
            "session check: only the Windows 10 DirectShow path uses a per-session frame section"
        );
        return;
    }
    let consumers: Vec<&Proc> = f.running(&DISCORD).collect();
    if consumers.is_empty() {
        skip!("session check: Discord is not running");
        return;
    }
    if deskcam_sessions.is_empty() {
        warn!(
            "Discord is running but deskcam.exe is not: DeskCam is listed but delivers black frames"
        );
        return;
    }
    let mut mismatched = false;
    for p in consumers {
        match &p.session {
            Ok(s) if !deskcam_sessions.contains(s) => {
                mismatched = true;
                fail!(
                    "{} PID {} runs in session {s} but deskcam.exe only in {deskcam_sessions:?}: the Windows 10 frame section is per session, so it gets black frames",
                    p.exe,
                    p.pid
                );
            }
            Ok(_) => {}
            Err(e) => warn!("{} PID {}: session unknown ({e})", p.exe, p.pid),
        }
    }
    if !mismatched {
        pass!("Discord and deskcam.exe share a session");
    }
}

#[cfg(test)]
mod tests {
    use super::Facts;

    #[test]
    fn camera_registration_requirements_follow_platform_and_backend() {
        for (build, backend, directshow, media_foundation) in [
            (19045, "auto", true, true),
            (19045, "dshow", true, true),
            (22000, "auto", false, true),
            (22000, "mf", false, true),
            (22000, "dshow", true, false),
        ] {
            let facts = Facts {
                build,
                backend: Some(backend.into()),
                ..Facts::default()
            };
            assert_eq!(
                (facts.dshow_expected(), facts.mf_expected()),
                (directshow, media_foundation),
                "camera requirements on build {build} with backend {backend}"
            );
        }
    }
}
