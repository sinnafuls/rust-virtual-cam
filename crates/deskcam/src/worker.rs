//! Background worker: owns the virtual camera, the shared frame sections and (only while an app
//! is pulling frames) the screen capture.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deskcam_proto::{FrameRing, Namespace, Section, StreamInfo, now_ms, os, paths, stream_info};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

use crate::backend::Backend;
use crate::capture::Capture;
use crate::config::{Config, FpsSel};
use crate::log::log;
use crate::monitor;

const OPEN_RETRY_MS: u64 = 250;
const IDLE_SLEEP: Duration = Duration::from_millis(250);

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Status {
    Starting,
    Idle,
    Streaming { w: u32, h: u32, fps: u32 },
    Error(String),
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Starting => write!(f, "DeskCam: starting"),
            Status::Idle => write!(f, "DeskCam: idle (no app using the camera)"),
            Status::Streaming { w, h, fps } => write!(f, "DeskCam: streaming {w}x{h} @ {fps} fps"),
            Status::Error(msg) => write!(f, "DeskCam: error: {msg}"),
        }
    }
}

pub fn spawn(cfg: Config, stop: Arc<AtomicBool>, report: impl Fn(Status) + Send + 'static) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("deskcam-worker".into())
        .spawn(move || {
            let ro = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
            let mf = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) };
            match &mf {
                Ok(()) => run(&cfg, &stop, &report),
                Err(e) => report(Status::Error(format!("Media Foundation startup failed: {e}"))),
            }
            unsafe {
                if mf.is_ok() {
                    let _ = MFShutdown();
                }
                if ro.is_ok() {
                    RoUninitialize();
                }
            }
        })
        .expect("spawn worker thread")
}

fn run(cfg: &Config, stop: &AtomicBool, report: &dyn Fn(Status)) {
    let build = os::build();
    log!("Windows build {build}");
    if build < os::MIN_BUILD {
        return report(Status::Error(format!(
            "DeskCam needs Windows 10 version 1903 (build {}) or newer; this is build {build}",
            os::MIN_BUILD
        )));
    }
    let monitors = monitor::enumerate();
    for m in &monitors {
        log!("{}", monitor::describe(m));
    }
    let mon = match monitor::select(&monitors, cfg.monitor) {
        Ok(m) => m.clone(),
        Err(e) => return report(Status::Error(e)),
    };
    let fps = match cfg.fps {
        FpsSel::Fixed(n) => n,
        FpsSel::Monitor => mon.refresh_hz.min(240),
    };
    let info = StreamInfo { width: cfg.width, height: cfg.height, fps };
    log!("using {}; output {}x{} @ {} fps", monitor::describe(&mon), info.width, info.height, fps);

    if let Err(e) = std::fs::create_dir_all(paths::data_dir())
        .and_then(|()| stream_info::write_atomic(&paths::stream_file(), info))
    {
        return report(Status::Error(format!("cannot write {}: {e}", paths::stream_file().display())));
    }
    let backend = match Backend::start(cfg, info) {
        Ok(b) => b,
        Err(e) => {
            log!("{e}");
            return report(Status::Error(e));
        }
    };
    log!("camera '{}' ready ({})", cfg.name, backend.kind());
    report(Status::Idle);

    let interval = Duration::from_nanos(1_000_000_000 / fps as u64);
    let mut feeds = backend.namespaces().map(|ns| ns.map(Feed::new));
    let mut capture: Option<Capture> = None;
    let (mut next_open, mut retry_at) = (0u64, 0u64);
    let mut next_frame = Instant::now();

    while !stop.load(Ordering::Acquire) {
        let now = now_ms();
        if now >= next_open && feeds.iter().flatten().any(|f| f.section.is_none()) {
            for f in feeds.iter_mut().flatten().filter(|f| f.section.is_none()) {
                f.section = Backend::open_section(f.ns, info);
                if f.section.is_some() {
                    log!("frame section opened ({:?})", f.ns);
                }
            }
            next_open = now + OPEN_RETRY_MS;
        }
        for f in feeds.iter_mut().flatten() {
            let Some(s) = &f.section else { continue };
            s.ring.touch_writer(now);
            let active = s.ring.reader_active(now);
            // A section nobody reads any more must not keep the last desktop frame.
            if f.active && !active {
                s.ring.invalidate();
            }
            f.current &= active;
            f.active = active;
        }
        let demand = feeds.iter().flatten().any(|f| f.active);

        if demand && capture.is_none() && now >= retry_at {
            match Capture::new(mon.hmon, info, fps, cfg.cursor, cfg.capture) {
                Ok(c) => {
                    log!("capture started");
                    capture = Some(c);
                    next_frame = Instant::now();
                    report(Status::Streaming { w: info.width, h: info.height, fps });
                }
                Err(e) => {
                    log!("capture failed: {e}");
                    report(Status::Error(format!("capture failed: {e}")));
                    retry_at = now + 2000;
                }
            }
        } else if !demand && capture.is_some() {
            capture = None;
            log!("capture stopped (no viewers)");
            report(Status::Idle);
        }

        match &mut capture {
            Some(c) => {
                if let Err(e) = stream(c, &mut feeds, interval, &mut next_frame) {
                    log!("capture error: {e}");
                    capture = None;
                    feeds.iter_mut().flatten().for_each(Feed::invalidate);
                    retry_at = now_ms() + 1000;
                    report(Status::Error(format!("capture error: {e}")));
                    std::thread::sleep(IDLE_SLEEP);
                }
            }
            None => std::thread::sleep(IDLE_SLEEP),
        }
    }

    drop(capture);
    feeds.iter_mut().flatten().for_each(Feed::invalidate);
    log!("worker stopped");
}

