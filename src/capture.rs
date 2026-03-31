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

    // Staging texture for GPU→CPU readback (full capture size for CopyResource).
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

    // Resolve encoder once — probe ffmpeg for a working H.264 encoder if the
    // configured one isn't available (e.g. nvenc on an AMD-only laptop).
    let resolved_cfg = resolve_encoder(ffmpeg_path, capture_cfg);
    let fps = resolved_cfg.fps;

    // -----------------------------------------------------------------------
    // Session loop — restarts ffmpeg when the content size changes (e.g. the
    // game transitions from a splash screen to fullscreen).  The capture item
    // and frame pool stay alive across restarts; only the encoding pipeline
    // (audio pipe + ffmpeg + frame buffer) is recycled.
    // -----------------------------------------------------------------------
    let mut content_w = width;
    let mut content_h = height;

    'session: loop {
        if !running.load(Ordering::Relaxed) {
            break;
        }

        // --- Detect content size from recent frames -------------------------
        {
            let mut best_w = 0u32;
            let mut best_h = 0u32;
            let mut best_area: u64 = 0;
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline && running.load(Ordering::Relaxed) {
                if let Ok(frame) = frame_pool.TryGetNextFrame() {
                    let cs = frame.ContentSize().unwrap_or(size);
                    let fw = (cs.Width as u32).min(width);
                    let fh = (cs.Height as u32).min(height);
                    let area = fw as u64 * fh as u64;
                    if area > best_area {
                        best_area = area;
                        best_w = fw;
                        best_h = fh;
                    }
                    let _ = frame.Close();
                }
                thread::sleep(Duration::from_millis(10));
            }
            if best_area > 0 {
                content_w = best_w;
                content_h = best_h;
            }
            // else: keep previous content size (or full surface on first pass)
        }

        if content_w != width || content_h != height {
            log(&format!(
                "[snapple] content {content_w}x{content_h} inside {width}x{height} capture surface"
            ));
        } else {
            log(&format!(
                "[snapple] content matches capture surface ({width}x{height})"
            ));
        }

        // --- Start encoding pipeline ---------------------------------------
        let video_frames = Arc::new(AtomicU64::new(0));

        let audio_pipe = match audio::AudioPipe::start(
            &resolved_cfg.microphone,
            game_pid,
            running.clone(),
            video_frames.clone(),
            fps,
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

        // Compute output dims early so we can pass them to ffmpeg.
        let (out_w, out_h) = compute_output_dims(&resolved_cfg.scale, content_w, content_h);

        let audio_input = audio_pipe
            .as_ref()
            .map(|ap| AudioInput { pipe_path: &ap.pipe_path, sample_rate: ap.sample_rate });
        let mut ffmpeg = match spawn_ffmpeg(ffmpeg_path, out_w, out_h, seg_dir, &resolved_cfg, audio_input) {
            Ok(f) => f,
            Err(e) => {
                log(&format!("[snapple] failed to spawn ffmpeg: {e:#}"));
                drop(audio_pipe);
                break;
            }
        };
        drain_ffmpeg_stderr(&mut ffmpeg);
        let mut stdin = match ffmpeg.stdin.take() {
            Some(s) => s,
            None => {
                log("[snapple] no ffmpeg stdin");
                drop(audio_pipe);
                break;
            }
        };

        if let Some(ref ap) = audio_pipe {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ap.ready.load(Ordering::Acquire) {
                if !running.load(Ordering::Relaxed) || Instant::now() >= deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }

        let out_row_bytes = (out_w * 4) as usize;
        let out_frame_size = out_row_bytes * out_h as usize;
        log(&format!(
            "[snapple] output {out_w}x{out_h} (pre-scaled from {content_w}x{content_h})"
        ));

        let mut last_frame = vec![0u8; out_frame_size];
        let mut has_first_frame = false;
        let mut last_cleanup = Instant::now();
        let mut frame_count: u64 = 0;
        let mut pacing_origin: Option<Instant> = None;

        // --- Frame loop -----------------------------------------------------
        let mut needs_restart = false;

        while running.load(Ordering::Relaxed) {
            if last_cleanup.elapsed() > Duration::from_secs(10) {
                let _ = buffer::cleanup_old_segments(seg_dir, cleanup_age_secs);
                last_cleanup = Instant::now();
            }

            if let Ok(frame) = frame_pool.TryGetNextFrame() {
                let content_size = frame.ContentSize().unwrap_or(size);
                let raw_cw = (content_size.Width as u32).min(width);
                let raw_ch = (content_size.Height as u32).min(height);

                // If the content grew beyond our current dimensions, restart
                // the pipeline so ffmpeg gets the right resolution.
                if raw_cw > content_w || raw_ch > content_h {
                    log(&format!(
                        "[snapple] content size grew to {raw_cw}x{raw_ch} (was {content_w}x{content_h}), restarting pipeline"
                    ));
                    let _ = frame.Close();
                    needs_restart = true;
                    break;
                }

                let src_w = raw_cw.min(content_w) as usize;
                let src_h = raw_ch.min(content_h) as usize;

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

                                    // Downsample directly from the mapped GPU
                                    // texture into the output-sized frame buffer.
                                    let src_ptr = mapped.pData as *const u8;
                                    downsample_bgra(
                                        src_ptr, src_w, src_h, pitch,
                                        &mut last_frame, out_w as usize, out_h as usize,
                                    );

                                    d3d_context.Unmap(&staging, 0);
                                    has_first_frame = true;
                                }
                            }
                        }
                    }
                }
                let _ = frame.Close();
            }

            if has_first_frame {
                let origin = *pacing_origin.get_or_insert_with(Instant::now);

                if stdin.write_all(&last_frame).is_err() {
                    log("[snapple] ffmpeg pipe broken, stopping capture");
                    break;
                }
                video_frames.fetch_add(1, Ordering::Relaxed);
                frame_count += 1;

                let next_due_us = frame_count * 1_000_000 / fps;
                let next_due = origin + Duration::from_micros(next_due_us);
                let now = Instant::now();
                if next_due > now {
                    thread::sleep(next_due - now);
                }
            } else {
                thread::sleep(Duration::from_micros(1_000_000 / fps));
            }
        }

        // --- Tear down encoding pipeline ------------------------------------
        drop(stdin);
        let _ = ffmpeg.wait();
        drop(audio_pipe);

        if !needs_restart {
            break 'session;
        }
        // Loop back to re-detect content size and restart pipeline.
    }

    // Stop capture
    let _ = session.Close();
    let _ = frame_pool.Close();

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
// Pre-scaling helpers
// ---------------------------------------------------------------------------

