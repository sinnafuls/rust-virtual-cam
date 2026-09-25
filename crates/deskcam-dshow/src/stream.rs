//! Delivery thread: while the graph is paused or running, pushes one sample per frame interval
//! to the downstream pin with the newest frame from the app's `Local\` section (black while the
//! app is not running or not capturing yet).
//!
//! Reading the section also writes the reader heartbeat, which is what makes `deskcam.exe` start
//! capturing the desktop.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deskcam_proto::{ReadOutcome, Section, convert, now_ms, paths, stream_info};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Media::DirectShow::{IMemAllocator, IMemInputPin, State_Stopped, VFW_E_NOT_COMMITTED, VFW_E_WRONG_STATE};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE;

use crate::filter::{Core, is_running};
use crate::format::{Format, VideoType, free_format_block};
use crate::{ObjGuard, trace};

const SECTION_RETRY_MS: u64 = 500;
/// After the writer has been gone this long, reopen: the app may be back with another size.
const REOPEN_AFTER_GONE_MS: u64 = 3000;

pub(crate) fn spawn(core: Arc<Core>) -> JoinHandle<()> {
    let guard = ObjGuard::new();
    std::thread::Builder::new()
        .name("deskcam-dshow".into())
        .spawn(move || {
            let _guard = guard;
            run(&core);
        })
        .expect("spawn delivery thread")
}

/// Everything one delivery needs, copied out of the state so no lock is held while the
/// allocator or the downstream pin block.
struct Job {
    allocator: IMemAllocator,
    input: IMemInputPin,
    vtype: VideoType,
    new_type: bool,
    clock: Option<IReferenceClock>,
    t_start: i64,
    running: bool,
}

fn run(core: &Core) {
    let mut source = FrameSource::default();
    let mut next_due = Instant::now();
    let mut traced_error = false;
    loop {
        let job = {
            let mut st = core.lock();
            loop {
                if st.filter_state == State_Stopped {
                    return;
                }
                if st.conn.is_none() {
                    st = core.wake.wait(st).unwrap_or_else(|p| p.into_inner());
                    continue;
                }
                let now = Instant::now();
                if next_due > now {
                    st = core.wake.wait_timeout(st, next_due - now).unwrap_or_else(|p| p.into_inner()).0;
                    continue;
                }
                let conn = st.conn.as_ref().expect("checked above");
                break Job {
                    allocator: conn.allocator.clone(),
                    input: conn.input.clone(),
                    vtype: st.vtype,
                    new_type: std::mem::take(&mut st.type_changed),
                    clock: st.clock.clone(),
                    t_start: st.t_start,
                    running: is_running(&st),
                };
            }
        };
        let interval = Duration::from_nanos(1_000_000_000 / job.vtype.fps.max(1) as u64);
        next_due += interval;
        let now = Instant::now();
        if next_due < now {
            next_due = now;
        }
        match deliver(&job, &mut source) {
            Ok(()) => {
                core.frames.fetch_add(1, Ordering::Relaxed);
                traced_error = false;
            }
            // Stopping or flushing: the state check above ends the loop.
            Err(e) if e.code() == VFW_E_NOT_COMMITTED || e.code() == VFW_E_WRONG_STATE => {}
            Err(e) => {
                if !traced_error {
                    traced_error = true;
                    trace(&format!("delivery failed: {e}"));
                }
            }
        }
    }
}

