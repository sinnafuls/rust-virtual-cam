//! The single video stream. `RequestSample` only queues the token; a delivery thread completes
//! one request per frame interval with the newest frame from the shared section, so consumers
//! see evenly paced samples instead of bursts of repeated frames.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deskcam_proto::color::nv12_row_to_bgrx;
use deskcam_proto::{ReadOutcome, Section, StreamInfo, now_ms};
use windows::Win32::Foundation::{E_FAIL, S_OK};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{GUID, HRESULT, IUnknown, Interface, Ref, implement};

use crate::attrs::{forward_attributes, new_store};
use crate::{ObjGuard, trace};

const ALLOCATOR_SAMPLES: u32 = 6;
const SECTION_RETRY_MS: u64 = 500;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Format {
    Nv12,
    Rgb32,
}

struct StreamState {
    info: StreamInfo,
    state: MF_STREAM_STATE,
    queue: Option<IMFMediaEventQueue>,
    allocator: Option<IMFVideoSampleAllocatorEx>,
    allocator_ready: bool,
    source: Option<IMFMediaSource>,
    pending: VecDeque<Option<IUnknown>>,
    format: Format,
    interval: Duration,
    section: Option<Section>,
    next_section_try: u64,
    section_traced: Option<bool>,
    alloc_fail_traced: bool,
    shutdown: bool,
}

// Media Foundation objects used here are free-threaded; all access is serialized by the mutex.
unsafe impl Send for StreamState {}

struct Shared {
    st: Mutex<StreamState>,
    cv: Condvar,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[implement(IMFMediaStream2, IMFAttributes, IKsControl)]
pub struct MediaStream {
    attrs: IMFAttributes,
    descriptor: IMFStreamDescriptor,
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    _guard: ObjGuard,
}

forward_attributes!(MediaStream_Impl, attrs);

fn media_type(info: StreamInfo, fps: u32, format: Format) -> windows_core::Result<IMFMediaType> {
    let (w, h) = (info.width, info.height);
    let t = unsafe { MFCreateMediaType()? };
    let (subtype, stride, frame_bytes) = match format {
        Format::Nv12 => (MFVideoFormat_NV12, w, w as u64 * h as u64 * 3 / 2),
        Format::Rgb32 => (MFVideoFormat_RGB32, w * 4, w as u64 * h as u64 * 4),
    };
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
        t.SetUINT32(&MF_MT_DEFAULT_STRIDE, stride)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, (frame_bytes * 8 * fps as u64).min(u32::MAX as u64) as u32)?;
        if format == Format::Nv12 {
            t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT601.0 as u32)?;
            t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        }
    }
    Ok(t)
}

pub(crate) fn new(info: StreamInfo, id: u32) -> windows_core::Result<windows_core::ComObject<MediaStream>> {
    let attrs = new_store()?;
    unsafe {
        attrs.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        attrs.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, id)?;
        attrs.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        attrs.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)?;
    }
    let mut types = vec![
        Some(media_type(info, info.fps, Format::Nv12)?),
        Some(media_type(info, info.fps, Format::Rgb32)?),
    ];
    if info.fps > 30 {
        types.push(Some(media_type(info, 30, Format::Nv12)?));
        types.push(Some(media_type(info, 30, Format::Rgb32)?));
    }
    let descriptor = unsafe { MFCreateStreamDescriptor(id, &types)? };
    unsafe { descriptor.GetMediaTypeHandler()?.SetCurrentMediaType(types[0].as_ref())? };
    let queue = unsafe { MFCreateEventQueue()? };

    let shared = Arc::new(Shared {
        st: Mutex::new(StreamState {
            info,
            state: MF_STREAM_STATE_STOPPED,
            queue: Some(queue),
            allocator: None,
            allocator_ready: false,
            source: None,
            pending: VecDeque::new(),
            format: Format::Nv12,
            interval: Duration::from_nanos(1_000_000_000 / info.fps as u64),
            section: None,
            next_section_try: 0,
            section_traced: None,
            alloc_fail_traced: false,
            shutdown: false,
        }),
        cv: Condvar::new(),
    });
    Ok(windows_core::ComObject::new(MediaStream {
        attrs,
        descriptor,
        shared,
        thread: Mutex::new(None),
        _guard: ObjGuard::new(),
    }))
}

impl MediaStream {
    pub(crate) fn descriptor(&self) -> IMFStreamDescriptor {
        self.descriptor.clone()
    }

    pub(crate) fn attributes(&self) -> IMFAttributes {
        self.attrs.clone()
    }

    pub(crate) fn set_source(&self, source: IMFMediaSource) {
        lock(&self.shared.st).source = Some(source);
    }

