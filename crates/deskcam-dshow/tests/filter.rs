//! Drives the filter the way DirectShow apps do, without registering anything: pin discovery,
//! capabilities, and a running graph into the stock Null Renderer.
//!
//! Without `deskcam.exe` running the filter streams black frames, which is enough to exercise
//! connection, allocator negotiation, state changes and delivery pacing.

use std::time::Duration;

use deskcam_dshow::filter;
use deskcam_dshow::format::{Format, VideoType, delete_media_type};
use deskcam_proto::StreamInfo;
use windows::Win32::Media::DirectShow::*;
use windows::Win32::Media::KernelStreaming::IKsPropertySet;
use windows::Win32::Media::MediaFoundation::{
    AMPROPSETID_Pin, CLSID_CaptureGraphBuilder2, CLSID_FilterGraph, MEDIATYPE_Video, PIN_CATEGORY_CAPTURE,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows_core::{GUID, IUnknown, Interface, w};

/// qedit.dll Null Renderer.
const CLSID_NULL_RENDERER: GUID = GUID::from_u128(0xc1f400a4_3f08_11d3_9f0b_006008039e37);
const INFO: StreamInfo = StreamInfo { width: 1280, height: 720, fps: 30 };

fn com() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

fn only_pin(filter: &IBaseFilter) -> IPin {
    let pins = unsafe { filter.EnumPins().unwrap() };
    let mut out = [None, None];
    let mut n = 0;
    let _ = unsafe { pins.Next(&mut out, Some(&mut n)) };
    assert_eq!(n, 1, "exactly one pin");
    out[0].take().unwrap()
}

#[test]
fn exposes_one_capture_pin_with_three_formats() {
    com();
    let filter: IBaseFilter = filter::create(INFO).unwrap().to_interface();
    let pin = only_pin(&filter);
    assert_eq!(unsafe { pin.QueryDirection().unwrap() }, PINDIR_OUTPUT);
    assert_eq!(unsafe { filter.FindPin(w!("Capture")).unwrap() }, pin);

    let mut info = PIN_INFO::default();
    unsafe { pin.QueryPinInfo(&mut info).unwrap() };
    let owner = std::mem::ManuallyDrop::into_inner(info.pFilter).unwrap();
    assert_eq!(owner.cast::<IUnknown>().unwrap(), filter.cast::<IUnknown>().unwrap());

    let ks: IKsPropertySet = pin.cast().unwrap();
    let (mut category, mut returned) = (GUID::zeroed(), 0u32);
    unsafe {
        ks.Get(
            &AMPROPSETID_Pin,
            AMPROPERTY_PIN_CATEGORY.0 as u32,
            std::ptr::null(),
            0,
            &mut category as *mut GUID as *mut _,
            size_of::<GUID>() as u32,
            &mut returned,
        )
        .unwrap()
    };
    assert_eq!(category, PIN_CATEGORY_CAPTURE);

    let config: IAMStreamConfig = pin.cast().unwrap();
    let (mut count, mut size) = (0, 0);
    unsafe { config.GetNumberOfCapabilities(&mut count, &mut size).unwrap() };
    assert_eq!((count, size as usize), (3, size_of::<VIDEO_STREAM_CONFIG_CAPS>()));
    for (i, format) in [Format::Nv12, Format::I420, Format::Yuy2].into_iter().enumerate() {
        let mut mt = std::ptr::null_mut();
        let mut caps = VIDEO_STREAM_CONFIG_CAPS::default();
        unsafe { config.GetStreamCaps(i as i32, &mut mt, &mut caps as *mut _ as *mut u8).unwrap() };
        assert_eq!(unsafe { VideoType::parse(mt) }, Some(VideoType::new(format, INFO)));
        assert_eq!((caps.MaxOutputSize.cx, caps.MaxOutputSize.cy), (1280, 720));
        unsafe { delete_media_type(mt) };
    }

    // Choosing YUY2 makes it the connection type and the first enumerated type.
    let yuy2 = VideoType::new(Format::Yuy2, INFO).alloc().unwrap();
    unsafe { config.SetFormat(yuy2).unwrap() };
    unsafe { delete_media_type(yuy2) };
    let current = unsafe { config.GetFormat().unwrap() };
    assert_eq!(unsafe { VideoType::parse(current) }.map(|t| t.format), Some(Format::Yuy2));
    unsafe { delete_media_type(current) };

    let wrong_size = VideoType { width: 640, height: 480, ..VideoType::new(Format::Nv12, INFO) }.alloc().unwrap();
    assert!(unsafe { config.SetFormat(wrong_size) }.is_err());
    unsafe { delete_media_type(wrong_size) };
}

#[test]
fn streams_into_null_renderer_and_stops_cleanly() {
    com();
    let object = filter::create(INFO).unwrap();
    let filter: IBaseFilter = object.to_interface();
    let renderer: IBaseFilter = match unsafe { CoCreateInstance(&CLSID_NULL_RENDERER, None, CLSCTX_INPROC_SERVER) } {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping: Null Renderer (qedit.dll) unavailable: {e}");
            return;
        }
    };
    let graph: IGraphBuilder = unsafe { CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER).unwrap() };
    let builder: ICaptureGraphBuilder2 =
        unsafe { CoCreateInstance(&CLSID_CaptureGraphBuilder2, None, CLSCTX_INPROC_SERVER).unwrap() };
    unsafe {
        graph.AddFilter(&filter, w!("DeskCam")).unwrap();
        graph.AddFilter(&renderer, w!("Null Renderer")).unwrap();
        builder.SetFiltergraph(&graph).unwrap();
        builder
            .RenderStream(Some(&PIN_CATEGORY_CAPTURE), &MEDIATYPE_Video, &filter, None::<&IBaseFilter>, &renderer)
            .unwrap();
    }
    assert!(unsafe { only_pin(&filter).ConnectedTo() }.is_ok());

    let control: IMediaControl = graph.cast().unwrap();
    unsafe { control.Run().unwrap() };
    std::thread::sleep(Duration::from_secs(1));
    unsafe { control.Stop().unwrap() };

    let frames = object.frames_delivered();
    assert!((15..=40).contains(&frames), "expected ~30 frames in 1 s at 30 fps, got {frames}");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(object.frames_delivered(), frames, "no delivery after Stop");
}
