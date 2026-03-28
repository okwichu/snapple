use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use std::{fs, io};

use anyhow::{bail, Context, Result};

use crate::{log, CREATE_NO_WINDOW};

/// Global save lock to prevent concurrent saves on rapid F8 presses.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// List .mp4 segments in a directory, sorted by modification time (oldest first).
fn list_segments(seg_dir: &Path) -> io::Result<Vec<(SystemTime, PathBuf)>> {
    let mut segments = Vec::new();
    for entry in fs::read_dir(seg_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mp4") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                segments.push((modified, path));
            }
        }
    }
    segments.sort_by_key(|(t, _)| *t);
    Ok(segments)
}

/// Delete segments older than `max_age_secs`.
pub fn cleanup_old_segments(seg_dir: &Path, max_age_secs: u64) -> io::Result<()> {
    let now = SystemTime::now();
    for (modified, path) in list_segments(seg_dir)? {
        if let Ok(age) = now.duration_since(modified) {
            if age > Duration::from_secs(max_age_secs) {
                let _ = fs::remove_file(&path);
            }
        }
    }
    Ok(())
}

/// Concatenate recent segments into a single clip.
pub fn save_clip(
    seg_dir: &Path,
    game_name: &str,
    ffmpeg_path: &Path,
    clips_dir: &Path,
    max_segments: usize,
) -> Result<PathBuf> {
    let _lock = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let mut segments = list_segments(seg_dir).context("Cannot read segment dir")?;

    // Skip the newest segment (might still be written by ffmpeg)
    if segments.len() > 1 {
        segments.pop();
    }

    // Take the last N segments
    let start = segments.len().saturating_sub(max_segments);
    let recent = &segments[start..];

    if recent.is_empty() {
        bail!("No segments available to save");
    }

    // Write ffmpeg concat list
    let concat_file = seg_dir.join("concat.txt");
    let concat_content: String = recent
        .iter()
        .map(|(_, p)| format!("file '{}'", p.to_string_lossy().replace('\\', "/")))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&concat_file, &concat_content)?;

    // Output filename
    let safe_name = sanitize_filename(game_name);
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let output = clips_dir.join(format!("{safe_name}_{timestamp}.mp4"));

    // Re-encode audio to eliminate AAC priming artifacts at segment
    // boundaries; video is still stream-copied (fast).
    let status = Command::new(ffmpeg_path)
        .args([
            "-y",
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            &concat_file.to_string_lossy(),
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            &output.to_string_lossy(),
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("Failed to run ffmpeg concat")?;

    let _ = fs::remove_file(&concat_file);

    if !status.success() {
        bail!("ffmpeg concat exited with {status}");
    }

    log(&format!("[snapple] saved clip: {}", output.display()));
    Ok(output)
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .collect::<String>()
        .trim()
        .replace(' ', "_")
}

#[cfg(test)]
mod tests {
    use super::{cleanup_old_segments, list_segments, sanitize_filename};
    use std::fs;
    use std::time::Duration;
    use tempfile::tempdir;

    #[test]
    fn segment_listing_is_sorted_oldest_first() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.mp4");
        let b = dir.path().join("b.mp4");
        fs::write(&a, b"a").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&b, b"b").unwrap();

        let segments = list_segments(dir.path()).unwrap();
        let names: Vec<_> = segments
            .iter()
            .map(|(_, path)| path.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["a.mp4", "b.mp4"]);
    }

    #[test]
    fn cleanup_with_zero_age_removes_all_existing_segments() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.mp4"), b"a").unwrap();
        std::thread::sleep(Duration::from_millis(5));

        cleanup_old_segments(dir.path(), 0).unwrap();

        let segments = list_segments(dir.path()).unwrap();
        assert!(segments.is_empty());
    }

    #[test]
    fn sanitize_filename_keeps_safe_characters_and_normalizes_spaces() {
        assert_eq!(sanitize_filename("Halo Infinite"), "Halo_Infinite");
        assert_eq!(sanitize_filename("Risk: Rain/2?"), "Risk_Rain2");
    }
}
