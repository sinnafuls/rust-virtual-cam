//! Named pagefile-backed section holding the [`FrameRing`].

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows_core::HSTRING;

use crate::layout::{FrameRing, Namespace, section_name, section_size};

/// SYSTEM + LocalService full access; Authenticated Users read/write (the app writes frames);
/// Everyone, AppContainers and LPAC read (sandboxed consumers).
pub const SECTION_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GRGW;;;AU)(A;;GR;;;WD)(A;;GR;;;AC)(A;;GR;;;S-1-15-2-2)";

/// Windows 10 section created by the app: same DACL, plus a low integrity label so low-IL
/// consumer processes (sandboxed browser helpers) can write the reader heartbeat too.
pub const SECTION_SDDL_LOCAL: &str =
    "D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GRGW;;;AU)(A;;GR;;;WD)(A;;GR;;;AC)(A;;GR;;;S-1-15-2-2)S:(ML;;NW;;;LW)";

pub struct Section {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    pub ring: FrameRing,
}

unsafe impl Send for Section {}
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

impl Section {
    fn map(handle: HANDLE, width: u32, height: u32, writable: bool) -> Option<Section> {
        let access = if writable { FILE_MAP(FILE_MAP_READ.0 | FILE_MAP_WRITE.0) } else { FILE_MAP_READ };
        let view = unsafe { MapViewOfFile(handle, access, 0, 0, section_size(width, height)) };
        if view.Value.is_null() {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return None;
        }
        let ring = unsafe { FrameRing::from_raw(view.Value as *mut u8, width, height, writable) };
        Some(Section { handle, view, ring })
    }

    /// Creates `name` with `sddl`, or opens it if it already exists; initializes the header only
    /// when this call created it. `None` if creation is not permitted.
    fn create_named(name: &str, sddl: &str, width: u32, height: u32) -> Option<Section> {
        let name = HSTRING::from(name);
        let size = section_size(width, height) as u64;
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let created = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(sddl), SDDL_REVISION_1, &mut sd, None)
                .ok()
                .and_then(|()| {
                    let sa = SECURITY_ATTRIBUTES {
                        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                        lpSecurityDescriptor: sd.0,
                        bInheritHandle: false.into(),
                    };
                    let handle = CreateFileMappingW(
                        INVALID_HANDLE_VALUE,
                        Some(&sa),
                        PAGE_READWRITE,
                        (size >> 32) as u32,
                        size as u32,
                        &name,
                    );
                    let existed = GetLastError() == ERROR_ALREADY_EXISTS;
                    LocalFree(Some(HLOCAL(sd.0)));
                    handle.ok().map(|h| (h, existed))
                })
        }?;
        let (handle, existed) = created;
        let section = Self::map(handle, width, height, true)?;
        if !existed {
            section.ring.init_header();
        }
        section.ring.header_valid().then_some(section)
    }

    fn open_named(name: &str, width: u32, height: u32, writable: bool) -> Option<Section> {
        let access = if writable { FILE_MAP_READ.0 | FILE_MAP_WRITE.0 } else { FILE_MAP_READ.0 };
        let handle = unsafe { OpenFileMappingW(access, false, &HSTRING::from(name)) }.ok()?;
        let section = Self::map(handle, width, height, writable)?;
        section.ring.header_valid().then_some(section)
    }

    /// Media source side: create the global section with [`SECTION_SDDL`], or open it when
    /// creation is not permitted (unprivileged consumer process) or it already exists.
    pub fn create_or_open_global(width: u32, height: u32) -> Option<Section> {
        Self::create_named(&section_name(Namespace::Global, width, height), SECTION_SDDL, width, height)
            .or_else(|| Self::open(width, height, true))
            .or_else(|| Self::open(width, height, false))
    }

    /// Opens an existing global section; `None` if missing, inaccessible or not initialized.
    pub fn open(width: u32, height: u32, writable: bool) -> Option<Section> {
        Self::open_named(&section_name(Namespace::Global, width, height), width, height, writable)
    }

    /// App side on Windows 10: create the session-local section the DirectShow filter reads.
    pub fn create_local(width: u32, height: u32) -> Option<Section> {
        Self::create_named(&section_name(Namespace::Local, width, height), SECTION_SDDL_LOCAL, width, height)
    }

    /// DirectShow filter side: open the app's session-local section.
    pub fn open_local(width: u32, height: u32, writable: bool) -> Option<Section> {
        Self::open_named(&section_name(Namespace::Local, width, height), width, height, writable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sddl_parses() {
        for sddl in [SECTION_SDDL, SECTION_SDDL_LOCAL] {
            let mut sd = PSECURITY_DESCRIPTOR::default();
            unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(&HSTRING::from(sddl), SDDL_REVISION_1, &mut sd, None)
                    .unwrap();
                LocalFree(Some(HLOCAL(sd.0)));
            }
        }
    }

    #[test]
    fn local_section_round_trip() {
        let (w, h) = (322, 182);
        let writer = Section::create_local(w, h).expect("create local section");
        let reader = Section::open_local(w, h, true).expect("open local section");
        writer.ring.touch_writer(crate::now_ms());
        writer.ring.publish(|b| b.fill(42));
        let mut seen = 0;
        assert_eq!(reader.ring.read_latest(crate::now_ms(), |b| seen = b[0]), crate::ReadOutcome::Frame);
        assert_eq!(seen, 42);
        assert!(Section::open_local(w + 2, h, false).is_none());
    }
}
