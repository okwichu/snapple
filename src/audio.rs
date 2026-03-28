// WASAPI audio capture: system loopback (game audio) + microphone, mixed to a named pipe.

use std::io::Write;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Pipes::*;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;

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
// Safety cap on the pending buffer.  2 seconds is generous enough to absorb
// startup transients and encoding spikes without discarding usable audio.
const MAX_PENDING_AUDIO_MS: usize = 2000;

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

/// Open a WASAPI capture source.
///
/// * `data_flow` — `eRender` for loopback (game audio), `eCapture` for microphone.
/// * `stream_flags` — pass `STREAMFLAGS_LOOPBACK` for loopback, `0` for mic.
fn open_wasapi(data_flow: EDataFlow, stream_flags: u32) -> Result<WasapiSource> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(data_flow, eConsole)?;
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

    let loopback = open_wasapi(eRender, STREAMFLAGS_LOOPBACK).context("loopback init")?;
    log(&format!(
        "[snapple] audio loopback: {}Hz {}ch float={}",
        loopback.sample_rate, loopback.channels, loopback.is_float
    ));

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

    while running.load(Ordering::Relaxed) {
        // --- loopback ---
        drain_samples_into(&loopback, &mut lb_raw);
        let lb: &[f32] = if loopback.channels == 2 {
            &lb_raw
        } else {
            to_stereo_into(&lb_raw, loopback.channels, &mut lb_stereo_buf);
            &lb_stereo_buf
        };

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
        let len = lb.len().max(mic_data.len());
        if len > 0 {
            let base = pending.len();
            pending.resize(base + len, 0.0);

            let overlap = lb.len().min(mic_data.len());
            for i in 0..overlap {
                pending[base + i] = (lb[i] + mic_data[i]).clamp(-1.0, 1.0);
            }
            let tail: &[f32] = if lb.len() > mic_data.len() {
                &lb[overlap..]
            } else {
                &mic_data[overlap..]
            };
            for (j, &s) in tail.iter().enumerate() {
                pending[base + overlap + j] = s.clamp(-1.0, 1.0);
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
    use super::MAX_PENDING_AUDIO_MS;

    fn allowed_values(video_frames: u64, sample_rate: u32, fps: u64, values_written: u64) -> u64 {
        let target_samples = video_frames * sample_rate as u64 / fps;
        let target_values = target_samples * 2;
        target_values.saturating_sub(values_written)
    }

    #[test]
    fn pacing_tracks_interleaved_values_consistently() {
        assert_eq!(allowed_values(1, 48_000, 60, 0), 1_600);
        assert_eq!(allowed_values(1, 48_000, 60, 1_600), 0);
        assert_eq!(allowed_values(2, 48_000, 60, 1_600), 1_600);
    }

    #[test]
    fn backlog_cap_allows_generous_buffer() {
        let max_pending_values = 48_000usize * 2 * MAX_PENDING_AUDIO_MS / 1000;
        assert_eq!(max_pending_values, 192_000); // 2 seconds at 48 kHz stereo
    }
}