/// One pass while capturing: hands readers that just joined the last converted frame, then waits
/// for a new desktop frame and publishes it to every reader on the frame grid.
fn stream(
    c: &mut Capture,
    feeds: &mut [Option<Feed>; 2],
    interval: Duration,
    next_frame: &mut Instant,
) -> windows_core::Result<()> {
    // A reader that (re)joins while another one keeps the capture running would otherwise see
    // nothing until the desktop changes.
    if feeds.iter().flatten().any(Feed::needs_frame)
        && c.republish(feeds.iter().flatten().filter(|f| f.needs_frame()).filter_map(Feed::ring))?
    {
        feeds.iter_mut().flatten().for_each(|f| f.current |= f.active);
    }

    // Sleep until Windows has a new desktop frame; a static screen costs nothing. The timeout
    // keeps the heartbeat and viewer check running.
    if !c.wait_frame(IDLE_SLEEP.as_millis() as u32) {
        return Ok(());
    }
    // Never exceed the configured rate, even where MinUpdateInterval is unsupported.
    let now = Instant::now();
    if now < *next_frame {
        std::thread::sleep(*next_frame - now);
    }
    let tick_start = Instant::now();
    if c.tick(feeds.iter().flatten().filter(|f| f.active).filter_map(Feed::ring))? {
        feeds.iter_mut().flatten().for_each(|f| f.current |= f.active);
    }
    // Stay on the frame grid. After a stall, restart the grid one interval after this frame:
    // never replay a backlog, never publish two frames closer than `interval`.
    *next_frame += interval;
    if *next_frame <= tick_start {
        *next_frame = tick_start + interval;
    }
    Ok(())
}

/// A frame section the worker writes and its reader state as of the last pass.
struct Feed {
    ns: Namespace,
    section: Option<Section>,
    /// A reader heartbeat is present.
    active: bool,
    /// The ring holds the capture's latest converted frame.
    current: bool,
}

impl Feed {
    fn new(ns: Namespace) -> Feed {
        Feed { ns, section: None, active: false, current: false }
    }

    fn ring(&self) -> Option<&FrameRing> {
        self.section.as_ref().map(|s| &s.ring)
    }

    fn needs_frame(&self) -> bool {
        self.active && !self.current
    }

    fn invalidate(&mut self) {
        if let Some(s) = &self.section {
            s.ring.invalidate();
        }
        self.current = false;
    }
}