    pub(crate) fn set_allocator(&self, allocator: IMFVideoSampleAllocatorEx) {
        let mut st = lock(&self.shared.st);
        st.allocator = Some(allocator);
        st.allocator_ready = false;
    }

    pub(crate) fn state(&self) -> MF_STREAM_STATE {
        lock(&self.shared.st).state
    }

    pub(crate) fn start(&self, ty: Option<IMFMediaType>) -> windows_core::Result<()> {
        let mut st = lock(&self.shared.st);
        if st.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        let queue = st.queue.clone().ok_or(windows_core::Error::from(MF_E_SHUTDOWN))?;
        let explicit = ty.is_some();
        let ty = match ty {
            Some(t) => t,
            None => unsafe { self.descriptor.GetMediaTypeHandler()?.GetCurrentMediaType()? },
        };
        let subtype = unsafe { ty.GetGUID(&MF_MT_SUBTYPE)? };
        st.format = if subtype == MFVideoFormat_NV12 {
            Format::Nv12
        } else if subtype == MFVideoFormat_RGB32 {
            Format::Rgb32
        } else {
            return Err(MF_E_INVALIDMEDIATYPE.into());
        };
        let rate = unsafe { ty.GetUINT64(&MF_MT_FRAME_RATE) }.unwrap_or(((st.info.fps as u64) << 32) | 1);
        let (num, den) = (rate >> 32, rate & 0xFFFF_FFFF);
        let (num, den) = if num == 0 || den == 0 { (st.info.fps as u64, 1) } else { (num, den) };
        st.interval = Duration::from_nanos(1_000_000_000 * den / num);

        if st.allocator.is_none() {
            let mut raw = std::ptr::null_mut();
            unsafe {
                MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocatorEx::IID, &mut raw)?;
                st.allocator = Some(IMFVideoSampleAllocatorEx::from_raw(raw));
            }
            st.allocator_ready = false;
        }
        if explicit || !st.allocator_ready {
            let allocator = st.allocator.clone().ok_or(windows_core::Error::from(MF_E_SHUTDOWN))?;
            unsafe { allocator.InitializeSampleAllocator(ALLOCATOR_SAMPLES, &ty)? };
            st.allocator_ready = true;
        }
        st.alloc_fail_traced = false;
        ensure_section(&mut st);
        unsafe { queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null())? };
        st.state = MF_STREAM_STATE_RUNNING;
        trace(&format!("stream started {:?} {}x{} interval {:?}", st.format, st.info.width, st.info.height, st.interval));
        drop(st);
        // The delivery thread exists only once an app actually streams; sources the Frame
        // Server creates just to inspect descriptors never get one.
        let mut thread = lock(&self.thread);
        if thread.is_none() {
            let shared = self.shared.clone();
            *thread = Some(
                std::thread::Builder::new()
                    .name("deskcam-delivery".into())
                    .spawn(move || delivery_loop(shared))
                    .map_err(|_| windows_core::Error::from(E_FAIL))?,
            );
        }
        self.shared.cv.notify_all();
        Ok(())
    }

    pub(crate) fn stop(&self) -> windows_core::Result<()> {
        let mut st = lock(&self.shared.st);
        if st.shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        let queue = st.queue.clone().ok_or(windows_core::Error::from(MF_E_SHUTDOWN))?;
        st.pending.clear();
        if let Some(allocator) = &st.allocator {
            if st.allocator_ready {
                unsafe {
                    let _ = allocator.UninitializeSampleAllocator();
                }
            }
        }
        st.allocator_ready = false;
        st.state = MF_STREAM_STATE_STOPPED;
        unsafe { queue.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, std::ptr::null())? };
        trace("stream stopped");
        Ok(())
    }

    pub(crate) fn shutdown(&self) {
        let (queue, _allocator, _source, _section) = {
            let mut st = lock(&self.shared.st);
            st.shutdown = true;
            st.pending.clear();
            (st.queue.take(), st.allocator.take(), st.source.take(), st.section.take())
        };
        self.shared.cv.notify_all();
        if let Some(handle) = lock(&self.thread).take() {
            if handle.thread().id() != std::thread::current().id() {
                let _ = handle.join();
            }
        }
        if let Some(queue) = queue {
            unsafe {
                let _ = queue.Shutdown();
            }
        }
    }

    fn queue(&self) -> windows_core::Result<IMFMediaEventQueue> {
        lock(&self.shared.st).queue.clone().ok_or(MF_E_SHUTDOWN.into())
    }
}

