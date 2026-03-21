// WASAPI audio capture: system loopback (game audio) + microphone, mixed to a named pipe.

use std::io::Write;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Pipes::*;
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;

use crate::log;

// WASAPI stream flag for loopback capture.
const STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;

// WAVEFORMATEX tag values.
const WAVE_FORMAT_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

// AUDCLNT_BUFFERFLAGS
const BUFFERFLAGS_SILENT: u32 = 0x0000_0002;

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
    thread: Option<JoinHandle<()>>,
}

impl AudioPipe {
    /// Create the named pipe and spawn the audio capture thread.
    /// Call **before** spawning ffmpeg so the pipe path exists when ffmpeg opens it.
    pub fn start(mic_device: &str, running: Arc<AtomicBool>) -> Result<Self> {
        // Probe loopback sample rate on this thread so we can return it to the caller.
        let sample_rate = detect_loopback_sample_rate()?;

        let pipe_path = format!(r"\\.\pipe\snapple_audio_{}", std::process::id());
        let pipe_wide: Vec<u16> = pipe_path.encode_utf16().chain(std::iter::once(0)).collect();

        let pipe_handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(pipe_wide.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(2), // PIPE_ACCESS_OUTBOUND
                PIPE_TYPE_BYTE | PIPE_WAIT,
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

        let thread = thread::Builder::new()
            .name("audio".into())
            .spawn(move || {
                if let Err(e) = audio_thread(handle, &mic, sample_rate, &running) {
                    log(&format!("[snapple] audio error: {e:#}"));
                }
            })
            .context("Failed to spawn audio thread")?;

        Ok(Self {
            pipe_path,
            sample_rate,
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
            10_000_000, // 1-second buffer in 100 ns units
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

    // Block until ffmpeg connects to the named pipe.
    let _ = unsafe { ConnectNamedPipe(pipe, None) };
    log("[snapple] audio pipe connected to ffmpeg");

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

    // Pre-allocated reusable buffers — avoids per-tick heap allocation.
    let mut lb_raw = Vec::<f32>::new();
    let mut lb_stereo_buf = Vec::<f32>::new();
    let mut mic_raw = Vec::<f32>::new();
    let mut mic_stereo_buf = Vec::<f32>::new();
    let mut mic_resampled_buf = Vec::<f32>::new();
    let mut write_buf = Vec::<u8>::new();

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

        // --- mix & write ---
        let len = lb.len().max(mic_data.len());
        if len > 0 {
            write_buf.resize(len * 4, 0);

            // Overlapping region: mix both sources.
            let overlap = lb.len().min(mic_data.len());
            for i in 0..overlap {
                let mixed = (lb[i] + mic_data[i]).clamp(-1.0, 1.0);
                write_buf[i * 4..i * 4 + 4].copy_from_slice(&mixed.to_le_bytes());
            }

            // Tail: only one source has data — clamp but no addition.
            let tail: &[f32] = if lb.len() > mic_data.len() {
                &lb[overlap..]
            } else {
                &mic_data[overlap..]
            };
            for (j, &s) in tail.iter().enumerate() {
                let i = overlap + j;
                let clamped = s.clamp(-1.0, 1.0);
                write_buf[i * 4..i * 4 + 4].copy_from_slice(&clamped.to_le_bytes());
            }

            if pipe_file.write_all(&write_buf[..len * 4]).is_err() {
                log("[snapple] audio pipe broken");
                break;
            }
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
