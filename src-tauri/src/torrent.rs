//! The BitTorrent engine, via `librqbit`.
//!
//! A torrent is not a byte range over HTTP: pieces arrive out of order from
//! many peers, so none of the segment machinery applies. What spool needs from
//! it is what it needs from every engine — bytes done, total, and a file at the
//! end — which `TorrentStats` provides directly.
//!
//! ponytail: the transfer stops at 100% and does not seed. Seeding needs a
//! ratio policy, an upload cap and a UI for both; add it if the point becomes
//! sharing rather than downloading.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub type ManagedTorrentHandle = Arc<librqbit::ManagedTorrent>;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Session,
    TorrentStatsState,
};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::download::{self, DownloadPlan, Engine, Progress};

/// How often the transfer's counters are read. Pieces land continuously, so
/// this is a display cadence, not a transfer one.
const POLL: Duration = Duration::from_millis(1000);

/// One file entry inside a multi-file or single-file torrent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentFileView {
    pub id: usize,
    pub path: String,
    pub length: u64,
    pub downloaded: u64,
    pub selected: bool,
    pub full_path: Option<String>,
    pub completed: bool,
}

/// Comprehensive inspection and live swarm telemetry for a torrent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentDetailsView {
    pub id: Option<String>,
    pub name: String,
    pub info_hash: String,
    pub total_bytes: u64,
    pub piece_length: u32,
    pub pieces_count: usize,
    pub files: Vec<TorrentFileView>,
    pub trackers: Vec<String>,
    pub peers_connected: usize,
    pub peers_total: usize,
    pub upload_speed: u64,
    pub downloaded_bytes: u64,
    pub uploaded_bytes: u64,
}

/// A `.torrent` file on disk, written as a plain absolute path or as a
/// `file://` URL — which is how a desktop entry's `%U` hands a file over.
fn local_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    let path = match reqwest::Url::parse(raw) {
        Ok(url) if url.scheme() == "file" => url.to_file_path().ok()?,
        _ if Path::new(raw).is_absolute() => PathBuf::from(raw),
        _ => return None,
    };
    let is_torrent = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent"));
    is_torrent.then_some(path)
}

/// Whether this URL belongs to the torrent engine: a magnet link, an
/// `http(s)` URL pointing at a `.torrent` file, or a `.torrent` on disk.
pub fn is_torrent_url(raw: &str) -> bool {
    if local_path(raw).is_some() {
        return true;
    }
    let raw = raw.trim();
    let lower = raw.to_ascii_lowercase();
    if lower.starts_with("magnet:") {
        return true;
    }
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return false;
    }
    // The path decides, not the query string: `?x=.torrent` is not a torrent.
    match reqwest::Url::parse(raw) {
        Ok(url) => url.path().to_ascii_lowercase().ends_with(".torrent"),
        Err(_) => false,
    }
}

/// The name to show before any metadata has been fetched.
///
/// A magnet link carries one in `dn` (display name); a `.torrent` URL has its
/// filename. Both are provisional — the real name replaces it as soon as the
/// torrent's metadata resolves.
pub fn suggested_name(raw: &str) -> String {
    let raw = raw.trim();
    if raw.to_ascii_lowercase().starts_with("magnet:") {
        // `magnet:?xt=urn:btih:...&dn=Some+Name`
        let query = raw.split_once('?').map(|(_, q)| q).unwrap_or("");
        for field in query.split('&') {
            if let Some(value) = field.strip_prefix("dn=") {
                // `+` is a space in a query string, and the rest is escaped.
                let name = download::percent_decode(&value.replace('+', " "));
                if let Some(name) = download::sanitize_filename(&name) {
                    return name;
                }
            }
        }
        return "torrent".to_string();
    }

    if let Some(path) = local_path(raw) {
        return path
            .file_stem()
            .and_then(|stem| download::sanitize_filename(&stem.to_string_lossy()))
            .unwrap_or_else(|| "torrent".to_string());
    }

    reqwest::Url::parse(raw)
        .ok()
        .and_then(|url| {
            url.path_segments()?
                .next_back()
                .map(download::percent_decode)
        })
        .and_then(|name| download::sanitize_filename(name.trim_end_matches(".torrent")))
        .unwrap_or_else(|| "torrent".to_string())
}

