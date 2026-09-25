# Windows 10 support plan

Status: **implemented, not yet tested on Windows 10 hardware.** Phases 0–3 are written; everything
type-checks for x64 and x86, and a Windows CI workflow runs the tests and registers the filter. The
design sections below were written first; see *Implementation status* for how the code turned out.

## Implementation status

| Phase | Status | Where |
|---|---|---|
| 0. `deskcam.exe` starts on Win10 | Done | `deskcam/src/vcam.rs` (runtime `MFCreateVirtualCamera`), `deskcam-proto/src/os.rs` (`RtlGetVersion`), `deskcam/src/capture.rs` (cursor/borderless non-fatal), `worker.rs` (build ≥ 18362 check) |
| 1. Backend abstraction | Done | `deskcam/src/backend.rs`, `config.rs` (`backend = auto \| mf \| dshow`), `deskcam-proto/src/section.rs` (`create_local` / `open_local`) |
| 2a. `deskcam-dshow` filter (NV12) | Done | `crates/deskcam-dshow`: `filter.rs`, `pin.rs`, `enums.rs`, `stream.rs`, `format.rs`, `lib.rs` (DLL exports, registration) |
| 2b. I420 / YUY2, x86 build, resize handling, flush on Stop | Done | `deskcam-proto/src/convert.rs` (+ unit tests), `stream.rs` (`FrameSource` reopens on size change and scales) |
| 3. Installer / uninstaller | Done | `scripts/install.ps1` (`-Backend auto\|mf\|dshow`, x64 + x86 registration), `scripts/uninstall.ps1` |
| 4. Tests / CI | Partly | `deskcam-dshow/tests/filter.rs` (pin/caps + a running graph into the Null Renderer), `.github/workflows/ci.yml`. **Still to do:** `probe --dshow`, and the manual app matrix on a Win10 VM |
| 5. Docs | Done | README support table, install steps and troubleshooting |
| 6. Desktop Duplication capture (no yellow border) | Not started | Optional |

### How it works now

```
deskcam.exe ── backend = auto ──► Windows 11: MFCreateVirtualCamera (unchanged path)
     │                           Windows 10: create Local\DeskCam-v1-WxH, keep it open
     │
     └── worker (unchanged): writes frames into the FrameRing while any reader heartbeat is fresh

App process (Discord, Chrome, OBS, ...)                 install.ps1 registered, once:
  enumerates CLSID_VideoInputDeviceCategory ─────────►  "DeskCam" → {566D2A35-...} → deskcam_dshow.dll
  CoCreateInstance → Filter (IBaseFilter)                (x64 in Program Files\DeskCam, x86 in \x86)
    └─ OutputPin (IPin, IAMStreamConfig, IKsPropertySet = PIN_CATEGORY_CAPTURE)
         Connect → downstream allocator (or CLSID_MemoryAllocator), 3+ buffers
         Pause/Run → delivery thread, one sample per 1/fps:
           read stream.bin → open Local\ section → touch_reader → read_latest
           → NV12 copy / I420 / YUY2 (nearest-neighbour scale if the app's size changed) → Receive
         Stop → Decommit, BeginFlush, join thread, EndFlush
```

### Where the code differs from the design below

