//! Opens the DeskCam virtual camera like any camera app and reports what it delivers.
//!
//! `cargo run --release --example probe -- [--name DeskCam] [--nv12] [--frames 90] [--out probe.bmp]`

use std::time::Instant;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows_core::PWSTR;

struct Args {
    name: String,
    nv12: bool,
    frames: u32,
    out: String,
}

fn parse_args() -> Args {
    let mut args = Args { name: "DeskCam".into(), nv12: false, frames: 90, out: "probe.bmp".into() };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--name" => args.name = it.next().expect("--name value"),
            "--nv12" => args.nv12 = true,
            "--frames" => args.frames = it.next().and_then(|v| v.parse().ok()).expect("--frames number"),
            "--out" => args.out = it.next().expect("--out path"),
            other => panic!("unknown argument {other}"),
        }
    }
    args
}

fn friendly_name(act: &IMFActivate) -> String {
    let (mut p, mut len) = (PWSTR::null(), 0u32);
    unsafe {
        if act.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut p, &mut len).is_err() {
            return String::new();
        }
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn write_bmp(path: &str, w: u32, h: u32, bgrx: &[u8]) -> std::io::Result<()> {
    let size = (w * h * 4) as usize;
    let mut out = Vec::with_capacity(54 + size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + size) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(-(h as i32)).to_le_bytes()); // top-down
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    out.extend_from_slice(&bgrx[..size]);
    std::fs::write(path, out)
}

fn main() -> windows_core::Result<()> {
    let args = parse_args();
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL)? };

    let mut attrs = None;
    unsafe { MFCreateAttributes(&mut attrs, 1)? };
    let attrs = attrs.unwrap();
    unsafe { attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)? };
    let (mut list, mut count) = (std::ptr::null_mut(), 0u32);
    unsafe { MFEnumDeviceSources(&attrs, &mut list, &mut count)? };
    let devices: Vec<IMFActivate> =
        unsafe { std::slice::from_raw_parts(list, count as usize) }.iter().flatten().cloned().collect();
    unsafe { CoTaskMemFree(Some(list as *const _)) };

    let names: Vec<String> = devices.iter().map(friendly_name).collect();
    for n in &names {
        println!("camera: {n}");
    }
    let Some(index) = names.iter().position(|n| n.starts_with(&args.name)) else {
        eprintln!("no camera starting with '{}'", args.name);
        std::process::exit(1);
    };
    let source: IMFMediaSource = unsafe { devices[index].ActivateObject()? };

    let mut reader_attrs = None;
    unsafe { MFCreateAttributes(&mut reader_attrs, 1)? };
    let reader_attrs = reader_attrs.unwrap();
    unsafe { reader_attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)? };
    let reader = unsafe { MFCreateSourceReaderFromMediaSource(&source, &reader_attrs)? };
    let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    let partial = unsafe { MFCreateMediaType()? };
    unsafe {
        partial.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        partial.SetGUID(&MF_MT_SUBTYPE, if args.nv12 { &MFVideoFormat_NV12 } else { &MFVideoFormat_RGB32 })?;
        reader.SetCurrentMediaType(stream, None, &partial)?;
    }
    let current = unsafe { reader.GetCurrentMediaType(stream)? };
    let size = unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE)? };
    let rate = unsafe { current.GetUINT64(&MF_MT_FRAME_RATE)? };
    let (w, h) = ((size >> 32) as u32, size as u32);
    println!(
        "negotiated {} {}x{} @ {}/{} fps",
        if args.nv12 { "NV12" } else { "RGB32" },
        w,
        h,
        rate >> 32,
        rate & 0xFFFF_FFFF
    );

    let mut got = 0u32;
    let mut first: Option<Instant> = None;
    let mut last = Vec::new();
    while got < args.frames {
        let (mut flags, mut sample) = (0u32, None);
        unsafe { reader.ReadSample(stream, 0, None, Some(&mut flags), None, Some(&mut sample))? };
        if flags & (MF_SOURCE_READERF_ERROR.0 as u32 | MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            eprintln!("stream ended, flags {flags:#x}");
            break;
        }
        let Some(sample) = sample else { continue };
        first.get_or_insert_with(Instant::now);
        got += 1;
        let buffer = unsafe { sample.ConvertToContiguousBuffer()? };
        let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
        unsafe { buffer.Lock(&mut data, None, Some(&mut len))? };
        last = unsafe { std::slice::from_raw_parts(data, len as usize) }.to_vec();
        unsafe { buffer.Unlock()? };
    }
    let secs = first.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    let fps = if got > 1 && secs > 0.0 { (got - 1) as f64 / secs } else { 0.0 };
    println!("received {got} frames, measured {fps:.1} fps");

    let pixels = (w * h) as usize;
    let mean = if args.nv12 {
        last.iter().take(pixels).map(|&v| v as u64).sum::<u64>() as f64 / pixels as f64
    } else {
        last.chunks_exact(4).take(pixels).map(|p| (p[0] as u64 + p[1] as u64 + p[2] as u64) / 3).sum::<u64>() as f64
            / pixels as f64
    };
    println!("mean luma of last frame: {mean:.1}");
    if !args.nv12 && last.len() >= pixels * 4 {
        write_bmp(&args.out, w, h, &last).expect("write bmp");
        println!("wrote {}", args.out);
    }
    unsafe {
        let _ = source.Shutdown();
        let _ = MFShutdown();
    }
    Ok(())
}
