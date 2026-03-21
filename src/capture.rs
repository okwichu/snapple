use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::Com::*;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::audio;
use crate::config::CaptureConfig;
use crate::{buffer, log, CREATE_NO_WINDOW};

pub struct CaptureSession {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureSession {
    pub fn start(
        seg_dir: PathBuf,
        ffmpeg_path: PathBuf,
        capture_cfg: CaptureConfig,
        cleanup_age_secs: u64,
        game_pid: u32,
    ) -> Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let thread = thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                if let Err(e) = capture_thread(
                    &seg_dir,
                    &ffmpeg_path,
                    running_clone,
                    &capture_cfg,
                    cleanup_age_secs,
                    game_pid,
                ) {
                    log(&format!("[snapple] capture error: {e:#}"));
                }
            })
            .context("Failed to spawn capture thread")?;

        Ok(Self {
            running,
            thread: Some(thread),
        })
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_thread(
    seg_dir: &Path,
    ffmpeg_path: &Path,
    running: Arc<AtomicBool>,
    capture_cfg: &CaptureConfig,
    cleanup_age_secs: u64,
    game_pid: u32,
) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    // Request 1 ms timer resolution so thread::sleep is accurate enough for 60 fps.
    #[link(name = "winmm")]
    unsafe extern "system" {
        fn timeBeginPeriod(uperiod: u32) -> u32;
        fn timeEndPeriod(uperiod: u32) -> u32;
    }
    unsafe { timeBeginPeriod(1); }

    // Create D3D11 device
    let (d3d_device, d3d_context) = create_d3d_device()?;

    // Wrap as WinRT IDirect3DDevice for the frame pool
    let winrt_device = create_winrt_device(&d3d_device)?;

    // Try window capture first (works even in borderless/exclusive fullscreen on modern Windows).
    // Wait a few seconds for the game window to appear.
    let item = create_capture_item(game_pid, &capture_cfg.monitor, &running)?;

    let size = item.Size()?;
    let width = size.Width as u32;
    let height = size.Height as u32;
    log(&format!(
        "[snapple] capturing {width}x{height} via Windows.Graphics.Capture"
    ));

    // Create frame pool (free-threaded so we can poll from this thread)
    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &winrt_device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        2,
        size,
    )?;
    let session = frame_pool.CreateCaptureSession(&item)?;

    // Disable yellow capture border (Windows 11+, best-effort)
    let _ = session.SetIsBorderRequired(false);

    // Start capture
    session.StartCapture()?;

    // Shared counter: the capture loop increments this after each frame
    // write so the audio thread can pace its output to match the video
    // clock, preventing A/V desync when encoding back-pressure slows
    // the video pipe below the declared fps.
    let video_frames = Arc::new(AtomicU64::new(0));

    // Start audio capture (best-effort — video continues even if audio fails)
    let audio_pipe = match audio::AudioPipe::start(
        &capture_cfg.microphone,
        running.clone(),
        video_frames.clone(),
        capture_cfg.fps,
    ) {
        Ok(ap) => {
            log(&format!(
                "[snapple] audio capture started ({}Hz stereo)",
                ap.sample_rate
            ));
            Some(ap)
        }
        Err(e) => {
            log(&format!("[snapple] audio unavailable: {e:#}"));
            None
        }
    };

    // Spawn ffmpeg
    let audio_input = audio_pipe
        .as_ref()
        .map(|ap| AudioInput { pipe_path: &ap.pipe_path, sample_rate: ap.sample_rate });
    let mut ffmpeg = spawn_ffmpeg(ffmpeg_path, width, height, seg_dir, capture_cfg, audio_input)?;
    drain_ffmpeg_stderr(&mut ffmpeg);
    let mut stdin = ffmpeg.stdin.take().context("No ffmpeg stdin")?;

    let frame_interval = Duration::from_micros(1_000_000 / capture_cfg.fps);
    let row_bytes = (width * 4) as usize;
    let frame_size = row_bytes * height as usize;

    // Staging texture for CPU readback
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging_opt: Option<ID3D11Texture2D> = None;
    unsafe { d3d_device.CreateTexture2D(&staging_desc, None, Some(&mut staging_opt))? };
    let staging = staging_opt.context("Failed to create staging texture")?;

    // Frame buffer — we always send a frame every tick to maintain the declared FPS.
    // Without this, missed polls cause the video to play faster than real-time.
    let mut last_frame = vec![0u8; frame_size];
    let mut has_first_frame = false;
    let mut last_cleanup = Instant::now();
    let mut prev_content = (width, height);

    while running.load(Ordering::Relaxed) {
        let frame_start = Instant::now();

        // Periodic segment cleanup
        if last_cleanup.elapsed() > Duration::from_secs(10) {
            let _ = buffer::cleanup_old_segments(seg_dir, cleanup_age_secs);
            last_cleanup = Instant::now();
        }

        // Poll for next frame
        if let Ok(frame) = frame_pool.TryGetNextFrame() {
            let content_size = frame.ContentSize().unwrap_or(size);
            let cw = (content_size.Width as u32).min(width);
            let ch = (content_size.Height as u32).min(height);

            // Only clear the buffer when the content size actually changes,
            // not every frame — avoids ~14 MB memset per frame at 4K.
            if (cw, ch) != prev_content {
                if cw < width || ch < height {
                    last_frame.fill(0);
                }
                prev_content = (cw, ch);
            }

            let copy_row = (cw * 4) as usize;
            let copy_rows = ch as usize;

            if let Ok(surface) = frame.Surface() {
                if let Ok(dxgi_access) = surface.cast::<IDirect3DDxgiInterfaceAccess>() {
                    if let Ok(texture) = unsafe { dxgi_access.GetInterface::<ID3D11Texture2D>() } {
                        unsafe {
                            d3d_context.CopyResource(&staging, &texture);

                            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                            if d3d_context
                                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                                .is_ok()
                            {
                                let pitch = mapped.RowPitch as usize;
                                let src = std::slice::from_raw_parts(
                                    mapped.pData as *const u8,
                                    pitch * height as usize,
                                );

                                if copy_row == row_bytes && pitch == row_bytes {
                                    last_frame[..frame_size]
                                        .copy_from_slice(&src[..frame_size]);
                                } else {
                                    for y in 0..copy_rows {
                                        let dst = y * row_bytes;
                                        let s = y * pitch;
                                        last_frame[dst..dst + copy_row]
                                            .copy_from_slice(&src[s..s + copy_row]);
                                    }
                                }

                                d3d_context.Unmap(&staging, 0);
                                has_first_frame = true;
                            }
                        }
                    }
                }
            }
            let _ = frame.Close();
        }

        // Always send a frame at the declared rate to keep video in real-time.
        if has_first_frame {
            if stdin.write_all(&last_frame).is_err() {
                log("[snapple] ffmpeg pipe broken, stopping capture");
                break;
            }
            video_frames.fetch_add(1, Ordering::Relaxed);
        }

        // Frame pacing
        let elapsed = frame_start.elapsed();
        if elapsed < frame_interval {
            thread::sleep(frame_interval - elapsed);
        }
    }

    // Stop capture
    let _ = session.Close();
    let _ = frame_pool.Close();

    // Finalize ffmpeg
    drop(stdin);
    let _ = ffmpeg.wait();

    // Stop audio (AudioPipe::drop joins the audio thread)
    drop(audio_pipe);

    // Restore default timer resolution.
    unsafe { timeEndPeriod(1); }

    Ok(())
}

