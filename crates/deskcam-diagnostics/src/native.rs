//! Native probes that load real Windows components: DirectShow discovery/activation/streaming,
//! Media Foundation enumeration, and the Windows.Graphics.Capture / Direct3D 11 prerequisites.
//!
//! Each mode runs on a worker thread under an inner watchdog that is shorter than the parent's
//! child-process timeout, so a hang is reported with the stage that did not return. Every
//! potentially blocking call is announced first (`[INFO] stage: ...`); the last such line in the
//! report is the stage that was running when anything went wrong.
//!
//! Nothing here writes files, registry values or settings. Shared-memory sections are opened
//! read-only. Media Foundation DLLs are resolved at run time so this executable still starts on
//! Windows N editions without the Media Feature Pack.

mod dshow;
mod graphics;
mod mf;

use std::fmt::Display;
use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use deskcam_proto::layout::{Namespace, section_name};
use deskcam_proto::{FrameRing, ReadOutcome, Section, StreamInfo, now_ms, paths, stream_info};
use windows::Win32::Foundation::{CloseHandle, WIN32_ERROR};
use windows::Win32::System::Diagnostics::Debug::{
    SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SEM_NOOPENFILEERRORBOX, SetErrorMode,
};
use windows::Win32::System::Memory::{FILE_MAP_READ, OpenFileMappingW};
use windows_core::HSTRING;

/// Inner watchdog per mode; the parent kills the child at 30 s (graphics, mf) / 45 s (dshow).
fn budget(mode: &str) -> Option<u64> {
    match mode {
        "dshow" => Some(38),
        "mf" | "graphics" => Some(24),
        _ => None,
    }
}

static STAGE: Mutex<String> = Mutex::new(String::new());

fn current_stage() -> String {
    STAGE.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Runs one native probe mode. 0: every check passed or was only informative; 1: a meaningful
/// check failed; 2: unknown mode.
pub fn run(mode: &str) -> i32 {
    // No "missing DLL" or crash dialogs: they would block until the parent's timeout.
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX | SEM_NOOPENFILEERRORBOX)
    };
    let Some(seconds) = budget(mode) else {
        println!("[FAIL] Unknown native probe mode '{mode}' (expected dshow, mf or graphics)");
        return 2;
    };
    println!(
        "[INFO] Native probe '{mode}': {}-bit process, Windows build {}",
        usize::BITS,
        deskcam_proto::os::build()
    );
    let body: fn(&mut Report) = match mode {
        "dshow" => dshow::run,
        "mf" => mf::run,
        _ => graphics::run,
    };
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name(format!("probe-{mode}"))
        .spawn(move || {
            let mut report = Report { failed: false };
            body(&mut report);
            let _ = tx.send(report.failed);
        });
    if let Err(e) = spawned {
        println!("[FAIL] Cannot start the probe thread: {e}");
        return 1;
    }
    match rx.recv_timeout(Duration::from_secs(seconds)) {
        Ok(false) => {
            println!("[INFO] Native probe '{mode}' finished: no failed checks");
            0
        }
        Ok(true) => {
            println!(
                "[INFO] Native probe '{mode}' finished: at least one check failed (see [FAIL] lines)"
            );
            1
        }
        Err(RecvTimeoutError::Timeout) => {
            println!(
                "[FAIL] Native probe '{mode}': stage '{}' did not return within {seconds}s (hang; later checks did not run)",
                current_stage()
            );
            1
        }
        Err(RecvTimeoutError::Disconnected) => {
            println!(
                "[FAIL] Native probe '{mode}': probe thread ended unexpectedly in stage '{}'",
                current_stage()
            );
            1
        }
    }
}

/// Line-oriented result log with the contract prefixes; remembers whether anything failed.
pub(crate) struct Report {
    failed: bool,
}

impl Report {
    /// Announces a stage before calling into anything that may block or crash.
    pub(crate) fn stage(&mut self, name: &str) {
        *STAGE.lock().unwrap_or_else(|p| p.into_inner()) = name.to_owned();
        println!("[INFO] stage: {name}");
    }

    pub(crate) fn pass(&mut self, msg: impl Display) {
        println!("[PASS] {msg}");
    }

    pub(crate) fn info(&mut self, msg: impl Display) {
        println!("[INFO] {msg}");
    }

    pub(crate) fn warn(&mut self, msg: impl Display) {
        println!("[WARN] {msg}");
    }

    pub(crate) fn fail(&mut self, msg: impl Display) {
        self.failed = true;
        println!("[FAIL] {msg}");
    }

