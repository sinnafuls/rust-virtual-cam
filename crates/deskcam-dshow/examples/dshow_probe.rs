//! Opens a DirectShow camera exactly like a Windows 10 app does (system device enumerator →
//! capture graph → Sample Grabber → Null Renderer) and reports what arrives. Build it for
//! `i686-pc-windows-msvc` too, to test the 32-bit filter.
//!
//! `dshow_probe [--name DeskCam] [--format nv12|i420|yuy2] [--frames 90] [--out probe.bmp]`
//!
//! Must run in the same desktop session as `deskcam.exe` (its frame section is session-local).

#![allow(non_snake_case)] // COM method names

use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use deskcam_dshow::format::{Format, VideoType, delete_media_type, free_format_block};
use windows::Win32::Media::DirectShow::{
    IAMStreamConfig, IBaseFilter, ICaptureGraphBuilder2, ICreateDevEnum, IGraphBuilder, IMediaControl,
};
use windows::Win32::Media::MediaFoundation::{
    AM_MEDIA_TYPE, CLSID_CaptureGraphBuilder2, CLSID_FilterGraph, CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory,
    MEDIATYPE_Video, PIN_CATEGORY_CAPTURE,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, IEnumMoniker, IMoniker,
};
use windows::Win32::System::Variant::VARIANT;
use windows_core::{BOOL, BSTR, ComObject, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, implement, interface, w};

/// qedit.dll, still shipped with Windows 10/11 though its header left the SDK.
const CLSID_SAMPLE_GRABBER: GUID = GUID::from_u128(0xc1f400a0_3f08_11d3_9f0b_006008039e37);
const CLSID_NULL_RENDERER: GUID = GUID::from_u128(0xc1f400a4_3f08_11d3_9f0b_006008039e37);

#[interface("6b652fff-11fe-4fce-92ad-0266b5d7c78f")]
unsafe trait ISampleGrabber: IUnknown {
    fn SetOneShot(&self, one_shot: BOOL) -> HRESULT;
    fn SetMediaType(&self, mt: *const AM_MEDIA_TYPE) -> HRESULT;
    fn GetConnectedMediaType(&self, mt: *mut AM_MEDIA_TYPE) -> HRESULT;
    fn SetBufferSamples(&self, buffer: BOOL) -> HRESULT;
    fn GetCurrentBuffer(&self, size: *mut i32, buffer: *mut i32) -> HRESULT;
    fn GetCurrentSample(&self, sample: *mut *mut c_void) -> HRESULT;
    fn SetCallback(&self, callback: *mut c_void, which: i32) -> HRESULT;
}

#[interface("0579154a-2b53-4994-b0d0-e773148eff85")]
unsafe trait ISampleGrabberCB: IUnknown {
    fn SampleCB(&self, time: f64, sample: *mut c_void) -> HRESULT;
    fn BufferCB(&self, time: f64, buffer: *mut u8, len: i32) -> HRESULT;
}

#[derive(Default)]
struct Received {
    count: u32,
    first: Option<Instant>,
    last: Option<Instant>,
    frame: Vec<u8>,
}

#[implement(ISampleGrabberCB)]
struct Sink(Mutex<Received>);

impl Sink {
    fn received(&self) -> MutexGuard<'_, Received> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ISampleGrabberCB_Impl for Sink_Impl {
    unsafe fn SampleCB(&self, _time: f64, _sample: *mut c_void) -> HRESULT {
        HRESULT(0)
    }

    unsafe fn BufferCB(&self, _time: f64, buffer: *mut u8, len: i32) -> HRESULT {
        let mut r = self.received();
        let now = Instant::now();
        r.first.get_or_insert(now);
        r.last = Some(now);
        r.count += 1;
        if !buffer.is_null() && len > 0 {
            r.frame.clear();
            r.frame.extend_from_slice(unsafe { std::slice::from_raw_parts(buffer, len as usize) });
        }
        HRESULT(0)
    }
}