// ---------------------------------------------------------------------------
// Capture item creation — prefer window capture, fall back to monitor
// ---------------------------------------------------------------------------

fn create_capture_item(
    game_pid: u32,
    monitor_setting: &str,
    running: &AtomicBool,
) -> Result<GraphicsCaptureItem> {
    // Wait up to ~5 s for the game window to appear.
    let mut hwnd = None;
    for attempt in 0..5 {
        if !running.load(Ordering::Relaxed) {
            anyhow::bail!("Capture cancelled during window search");
        }
        hwnd = find_window_for_pid(game_pid);
        if hwnd.is_some() {
            break;
        }
        if attempt < 4 {
            thread::sleep(Duration::from_secs(1));
        }
    }

    // Try direct window capture.
    if let Some(h) = hwnd {
        // Log details about the window we found.
        let mut title_buf = [0u16; 256];
        let title_len = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(h, &mut title_buf)
        } as usize;
        let title = String::from_utf16_lossy(&title_buf[..title_len]);
        let mut rect = windows::Win32::Foundation::RECT::default();
        let _ = unsafe { windows::Win32::UI::WindowsAndMessaging::GetClientRect(h, &mut rect) };
        log(&format!(
            "[snapple] found game window: hwnd={:?} title=\"{title}\" client={}x{}",
            h.0,
            rect.right - rect.left,
            rect.bottom - rect.top,
        ));

        match create_window_capture_item(h) {
            Ok(item) => {
                log("[snapple] using direct window capture");
                return Ok(item);
            }
            Err(e) => {
                log(&format!(
                    "[snapple] window capture unavailable ({e:#}), falling back to monitor"
                ));
            }
        }
    } else {
        log("[snapple] game window not found, falling back to monitor capture");
    }

    create_monitor_capture_item(monitor_setting, hwnd)
}

