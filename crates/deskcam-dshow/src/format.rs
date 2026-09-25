//! The media types the output pin offers, as DirectShow `AM_MEDIA_TYPE` + `VIDEOINFOHEADER`.
//!
//! Every type has the resolution from `stream.bin` (the running app's output). Consumers pick
//! one of three pixel formats: NV12 (a straight copy of the shared frame), I420 or YUY2.

use std::mem::ManuallyDrop;

use deskcam_proto::{StreamInfo, convert};
use windows::Win32::Foundation::{E_OUTOFMEMORY, RECT};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Media::MediaFoundation::{
    AM_MEDIA_TYPE, FORMAT_VideoInfo, MEDIASUBTYPE_I420, MEDIASUBTYPE_NV12, MEDIASUBTYPE_YUY2, MEDIATYPE_Video,
    VIDEOINFOHEADER,
};
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};
use windows_core::GUID;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Nv12,
    I420,
    Yuy2,
}

/// Offered in this order; NV12 first because it needs no conversion.
pub const FORMATS: [Format; 3] = [Format::Nv12, Format::I420, Format::Yuy2];

impl Format {
    pub fn subtype(self) -> GUID {
        match self {
            Format::Nv12 => MEDIASUBTYPE_NV12,
            Format::I420 => MEDIASUBTYPE_I420,
            Format::Yuy2 => MEDIASUBTYPE_YUY2,
        }
    }

    pub fn from_subtype(subtype: &GUID) -> Option<Format> {
        FORMATS.into_iter().find(|f| f.subtype() == *subtype)
    }

    fn bits(self) -> u16 {
        match self {
            Format::Nv12 | Format::I420 => 12,
            Format::Yuy2 => 16,
        }
    }

    pub fn frame_bytes(self, width: u32, height: u32) -> usize {
        let (w, h) = (width as usize, height as usize);
        match self {
            Format::Nv12 => convert::nv12_size(w, h),
            Format::I420 => convert::i420_size(w, h),
            Format::Yuy2 => convert::yuy2_size(w, h),
        }
    }
}

/// A complete output type: pixel format, size and frame rate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct VideoType {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl VideoType {
    pub fn new(format: Format, info: StreamInfo) -> VideoType {
        VideoType { format, width: info.width, height: info.height, fps: info.fps }
    }

    /// All offered types, `preferred` first.
    pub fn offered(preferred: VideoType) -> Vec<VideoType> {
        let mut list = vec![preferred];
        list.extend(FORMATS.into_iter().filter(|f| *f != preferred.format).map(|format| VideoType { format, ..preferred }));
        list
    }

    pub fn frame_bytes(&self) -> usize {
        self.format.frame_bytes(self.width, self.height)
    }

    /// Frame duration in 100 ns units.
    pub fn interval(&self) -> i64 {
        10_000_000 / self.fps.max(1) as i64
    }

    pub fn bit_rate(&self) -> u32 {
        (self.frame_bytes() as u64 * 8 * self.fps as u64).min(u32::MAX as u64) as u32
    }

    fn header(&self) -> VIDEOINFOHEADER {
        let rect = RECT { left: 0, top: 0, right: self.width as i32, bottom: self.height as i32 };
        VIDEOINFOHEADER {
            rcSource: rect,
            rcTarget: rect,
            dwBitRate: self.bit_rate(),
            dwBitErrorRate: 0,
            AvgTimePerFrame: self.interval(),
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.width as i32,
                // Positive height: YUV layouts are always top-down.
                biHeight: self.height as i32,
                biPlanes: 1,
                biBitCount: self.format.bits(),
                // FOURCC subtypes carry the FOURCC in Data1.
                biCompression: self.format.subtype().data1,
                biSizeImage: self.frame_bytes() as u32,
                ..Default::default()
            },
        }
    }

    /// Fills a caller-owned `AM_MEDIA_TYPE`; its format block is `CoTaskMemAlloc`ed and must be
    /// released with [`free_format_block`] (or by the caller's `FreeMediaType`).
    ///
    /// # Safety
    /// `mt` must be valid for writes; its previous contents are overwritten without being freed.
    pub unsafe fn write_to(&self, mt: *mut AM_MEDIA_TYPE) -> windows_core::Result<()> {
        let block = unsafe { CoTaskMemAlloc(size_of::<VIDEOINFOHEADER>()) } as *mut VIDEOINFOHEADER;
        if block.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        unsafe {
            block.write(self.header());
            mt.write(AM_MEDIA_TYPE {
                majortype: MEDIATYPE_Video,
                subtype: self.format.subtype(),
                bFixedSizeSamples: true.into(),
                bTemporalCompression: false.into(),
                lSampleSize: self.frame_bytes() as u32,
                formattype: FORMAT_VideoInfo,
                pUnk: ManuallyDrop::new(None),
                cbFormat: size_of::<VIDEOINFOHEADER>() as u32,
                pbFormat: block as *mut u8,
            });
        }
        Ok(())
    }

    /// A `CoTaskMemAlloc`ed media type, as returned by `GetFormat`, `GetStreamCaps` and
    /// `IEnumMediaTypes::Next`; the caller frees it (`DeleteMediaType`).
    pub fn alloc(&self) -> windows_core::Result<*mut AM_MEDIA_TYPE> {
        let mt = unsafe { CoTaskMemAlloc(size_of::<AM_MEDIA_TYPE>()) } as *mut AM_MEDIA_TYPE;
        if mt.is_null() {
            return Err(E_OUTOFMEMORY.into());
        }
        if let Err(e) = unsafe { self.write_to(mt) } {
            unsafe { CoTaskMemFree(Some(mt as *const _)) };
            return Err(e);
        }
        Ok(mt)
    }

    /// Parses a fully specified video type from a caller. Returns `None` for anything that is not
    /// one of our pixel formats with a sane `VIDEOINFOHEADER`; `fps` is 0 when unspecified.
    ///
    /// # Safety
    /// `mt` must be null or point to a valid `AM_MEDIA_TYPE` whose format block has `cbFormat` bytes.
    pub unsafe fn parse(mt: *const AM_MEDIA_TYPE) -> Option<VideoType> {
        let mt = unsafe { mt.as_ref() }?;
        if mt.majortype != MEDIATYPE_Video || mt.formattype != FORMAT_VideoInfo {
            return None;
        }
        let format = Format::from_subtype(&mt.subtype)?;
        if mt.pbFormat.is_null() || (mt.cbFormat as usize) < size_of::<VIDEOINFOHEADER>() {
            return None;
        }
        let vih = unsafe { (mt.pbFormat as *const VIDEOINFOHEADER).read_unaligned() };
        let (w, h) = (vih.bmiHeader.biWidth, vih.bmiHeader.biHeight.unsigned_abs());
        if w <= 0 {
            return None;
        }
        let fps = match vih.AvgTimePerFrame {
            t if t > 0 => ((10_000_000 + t / 2) / t).clamp(1, 240) as u32,
            _ => 0,
        };
        Some(VideoType { format, width: w as u32, height: h, fps })
    }

    /// Whether a possibly partial type (as passed to `IPin::Connect`) admits `self`: GUID_NULL
    /// fields are wildcards, and a present `VIDEOINFOHEADER` must match our size.
    ///
    /// # Safety
    /// As for [`VideoType::parse`].
    pub unsafe fn admitted_by(&self, mt: *const AM_MEDIA_TYPE) -> bool {
        let Some(m) = (unsafe { mt.as_ref() }) else { return true };
        let wild = |g: &GUID, want: GUID| *g == GUID::zeroed() || *g == want;
        if !wild(&m.majortype, MEDIATYPE_Video) || !wild(&m.subtype, self.format.subtype()) {
            return false;
        }
        if m.formattype == GUID::zeroed() {
            return true;
        }
        match unsafe { VideoType::parse(mt) } {
            Some(t) => t.format == self.format && t.width == self.width && t.height == self.height,
            None => false,
        }
    }
}

