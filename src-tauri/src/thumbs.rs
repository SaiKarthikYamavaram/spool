//! Poster frames for downloaded video.
//!
//! yt-dlp hands back a thumbnail URL for the sites it knows, but that covers
//! only downloads that went through it. A plain HTTP download of an .mp4, or a
//! stream pulled from a manifest, has no metadata to ask — so the frame is
//! taken from the file itself once it is on disk.
//!
//! ffmpeg is optional. Everything here degrades to "no thumbnail" rather than
//! failing a download, and a file that yields nothing is remembered so the
//! probe is not repeated on every render.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

/// Extensions worth pointing ffmpeg at. Audio is deliberately absent: cover art
/// is not a frame, and a file without it wastes a process launch per row.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mkv", "webm", "mov", "avi", "flv", "m4v", "mpg", "mpeg", "wmv", "ts", "m2ts", "ogv",
];

pub fn is_video_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => VIDEO_EXTS.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// Where a download's poster frame is cached. Keyed by the download id, which
/// is stable for the life of the entry and safe as a filename.
pub fn cache_path(cache_dir: &Path, id: &str) -> PathBuf {
    cache_dir.join(format!("{id}.jpg"))
}

/// Extract a poster frame, or return `None` if that is not possible.
///
/// Seeks to 20% rather than a fixed offset: a title card or a black lead-in is
/// common at the start, and a percentage adapts to a 30-second clip as well as
/// a feature. `-ss` before `-i` keeps the seek fast (it skips rather than
/// decodes), which matters when this runs once per completed video.
pub async fn extract(ffmpeg: &str, video: &Path, out: &Path, duration_secs: Option<f64>) -> Option<PathBuf> {
    if let Some(parent) = out.parent() {
        tokio::fs::create_dir_all(parent).await.ok()?;
    }

    let seek = duration_secs
        .filter(|d| d.is_finite() && *d > 1.0)
        .map(|d| d * 0.2)
        .unwrap_or(1.0);

    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let status = cmd
        .arg("-nostdin")
        .arg("-loglevel").arg("error")
        .arg("-y")
        .arg("-ss").arg(format!("{seek:.2}"))
        .arg("-i").arg(video)
        .arg("-frames:v").arg("1")
        // 320px wide, height to match the source's aspect ratio (-2 keeps it
        // even, which some encoders require).
        .arg("-vf").arg("scale=320:-2")
        .arg("-q:v").arg("6")
        .arg(out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .ok()?;

    // ffmpeg reports success even when a seek past the end wrote nothing, so
    // trust the file rather than the exit code.
    if status.success() && tokio::fs::metadata(out).await.map(|m| m.len() > 0).unwrap_or(false) {
        Some(out.to_path_buf())
    } else {
        let _ = tokio::fs::remove_file(out).await;
        None
    }
}

/// Read a video's duration, so the seek can be proportional. `None` when
/// ffprobe is missing or the file has no duration (a stream, a broken mux).
pub async fn duration(ffprobe: &str, video: &Path) -> Option<f64> {
    let mut cmd = Command::new(ffprobe);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let out = cmd
        .arg("-v").arg("error")
        .arg("-show_entries").arg("format=duration")
        .arg("-of").arg("default=noprint_wrappers=1:nokey=1")
        .arg(video)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;

    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_video_by_extension() {
        assert!(is_video_file(Path::new("/d/clip.mp4")));
        assert!(is_video_file(Path::new("/d/clip.MKV")), "extension match is case-insensitive");
        assert!(is_video_file(Path::new("/d/Big Buck Bunny [id].webm")));

        // Audio has cover art, not frames; pointing ffmpeg at it wastes a
        // process per row for something that is not a poster.
        assert!(!is_video_file(Path::new("/d/song.mp3")));
        assert!(!is_video_file(Path::new("/d/album.flac")));
        assert!(!is_video_file(Path::new("/d/archive.zip")));
        assert!(!is_video_file(Path::new("/d/README")));
        // A dot in a directory name must not be read as the file's extension.
        assert!(!is_video_file(Path::new("/d/v1.2/README")));
    }

    #[test]
    fn cache_path_is_keyed_by_id() {
        let p = cache_path(Path::new("/cache"), "7");
        assert_eq!(p, PathBuf::from("/cache/7.jpg"));
    }
}
