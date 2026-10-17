## Install

1. Download **DeskCam-Setup-*.exe** below and run it. Accept the administrator prompt.
   Windows may say "Windows protected your PC" because the installer isn't code-signed: click
   **More info → Run anyway**.
2. Pick whether DeskCam starts when you sign in and whether to add a desktop shortcut.
3. In Discord, OBS or your browser, choose the **DeskCam** camera.

Works on Windows 11 and 64-bit Windows 10 (version 1903 or newer); the installer picks the right
camera for your Windows version. Settings: right-click the tray icon → **Open config**. For Discord,
keep `fps = 30`.

To update, run the newer installer. To remove: Settings → Apps → DeskCam → Uninstall.

## Something not working?

Create a diagnostic report and [open a bug report](https://github.com/sinnafuls/rust-virtual-cam/issues/new?template=bug_report.yml).

- **Installed with this release:** Start menu → **DeskCam Diagnostics**.
- **Installed from an older release:** download **DeskCam-Diagnostics.zip** below, extract all files
  and run **deskcam-diagnostics.exe** from the extracted folder (keep the `x86` folder beside it).

Run it as your normal user, not as administrator, while DeskCam and the affected app are running. It
saves a text report (the window shows where) and changes no settings. It briefly opens the camera,
which can show Windows' yellow capture border, but saves no screenshots or video and uploads nothing.
The report contains your account name, device names, paths and log lines: review it before attaching
it to the issue. If the tool won't run, open the issue anyway with the exact error message or a
screenshot. Full steps: [Debugging and reporting problems](https://github.com/sinnafuls/rust-virtual-cam#debugging-and-reporting-problems).