/// Extract tracker announce URLs from a magnet link or .torrent path.
pub fn extract_trackers(raw: &str) -> Vec<String> {
    let mut trackers = Vec::new();
    let raw = raw.trim();
    if raw.to_ascii_lowercase().starts_with("magnet:") {
        if let Some((_, query)) = raw.split_once('?') {
            for field in query.split('&') {
                if let Some(value) = field.strip_prefix("tr=") {
                    let decoded = download::percent_decode(&value.replace('+', " "));
                    if !trackers.contains(&decoded) {
                        trackers.push(decoded);
                    }
                }
            }
        }
    }
    trackers
}

/// Build a plan without touching the network.
///
/// Nothing is known yet for a magnet link — not the size, not the real name,
/// not even how many files there are — so the plan carries a placeholder and
/// the run fills both in once the metadata arrives. This is the same shape the
/// yt-dlp engine uses for the same reason.
pub fn prepare(
    url: &str,
    dest_dir: &Path,
    custom_name: Option<&str>,
    only_files: Option<Vec<usize>>,
) -> Result<DownloadPlan, String> {
    if !is_torrent_url(url) {
        return Err(format!("not a torrent: {url}"));
    }
    // A file on disk is stored as its plain path whichever way it was written,
    // and a missing one is refused now rather than after the row is queued.
    let url = match local_path(url) {
        Some(path) if !path.is_file() => return Err(format!("no such file: {}", path.display())),
        Some(path) => path.display().to_string(),
        None => url.trim().to_string(),
    };
    let name = custom_name
        .and_then(download::sanitize_filename)
        .unwrap_or_else(|| suggested_name(&url));

    Ok(DownloadPlan {
        url,
        final_path: dest_dir.join(&name),
        // librqbit manages its own partial files inside the output folder.
        part_path: dest_dir.join(&name),
        total: None,
        supports_ranges: false,
        validator: None,
        ranges: Vec::new(),
        engine: Engine::Torrent,
        thumbnail: None,
        only_files,
    })
}

/// Construct a `TorrentDetailsView` from a live or paused `ManagedTorrentHandle`.
pub fn torrent_details_from_handle(
    handle: &ManagedTorrentHandle,
    id: Option<String>,
    final_path: Option<&Path>,
) -> TorrentDetailsView {
    let stats = handle.stats();
    let info_hash = handle.info_hash().as_string();
    let name = handle.name().unwrap_or_else(|| "torrent".to_string());
    let mut trackers: Vec<String> = handle
        .shared
        .trackers
        .iter()
        .map(|u| u.to_string())
        .collect();
    trackers.sort();

    let (peers_connected, peers_total, upload_speed) = match &stats.live {
        Some(l) => (
            l.snapshot.peer_stats.live as usize,
            l.snapshot.peer_stats.seen as usize,
            l.upload_speed.as_bytes(),
        ),
        None => (0, 0, 0),
    };

    let only_set = handle.only_files();

    let (total_bytes, piece_length, pieces_count, files) = handle
        .with_metadata(|meta| {
            let piece_len = meta.lengths().default_piece_length();
            let piece_cnt = meta.lengths().total_pieces() as usize;
            let tot = meta.lengths().total_length();
            let mut files_vec = Vec::new();
            for (i, fi) in meta.file_infos.iter().enumerate() {
                let rel = fi.relative_filename.display().to_string();
                let len = fi.len;
                let done = stats.file_progress.get(i).copied().unwrap_or(0);
                let selected = only_set.as_ref().map_or(true, |s| s.contains(&i));

                let candidate1 = final_path.map(|p| {
                    if p.is_dir() {
                        p.join(&fi.relative_filename)
                    } else {
                        p.to_path_buf()
                    }
                });
                let candidate2 = handle.output_folder().join(&fi.relative_filename);

                let full_path_buf = candidate1
                    .filter(|p| p.exists())
                    .or_else(|| if candidate2.exists() { Some(candidate2) } else { None });

                let is_completed = (done >= len && len > 0) || full_path_buf.is_some();
                let full_path_str = full_path_buf.map(|p| p.display().to_string());

                files_vec.push(TorrentFileView {
                    id: i,
                    path: rel,
                    length: len,
                    downloaded: done,
                    selected,
                    full_path: full_path_str,
                    completed: is_completed,
                });
            }
            (tot, piece_len, piece_cnt, files_vec)
        })
        .unwrap_or_else(|_| (stats.total_bytes, 0, 0, Vec::new()));

    TorrentDetailsView {
        id,
        name,
        info_hash,
        total_bytes,
        piece_length,
        pieces_count,
        files,
        trackers,
        peers_connected,
        peers_total,
        upload_speed,
        downloaded_bytes: stats.progress_bytes,
        uploaded_bytes: stats.uploaded_bytes,
    }
}