- **No app manifest.** `RtlGetVersion` reports the real build without one, so Phase 0.5 was dropped.
- **Section security.** The `Local\` section keeps the same DACL as the global one plus a
  **low integrity label** (`S:(ML;;NW;;;LW)`), so low-integrity consumer processes can still write the
  reader heartbeat. Without the heartbeat, capture never starts.
- **Camera name on Win10** is passed at registration: `regsvr32 /n /i:"Name" deskcam_dshow.dll` calls
  `DllInstall`, and `install.ps1` reads `name` from `config.ini`. Plain `regsvr32` registers as
  "DeskCam". The app logs a note if the registered name differs from `config.ini`.
- **Frame rate:** a consumer may pick any rate from 1 to 240 via `IAMStreamConfig::SetFormat`, and the
  filter paces delivery to it. The size must be the one advertised.
- **Paused state:** like OBS, the filter delivers samples in Paused too (without timestamps) so renderers
  can cue. After `Run`, samples carry stream time (`clock − tStart`).
- **`GetSyncSource` with no clock** returns `VFW_E_NO_CLOCK`, because the `windows` crate trait cannot
  return S_OK with NULL.
- **Locked DLLs on reinstall:** apps keep the filter loaded, so `install.ps1` renames an in-use DLL and
  copies the new one next to it. The leftover is deleted on a later install.

### Next steps

1. Run CI on the branch and fix anything Windows-specific that `cargo check` could not catch.
2. Test on a Windows 10 22H2 VM using the manual matrix in Phase 4.
3. `probe --dshow`: enumerate the device like an app does and save a snapshot.
4. Optional: a placeholder image while `deskcam.exe` isn't running, and Desktop Duplication capture.

## 1. The problem

DeskCam on Windows 11 works like this:

```
deskcam.exe (user session)                       Windows Camera Frame Server (session 0, LocalService)
  MFCreateVirtualCamera(CLSID) ───registers───►   loads deskcam_source.dll (IMFMediaSource)
  WGC capture → GPU NV12 → FrameRing  ◄─shared─►  creates Global\DeskCam-v1-WxH, reads FrameRing
                                                  │
                                                  └─► every app (MF, DirectShow, WinRT, UWP) sees the camera
```

On Windows 10, three parts of that chain are missing or behave differently:

| Piece | Windows 11 | Windows 10 |
|---|---|---|
| `MFCreateVirtualCamera` (mfsensorgroup.dll) | Available since build 22000 | **Missing.** There is no supported way to add a software camera to the Frame Server. |
| Linking | Import resolves | `windows-rs` 0.62 links through `raw-dylib`, which makes a **load-time** import (confirmed in `windows-link`'s `link!` macro). Because of that, `deskcam.exe` **fails to start** on Win10 ("entry point not found") before any code runs. Fixed in Phase 0, and CI checks the import table. |
| `GraphicsCaptureAccess` / `SetIsBorderRequired` | Available | Missing. This is already non-fatal (it's only logged), so Win10 shows the yellow capture border. |
| `SetIsCursorCaptureEnabled` | Available | Needs Win10 2004 (19041) or later. It is currently called with `?`, so on older builds a failure here is fatal. |
| `IGraphicsCaptureItemInterop::CreateForMonitor` | Available | Needs Win10 1903 (18362) or later. |

The Frame Server does exist on Win10, but it only hosts sources for physical devices (Device MFTs) and
sensor groups. The only ways to add a camera that apps can pick are:

| Option | Seen by | Cost / risk | Verdict |
|---|---|---|---|
| **A. DirectShow source filter** registered under `CLSID_VideoInputDeviceCategory` (the approach used by OBS Virtual Camera, Unity Capture, softcam, akvirtualcamera) | DirectShow apps: Discord, Zoom, Teams (classic and new/WebView2), Chrome, Edge, Firefox, OBS, Skype, most conferencing apps. Chromium lists DirectShow-only devices next to MF ones. | User-mode DLL, no driver signing. It loads **inside every consumer process**, so we need 32-bit and 64-bit builds. | **Recommended** |
| B. Kernel AVStream driver (based on the `avshws` sample) | Everything, including MF-only and UWP apps (the Windows Camera app, Windows Hello-style consumers) | Needs an EV code-signing certificate plus Microsoft attestation signing (or test-signing mode). A bug here means a BSOD, and kernel Rust (`windows-drivers-rs`) is immature for AVStream. | Future option. Worth it only if MF-only apps turn out to matter. |
| C. Sensor group / Device MFT hack | — | Needs a real camera to attach to, is undocumented for this use, and is fragile. | Rejected |

**Decision: Option A.** On Windows 10, DeskCam ships a DirectShow capture filter, `deskcam_dshow.dll`, which reads
the same shared `FrameRing` that `deskcam_source.dll` reads on Win11. The capture pipeline, config,
tray and proto crate stay shared. Only the part that exposes the camera differs by OS.

Known limitation to put in the README: on Windows 10, **MF-only consumers (the built-in Windows Camera
app, some UWP apps) will not list DeskCam.** Discord, OBS, browsers, Zoom and Teams will.

## 2. Target architecture

```
                         ┌──────────── Windows 11 (build ≥ 22000) ────────────┐
