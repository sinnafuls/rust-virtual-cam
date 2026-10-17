# DeskCam

Share your desktop as a **webcam** on **Windows 10 and Windows 11**. DeskCam adds a camera named
**"DeskCam"** that streams one of your monitors for Discord, OBS, AMD Privacy View, browsers and
other camera apps.

- Written in Rust, with no dependencies beyond the Windows APIs.
- Runs in the background with a tray icon.
- Uses almost no CPU while no app is using the camera; it only captures while something is watching.
- Configured with a small `config.ini` file.

| | How the camera is added | Which apps see it |
|---|---|---|
| **Windows 11** (build 22000 or newer) | Windows' virtual camera API (`MFCreateVirtualCamera`); shows as "DeskCam (Windows Virtual Camera)" | Media Foundation, WinRT and DirectShow consumers. The existing Windows 11 camera path is unchanged. |
| **Windows 10** (1903 or newer, 64-bit) | DirectShow filters for 64-/32-bit apps, plus a driverless software camera served by Windows Camera Frame Server | DirectShow consumers and Media Foundation/WinRT consumers, including AMD Privacy View and the Windows Camera app. Shows as "DeskCam". |

The Windows 10 software camera lets apps discover DeskCam through the same camera APIs they use
for physical cameras. It requires administrator permission to install, but **no custom kernel
driver, purchased certificate, Test Mode, or Secure Boot changes**. Some apps may list both
camera paths; both are backed by the same desktop capture.