/// Inspect a torrent file or magnet link without starting a transfer.
///
/// Fetches metainfo (files, sizes, piece count, info hash, and announce trackers)
/// so the user can inspect content or select which files to download up front.
pub async fn inspect(session: &Arc<Session>, url: &str) -> Result<TorrentDetailsView, String> {
    if !is_torrent_url(url) {
        return Err(format!("not a torrent: {url}"));
    }

    let source = match local_path(url) {
        Some(path) => {
            if !path.is_file() {
                return Err(format!("no such file: {}", path.display()));
            }
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            AddTorrent::from_bytes(bytes)
        }
        None => AddTorrent::from_url(url.trim().to_string()),
    };

    let timeout_dur = if url.trim().to_ascii_lowercase().starts_with("magnet:") {
        Duration::from_secs(12)
    } else {
        Duration::from_secs(6)
    };

    let options = AddTorrentOptions {
        list_only: true,
        ..Default::default()
    };

    let response = match tokio::time::timeout(timeout_dur, session.add_torrent(source, Some(options))).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => return Err(format!("cannot inspect torrent: {e:#}")),
        Err(_) => {
            return Err("Timed out resolving torrent metadata from swarm. You can still add the torrent directly.".to_string())
        }
    };

    match response {
        AddTorrentResponse::ListOnly(list) => {
            let info_hash = list.info_hash.as_string();
            let name = list
                .info
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| suggested_name(url));
            let piece_length = list.info.lengths().default_piece_length();
            let pieces_count = list.info.lengths().total_pieces() as usize;
            let total_bytes = list.info.lengths().total_length();

            let mut files = Vec::new();
            let only_set = list.only_files.as_ref();
            for (i, fd) in list.info.iter_file_details().enumerate() {
                let rel = fd.filename.to_pathbuf().display().to_string();
                let len = fd.len;
                let selected = only_set.map_or(true, |s| s.contains(&i));
                files.push(TorrentFileView {
                    id: i,
                    path: rel,
                    length: len,
                    downloaded: 0,
                    selected,
                    full_path: None,
                    completed: false,
                });
            }

            let mut trackers = extract_trackers(url);
            if trackers.is_empty() && !list.torrent_bytes.is_empty() {
                if let Ok(meta) = librqbit::torrent_from_bytes(&list.torrent_bytes) {
                    if let Some(ann) = meta.announce {
                        let a = String::from_utf8_lossy(ann.as_ref()).to_string();
                        if !trackers.contains(&a) {
                            trackers.push(a);
                        }
                    }
                    for group in meta.announce_list {
                        for tr in group {
                            let s = String::from_utf8_lossy(tr.as_ref()).to_string();
                            if !trackers.contains(&s) {
                                trackers.push(s);
                            }
                        }
                    }
                }
            }

            Ok(TorrentDetailsView {
                id: None,
                name,
                info_hash,
                total_bytes,
                piece_length,
                pieces_count,
                files,
                trackers,
                peers_connected: list.seen_peers.len(),
                peers_total: list.seen_peers.len(),
                upload_speed: 0,
                downloaded_bytes: 0,
                uploaded_bytes: 0,
            })
        }
        AddTorrentResponse::AlreadyManaged(_, handle) | AddTorrentResponse::Added(_, handle) => {
            Ok(torrent_details_from_handle(&handle, None, None))
        }
    }
}