deskcam.exe ─ backend ──►│ MfVirtualCamera: MFCreateVirtualCamera (unchanged) │
   │                     └────────────────────────────────────────────────────┘
   │                     ┌──────────── Windows 10 (build < 22000) ────────────┐
   └──────── backend ───►│ DShowBackend: create + own Local\ section only;    │
                         │ filter registered once by install.ps1              │
                         └────────────────────────────────────────────────────┘

Consumer app process (Discord, Chrome, ...)
  System Device Enumerator → CLSID_VideoInputDeviceCategory → "DeskCam"
  → CoCreateInstance(CLSID_DESKCAM_DSHOW) → deskcam_dshow.dll (x64 or x86, matching the app)
  → output pin pushes samples read from FrameRing (opened by name, read + heartbeat)
```

### Differences in who owns the section

| | Win11 (today) | Win10 (new) |
|---|---|---|
| Section creator | Media source in session 0 (`Global\`, no privilege needed there) | **`deskcam.exe`**, because consumers run in the user session, and creating `Global\` objects there needs `SeCreateGlobalPrivilege`, which normal user processes don't have |
| Name | `Global\DeskCam-v1-WxH` | `Local\DeskCam-v1-WxH` (same session as the consumers) |
| Reader heartbeat | Media source | Each filter instance (several apps can read at once, and that's fine: the heartbeat is "latest wins") |
| App not running | Black frames (`WriterGone`) | Black frames, or a static "DeskCam is not running" placeholder (nice to have) |

`FrameRing` itself (seqlock, heartbeats, 3 slots) needs **no format change**.

## 3. Work breakdown

### Phase 0: Make the existing binary start and run on Win10 (small, do first)

1. **Load `MFCreateVirtualCamera` at runtime** in `crates/deskcam/src/vcam.rs`:
   `LoadLibraryExW("mfsensorgroup.dll", LOAD_LIBRARY_SEARCH_SYSTEM32)` + `GetProcAddress`, cast to the
   documented signature, and return a clear "not supported on this Windows version" error when the export is missing.
   Keep `IMFVirtualCamera` usage as it is (it's a COM interface and needs no import).
2. **Detect the OS build once** (`RtlGetVersion` from ntdll, because `GetVersionEx` lies without a manifest; or
   add a `supportedOS` manifest). Add `deskcam_proto::os::is_win11()` (build ≥ 22000).
3. **capture.rs**: make `SetIsCursorCaptureEnabled` non-fatal (log it, as the borderless call already does).
   Skip the `GraphicsCaptureAccess` call entirely on Win10 so the log stays clean.
4. **Minimum supported version**: Win10 **1903 (18362)** for WGC monitor capture, and 22H2 (19045) recommended.
   Show a clear error in the tray tooltip below that.
5. Add an app manifest (`embed-resource` or `winres` in `build.rs`) declaring Win10/11 `supportedOS` and
   `dpiAware`. This also makes version APIs report the real version.

Exit criterion: on a Win10 22H2 VM, `deskcam.exe` starts, the tray shows *"virtual camera not supported
on Windows 10 yet"*, and nothing crashes.

### Phase 1: Camera backend abstraction in `deskcam`

1. New module `crates/deskcam/src/backend.rs`:
   ```rust
   pub enum Backend { MfVirtualCamera(VirtualCamera), DirectShow }
   impl Backend {
       pub fn start(cfg: &Config, info: StreamInfo) -> Result<Backend, String>;
       /// Win11: open the source's Global\ section (retry until a consumer creates it).
       /// Win10: create the Local\ section ourselves, once, at startup.
       pub fn section(&self, info: StreamInfo) -> Option<Section>;
   }
   ```
2. `worker.rs` swaps `VirtualCamera::create` + `Section::open` for the backend calls. The demand loop
   (`reader_active` → start/stop WGC) is unchanged.
3. `deskcam-proto/src/section.rs`: add `Section::create_local(width, height)`. It uses the same `SECTION_SDDL`
   minus the SY/LS entries (not needed), keeps AU read/write for heartbeats, and keeps `AC` / `S-1-15-2-2` read so
   AppContainer consumers can at least try to open it. Also add a `section_name_local()` variant.
4. Config: add an optional `backend = auto | mf | dshow` key (default `auto`) so the DirectShow path can be
   tested on Win11 too. This is useful for development and CI.

### Phase 2: `deskcam-dshow` crate (the core of the work)

New workspace member `crates/deskcam-dshow`, `crate-type = ["cdylib", "rlib"]`, with windows features
`Win32_Media_DirectShow`, `Win32_Media_MediaFoundation` (for GUIDs), `Win32_System_Com`,
`Win32_Graphics_Gdi` (`BITMAPINFOHEADER`), `Win32_System_Registry`, `Win32_System_Threading`.

We write this without Microsoft's C++ `strmbase` base classes, so we implement the minimum COM surface ourselves with
`#[implement]`. It is about the same amount of code as `deskcam-source`.

