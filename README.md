# DeskCam

Share your desktop as a **webcam** on Windows 11. DeskCam adds a camera named
**"DeskCam (Windows Virtual Camera)"** that streams one of your monitors, so you can pick it in
Discord, OBS, AMD Privacy View, browsers, or any other app that uses a camera.

- Written in Rust, with no dependencies beyond the Windows APIs.
- Runs in the background with a tray icon.
- Uses almost no CPU while no app is using the camera; it only captures while something is watching.
- Configured with a small `config.ini` file.

> Windows 11 only. The virtual camera API it uses (`MFCreateVirtualCamera`) does not exist on Windows 10.

## Requirements

- Windows 11
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

2. Open **PowerShell as Administrator** in the same folder and run the installer:

   ```powershell
   powershell -ExecutionPolicy Bypass -File scripts\install.ps1
   ```

   The installer does the following:
   - copies the program to `C:\Program Files\DeskCam`;
   - registers the camera with Windows (this step needs administrator rights);
   - creates the settings folder `C:\ProgramData\DeskCam`;
   - makes DeskCam start automatically when you log in;
   - starts DeskCam right away. The tray icon may be hidden behind the `^` arrow on the taskbar.

3. Open Discord (or OBS, your browser, and so on) and select **DeskCam (Windows Virtual Camera)** as your camera:
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
name = DeskCam      ; camera name shown in apps
```

Tips:
- Discord's camera feed normally runs at 30 fps or less, so `fps = 30` is a good default.
- To find the display number, choose **Open log** in the tray menu. The top of the log lists every display with its number, resolution and refresh rate.

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
- **Check that frames are flowing.** Run `cargo run --release --example probe`. It opens the camera like an app would and prints the resolution, measured fps and brightness, and saves a snapshot to `probe.bmp`.

## How it works

DeskCam has two parts:

- **`deskcam.exe`** runs in your session.
  - It registers the virtual camera with `MFCreateVirtualCamera`.
  - When an app starts using the camera, it captures the monitor with Windows.Graphics.Capture, then scales it and converts it to NV12 on the GPU.
  - It writes each frame into shared memory.
- **`deskcam_source.dll`** is a Media Foundation media source.
  - It is loaded by the Windows Camera Frame Server.
  - It reads the newest frame from shared memory and delivers it to apps at the configured frame rate.

```
crates/
  deskcam/          tray app: config, screen capture, GPU conversion
  deskcam-source/   media source DLL loaded by Windows' camera service
  deskcam-proto/    shared-memory layout used by both
scripts/            install.ps1 / uninstall.ps1
```

Credits: the media source design follows [smourier/VCamSample](https://github.com/smourier/VCamSample)
and [bj-rn/VL.Video.VirtualCamera](https://github.com/bj-rn/VL.Video.VirtualCamera).
