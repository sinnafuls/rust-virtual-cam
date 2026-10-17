//! Read-only diagnostics. Native probes run in bounded child processes so a camera driver
//! crash, missing optional component, or COM hang cannot discard the rest of the report.

mod inventory;
mod native;

use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BUG_REPORT_URL: &str =
    "https://github.com/sinnafuls/rust-virtual-cam/issues/new?template=bug_report.yml";

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|a| a == "--probe") {
        let code = match args.get(1).and_then(|s| s.to_str()) {
            Some("inventory") => {
                inventory::run();
                0
            }
            Some("logs") => {
                inventory::logs();
                0
            }
            Some(mode @ ("graphics" | "mf" | "dshow")) => native::run(mode),
            _ => {
                eprintln!("[FAIL] Unknown internal probe mode");
                2
            }
        };
        std::process::exit(code);
    }
    let mut out = None;
    let mut pause = true;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        if arg == "--no-pause" {
            pause = false;
        } else if arg == "--out" {
            out = it.next().map(PathBuf::from);
            if out.is_none() {
                usage_error("--out needs a file path");
            }
        } else if arg == "--help" || arg == "-h" {
            println!(
                "DeskCam diagnostics\nUsage: deskcam-diagnostics.exe [--out report.txt] [--no-pause]\n\nRun as the affected user while DeskCam is running. No administrator rights needed.\nChecks registration, camera discovery, DLL activation, graphics support and sample delivery.\nBriefly opens only DeskCam; may trigger desktop capture and its yellow border.\nNo screenshots, uploads, installation or system-setting changes.\nReports contain account name/SID, camera/device names, process/file paths, configuration and recent DeskCam logs.\nExit 0 means the report was collected, not that every check passed.\n\nStill having trouble? Review/redact the report, then attach the .txt to a bug report:\n{BUG_REPORT_URL}"
            );
            return;
        } else {
            usage_error(&format!("Unknown argument: {}", arg.to_string_lossy()));
        }
    }
    println!("DeskCam diagnostics — leave DeskCam running in this desktop session.");
    println!(
        "No settings are changed. DeskCam will be opened briefly; capture may show a yellow border."
    );
    println!(
        "No screenshots or uploads. Review account identifiers, paths, device names and log text before sharing."
    );
    let result = collect(out);
    let code = match result {
        Ok(path) => {
            println!("\nReport saved to: {}", path.display());
            println!(
                "Next: review/redact personal details in the report before sharing it in a PUBLIC GitHub issue."
            );
            println!("Report a bug: {BUG_REPORT_URL}");
            println!(
                "Attach the reviewed .txt file; include DeskCam/app versions, Windows build and steps to reproduce."
            );
            0
        }
        Err(error) => {
            eprintln!("Cannot collect report: {error}");
            eprintln!("You can still report a bug without a diagnostic report: {BUG_REPORT_URL}");
            eprintln!("Include this exact error, your Windows build and DeskCam/app versions.");
            2
        }
    };
    if pause && io::stdin().is_terminal() {
        println!("Press Enter to close.");
        let _ = io::stdin().read_line(&mut String::new());
    }
    std::process::exit(code);
}

fn usage_error(message: &str) -> ! {
    eprintln!("{message}\nUsage: deskcam-diagnostics.exe [--out report.txt] [--no-pause]");
    std::process::exit(2);
}

fn create_report(requested: Option<PathBuf>) -> io::Result<(PathBuf, File)> {
    if let Some(path) = requested {
        // Never silently overwrite an existing report or a user file.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        return Ok((std::path::absolute(path)?, file));
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let name = format!("diagnostics-{stamp}-{}.txt", std::process::id());
    let mut bases = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        bases.push(PathBuf::from(local).join("DeskCam"));
    }
    bases.push(std::env::temp_dir().join("DeskCam"));
    let mut last_error = None;
    for base in bases {
        let path = base.join(&name);
        match fs::create_dir_all(&base)
            .and_then(|()| OpenOptions::new().write(true).create_new(true).open(&path))
        {
            Ok(file) => return Ok((path, file)),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::other("No writable report directory")))
}