    pub(crate) fn skip(&mut self, msg: impl Display) {
        println!("[SKIP] {msg}");
    }
}

/// `HRESULT 0xXXXXXXXX (system message)`.
pub(crate) fn hr(e: &windows_core::Error) -> String {
    let code = e.code().0 as u32;
    let msg = e.message();
    let msg = msg.trim();
    if msg.is_empty() {
        format!("HRESULT 0x{code:08X}")
    } else {
        format!("HRESULT 0x{code:08X} ({msg})")
    }
}

/// Announces `stage`, runs `f`, and logs a failure with the stage and HRESULT.
pub(crate) fn step<T>(
    r: &mut Report,
    stage: &str,
    f: impl FnOnce() -> windows_core::Result<T>,
) -> Option<T> {
    r.stage(stage);
    match f() {
        Ok(v) => Some(v),
        Err(e) => {
            r.fail(format!("{stage}: {}{}", hr(&e), hint_suffix(&e)));
            None
        }
    }
}

/// `Win32 error N (HRESULT 0x8007XXXX, system message)`.
pub(crate) fn win32(err: WIN32_ERROR) -> String {
    let e = windows_core::Error::from(err.to_hresult());
    format!("Win32 error {} = {}", err.0, hr(&e))
}

/// Likely meaning of an in-process COM activation failure.
pub(crate) fn activation_hint(code: u32) -> &'static str {
    match code {
        0x8004_0154 => {
            "class not registered in this process' registry view (32-bit and 64-bit views are separate)"
        }
        0x8004_0111 => "DLL loaded but does not serve this CLSID (stale or foreign InprocServer32)",
        0x8007_007E => "InprocServer32 DLL or one of its dependencies was not found",
        0x8007_00C1 => "DLL has the wrong architecture for this process or is corrupt",
        0x8007_045A => "DLL initialization failed (DllMain)",
        0x8007_0005 => "access denied loading the DLL (file ACL or policy)",
        0x8007_04EC | 0x8007_10D2 => "blocked by application control policy (AppLocker/WDAC/SRP)",
        0x8007_0057 => "invalid argument returned by the class factory",
        0x8000_4002 => "object does not implement the requested interface",
        0x8007_007F => "a required DLL export was not found (class factory or dependency API)",
        _ => "",
    }
}

pub(crate) fn hint_suffix(e: &windows_core::Error) -> String {
    match activation_hint(e.code().0 as u32) {
        "" => String::new(),
        h => format!(" - likely: {h}"),
    }
}

pub(crate) fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// FOURCC text for FOURCC-style subtypes (`Data1` printable), else the full GUID.
pub(crate) fn fourcc_or_guid(g: &windows_core::GUID) -> String {
    let bytes = g.data1.to_le_bytes();
    // FOURCC subtypes all share the {XXXXXXXX-0000-0010-8000-00AA00389B71} base.
    let fourcc_base = g.data2 == 0
        && g.data3 == 0x0010
        && g.data4 == [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71];
    if fourcc_base && bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("{g:?}")
    }
}

/// Luma statistics of a plane sample; computed in memory only, never stored.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Luma {
    pub min: u8,
    pub max: u8,
    pub mean: f64,
}

impl Luma {
    /// Samples every `step`-th byte of `plane`.
    pub(crate) fn of(plane: &[u8], step: usize) -> Option<Luma> {
        let (mut min, mut max, mut sum, mut n) = (u8::MAX, 0u8, 0u64, 0u64);
        for &v in plane.iter().step_by(step.max(1)) {
            min = min.min(v);
            max = max.max(v);
            sum += v as u64;
            n += 1;
        }
        (n > 0).then(|| Luma {
            min,
            max,
            mean: sum as f64 / n as f64,
        })
    }

    /// The constant video-black (Y = 16) that the DeskCam sources serve when no app frame exists.
    pub(crate) fn is_fallback_black(&self) -> bool {
        self.min >= 15 && self.max <= 17
    }
}

impl Display for Luma {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "luma min {} max {} mean {:.1}",
            self.min, self.max, self.mean
        )
    }
}

