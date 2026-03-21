use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sysinfo::System;

use crate::config::SteamConfig;

/// Extract quoted key-value pairs from VDF/ACF text.
fn parse_kv_pairs(text: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c == '"' {
            chars.next();
            let key: String = chars.by_ref().take_while(|&c| c != '"').collect();
            // skip whitespace between key and value
            while chars.peek().map_or(false, |c| c.is_whitespace()) {
                chars.next();
            }
            if chars.peek() == Some(&'"') {
                chars.next();
                let val: String = chars.by_ref().take_while(|&c| c != '"').collect();
                pairs.push((key, val));
            }
        } else {
            chars.next();
        }
    }
    pairs
}

/// Parse libraryfolders.vdf to find all Steam library paths.
fn parse_library_folders(steam_dir: &Path) -> Vec<PathBuf> {
    let vdf = steam_dir.join("config").join("libraryfolders.vdf");
    let text = match std::fs::read_to_string(&vdf) {
        Ok(t) => t,
        Err(_) => return vec![steam_dir.to_path_buf()],
    };

    let mut libraries = Vec::new();
    for (key, val) in parse_kv_pairs(&text) {
        if key == "path" {
            libraries.push(PathBuf::from(val));
        }
    }
    if libraries.is_empty() {
        libraries.push(steam_dir.to_path_buf());
    }
    libraries
}

/// Discover installed Steam games.
/// Returns vec of (lowercase installdir, game name).
fn load_steam_games(steam_path: &Path, skip_appids: &[String]) -> Vec<(String, String)> {
    let libraries = parse_library_folders(steam_path);
    let mut games = Vec::new();

    for lib in &libraries {
        let steamapps = lib.join("steamapps");
        if !steamapps.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(&steamapps) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("appmanifest_") || !name_str.ends_with(".acf") {
                continue;
            }
            let text = match std::fs::read_to_string(entry.path()) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let pairs = parse_kv_pairs(&text);
            let mut appid = "";
            let mut game_name = "";
            let mut installdir = "";
            for (k, v) in &pairs {
                match k.as_str() {
                    "appid" => appid = v,
                    "name" => game_name = v,
                    "installdir" => installdir = v,
                    _ => {}
                }
            }
            if skip_appids.iter().any(|id| id == appid)
                || game_name.is_empty()
                || installdir.is_empty()
            {
                continue;
            }
            games.push((installdir.to_lowercase(), game_name.to_string()));
        }
    }
    games
}

pub enum GameEvent {
    Started { name: String, pid: u32 },
    Stopped,
}

/// Polls running processes to detect Steam games.
pub fn spawn_monitor(tx: mpsc::Sender<GameEvent>, steam_cfg: SteamConfig) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let games = load_steam_games(&steam_cfg.path, &steam_cfg.skip_appids);
        let marker = "steamapps\\common\\";
        let mut current_game: Option<String> = None;
        let mut sys = System::new();

        loop {
            sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

            let mut detected: Option<(String, u32)> = None;
            for (pid, process) in sys.processes() {
                let exe = match process.exe() {
                    Some(e) => e,
                    None => continue,
                };
                let exe_str = exe.to_string_lossy().to_lowercase();
                if let Some(idx) = exe_str.find(marker) {
                    let after = &exe_str[idx + marker.len()..];
                    let installdir = match after.split('\\').next() {
                        Some(d) => d,
                        None => continue,
                    };
                    for (dir, name) in &games {
                        if dir == installdir {
                            detected = Some((name.clone(), pid.as_u32()));
                            break;
                        }
                    }
                    if detected.is_some() {
                        break;
                    }
                }
            }

            match (&current_game, &detected) {
                (None, Some((name, pid))) => {
                    current_game = Some(name.clone());
                    let _ = tx.send(GameEvent::Started {
                        name: name.clone(),
                        pid: *pid,
                    });
                }
                (Some(_), None) => {
                    current_game = None;
                    let _ = tx.send(GameEvent::Stopped);
                }
                _ => {}
            }

            thread::sleep(Duration::from_secs(steam_cfg.poll_interval_secs));
        }
    })
}