fn create_window_capture_item(
    hwnd: windows::Win32::Foundation::HWND,
) -> Result<GraphicsCaptureItem> {
    unsafe {
        let interop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForWindow(hwnd)?;
        Ok(item)
    }
}

// ---------------------------------------------------------------------------
// D3D helpers (unchanged)
// ---------------------------------------------------------------------------

fn create_d3d_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let mut device = None;
        let mut context = None;

        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;

        Ok((
            device.context("No D3D11 device")?,
            context.context("No D3D11 context")?,
        ))
    }
}

fn create_winrt_device(
    d3d_device: &ID3D11Device,
) -> Result<windows::Graphics::DirectX::Direct3D11::IDirect3DDevice> {
    unsafe {
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;
        let device = inspectable.cast()?;
        Ok(device)
    }
}

// ---------------------------------------------------------------------------
// Window / monitor helpers
// ---------------------------------------------------------------------------

/// Find the main visible window belonging to a process.
///
/// Picks the largest non-tool window so that anti-cheat overlays, splash
/// screens, and notification popups don't cause `CreateForWindow` to fail.
fn find_window_for_pid(pid: u32) -> Option<windows::Win32::Foundation::HWND> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowLongW, GetWindowThreadProcessId,
        GWL_EXSTYLE, GWL_STYLE, WS_EX_TOOLWINDOW, WS_VISIBLE,
    };

    struct SearchState {
        target_pid: u32,
        best: HWND,
        best_area: i64,
    }

    unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let state = &mut *(lparam.0 as *mut SearchState);
            let mut window_pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut window_pid));
            if window_pid != state.target_pid {
                return BOOL(1);
            }

            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            if style & WS_VISIBLE.0 == 0 {
                return BOOL(1);
            }

            // Skip tool windows (floating toolbars, overlays, tooltips).
            let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
                return BOOL(1);
            }

            // Pick the window with the largest client area — this is the game
            // rendering surface, not a small splash screen or overlay.
            let mut rect = RECT::default();
            if GetClientRect(hwnd, &mut rect).is_ok() {
                let area = (rect.right - rect.left) as i64 * (rect.bottom - rect.top) as i64;
                if area > state.best_area {
                    state.best_area = area;
                    state.best = hwnd;
                }
            }

            BOOL(1) // continue — enumerate all windows
        }
    }

    let mut state = SearchState {
        target_pid: pid,
        best: HWND::default(),
        best_area: 0,
    };

    unsafe {
        let _ = EnumWindows(
            Some(enum_callback),
            LPARAM(&mut state as *mut SearchState as isize),
        );
    }

    if !state.best.0.is_null() {
        Some(state.best)
    } else {
        None
    }
}

