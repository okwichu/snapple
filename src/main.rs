#![windows_subsystem = "windows"]

mod audio;
mod buffer;
mod capture;
mod config;
mod games;
mod icon;
mod sound;

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use global_hotkey::hotkey::{Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use muda::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::TrayIconBuilder;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const CREATE_NO_WINDOW: u32 = 0x08000000;

fn find_ffmpeg() -> Result<PathBuf> {
    // 1. Check next to our own exe (bundled installer case)
    let bundled = config::exe_sibling("ffmpeg.exe");
    if bundled.exists() {
        return Ok(bundled);
    }

    // 2. Check PATH
    let output = ProcessCommand::new("where")
        .arg("ffmpeg")
        .output()
        .context("Failed to run 'where ffmpeg'")?;
    if output.status.success() {
        let path = String::from_utf8_lossy(&output.stdout);
        if let Some(line) = path.lines().next() {
            let p = PathBuf::from(line.trim());
            if p.exists() {
                return Ok(p);
            }
        }
    }
    anyhow::bail!(
        "ffmpeg not found. Expected next to snapple.exe or on PATH.\n\
         Install with: winget install Gyan.FFmpeg"
    )
}

fn log_file_path() -> &'static PathBuf {
    static PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATH.get_or_init(|| config::exe_sibling("snapple.log"))
}

fn setup_logging() {
    let log_path = log_file_path();
    // Redirect panics to the log file
    let log_path2 = log_path.clone();
    std::panic::set_hook(Box::new(move |info| {
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path2)
            .and_then(|mut f| writeln!(f, "PANIC: {info}"));
    }));
}

pub fn log(msg: &str) {
    eprintln!("{msg}");
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file_path())
        .and_then(|mut f| writeln!(f, "{msg}"));
}

fn run() -> Result<()> {
    let cfg = config::load();

    let ffmpeg_path = find_ffmpeg()?;
    log(&format!("[snapple] using ffmpeg: {}", ffmpeg_path.display()));

    // Create directories
    std::fs::create_dir_all(&cfg.clips_dir)?;

    let seg_dir = env::temp_dir().join("snapple_segments");
    std::fs::create_dir_all(&seg_dir)?;
    // Clean any leftover segments from a previous run
    let _ = buffer::cleanup_old_segments(&seg_dir, 0);

    // Generate assets
    let tray_icon_img = icon::create_tray_icon();
    let shutter_wav = sound::generate_shutter_wav();

    // Build tray menu
    let status_item = MenuItem::new("Snapple \u{2014} Idle", false, None);
    let separator = PredefinedMenuItem::separator();
    let open_clips = MenuItem::new("Open Clips Folder", true, None);
    let quit_item = MenuItem::new("Quit", true, None);

    let menu = Menu::new();
    menu.append(&status_item)?;
    menu.append(&separator)?;
    menu.append(&open_clips)?;
    menu.append(&quit_item)?;

    let _tray = TrayIconBuilder::new()
        .with_icon(tray_icon_img)
        .with_menu(Box::new(menu))
        .with_tooltip("Snapple \u{2014} Idle")
        .build()?;

    // Register global hotkey
    let hotkey_code = config::parse_hotkey_code(&cfg.hotkey).unwrap_or(Code::F8);
    let hotkey_manager = GlobalHotKeyManager::new()?;
    let hotkey = HotKey::new(None, hotkey_code);
    hotkey_manager.register(hotkey)?;
    log(&format!("[snapple] {} hotkey registered", cfg.hotkey));

    // Start game monitor
    let (game_tx, game_rx) = mpsc::channel();
    let _monitor_handle = games::spawn_monitor(game_tx, cfg.steam.clone());

    // App state
    let mut current_game: Option<String> = None;
    let mut capture_session: Option<capture::CaptureSession> = None;
    let cleanup_age = cfg.cleanup_age_secs();

    log("[snapple] ready — waiting for a Steam game to launch");

    // Main event loop
    loop {
        // Pump Win32 messages (required by tray-icon and global-hotkey)
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Handle game detection events
        while let Ok(event) = game_rx.try_recv() {
            match event {
                games::GameEvent::Started { name, pid }
                | games::GameEvent::Switched { name, pid } => {
                    log(&format!("[snapple] game detected: {name} (pid {pid})"));

                    // Stop any existing capture first (handles game-to-game switches).
                    if let Some(mut session) = capture_session.take() {
                        session.stop();
                    }

                    match capture::CaptureSession::start(
                        seg_dir.clone(),
                        ffmpeg_path.clone(),
                        cfg.capture.clone(),
                        cleanup_age,
                        pid,
                    ) {
                        Ok(session) => {
                            capture_session = Some(session);
                            current_game = Some(name.clone());
                            status_item.set_text(format!("Snapple \u{2014} Recording {name}"));
                            log("[snapple] capture started");
                        }
                        Err(e) => {
                            // Capture failed — don't claim we're recording.
                            current_game = None;
                            status_item.set_text("Snapple \u{2014} Idle");
                            log(&format!("[snapple] failed to start capture: {e:#}"));
                        }
                    }
                }
                games::GameEvent::Stopped => {
                    log("[snapple] game stopped");
                    if let Some(mut session) = capture_session.take() {
                        session.stop();
                    }
                    current_game = None;
                    status_item.set_text("Snapple \u{2014} Idle");
                }
            }
        }

        // Handle menu events
        if let Ok(event) = muda::MenuEvent::receiver().try_recv() {
            if event.id() == quit_item.id() {
                log("[snapple] quitting");
                break;
            }
            if event.id() == open_clips.id() {
                let _ = ProcessCommand::new("explorer")
                    .arg(cfg.clips_dir.as_os_str())
                    .spawn();
            }
        }

        // Handle hotkey — only save if there is an active capture session.
        if let Ok(_event) = GlobalHotKeyEvent::receiver().try_recv() {
            if capture_session.is_some() {
                if let Some(game) = current_game.as_deref() {
                    log(&format!(
                        "[snapple] {} pressed — saving clip for {game}",
                        cfg.hotkey
                    ));
                    sound::play_shutter(&shutter_wav);

                    match buffer::save_clip(
                        &seg_dir,
                        game,
                        &ffmpeg_path,
                        &cfg.clips_dir,
                        cfg.buffer.segments,
                    ) {
                        Ok(path) => log(&format!("[snapple] clip saved: {}", path.display())),
                        Err(e) => log(&format!("[snapple] save failed: {e:#}")),
                    }
                }
            } else {
                log(&format!(
                    "[snapple] {} pressed but no active capture",
                    cfg.hotkey
                ));
            }
        }

        std::thread::sleep(Duration::from_millis(50));
    }

    // Clean shutdown
    if let Some(mut session) = capture_session.take() {
        session.stop();
    }
    let _ = std::fs::remove_dir_all(&seg_dir);

    Ok(())
}

fn main() {
    setup_logging();
    log("[snapple] starting");
    if let Err(e) = run() {
        log(&format!("[snapple] FATAL: {e:#}"));
        // Keep window open briefly so user can see
        std::thread::sleep(Duration::from_secs(5));
        std::process::exit(1);
    }
}
