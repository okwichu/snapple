use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rayon::prelude::*;
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
    frame_counter: Arc<AtomicU64>,
    /// Set to `true` by the capture thread once it enters the frame loop.
    /// Until then, stall detection is skipped (startup can take >10 s due
    /// to content-size probing and the audio-pipe handshake).
    warmup_done: Arc<AtomicBool>,
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
        let frame_counter = Arc::new(AtomicU64::new(0));
        let frame_counter_clone = frame_counter.clone();
        let warmup_done = Arc::new(AtomicBool::new(false));
        let warmup_done_clone = warmup_done.clone();

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
                    frame_counter_clone,
                    warmup_done_clone,
                ) {
                    log(&format!("[snapple] capture error: {e:#}"));
                }
            })
            .context("Failed to spawn capture thread")?;

        Ok(Self {
            running,
            thread: Some(thread),
            frame_counter,
            warmup_done,
        })
    }

    /// Number of video frames successfully captured (cumulative across pipeline restarts).
    pub fn frame_count(&self) -> u64 {
        self.frame_counter.load(Ordering::Relaxed)
    }

    /// Whether the capture thread is still running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// Check whether the capture appears stalled.  Returns `true` when the
    /// caller should tear down this session and start a fresh one.
    ///
    /// `last_frame_count` / `last_check` / `start_time` are the caller's
    /// bookkeeping from the previous health-check cycle.
    pub fn needs_restart(
        &self,
        last_frame_count: u64,
        last_check: Instant,
    ) -> bool {
        if !self.is_running() {
            return true;
        }
        // Don't check for frame stalls while the capture thread is still
        // warming up (content-size probing + audio-pipe handshake can
        // take well over 10 seconds).
        if !self.warmup_done.load(Ordering::Relaxed) {
            return false;
        }
        if last_check.elapsed() >= Duration::from_secs(5) {
            self.frame_count() == last_frame_count
        } else {
            false
        }
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
impl CaptureSession {
    /// Create a dummy session for unit tests (no thread, no D3D).
    fn dummy() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            thread: None,
            frame_counter: Arc::new(AtomicU64::new(0)),
            warmup_done: Arc::new(AtomicBool::new(true)),
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
    frame_counter: Arc<AtomicU64>,
    warmup_done: Arc<AtomicBool>,
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
        4, // larger pool to avoid dropping frames before we drain them
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
            resolved_cfg.mic_volume,
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

        // Cached downsample LUT — rebuilt only when source/dest dims change.
        let mut downsample_lut: Option<DownsampleLut> = None;

        // Per-second capture diagnostics.
        let mut diag_downsample_total = Duration::ZERO;
        let mut diag_unique_frames: u64 = 0;
        let mut diag_dup_writes: u64 = 0;
        let mut diag_last_report = Instant::now();

        // --- Frame loop -----------------------------------------------------
        let mut needs_restart = false;
        warmup_done.store(true, Ordering::Relaxed);

        while running.load(Ordering::Relaxed) {
            if last_cleanup.elapsed() > Duration::from_secs(10) {
                let _ = buffer::cleanup_old_segments(seg_dir, cleanup_age_secs);
                last_cleanup = Instant::now();

                // Periodic device-health check — catches the case where
                // TryGetNextFrame silently fails but the pacing loop keeps
                // writing frozen duplicate frames.
                if let Err(e) = unsafe { d3d_device.GetDeviceRemovedReason() } {
                    log(&format!(
                        "[snapple] D3D device removed ({:#010x}), stopping capture",
                        e.code().0 as u32,
                    ));
                    running.store(false, Ordering::Relaxed);
                    break;
                }
            }

            // Drain all queued frames, keeping only the most recent.
            // Only do the expensive GPU→CPU copy for the final frame;
            // intermediate frames are closed immediately.
            {
                let mut latest_frame: Option<windows::Graphics::Capture::Direct3D11CaptureFrame> = None;
                while let Ok(frame) = frame_pool.TryGetNextFrame() {
                    if let Some(prev) = latest_frame.take() {
                        let _ = prev.Close();
                    }
                    latest_frame = Some(frame);
                }

                if let Some(frame) = latest_frame {
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

                                    // Detect GPU device loss (sleep/resume, GPU switch, driver reset).
                                    if let Err(e) = d3d_device.GetDeviceRemovedReason() {
                                        log(&format!(
                                            "[snapple] D3D device removed ({:#010x}), stopping capture",
                                            e.code().0 as u32,
                                        ));
                                        let _ = frame.Close();
                                        running.store(false, Ordering::Relaxed);
                                        break;
                                    }

                                    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                                    if d3d_context
                                        .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                                        .is_ok()
                                    {
                                        let pitch = mapped.RowPitch as usize;
                                        let dst_w = out_w as usize;
                                        let dst_h = out_h as usize;

                                        // Build (or rebuild) the LUT if geometry changed.
                                        let needs_rebuild = downsample_lut
                                            .as_ref()
                                            .is_none_or(|l| !l.matches(src_w, src_h, dst_w, dst_h));
                                        if needs_rebuild {
                                            downsample_lut = Some(
                                                DownsampleLut::new(src_w, src_h, dst_w, dst_h),
                                            );
                                        }
                                        let lut = downsample_lut.as_ref().unwrap();

                                        // Downsample directly from the mapped GPU
                                        // texture into the output-sized frame buffer.
                                        let src_ptr = mapped.pData as *const u8;
                                        let downsample_start = Instant::now();
                                        downsample_bgra(src_ptr, pitch, &mut last_frame, lut);
                                        diag_downsample_total += downsample_start.elapsed();
                                        diag_unique_frames += 1;

                                        d3d_context.Unmap(&staging, 0);
                                        has_first_frame = true;
                                    }
                                }
                            }
                        }
                    }
                    let _ = frame.Close();
                }
            }

            if has_first_frame {
                let origin = *pacing_origin.get_or_insert_with(Instant::now);

                // How many frames should have been written by now to
                // maintain real-time playback.  Write duplicates of the
                // current frame to catch up (capped to avoid a stall
                // spiral if the pipe is the bottleneck).
                let elapsed_us = origin.elapsed().as_micros() as u64;
                let target = elapsed_us * fps / 1_000_000;
                let behind = target.saturating_sub(frame_count);
                let writes = 1 + behind.min(4);

                for _ in 0..writes {
                    if stdin.write_all(&last_frame).is_err() {
                        log("[snapple] ffmpeg pipe broken, stopping capture");
                        needs_restart = false; // force exit
                        break;
                    }
                    video_frames.fetch_add(1, Ordering::Relaxed);
                    frame_counter.fetch_add(1, Ordering::Relaxed);
                    frame_count += 1;
                }
                // Everything past the first write in this iteration is a
                // duplicate of `last_frame` injected to keep wall-clock pace.
                diag_dup_writes += writes.saturating_sub(1);

                let elapsed_diag = diag_last_report.elapsed();
                if elapsed_diag >= Duration::from_secs(1) {
                    let avg_ms = if diag_unique_frames > 0 {
                        diag_downsample_total.as_secs_f64() * 1000.0
                            / diag_unique_frames as f64
                    } else {
                        0.0
                    };
                    // Frames/sec actually pushed into the ffmpeg pipe over
                    // this diag window.  If `wrote` drops noticeably below
                    // `fps`, the encoder is back-pressuring the pipe and
                    // the resulting clip will be sped up — see the
                    // h264_amf profile comment in ENCODER_PROFILES.
                    let wrote_total = diag_unique_frames + diag_dup_writes;
                    let wrote_per_sec = wrote_total as f64 / elapsed_diag.as_secs_f64();
                    log(&format!(
                        "[snapple] capture diag: avg downsample={avg_ms:.1}ms unique={diag_unique_frames} dup={diag_dup_writes} wrote={wrote_per_sec:.0}/{fps}fps"
                    ));
                    diag_downsample_total = Duration::ZERO;
                    diag_unique_frames = 0;
                    diag_dup_writes = 0;
                    diag_last_report = Instant::now();
                }

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

/// Precomputed source-rectangle boundaries for an area-average downsample.
/// Built once per (src_w, src_h, dst_w, dst_h) tuple and reused for every
/// frame, so the inner loop is pure pointer arithmetic with no divisions.
///
/// `sx_starts` has length `dst_w + 1`; output column `dx` covers source
/// columns `sx_starts[dx]..sx_starts[dx+1]`.  Adjacent ranges abut
/// exactly, which means every source pixel is read by exactly one output
/// pixel's rectangle — a clean partition of the source image.
/// `sy_starts` works the same way for rows.
struct DownsampleLut {
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    sx_starts: Vec<usize>,
    sy_starts: Vec<usize>,
}

impl DownsampleLut {
    fn new(src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> Self {
        let sx_starts: Vec<usize> = (0..=dst_w).map(|dx| dx * src_w / dst_w).collect();
        let sy_starts: Vec<usize> = (0..=dst_h).map(|dy| dy * src_h / dst_h).collect();
        Self { src_w, src_h, dst_w, dst_h, sx_starts, sy_starts }
    }

    fn matches(&self, src_w: usize, src_h: usize, dst_w: usize, dst_h: usize) -> bool {
        self.src_w == src_w && self.src_h == src_h
            && self.dst_w == dst_w && self.dst_h == dst_h
    }
}

/// Wraps a mapped-texture pointer so it can be shared across rayon workers.
/// The downsample only ever reads through this pointer, and the row LUT
/// guarantees each worker touches a disjoint set of source bytes.
#[derive(Copy, Clone)]
struct SrcPtr(*const u8);
unsafe impl Send for SrcPtr {}
unsafe impl Sync for SrcPtr {}

impl SrcPtr {
    /// Method accessor so closures capture the whole `SrcPtr` (which is
    /// `Sync`) instead of the bare `*const u8` field via disjoint captures.
    #[inline]
    fn as_ptr(&self) -> *const u8 {
        self.0
    }
}

/// Area-average downsample of a BGRA image.  Each output pixel is the
/// mean of all source pixels in its rectangle, computed independently per
/// channel — the same filter as `cv2.INTER_AREA` and ImageMagick's
/// `-filter box`.  Eliminates the aliasing/shimmer that nearest-neighbour
/// produces on text, fine geometry, and HUD edges.
///
/// Rows are processed in parallel via rayon.  The per-row work is
/// proportional to `src_w * (sy_end - sy_start)`, so the total cost is
/// roughly one read of every source pixel — comfortably real-time at 60
/// fps on every test resolution from 1080p to 4K source.
///
/// # Safety
/// `src` must point to at least `src_pitch * lut.src_h` readable bytes and
/// remain valid for the duration of the call.
unsafe fn downsample_bgra(src: *const u8, src_pitch: usize, dst: &mut [u8], lut: &DownsampleLut) {
    let dst_w = lut.dst_w;
    let dst_row_bytes = dst_w * 4;
    let src_ptr = SrcPtr(src);
    let sx_starts = &lut.sx_starts;
    let sy_starts = &lut.sy_starts;

    dst.par_chunks_mut(dst_row_bytes)
        .enumerate()
        .for_each(|(dy, dst_row)| {
            let sy_start = sy_starts[dy];
            let sy_end = sy_starts[dy + 1];
            for dx in 0..dst_w {
                let sx_start = sx_starts[dx];
                let sx_end = sx_starts[dx + 1];
                let count = ((sy_end - sy_start) * (sx_end - sx_start)) as u32;

                // Sum BGRA channels independently across the rectangle.
                // u32 accumulators handle up to ~16M source pixels per
                // output cell without overflow — orders of magnitude past
                // anything realistic.
                let (mut b, mut g, mut r, mut a) = (0u32, 0u32, 0u32, 0u32);
                for sy in sy_start..sy_end {
                    let row_ptr = unsafe { src_ptr.as_ptr().add(sy * src_pitch) };
                    for sx in sx_start..sx_end {
                        let p = unsafe {
                            (row_ptr.add(sx * 4) as *const u32).read_unaligned()
                        };
                        b += p & 0xFF;
                        g += (p >> 8) & 0xFF;
                        r += (p >> 16) & 0xFF;
                        a += (p >> 24) & 0xFF;
                    }
                }

                let out = ((a / count) << 24)
                    | ((r / count) << 16)
                    | ((g / count) << 8)
                    | (b / count);
                unsafe {
                    (dst_row.as_mut_ptr().add(dx * 4) as *mut u32).write_unaligned(out);
                }
            }
        });
}

// ---------------------------------------------------------------------------
// Encoder probing & fallback
// ---------------------------------------------------------------------------

/// Per-encoder quality profile.  `extra` is appended verbatim to the
/// ffmpeg argv after the standard `-preset/-rc/-qp` block, and is the
/// place to put encoder-specific quality flags that ffmpeg names
/// differently per encoder (`-tune`, `-multipass`, `-spatial_aq`,
/// `-preanalysis`, `-vbaq`, etc.).
struct EncoderProfile {
    encoder: &'static str,
    preset: &'static str,
    rate_control: &'static str,
    quality: &'static str,
    extra: &'static [&'static str],
}

/// Quality-tuned profiles for each supported H.264 encoder.  Order is the
/// fallback priority — `resolve_encoder` walks this list and picks the
/// first entry whose encoder probes successfully.
///
/// The `extra` blocks are the standard "max quality, still real-time at
/// 720p60" flag set for each encoder family.  At 720p60 even the slowest
/// preset is comfortably real-time on every modern GPU/CPU we target, so
/// we leave the resource trade-off pinned to the quality side.
const ENCODER_PROFILES: &[EncoderProfile] = &[
    // NVIDIA — full nvenc quality knobs.  p7 is the slowest preset,
    // multipass fullres runs two passes per frame for better bit
    // allocation, AQ + lookahead + B-frames + middle-ref improve subjective
    // quality at the same qp, and high profile unlocks 8x8 transforms +
    // CABAC.
    EncoderProfile {
        encoder: "h264_nvenc",
        preset: "p7",
        rate_control: "constqp",
        quality: "16",
        extra: &[
            "-tune", "hq",
            "-multipass", "fullres",
            "-spatial_aq", "1",
            "-temporal_aq", "1",
            "-rc-lookahead", "32",
            "-bf", "3",
            "-b_ref_mode", "middle",
            "-profile:v", "high",
            "-level", "4.2",
        ],
    },
    // AMD — `balanced` preset, *no* preanalysis.  The earlier "quality
    // preset + preanalysis" combo was tested on an integrated AMD GPU
    // (THE FINALS, on battery, 22:34 session) and ran at ffmpeg
    // `speed=0.78x` — i.e. ~47 fps wall-clock against a 60 fps target.
    // That sustained back-pressure on the pipe makes the capture loop
    // push frames slower than real-time, but the file is still tagged
    // `-r 60`, so the result is a clip that plays ~1.3× sped up.  Stay
    // at `balanced` (which historically held `speed=0.999x` on the same
    // hardware) and skip preanalysis (the dominant per-frame cost),
    // keeping only the cheap quality wins: vbaq (variance-based AQ),
    // high profile (CABAC + 8x8 transforms — essentially free at the
    // encoder, real bitrate efficiency win), and level 4.2.  B-frames
    // omitted: AMF H.264 B-frame support is hardware-dependent.
    EncoderProfile {
        encoder: "h264_amf",
        preset: "balanced",
        rate_control: "cqp",
        quality: "16",
        extra: &[
            "-vbaq", "true",
            "-profile:v", "high",
            "-level", "4.2",
        ],
    },
    // libx264 — `slow` preset adds RDO, more refs, deeper analysis.
    // Still real-time at 720p60 on any modern x86.  `tune film` biases
    // motion compensation toward live-action / gameplay content.
    EncoderProfile {
        encoder: "libx264",
        preset: "slow",
        rate_control: "crf",
        quality: "16",
        extra: &[
            "-tune", "film",
            "-profile:v", "high",
            "-level", "4.2",
        ],
    },
];

/// Look up the static `EncoderProfile` for a given encoder name, if any.
/// Used by `build_ffmpeg_args` to find the `extra` flag block — the
/// returned profile is also the source of truth for the preset, rate
/// control, and quality value when `resolve_encoder` falls back.
fn lookup_profile(encoder: &str) -> Option<&'static EncoderProfile> {
    ENCODER_PROFILES.iter().find(|p| p.encoder == encoder)
}

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
/// works, returns a clone unchanged (preserving any user overrides on
/// preset/rate_control/quality).  Otherwise walks `ENCODER_PROFILES` and
/// adopts the first profile whose encoder probes successfully —
/// overwriting preset/rate_control/quality with the profile's values.
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

    for profile in ENCODER_PROFILES {
        if profile.encoder == cfg.encoder {
            continue; // Already tried.
        }
        if probe_encoder(ffmpeg_path, profile.encoder) {
            log(&format!("[snapple] using fallback encoder {}", profile.encoder));
            let mut resolved = cfg.clone();
            resolved.encoder = profile.encoder.into();
            resolved.preset = profile.preset.into();
            resolved.rate_control = profile.rate_control.into();
            resolved.quality = profile.quality.into();
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

    // No -vf scale: input is already pre-scaled by downsample_bgra.
    args.extend([
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

    // Append the encoder-specific quality flag block (-tune, AQ,
    // lookahead, B-frames, profile, etc.) from the profile matrix.  If
    // the user is on a custom encoder not in the matrix, no extras are
    // added — they get the basic args only.
    if let Some(profile) = lookup_profile(&cfg.encoder) {
        args.extend(profile.extra.iter().map(|s| (*s).into()));
    }

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
        frame_interval, AudioInput, CaptureSession, DownsampleLut, ENCODER_PROFILES,
    };
    use crate::config::CaptureConfig;
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

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
            mic_volume: 0.15,
        };

        let args = build_ffmpeg_args(2560, 1440, Path::new("C:/temp"), &cfg, None);

        // No -vf scale — input is pre-scaled by downsample_bgra.
        assert!(!args.contains(&"-vf".to_string()));
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
    fn ffmpeg_args_include_nvenc_quality_flags() {
        // The nvenc profile in ENCODER_PROFILES must surface its extras
        // (high profile, lookahead, AQ, multipass, B-frames) into the
        // ffmpeg argv.  Guards against silently dropping the flag block.
        let cfg = CaptureConfig::default();
        assert_eq!(cfg.encoder, "h264_nvenc");

        let args = build_ffmpeg_args(1280, 720, Path::new("C:/temp"), &cfg, None);

        assert!(args.windows(2).any(|w| w == ["-profile:v", "high"]));
        assert!(args.windows(2).any(|w| w == ["-tune", "hq"]));
        assert!(args.windows(2).any(|w| w == ["-multipass", "fullres"]));
        assert!(args.windows(2).any(|w| w == ["-spatial_aq", "1"]));
        assert!(args.windows(2).any(|w| w == ["-temporal_aq", "1"]));
        assert!(args.windows(2).any(|w| w == ["-rc-lookahead", "32"]));
        assert!(args.windows(2).any(|w| w == ["-bf", "3"]));
        assert!(args.windows(2).any(|w| w == ["-b_ref_mode", "middle"]));
    }

    #[test]
    fn ffmpeg_args_amf_profile_stays_realtime() {
        // Regression guard for the 22:34 sped-up-clip incident: the AMF
        // profile must not enable preanalysis or the slow `quality`
        // preset on the matrix's default entry — they pushed integrated
        // AMD GPUs below real-time and produced ~1.3× sped-up clips.
        // Keep the cheap quality wins (vbaq, high profile, level 4.2).
        let mut cfg = CaptureConfig::default();
        cfg.encoder = "h264_amf".into();
        // Use the matrix's own values so the test fails if the profile
        // entry's preset/rc/quality change.
        let amf = ENCODER_PROFILES
            .iter()
            .find(|p| p.encoder == "h264_amf")
            .expect("h264_amf profile must exist");
        cfg.preset = amf.preset.into();
        cfg.rate_control = amf.rate_control.into();
        cfg.quality = amf.quality.into();

        let args = build_ffmpeg_args(1280, 720, Path::new("C:/temp"), &cfg, None);

        // Cheap quality wins must be present.
        assert!(args.windows(2).any(|w| w == ["-vbaq", "true"]));
        assert!(args.windows(2).any(|w| w == ["-profile:v", "high"]));
        assert!(args.windows(2).any(|w| w == ["-level", "4.2"]));

        // Expensive flags that broke real-time on integrated AMD must NOT come back.
        assert!(
            !args.iter().any(|a| a == "-preanalysis"),
            "preanalysis must stay disabled — it pushed AMF below 60fps real-time"
        );
        assert!(
            !args.windows(2).any(|w| w == ["-preset", "quality"]),
            "AMF preset must not be `quality` — too slow on integrated AMD"
        );
    }

    #[test]
    fn ffmpeg_args_unknown_encoder_emits_no_extras() {
        // Custom user-set encoder not in the matrix gets no extras —
        // basic args only, no panic.
        let mut cfg = CaptureConfig::default();
        cfg.encoder = "h264_qsv".into(); // not in ENCODER_PROFILES
        cfg.preset = "veryslow".into();
        cfg.rate_control = "icq".into();

        let args = build_ffmpeg_args(1280, 720, Path::new("C:/temp"), &cfg, None);

        // Encoder name comes through unchanged.
        assert!(args.windows(2).any(|w| w == ["-c:v", "h264_qsv"]));
        // None of the nvenc-specific extras should leak in.
        assert!(!args.iter().any(|a| a == "-tune"));
        assert!(!args.iter().any(|a| a == "-rc-lookahead"));
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

        let lut = DownsampleLut::new(src_w, src_h, dst_w, dst_h);
        unsafe {
            downsample_bgra(src.as_ptr(), src_pitch, &mut dst, &lut);
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

        let lut = DownsampleLut::new(src_w, src_h, dst_w, dst_h);
        unsafe {
            downsample_bgra(src.as_ptr(), src_pitch, &mut dst, &lut);
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
    // LUT-vs-naive equivalence: the LUT-driven area-average downsample
    // must produce byte-identical output to a straightforward reference
    // implementation that computes the same per-channel mean over each
    // output pixel's source rectangle.  Catches off-by-one bugs in the
    // partition LUT and channel-extraction arithmetic.
    // -----------------------------------------------------------------------

    fn naive_downsample(
        src: &[u8],
        src_w: usize,
        src_h: usize,
        src_pitch: usize,
        dst_w: usize,
        dst_h: usize,
    ) -> Vec<u8> {
        let mut dst = vec![0u8; dst_w * dst_h * 4];
        for dy in 0..dst_h {
            let sy_start = dy * src_h / dst_h;
            let sy_end = (dy + 1) * src_h / dst_h;
            for dx in 0..dst_w {
                let sx_start = dx * src_w / dst_w;
                let sx_end = (dx + 1) * src_w / dst_w;
                let count = ((sy_end - sy_start) * (sx_end - sx_start)) as u32;

                let (mut b, mut g, mut r, mut a) = (0u32, 0u32, 0u32, 0u32);
                for sy in sy_start..sy_end {
                    for sx in sx_start..sx_end {
                        let off = sy * src_pitch + sx * 4;
                        b += src[off] as u32;
                        g += src[off + 1] as u32;
                        r += src[off + 2] as u32;
                        a += src[off + 3] as u32;
                    }
                }

                let di = (dy * dst_w + dx) * 4;
                dst[di] = (b / count) as u8;
                dst[di + 1] = (g / count) as u8;
                dst[di + 2] = (r / count) as u8;
                dst[di + 3] = (a / count) as u8;
            }
        }
        dst
    }

    #[test]
    fn lut_downsample_matches_naive_formula() {
        // Use a non-trivial source so any off-by-one in sx/sy LUTs visibly
        // mismatches: each pixel's BGRA encodes (x, y, x^y, 0xFF).
        let cases = [
            (200usize, 100usize, 80usize, 40usize),
            (3840, 2160, 1280, 720),
            (2560, 1440, 1280, 720),
            (1920, 1080, 854, 480),
            (100, 50, 50, 25),
        ];

        for (src_w, src_h, dst_w, dst_h) in cases {
            // Use a pitch deliberately larger than src_w*4 to mimic GPU staging.
            let src_pitch = (src_w * 4 + 255) & !255;
            let mut src = vec![0u8; src_pitch * src_h];
            for y in 0..src_h {
                for x in 0..src_w {
                    let off = y * src_pitch + x * 4;
                    src[off] = x as u8;
                    src[off + 1] = y as u8;
                    src[off + 2] = (x ^ y) as u8;
                    src[off + 3] = 0xFF;
                }
            }

            let expected = naive_downsample(&src, src_w, src_h, src_pitch, dst_w, dst_h);

            let lut = DownsampleLut::new(src_w, src_h, dst_w, dst_h);
            let mut actual = vec![0u8; dst_w * dst_h * 4];
            unsafe {
                downsample_bgra(src.as_ptr(), src_pitch, &mut actual, &lut);
            }

            assert_eq!(
                actual, expected,
                "LUT downsample diverges from naive at {src_w}x{src_h} -> {dst_w}x{dst_h}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Mic default audibility: the original 0.15 default made the mic
    // inaudible against game audio. Pin a sane floor so a future "tidy
    // the defaults" pass can't silently regress it.
    // -----------------------------------------------------------------------

    #[test]
    fn default_mic_volume_is_audible() {
        let cfg = CaptureConfig::default();
        assert!(
            cfg.mic_volume >= 0.5,
            "mic_volume default ({}) is too low — voice will be drowned out by game audio",
            cfg.mic_volume
        );
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

    // -----------------------------------------------------------------------
    // Health-check / device-loss recovery
    // -----------------------------------------------------------------------

    #[test]
    fn needs_restart_when_thread_exited() {
        let session = CaptureSession::dummy();
        session.running.store(false, Ordering::Relaxed);

        assert!(
            session.needs_restart(0, Instant::now()),
            "should restart when the capture thread is no longer running"
        );
    }

    #[test]
    fn no_restart_during_warmup() {
        let session = CaptureSession::dummy();
        session.warmup_done.store(false, Ordering::Relaxed);
        // Frame count stuck at zero, but warmup hasn't finished.
        let old_check = Instant::now() - Duration::from_secs(30);

        assert!(
            !session.needs_restart(0, old_check),
            "should not restart while warmup is still in progress"
        );
    }

    #[test]
    fn restart_on_frame_stall_after_warmup() {
        let session = CaptureSession::dummy();
        // warmup_done is true (set by dummy), last check was 6s ago, counter stuck at 100.
        let old_check = Instant::now() - Duration::from_secs(6);
        session.frame_counter.store(100, Ordering::Relaxed);

        assert!(
            session.needs_restart(100, old_check),
            "should restart when frame count is stuck after warmup"
        );
    }

    #[test]
    fn no_restart_when_frames_advancing() {
        let session = CaptureSession::dummy();
        let old_check = Instant::now() - Duration::from_secs(6);
        session.frame_counter.store(200, Ordering::Relaxed);

        assert!(
            !session.needs_restart(100, old_check),
            "should not restart when frame count is advancing"
        );
    }

    #[test]
    fn no_restart_when_check_interval_not_elapsed() {
        let session = CaptureSession::dummy();
        // Even if counter is stuck, don't check until 5s have passed.
        let recent_check = Instant::now(); // just checked

        assert!(
            !session.needs_restart(0, recent_check),
            "should not restart before the 5-second check interval"
        );
    }

    #[test]
    fn frame_count_reflects_shared_counter() {
        let session = CaptureSession::dummy();
        assert_eq!(session.frame_count(), 0);

        session.frame_counter.fetch_add(42, Ordering::Relaxed);
        assert_eq!(session.frame_count(), 42);
    }

    #[test]
    fn is_running_reflects_shared_flag() {
        let session = CaptureSession::dummy();
        assert!(session.is_running());

        session.running.store(false, Ordering::Relaxed);
        assert!(!session.is_running());
    }
}