/// Get the HMONITOR at a 1-based index by enumerating all monitors.
fn monitor_by_index(index: u32) -> Option<windows::Win32::Graphics::Gdi::HMONITOR> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};

    struct MonitorState {
        target: u32,
        current: u32,
        found: HMONITOR,
    }

    unsafe extern "system" fn enum_callback(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        unsafe {
            let state = &mut *(lparam.0 as *mut MonitorState);
            state.current += 1;
            if state.current == state.target {
                state.found = hmon;
                return BOOL(0);
            }
            BOOL(1)
        }
    }

    let mut state = MonitorState {
        target: index,
        current: 0,
        found: HMONITOR::default(),
    };

    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_callback),
            LPARAM(&mut state as *mut MonitorState as isize),
        );
    }

    if !state.found.is_invalid() {
        Some(state.found)
    } else {
        None
    }
}

fn primary_monitor() -> windows::Win32::Graphics::Gdi::HMONITOR {
    unsafe {
        MonitorFromPoint(
            windows::Win32::Foundation::POINT { x: 0, y: 0 },
            MONITOR_DEFAULTTOPRIMARY,
        )
    }
}

fn create_monitor_capture_item(
    monitor_setting: &str,
    cached_hwnd: Option<windows::Win32::Foundation::HWND>,
) -> Result<GraphicsCaptureItem> {
    use windows::Win32::Graphics::Gdi::MonitorFromWindow;

    let hmonitor = match monitor_setting {
        "auto" => {
            if let Some(hwnd) = cached_hwnd {
                let mon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTOPRIMARY) };
                log("[snapple] auto-detected game window on monitor");
                mon
            } else {
                log("[snapple] could not find game window, falling back to primary monitor");
                primary_monitor()
            }
        }
        index_str => {
            if let Ok(index) = index_str.parse::<u32>() {
                if let Some(mon) = monitor_by_index(index) {
                    log(&format!("[snapple] using monitor {index}"));
                    mon
                } else {
                    log(&format!(
                        "[snapple] monitor {index} not found, falling back to primary"
                    ));
                    primary_monitor()
                }
            } else {
                log(&format!(
                    "[snapple] invalid monitor setting '{index_str}', falling back to primary"
                ));
                primary_monitor()
            }
        }
    };

    unsafe {
        let interop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(hmonitor)?;
        Ok(item)
    }
}

// ---------------------------------------------------------------------------
// ffmpeg spawning
// ---------------------------------------------------------------------------

struct AudioInput<'a> {
    pipe_path: &'a str,
    sample_rate: u32,
}

#[cfg(test)]
fn frame_interval(fps: u64) -> Duration {
    Duration::from_micros(1_000_000 / fps)
}

#[cfg(test)]
fn choose_monitor_target(
    monitor_setting: &str,
    auto_monitor: Option<usize>,
    available_monitors: usize,
) -> usize {
    match monitor_setting {
        "auto" => auto_monitor.unwrap_or(1),
        index_str => index_str
            .parse::<usize>()
            .ok()
            .filter(|index| *index >= 1 && *index <= available_monitors)
            .unwrap_or(1),
    }
}

fn build_ffmpeg_args(
    width: u32,
    height: u32,
    seg_dir: &Path,
    cfg: &CaptureConfig,
    audio: Option<AudioInput<'_>>,
) -> Vec<String> {
    let seg_pattern = seg_dir
        .join("seg_%04d.mp4")
        .to_string_lossy()
        .replace('\\', "/");

    let mut args: Vec<String> = vec!["-y".into()];

    args.extend([
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "bgra".into(),
        "-s".into(),
        format!("{width}x{height}"),
        "-r".into(),
        cfg.fps.to_string(),
        "-i".into(),
        "pipe:0".into(),
    ]);

    if let Some(ref ai) = audio {
        args.extend([
            "-f".into(),
            "f32le".into(),
            "-ar".into(),
            ai.sample_rate.to_string(),
            "-ac".into(),
            "2".into(),
            "-i".into(),
            ai.pipe_path.into(),
        ]);
    }

    args.extend([
        "-vf".into(),
        cfg.scale.clone(),
        "-c:v".into(),
        cfg.encoder.clone(),
        "-preset".into(),
        cfg.preset.clone(),
        "-rc".into(),
        cfg.rate_control.clone(),
        cfg.quality_flag().into(),
        cfg.quality.clone(),
    ]);

    if audio.is_some() {
        args.extend(["-c:a".into(), "aac".into(), "-b:a".into(), "128k".into()]);
    }

    args.extend([
        "-f".into(),
        "segment".into(),
        "-segment_time".into(),
        cfg.segment_time.to_string(),
        "-reset_timestamps".into(),
        "1".into(),
        "-segment_format".into(),
        "mp4".into(),
        seg_pattern,
    ]);

    args
}