| Object | Interfaces | Notes |
|---|---|---|
| `Filter` | `IBaseFilter` (+ `IMediaFilter`, `IPersist`), `IAMFilterMiscFlags` (`AM_FILTER_MISC_FLAGS_IS_SOURCE`), `ISpecifyPropertyPages` (optional, skip) | Tracks the state (Stopped/Paused/Running), the graph (`IFilterGraph`, weak), the reference clock (`IReferenceClock`) and `tStart`. Exposes exactly one pin. |
| `OutputPin` | `IPin`, `IAMStreamConfig`, `IKsPropertySet` (returns `PIN_CATEGORY_CAPTURE` for `AMPROPERTY_PIN_CATEGORY`, which is required for apps to treat it as a capture pin). OBS ships without `IQualityControl`/`IAMPushSource`, so we leave them out of the MVP. | Connection negotiation, allocator negotiation via `IMemInputPin::GetAllocator`/`NotifyAllocator` + `DecideBufferSize` |
| `EnumPins`, `EnumMediaTypes` | `IEnumPins`, `IEnumMediaTypes` | Small cloneable cursors over fixed lists |
| class factory | `IClassFactory` | Same pattern as `deskcam-source/src/lib.rs` (`ObjGuard`, `DllCanUnloadNow`) |

**Media types** (`AM_MEDIA_TYPE` + `VIDEOINFOHEADER`, `FORMAT_VideoInfo`), in preference order:

1. `MEDIASUBTYPE_NV12`: a direct copy of the ring slot (what `write_frame` already does for MF)
2. `MEDIASUBTYPE_I420`: a plane swizzle (U and V de-interleaved)
3. `MEDIASUBTYPE_YUY2`: CPU 4:2:0 → 4:2:2 repack. Several older apps and Discord's engine like it.
4. *(Optional, only if an app needs it)* `MEDIASUBTYPE_RGB32`: reuse `deskcam_proto::color::nv12_row_to_bgrx`.
   **DirectShow RGB is bottom-up** (positive `biHeight`), so write rows in reverse order. OBS ships
   without RGB, which suggests the three YUV formats cover mainstream apps.

At first, resolution and fps are the single mode taken from `stream.bin`, loaded at filter creation exactly as
`ClassFactory::CreateInstance` does today. `IAMStreamConfig::GetStreamCaps` reports one
`VIDEO_STREAM_CONFIG_CAPS` per subtype. `SetFormat` accepts only those.
*Later:* offer 1280×720 and 640×360 downscales on the CPU for apps that insist on smaller sizes.

