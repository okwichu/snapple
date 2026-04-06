# Snapple

Lightweight Windows game clip capture utility. Snapple runs in the system tray, automatically detects Steam games, and continuously records your screen. Press a hotkey to save the last 60 seconds as a clip.

## Features

- **Automatic game detection** — monitors Steam for running games, starts/stops recording automatically
- **GPU-accelerated encoding** — auto-detects NVIDIA NVENC, AMD AMF, or falls back to CPU (libx264)
- **Game audio capture** — detects the game's audio device via WASAPI session enumeration, even when the game uses a non-default output
- **Microphone mixing** — optionally records mic audio alongside game audio
- **Hotkey clip saving** — press F8 to save the last 60 seconds
- **Direct window capture** — uses Windows.Graphics.Capture for low-overhead, borderless-compatible capture

## Requirements

- Windows 10 (1903+) or Windows 11
- [FFmpeg](https://ffmpeg.org/) — install with `winget install Gyan.FFmpeg`, or place `ffmpeg.exe` next to `Snapple.exe`
- A GPU with hardware encoding support (NVIDIA or AMD) is recommended but not required

## Usage

1. Run `Snapple.exe`. It appears in the system tray.
2. Launch a Steam game. Snapple automatically starts recording.
3. Press **F8** (default) to save the last 60 seconds as a clip.
4. Clips are saved to `D:\Clips` by default.

## Configuration

Place a `snapple.toml` file next to `Snapple.exe` to customize behavior. All fields are optional — defaults are used for anything not specified.

### Example `snapple.toml`

```toml
# Where clips are saved.
clips_dir = "D:/Clips"

# Hotkey to save a clip.
# Supported: F1-F12, PrintScreen, ScrollLock, Pause, Insert, Home, End, Delete
hotkey = "F8"

[capture]
# Frames per second for screen capture.
fps = 60

# FFmpeg video filter for scaling. "-2" preserves aspect ratio.
# Set to a higher value like 1080 for higher quality (larger files).
scale = "scale=-2:720"

# Video encoder. Change this based on your GPU:
#   NVIDIA:  "h264_nvenc"
#   AMD:     "h264_amf"
#   CPU:     "libx264"  (slow, but works everywhere)
# If the configured encoder isn't available, Snapple auto-detects a working one.
encoder = "h264_nvenc"

# Encoder preset. Controls speed vs quality tradeoff.
#   NVENC:   "p1" (fastest) to "p7" (best quality)
#   libx264: "ultrafast", "fast", "medium", "slow", "veryslow"
#   AMF:     "speed", "balanced", "quality"
preset = "p4"

# Rate control mode.
#   NVENC:   "constqp" or "vbr"
#   libx264: "crf" (use quality field for CRF value, e.g. "20")
#   AMF:     "cqp"
rate_control = "constqp"

# Quality parameter (lower = better quality, larger files).
#   NVENC constqp: "16"-"28" typical (QP value)
#   libx264 crf:   "18"-"23" typical (CRF value)
#   AMF cqp:       "16"-"28" typical (QP value)
quality = "20"

# Duration of each recording segment in seconds.
# Shorter segments = more responsive clip saving, slightly more overhead.
segment_time = 5

# Which monitor to capture.
#   "auto" — automatically captures the monitor the game window is on (default)
#   "1", "2", etc. — capture a specific monitor by index
monitor = "auto"

# Microphone device: "default" for system default, "none" to disable.
microphone = "default"

[buffer]
# Number of segments to include when saving a clip.
# Total clip duration = segments * segment_time (default: 12 * 5 = 60 seconds).
segments = 12

[steam]
# Steam installation directory. Auto-detected from the Windows registry by default.
# Uncomment to override:
# path = "C:/Program Files (x86)/Steam"

# Steam App IDs to ignore (e.g. redistributables).
skip_appids = ["228980"]

# How often (in seconds) to check for running Steam games.
poll_interval_secs = 5
```

### Encoder quick-start

| GPU | encoder | preset | rate_control | quality |
|-----|---------|--------|-------------|---------|
| NVIDIA | `h264_nvenc` | `p4` | `constqp` | `20` |
| AMD | `h264_amf` | `balanced` | `cqp` | `20` |
| CPU (any) | `libx264` | `fast` | `crf` | `20` |

## Changelog

### 1.0.1

- Fixed an edge case where capture silently stops after system sleep/resume or GPU disconnect (e.g. unplugging a laptop from an external GPU). Snapple now detects D3D device loss and automatically restarts the capture pipeline.
- Shutter sound now only plays when a clip is actually saved.

## Building from source

```
cargo build --release
```

The binary is at `target/release/Snapple.exe`.

### Building an MSI installer

Requires [WiX Toolset v4](https://wixtoolset.org/) and `cargo-wix`:

```
cargo install cargo-wix
cargo wix
```

## License

MIT
