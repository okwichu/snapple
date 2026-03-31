#![windows_subsystem = "windows"]

mod audio;
mod buffer;
mod capture;
mod config;
mod games;
mod icon;
mod sound;

use std::env;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::{mpsc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use global_hotkey::hotkey::{Code, HotKey};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager};
use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
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
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| config::exe_sibling("snapple.log"))
}

/// Thread-safe log writer. Opening the file once and protecting it with a
/// Mutex prevents Windows file-locking races that silently dropped messages
/// when multiple threads (capture, audio, main) called `log()` concurrently.
fn log_writer() -> &'static Mutex<Option<File>> {
    static WRITER: OnceLock<Mutex<Option<File>>> = OnceLock::new();
    WRITER.get_or_init(|| {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file_path())
            .ok();
        Mutex::new(file)
    })
}

fn setup_logging() {
    // Force the log writer to initialise before any threads start.
    let _ = log_writer();

    std::panic::set_hook(Box::new(|info| {
        let msg = format!("PANIC: {info}");
        // Try the shared writer first; fall back to an independent open
        // only if the Mutex is poisoned (i.e. the panic happened while
        // another thread held the lock).
        if let Ok(mut guard) = log_writer().lock() {
            if let Some(ref mut f) = *guard {
                let _ = writeln!(f, "{msg}");
                let _ = f.flush();
                return;
            }
        }
        let _ = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file_path())
            .and_then(|mut f| writeln!(f, "{msg}"));
    }));
}

pub fn log(msg: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    eprintln!("{ts} {msg}");
    if let Ok(mut guard) = log_writer().lock() {
        if let Some(ref mut f) = *guard {
            let _ = writeln!(f, "{ts} {msg}");
            let _ = f.flush();
        }
    }
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
    let mic_item = CheckMenuItem::new("Record Microphone", true, true, None);
    let startup_item = CheckMenuItem::new("Start with Windows", true, is_startup_enabled(), None);
    let separator = PredefinedMenuItem::separator();
    let open_clips = MenuItem::new("Open Clips Folder", true, None);
    let quit_item = MenuItem::new("Quit", true, None);

    // Enable startup by default on first run.
    if !is_startup_enabled() {
        set_startup_enabled(true);
        startup_item.set_checked(true);
    }

    let menu = Menu::new();
    menu.append(&status_item)?;
    menu.append(&mic_item)?;
    menu.append(&startup_item)?;
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
    let _monitor_handle = games::spawn_monitor(game_tx, cfg.steam.clone(), cfg.extra_games.clone());

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

                    let mut cap_cfg = cfg.capture.clone();
                    if !mic_item.is_checked() {
                        cap_cfg.microphone = "none".into();
                    }

                    match capture::CaptureSession::start(
                        seg_dir.clone(),
                        ffmpeg_path.clone(),
                        cap_cfg,
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
            if event.id() == startup_item.id() {
                set_startup_enabled(startup_item.is_checked());
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

// ---------------------------------------------------------------------------
// Windows startup (Run registry key)
// ---------------------------------------------------------------------------

const STARTUP_REG_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_REG_VALUE: &str = "Snapple";

fn is_startup_enabled() -> bool {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let Ok(run_key) = hkcu.open_subkey(STARTUP_REG_KEY) else {
        return false;
    };
    run_key.get_value::<String, _>(STARTUP_REG_VALUE).is_ok()
}

fn set_startup_enabled(enable: bool) {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    if enable {
        let exe = std::env::current_exe().unwrap_or_default();
        if let Ok(run_key) = hkcu.open_subkey_with_flags(
            STARTUP_REG_KEY,
            winreg::enums::KEY_SET_VALUE,
        ) {
            let _ = run_key.set_value(STARTUP_REG_VALUE, &exe.to_string_lossy().as_ref());
        }
    } else if let Ok(run_key) = hkcu.open_subkey_with_flags(
        STARTUP_REG_KEY,
        winreg::enums::KEY_SET_VALUE,
    ) {
        let _ = run_key.delete_value(STARTUP_REG_VALUE);
    }
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