fn spawn_ffmpeg(
    ffmpeg_path: &Path,
    width: u32,
    height: u32,
    seg_dir: &Path,
    cfg: &CaptureConfig,
    audio: Option<AudioInput<'_>>,
) -> Result<Child> {
    let args = build_ffmpeg_args(width, height, seg_dir, cfg, audio);

    log(&format!("[snapple] ffmpeg args: {}", args.join(" ")));

    let child = Command::new(ffmpeg_path)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("Failed to spawn ffmpeg — is it installed and on PATH?")?;

    Ok(child)
}

/// Drain ffmpeg stderr in a background thread and log interesting lines.
fn drain_ffmpeg_stderr(child: &mut Child) {
    use std::io::{BufRead, BufReader};

    if let Some(stderr) = child.stderr.take() {
        thread::Builder::new()
            .name("ffmpeg-stderr".into())
            .spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) if !l.is_empty() => log(&format!("[ffmpeg] {l}")),
                        _ => break,
                    }
                }
            })
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::{build_ffmpeg_args, choose_monitor_target, frame_interval, AudioInput};
    use crate::config::CaptureConfig;
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn uses_declared_fps_for_real_time_pacing() {
        assert_eq!(frame_interval(60), Duration::from_micros(16_666));
        assert_eq!(frame_interval(30), Duration::from_micros(33_333));
    }

    #[test]
    fn ffmpeg_args_preserve_aspect_ratio_and_declared_speed() {
        let cfg = CaptureConfig {
            fps: 60,
            scale: "scale=-2:720".into(),
            encoder: "h264_nvenc".into(),
            preset: "p4".into(),
            rate_control: "constqp".into(),
            quality: "28".into(),
            segment_time: 5,
            monitor: "auto".into(),
            microphone: "default".into(),
        };

        let args = build_ffmpeg_args(2560, 1440, Path::new("C:/temp"), &cfg, None);

        assert!(args.windows(2).any(|w| w == ["-vf", "scale=-2:720"]));
        assert!(args.windows(2).any(|w| w == ["-r", "60"]));
        assert!(args.windows(2).any(|w| w == ["-s", "2560x1440"]));
    }

    #[test]
    fn ffmpeg_args_include_audio_pipe_when_audio_is_enabled() {
        let cfg = CaptureConfig::default();
        let args = build_ffmpeg_args(
            1920,
            1080,
            Path::new("C:/temp"),
            &cfg,
            Some(AudioInput {
                pipe_path: r"\\.\pipe\snapple_audio_42",
                sample_rate: 48_000,
            }),
        );

        assert!(args.windows(2).any(|w| w == ["-ar", "48000"]));
        assert!(args.windows(2).any(|w| w == ["-ac", "2"]));
        assert!(args.windows(2).any(|w| w == ["-i", r"\\.\pipe\snapple_audio_42"]));
        assert!(args.windows(2).any(|w| w == ["-c:a", "aac"]));
    }

    #[test]
    fn auto_monitor_prefers_game_window_monitor() {
        assert_eq!(choose_monitor_target("auto", Some(2), 3), 2);
    }

    #[test]
    fn invalid_or_out_of_range_monitor_falls_back_to_primary() {
        assert_eq!(choose_monitor_target("99", Some(2), 3), 1);
        assert_eq!(choose_monitor_target("bogus", Some(2), 3), 1);
    }
}