**Something not working?** See [Debugging and reporting problems](#debugging-and-reporting-problems)
to collect a diagnostic report and open a bug report.

## Install

1. Download **`DeskCam-Setup-x.y.z.exe`** from the [latest release](https://github.com/sinnafuls/rust-virtual-cam/releases/latest).
2. Run it and accept the administrator prompt (registering a camera with Windows needs admin rights).
   - Windows may show **"Windows protected your PC"**, because the installer isn't code-signed.
     Click **More info → Run anyway**.
   - Choose whether DeskCam should **start automatically when you sign in** (recommended) and whether
     to add a **desktop shortcut**.
3. Leave **Start DeskCam now** ticked on the last page. DeskCam runs in the tray; the icon may be
   hidden behind the `^` arrow on the taskbar.
4. Open Discord (or OBS, your browser, and so on) and pick the camera:
   **DeskCam (Windows Virtual Camera)** on Windows 11, **DeskCam** on Windows 10.
   On Windows 10, restart apps that were already open.
   - **Discord:** Settings → Voice & Video → Camera
   - **OBS:** Sources → + → Video Capture Device → DeskCam

The installer selects the camera registration for your Windows version. On Windows 10 it installs
both DirectShow filters and `deskcam_source.dll`, whose registration creates the software camera
for Media Foundation and WinRT apps. **Upgrade older Windows 10 installations with the new installer**
to add this camera path, then fully quit and reopen the consuming apps.

Requirements: Windows 11, or 64-bit Windows 10 version 1903 (build 18362) or newer.

## Settings

Right-click the tray icon and choose **Open config**. Edit the file, save it, then choose **Restart** from the tray menu.

```ini
[camera]
monitor = primary   ; "primary" or a display number (see "Open log" for the list)
fps = 30            ; 1-240, or "monitor" to match the display's refresh rate
width = 1920        ; output size; the desktop is scaled to fit (black bars if needed)
height = 1080       ; even numbers, 320x180 up to 3840x2160
cursor = true       ; show the mouse cursor
name = DeskCam      ; camera name shown in apps (Windows 10: run install.ps1 again after changing it)
backend = auto      ; auto, mf (Windows 11 virtual camera) or dshow (DirectShow filter)
capture = auto      ; Windows 10: Desktop Duplication; Windows 11: Windows.Graphics.Capture
```

Tips:
- Keep `fps = 30` for Discord. Discord sends the camera at 30 fps or less anyway, but it still processes every frame it receives: at `fps = 144` it can use many CPU cores and make your whole PC stutter.
- To find the display number, choose **Open log** in the tray menu. The top of the log lists every display with its number, resolution and refresh rate.
- `backend = auto` selects the normal camera path for your Windows version. For a DirectShow-only test on Windows 11, set `backend = dshow` and run `scripts\install.ps1 -Backend dshow`; this is not needed for normal use.
- `capture = auto` uses Desktop Duplication on Windows 10 to avoid the yellow capture border, falling back to Windows.Graphics.Capture if duplication is unavailable. Windows 11 keeps Windows.Graphics.Capture by default. You can explicitly choose `wgc` or `duplication`.

## Tray menu

| Item        | What it does                                          |
|-------------|-------------------------------------------------------|
| Open config | Opens `config.ini`                                    |
| Open log    | Opens `%LOCALAPPDATA%\DeskCam\deskcam.log`            |
| Restart     | Reloads settings and restarts the camera              |
| Exit        | Stops desktop capture and exits; removes the Windows 11 session camera |

Hover over the icon to see the current status: idle, streaming, or an error.

On Windows 10, the installed camera entries remain until uninstall; consumers receive black
frames while DeskCam is not running.

From a terminal, `deskcam.exe stop` closes a running instance.

## Updating

Download the newer installer and run it. It closes DeskCam (and, on Windows 10, asks to close apps that
are using the camera), updates it and keeps your settings.

## Uninstall

**Settings → Apps → Installed apps → DeskCam → Uninstall** (or *Add or remove programs*). This removes the
program, the camera and the autostart entry. Your `config.ini` is kept.

## Troubleshooting

- **The camera doesn't show up in apps.** Make sure DeskCam is running (look for the tray icon), then restart the app you're using. If the tray tooltip shows an error saying the virtual camera failed to start, run `install.ps1` as Administrator again.
- **The camera is black.**
  - Open the log from the tray menu. It should say `capture started` while an app is using the camera.
  - If you just changed settings, use **Restart** from the tray menu.
- **"Access denied" error.** Install with the installer (or `scripts\install.ps1` when building from source). Don't register the DLL from the `target` folder: Windows' camera service can't read files inside your user folder.
- **Check that frames are flowing (source checkout only).** Run `cargo run --release --example probe`. It opens the camera like an app would and prints the resolution, measured fps and brightness, and saves a snapshot to `probe.bmp`. (Windows 11 only for now.)
- **Windows 10: the camera doesn't show up in an app.** Upgrade with the current installer and fully quit and reopen the app. AMD Privacy View and other Media Foundation/WinRT consumers need the software camera registered by `deskcam_source.dll`; DirectShow consumers also need the matching 64-/32-bit filter. The installer includes both. Collect a diagnostic report if it is still missing.
- **Windows 10: a yellow border appears around the screen while streaming.** This is Windows.Graphics.Capture's border. `capture = auto` normally avoids it using Desktop Duplication; `capture = wgc` or a fallback to WGC can show it. The border is drawn on your screen, not in the video.
- **Windows 10: install.ps1 says files are in use.** An app that used the camera still has the filter loaded. The installer works around this, but close those apps before uninstalling.

If none of this helps, see [Debugging and reporting problems](#debugging-and-reporting-problems).

## Debugging and reporting problems

Still stuck? Please [open a bug report](https://github.com/sinnafuls/rust-virtual-cam/issues/new?template=bug_report.yml).
A diagnostic report from your PC usually shows the cause much faster than a description alone, and
making one takes a few minutes. Nothing is uploaded automatically: you decide what to share.

### 1. Try the quick fixes

- Go through the [Troubleshooting](#troubleshooting) list above.
- Right-click the DeskCam tray icon → **Restart**. Then fully close and reopen the app that should see
  the camera (Discord keeps running in the tray: right-click its tray icon → **Quit Discord**).
- Check that you have the [latest release](https://github.com/sinnafuls/rust-virtual-cam/releases/latest).

### 2. Run DeskCam Diagnostics

Before you start:

- **Leave DeskCam running** (tray icon visible). If DeskCam won't start or keeps closing, run the
  diagnostics anyway.
- **Leave the app with the problem open**, ideally while it shows the problem (for example with
  DeskCam selected as its camera).
- Use your **normal Windows account** on the PC with the problem. **Don't** choose *Run as administrator*.

Then start the tool:

- **Start menu → DeskCam Diagnostics.** The installer adds it. The program itself is
  `C:\Program Files\DeskCam\deskcam-diagnostics.exe` (also installed there by `scripts\install.ps1`).
- **No "DeskCam Diagnostics" in your Start menu?** Installs from older releases don't include it.
  Download **[DeskCam-Diagnostics.zip](https://github.com/sinnafuls/rust-virtual-cam/releases/latest/download/DeskCam-Diagnostics.zip)**,
  right-click it → **Extract All**, open the extracted `DeskCam-Diagnostics` folder and double-click
  **`deskcam-diagnostics.exe`**. Don't run it from inside the ZIP, and keep the `x86` folder next to it:
  it checks what 32-bit apps see. Nothing needs to be installed.
- If Windows says **"Windows protected your PC"**, click **More info → Run anyway** (the tool isn't
  code-signed, like the installer).

A console window lists each check as it runs. The tool briefly opens the DeskCam camera the way an app
would, so DeskCam may start capturing for a moment and Windows may draw its yellow capture border around
the screen. It saves no screenshots or video and changes no settings. Wait until it prints
**Report saved to:** followed by the report's path, then press Enter to close the window.

The report is a text file named `diagnostics-<numbers>.txt`, normally in `%LOCALAPPDATA%\DeskCam`
(paste that into the File Explorer address bar to open the folder).

### 3. Check the report before sharing it

GitHub issues are **public**. Open the report in Notepad and look through it before attaching it. It
contains your Windows account name and SID, the PC's camera and device names, file and process paths,
your DeskCam settings and recent lines from the DeskCam log. Replace anything you don't want to share
with `<redacted>` instead of deleting whole sections. Keep the diagnostic findings and error codes,
redacting private identifiers within `[FAIL]`/`[WARN]` lines too when needed.

### 4. Open a bug report

Open the **[bug report form](https://github.com/sinnafuls/rust-virtual-cam/issues/new?template=bug_report.yml)**
(you need a free GitHub account) and fill it in:

- **DeskCam version:** Settings → Apps → Installed apps → DeskCam, or the `DeskCam x.y.z starting`
  line in the log (tray menu → **Open log**).
- **Windows version:** press Win+R, type `winver` and press Enter. Copy the line that looks like
  *Version 24H2 (OS Build 26100.2033)*.
- **The app with the problem** and its version, for example *Discord 1.0.9170*, *OBS 31.0.2* or
  *Chrome 130*, and whether it's 32-bit or 64-bit if you know (Task Manager → Details → right-click a
  column header → Select columns → Platform).
- **Steps to reproduce**, what you **expected** and what **actually happened** (no camera in the list,
  an error when opening it, black or frozen video, …).
- **New or regression:** whether this ever worked, and with which DeskCam version.
- **The report:** drag the `.txt` file onto the *Diagnostic report* box (or click the bar at the bottom
  of the box to pick it). GitHub uploads it and inserts a link. Only paste the text itself if attaching
  fails.

**If DeskCam Diagnostics won't start, is blocked, or closes without saying *Report saved to*,** report
the problem anyway. Write down the exact message (from Windows, your antivirus or the console window) or
attach a screenshot of it, say where you got the tool (Start menu or ZIP) and fill in the rest of the form.

### What the report checks

For anyone reading a report. The report checks the actual Windows build/edition; backend settings;
running DeskCam, Discord and AMD processes and their sessions/architectures; both COM/DirectShow
registry views and per-user overrides; installed DLL paths/architectures; Media Foundation
availability; capture APIs, monitors and D3D11 adapters; Frame Server services; camera/capture consent
and policies; and the latest DeskCam log. It enumerates cameras through both DirectShow and Media
Foundation. For the DirectShow backend it also activates the DeskCam CLSID, negotiates a capture graph
and counts delivered samples while checking the shared-memory writer, distinguishing black fallback
samples from published frames. Media Foundation is enumerated only: a friendly-name match is not
sufficient proof to safely choose a camera to open. The 64-bit tool runs the `x86\deskcam-diagnostics.exe`
helper to see the 32-bit registry and DirectShow view.

Checks run in timed child processes: a hung or crashing component does not discard the rest of the
report. Missing newer optional APIs are reported separately from required APIs. The tool does not
register cameras, change privacy settings, install anything or upload data.

For scripted runs: `deskcam-diagnostics.exe --no-pause --out report.txt`. The output file must not
already exist. Exit code 0 means the report was collected, **not** that every check passed. Inspect
`[FAIL]`, `[WARN]`, `[SKIP]`, HRESULTs and each probe's exit code. On Windows 11 a missing DirectShow
registration can be normal. On Windows 10 the installed software camera should also appear in Media
Foundation enumeration; if it is missing, check media-source registration and the Frame Server service.
Successful probes do not guarantee that a particular app accepts a camera or permits it in its sandbox.

## Building from source

Needs [Rust](https://rustup.rs/) (stable, `x86_64-pc-windows-msvc`) and
[Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the
**"Desktop development with C++"** workload.

```powershell
git clone https://github.com/sinnafuls/rust-virtual-cam.git
cd rust-virtual-cam
cargo build --release
# 32-bit camera filter (32-bit apps on Windows 10) and the 32-bit diagnostics helper;
# scripts\install.ps1 and the installer need both:
rustup target add i686-pc-windows-msvc
cargo build --release --target i686-pc-windows-msvc -p deskcam-dshow -p deskcam-diagnostics
```

Then either install straight from the build in an **Administrator PowerShell**:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install.ps1     # scripts\uninstall.ps1 to remove
```

or build the installer with [Inno Setup 6](https://jrsoftware.org/isinfo.php):

```powershell
ISCC.exe /DAppVersion=0.1.0 installer\deskcam.iss   # writes target\installer\DeskCam-Setup-0.1.0.exe
```

To build the portable `DeskCam-Diagnostics.zip` (64-bit tool plus its `x86` helper) on its own:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-diagnostics.ps1   # writes target\diagnostics\DeskCam-Diagnostics.zip
```

Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed.

## How it works

DeskCam has two parts: the app, and a DLL that apps talk to as if it were a camera.

- **`deskcam.exe`** runs in your session.
  - When an app starts using the camera, it captures the monitor with Windows.Graphics.Capture or Desktop Duplication, then scales it and converts it to NV12 on the GPU.
  - It writes each frame into shared memory.
- **Windows 11:** `deskcam.exe` registers the camera with `MFCreateVirtualCamera`. **`deskcam_source.dll`**, a Media Foundation media source, is loaded by the Windows Camera Frame Server. It reads the newest frame from shared memory and delivers it to apps.
- **Windows 10:** the installer registers **`deskcam_dshow.dll`** for DirectShow consumers and
  **`deskcam_source.dll`** for Media Foundation/WinRT consumers. Source registration creates
  `SWD\DeskCam\DeskCam0` with a camera interface pointing to the source's COM class through
  `CustomCaptureSourceClsid`. Windows Camera Frame Server loads that source in its own service.
  The worker supplies the session-local DirectShow ring and the global Frame Server ring from
  one capture/conversion pipeline.

Either way, the app only captures the screen while some app is reading frames.

```
crates/
  deskcam/          tray app: config, screen capture, GPU conversion
  deskcam-source/   media source DLL loaded by Windows' camera service on Windows 10 and 11
  deskcam-dshow/    Windows 10: DirectShow capture filter DLL loaded by camera apps
  deskcam-diagnostics/ portable prerequisite, registration and camera-discovery reporter
  deskcam-proto/    shared-memory layout, format conversion and helpers shared by the crates
scripts/            install.ps1 / uninstall.ps1: install from a source build
installer/          deskcam.iss: the Inno Setup installer published in releases
```

Credits: the media source design follows [smourier/VCamSample](https://github.com/smourier/VCamSample)
and [bj-rn/VL.Video.VirtualCamera](https://github.com/bj-rn/VL.Video.VirtualCamera). The Windows 10
filter was written from the DirectShow documentation, using OBS Studio's virtual camera as a
behavioral reference (no OBS code is included).