**Streaming thread** (a direct port of `media_stream::delivery_loop`):

```
on Pause→Run:   spawn thread; open Local\ section (retry every 250 ms, like ensure_section)
loop at 1/fps:  sample = allocator.GetBuffer()
                ring.touch_reader(now_ms())
                ring.read_latest(...) → convert into sample buffer (black on NoFrame/WriterGone)
                sample.SetTime(start, start + AvgTimePerFrame)   // stream time, from graph clock
                sample.SetSyncPoint(TRUE); SetActualDataLength(frame bytes)
                peer IMemInputPin::Receive(sample)
on Stop:        allocator.Decommit(); join thread
```

Timestamps come from the graph's `IReferenceClock` minus `tStart` when a clock exists. If there is none, they
come from `frame_index * AvgTimePerFrame`. Handle `Receive` returning `S_FALSE` or an error by stopping delivery
quietly (the downstream filter is flushing).

**Registration** (`DllRegisterServer` / `DllUnregisterServer`):

1. `HKLM\Software\Classes\CLSID\{CLSID_DESKCAM_DSHOW}\InprocServer32` = DLL path, `ThreadingModel=Both`
   (same helper as `deskcam-source`, factored into `deskcam-proto` or a small shared `deskcam-com` crate).
2. `IFilterMapper2::RegisterFilter(CLSID, name, NULL, &CLSID_VideoInputDeviceCategory, name, &REGFILTER2)`
   with one output pin of `MEDIATYPE_Video` / `MEDIASUBTYPE_NULL`, merit `MERIT_DO_NOT_USE`.
3. The friendly name is read from `config.ini` at registration time. Renaming on Win10 therefore needs a
   re-run of `install.ps1`, so document this and have the tray log a hint when `name` differs from the
   registered one.
4. A **new CLSID** for the DirectShow filter (never reuse `deskcam_proto::CLSID`, which is the MF source).

**32-bit build:** consumers load the filter in-process, so a 32-bit app needs a 32-bit DLL. Build with
`--target i686-pc-windows-msvc` and register it with `%WINDIR%\SysWOW64\regsvr32.exe`. `deskcam-proto` must
compile for x86. `AtomicU64` works on i686 (`cmpxchg8b`), and the mapped view is page-aligned. A 4K
section (~37 MB) fits comfortably in a 32-bit address space.

### Lessons from OBS Virtual Camera

I read OBS's implementation as a reference: `obs-studio/plugins/win-dshow/virtualcam-module`,
`plugins/win-dshow/virtualcam.c`, `shared/obs-shared-memory-queue`, and the `OutputFilter`/`OutputPin` COM
plumbing in `obsproject/libdshowcapture/source/output-filter.{hpp,cpp}`.

> **License boundary:** OBS is **GPL-2.0** and libdshowcapture is **LGPL-2.1**. DeskCam is MIT. Use them
> only to learn *what* a working DirectShow virtual camera does. Write our code from the DirectShow
> documentation, and do not translate their source line by line.

**What OBS confirms about our design**

