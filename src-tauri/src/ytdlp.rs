//! Video backend: delegate streaming sites to `yt-dlp`.
//!
//! spool's own HTTP engine downloads files. Adaptive-streaming sites
//! (YouTube DASH, HLS) are a different problem: separate audio/video streams,
//! signed short-lived segment URLs, a per-session cipher, then a mux. `yt-dlp`
//! solves all of that and supports ~1800 sites, so for those we shell out to it
//! rather than reinventing an extractor. Normal file downloads never touch this
//! module.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Hosts routed to yt-dlp automatically. Not exhaustive — yt-dlp handles far
/// more — but these are the ones worth auto-detecting; anything else can still
/// be forced via the "Download video" action.
const VIDEO_HOSTS: &[&str] = &[
    "youtube.com", "youtu.be", "m.youtube.com", "music.youtube.com",
    "vimeo.com", "dailymotion.com", "twitch.tv", "clips.twitch.tv",
    "tiktok.com", "instagram.com", "facebook.com", "fb.watch",
    "twitter.com", "x.com", "reddit.com", "soundcloud.com",
    "bilibili.com", "nicovideo.jp", "streamable.com",
];

/// HLS/DASH manifests are playlists, not files: fetching one over HTTP saves a
/// few KB of text. They must go through yt-dlp, which pulls the segments and
/// muxes them.
pub fn is_stream_manifest(url: &str) -> bool {
    let parsed = match reqwest::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    // Routing on this hands the URL to yt-dlp as a subprocess argument, so keep
    // it to the same schemes `download::validate_url` allows rather than
    // letting a `file://` path in through the video branch.
    if !matches!(parsed.scheme(), "http" | "https") {
        return false;
    }
    let path = parsed.path().to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".mpd")
}

