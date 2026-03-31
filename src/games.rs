use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use sysinfo::System;

use crate::config::{ExtraGame, SteamConfig};

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
    /// A different game replaced the currently tracked one.
    /// The receiver should stop the old capture, then start the new one.
    Switched { name: String, pid: u32 },
}

fn next_game_event(
    current: Option<(&str, u32)>,
    detected: Option<(&str, u32)>,
) -> Option<GameEvent> {
    match (current, detected) {
        (None, Some((name, pid))) => Some(GameEvent::Started {
            name: name.to_string(),
            pid,
        }),
        (Some((cur_name, cur_pid)), Some((name, pid)))
            if cur_name != name || cur_pid != pid =>
        {
            Some(GameEvent::Switched {
                name: name.to_string(),
                pid,
            })
        }
        (Some(_), None) => Some(GameEvent::Stopped),
        _ => None,
    }
}

/// Polls running processes to detect Steam and non-Steam games.
pub fn spawn_monitor(
    tx: mpsc::Sender<GameEvent>,
    steam_cfg: SteamConfig,
    extra_games: Vec<ExtraGame>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let steam_games = load_steam_games(&steam_cfg.path, &steam_cfg.skip_appids);
        let marker = "steamapps\\common\\";

        // Pre-lowercase extra game exe names for fast comparison.
        let extra: Vec<(String, String)> = extra_games
            .iter()
            .map(|g| (g.exe.to_lowercase(), g.name.clone()))
            .collect();

        let mut current_game: Option<(String, u32)> = None;
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

                // Check Steam games (by install directory).
                if let Some(idx) = exe_str.find(marker) {
                    let after = &exe_str[idx + marker.len()..];
                    let installdir = match after.split('\\').next() {
                        Some(d) => d,
                        None => continue,
                    };
                    for (dir, name) in &steam_games {
                        if dir == installdir {
                            detected = Some((name.clone(), pid.as_u32()));
                            break;
                        }
                    }
                    if detected.is_some() {
                        break;
                    }
                }

                // Check extra (non-Steam) games by executable name.
                if let Some(file_name) = exe.file_name() {
                    let file_lower = file_name.to_string_lossy().to_lowercase();
                    for (exe_name, game_name) in &extra {
                        if file_lower == *exe_name {
                            detected = Some((game_name.clone(), pid.as_u32()));
                            break;
                        }
                    }
                    if detected.is_some() {
                        break;
                    }
                }
            }

            if let Some(event) = next_game_event(
                current_game.as_ref().map(|(name, pid)| (name.as_str(), *pid)),
                detected.as_ref().map(|(name, pid)| (name.as_str(), *pid)),
            ) {
                match &event {
                    GameEvent::Started { name, pid } | GameEvent::Switched { name, pid } => {
                        current_game = Some((name.clone(), *pid));
                    }
                    GameEvent::Stopped => {
                        current_game = None;
                    }
                }
                let _ = tx.send(event);
            }

            thread::sleep(Duration::from_secs(steam_cfg.poll_interval_secs));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{next_game_event, parse_kv_pairs, GameEvent};

    #[test]
    fn parses_vdf_key_value_pairs() {
        let text = "\"libraryfolders\"\n{\n    \"0\"\n    {\n        \"path\"    \"D:\\\\SteamLibrary\"\n    }\n}";
        let pairs = parse_kv_pairs(text);
        assert!(pairs.iter().any(|(k, v)| k == "path" && v == r"D:\\SteamLibrary"));
    }

    #[test]
    fn emits_started_when_first_game_appears() {
        let event = next_game_event(None, Some(("Game A", 101)));
        assert!(matches!(
            event,
            Some(GameEvent::Started { name, pid }) if name == "Game A" && pid == 101
        ));
    }

    #[test]
    fn emits_switch_when_different_game_is_detected() {
        let event = next_game_event(Some(("Game A", 101)), Some(("Game B", 202)));
        assert!(matches!(
            event,
            Some(GameEvent::Switched { name, pid }) if name == "Game B" && pid == 202
        ));
    }

    #[test]
    fn emits_switch_when_same_game_restarts_with_new_pid() {
        let event = next_game_event(Some(("Game A", 101)), Some(("Game A", 202)));
        assert!(matches!(
            event,
            Some(GameEvent::Switched { name, pid }) if name == "Game A" && pid == 202
        ));
    }

    #[test]
    fn emits_stopped_when_last_game_closes() {
        let event = next_game_event(Some(("Game A", 101)), None);
        assert!(matches!(event, Some(GameEvent::Stopped)));
    }

    #[test]
    fn emits_nothing_when_same_game_and_pid_remain() {
        let event = next_game_event(Some(("Game A", 101)), Some(("Game A", 101)));
        assert!(event.is_none());
    }
}
