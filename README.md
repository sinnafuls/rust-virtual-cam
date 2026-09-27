# DeskCam

Share your desktop as a **webcam** on Windows 11 and Windows 10. DeskCam adds a camera named
**"DeskCam"** that streams one of your monitors, so you can pick it in Discord, OBS, AMD Privacy
View, browsers, or any other app that uses a camera.

- Written in Rust, with no dependencies beyond the Windows APIs.
- Runs in the background with a tray icon.
- Uses almost no CPU while no app is using the camera; it only captures while something is watching.
- Configured with a small `config.ini` file.

| | How the camera is added | Which apps see it |
|---|---|---|
| **Windows 11** | Windows' virtual camera API (`MFCreateVirtualCamera`); shows as "DeskCam (Windows Virtual Camera)" | All camera apps |
| **Windows 10** (1903 or newer, 64-bit) | A DirectShow capture filter (the same method OBS Virtual Camera uses) | Discord, OBS, Chrome, Edge, Firefox, Zoom, Teams and other DirectShow apps. **Not** the built-in Windows Camera app or other Media Foundation-only apps |

> Windows 10 support is new: it passes automated tests on Windows 10 (browsers, 64- and 32-bit apps),
> but hasn't been tried in Discord, OBS or Zoom on Windows 10 yet. See [docs/WIN10_PLAN.md](docs/WIN10_PLAN.md).

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

The installer detects your Windows version and installs the right camera for it: the Windows 11 virtual
camera, or on Windows 10 the DirectShow camera for both 64-bit and 32-bit apps.

Requirements: Windows 11, or 64-bit Windows 10 version 1903 or newer.

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
```

Tips:
- Keep `fps = 30` for Discord. Discord sends the camera at 30 fps or less anyway, but it still processes every frame it receives: at `fps = 144` it can use many CPU cores and make your whole PC stutter.
- To find the display number, choose **Open log** in the tray menu. The top of the log lists every display with its number, resolution and refresh rate.
- `backend = auto` picks the right method for your Windows version. To try the Windows 10 method on Windows 11, set `backend = dshow` and run `install.ps1 -Backend dshow`.

## Tray menu

| Item        | What it does                                          |
|-------------|-------------------------------------------------------|
| Open config | Opens `config.ini`                                    |
| Open log    | Opens `%LOCALAPPDATA%\DeskCam\deskcam.log`            |
| Restart     | Reloads settings and restarts the camera              |
| Exit        | Stops DeskCam and removes the camera until next start |

Hover over the icon to see the current status: idle, streaming, or an error.

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
- **Check that frames are flowing.** Run `cargo run --release --example probe`. It opens the camera like an app would and prints the resolution, measured fps and brightness, and saves a snapshot to `probe.bmp`. (Windows 11 only for now.)
- **Windows 10: the camera doesn't show up in an app.** Restart the app after installing. The built-in Windows Camera app can't see DeskCam on Windows 10. For a 32-bit app, make sure you built and installed the 32-bit filter (see Install).
- **Windows 10: a yellow border appears around the screen while streaming.** Windows 10 always shows it during screen capture and it can't be turned off. It is drawn on your screen, not in the video.
- **Windows 10: install.ps1 says files are in use.** An app that used the camera still has the filter loaded. The installer works around this, but close those apps before uninstalling.

## Building from source

Needs [Rust](https://rustup.rs/) (stable, `x86_64-pc-windows-msvc`) and
[Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the
**"Desktop development with C++"** workload.

```powershell
git clone https://github.com/sinnafuls/rust-virtual-cam.git
cd rust-virtual-cam
cargo build --release
# 32-bit camera filter for 32-bit apps on Windows 10:
rustup target add i686-pc-windows-msvc
cargo build --release --target i686-pc-windows-msvc -p deskcam-dshow
```

Then either install straight from the build in an **Administrator PowerShell**:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\install.ps1     # scripts\uninstall.ps1 to remove
```

or build the installer with [Inno Setup 6](https://jrsoftware.org/isinfo.php):

```powershell
ISCC.exe /DAppVersion=0.1.0 installer\deskcam.iss   # writes target\installer\DeskCam-Setup-0.1.0.exe
```

Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed.

## How it works

DeskCam has two parts: the app, and a DLL that apps talk to as if it were a camera.

- **`deskcam.exe`** runs in your session.
  - When an app starts using the camera, it captures the monitor with Windows.Graphics.Capture, then scales it and converts it to NV12 on the GPU.
  - It writes each frame into shared memory.
- **Windows 11:** `deskcam.exe` registers the camera with `MFCreateVirtualCamera`. **`deskcam_source.dll`**, a Media Foundation media source, is loaded by the Windows Camera Frame Server. It reads the newest frame from shared memory and delivers it to apps.
- **Windows 10:** `install.ps1` registers **`deskcam_dshow.dll`**, a DirectShow capture filter, as a video device. Each app that opens the camera loads it into its own process. It reads the newest frame from the shared memory `deskcam.exe` created and delivers it as NV12, I420 or YUY2.

Either way, the app only captures the screen while some app is reading frames.

```
crates/
  deskcam/          tray app: config, screen capture, GPU conversion
  deskcam-source/   Windows 11: media source DLL loaded by Windows' camera service
  deskcam-dshow/    Windows 10: DirectShow capture filter DLL loaded by camera apps
  deskcam-proto/    shared-memory layout, format conversion and helpers used by all three
scripts/            install.ps1 / uninstall.ps1: install from a source build
installer/          deskcam.iss: the Inno Setup installer published in releases
docs/               WIN10_PLAN.md: design of the Windows 10 support
```

Credits: the media source design follows [smourier/VCamSample](https://github.com/smourier/VCamSample)
and [bj-rn/VL.Video.VirtualCamera](https://github.com/bj-rn/VL.Video.VirtualCamera). The Windows 10
filter was written from the DirectShow documentation, using OBS Studio's virtual camera as a
behavioral reference (no OBS code is included).
