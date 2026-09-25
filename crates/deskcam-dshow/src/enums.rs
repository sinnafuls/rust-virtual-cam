//! `IEnumPins` / `IEnumMediaTypes` cursors over fixed lists.

use std::sync::atomic::{AtomicUsize, Ordering};

use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, S_FALSE, S_OK};
use windows::Win32::Media::DirectShow::{IEnumMediaTypes, IEnumMediaTypes_Impl, IEnumPins, IEnumPins_Impl, IPin};
use windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE;
use windows_core::{HRESULT, implement};

use crate::ObjGuard;
use crate::format::{VideoType, delete_media_type};

/// Shared `Next` logic: validates arguments, then emits up to `count` items from `pos`.
fn next(
    pos: &AtomicUsize,
    len: usize,
    count: u32,
    fetched: *mut u32,
    mut emit: impl FnMut(usize, usize) -> windows_core::Result<()>,
) -> HRESULT {
    if count > 1 && fetched.is_null() {
        return E_INVALIDARG;
    }
    let mut n = 0usize;
    while n < count as usize {
        let i = pos.load(Ordering::Relaxed);
        if i >= len {
            break;
        }
        if let Err(e) = emit(n, i) {
            return e.code();
        }
        pos.store(i + 1, Ordering::Relaxed);
        n += 1;
    }
    if !fetched.is_null() {
        unsafe { *fetched = n as u32 };
    }
    if n == count as usize { S_OK } else { S_FALSE }
}

fn skip(pos: &AtomicUsize, len: usize, count: u32) -> windows_core::Result<()> {
    let target = pos.load(Ordering::Relaxed) + count as usize;
    pos.store(target.min(len), Ordering::Relaxed);
    if target <= len { Ok(()) } else { Err(windows_core::Error::from_hresult(S_FALSE)) }
}

#[implement(IEnumPins)]
pub struct EnumPins {
    pin: IPin,
    pos: AtomicUsize,
    _guard: ObjGuard,
}

impl EnumPins {
    pub fn new(pin: IPin) -> EnumPins {
        EnumPins { pin, pos: AtomicUsize::new(0), _guard: ObjGuard::new() }
    }
}

impl IEnumPins_Impl for EnumPins_Impl {
    fn Next(&self, count: u32, pins: *mut Option<IPin>, fetched: *mut u32) -> HRESULT {
        if pins.is_null() {
            return E_POINTER;
        }
        next(&self.pos, 1, count, fetched, |n, _| {
            unsafe { pins.add(n).write(Some(self.pin.clone())) };
            Ok(())
        })
    }

    fn Skip(&self, count: u32) -> windows_core::Result<()> {
        skip(&self.pos, 1, count)
    }

    fn Reset(&self) -> windows_core::Result<()> {
        self.pos.store(0, Ordering::Relaxed);
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IEnumPins> {
        let copy = EnumPins::new(self.pin.clone());
        copy.pos.store(self.pos.load(Ordering::Relaxed), Ordering::Relaxed);
        Ok(copy.into())
    }
}

#[implement(IEnumMediaTypes)]
pub struct EnumMediaTypes {
    types: Vec<VideoType>,
    pos: AtomicUsize,
    _guard: ObjGuard,
}

impl EnumMediaTypes {
    pub fn new(types: Vec<VideoType>) -> EnumMediaTypes {
        EnumMediaTypes { types, pos: AtomicUsize::new(0), _guard: ObjGuard::new() }
    }
}

impl IEnumMediaTypes_Impl for EnumMediaTypes_Impl {
    fn Next(&self, count: u32, out: *mut *mut AM_MEDIA_TYPE, fetched: *mut u32) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        let mut written = 0usize;
        let hr = next(&self.pos, self.types.len(), count, fetched, |n, i| {
            unsafe { out.add(n).write(self.types[i].alloc()?) };
            written = n + 1;
            Ok(())
        });
        if hr.is_err() {
            // Nothing is handed out on failure.
            for n in 0..written {
                unsafe { delete_media_type(*out.add(n)) };
            }
        }
        hr
    }

    fn Skip(&self, count: u32) -> windows_core::Result<()> {
        skip(&self.pos, self.types.len(), count)
    }

    fn Reset(&self) -> windows_core::Result<()> {
        self.pos.store(0, Ordering::Relaxed);
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IEnumMediaTypes> {
        let copy = EnumMediaTypes::new(self.types.clone());
        copy.pos.store(self.pos.load(Ordering::Relaxed), Ordering::Relaxed);
        Ok(copy.into())
    }
}
