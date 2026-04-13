// WASAPI audio capture: system loopback (game audio) + microphone, mixed to a named pipe.

use std::io::Write;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::core::{implement, Interface, Ref, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::*;
use windows::Win32::System::Pipes::*;
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BLOB;

use crate::log;

// WASAPI stream flag for loopback capture.
const STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;

// WAVEFORMATEX tag values.
const WAVE_FORMAT_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

// AUDCLNT_BUFFERFLAGS
const BUFFERFLAGS_SILENT: u32 = 0x0000_0002;
// 1-second WASAPI buffer (in 100 ns units) — large enough that brief pipe
// stalls or encoding back-pressure won't cause WASAPI to drop audio.
const SHARED_BUFFER_DURATION_100NS: i64 = 10_000_000;
// Safety cap on the pending buffer.  150 ms is tight enough to prevent
// audible audio delay when encoding backpressure stalls the video thread,
// while still absorbing normal scheduling jitter.
const MAX_PENDING_AUDIO_MS: usize = 150;

// Loopback gain applied during the mix so the sum of game audio + mic has
// headroom before the soft limiter engages.
const LOOPBACK_MIX_GAIN: f32 = 0.7;

/// Soft-knee limiter: smoothly compresses values toward ±1.0 instead of the
/// audible square-wave distortion produced by hard clamping. Identity near
/// zero, asymptotes to ±1 for large inputs.
#[inline]
fn soft_clip(x: f32) -> f32 {
    x / (1.0 + x.abs())
}

/// Wraps a HANDLE so it can be sent to another thread.
///
/// # Safety
/// The caller must ensure exclusive ownership — the handle must not be used
/// from the original thread after sending.
struct SendableHandle(HANDLE);
unsafe impl Send for SendableHandle {}

/// Manages the audio capture thread and the named pipe that feeds ffmpeg.
pub struct AudioPipe {
    pub pipe_path: String,
    pub sample_rate: u32,
    /// Set to `true` once WASAPI capture is running and the pipe is
    /// connected.  The capture thread should wait for this before
    /// sending video frames so both streams start at the same time.
    pub ready: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPipe {
    /// Create the named pipe and spawn the audio capture thread.
    /// Call **before** spawning ffmpeg so the pipe path exists when ffmpeg opens it.
    pub fn start(
        mic_device: &str,
        mic_volume: f32,
        game_pid: u32,
        running: Arc<AtomicBool>,
        video_frames: Arc<AtomicU64>,
        video_fps: u64,
    ) -> Result<Self> {
        // Probe loopback sample rate on this thread so we can return it to the caller.
        let sample_rate = detect_loopback_sample_rate()?;

        let pipe_path = format!(r"\\.\pipe\snapple_audio_{}", std::process::id());
        let pipe_wide: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();

        let pipe_handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(pipe_wide.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(2), // PIPE_ACCESS_OUTBOUND
                PIPE_TYPE_BYTE | PIPE_NOWAIT,  // non-blocking for ConnectNamedPipe polling
                1,     // max instances
                65536, // out buffer
                0,     // in buffer (unused for outbound)
                0,     // default timeout
                None,
            )
        };
        if pipe_handle == INVALID_HANDLE_VALUE {
            anyhow::bail!("CreateNamedPipeW failed");
        }

        let handle = SendableHandle(pipe_handle);
        let mic = mic_device.to_string();
        let ready = Arc::new(AtomicBool::new(false));
        let ready_clone = ready.clone();

        let thread = thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                if let Err(e) = audio_thread(
                    handle,
                    &mic,
                    mic_volume,
                    game_pid,
                    sample_rate,
                    &running,
                    &video_frames,
                    video_fps,
                    &ready_clone,
                ) {
                    log(&format!("[snapple] audio error: {e:#}"));
                }
            })
            .context("Failed to spawn audio thread")?;

        Ok(Self {
            pipe_path,
            sample_rate,
            ready,
            thread: Some(thread),
        })
    }
}

