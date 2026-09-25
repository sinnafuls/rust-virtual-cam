//! Background worker: owns the virtual camera, the shared section and (only while an app is
//! pulling frames) the screen capture.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deskcam_proto::{Section, StreamInfo, now_ms, paths, stream_info};
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFShutdown, MFStartup};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

use crate::capture::Capture;
use crate::config::{Config, FpsSel};
use crate::log::log;
use crate::monitor;
use crate::vcam::VirtualCamera;

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
    let _vcam = match VirtualCamera::create(&cfg.name) {
        Ok(v) => v,
        Err(e) => {
            log!("{e}");
            return report(Status::Error(e));
        }
    };
    log!("virtual camera '{}' registered", cfg.name);
    report(Status::Idle);

    let interval = Duration::from_nanos(1_000_000_000 / fps as u64);
    let mut section: Option<Section> = None;
    let mut capture: Option<Capture> = None;
    let (mut next_open, mut retry_at) = (0u64, 0u64);
    let mut next_frame = Instant::now();

    while !stop.load(Ordering::Acquire) {
        let now = now_ms();
        if section.is_none() && now >= next_open {
            section = Section::open(info.width, info.height, true);
            next_open = now + OPEN_RETRY_MS;
            if section.is_some() {
                log!("frame section opened");
            }
        }
        let demand = match &section {
            Some(s) => {
                s.ring.touch_writer(now);
                s.ring.reader_active(now)
            }
            None => false,
        };

        if demand && capture.is_none() && now >= retry_at {
            match Capture::new(mon.hmon, info, fps, cfg.cursor) {
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
            if let Some(s) = &section {
                s.ring.invalidate();
            }
            log!("capture stopped (no viewers)");
            report(Status::Idle);
        }

        match (&mut capture, &section) {
            (Some(c), Some(s)) => {
                if let Err(e) = c.tick(&s.ring) {
                    log!("capture error: {e}");
                    capture = None;
                    s.ring.invalidate();
                    retry_at = now + 1000;
                    report(Status::Error(format!("capture error: {e}")));
                    std::thread::sleep(IDLE_SLEEP);
                    continue;
                }
                next_frame += interval;
                let now = Instant::now();
                if next_frame < now {
                    next_frame = now + interval;
                }
                std::thread::sleep(next_frame - now);
            }
            _ => std::thread::sleep(IDLE_SLEEP),
        }
    }

    drop(capture);
    if let Some(s) = &section {
        s.ring.invalidate();
    }
    log!("worker stopped");
}