fn collect(requested: Option<PathBuf>) -> io::Result<PathBuf> {
    let (path, mut file) = create_report(requested)?;
    let exe = std::env::current_exe()?;
    writeln!(
        file,
        "DeskCam diagnostic report v{} | {}-bit {}",
        env!("CARGO_PKG_VERSION"),
        usize::BITS,
        std::env::consts::ARCH
    )?;
    writeln!(
        file,
        "Executable: {}\nReport: {}",
        exe.display(),
        path.display()
    )?;
    writeln!(
        file,
        "UTC Unix seconds: {}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    )?;
    writeln!(
        file,
        "Read-only checks, except report creation and temporary camera use. No images saved or uploads."
    )?;
    writeln!(
        file,
        "Run as the affected user in the same desktop session as DeskCam, preferably unelevated."
    )?;
    writeln!(
        file,
        "Windows 10 installs DirectShow filters plus a driverless Frame Server camera for Media Foundation and WinRT apps."
    )?;
    writeln!(
        file,
        "Windows 11 normally uses MF instead: missing DirectShow registration there can be expected."
    )?;
    writeln!(
        file,
        "Probe success does not prove Discord/AMD acceptance or sandbox access. Only DeskCam is activated."
    )?;
    writeln!(
        file,
        "[WARN] Privacy: report contains account name/SID, process/file paths, device names, settings and recent DeskCam log text. Review before sharing."
    )?;
    println!("Writing {}", path.display());
    probe(
        &mut file,
        &exe,
        "inventory",
        "System, registration and policies",
        30,
    )?;
    probe(
        &mut file,
        &exe,
        "graphics",
        "Capture API and GPU prerequisites",
        30,
    )?;
    probe(
        &mut file,
        &exe,
        "mf",
        "Media Foundation device enumeration",
        30,
    )?;
    probe(
        &mut file,
        &exe,
        "dshow",
        "Native DirectShow enumeration, activation and sample delivery",
        45,
    )?;
    if usize::BITS == 64 {
        let helper = exe
            .parent()
            .unwrap_or(Path::new("."))
            .join("x86")
            .join("deskcam-diagnostics.exe");
        if helper.is_file() {
            probe(
                &mut file,
                &helper,
                "dshow",
                "32-bit DirectShow enumeration, activation and sample delivery",
                45,
            )?;
        } else {
            writeln!(
                file,
                "\n[SKIP] 32-bit activation/streaming: helper missing: {}",
                helper.display()
            )?;
            writeln!(
                file,
                "[WARN] Registry inspection alone cannot prove a 32-bit app can load the camera. Extract the complete diagnostics ZIP and rerun."
            )?;
        }
    }
    probe(
        &mut file,
        &exe,
        "logs",
        "DeskCam log after camera probes",
        15,
    )?;
    writeln!(file, "\n=== Collection complete ===")?;
    writeln!(
        file,
        "Interpret stages separately: missing category = discovery/registration; activation error = DLL/dependency/permissions; graph/sample failure = negotiation/delivery; stale writer = DeskCam/capture path."
    )?;
    writeln!(
        file,
        "A missing MF camera on Windows 10 indicates a missing or unavailable software-camera registration; compare DirectShow results too. Absent privacy keys are not proof of denial."
    )?;
    writeln!(
        file,
        "If these pass but an app does not list DeskCam, restart that app and supply its exact version and architecture with this report. App filtering/sandbox/Media Foundation-only behavior is not reproduced by this tool."
    )?;
    writeln!(
        file,
        "\nStill having trouble? Report a bug: {BUG_REPORT_URL}"
    )?;
    writeln!(
        file,
        "GitHub issues are public. Review/redact account identifiers, paths and log text, then attach this .txt file with DeskCam/app versions and steps to reproduce. Nothing is uploaded automatically."
    )?;
    file.sync_all()?;
    Ok(path)
}

fn probe(
    file: &mut File,
    executable: &Path,
    mode: &str,
    title: &str,
    seconds: u64,
) -> io::Result<()> {
    println!("Checking: {title} …");
    writeln!(
        file,
        "\n=== {title} ===\n[INFO] Probe: {} --probe {mode}; timeout {seconds}s",
        executable.display()
    )?;
    file.flush()?;
    let stdout = file.try_clone()?;
    let stderr = file.try_clone()?;
    let start = Instant::now();
    let mut command = Command::new(executable);
    command
        .args(["--probe", mode])
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW: no extra consoles per probe.
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            writeln!(
                file,
                "[FAIL] Cannot launch probe: {error} (OS error {:?})",
                error.raw_os_error()
            )?;
            return Ok(());
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let code = status
                    .code()
                    .map(|c| format!("{c} (0x{:08X})", c as u32))
                    .unwrap_or_else(|| "terminated without exit code".into());
                writeln!(
                    file,
                    "[{}] Probe exit: {code}; elapsed {:.2}s",
                    if status.success() { "INFO" } else { "FAIL" },
                    start.elapsed().as_secs_f64()
                )?;
                return Ok(());
            }
            Ok(None) if start.elapsed() < Duration::from_secs(seconds) => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Ok(None) => {
                let killed = child.kill();
                if killed.is_ok() {
                    let _ = child.wait();
                }
                writeln!(
                    file,
                    "[FAIL] Probe timed out after {seconds}s; termination: {killed:?}. Last stage above identifies the hang."
                )?;
                return Ok(());
            }
            Err(error) => {
                let killed = child.kill();
                if killed.is_ok() {
                    let _ = child.wait();
                }
                writeln!(
                    file,
                    "[FAIL] Cannot wait for probe: {error}; termination: {killed:?}"
                )?;
                return Ok(());
            }
        }
    }
}
