//! Drives the media source the way the Frame Server does (activator → source → descriptor)
//! without registering anything.

use deskcam_proto::StreamInfo;
use windows::Win32::Media::MediaFoundation::*;
use windows_core::Interface;

fn frame_rate(t: &IMFMediaType) -> (u64, u64) {
    let r = unsafe { t.GetUINT64(&MF_MT_FRAME_RATE).unwrap() };
    (r >> 32, r & 0xFFFF_FFFF)
}

#[test]
fn activator_exposes_configured_stream() {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap() };
    let act = deskcam_source::activator::create(StreamInfo { width: 1280, height: 720, fps: 60 }).unwrap();
    let source: IMFMediaSource = unsafe { act.ActivateObject().unwrap() };

    let pd = unsafe { source.CreatePresentationDescriptor().unwrap() };
    assert_eq!(unsafe { pd.GetStreamDescriptorCount().unwrap() }, 1);
    let mut selected = Default::default();
    let mut desc = None;
    unsafe { pd.GetStreamDescriptorByIndex(0, &mut selected, &mut desc).unwrap() };
    let handler = unsafe { desc.unwrap().GetMediaTypeHandler().unwrap() };
    assert_eq!(unsafe { handler.GetMediaTypeCount().unwrap() }, 4);

    let first = unsafe { handler.GetMediaTypeByIndex(0).unwrap() };
    assert_eq!(unsafe { first.GetGUID(&MF_MT_SUBTYPE).unwrap() }, MFVideoFormat_NV12);
    assert_eq!(unsafe { first.GetUINT64(&MF_MT_FRAME_SIZE).unwrap() }, (1280u64 << 32) | 720);
    assert_eq!(frame_rate(&first), (60, 1));
    let third = unsafe { handler.GetMediaTypeByIndex(2).unwrap() };
    assert_eq!(frame_rate(&third), (30, 1));

    let attrs = unsafe { source.cast::<IMFMediaSourceEx>().unwrap().GetSourceAttributes().unwrap() };
    assert!(unsafe { attrs.GetItem(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION, None) }.is_ok());
    assert_eq!(unsafe { attrs.GetUINT32(&MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES).unwrap() }, 1);

    unsafe { source.Shutdown().unwrap() };
    assert_eq!(unsafe { source.Shutdown() }.unwrap_err().code(), MF_E_SHUTDOWN);
    unsafe { act.DetachObject().unwrap() };
}

fn next_event(generator: &IMFMediaEventGenerator) -> IMFMediaEvent {
    unsafe { generator.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0)).unwrap() }
}

#[test]
fn started_stream_delivers_paced_black_nv12_without_writer() {
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows_core::{GUID, IUnknown};

    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap() };
    let act = deskcam_source::activator::create(StreamInfo { width: 320, height: 180, fps: 30 }).unwrap();
    let source: IMFMediaSource = unsafe { act.ActivateObject().unwrap() };
    let pd = unsafe { source.CreatePresentationDescriptor().unwrap() };
    unsafe {
        pd.SelectStream(0).unwrap();
        source.Start(&pd, &GUID::zeroed(), &PROPVARIANT::default()).unwrap();
    }

    let new_stream = next_event(&source);
    assert_eq!(unsafe { new_stream.GetType().unwrap() }, MENewStream.0 as u32);
    let value = unsafe { new_stream.GetValue().unwrap() };
    let stream: IMFMediaStream = IUnknown::try_from(&value).unwrap().cast().unwrap();
    assert_eq!(unsafe { next_event(&source).GetType().unwrap() }, MESourceStarted.0 as u32);
    assert_eq!(unsafe { next_event(&stream).GetType().unwrap() }, MEStreamStarted.0 as u32);

    let begin = std::time::Instant::now();
    for _ in 0..10 {
        unsafe { stream.RequestSample(None).unwrap() };
    }
    let mut last = None;
    for _ in 0..10 {
        let ev = next_event(&stream);
        assert_eq!(unsafe { ev.GetType().unwrap() }, MEMediaSample.0 as u32);
        last = Some(unsafe { ev.GetValue().unwrap() });
    }
    let elapsed = begin.elapsed();
    // 10 samples at 30 fps are paced over ~9 intervals, not delivered in a burst.
    assert!(elapsed >= std::time::Duration::from_millis(250), "delivered too fast: {elapsed:?}");

    let sample: IMFSample = IUnknown::try_from(&last.unwrap()).unwrap().cast().unwrap();
    let buffer = unsafe { sample.ConvertToContiguousBuffer().unwrap() };
    let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
    unsafe { buffer.Lock(&mut data, None, Some(&mut len)).unwrap() };
    let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
    assert!(bytes.len() >= 320 * 180 * 3 / 2);
    assert!(bytes[..320 * 180].iter().all(|&y| y == 16), "luma not black");
    unsafe { buffer.Unlock().unwrap() };

    unsafe { source.Shutdown().unwrap() };
}

fn process_thread_count() -> usize {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let pid = std::process::id();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).unwrap();
        let mut entry = THREADENTRY32 { dwSize: size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut count = 0;
        let mut ok = Thread32First(snap, &mut entry).is_ok();
        while ok {
            if entry.th32OwnerProcessID == pid {
                count += 1;
            }
            ok = Thread32Next(snap, &mut entry).is_ok();
        }
        let _ = windows::Win32::Foundation::CloseHandle(snap);
        count
    }
}

/// The Frame Server and its monitor create activators they never activate (enumeration,
/// attribute queries). Releasing them, or shutting them down after activation, must not leave
/// delivery threads behind in those long-lived service processes.
#[test]
fn released_activators_leave_no_threads() {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap() };
    let info = StreamInfo { width: 320, height: 180, fps: 30 };
    // Warm up MF's own worker threads before taking the baseline.
    drop(deskcam_source::activator::create(info).unwrap());
    std::thread::sleep(std::time::Duration::from_millis(200));
    let before = process_thread_count();
    for _ in 0..8 {
        drop(deskcam_source::activator::create(info).unwrap());
    }
    for _ in 0..8 {
        let act = deskcam_source::activator::create(info).unwrap();
        let _source: IMFMediaSource = unsafe { act.ActivateObject().unwrap() };
        unsafe { act.ShutdownObject().unwrap() };
    }
    for _ in 0..8 {
        let act = deskcam_source::activator::create(info).unwrap();
        let source: IMFMediaSource = unsafe { act.ActivateObject().unwrap() };
        drop(source);
        drop(act);
    }
    for _ in 0..4 {
        use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
        let act = deskcam_source::activator::create(info).unwrap();
        let source: IMFMediaSource = unsafe { act.ActivateObject().unwrap() };
        let pd = unsafe { source.CreatePresentationDescriptor().unwrap() };
        unsafe {
            pd.SelectStream(0).unwrap();
            source.Start(&pd, &windows_core::GUID::zeroed(), &PROPVARIANT::default()).unwrap();
            act.ShutdownObject().unwrap();
        }
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    let after = process_thread_count();
    assert!(after <= before + 1, "threads grew from {before} to {after}");
}
