//! Layout of the shared frame section and its lock-free seqlock ring.
//!
//! On Windows 11 the section is created by the media source inside the Frame Server (session 0,
//! where creating `Global\` objects needs no privilege) and opened by the app for writing. On
//! Windows 10 the app creates a `Local\` section itself and the DirectShow filter, loaded inside
//! each consumer process of the same session, opens it.
//! Every header field may be written by any Authenticated User, so readers bound-check
//! everything and never trust sizes from the header.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};

pub const SLOT_COUNT: usize = 3;
pub const HEADER_SIZE: usize = 4096;
pub const SECTION_MAGIC: u32 = 0x4D43_4B44; // b"DKCM"
pub const SECTION_VERSION: u32 = 1;
pub const NO_SLOT: u32 = u32::MAX;
/// The app captures only while a reader delivered a sample within this window.
pub const READER_ACTIVE_MS: u64 = 1500;
/// The media source serves black when the app has not touched the ring within this window.
pub const WRITER_ALIVE_MS: u64 = 2000;

const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_WIDTH: usize = 8;
const OFF_HEIGHT: usize = 12;
const OFF_LATEST: usize = 16;
const OFF_WRITER_HB: usize = 32;
const OFF_READER_HB: usize = 40;
const OFF_SEQ: usize = 64;

/// Kernel object namespace of the frame section.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Namespace {
    /// Windows 11: created by the media source inside the Frame Server (session 0).
    Global,
    /// Windows 10: created by the app in the user's session, where DirectShow consumers run.
    Local,
}

pub fn section_name(ns: Namespace, width: u32, height: u32) -> String {
    let prefix = match ns {
        Namespace::Global => "Global",
        Namespace::Local => "Local",
    };
    format!("{prefix}\\DeskCam-v1-{width}x{height}")
}

/// NV12 frame size in bytes.
pub fn frame_size(width: u32, height: u32) -> usize {
    width as usize * height as usize * 3 / 2
}

pub fn section_size(width: u32, height: u32) -> usize {
    HEADER_SIZE + SLOT_COUNT * frame_size(width, height)
}

