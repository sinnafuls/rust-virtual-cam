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

> **Windows 10 support is new and not yet tested on real hardware.** See [docs/WIN10_PLAN.md](docs/WIN10_PLAN.md).

## Requirements

- Windows 11, or 64-bit Windows 10 version 1903 or newer
- [Rust](https://rustup.rs/), stable, with the default `x86_64-pc-windows-msvc` toolchain
- [Visual Studio Build Tools](https://visualstudio.microsoft.com/visual-cpp-build-tools/) with the
  **"Desktop development with C++"** workload (for the MSVC linker and Windows SDK)

## Install

1. Build it:

   ```powershell
   git clone https://github.com/sinnafuls/rust-virtual-cam.git
   cd rust-virtual-cam
   cargo build --release
   ```

   **Windows 10 only:** also build the 32-bit camera filter, so 32-bit apps can see the camera too:

   ```powershell
   rustup target add i686-pc-windows-msvc
   cargo build --release --target i686-pc-windows-msvc -p deskcam-dshow
   ```

2. Open **PowerShell as Administrator** in the same folder and run the installer:

   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\install.ps1
   ```

   The installer does the following:
   - copies the program to `C:\Program Files\DeskCam`;
   - registers the camera with Windows (this step needs administrator rights). On Windows 10 it registers the DirectShow filter, 64-bit and 32-bit;
   - creates the settings folder `C:\ProgramData\DeskCam`;
   - makes DeskCam start automatically when you log in;
   - starts DeskCam right away. The tray icon may be hidden behind the `^` arrow on the taskbar.

3. Open Discord (or OBS, your browser, and so on) and select **DeskCam (Windows Virtual Camera)** (Windows 11) or **DeskCam** (Windows 10) as your camera. On Windows 10, restart any app that was already open:
   - **Discord:** Settings → Voice & Video → Camera
   - **OBS:** Sources → + → Video Capture Device → DeskCam

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
- Discord's camera feed normally runs at 30 fps or less, so `fps = 30` is a good default.
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

Pull the latest code, run `cargo build --release`, then run `scripts\install.ps1` as Administrator again.

## Uninstall

In an Administrator PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\uninstall.ps1
```

This removes the program, the camera registration and the autostart entry. Your `config.ini` is kept.

## Troubleshooting

- **The camera doesn't show up in apps.** Make sure DeskCam is running (look for the tray icon), then restart the app you're using. If the tray tooltip shows an error saying the virtual camera failed to start, run `install.ps1` as Administrator again.
- **The camera is black.**
  - Open the log from the tray menu. It should say `capture started` while an app is using the camera.
  - If you just changed settings, use **Restart** from the tray menu.
- **"Access denied" error.** Always install with `scripts\install.ps1`. Don't register the DLL from the `target` folder: Windows' camera service can't read files inside your user folder.
- **Check that frames are flowing.** Run `cargo run --release --example probe`. It opens the camera like an app would and prints the resolution, measured fps and brightness, and saves a snapshot to `probe.bmp`. (Windows 11 only for now.)
- **Windows 10: the camera doesn't show up in an app.** Restart the app after installing. The built-in Windows Camera app can't see DeskCam on Windows 10. For a 32-bit app, make sure you built and installed the 32-bit filter (see Install).
- **Windows 10: a yellow border appears around the screen while streaming.** Windows 10 always shows it during screen capture and it can't be turned off. It is drawn on your screen, not in the video.
- **Windows 10: install.ps1 says files are in use.** An app that used the camera still has the filter loaded. The installer works around this, but close those apps before uninstalling.

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
scripts/            install.ps1 / uninstall.ps1
docs/               WIN10_PLAN.md: design of the Windows 10 support
```

Credits: the media source design follows [smourier/VCamSample](https://github.com/smourier/VCamSample)
and [bj-rn/VL.Video.VirtualCamera](https://github.com/bj-rn/VL.Video.VirtualCamera). The Windows 10
filter was written from the DirectShow documentation, using OBS Studio's virtual camera as a
behavioral reference (no OBS code is included).
