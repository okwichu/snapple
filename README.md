# Snapple

Lightweight Windows game clip capture utility. Snapple runs in the system tray, automatically detects Steam games, and continuously records your screen. Press a hotkey to save the last 60 seconds as a clip.

## What's new in v0.2

- **Game audio capture fixed** — Snapple now detects which audio device the game is actually using (via WASAPI session enumeration) and captures from that device. Previously, loopback capture silently recorded from the wrong endpoint when the game used a non-default audio device.
- Microphone mixing still supported alongside game audio.

## Requirements

- Windows 10 (1903+) or Windows 11
- [FFmpeg](https://ffmpeg.org/) — install with `winget install Gyan.FFmpeg`, or place `ffmpeg.exe` next to `Snapple.exe`
- A GPU with hardware encoding support (NVIDIA, AMD, or software fallback — see [encoder config](#capture))

## Usage

1. Run `Snapple.exe`. It appears in the system tray.
2. Launch a Steam game. Snapple automatically starts recording.
3. Press **F8** (default) to save the last 60 seconds as a clip.
4. Clips are saved to `~/Videos/Snapple` by default.

## Configuration

Place a `snapple.toml` file next to `Snapple.exe` to customize behavior. All fields are optional — defaults are used for anything not specified.

### Example `snapple.toml`

```toml
# Where clips are saved. Default: your Videos folder under "Snapple".
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
encoder = "h264_nvenc"

# Encoder preset. Controls speed vs quality tradeoff.
#   NVENC:   "p1" (fastest) to "p7" (best quality)
#   libx264: "ultrafast", "fast", "medium", "slow", "veryslow"
#   AMF:     "speed", "balanced", "quality"
preset = "p4"

# Rate control mode.
#   NVENC:   "constqp" or "vbr"
#   libx264: "crf" (use quality field for CRF value, e.g. "23")
#   AMF:     "cqp"
rate_control = "constqp"

# Quality parameter (lower = better quality, larger files).
#   NVENC constqp: "20"-"35" typical (QP value)
#   libx264 crf:   "18"-"28" typical (CRF value)
quality = "28"

# Duration of each recording segment in seconds.
# Shorter segments = more responsive clip saving, slightly more overhead.
segment_time = 5

# Which monitor to capture.
#   "auto" — automatically captures the monitor the game window is on (default)
#   "1", "2", etc. — capture a specific monitor by index
monitor = "auto"

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
| NVIDIA | `h264_nvenc` | `p4` | `constqp` | `28` |
| AMD | `h264_amf` | `balanced` | `cqp` | `28` |
| CPU (any) | `libx264` | `fast` | `crf` | `23` |

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