/// Milliseconds since boot; consistent across processes.
pub fn now_ms() -> u64 {
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadOutcome {
    Frame,
    NoFrame,
    WriterGone,
}

/// View over a mapped section of [`section_size`] bytes.
pub struct FrameRing {
    base: *mut u8,
    width: u32,
    height: u32,
    writable: bool,
}

unsafe impl Send for FrameRing {}
unsafe impl Sync for FrameRing {}

impl FrameRing {
    /// # Safety
    /// `base` must be 8-byte aligned and valid for `section_size(width, height)` bytes for
    /// the lifetime of the ring; writes require `writable`.
    pub unsafe fn from_raw(base: *mut u8, width: u32, height: u32, writable: bool) -> Self {
        Self { base, width, height, writable }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    fn u32_at(&self, off: usize) -> &AtomicU32 {
        unsafe { &*(self.base.add(off) as *const AtomicU32) }
    }

    fn u64_at(&self, off: usize) -> &AtomicU64 {
        unsafe { &*(self.base.add(off) as *const AtomicU64) }
    }

    fn seq(&self, slot: usize) -> &AtomicU64 {
        self.u64_at(OFF_SEQ + slot * 8)
    }

    fn slot_ptr(&self, slot: usize) -> *mut u8 {
        unsafe { self.base.add(HEADER_SIZE + slot * frame_size(self.width, self.height)) }
    }

    /// Initializes a freshly created (zeroed) section. Creator only.
    pub fn init_header(&self) {
        self.u32_at(OFF_WIDTH).store(self.width, Ordering::Relaxed);
        self.u32_at(OFF_HEIGHT).store(self.height, Ordering::Relaxed);
        self.u32_at(OFF_LATEST).store(NO_SLOT, Ordering::Relaxed);
        self.u64_at(OFF_WRITER_HB).store(0, Ordering::Relaxed);
        self.u64_at(OFF_READER_HB).store(0, Ordering::Relaxed);
        for slot in 0..SLOT_COUNT {
            self.seq(slot).store(0, Ordering::Relaxed);
        }
        self.u32_at(OFF_VERSION).store(SECTION_VERSION, Ordering::Relaxed);
        self.u32_at(OFF_MAGIC).store(SECTION_MAGIC, Ordering::Release);
    }

    pub fn header_valid(&self) -> bool {
        self.u32_at(OFF_MAGIC).load(Ordering::Acquire) == SECTION_MAGIC
            && self.u32_at(OFF_VERSION).load(Ordering::Relaxed) == SECTION_VERSION
            && self.u32_at(OFF_WIDTH).load(Ordering::Relaxed) == self.width
            && self.u32_at(OFF_HEIGHT).load(Ordering::Relaxed) == self.height
    }

    pub fn touch_writer(&self, now_ms: u64) {
        if self.writable {
            self.u64_at(OFF_WRITER_HB).store(now_ms, Ordering::Release);
        }
    }

    pub fn touch_reader(&self, now_ms: u64) {
        if self.writable {
            self.u64_at(OFF_READER_HB).store(now_ms, Ordering::Release);
        }
    }

    pub fn reader_active(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.u64_at(OFF_READER_HB).load(Ordering::Acquire)) < READER_ACTIVE_MS
    }

    pub fn writer_alive(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.u64_at(OFF_WRITER_HB).load(Ordering::Acquire)) < WRITER_ALIVE_MS
    }

    /// Drops the published frame so no later reader can see stale desktop content.
    pub fn invalidate(&self) {
        if self.writable {
            self.u32_at(OFF_LATEST).store(NO_SLOT, Ordering::Release);
        }
    }

    /// Writer: fills the slot after the latest one and publishes it.
    pub fn publish(&self, fill: impl FnOnce(&mut [u8])) {
        debug_assert!(self.writable);
        let latest = self.u32_at(OFF_LATEST).load(Ordering::Acquire);
        let slot = if (latest as usize) < SLOT_COUNT { (latest as usize + 1) % SLOT_COUNT } else { 0 };
        let seq = self.seq(slot);
        let s = seq.load(Ordering::Relaxed) & !1;
        seq.store(s + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        let bytes = unsafe {
            std::slice::from_raw_parts_mut(self.slot_ptr(slot), frame_size(self.width, self.height))
        };
        fill(bytes);
        seq.store(s + 2, Ordering::Release);
        self.u32_at(OFF_LATEST).store(slot as u32, Ordering::Release);
    }

    /// Reader: hands the latest published frame to `copy`; `copy` may run up to 3 times when
    /// the writer laps the reader. A frame still torn after 3 attempts is accepted.
    pub fn read_latest(&self, now_ms: u64, mut copy: impl FnMut(&[u8])) -> ReadOutcome {
        if !self.writer_alive(now_ms) {
            return ReadOutcome::WriterGone;
        }
        let len = frame_size(self.width, self.height);
        for attempt in 0..3 {
            let slot = self.u32_at(OFF_LATEST).load(Ordering::Acquire) as usize;
            if slot >= SLOT_COUNT {
                return ReadOutcome::NoFrame;
            }
            let seq = self.seq(slot);
            let s1 = seq.load(Ordering::Acquire);
            if s1 & 1 == 1 && attempt < 2 {
                std::hint::spin_loop();
                continue;
            }
            copy(unsafe { std::slice::from_raw_parts(self.slot_ptr(slot), len) });
            fence(Ordering::Acquire);
            if seq.load(Ordering::Relaxed) == s1 || attempt == 2 {
                return ReadOutcome::Frame;
            }
        }
        ReadOutcome::Frame
    }

    #[cfg(test)]
    fn latest_slot(&self) -> u32 {
        self.u32_at(OFF_LATEST).load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    struct Buf(Vec<u64>);

    fn ring(buf: &mut Buf, w: u32, h: u32) -> FrameRing {
        unsafe { FrameRing::from_raw(buf.0.as_mut_ptr() as *mut u8, w, h, true) }
    }

    fn alloc(w: u32, h: u32) -> Buf {
        Buf(vec![0u64; section_size(w, h).div_ceil(8)])
    }

    #[test]
    fn header_and_heartbeats() {
        let mut buf = alloc(64, 32);
        let r = ring(&mut buf, 64, 32);
        assert!(!r.header_valid());
        r.init_header();
        assert!(r.header_valid());
        let other = unsafe { FrameRing::from_raw(buf.0.as_mut_ptr() as *mut u8, 64, 34, false) };
        assert!(!other.header_valid());
        assert_eq!(r.read_latest(10_000, |_| {}), ReadOutcome::WriterGone);
        r.touch_writer(10_000);
        assert_eq!(r.read_latest(10_500, |_| {}), ReadOutcome::NoFrame);
        assert!(!r.reader_active(10_000));
        r.touch_reader(10_000);
        assert!(r.reader_active(11_000));
        assert!(!r.reader_active(11_600));
        r.publish(|b| b.fill(7));
        let mut seen = 0;
        assert_eq!(r.read_latest(10_100, |b| seen = b[0]), ReadOutcome::Frame);
        assert_eq!(seen, 7);
        r.invalidate();
        assert_eq!(r.read_latest(10_100, |_| {}), ReadOutcome::NoFrame);
    }

    #[test]
    fn publish_never_writes_latest_slot() {
        let mut buf = alloc(16, 16);
        let r = ring(&mut buf, 16, 16);
        r.init_header();
        let slot_of = |p: *const u8| (p as usize - r.base as usize - HEADER_SIZE) / frame_size(16, 16);
        for i in 0..10u8 {
            let before = r.latest_slot();
            let mut written = usize::MAX;
            r.publish(|b| {
                b.fill(i);
                written = slot_of(b.as_ptr());
            });
            assert_ne!(written as u32, before);
            assert_eq!(r.latest_slot(), written as u32);
        }
    }

    #[test]
    fn seqlock_hammer() {
        let (w, h) = (64u32, 64u32);
        let mut buf = alloc(w, h);
        let base = buf.0.as_mut_ptr() as usize;
        let writer = unsafe { FrameRing::from_raw(base as *mut u8, w, h, true) };
        writer.init_header();
        writer.touch_writer(1);
        let done = Arc::new(AtomicBool::new(false));
        let done_w = done.clone();
        let t = std::thread::spawn(move || {
            let r = unsafe { FrameRing::from_raw(base as *mut u8, w, h, true) };
            for i in 0..20_000u32 {
                r.publish(|b| b.fill(i as u8));
            }
            done_w.store(true, Ordering::Release);
        });
        let reader = unsafe { FrameRing::from_raw(base as *mut u8, w, h, false) };
        let (mut frames, mut torn) = (0u64, 0u64);
        let mut copy = vec![0u8; frame_size(w, h)];
        while !done.load(Ordering::Acquire) {
            if reader.read_latest(2, |b| copy.copy_from_slice(b)) == ReadOutcome::Frame {
                frames += 1;
                if copy.iter().any(|&v| v != copy[0]) {
                    torn += 1;
                }
            }
        }
        t.join().unwrap();
        drop(buf);
        assert!(frames > 0);
        assert!(torn * 1000 <= frames, "torn {torn} of {frames}");
    }
}