pub fn is_video_site(url: &str) -> bool {
    let host = match reqwest::Url::parse(url) {
        Ok(u) => u.host_str().unwrap_or("").to_ascii_lowercase(),
        Err(_) => return false,
    };
    VIDEO_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// Where cookies come from for age/region-locked or members-only videos.
#[derive(Debug, Clone, Default)]
pub struct Cookies {
    /// Netscape `cookies.txt` path → `--cookies`.
    pub file: Option<PathBuf>,
    /// Browser name → `--cookies-from-browser` (reads the live jar directly).
    pub browser: Option<String>,
}

/// One progress tick parsed from yt-dlp. Bytes are for the *current* stream
/// (yt-dlp downloads video then audio), so totals reset between phases. Speed
/// is derived on the frontend from the byte deltas, so it is not carried here.
#[derive(Debug, Clone, Copy)]
pub struct Tick {
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// Resolve a video's title and thumbnail URL without downloading, so the row
/// can show the real name and a preview immediately. Best-effort: any failure
/// or a slow site returns `(None, None)` and the download still proceeds.
pub async fn resolve_meta(
    ytdlp: &str,
    url: &str,
    cookies: &Cookies,
    proxy: Option<&str>,
) -> (Option<String>, Option<String>) {
    let mut cmd = Command::new(ytdlp);
    cmd.arg("--skip-download")
        .arg("--no-playlist")
        .arg("--no-warnings")
        .arg("-O")
        .arg("%(title)s|||%(thumbnail)s");
    match (&cookies.file, &cookies.browser) {
        (Some(path), _) => { cmd.arg("--cookies").arg(path); }
        (None, Some(b)) if !b.is_empty() => { cmd.arg("--cookies-from-browser").arg(b); }
        _ => {}
    }
    if let Some(p) = proxy.filter(|p| !p.is_empty()) {
        cmd.arg("--proxy").arg(p);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    cmd.arg("--").arg(url).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);

    let out = match tokio::time::timeout(Duration::from_secs(15), cmd.output()).await {
        Ok(Ok(o)) if o.status.success() => o,
        _ => return (None, None),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let (title, thumb) = line.split_once("|||").unwrap_or((line, ""));

    let clean = |s: &str| {
        let s = s.trim();
        if s.is_empty() || s == "NA" { None } else { Some(s.to_string()) }
    };
    let thumb = clean(thumb).filter(|u| u.starts_with("http"));
    (clean(title), thumb)
}

/// Format-selection args for a quality choice: "best" (default), a max height
/// like "1080", or "audio" (extract to mp3).
///
/// `height<=?H` is non-strict — if nothing matches exactly, yt-dlp still picks
/// the closest rather than failing. Video+audio (`bv*+ba`) is merged by ffmpeg;
/// `/b` is the fallback for progressive-only streams.
fn format_args(quality: &str) -> Vec<String> {
    match quality {
        "" | "best" => vec![],
        "audio" => vec![
            "-f".into(), "bestaudio/best".into(),
            "-x".into(), "--audio-format".into(), "mp3".into(),
        ],
        h => vec![
            "-f".into(),
            format!("bv*[height<=?{h}]+ba/b[height<=?{h}]"),
        ],
    }
}

/// Run yt-dlp for `url`, saving into `dir`. Returns the final file path.
///
/// `on_file` fires as soon as yt-dlp names an output (well before completion),
/// so the UI can replace the "…video" placeholder with the real title.
#[allow(clippy::too_many_arguments)]
pub async fn run<F, G>(
    ytdlp: &str,
    url: &str,
    dir: &Path,
    cookies: &Cookies,
    quality: &str,
    name: Option<&str>,
    limit_kb: Option<u64>,
    proxy: Option<&str>,
    token: CancellationToken,
    on_progress: F,
    on_file: G,
) -> Result<PathBuf, String>
where
    F: Fn(Tick) + Send + 'static,
    G: Fn(&Path) + Send + 'static,
{
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let out_tmpl = out_template(dir, name);

    let mut cmd = Command::new(ytdlp);
    cmd.arg("--newline")
        .arg("--no-playlist")
        .arg("-o")
        .arg(&out_tmpl)
        // Pipe-delimited machine progress on stdout. NA where a field is
        // absent. (Do NOT add --print here: it silently suppresses
        // --progress-template. The final path is parsed from the Destination /
        // Merger lines instead.)
        .arg("--progress-template")
        .arg("download:FDPROG|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s")
        .arg("--no-warnings")
        // HLS/DASH streams arrive as many small fragments; fetched one at a
        // time they are latency-bound, not bandwidth-bound.
        .arg("--concurrent-fragments")
        .arg("8")
        .arg("--retries")
        .arg("10")
        .arg("--fragment-retries")
        .arg("10");

    for a in format_args(quality) {
        cmd.arg(a);
    }

    match (&cookies.file, &cookies.browser) {
        (Some(path), _) => {
            cmd.arg("--cookies").arg(path);
        }
        (None, Some(browser)) if !browser.is_empty() => {
            cmd.arg("--cookies-from-browser").arg(browser);
        }
        _ => {}
    }

    if let Some(rate) = limit_kb.filter(|r| *r > 0) {
        cmd.arg("--limit-rate").arg(format!("{rate}K"));
    }
    if let Some(p) = proxy.filter(|p| !p.is_empty()) {
        cmd.arg("--proxy").arg(p);
    }

    cmd.arg("--").arg(url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    // Run yt-dlp in its own process group so cancellation can signal the whole
    // group — otherwise a spawned ffmpeg (used for merging/remuxing) is
    // reparented to init and keeps running after we kill yt-dlp.
    #[cfg(unix)]
    cmd.process_group(0);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start yt-dlp: {e} (is it installed?)"))?;
    let child_pid = child.id();

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    // Drain stderr concurrently so the pipe never blocks; keep the tail for the
    // error message.
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut tail = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            tail.push(line);
            if tail.len() > 20 {
                tail.remove(0);
            }
        }
        tail.join("\n")
    });

    // The final path is learned from yt-dlp's own messages. A merge or audio
    // extraction produces the true output and overrides the per-stream
    // download destination.
    let mut destination: Option<PathBuf> = None;
    let mut merged: Option<PathBuf> = None;
    let mut reader = BufReader::new(stdout).lines();

    loop {
        tokio::select! {
            _ = token.cancelled() => {
                // Kill the whole process group so a spawned ffmpeg dies too.
                #[cfg(unix)]
                if let Some(pid) = child_pid {
                    unsafe { libc::kill(-(pid as i32), libc::SIGKILL); }
                }
                #[cfg(windows)]
                if let Some(pid) = child_pid {
                    let mut kill_cmd = std::process::Command::new("taskkill");
                    kill_cmd.args(["/F", "/T", "/PID", &pid.to_string()]);
                    {
                        use std::os::windows::process::CommandExt;
                        kill_cmd.creation_flags(0x08000000);
                    }
                    let _ = kill_cmd.spawn();
                }
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err("cancelled".into());
            }
            line = reader.next_line() => {
                match line {
                    Ok(Some(line)) => {
                        if let Some(rest) = line.strip_prefix("FDPROG|") {
                            if let Some(tick) = parse_progress(rest) {
                                on_progress(tick);
                            }
                        } else if let Some(p) = parse_destination(&line) {
                            on_file(&p);
                            destination = Some(p);
                        } else if let Some(p) = parse_final(&line) {
                            on_file(&p);
                            merged = Some(p);
                        }
                    }
                    Ok(None) => break, // EOF
                    Err(e) => return Err(format!("yt-dlp output error: {e}")),
                }
            }
        }
    }
    let final_path = merged.or(destination);

    let status = child.wait().await.map_err(|e| format!("yt-dlp failed: {e}"))?;
    if !status.success() {
        let tail = stderr_task.await.unwrap_or_default();
        let msg = tail.lines().rev().find(|l| l.contains("ERROR")).unwrap_or(&tail);
        return Err(format!("yt-dlp failed: {}", msg.trim()));
    }

    // On success yt-dlp printed the path; fall back to the directory so the
    // "open folder" action still works if the print was somehow missed.
    Ok(final_path.unwrap_or_else(|| dir.to_path_buf()))
}

/// A per-stream download target: `[download] Destination: PATH`, or a
/// `... has already been downloaded` line.
fn parse_destination(line: &str) -> Option<PathBuf> {
    if let Some(p) = line.strip_prefix("[download] Destination: ") {
        return Some(PathBuf::from(p.trim()));
    }
    if let Some(rest) = line.strip_prefix("[download] ") {
        if let Some(p) = rest.strip_suffix(" has already been downloaded") {
            return Some(PathBuf::from(p.trim()));
        }
    }
    None
}

/// The real output after a merge or audio extraction, which supersedes the
/// per-stream destination.
fn parse_final(line: &str) -> Option<PathBuf> {
    // [Merger] Merging formats into "PATH"
    if let Some(i) = line.find("Merging formats into \"") {
        let rest = &line[i + "Merging formats into \"".len()..];
        if let Some(end) = rest.rfind('"') {
            return Some(PathBuf::from(&rest[..end]));
        }
    }
    // Other post-processors that produce/rename the final file.
    for tag in ["[ExtractAudio] ", "[VideoRemuxer] ", "[VideoConvertor] ", "[FixupM3u8] ", "[FixupM4a] "] {
        if let Some(p) = line.strip_prefix(tag) {
            // Some emit `Destination: X`, some `Remuxing ... into "X"`, `to "X"`, `Fixing code of "X"`, `Correcting container in "X"`.
            for needle in ["into \"", "to \"", "of \"", "in \""] {
                if let Some(i) = p.find(needle) {
                    let rest = &p[needle.len() + i..];
                    if let Some(end) = rest.rfind('"') {
                        return Some(PathBuf::from(&rest[..end]));
                    }
                }
            }
            let p = p.trim_start_matches("Destination: ").trim();
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
    }
    None
}

/// Parse `downloaded|total|estimate|speed`, each a number or "NA". Total falls
/// back to the estimate when the exact size is not yet known.
/// yt-dlp's `-o` output template.
///
/// A name from the add dialog fixes the stem; yt-dlp still chooses the
/// container. `%` is the template's escape character, so any in the user's
/// text is doubled to keep it literal — otherwise "100% done" would be read as
/// a field and mangle the filename.
fn out_template(dir: &Path, name: Option<&str>) -> String {
    match name.map(|n| n.replace('%', "%%")) {
        Some(stem) => format!("{}/{stem}.%(ext)s", dir.display()),
        None => format!("{}/%(title)s [%(id)s].%(ext)s", dir.display()),
    }
}

fn parse_progress(s: &str) -> Option<Tick> {
    let mut it = s.split('|');
    let downloaded = num_u64(it.next()?)?;
    let total_field = it.next().unwrap_or("NA");
    let est_field = it.next().unwrap_or("NA");
    let total = num_u64(total_field).or_else(|| num_u64(est_field));
    Some(Tick { downloaded, total })
}

fn num_u64(s: &str) -> Option<u64> {
    let s = s.trim();
    if s == "NA" || s.is_empty() {
        return None;
    }
    // yt-dlp emits floats for byte fields sometimes (e.g. "1048576.0").
    s.parse::<f64>().ok().map(|f| f as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_stream_manifests() {
        assert!(is_stream_manifest("https://e.test/live/x36xhzz.m3u8"));
        assert!(is_stream_manifest("https://e.test/v.mpd?token=abc"));
        assert!(!is_stream_manifest("https://e.test/video.mp4"));
        // Only the path counts, not a query string that merely mentions it.
        assert!(!is_stream_manifest("https://e.test/get?f=movie.m3u8.txt"));
        assert!(!is_stream_manifest("not a url"));
    }

    #[test]
    fn detects_video_hosts() {
        assert!(is_video_site("https://www.youtube.com/watch?v=abc"));
        assert!(is_video_site("https://youtu.be/abc"));
        assert!(is_video_site("https://vimeo.com/12345"));
        assert!(is_video_site("https://clips.twitch.tv/foo"));
        assert!(!is_video_site("https://example.com/file.zip"));
        assert!(!is_video_site("https://notyoutube.com.evil.test/x"));
        assert!(!is_video_site("not a url"));
    }

    #[test]
    fn format_args_map_quality() {
        assert!(format_args("best").is_empty());
        assert!(format_args("").is_empty());
        assert_eq!(
            format_args("1080"),
            vec!["-f", "bv*[height<=?1080]+ba/b[height<=?1080]"]
        );
        let audio = format_args("audio");
        assert!(audio.contains(&"--audio-format".to_string()));
        assert!(audio.contains(&"mp3".to_string()));
    }

    #[test]
    fn parses_progress_lines() {
        let t = parse_progress("1048576|10485760|NA|524288.0").unwrap();
        assert_eq!(t.downloaded, 1_048_576);
        assert_eq!(t.total, Some(10_485_760));

        // Total absent, estimate present.
        let t = parse_progress("2048|NA|4096|1000").unwrap();
        assert_eq!(t.downloaded, 2048);
        assert_eq!(t.total, Some(4096));

        // Everything unknown but downloaded.
        let t = parse_progress("500|NA|NA|NA").unwrap();
        assert_eq!(t.downloaded, 500);
        assert_eq!(t.total, None);
    }

    #[test]
    fn manifest_detection_is_case_insensitive_and_path_only() {
        assert!(is_stream_manifest("https://e.test/LIVE/STREAM.M3U8"));
        assert!(is_stream_manifest("https://e.test/dash/manifest.MPD#t=10"));
        // A manifest name in the middle of the path is not the resource.
        assert!(!is_stream_manifest("https://e.test/x.m3u8/thumb.jpg"));
        // Non-http schemes never reach yt-dlp through this check.
        assert!(!is_stream_manifest("file:///tmp/x.m3u8"));
    }

    #[test]
    fn video_host_matching_covers_subdomains_but_not_lookalikes() {
        assert!(is_video_site("https://m.youtube.com/watch?v=abc"));
        assert!(is_video_site("https://music.youtube.com/watch?v=abc"));
        assert!(is_video_site("https://WWW.YouTube.COM/watch?v=abc"), "host match is case-insensitive");
        // A suffix that is not on a label boundary must not match.
        assert!(!is_video_site("https://evilyoutube.com/watch?v=abc"));
        assert!(!is_video_site("https://youtube.com.evil.test/watch?v=abc"));
    }

    #[test]
    fn unknown_quality_becomes_a_height_filter_not_a_crash() {
        // Anything that is not "" / "best" / "audio" is treated as a height, so
        // a stale or hand-edited value still produces a usable format string.
        assert_eq!(
            format_args("240"),
            vec!["-f", "bv*[height<=?240]+ba/b[height<=?240]"]
        );
        // `<=?` keeps it a soft preference, so a garbage value cannot make
        // yt-dlp fail to find any format at all.
        assert!(format_args("nonsense")[1].contains("<=?nonsense"));
    }

    #[test]
    fn progress_lines_that_are_not_progress_are_rejected() {
        // A line with nothing parseable in the first field yields no tick.
        assert!(parse_progress("NA|NA|NA|NA").is_none());
        assert!(parse_progress("").is_none());
        assert!(parse_progress("hello world").is_none());
        // Short lines: missing trailing fields default to NA rather than panic.
        let t = parse_progress("4096").unwrap();
        assert_eq!(t.downloaded, 4096);
        assert_eq!(t.total, None);
        // A float byte count truncates rather than failing.
        assert_eq!(parse_progress("1048576.9|NA|NA").unwrap().downloaded, 1_048_576);
        // A total smaller than downloaded is passed through untouched; banking
        // finished phases is the caller's job, not the parser's.
        let t = parse_progress("900|100|NA").unwrap();
        assert_eq!((t.downloaded, t.total), (900, Some(100)));
    }

    #[test]
    fn output_template_escapes_percent_in_a_chosen_name() {
        let dir = Path::new("/d");
        // No name: yt-dlp's own title/id template.
        assert_eq!(out_template(dir, None), "/d/%(title)s [%(id)s].%(ext)s");
        // A chosen name fixes the stem and leaves the container to yt-dlp.
        assert_eq!(out_template(dir, Some("My Video")), "/d/My Video.%(ext)s");
        // A literal % must be doubled, or yt-dlp reads "%(e" as a field.
        assert_eq!(out_template(dir, Some("100% done")), "/d/100%% done.%(ext)s");
        assert_eq!(out_template(dir, Some("%(title)s")), "/d/%%(title)s.%(ext)s");
    }

    #[test]
    fn parses_output_paths() {
        assert_eq!(
            parse_destination("[download] Destination: /d/Video.f137.mp4").unwrap(),
            PathBuf::from("/d/Video.f137.mp4")
        );
        assert_eq!(
            parse_destination("[download] /d/Video.mp4 has already been downloaded").unwrap(),
            PathBuf::from("/d/Video.mp4")
        );
        assert_eq!(
            parse_final("[Merger] Merging formats into \"/d/Video.mp4\"").unwrap(),
            PathBuf::from("/d/Video.mp4")
        );
        assert_eq!(
            parse_final("[ExtractAudio] Destination: /d/Song.mp3").unwrap(),
            PathBuf::from("/d/Song.mp3")
        );
        assert_eq!(
            parse_final("[VideoRemuxer] Remuxing video from \"/d/Video.f137.mp4\" to \"/d/Video.mp4\"").unwrap(),
            PathBuf::from("/d/Video.mp4")
        );
        assert_eq!(
            parse_final("[FixupM3u8] Fixing code of \"/d/Live.mp4\"").unwrap(),
            PathBuf::from("/d/Live.mp4")
        );
        assert_eq!(
            parse_final("[FixupM4a] Correcting container in \"/d/Audio.m4a\"").unwrap(),
            PathBuf::from("/d/Audio.m4a")
        );
        assert!(parse_destination("[download] 5.0% of 10MiB").is_none());
    }
}