impl Drop for AudioPipe {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------
// WASAPI helpers
// ---------------------------------------------------------------------------

struct WasapiSource {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    sample_rate: u32,
    channels: u16,
    is_float: bool,
}

/// Probe the default render endpoint's sample rate without initialising a full
/// capture session. Runs once per game launch — the overhead is negligible.
fn detect_loopback_sample_rate() -> Result<u32> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let fmt = client.GetMixFormat()?;
        let rate = (*fmt).nSamplesPerSec;
        CoTaskMemFree(Some(fmt as *const _ as _));
        Ok(rate)
    }
}

fn is_float_format(tag: u16, bits: u16) -> bool {
    tag == WAVE_FORMAT_FLOAT || (tag == WAVE_FORMAT_EXTENSIBLE && bits == 32)
}

/// Log the device ID of the current default render device.
fn log_default_render_device() -> String {
    unsafe {
        let enumerator: std::result::Result<IMMDeviceEnumerator, _> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL);
        let Ok(enumerator) = enumerator else {
            return "(unknown)".into();
        };
        let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
            return "(unknown)".into();
        };
        device
            .GetId()
            .ok()
            .and_then(|id| id.to_string().ok())
            .unwrap_or_else(|| "(unknown)".into())
    }
}

/// Open a WASAPI capture source on the default endpoint.
///
/// * `data_flow` — `eRender` for loopback (game audio), `eCapture` for microphone.
/// * `stream_flags` — pass `STREAMFLAGS_LOOPBACK` for loopback, `0` for mic.
fn open_wasapi(data_flow: EDataFlow, stream_flags: u32) -> Result<WasapiSource> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(data_flow, eConsole)?;
        open_wasapi_device(&device, stream_flags)
    }
}

/// Open a WASAPI capture source on a specific `IMMDevice`.
fn open_wasapi_device(device: &IMMDevice, stream_flags: u32) -> Result<WasapiSource> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let fmt = client.GetMixFormat()?;

        let sr = (*fmt).nSamplesPerSec;
        let ch = (*fmt).nChannels;
        let bits = (*fmt).wBitsPerSample;
        let tag = (*fmt).wFormatTag;

        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            stream_flags,
            SHARED_BUFFER_DURATION_100NS,
            0,
            fmt,
            None,
        )?;

        let capture: IAudioCaptureClient = client.GetService()?;
        CoTaskMemFree(Some(fmt as *const _ as _));

        Ok(WasapiSource {
            client,
            capture,
            sample_rate: sr,
            channels: ch,
            is_float: is_float_format(tag, bits),
        })
    }
}

// ---------------------------------------------------------------------------
// Process loopback (Windows 10 build 20348+)
// ---------------------------------------------------------------------------
//
// `ActivateAudioInterfaceAsync` against the `VAD\Process_Loopback` virtual
// device gives us a loopback `IAudioClient` that captures whatever the
// target process tree renders, regardless of which render endpoint Windows
// is currently routing the game to.  This avoids the entire class of bugs
// where loopback opens the "wrong" device — e.g. on hybrid laptops when
// the dGPU sleeps and the set of HDMI/DisplayPort audio endpoints changes
// while the game is still rendering to a now-stale endpoint.
//
// Side effect (almost always desired for game clips): only the target
// process tree is captured, so Discord, browser audio, and OS sounds no
// longer leak into clips.

/// Completion handler for `ActivateAudioInterfaceAsync`. The activation is
/// asynchronous — we hand it this handler and a Win32 event, then block on
/// the event until `ActivateCompleted` fires and we can call
/// `GetActivateResult` on the returned operation.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    event: HANDLE,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        unsafe {
            let _ = SetEvent(self.event);
        }
        Ok(())
    }
}

