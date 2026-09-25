//! `C:\ProgramData\DeskCam\config.ini`.

use std::fs;

use deskcam_proto::paths;

pub const DEFAULT_TEXT: &str = r#"; DeskCam configuration. Use tray menu "Restart" after editing.
[camera]
; Display to capture: "primary" or N from \\.\DISPLAYN (see deskcam.log for the list)
monitor = primary
; Frames per second (1-240) or "monitor" to use the display's refresh rate
fps = 30
; Output size; the desktop is scaled to fit and letterboxed. Even numbers, 320x180 .. 3840x2160
width = 1920
height = 1080
; Draw the mouse cursor
cursor = true
; Camera name shown in apps (Windows appends "Windows Virtual Camera")
name = DeskCam
"#;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MonitorSel {
    Primary,
    Display(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FpsSel {
    Fixed(u32),
    Monitor,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Config {
    pub monitor: MonitorSel,
    pub fps: FpsSel,
    pub width: u32,
    pub height: u32,
    pub cursor: bool,
    pub name: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            monitor: MonitorSel::Primary,
            fps: FpsSel::Fixed(30),
            width: 1920,
            height: 1080,
            cursor: true,
            name: "DeskCam".into(),
        }
    }
}

fn parse_even(key: &str, value: &str, min: u32, max: u32, line: usize) -> Result<u32, String> {
    match value.parse::<u32>() {
        Ok(v) if (min..=max).contains(&v) && v % 2 == 0 => Ok(v),
        _ => Err(format!("config.ini line {line}: {key} must be an even number from {min} to {max}, got '{value}'")),
    }
}

pub fn parse(text: &str) -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut in_camera = false;
    for (idx, raw) in text.lines().enumerate() {
        let n = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if !section.trim().eq_ignore_ascii_case("camera") {
                return Err(format!("config.ini line {n}: unknown section '[{}]'", section.trim()));
            }
            in_camera = true;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("config.ini line {n}: expected 'key = value'"));
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        if !in_camera {
            return Err(format!("config.ini line {n}: key '{key}' must be inside [camera]"));
        }
        match key.as_str() {
            "monitor" => {
                cfg.monitor = if value.eq_ignore_ascii_case("primary") {
                    MonitorSel::Primary
                } else {
                    match value.parse::<u32>() {
                        Ok(v) if v >= 1 => MonitorSel::Display(v),
                        _ => return Err(format!("config.ini line {n}: monitor must be 'primary' or a display number, got '{value}'")),
                    }
                }
            }
            "fps" => {
                cfg.fps = if value.eq_ignore_ascii_case("monitor") {
                    FpsSel::Monitor
                } else {
                    match value.parse::<u32>() {
                        Ok(v) if (1..=240).contains(&v) => FpsSel::Fixed(v),
                        _ => return Err(format!("config.ini line {n}: fps must be 1-240 or 'monitor', got '{value}'")),
                    }
                }
            }
            "width" => cfg.width = parse_even("width", value, 320, 3840, n)?,
            "height" => cfg.height = parse_even("height", value, 180, 2160, n)?,
            "cursor" => {
                cfg.cursor = match value.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" => true,
                    "false" | "0" | "no" => false,
                    _ => return Err(format!("config.ini line {n}: cursor must be true or false, got '{value}'")),
                }
            }
            "name" => {
                let len = value.chars().count();
                if !(1..=64).contains(&len) {
                    return Err(format!("config.ini line {n}: name must be 1-64 characters"));
                }
                cfg.name = value.to_owned();
            }
            _ => return Err(format!("config.ini line {n}: unknown key '{key}'")),
        }
    }
    Ok(cfg)
}

/// Loads the config, writing the default file first when it does not exist.
pub fn load() -> Result<Config, String> {
    let path = paths::config_file();
    if !path.exists() {
        fs::create_dir_all(paths::data_dir()).map_err(|e| format!("cannot create {}: {e}", paths::data_dir().display()))?;
        fs::write(&path, DEFAULT_TEXT).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }
    let text = fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_text_parses_to_defaults() {
        assert_eq!(parse(DEFAULT_TEXT).unwrap(), Config::default());
    }

    #[test]
    fn fps_monitor_and_display_number() {
        let cfg = parse("[camera]\nfps = monitor\nmonitor = 2\n").unwrap();
        assert_eq!(cfg.fps, FpsSel::Monitor);
        assert_eq!(cfg.monitor, MonitorSel::Display(2));
    }

    #[test]
    fn odd_width_rejected_with_line() {
        let err = parse("[camera]\n; c\nwidth = 1921\n").unwrap_err();
        assert!(err.contains("line 3") && err.contains("width"), "{err}");
    }

    #[test]
    fn unknown_key_rejected_with_line() {
        let err = parse("[camera]\nfsp = 30\n").unwrap_err();
        assert!(err.contains("line 2") && err.contains("fsp"), "{err}");
    }
}