impl Drop for MediaStream {
    fn drop(&mut self) {
        if lock(&self.thread).is_some() {
            self.shutdown();
        }
    }
}

fn ensure_section(st: &mut StreamState) {
    if st.section.is_some() {
        return;
    }
    let now = now_ms();
    if now < st.next_section_try {
        return;
    }
    st.section = Section::create_or_open_global(st.info.width, st.info.height);
    let ok = st.section.is_some();
    if !ok {
        st.next_section_try = now + SECTION_RETRY_MS;
    }
    if st.section_traced != Some(ok) {
        st.section_traced = Some(ok);
        trace(if ok { "section ready" } else { "section unavailable, retrying" });
    }
}

fn delivery_loop(shared: Arc<Shared>) {
    let mut next_due = Instant::now();
    let mut st = lock(&shared.st);
    loop {
        while !st.shutdown && (st.pending.is_empty() || st.state != MF_STREAM_STATE_RUNNING) {
            st = shared.cv.wait(st).unwrap_or_else(|p| p.into_inner());
        }
        if st.shutdown {
            return;
        }
        let interval = st.interval;
        let now = Instant::now();
        if next_due + interval < now {
            next_due = now;
        }
        if next_due > now {
            drop(st);
            std::thread::sleep(next_due - now);
            st = lock(&shared.st);
            continue;
        }
        if let Err(e) = deliver_one(&mut st) {
            trace(&format!("deliver failed: {e}"));
        }
        next_due += interval;
    }
}

fn deliver_one(st: &mut StreamState) -> windows_core::Result<()> {
    let Some(token) = st.pending.pop_front() else { return Ok(()) };
    ensure_section(st);
    let (Some(allocator), Some(queue)) = (st.allocator.clone(), st.queue.clone()) else {
        return Ok(());
    };
    let sample = match unsafe { allocator.AllocateSample() } {
        Ok(s) => s,
        Err(e) => {
            // All samples are held downstream; drop this request, the pipeline asks again.
            if !st.alloc_fail_traced {
                st.alloc_fail_traced = true;
                trace(&format!("AllocateSample failed: {e}"));
            }
            return Ok(());
        }
    };
    fill_sample(st, &sample)?;
    if let Some(section) = &st.section {
        section.ring.touch_reader(now_ms());
    }
    let duration = (st.interval.as_nanos() / 100) as i64;
    unsafe {
        sample.SetSampleTime(MFGetSystemTime())?;
        sample.SetSampleDuration(duration)?;
        sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        if let Some(token) = &token {
            sample.SetUnknown(&MFSampleExtension_Token, token)?;
        }
        queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample)?;
    }
    Ok(())
}

fn fill_sample(st: &StreamState, sample: &IMFSample) -> windows_core::Result<()> {
    let (w, h) = (st.info.width as usize, st.info.height as usize);
    let row_bytes = match st.format {
        Format::Nv12 => w,
        Format::Rgb32 => w * 4,
    };
    let rows = match st.format {
        Format::Nv12 => h * 3 / 2,
        Format::Rgb32 => h,
    };
    let buffer = unsafe { sample.GetBufferByIndex(0)? };
    if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer2>() {
        let (mut scan0, mut pitch, mut start, mut len) = (std::ptr::null_mut(), 0i32, std::ptr::null_mut(), 0u32);
        unsafe { buffer2d.Lock2DSize(MF2DBuffer_LockFlags_Write, &mut scan0, &mut pitch, &mut start, &mut len)? };
        let fits = (pitch.unsigned_abs() as usize) >= row_bytes
            && (pitch.unsigned_abs() as usize) * rows <= len as usize;
        if fits {
            write_frame(st, scan0, pitch as isize);
        }
        unsafe { buffer2d.Unlock2D()? };
        if !fits {
            return Err(E_FAIL.into());
        }
    } else {
        let needed = row_bytes * rows;
        let (mut data, mut max) = (std::ptr::null_mut(), 0u32);
        unsafe { buffer.Lock(&mut data, Some(&mut max), None)? };
        let fits = max as usize >= needed;
        if fits {
            write_frame(st, data, row_bytes as isize);
        }
        unsafe { buffer.Unlock()? };
        if !fits {
            return Err(E_FAIL.into());
        }
        unsafe { buffer.SetCurrentLength(needed as u32)? };
    }
    Ok(())
}

