use std::path::PathBuf;

use global_hotkey::hotkey::Code;
use serde::Deserialize;

/// Resolve a file path next to the running executable, falling back to the bare filename.
pub fn exe_sibling(filename: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(filename)))
        .unwrap_or_else(|| PathBuf::from(filename))
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub clips_dir: PathBuf,
    pub hotkey: String,
    pub capture: CaptureConfig,
    pub buffer: BufferConfig,
    pub steam: SteamConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub fps: u64,
    pub scale: String,
    pub encoder: String,
    pub preset: String,
    pub rate_control: String,
    pub quality: String,
    pub segment_time: u64,
    /// Which monitor to capture: "auto" (follows the game window), or a 1-based index.
    /// Used as fallback when window capture is unavailable.
    pub monitor: String,
    /// Microphone device: "default" for system default, "none" to disable.
    pub microphone: String,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct BufferConfig {
    pub segments: usize,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SteamConfig {
    pub path: PathBuf,
    pub skip_appids: Vec<String>,
    pub poll_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clips_dir: dirs::video_dir()
                .unwrap_or_else(|| PathBuf::from("Videos"))
                .join("Snapple"),
            hotkey: "F8".into(),
            capture: CaptureConfig::default(),
            buffer: BufferConfig::default(),
            steam: SteamConfig::default(),
        }
    }
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fps: 60,
            scale: "scale=-2:720".into(),
            encoder: "h264_nvenc".into(),
            preset: "p4".into(),
            rate_control: "constqp".into(),
            quality: "28".into(),
            segment_time: 5,
            monitor: "auto".into(),
            microphone: "default".into(),
        }
    }
}

impl CaptureConfig {
    /// Return the ffmpeg flag name for the quality parameter based on rate_control mode.
    pub fn quality_flag(&self) -> &str {
        match self.rate_control.as_str() {
            "crf" => "-crf",
            "vbr" => "-b:v",
            _ => "-qp", // constqp, cqp
        }
    }
}

impl Default for BufferConfig {
    fn default() -> Self {
        Self { segments: 12 }
    }
}

impl Default for SteamConfig {
    fn default() -> Self {
        Self {
            path: detect_steam_path(),
            skip_appids: vec!["228980".into()],
            poll_interval_secs: 5,
        }
    }
}

fn detect_steam_path() -> PathBuf {
    if let Ok(hklm) = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE)
        .open_subkey(r"SOFTWARE\WOW6432Node\Valve\Steam")
    {
        if let Ok(path) = hklm.get_value::<String, _>("InstallPath") {
            return PathBuf::from(path);
        }
    }
    PathBuf::from(r"C:\Program Files (x86)\Steam")
}

pub fn load() -> Config {
    let config_path = exe_sibling("snapple.toml");

    match std::fs::read_to_string(&config_path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => {
                crate::log(&format!(
                    "[snapple] loaded config from {}",
                    config_path.display()
                ));
                cfg
            }
            Err(e) => {
                crate::log(&format!(
                    "[snapple] config parse error: {e}, using defaults"
                ));
                Config::default()
            }
        },
        Err(_) => {
            crate::log("[snapple] no snapple.toml found, using defaults");
            Config::default()
        }
    }
}

/// Compute the max age (in seconds) for segment cleanup during capture.
/// Keeps enough segments for a full clip plus one extra.
impl Config {
    pub fn cleanup_age_secs(&self) -> u64 {
        self.capture.segment_time * (self.buffer.segments as u64 + 1)
    }
}

pub fn parse_hotkey_code(name: &str) -> Option<Code> {
    match name.to_uppercase().as_str() {
        "F1" => Some(Code::F1),
        "F2" => Some(Code::F2),
        "F3" => Some(Code::F3),
        "F4" => Some(Code::F4),
        "F5" => Some(Code::F5),
        "F6" => Some(Code::F6),
        "F7" => Some(Code::F7),
        "F8" => Some(Code::F8),
        "F9" => Some(Code::F9),
        "F10" => Some(Code::F10),
        "F11" => Some(Code::F11),
        "F12" => Some(Code::F12),
        "PRINTSCREEN" => Some(Code::PrintScreen),
        "SCROLLLOCK" => Some(Code::ScrollLock),
        "PAUSE" => Some(Code::Pause),
        "INSERT" => Some(Code::Insert),
        "HOME" => Some(Code::Home),
        "END" => Some(Code::End),
        "DELETE" => Some(Code::Delete),
        _ => None,
    }
}