fn deliver(job: &Job, source: &mut FrameSource) -> windows_core::Result<()> {
    let mut sample = None;
    unsafe { job.allocator.GetBuffer(&mut sample, None, None, 0)? };
    let sample = sample.ok_or_else(|| windows_core::Error::from(E_FAIL))?;
    let bytes = job.vtype.frame_bytes();
    unsafe {
        if (sample.GetSize() as usize) < bytes {
            return Err(E_FAIL.into());
        }
        let out = std::slice::from_raw_parts_mut(sample.GetPointer()?, bytes);
        source.fill(&job.vtype, out);
        sample.SetActualDataLength(bytes as i32)?;
        sample.SetSyncPoint(true)?;
        sample.SetDiscontinuity(false)?;
        sample.SetPreroll(false)?;
        if job.new_type {
            let mut mt = AM_MEDIA_TYPE::default();
            job.vtype.write_to(&mut mt)?;
            let result = sample.SetMediaType(&mt);
            free_format_block(&mut mt);
            result?;
        }
        // Stream time once running; untimed samples in Paused are rendered as soon as they arrive.
        if job.running
            && let Some(clock) = &job.clock
            && let Ok(now) = clock.GetTime()
        {
            let start = now - job.t_start;
            let end = start + job.vtype.interval();
            sample.SetTime(Some(&start), Some(&end))?;
        }
        job.input.Receive(&sample)
    }
}

/// The app's frame section, reopened with the current `stream.bin` size whenever it is missing
/// or its writer has been gone for a while.
#[derive(Default)]
struct FrameSource {
    section: Option<(Section, usize, usize)>,
    next_try: u64,
    gone_since: Option<u64>,
    scratch: Vec<u8>,
    traced: Option<bool>,
}

impl FrameSource {
    fn ensure_section(&mut self, now: u64) {
        if self.section.is_some() || now < self.next_try {
            return;
        }
        self.next_try = now + SECTION_RETRY_MS;
        let info = stream_info::load_or_default(&paths::stream_file());
        let (w, h) = (info.width, info.height);
        // Read-only still shows frames, but without the heartbeat the app never starts capturing.
        self.section = Section::open_local(w, h, true)
            .or_else(|| Section::open_local(w, h, false))
            .map(|s| (s, w as usize, h as usize));
        self.gone_since = None;
        let ok = self.section.is_some();
        if self.traced != Some(ok) {
            self.traced = Some(ok);
            trace(if ok { "section opened" } else { "section unavailable (is deskcam.exe running?), retrying" });
        }
    }

    /// Writes the newest frame in the negotiated layout into `out`, or black.
    fn fill(&mut self, vt: &VideoType, out: &mut [u8]) {
        let now = now_ms();
        self.ensure_section(now);
        let (dw, dh) = (vt.width as usize, vt.height as usize);
        let scratch = &mut self.scratch;
        let outcome = match &self.section {
            Some((section, sw, sh)) => {
                section.ring.touch_reader(now);
                section.ring.read_latest(now, |slot| convert_frame(slot, (*sw, *sh), vt.format, (dw, dh), scratch, out))
            }
            None => ReadOutcome::NoFrame,
        };
        match outcome {
            ReadOutcome::WriterGone => {
                let since = *self.gone_since.get_or_insert(now);
                if now.saturating_sub(since) >= REOPEN_AFTER_GONE_MS {
                    self.section = None;
                    self.next_try = now;
                }
            }
            _ => self.gone_since = None,
        }
        if outcome != ReadOutcome::Frame {
            match vt.format {
                Format::Nv12 | Format::I420 => convert::black_420(dw, dh, out),
                Format::Yuy2 => convert::black_yuy2(dw, dh, out),
            }
        }
    }
}

/// Converts one NV12 frame of `src` size into `out` (`dst` size, `format` layout), scaling first
/// when the app's resolution differs from the negotiated one.
fn convert_frame(
    slot: &[u8],
    (sw, sh): (usize, usize),
    format: Format,
    (dw, dh): (usize, usize),
    scratch: &mut Vec<u8>,
    out: &mut [u8],
) {
    let frame: &[u8] = if (sw, sh) == (dw, dh) {
        slot
    } else {
        scratch.resize(convert::nv12_size(dw, dh), 0);
        convert::scale_nv12_nearest(slot, sw, sh, scratch, dw, dh);
        scratch
    };
    match format {
        Format::Nv12 => out.copy_from_slice(&frame[..convert::nv12_size(dw, dh)]),
        Format::I420 => convert::nv12_to_i420(frame, dw, dh, out),
        Format::Yuy2 => convert::nv12_to_yuy2(frame, dw, dh, out),
    }
}