struct Args {
    name: String,
    format: Format,
    frames: u32,
    out: String,
}

fn parse_args() -> Args {
    let mut a = Args { name: "DeskCam".into(), format: Format::Nv12, frames: 90, out: "probe.bmp".into() };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => a.name = it.next().expect("--name value"),
            "--frames" => a.frames = it.next().and_then(|v| v.parse().ok()).expect("--frames number"),
            "--out" => a.out = it.next().expect("--out path"),
            "--format" => {
                a.format = match it.next().as_deref() {
                    Some("nv12") => Format::Nv12,
                    Some("i420") => Format::I420,
                    Some("yuy2") => Format::Yuy2,
                    other => panic!("--format must be nv12, i420 or yuy2, got {other:?}"),
                }
            }
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// All video capture devices as (friendly name, moniker).
fn devices() -> windows_core::Result<Vec<(String, IMoniker)>> {
    let dev_enum: ICreateDevEnum = unsafe { CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)? };
    let mut monikers: Option<IEnumMoniker> = None;
    unsafe { dev_enum.CreateClassEnumerator(&CLSID_VideoInputDeviceCategory, &mut monikers, 0)? };
    let Some(monikers) = monikers else { return Ok(Vec::new()) }; // S_FALSE: category empty
    let mut out = Vec::new();
    loop {
        let mut next = [None];
        if unsafe { monikers.Next(&mut next, None) } != windows::Win32::Foundation::S_OK {
            break;
        }
        let Some(moniker) = next[0].take() else { break };
        let bag: IPropertyBag = unsafe { moniker.BindToStorage(None, None)? };
        let mut name = VARIANT::default();
        let name = match unsafe { bag.Read(w!("FriendlyName"), &mut name, None) } {
            Ok(()) => BSTR::try_from(&name).map(|b| b.to_string()).unwrap_or_default(),
            Err(_) => String::new(),
        };
        out.push((name, moniker));
    }
    Ok(out)
}

/// NV12 or I420 frame to a top-down 32-bpp BMP.
fn write_bmp(path: &str, vt: &VideoType, frame: &[u8]) -> std::io::Result<()> {
    let (w, h) = (vt.width as usize, vt.height as usize);
    let mut bgrx = vec![0u8; w * h * 4];
    let mut uv = vec![0u8; w];
    for r in 0..h {
        let y = &frame[r * w..][..w];
        let chroma: &[u8] = match vt.format {
            Format::Nv12 => &frame[w * h + (r / 2) * w..][..w],
            _ => {
                let (u_plane, v_plane) = (w * h, w * h + (w / 2) * (h / 2));
                for x in 0..w / 2 {
                    uv[2 * x] = frame[u_plane + (r / 2) * (w / 2) + x];
                    uv[2 * x + 1] = frame[v_plane + (r / 2) * (w / 2) + x];
                }
                &uv
            }
        };
        deskcam_proto::color::nv12_row_to_bgrx(y, chroma, &mut bgrx[r * w * 4..][..w * 4]);
    }
    let size = bgrx.len();
    let mut out = Vec::with_capacity(54 + size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + size) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    out.extend_from_slice(&bgrx);
    std::fs::write(path, out)
}