/// Run or resume a torrent.
///
/// Resume needs no special handling: librqbit rechecks what is already in the
/// output folder and asks peers only for the pieces still missing — which is
/// why `overwrite` has to be set, since the files are already there.
pub async fn run<F, G, H>(
    session: &Arc<Session>,
    plan: &DownloadPlan,
    progress: &Progress,
    token: CancellationToken,
    on_progress: F,
    on_name: G,
    on_handle: H,
) -> Result<PathBuf, String>
where
    F: Fn(u64, Option<u64>, Option<usize>, Option<usize>, Option<u64>, Option<u64>) + Send + Sync + 'static,
    G: Fn(&str) + Send + Sync + 'static,
    H: Fn(ManagedTorrentHandle) + Send + Sync + 'static,
{
    let output_folder = plan
        .final_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));

    let options = AddTorrentOptions {
        output_folder: Some(output_folder.display().to_string()),
        // Required for resume: the files from the previous run are already on
        // disk, and without this librqbit refuses to write over them.
        overwrite: true,
        only_files: plan.only_files.clone(),
        ..Default::default()
    };

    let source = match local_path(&plan.url) {
        // Read on every run rather than kept in the plan: the file is small,
        // and the plan stays a plain string like every other engine's.
        Some(path) => AddTorrent::from_bytes(
            tokio::fs::read(&path)
                .await
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
        ),
        None => AddTorrent::from_url(plan.url.clone()),
    };
    let response = session
        .add_torrent(source, Some(options))
        .await
        .map_err(|e| format!("cannot add torrent: {e:#}"))?;
    let handle = response
        .into_handle()
        .ok_or_else(|| "that torrent produced no transfer".to_string())?;

    on_handle(handle.clone());

    // The real name, once the metadata is in. For a magnet that is the first
    // moment the row can stop saying "torrent".
    let mut named = false;
    let mut interval = tokio::time::interval(POLL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                // Leave the torrent in the session, paused: a resume then
                // picks it up with its pieces intact instead of rechecking
                // the whole thing from scratch.
                let _ = session.pause(&handle).await;
                return Err("cancelled".to_string());
            }
            _ = interval.tick() => {}
        }

        let stats = handle.stats();

        if !named {
            if let Some(name) = handle.name().and_then(|n| download::sanitize_filename(&n)) {
                on_name(&name);
                named = true;
            }
        }

        if let TorrentStatsState::Error = stats.state {
            return Err(stats.error.unwrap_or_else(|| "the torrent failed".to_string()));
        }

        progress.set_absolute(stats.progress_bytes);
        let total = (stats.total_bytes > 0).then_some(stats.total_bytes);
        let (peers, peers_total, up_speed) = match &stats.live {
            Some(l) => (
                Some(l.snapshot.peer_stats.live as usize),
                Some(l.snapshot.peer_stats.seen as usize),
                Some(l.upload_speed.as_bytes()),
            ),
            None => (None, None, None),
        };
        on_progress(
            stats.progress_bytes,
            total,
            peers,
            peers_total,
            up_speed,
            Some(stats.uploaded_bytes),
        );

        if stats.finished {
            break;
        }
    }

    // Stop at 100%: without this the transfer would sit in the queue seeding
    // and never report itself done.
    let _ = session.pause(&handle).await;

    // Now that the metadata is known, the real output path is too — a
    // multi-file torrent is a folder, a single-file one is the file in it.
    // Untrusted metadata must be sanitized and never default to the base
    // output directory.
    let final_path = handle
        .name()
        .and_then(|n| download::sanitize_filename(&n))
        .map(|name| handle.output_folder().join(name))
        .unwrap_or_else(|| plan.final_path.clone());
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnets_and_torrent_files_are_claimed() {
        assert!(is_torrent_url("magnet:?xt=urn:btih:abc"));
        assert!(is_torrent_url("MAGNET:?xt=urn:btih:abc"), "the scheme is case-insensitive");
        assert!(is_torrent_url("https://example.com/ubuntu.torrent"));
        assert!(is_torrent_url("http://example.com/path/x.TORRENT"));
    }

    #[test]
    fn ordinary_downloads_are_left_alone() {
        assert!(!is_torrent_url("https://example.com/a.zip"));
        assert!(!is_torrent_url("ftp://example.com/a.torrent"), "FTP is the FTP engine's");
        assert!(!is_torrent_url("not a url"));
    }

    /// The path decides. A query string that merely mentions `.torrent` is an
    /// ordinary HTTP download.
    #[test]
    fn a_query_string_does_not_make_a_torrent() {
        assert!(!is_torrent_url("https://example.com/get?file=a.torrent"));
    }

    #[test]
    fn a_magnet_display_name_becomes_the_row_name() {
        assert_eq!(
            suggested_name("magnet:?xt=urn:btih:abc&dn=Debian+12+ISO"),
            "Debian 12 ISO",
        );
        assert_eq!(
            suggested_name("magnet:?xt=urn:btih:abc&dn=my%20file.iso"),
            "my file.iso",
        );
    }

    #[test]
    fn a_magnet_without_a_name_still_has_one() {
        assert_eq!(suggested_name("magnet:?xt=urn:btih:abc"), "torrent");
    }

    #[test]
    fn a_torrent_url_names_itself_after_its_file() {
        assert_eq!(suggested_name("https://example.com/ubuntu-24.04.torrent"), "ubuntu-24.04");
    }

    #[test]
    fn a_plan_carries_the_provisional_name() {
        let plan = prepare("magnet:?xt=urn:btih:abc&dn=Thing", Path::new("/tmp/dl"), None, None).unwrap();
        assert_eq!(plan.engine, Engine::Torrent);
        assert_eq!(plan.final_path, Path::new("/tmp/dl/Thing"));
        assert!(plan.total.is_none(), "a magnet knows no size until metadata arrives");
        assert!(plan.ranges.is_empty(), "there is nothing to segment");
    }

    #[test]
    fn a_plan_carries_file_selection() {
        let plan = prepare("magnet:?xt=urn:btih:abc&dn=Thing", Path::new("/tmp/dl"), None, Some(vec![1, 3])).unwrap();
        assert_eq!(plan.only_files, Some(vec![1, 3]));
    }

    #[test]
    fn a_typed_name_wins_over_the_magnet() {
        let plan =
            prepare("magnet:?xt=urn:btih:abc&dn=Thing", Path::new("/tmp/dl"), Some("mine"), None).unwrap();
        assert_eq!(plan.final_path, Path::new("/tmp/dl/mine"));
    }

    #[test]
    fn a_non_torrent_is_refused() {
        assert!(prepare("https://example.com/a.zip", Path::new("/tmp"), None, None).is_err());
    }

    #[test]
    fn a_torrent_file_on_disk_is_claimed() {
        assert!(is_torrent_url("/home/me/Downloads/debian.torrent"));
        assert!(is_torrent_url("file:///home/me/My%20Files/debian.TORRENT"));
        assert!(!is_torrent_url("/home/me/notes.txt"));
        assert!(!is_torrent_url("relative/debian.torrent"), "only absolute paths");
    }

    /// The add dialog hands a picked file over as a `file://` URL, so a space
    /// in the path cannot split it into two links.
    #[test]
    fn a_file_url_is_stored_as_its_path() {
        let dir = std::env::temp_dir().join(format!("spool-torrent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("My Distro.torrent");
        std::fs::write(&file, b"d4:infoe").unwrap();
        let url = reqwest::Url::from_file_path(&file).unwrap().to_string();

        let plan = prepare(&url, Path::new("/tmp/dl"), None, None).unwrap();
        assert_eq!(plan.url, file.display().to_string());
        assert_eq!(plan.final_path, Path::new("/tmp/dl/My Distro"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_torrent_file_is_refused_up_front() {
        assert!(prepare("/nonexistent/spool/x.torrent", Path::new("/tmp"), None, None).is_err());
    }

    #[test]
    fn magnet_trackers_are_extracted() {
        let url = "magnet:?xt=urn:btih:abc&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=http%3A%2F%2Ftracker.example.com%2Fannounce";
        let trackers = extract_trackers(url);
        assert_eq!(trackers.len(), 2);
        assert_eq!(trackers[0], "udp://tracker.opentrackr.org:1337/announce");
        assert_eq!(trackers[1], "http://tracker.example.com/announce");
    }
}