| Our plan | OBS |
|---|---|
| DirectShow filter under `CLSID_VideoInputDeviceCategory` | Same. OBS uses DirectShow on **every** Windows version, including 11 (its `win-dshow` virtual camera has no `MFCreateVirtualCamera` path), and it works in Discord, Chrome, Zoom and Teams. |
| Writer app creates the section in the user session; filter opens it | Same. `video_queue_create` makes `OBSVirtualCamVideo` with no prefix, which is session-local, the equivalent of `Local\`. |
| Resolution known before the app starts, from `stream.bin` | Same idea. The filter constructor reads `%APPDATA%\obs-virtualcam.txt` (`"WxHxinterval"`), which OBS writes when the camera starts. |
| Object set: filter + one output pin + 2 enumerators + class factory | Same: `IBaseFilter`, `IPin`, `IAMStreamConfig`, `IKsPropertySet` (only `AMPROPERTY_PIN_CATEGORY` → `PIN_CATEGORY_CAPTURE`), and `IAMFilterMiscFlags` → `AM_FILTER_MISC_FLAGS_IS_SOURCE`. No `IAMPushSource`, `IQualityControl` or property pages, **so they are dropped from the MVP.** |
| Registration: HKCR CLSID + `IFilterMapper2::RegisterFilter`, `MERIT_DO_NOT_USE` | Same. The pin type is registered as `MEDIATYPE_Video`/`MEDIASUBTYPE_NV12`. It exports `DllInstall` too. x86, x64 and ARM64 DLLs are registered separately. The friendly name is hard-coded, so OBS doesn't support renaming either. |

**Changes to our plan based on OBS**

1. **Formats:** OBS offers only **NV12, I420 and YUY2** (no RGB), and that is enough for every mainstream
   app. Ship those three in Phase 2a. Make RGB32 optional and add it only if a real app needs it.
2. **One mode per format:** OBS advertises a single resolution/fps (the current source's), not a list.
   That matches our "one mode from `stream.bin`" plan.
3. **Resolution mismatch:** if the source resolution changes while an app still holds the filter, OBS keeps
   the negotiated output size and **scales on the CPU** (`tiny-nv12-scale`, nearest neighbour). Our section
   name contains `WxH`, so without handling this a filter would wait forever on the old name. **Add:**
   the filter re-reads `stream.bin` when the writer has been gone for a while, opens the new section, and
   nearest-neighbour scales into the negotiated size. This is a small function of our own in `deskcam-proto`
   and can be unit tested.
4. **Placeholder when DeskCam isn't running:** OBS shows a bundled image (decoded with GDI+ and scaled),
   and falls back to flat grey (`memset 127`). We start with black, as now. A placeholder image is a later polish item.
5. **Thread lifecycle:** OBS creates the delivery thread in the filter constructor, blocks it on a
   "start" event that `Pause()` signals, and stops it with a "stop" event in the destructor. Pacing is an
   absolute deadline (`sleepto_100ns`: `Sleep(ms-1)` then spin). Our `Instant`-based loop from
   `media_stream::delivery_loop` already does the same job.
6. **Allocator negotiation (at `Connect`):** call `IMemInputPin::GetAllocator`. On
   `VFW_E_NO_ALLOCATOR`, fall back to `CoCreateInstance(CLSID_MemoryAllocator)`. If
   `GetAllocatorRequirements` is `E_NOTIMPL`, default to **4 buffers, 32-byte alignment**. Set `cbBuffer` = frame size,
   then `SetProperties`, then `NotifyAllocator(alloc, FALSE)`. **Commit** on Stopped→Paused.
7. **Per-sample:** `GetBuffer` → `SetActualDataLength` → `SetSyncPoint(TRUE)` / `SetDiscontinuity(FALSE)` /
   `SetPreroll(FALSE)` → fill → `SetTime` + `SetMediaTime` → `Receive`. After a format change, attach the
   new type once with `IMediaSample::SetMediaType`.
8. **Stop:** call `BeginFlush()`/`EndFlush()` on the connected pin so downstream drops queued samples.
9. **Timestamps:** OBS stamps samples with **raw** `IReferenceClock::GetTime()` and adds one interval per frame.
   The DirectShow docs say capture timestamps are *stream time* (`clock - tStart`). We will follow the
   docs. If an app misbehaves, OBS's approach is a known-working fallback.
10. **Connect:** OBS offers only its current media type to `ReceiveConnection` and doesn't walk the peer's
    types. That's enough, because apps pick a format through `IAMStreamConfig::SetFormat` first.

**Where we deliberately differ from OBS**

- **`QueryAccept` / `SetFormat`:** OBS accepts *any* media type. We check subtype, size and
  `FORMAT_VideoInfo` and return `VFW_E_INVALIDMEDIATYPE` otherwise, so bad input from an app can't
  corrupt buffer sizes.
- **Demand-driven capture:** OBS writes frames whenever its virtual camera is on. We keep the reader
  heartbeat, so the desktop is only captured while an app is actually pulling frames.
- **Section security:** OBS relies on the default DACL. We keep an explicit SDDL (Phase 1.3).
- **Frame integrity:** OBS's queue has no torn-frame protection; it just keeps 3 slots and a read index,
  and after 10 repeated indices it treats the source as stalled. Our seqlock `FrameRing` stays.

### Phase 3: Installer and uninstaller

`scripts/install.ps1`:

```powershell
$build = [int](Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion').CurrentBuildNumber
if ($build -ge 22000) {
    # existing path: register deskcam_source.dll, restart FrameServer
} else {
    if ($build -lt 18362) { throw "Windows 10 1903 or newer is required." }
    # copy deskcam_dshow.dll (x64) and deskcam_dshow32.dll (x86)
    & "$env:WINDIR\System32\regsvr32.exe" /s "$Inst\deskcam_dshow.dll"
    & "$env:WINDIR\SysWOW64\regsvr32.exe" /s "$Inst\deskcam_dshow32.dll"   # only on 64-bit Windows
}
```

- Skip the `FrameServer` service stops on Win10.
- Keep the `icacls` step: consumers read `stream.bin` from `C:\ProgramData\DeskCam`.
- `uninstall.ps1` mirrors this with `regsvr32 /u` for both DLLs. Apps that currently hold the DLL
  keep it loaded until they restart, so print a hint to close Discord and other apps first.
- `-BuildDir32` parameter defaulting to `target\i686-pc-windows-msvc\release`.

### Phase 4: Testing

1. **Unit tests** (any OS with `cargo test` on Windows): media-type builders, I420/YUY2 converters, the
   nearest-neighbour NV12 scaler, (golden pixels, like `color.rs`), and `VIDEOINFOHEADER` sizes/strides.
2. **Integration test** `crates/deskcam-dshow/tests/graph.rs`, without registration (mirrors
   `deskcam-source/tests/activate.rs`): instantiate the filter through its Rust constructor, build a filter graph
   with `CLSID_FilterGraph`, add the filter and a **Null Renderer** (or a minimal Rust sink that counts
   samples), `ICaptureGraphBuilder2::RenderStream(PIN_CATEGORY_CAPTURE, ...)`, run for 1 s, and assert
   about `fps` samples with increasing timestamps and the correct size per subtype.
3. **probe example**: add `--dshow` to `crates/deskcam/examples/probe.rs`. It enumerates
   `CLSID_VideoInputDeviceCategory` via `ICreateDevEnum`, finds "DeskCam", grabs frames through a sink, and
   writes `probe.bmp`. This lets people check that frames flow on Win10 and on Win11 with `backend = dshow`.
4. **CI**: add `.github/workflows/ci.yml` on `windows-latest` with a matrix of x86_64 + i686 build, `cargo test`,
   `cargo clippy`. The DirectShow graph test runs there (runners are admin and DirectShow is present).
   MF virtual camera tests cannot run on Server 2022 (build 20348), so gate them on build ≥ 22000.
5. **Manual matrix on a Win10 22H2 VM**:

   | App | Arch | Expectation |
   |---|---|---|
   | Discord desktop | x64 | Listed, streams |
   | OBS 30+ (Video Capture Device) | x64 | Listed, streams, all 4 formats selectable |
   | Chrome / Edge (webcamtests.com) | x64 | Listed, streams |
   | Firefox | x64 | Listed, streams |
   | Zoom | x64/x86 | Listed, streams |
   | Any 32-bit DirectShow app (e.g. AMCap x86, GraphEdit x86) | x86 | Listed, streams |
   | Windows Camera app | UWP/MF | **Not listed (expected, documented)** |
   | Two apps at once | — | Both stream, capture stops ~1.5 s after both close |
   | DeskCam not running | — | Camera listed, black/placeholder frames, app doesn't hang |

### Phase 5: Documentation

- README: rename "Windows 11 only" to a support table (Win11: all apps; Win10 1903+: DirectShow apps),
  explain the name-change caveat, and add Win10 troubleshooting (the yellow border is normal, and the Windows
  Camera app won't list DeskCam).
- Update the "How it works" section and the crate map with `deskcam-dshow`.

## 4. Risks and open questions

| Risk | Mitigation |
|---|---|
| The `raw-dylib` import blocks startup on Win10 (Phase 0.1) | Dynamic `GetProcAddress`. Verify with `dumpbin /imports deskcam.exe` that `MFCreateVirtualCamera` is gone. |
| AppContainer / LPAC consumers resolve `Local\` inside their own `AppContainerNamedObjects` directory and can't see our section | Most target apps (Discord, Chrome's capture utility process, Zoom, OBS) are not AppContainer'd. If one is, open via the full `\Sessions\<id>\BaseNamedObjects\...` path using `NtOpenSection` as a fallback, and keep the `AC` read ACE. |
| Several consumers call `touch_reader` at once | It's a single store of `now`, which is harmless. Capture runs while any reader is active. |
| An app picks RGB24 only | Add `MEDIASUBTYPE_RGB24` (bottom-up, 3 bytes/px, 4-byte row alignment) if found in testing. |
| Filter loaded in a process that outlives DeskCam | Heartbeat logic already serves black when the writer is gone, and the section reopens when DeskCam restarts (retry loop). Mapping 3 × 4K frames per process is acceptable. |
| Yellow WGC border on Win10 | This is how Windows 10 works and can't be turned off there. *Optional* Phase 6: a DXGI Desktop Duplication capture backend (Win8+, no border, needs manual cursor compositing via `GetFramePointerShape`, and must handle `DXGI_ERROR_ACCESS_LOST` on UAC/secure desktop switches). |
| MF-only apps can't see the camera | Documented limitation. The only real fix is Option B (a signed AVStream driver), which is out of scope. |
| Windows 10 end of support (Oct 2025; ESU through Oct 2026 consumer / 2028 commercial) | Keep the Win10 path isolated (its own crate plus one backend enum arm) so it can be dropped cleanly. |

## 5. Suggested order of pull requests

1. **Phase 0**: dynamic `MFCreateVirtualCamera`, OS detection, non-fatal cursor flag, manifest.
2. **Phase 1**: backend abstraction plus `Section::create_local` and the `backend=` config key (still Win11 only).
3. **Phase 2a**: `deskcam-dshow` with NV12 only, registration, graph integration test, and `probe --dshow`.
4. **Phase 2b**: I420 / YUY2 formats, the i686 build, re-reading `stream.bin` plus nearest-neighbour scaling when the resolution changes, and flushing on Stop.
5. **Phase 3**: installer and uninstaller.
6. **Phase 4/5**: CI workflow, README, and a manual test pass on a Win10 VM.
7. *(Optional)* Phase 6: Desktop Duplication capture backend.

Rough size: Phase 0 ~100 LOC, Phase 1 ~150 LOC, Phase 2 ~1,200–1,500 LOC (similar to `deskcam-source`),
Phase 3–5 ~200 LOC + docs.

## References

- OBS Virtual Camera (GPL-2.0, reference only): `obs-studio/plugins/win-dshow/virtualcam-module`,
  `shared/obs-shared-memory-queue`, and `obsproject/libdshowcapture` `source/output-filter.cpp` (LGPL-2.1).
  See "Lessons from OBS Virtual Camera" above.
- `schellingb/UnityCapture`, `tshino/softcam`, `webcamoid/akvirtualcamera`: DirectShow virtual cameras
- Microsoft docs: *Registering a DirectShow Filter* / `IFilterMapper2::RegisterFilter`, *Writing Capture
  Filters* (`PIN_CATEGORY_CAPTURE`, `IKsPropertySet`, `IAMStreamConfig`)
- Windows driver samples `avstream/avshws` (Option B reference)
- `MFCreateVirtualCamera` minimum client: Windows 11 (build 22000)