/// Open a process-loopback capture against `game_pid`'s process tree.
///
/// Returns `Err` on Windows builds older than 10.0.20348, when the game's
/// process can't be activated for loopback (anti-cheat blocks, invalid PID),
/// or when the activation otherwise fails.  Callers should treat this as a
/// best-effort path and fall back to default-device loopback.
fn open_process_loopback(game_pid: u32) -> Result<WasapiSource> {
    unsafe {
        // Build the activation parameters describing the target PID and the
        // include-process-tree mode (so we capture child processes too).
        let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: game_pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };

        // Wrap the params in a PROPVARIANT of type VT_BLOB.  This packaging
        // is the documented quirk of `ActivateAudioInterfaceAsync` —
        // anything else returns E_INVALIDARG.
        //
        // ManuallyDrop suppresses `PROPVARIANT::drop` (which calls
        // `PropVariantClear` → `CoTaskMemFree(pBlobData)`).  Our blob
        // points at a stack local, not heap, so freeing it would corrupt
        // the heap.
        let mut activate_params: core::mem::ManuallyDrop<PROPVARIANT> =
            core::mem::ManuallyDrop::new(core::mem::zeroed());
        {
            let inner = &mut *activate_params.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob.cbSize = core::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32;
            inner.Anonymous.blob.pBlobData = &mut params as *mut _ as *mut u8;
        }

        // Manual-reset event signalled by the completion handler.
        let event = CreateEventW(None, true, false, PCWSTR::null())
            .context("CreateEventW for activation handler")?;

        let handler: IActivateAudioInterfaceCompletionHandler =
            ActivationHandler { event }.into();

        let op = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*activate_params),
            &handler,
        );

        let op = match op {
            Ok(op) => op,
            Err(e) => {
                let _ = CloseHandle(event);
                return Err(anyhow::anyhow!("ActivateAudioInterfaceAsync: {e}"));
            }
        };

        // Wait up to 5 s for the completion handler to fire.
        let wait = WaitForSingleObject(event, 5000);
        let _ = CloseHandle(event);
        if wait != WAIT_OBJECT_0 {
            anyhow::bail!("activation handler did not complete within 5s");
        }

        // Pull the activation result back out of the operation.
        let mut activate_hr = windows::core::HRESULT(0);
        let mut activated_iface: Option<windows::core::IUnknown> = None;
        op.GetActivateResult(&mut activate_hr, &mut activated_iface)
            .context("GetActivateResult")?;
        activate_hr.ok().context("activation HRESULT")?;
        let client: IAudioClient = activated_iface
            .context("activation returned no interface")?
            .cast()
            .context("activation interface is not IAudioClient")?;

        // The virtual loopback device has no native mix format — we must
        // pick one explicitly.  48 kHz / 2 channel / 32-bit float matches
        // what every modern game renders internally and what the existing
        // resampler downstream expects.
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_FLOAT,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 2 * 4,
            nBlockAlign: 2 * 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                STREAMFLAGS_LOOPBACK,
                SHARED_BUFFER_DURATION_100NS,
                0,
                &format,
                None,
            )
            .context("IAudioClient::Initialize for process loopback")?;

        let capture: IAudioCaptureClient = client.GetService().context("GetService")?;

        Ok(WasapiSource {
            client,
            capture,
            sample_rate: 48_000,
            channels: 2,
            is_float: true,
        })
    }
}

/// Drain all available samples from a WASAPI capture client into `out` (interleaved f32).
fn drain_samples_into(src: &WasapiSource, out: &mut Vec<f32>) {
    out.clear();
    unsafe {
        loop {
            let pkt = match src.capture.GetNextPacketSize() {
                Ok(n) => n,
                Err(_) => break,
            };
            if pkt == 0 {
                break;
            }

            let mut buf: *mut u8 = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            if src
                .capture
                .GetBuffer(&mut buf, &mut frames, &mut flags, None, None)
                .is_err()
            {
                break;
            }

            let n = frames as usize * src.channels as usize;
            if flags & BUFFERFLAGS_SILENT != 0 {
                out.extend(std::iter::repeat_n(0.0f32, n));
            } else if src.is_float {
                let sl = std::slice::from_raw_parts(buf as *const f32, n);
                out.extend_from_slice(sl);
            } else {
                let sl = std::slice::from_raw_parts(buf as *const i16, n);
                for &s in sl {
                    out.push(s as f32 / 32768.0);
                }
            }

            let _ = src.capture.ReleaseBuffer(frames);
        }
    }
}

// ---------------------------------------------------------------------------
// Format conversion (buffer-reusing variants)
// ---------------------------------------------------------------------------

/// Convert non-stereo interleaved samples to stereo.
/// Only call when `ch != 2` — for stereo input, use the buffer directly.
fn to_stereo_into(data: &[f32], ch: u16, out: &mut Vec<f32>) {
    out.clear();
    match ch {
        1 => {
            out.reserve(data.len() * 2);
            for &s in data {
                out.push(s);
                out.push(s);
            }
        }
        n => {
            let frames = data.len() / n as usize;
            out.reserve(frames * 2);
            for f in 0..frames {
                out.push(data[f * n as usize]);
                out.push(data[f * n as usize + 1]);
            }
        }
    }
}