fn main() -> windows_core::Result<()> {
    let args = parse_args();
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    println!("probe: {}-bit process", usize::BITS);

    let all = devices()?;
    for (name, _) in &all {
        println!("camera: {name}");
    }
    let Some((_, moniker)) = all.iter().find(|(n, _)| n.starts_with(&args.name)) else {
        eprintln!("no camera starting with '{}'", args.name);
        std::process::exit(1);
    };
    let source: IBaseFilter = unsafe { moniker.BindToObject(None, None)? };

    let graph: IGraphBuilder = unsafe { CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER)? };
    let builder: ICaptureGraphBuilder2 = unsafe { CoCreateInstance(&CLSID_CaptureGraphBuilder2, None, CLSCTX_INPROC_SERVER)? };
    let grabber_filter: IBaseFilter = unsafe { CoCreateInstance(&CLSID_SAMPLE_GRABBER, None, CLSCTX_INPROC_SERVER)? };
    let renderer: IBaseFilter = unsafe { CoCreateInstance(&CLSID_NULL_RENDERER, None, CLSCTX_INPROC_SERVER)? };
    let grabber: ISampleGrabber = grabber_filter.cast()?;
    let sink = ComObject::new(Sink(Mutex::new(Received::default())));
    let sink_cb: ISampleGrabberCB = sink.to_interface();
    unsafe {
        builder.SetFiltergraph(&graph)?;
        graph.AddFilter(&source, w!("Camera"))?;
        graph.AddFilter(&grabber_filter, w!("Sample Grabber"))?;
        graph.AddFilter(&renderer, w!("Null Renderer"))?;
    }

    // Pick the format through IAMStreamConfig, as apps do before connecting.
    let mut config: *mut c_void = std::ptr::null_mut();
    unsafe {
        builder.FindInterface(Some(&PIN_CATEGORY_CAPTURE), Some(&MEDIATYPE_Video), &source, &IAMStreamConfig::IID, &mut config)?;
    }
    let config = unsafe { IAMStreamConfig::from_raw(config) };
    let current = unsafe { config.GetFormat()? };
    let mut vt = unsafe { VideoType::parse(current) }.expect("camera reports a video type");
    unsafe { delete_media_type(current) };
    vt.format = args.format;
    let wanted = vt.alloc()?;
    unsafe {
        config.SetFormat(wanted)?;
        grabber.SetMediaType(wanted).ok()?;
        delete_media_type(wanted);
        grabber.SetBufferSamples(false.into()).ok()?;
        grabber.SetCallback(sink_cb.as_raw(), 1).ok()?; // 1 = BufferCB
        builder.RenderStream(Some(&PIN_CATEGORY_CAPTURE), &MEDIATYPE_Video, &source, &grabber_filter, &renderer)?;
    }
    let mut connected = AM_MEDIA_TYPE::default();
    unsafe { grabber.GetConnectedMediaType(&mut connected).ok()? };
    let vt = unsafe { VideoType::parse(&connected) }.expect("connected type is video");
    unsafe { free_format_block(&mut connected) };
    println!("negotiated {:?} {}x{} @ {} fps", vt.format, vt.width, vt.height, vt.fps);

    let control: IMediaControl = graph.cast()?;
    unsafe { control.Run()? };
    let deadline = Instant::now() + Duration::from_secs_f64(args.frames as f64 / vt.fps.max(1) as f64 + 10.0);
    let received = loop {
        std::thread::sleep(Duration::from_millis(100));
        if sink.received().count >= args.frames || Instant::now() > deadline {
            unsafe { control.Stop()? };
            break std::mem::take(&mut *sink.received());
        }
    };

    let fps = match (received.first, received.last) {
        (Some(a), Some(b)) if received.count > 1 => (received.count - 1) as f64 / (b - a).as_secs_f64(),
        _ => 0.0,
    };
    println!("received {} frames, measured {fps:.1} fps", received.count);
    let pixels = (vt.width * vt.height) as usize;
    if received.frame.len() >= vt.frame_bytes() {
        let luma: Vec<u8> = match vt.format {
            Format::Yuy2 => received.frame.iter().step_by(2).take(pixels).copied().collect(),
            _ => received.frame[..pixels].to_vec(),
        };
        let mean = luma.iter().map(|&v| v as u64).sum::<u64>() as f64 / pixels as f64;
        let (min, max) = (luma.iter().min().unwrap(), luma.iter().max().unwrap());
        println!("last frame luma: mean {mean:.1} min {min} max {max}");
        if vt.format != Format::Yuy2 {
            write_bmp(&args.out, &vt, &received.frame).expect("write bmp");
            println!("wrote {}", args.out);
        }
    }
    std::process::exit(if received.count >= args.frames { 0 } else { 2 });
}