/// Frees a media type's format block and `pUnk` (the `FreeMediaType` helper from strmbase).
///
/// # Safety
/// `mt` must hold a format block allocated with `CoTaskMemAlloc` (or null).
pub unsafe fn free_format_block(mt: &mut AM_MEDIA_TYPE) {
    if !mt.pbFormat.is_null() {
        unsafe { CoTaskMemFree(Some(mt.pbFormat as *const _)) };
        mt.pbFormat = std::ptr::null_mut();
    }
    mt.cbFormat = 0;
    unsafe { ManuallyDrop::drop(&mut mt.pUnk) };
    mt.pUnk = ManuallyDrop::new(None);
}

/// Frees a media type allocated by [`VideoType::alloc`] (`DeleteMediaType`).
///
/// # Safety
/// `mt` must be null or a `CoTaskMemAlloc`ed media type with an owned format block.
pub unsafe fn delete_media_type(mt: *mut AM_MEDIA_TYPE) {
    if let Some(m) = unsafe { mt.as_mut() } {
        unsafe {
            free_format_block(m);
            CoTaskMemFree(Some(mt as *const _));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: StreamInfo = StreamInfo { width: 1280, height: 720, fps: 30 };

    #[test]
    fn round_trip_all_formats() {
        for vt in VideoType::offered(VideoType::new(Format::Yuy2, INFO)) {
            let mt = vt.alloc().unwrap();
            assert_eq!(unsafe { VideoType::parse(mt) }, Some(vt));
            assert!(unsafe { vt.admitted_by(mt) });
            unsafe { delete_media_type(mt) };
        }
    }

    #[test]
    fn offered_puts_preferred_first_without_duplicates() {
        let list = VideoType::offered(VideoType::new(Format::I420, INFO));
        let formats: Vec<Format> = list.iter().map(|t| t.format).collect();
        assert_eq!(formats, [Format::I420, Format::Nv12, Format::Yuy2]);
    }

    #[test]
    fn partial_types() {
        let nv12 = VideoType::new(Format::Nv12, INFO);
        let any = AM_MEDIA_TYPE::default();
        assert!(unsafe { nv12.admitted_by(&any) });
        let yuy2_only = AM_MEDIA_TYPE { majortype: MEDIATYPE_Video, subtype: MEDIASUBTYPE_YUY2, ..Default::default() };
        assert!(!unsafe { nv12.admitted_by(&yuy2_only) });
        let other_size = VideoType { width: 640, height: 480, ..nv12 }.alloc().unwrap();
        assert!(!unsafe { nv12.admitted_by(other_size) });
        unsafe { delete_media_type(other_size) };
    }

    #[test]
    fn sizes() {
        let vt = VideoType::new(Format::Yuy2, INFO);
        assert_eq!(vt.frame_bytes(), 1280 * 720 * 2);
        assert_eq!(vt.interval(), 333_333);
        assert_eq!(Format::Nv12.frame_bytes(1280, 720), 1280 * 720 * 3 / 2);
    }
}