/// Writes the newest frame (or black) into a locked buffer whose row `r` starts at `dst + r*pitch`.
fn write_frame(st: &StreamState, dst: *mut u8, pitch: isize) {
    let (w, h) = (st.info.width as usize, st.info.height as usize);
    let format = st.format;
    let row = |r: usize, len: usize| unsafe { std::slice::from_raw_parts_mut(dst.offset(r as isize * pitch), len) };
    let outcome = match &st.section {
        Some(section) => section.ring.read_latest(now_ms(), |slot| match format {
            Format::Nv12 if pitch == w as isize => {
                // Tightly packed destination: one copy for both planes.
                unsafe { std::slice::from_raw_parts_mut(dst, slot.len()) }.copy_from_slice(slot);
            }
            Format::Nv12 => {
                for r in 0..h * 3 / 2 {
                    row(r, w).copy_from_slice(&slot[r * w..][..w]);
                }
            }
            Format::Rgb32 => {
                for r in 0..h {
                    nv12_row_to_bgrx(&slot[r * w..][..w], &slot[w * h + (r / 2) * w..][..w], row(r, w * 4));
                }
            }
        }),
        None => ReadOutcome::NoFrame,
    };
    if outcome != ReadOutcome::Frame {
        match format {
            Format::Nv12 => {
                for r in 0..h {
                    row(r, w).fill(16);
                }
                for r in h..h * 3 / 2 {
                    row(r, w).fill(128);
                }
            }
            Format::Rgb32 => {
                for r in 0..h {
                    for px in row(r, w * 4).chunks_exact_mut(4) {
                        px.copy_from_slice(&[0, 0, 0, 255]);
                    }
                }
            }
        }
    }
}

impl IMFMediaEventGenerator_Impl for MediaStream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> windows_core::Result<IMFMediaEvent> {
        let queue = self.queue()?;
        unsafe { queue.GetEvent(flags.0) }
    }

    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> windows_core::Result<()> {
        let queue = self.queue()?;
        unsafe { queue.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }

    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> windows_core::Result<IMFMediaEvent> {
        let queue = self.queue()?;
        unsafe { queue.EndGetEvent(result.as_ref()) }
    }

    fn QueueEvent(&self, met: u32, ext: *const GUID, hr: HRESULT, value: *const PROPVARIANT) -> windows_core::Result<()> {
        let queue = self.queue()?;
        unsafe { queue.QueueEventParamVar(met, ext, hr, value) }
    }
}

impl IMFMediaStream_Impl for MediaStream_Impl {
    fn GetMediaSource(&self) -> windows_core::Result<IMFMediaSource> {
        lock(&self.shared.st).source.clone().ok_or(MF_E_SHUTDOWN.into())
    }

    fn GetStreamDescriptor(&self) -> windows_core::Result<IMFStreamDescriptor> {
        if lock(&self.shared.st).shutdown {
            return Err(MF_E_SHUTDOWN.into());
        }
        Ok(self.descriptor.clone())
    }

    fn RequestSample(&self, token: Ref<IUnknown>) -> windows_core::Result<()> {
        let mut st = lock(&self.shared.st);
        if st.shutdown || st.queue.is_none() {
            return Err(MF_E_SHUTDOWN.into());
        }
        if st.state != MF_STREAM_STATE_RUNNING {
            return Err(MF_E_MEDIA_SOURCE_WRONGSTATE.into());
        }
        st.pending.push_back(token.cloned());
        self.shared.cv.notify_one();
        Ok(())
    }
}

impl IMFMediaStream2_Impl for MediaStream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> windows_core::Result<()> {
        let current = self.state();
        if current == value {
            return Ok(());
        }
        match value {
            MF_STREAM_STATE_PAUSED if current == MF_STREAM_STATE_RUNNING => {
                lock(&self.shared.st).state = value;
                Ok(())
            }
            MF_STREAM_STATE_RUNNING => self.start(None),
            MF_STREAM_STATE_STOPPED => self.stop(),
            _ => Err(MF_E_INVALID_STATE_TRANSITION.into()),
        }
    }

    fn GetStreamState(&self) -> windows_core::Result<MF_STREAM_STATE> {
        Ok(self.state())
    }
}

impl IKsControl_Impl for MediaStream_Impl {
    fn KsProperty(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut core::ffi::c_void, _dl: u32, ret: *mut u32) -> windows_core::Result<()> {
        crate::media_source::ks_not_found(ret)
    }

    fn KsMethod(&self, _m: *const KSIDENTIFIER, _ml: u32, _d: *mut core::ffi::c_void, _dl: u32, ret: *mut u32) -> windows_core::Result<()> {
        crate::media_source::ks_not_found(ret)
    }

    fn KsEvent(&self, _e: *const KSIDENTIFIER, _el: u32, _d: *mut core::ffi::c_void, _dl: u32, ret: *mut u32) -> windows_core::Result<()> {
        crate::media_source::ks_not_found(ret)
    }
}