/// FNV-1a over a sparse sample, to tell distinct frames apart without keeping pixels.
pub(crate) fn sparse_hash(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes.iter().step_by(61) {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// `stream.bin` as the camera DLLs read it (they fall back to the default when it is invalid).
pub(crate) fn load_stream_info(r: &mut Report) -> StreamInfo {
    let path = paths::stream_file();
    match crate::inventory::read_bounded(&path, 33) {
        Ok(bytes) => match StreamInfo::from_bytes(&bytes) {
            Some(info) => {
                r.info(format!(
                    "{}: {}x{} @ {} fps",
                    path.display(),
                    info.width,
                    info.height,
                    info.fps
                ));
                info
            }
            None => {
                let d = stream_info::DEFAULT_STREAM;
                r.warn(format!(
                    "{}: invalid record ({} bytes); camera DLLs fall back to {}x{} @ {}",
                    path.display(),
                    bytes.len(),
                    d.width,
                    d.height,
                    d.fps
                ));
                d
            }
        },
        Err(e) => {
            let d = stream_info::DEFAULT_STREAM;
            r.warn(format!(
                "{}: cannot read ({e}, OS error {:?}); deskcam.exe writes it at start; camera DLLs assume {}x{} @ {}",
                path.display(),
                e.raw_os_error(),
                d.width,
                d.height,
                d.fps
            ));
            d
        }
    }
}

/// Opens the frame section read-only (never creates it). On failure reports the exact error.
pub(crate) fn open_section(r: &mut Report, ns: Namespace, info: StreamInfo) -> Option<Section> {
    let name = section_name(ns, info.width, info.height);
    r.stage(&format!("open frame section {name} (read-only)"));
    let opened = match ns {
        Namespace::Local => Section::open_local(info.width, info.height, false),
        Namespace::Global => Section::open(info.width, info.height, false),
    };
    if let Some(section) = opened {
        r.pass(format!(
            "Frame section {name} opened read-only; header valid"
        ));
        return Some(section);
    }
    // Section hides why; repeat the raw open for the exact error.
    let raw = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, &HSTRING::from(name.as_str())) };
    match raw {
        Ok(h) => {
            unsafe {
                let _ = CloseHandle(h);
            }
            r.warn(format!(
                "Frame section {name} exists but cannot be mapped for {}x{} or has no valid header (size or version mismatch)",
                info.width, info.height
            ));
        }
        Err(e) => {
            r.warn(format!(
                "Frame section {name} cannot be opened: {} (not found = deskcam.exe not running with this backend in this session, or a different output size)",
                hr(&e)
            ));
        }
    }
    None
}

/// One read-only look at the writer side of the ring.
#[derive(Clone, Copy)]
pub(crate) struct RingSample {
    pub writer_alive: bool,
    pub outcome: ReadOutcome,
    pub hash: Option<u64>,
    pub luma: Option<Luma>,
}

pub(crate) fn sample_ring(ring: &FrameRing) -> RingSample {
    let now = now_ms();
    let writer_alive = ring.writer_alive(now);
    let y_len = ring.width() as usize * ring.height() as usize;
    let (mut hash, mut luma) = (None, None);
    let outcome = ring.read_latest(now, |slot| {
        hash = Some(sparse_hash(slot));
        luma = Luma::of(&slot[..y_len.min(slot.len())], 7);
    });
    RingSample {
        writer_alive,
        outcome,
        hash,
        luma,
    }
}

pub(crate) fn describe_ring(s: &RingSample) -> String {
    let outcome = match s.outcome {
        ReadOutcome::Frame => "published frame present",
        ReadOutcome::NoFrame => "no published frame (app idle or capture not producing)",
        ReadOutcome::WriterGone => "writer heartbeat stale",
    };
    match s.luma {
        Some(l) => format!("writer alive: {}; {outcome}; {l}", s.writer_alive),
        None => format!("writer alive: {}; {outcome}", s.writer_alive),
    }
}

/// Collects ring observations during streaming.
#[derive(Default)]
pub(crate) struct RingWatch {
    pub polls: u32,
    pub alive: u32,
    pub frames: u32,
    pub distinct: u32,
    last_hash: Option<u64>,
    pub last_luma: Option<Luma>,
}

impl RingWatch {
    pub(crate) fn add(&mut self, s: &RingSample) {
        self.polls += 1;
        self.alive += s.writer_alive as u32;
        if s.outcome == ReadOutcome::Frame {
            self.frames += 1;
            if s.hash.is_some() && s.hash != self.last_hash {
                self.distinct += 1;
                self.last_hash = s.hash;
            }
            self.last_luma = s.luma;
        }
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "{} polls: writer alive in {}, published frame in {}, {} distinct frame contents{}",
            self.polls,
            self.alive,
            self.frames,
            self.distinct,
            self.last_luma
                .map(|l| format!("; last {l}"))
                .unwrap_or_default()
        )
    }
}