/// Linear-interpolation resampler for interleaved stereo.
/// Only call when `from != to` — for matching rates, use the buffer directly.
fn resample_stereo_into(data: &[f32], from: u32, to: u32, out: &mut Vec<f32>) {
    out.clear();
    if data.is_empty() {
        return;
    }
    let in_frames = data.len() / 2;
    let out_frames = (in_frames as u64 * to as u64 / from as u64) as usize;
    out.reserve(out_frames * 2);
    for i in 0..out_frames {
        let pos = i as f64 * from as f64 / to as f64;
        let idx = pos as usize;
        let frac = (pos - idx as f64) as f32;
        for c in 0..2usize {
            let a = data.get(idx * 2 + c).copied().unwrap_or(0.0);
            let b = data.get((idx + 1) * 2 + c).copied().unwrap_or(a);
            out.push(a + (b - a) * frac);
        }
    }
}

// ---------------------------------------------------------------------------
// Audio thread
// ---------------------------------------------------------------------------

fn audio_thread(
    pipe: SendableHandle,
    mic_device: &str,
    mic_volume: f32,
    game_pid: u32,
    target_rate: u32,
    running: &AtomicBool,
    video_frames: &AtomicU64,
    video_fps: u64,
    ready: &AtomicBool,
) -> Result<()> {
    let pipe = pipe.0;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    // Prefer process loopback so we follow the game's audio regardless of
    // which render endpoint Windows happens to be routing it to.  Falls
    // back to default-device loopback on older Windows builds or if
    // activation fails for any reason (anti-cheat, permissions, etc.).
    let loopback = match open_process_loopback(game_pid) {
        Ok(lb) => {
            log(&format!(
                "[snapple] audio loopback: process pid={game_pid} {}Hz {}ch float={}",
                lb.sample_rate, lb.channels, lb.is_float
            ));
            lb
        }
        Err(e) => {
            log(&format!(
                "[snapple] process loopback unavailable ({e:#}), falling back to default device"
            ));
            let loopback_device_name = log_default_render_device();
            let lb =
                open_wasapi(eRender, STREAMFLAGS_LOOPBACK).context("loopback init (fallback)")?;
            log(&format!(
                "[snapple] audio loopback (fallback): {}Hz {}ch float={} device={loopback_device_name}",
                lb.sample_rate, lb.channels, lb.is_float
            ));
            lb
        }
    };

    let mic = if mic_device != "none" {
        match open_wasapi(eCapture, 0) {
            Ok(m) => {
                log(&format!(
                    "[snapple] audio mic: {}Hz {}ch float={}",
                    m.sample_rate, m.channels, m.is_float
                ));
                Some(m)
            }
            Err(e) => {
                log(&format!("[snapple] mic unavailable: {e:#}"));
                None
            }
        }
    } else {
        None
    };

    // Poll for ffmpeg to connect, checking `running` so we can exit if
    // ffmpeg fails to spawn or crashes before opening the pipe.
    loop {
        if !running.load(Ordering::Relaxed) {
            log("[snapple] audio: shutdown before ffmpeg connected");
            unsafe { let _ = CloseHandle(pipe); }
            return Ok(());
        }
        match unsafe { ConnectNamedPipe(pipe, None) } {
            Ok(()) => break,
            Err(e) => {
                // ERROR_PIPE_CONNECTED (0x80070217) — client already connected.
                if e.code().0 as u32 == 0x80070217 {
                    break;
                }
                // With PIPE_NOWAIT, ERROR_PIPE_LISTENING (0x80070224) means
                // no client yet — keep polling.
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    log("[snapple] audio pipe connected to ffmpeg");

    // Switch the pipe to blocking mode for reliable writes.
    let wait_mode = PIPE_WAIT;
    unsafe { let _ = SetNamedPipeHandleState(pipe, Some(&wait_mode), None, None); }

    // Wrap the raw HANDLE in a File for convenient writing.
    // Ownership transfers here — File::drop will call CloseHandle.
    let mut pipe_file =
        unsafe { std::fs::File::from_raw_handle(pipe.0 as std::os::windows::io::RawHandle) };

    // Start WASAPI capture.
    unsafe {
        loopback.client.Start()?;
        if let Some(ref m) = mic {
            m.client.Start()?;
        }
    }
    ready.store(true, Ordering::Release);

    // Pre-allocated reusable buffers — avoids per-tick heap allocation.
    let mut lb_raw = Vec::<f32>::new();
    let mut lb_stereo_buf = Vec::<f32>::new();
    let mut lb_resampled_buf = Vec::<f32>::new();
    let mut mic_raw = Vec::<f32>::new();
    let mut mic_stereo_buf = Vec::<f32>::new();
    let mut mic_resampled_buf = Vec::<f32>::new();
    let mut write_buf = Vec::<u8>::new();

    // Audio samples waiting to be sent — buffered here so we can
    // throttle output to match the video frame clock.
    let mut pending = Vec::<f32>::new();
    // Total interleaved f32 values written to the pipe so far.
    let mut values_written: u64 = 0;
    let max_pending_values = target_rate as usize * 2 * MAX_PENDING_AUDIO_MS / 1000;

    // Diagnostic: track peak audio level and report periodically.
    let mut diag_peak: f32 = 0.0;
    let mut diag_lb_samples: u64 = 0;
    let mut diag_last_report = Instant::now();

    while running.load(Ordering::Relaxed) {
        // --- loopback ---
        drain_samples_into(&loopback, &mut lb_raw);
        let lb_stereo: &[f32] = if loopback.channels == 2 {
            &lb_raw
        } else {
            to_stereo_into(&lb_raw, loopback.channels, &mut lb_stereo_buf);
            &lb_stereo_buf
        };
        let lb: &[f32] = if loopback.sample_rate != target_rate {
            resample_stereo_into(lb_stereo, loopback.sample_rate, target_rate, &mut lb_resampled_buf);
            &lb_resampled_buf
        } else {
            lb_stereo
        };

        // Diagnostic: track loopback audio levels.
        diag_lb_samples += lb.len() as u64;
        for &s in lb.iter() {
            let a = s.abs();
            if a > diag_peak {
                diag_peak = a;
            }
        }
        if diag_last_report.elapsed() >= Duration::from_secs(10) {
            let peak_db = if diag_peak > 0.0 {
                20.0 * diag_peak.log10()
            } else {
                -120.0
            };
            log(&format!(
                "[snapple] audio diag: loopback peak={peak_db:.1}dB samples={diag_lb_samples}"
            ));
            diag_peak = 0.0;
            diag_lb_samples = 0;
            diag_last_report = Instant::now();
        }

        // --- microphone ---
        let mic_data: &[f32] = if let Some(ref m) = mic {
            drain_samples_into(m, &mut mic_raw);
            let stereo: &[f32] = if m.channels == 2 {
                &mic_raw
            } else {
                to_stereo_into(&mic_raw, m.channels, &mut mic_stereo_buf);
                &mic_stereo_buf
            };
            if m.sample_rate != target_rate {
                resample_stereo_into(stereo, m.sample_rate, target_rate, &mut mic_resampled_buf);
                &mic_resampled_buf
            } else {
                stereo
            }
        } else {
            &[]
        };

        // --- mix into pending buffer ---
        // Loopback is the master timeline — its length determines how many
        // samples are appended.  Mic samples beyond the loopback length are
        // discarded; splicing them in would insert discontinuities into the
        // game audio waveform, causing a gritty buzz.
        //
        // Game audio is attenuated to leave headroom for the mic, then the
        // sum is run through soft_clip so loud peaks compress smoothly
        // instead of producing the harsh distortion of a hard clamp.
        let len = lb.len();
        if len > 0 {
            let base = pending.len();
            pending.resize(base + len, 0.0);

            let overlap = len.min(mic_data.len());
            for i in 0..overlap {
                let mixed = lb[i] * LOOPBACK_MIX_GAIN + mic_data[i] * mic_volume;
                pending[base + i] = soft_clip(mixed);
            }
            for i in overlap..len {
                pending[base + i] = soft_clip(lb[i] * LOOPBACK_MIX_GAIN);
            }
        }

        if pending.len() > max_pending_values {
            let stale = pending.len() - max_pending_values;
            let drop_values = stale & !1; // keep stereo frame alignment
            if drop_values > 0 {
                pending.drain(..drop_values);
            }
        }

        // --- pace output to video clock ---
        // Each video frame corresponds to (sample_rate / fps) stereo
        // frames = (sample_rate / fps * 2) f32 values.  Only write up
        // to the amount the video timeline has consumed so far.
        let vf = video_frames.load(Ordering::Relaxed);
        let target_samples = vf * target_rate as u64 / video_fps;
        let target_values = target_samples * 2; // stereo
        let allowed = target_values.saturating_sub(values_written) as usize;
        let to_write = allowed.min(pending.len());

        if to_write > 0 {
            write_buf.resize(to_write * 4, 0);
            for (i, &s) in pending[..to_write].iter().enumerate() {
                write_buf[i * 4..i * 4 + 4].copy_from_slice(&s.to_le_bytes());
            }

            if pipe_file.write_all(&write_buf[..to_write * 4]).is_err() {
                log("[snapple] audio pipe broken");
                break;
            }
            values_written += to_write as u64;
            pending.drain(..to_write);
        }

        thread::sleep(Duration::from_millis(5));
    }

    unsafe {
        let _ = loopback.client.Stop();
        if let Some(ref m) = mic {
            let _ = m.client.Stop();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{open_process_loopback, resample_stereo_into, soft_clip, to_stereo_into, MAX_PENDING_AUDIO_MS};

    // -----------------------------------------------------------------------
    // Process loopback failure handling: an obviously-invalid PID must
    // return Err rather than panic, so the audio thread's fallback path
    // engages cleanly.
    // -----------------------------------------------------------------------

    #[test]
    fn process_loopback_invalid_pid_returns_err() {
        // PID 0 is the System Idle Process — activation should never
        // succeed against it, but it also must not panic.  On Windows
        // builds older than 10.0.20348 the call fails earlier with
        // "API not available" — also fine, also Err.
        let result = open_process_loopback(0);
        assert!(
            result.is_err(),
            "open_process_loopback(0) unexpectedly succeeded"
        );
    }

    // -----------------------------------------------------------------------
    // soft_clip: guards against accidental regression to a hard clamp, which
    // produced the audible square-wave distortion that motivated the fix.
    // -----------------------------------------------------------------------

    #[test]
    fn soft_clip_is_near_identity_for_small_inputs() {
        // For quiet signals the limiter should be effectively transparent.
        for &x in &[-0.1f32, -0.01, 0.0, 0.01, 0.1] {
            let y = soft_clip(x);
            assert!((y - x).abs() < 0.02, "soft_clip({x}) = {y}, expected ~{x}");
        }
    }

    #[test]
    fn soft_clip_is_strictly_bounded_below_unity() {
        // No finite input should ever escape (-1, 1) — that's the whole
        // point of replacing the hard clamp.
        for &x in &[-1000.0f32, -10.0, -1.0, 1.0, 10.0, 1000.0] {
            let y = soft_clip(x);
            assert!(y.abs() < 1.0, "soft_clip({x}) = {y}, must be < 1.0 in magnitude");
        }
    }

    #[test]
    fn soft_clip_is_monotonic() {
        // A limiter that isn't monotonic would scramble waveform shape.
        let mut prev = soft_clip(-5.0);
        let mut x = -5.0f32;
        while x < 5.0 {
            x += 0.05;
            let y = soft_clip(x);
            assert!(y >= prev, "soft_clip not monotonic at x={x}: {prev} -> {y}");
            prev = y;
        }
    }

    fn allowed_values(video_frames: u64, sample_rate: u32, fps: u64, values_written: u64) -> u64 {
        let target_samples = video_frames * sample_rate as u64 / fps;
        let target_values = target_samples * 2;
        target_values.saturating_sub(values_written)
    }

    /// Simulate the pending-buffer drain logic from the audio thread.
    /// Returns (values_written, pending_len) after processing.
    fn simulate_drain(
        pending: &mut Vec<f32>,
        video_frames: u64,
        sample_rate: u32,
        fps: u64,
        values_written: &mut u64,
    ) -> usize {
        let max_pending_values = sample_rate as usize * 2 * MAX_PENDING_AUDIO_MS / 1000;
        if pending.len() > max_pending_values {
            let stale = pending.len() - max_pending_values;
            let drop_values = stale & !1;
            if drop_values > 0 {
                pending.drain(..drop_values);
            }
        }
        let target_samples = video_frames * sample_rate as u64 / fps;
        let target_values = target_samples * 2;
        let allowed = target_values.saturating_sub(*values_written) as usize;
        let to_write = allowed.min(pending.len());
        *values_written += to_write as u64;
        pending.drain(..to_write);
        to_write
    }

    // -----------------------------------------------------------------------
    // Pacing arithmetic
    // -----------------------------------------------------------------------

    #[test]
    fn pacing_tracks_interleaved_values_consistently() {
        assert_eq!(allowed_values(1, 48_000, 60, 0), 1_600);
        assert_eq!(allowed_values(1, 48_000, 60, 1_600), 0);
        assert_eq!(allowed_values(2, 48_000, 60, 1_600), 1_600);
    }

    #[test]
    fn pacing_exact_ratio_at_common_rates() {
        // 48 kHz / 60 fps = exactly 800 samples per frame, no rounding error.
        for vf in 0..120 {
            let expected = vf * 800 * 2;
            assert_eq!(allowed_values(vf, 48_000, 60, 0), expected);
        }
        // 44.1 kHz / 30 fps = 1470 samples per frame (exact).
        assert_eq!(allowed_values(1, 44_100, 30, 0), 2_940);
    }

    // -----------------------------------------------------------------------
    // Pending-buffer cap — the core audio-delay prevention
    // -----------------------------------------------------------------------

    #[test]
    fn pending_cap_prevents_audible_delay() {
        // The cap MUST stay ≤ 200 ms to keep audio delay imperceptible.
        assert!(
            MAX_PENDING_AUDIO_MS <= 200,
            "MAX_PENDING_AUDIO_MS is {MAX_PENDING_AUDIO_MS} — must be ≤ 200 to prevent audible delay"
        );
    }

    #[test]
    fn pending_cap_value_at_48khz_stereo() {
        let max_pending_values = 48_000usize * 2 * MAX_PENDING_AUDIO_MS / 1000;
        // 150 ms at 48 kHz stereo = 14,400 interleaved f32 values.
        assert_eq!(max_pending_values, 14_400);
    }

    #[test]
    fn pending_drain_discards_oldest_when_over_cap() {
        // Simulate 500 ms of audio sitting in pending (well over the 150 ms cap).
        let sample_rate: u32 = 48_000;
        let half_sec_values = sample_rate as usize * 2 * 500 / 1000; // 48,000 values
        let mut pending: Vec<f32> = (0..half_sec_values).map(|i| i as f32).collect();
        let mut written: u64 = 0;

        // Allow 1 frame of video (800 samples = 1,600 values).
        simulate_drain(&mut pending, 1, sample_rate, 60, &mut written);

        let cap_values = sample_rate as usize * 2 * MAX_PENDING_AUDIO_MS / 1000;
        // Pending should be at most the cap minus what was just written.
        assert!(
            pending.len() <= cap_values,
            "pending {} exceeds cap {cap_values}",
            pending.len()
        );
        // The OLDEST samples (low values) should have been discarded.
        if !pending.is_empty() {
            assert!(
                pending[0] > 0.0,
                "oldest sample should have been drained, got {}",
                pending[0]
            );
        }
    }

    #[test]
    fn steady_state_60fps_no_drift() {
        // Simulate 10 seconds of perfectly steady 60 fps capture.
        // Pending should never grow beyond a few ms of audio.
        let sample_rate: u32 = 48_000;
        let fps: u64 = 60;
        let values_per_tick = (sample_rate as usize * 2 * 5) / 1000; // 5 ms of audio per tick
        let mut pending = Vec::<f32>::new();
        let mut written: u64 = 0;
        let mut vf: u64 = 0;
        let mut max_pending = 0usize;

        // 2000 ticks × 5 ms = 10 seconds
        for tick in 0..2_000u64 {
            // Audio arrives every tick.
            pending.extend(std::iter::repeat_n(0.0f32, values_per_tick));

            // Video frame every ~3.33 ticks (60 fps ÷ 200 Hz tick).
            // Advance video_frames to match wall-clock time.
            let expected_vf = (tick + 1) * fps / 200;
            vf = vf.max(expected_vf);

            simulate_drain(&mut pending, vf, sample_rate, fps, &mut written);
            max_pending = max_pending.max(pending.len());
        }

        // In steady state, pending should be tiny (well under 50 ms).
        let max_pending_ms = max_pending * 1000 / (sample_rate as usize * 2);
        assert!(
            max_pending_ms < 50,
            "pending peaked at {max_pending_ms} ms in steady state — expected < 50 ms"
        );
    }

    #[test]
    fn video_stall_delay_bounded_by_cap() {
        // Simulate a 1-second video stall: audio keeps arriving but
        // video_frames freezes.  Pending must stay ≤ cap.
        let sample_rate: u32 = 48_000;
        let fps: u64 = 60;
        let values_per_tick = (sample_rate as usize * 2 * 5) / 1000;
        let mut pending = Vec::<f32>::new();
        let mut written: u64 = 0;
        let cap_values = sample_rate as usize * 2 * MAX_PENDING_AUDIO_MS / 1000;

        // 1 second of normal operation.
        let mut vf: u64 = 0;
        for tick in 0..200u64 {
            pending.extend(std::iter::repeat_n(0.0f32, values_per_tick));
            vf = (tick + 1) * fps / 200;
            simulate_drain(&mut pending, vf, sample_rate, fps, &mut written);
        }

        // 1-second stall: audio arrives but video_frames frozen.
        let frozen_vf = vf;
        for _ in 0..200u64 {
            pending.extend(std::iter::repeat_n(0.0f32, values_per_tick));
            simulate_drain(&mut pending, frozen_vf, sample_rate, fps, &mut written);
        }

        assert!(
            pending.len() <= cap_values,
            "pending {} exceeds cap {cap_values} during video stall",
            pending.len()
        );

        // After stall, the maximum delay in the buffer is bounded.
        let delay_ms = pending.len() * 1000 / (sample_rate as usize * 2);
        assert!(
            delay_ms <= MAX_PENDING_AUDIO_MS,
            "delay {delay_ms} ms exceeds MAX_PENDING_AUDIO_MS {MAX_PENDING_AUDIO_MS}"
        );
    }

    // -----------------------------------------------------------------------
    // Format conversion helpers
    // -----------------------------------------------------------------------

    #[test]
    fn mono_to_stereo_duplicates_channels() {
        let mono = vec![1.0f32, 2.0, 3.0];
        let mut out = Vec::new();
        to_stereo_into(&mono, 1, &mut out);
        assert_eq!(out, vec![1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
    }

    #[test]
    fn multichannel_to_stereo_keeps_first_two() {
        // 4-channel: [L R C LFE] per frame
        let quad = vec![1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut out = Vec::new();
        to_stereo_into(&quad, 4, &mut out);
        assert_eq!(out, vec![1.0, 2.0, 5.0, 6.0]);
    }

    #[test]
    fn resample_identity_when_rates_close() {
        // Resample 48000→48000 should be near-identity (caller should skip,
        // but verify the function doesn't corrupt data if called anyway).
        let input: Vec<f32> = (0..200).map(|i| (i as f32) / 200.0).collect();
        let mut out = Vec::new();
        resample_stereo_into(&input, 48_000, 48_000, &mut out);
        assert_eq!(out.len(), input.len());
        for (a, b) in out.iter().zip(input.iter()) {
            assert!((a - b).abs() < 1e-5, "sample mismatch: {a} vs {b}");
        }
    }

    #[test]
    fn resample_preserves_stereo_frame_count() {
        // 44100→48000: output should have more frames.
        let in_frames = 441; // 10 ms at 44.1 kHz
        let input: Vec<f32> = vec![0.5; in_frames * 2];
        let mut out = Vec::new();
        resample_stereo_into(&input, 44_100, 48_000, &mut out);
        let out_frames = out.len() / 2;
        let expected = (in_frames as u64 * 48_000 / 44_100) as usize; // 480
        assert_eq!(out_frames, expected);
    }
}