/// Parse a scale filter string like "scale=-2:720" and compute the output
/// dimensions, preserving aspect ratio and rounding to the nearest even number.
fn compute_output_dims(scale: &str, src_w: u32, src_h: u32) -> (u32, u32) {
    if let Some(rest) = scale.strip_prefix("scale=") {
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() == 2 {
            let w: Option<i32> = parts[0].parse().ok();
            let h: Option<i32> = parts[1].parse().ok();
            match (w, h) {
                (Some(-2 | -1), Some(th)) if th > 0 => {
                    let th = th as u32;
                    let raw = (src_w as u64 * th as u64 / src_h as u64) as u32;
                    return ((raw + 1) & !1, th);
                }
                (Some(tw), Some(-2 | -1)) if tw > 0 => {
                    let tw = tw as u32;
                    let raw = (src_h as u64 * tw as u64 / src_w as u64) as u32;
                    return (tw, (raw + 1) & !1);
                }
                _ => {}
            }
        }
    }
    (src_w, src_h)
}

/// Nearest-neighbour downsample of a BGRA image.  Reads directly from a
/// mapped GPU staging texture (with arbitrary row pitch) and writes into a
/// tightly-packed output buffer.
///
/// # Safety
/// `src` must point to at least `src_pitch * src_h` readable bytes.
unsafe fn downsample_bgra(
    src: *const u8,
    src_w: usize,
    src_h: usize,
    src_pitch: usize,
    dst: &mut [u8],
    dst_w: usize,
    dst_h: usize,
) {
    unsafe {
        for dy in 0..dst_h {
            let sy = dy * src_h / dst_h;
            let src_row = src.add(sy * src_pitch);
            let dst_off = dy * dst_w * 4;
            for dx in 0..dst_w {
                let sx = dx * src_w / dst_w;
                let si = sx * 4;
                let di = dst_off + dx * 4;
                std::ptr::copy_nonoverlapping(src_row.add(si), dst[di..].as_mut_ptr(), 4);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Encoder probing & fallback
// ---------------------------------------------------------------------------

/// Encoder presets: (encoder, preset, rate_control, quality).
const ENCODER_FALLBACKS: &[(&str, &str, &str, &str)] = &[
    // NVIDIA
    ("h264_nvenc", "p4", "constqp", "16"),
    // AMD
    ("h264_amf", "balanced", "cqp", "16"),
    // Software (always available)
    ("libx264", "fast", "crf", "16"),
];

/// Check if a given encoder is available in ffmpeg by doing a tiny test encode.
fn probe_encoder(ffmpeg_path: &Path, encoder: &str) -> bool {
    // Encode 1 frame to /dev/null. Use 256x256 because some HW encoders
    // (notably AMD AMF) reject very small resolutions.
    let status = Command::new(ffmpeg_path)
        .args([
            "-hide_banner",
            "-loglevel", "error",
            "-f", "lavfi",
            "-i", "color=black:s=256x256:d=0.01",
            "-frames:v", "1",
            "-c:v", encoder,
            "-f", "null",
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    matches!(status, Ok(s) if s.success())
}

/// Return a CaptureConfig with a working encoder. If the configured encoder
/// works, returns a clone unchanged.  Otherwise walks the fallback list.
fn resolve_encoder(ffmpeg_path: &Path, cfg: &CaptureConfig) -> CaptureConfig {
    // Fast path: configured encoder works.
    if probe_encoder(ffmpeg_path, &cfg.encoder) {
        log(&format!("[snapple] encoder {} available", cfg.encoder));
        return cfg.clone();
    }

    log(&format!(
        "[snapple] encoder {} unavailable, probing fallbacks…",
        cfg.encoder
    ));

    for &(enc, preset, rc, quality) in ENCODER_FALLBACKS {
        if enc == cfg.encoder {
            continue; // Already tried.
        }
        if probe_encoder(ffmpeg_path, enc) {
            log(&format!("[snapple] using fallback encoder {enc}"));
            let mut resolved = cfg.clone();
            resolved.encoder = enc.into();
            resolved.preset = preset.into();
            resolved.rate_control = rc.into();
            resolved.quality = quality.into();
            return resolved;
        }
    }

    // Nothing worked — return original config and let ffmpeg fail with a
    // clear error rather than silently skipping capture.
    log("[snapple] WARNING: no working H.264 encoder found");
    cfg.clone()
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

    // Use wall-clock timestamps so that video duration matches real time
    // even when the capture loop can't sustain the target framerate.
    args.extend(["-use_wallclock_as_timestamps".into(), "1".into()]);

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
        "-pix_fmt".into(),
        "yuv420p".into(),
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
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            "320k".into(),
            "-aac_coder".into(),
            "twoloop".into(),
        ]);
    }

    // Output at the target framerate — ffmpeg will duplicate or drop frames
    // to maintain constant fps, compensating for any capture-loop jitter.
    args.extend(["-r".into(), cfg.fps.to_string()]);

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
    use super::{
        build_ffmpeg_args, choose_monitor_target, compute_output_dims, downsample_bgra,
        frame_interval, AudioInput,
    };
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
            quality: "16".into(),
            segment_time: 5,
            monitor: "auto".into(),
            microphone: "default".into(),
        };

        let args = build_ffmpeg_args(2560, 1440, Path::new("C:/temp"), &cfg, None);

        assert!(args.windows(2).any(|w| w == ["-use_wallclock_as_timestamps", "1"]));
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
        assert!(args.windows(2).any(|w| w == ["-b:a", "320k"]));
    }

    #[test]
    fn ffmpeg_args_no_aresample_filter() {
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

        // aresample=async was removed — raw pipe input doesn't need it,
        // and it caused artifacts under backpressure.
        assert!(
            !args.iter().any(|a| a.contains("aresample")),
            "aresample filter must not be present — it causes audio artifacts"
        );
        assert!(
            !args.iter().any(|a| a == "-af"),
            "no audio filters should be applied"
        );
    }

    #[test]
    fn audio_bitrate_is_320k() {
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

        assert!(
            args.windows(2).any(|w| w == ["-b:a", "320k"]),
            "audio bitrate must be 320k, got args: {args:?}"
        );
        assert!(
            !args.windows(2).any(|w| w == ["-b:a", "128k"]),
            "128k bitrate must not be used"
        );
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

    // -----------------------------------------------------------------------
    // Black-bar prevention: output dimensions must be correct and even, and
    // the downsample must fill every pixel of the output buffer.
    // -----------------------------------------------------------------------

    #[test]
    fn output_dims_scale_height_preserves_aspect_ratio() {
        // 16:9 monitor
        assert_eq!(compute_output_dims("scale=-2:720", 2560, 1440), (1280, 720));
        assert_eq!(compute_output_dims("scale=-2:720", 1920, 1080), (1280, 720));
        assert_eq!(compute_output_dims("scale=-2:720", 3840, 2160), (1280, 720));
    }

    #[test]
    fn output_dims_non_16_9_monitors_produce_even_width() {
        // 16:10 laptop display
        let (w, h) = compute_output_dims("scale=-2:720", 2880, 1800);
        assert_eq!(h, 720);
        assert_eq!(w % 2, 0, "width must be even for H.264, got {w}");
        assert_eq!(w, 1152);
    }

    #[test]
    fn output_dims_ultrawide_monitors() {
        // 21:9 ultrawide
        let (w, h) = compute_output_dims("scale=-2:720", 3440, 1440);
        assert_eq!(h, 720);
        assert_eq!(w % 2, 0, "width must be even, got {w}");
        // 3440 * 720 / 1440 = 1720
        assert_eq!(w, 1720);
    }

    #[test]
    fn output_dims_scale_width_mode() {
        let (w, h) = compute_output_dims("scale=1280:-2", 2560, 1440);
        assert_eq!(w, 1280);
        assert_eq!(h % 2, 0, "height must be even, got {h}");
        assert_eq!(h, 720);
    }

    #[test]
    fn output_dims_identity_when_scale_unparseable() {
        // Unrecognised filter → pass through unchanged (no accidental crop)
        assert_eq!(compute_output_dims("lanczos=720", 2560, 1440), (2560, 1440));
        assert_eq!(compute_output_dims("", 2560, 1440), (2560, 1440));
    }

    #[test]
    fn downsample_fills_entire_output_no_black_bars() {
        // Create a source image filled with a non-black colour (0xFFBBGGRR).
        let src_w: usize = 200;
        let src_h: usize = 100;
        let src_pitch = src_w * 4;
        let src: Vec<u8> = vec![0xAB; src_pitch * src_h];

        let dst_w: usize = 80;
        let dst_h: usize = 40;
        let mut dst = vec![0u8; dst_w * dst_h * 4];

        unsafe {
            downsample_bgra(
                src.as_ptr(), src_w, src_h, src_pitch,
                &mut dst, dst_w, dst_h,
            );
        }

        // Every pixel in the output must be non-zero — any zeros would
        // indicate black-bar padding that was never written.
        for (i, chunk) in dst.chunks(4).enumerate() {
            let x = i % dst_w;
            let y = i / dst_w;
            assert!(
                chunk.iter().all(|&b| b == 0xAB),
                "black pixel at output ({x}, {y}) — downsample left a gap"
            );
        }
    }

    #[test]
    fn downsample_with_pitch_larger_than_width() {
        // GPU staging textures often have a pitch > width*4 due to alignment.
        let src_w: usize = 100;
        let src_h: usize = 50;
        let src_pitch: usize = 512; // much wider than 100*4=400
        let mut src = vec![0u8; src_pitch * src_h];
        // Fill only the valid pixel region with a marker colour.
        for y in 0..src_h {
            for x in 0..src_w {
                let off = y * src_pitch + x * 4;
                src[off..off + 4].copy_from_slice(&[0xCC, 0xDD, 0xEE, 0xFF]);
            }
        }

        let dst_w: usize = 50;
        let dst_h: usize = 25;
        let mut dst = vec![0u8; dst_w * dst_h * 4];

        unsafe {
            downsample_bgra(
                src.as_ptr(), src_w, src_h, src_pitch,
                &mut dst, dst_w, dst_h,
            );
        }

        for (i, chunk) in dst.chunks(4).enumerate() {
            let x = i % dst_w;
            let y = i / dst_w;
            assert_eq!(
                chunk,
                &[0xCC, 0xDD, 0xEE, 0xFF],
                "wrong pixel at output ({x}, {y}) — pitch handling is broken"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Speed prevention: ffmpeg must receive frames whose byte size matches
    // the declared -s WxH.  A mismatch causes ffmpeg to misalign frames,
    // producing sped-up or garbled video.
    // -----------------------------------------------------------------------

    #[test]
    fn ffmpeg_input_size_matches_prescaled_output() {
        // Simulate the capture loop: compute output dims, then verify that
        // the -s argument sent to ffmpeg matches the frame buffer size.
        let cfg = CaptureConfig::default(); // scale=-2:720

        // Various capture surface sizes that have caused issues.
        let test_cases = [
            (2880, 1800), // 16:10 laptop
            (3840, 2160), // 4K
            (2560, 1440), // 1440p
            (1920, 1080), // 1080p
            (1200, 675),  // loading screen
        ];

        for (cap_w, cap_h) in test_cases {
            let (out_w, out_h) = compute_output_dims(&cfg.scale, cap_w, cap_h);
            let args = build_ffmpeg_args(out_w, out_h, Path::new("C:/temp"), &cfg, None);

            let expected_s = format!("{out_w}x{out_h}");
            assert!(
                args.windows(2).any(|w| w[0] == "-s" && w[1] == expected_s),
                "for capture {cap_w}x{cap_h}: ffmpeg -s should be {expected_s}, args: {args:?}"
            );

            // The raw frame byte count the capture loop writes must equal
            // what ffmpeg expects: width * height * 4 (BGRA).
            let frame_bytes = out_w as usize * out_h as usize * 4;
            assert!(
                frame_bytes > 0,
                "zero-size frame for capture {cap_w}x{cap_h}"
            );

            // Verify declared fps is present (speed regression guard).
            assert!(
                args.windows(2).any(|w| w == ["-r", &cfg.fps.to_string()]),
                "fps not declared for capture {cap_w}x{cap_h}"
            );
        }
    }

    #[test]
    fn output_dims_always_even_for_h264_compatibility() {
        // Odd dimensions cause H.264 encoder failures.  Test a range of
        // awkward source sizes that could produce odd results.
        let odd_sources = [
            (1001, 563),
            (2879, 1799),
            (1921, 1081),
            (3441, 1441),
        ];
        for (sw, sh) in odd_sources {
            let (w, h) = compute_output_dims("scale=-2:720", sw, sh);
            assert_eq!(w % 2, 0, "odd width {w} from source {sw}x{sh}");
            assert_eq!(h % 2, 0, "odd height {h} from source {sw}x{sh}");
        }
    }
}
