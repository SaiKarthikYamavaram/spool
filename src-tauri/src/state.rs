//! Shared application state and the download queue.
//!
//! Locking rule for everything here: the `Mutex` guards are held for plain
//! data access only, and never across an `.await`. Parking a task on an
//! executor thread while holding a `std::sync::Mutex` is how this shape
//! deadlocks, and with up to 24 segment tasks plus a ticker plus IPC handlers
//! all touching the queue, it would deadlock reliably.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tokio_util::sync::CancellationToken;

use crate::cookies;
use crate::download::{self, Progress, Session};
use crate::queue::{self, Download, Status};
use crate::throttle::Throttle;

/// How long an unanswered add-dialog request is kept before being swept.
const PENDING_TTL_SECS: u64 = 60 * 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub download_dir: Option<PathBuf>,
    pub max_concurrent: usize,
    pub segments: u32,
    pub theme: String,
    /// Netscape `cookies.txt` exported from a browser.
    ///
    /// The only way past an interactive anti-bot challenge: the browser solves
    /// it, and spool replays the cookie it earned. Header spoofing and TLS
    /// fingerprint impersonation both fail against a managed challenge.
    pub cookies_file: Option<PathBuf>,
    /// Must match the browser the cookies came from — a `cf_clearance` cookie
    /// is bound to the exact User-Agent that earned it.
    pub user_agent: Option<String>,
    /// Aggregate download speed cap in KB/s across all transfers. 0 or absent
    /// means unlimited.
    #[serde(default)]
    pub bandwidth_kb: u64,
    /// Path to the yt-dlp binary. Empty falls back to `yt-dlp` on PATH.
    #[serde(default)]
    pub ytdlp_path: String,
    /// Browser to read cookies from for yt-dlp (`--cookies-from-browser`),
    /// e.g. "brave". Empty uses the manual cookies file, if any.
    #[serde(default)]
    pub cookies_browser: String,
    /// yt-dlp quality: "best" (default), a max height ("2160".."480"), or
    /// "audio" (extract to mp3).
    #[serde(default)]
    pub video_quality: String,
    /// Proxy URL for every download, e.g. "http://host:8080" or
    /// "socks5://host:1080". Empty means direct.
    #[serde(default)]
    pub proxy: String,
    /// Sort finished downloads into per-type sub-folders (Video, Audio, ...)
    /// of the download folder. Ignored when a location is picked per download.
    #[serde(default)]
    pub categorize: bool,
    /// Closing the window hides to the tray and downloads carry on. Off makes
    /// the close button quit, which is what a user who does not want a
    /// background process expects.
    ///
    /// Defaults true, and explicitly so: a plain `#[serde(default)]` on a bool
    /// is `false`, which would read every settings file written before this
    /// field existed as "quit on close" and change the behaviour under people
    /// who never asked for it.
    #[serde(default = "yes")]
    pub run_in_background: bool,
    /// Start spool with the desktop session. Written to the desktop's
    /// autostart entry, so it is real state on disk rather than a preference
    /// we consult.
    ///
    /// Defaults true for the same reason as `run_in_background`: a download
    /// manager is only useful when it is already there as a download arrives.
    #[serde(default = "yes")]
    pub start_on_login: bool,
    /// Start minimised to the tray. Only meaningful with `start_on_login`:
    /// a launcher-started app should show itself.
    #[serde(default = "yes")]
    pub start_minimised: bool,
    /// Only transfer inside a daily time window. Off by default: a download
    /// manager that silently refuses to download is a support call.
    #[serde(default)]
    pub schedule_enabled: bool,
    /// Window bounds as local `HH:MM`. A start later than the stop wraps over
    /// midnight ("22:00" to "06:00"), which is the usual off-peak case.
    #[serde(default)]
    pub schedule_start: String,
    #[serde(default)]
    pub schedule_stop: String,
    /// What to do once every download has finished: "none" (default), "quit",
    /// or "shutdown" (powers the machine off).
    #[serde(default)]
    pub on_all_done: String,
    /// Watch the system clipboard and offer to download any URL copied to it.
    #[serde(default)]
    pub clipboard_watch: bool,
}

/// Serde needs a function for a non-`false` bool default.
fn yes() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            download_dir: None,
            max_concurrent: 3,
            segments: download::DEFAULT_SEGMENTS,
            theme: "system".into(),
            cookies_file: None,
            user_agent: None,
            bandwidth_kb: 0,
            ytdlp_path: String::new(),
            cookies_browser: String::new(),
            video_quality: "best".into(),
            proxy: String::new(),
            categorize: false,
            // On by default: this is a download manager, and closing the
            // window mid-transfer should not cancel the transfer.
            run_in_background: true,
            start_on_login: true,
            start_minimised: true,
            schedule_enabled: false,
            schedule_start: "22:00".into(),
            schedule_stop: "06:00".into(),
            on_all_done: "none".into(),
            clipboard_watch: false,
        }
    }
}

/// Per-download choices from the add dialog. All optional: omitted fields fall
/// back to the global settings.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AddOptions {
    /// Save location for this download only.
    pub dir: Option<String>,
    /// yt-dlp quality override ("best", "1080", "audio", ...).
    pub quality: Option<String>,
    /// Filename to save as, instead of the one the server or the video title
    /// suggests. An extension is added from the source when omitted.
    pub name: Option<String>,
    /// Start immediately (default), or add it paused for later.
    pub start: Option<bool>,
}

/// What a multi-row selection asks for. Sending the whole selection as one
/// command keeps it to a single round trip and a single queue emit, rather than
/// one of each per row.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BulkAction {
    Pause,
    Resume,
    Remove,
    RemoveWithFile,
}

/// A download the extension captured but the user has not confirmed yet.
/// Held here so the browser session (cookies/UA/referer) survives until the
/// add dialog is answered.
#[derive(Debug, Clone)]
pub struct PendingAdd {
    pub url: String,
    pub session: Option<Session>,
    pub force_video: bool,
    /// When it was parked, so abandoned requests can be swept.
    pub added_at: u64,
}

/// Emitted to the frontend to open the add dialog for a captured URL.
#[derive(Debug, Clone, Serialize)]
pub struct ConfirmRequest {
    pub token: String,
    pub url: String,
    pub video: bool,
}

/// One row as the frontend sees it.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadView {
    pub id: String,
    pub url: String,
    pub filename: String,
    pub status: Status,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub segments: usize,
    pub error: Option<String>,
    pub path: String,
    // Detail-view fields.
    pub added_at: u64,
    /// Inclusive `[start, end]` byte range per segment.
    pub ranges: Vec<(u64, u64)>,
    /// Bytes done per segment, index-aligned with `ranges`.
    pub done: Vec<u64>,
    pub supports_ranges: bool,
    /// Session captured by the extension, if any (cookie value withheld).
    pub user_agent: Option<String>,
    pub referer: Option<String>,
    pub has_cookie: bool,
    /// Remote thumbnail URL for a preview, if any.
    pub thumbnail: Option<String>,
    pub engine: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProgressRow {
    pub id: String,
    pub downloaded: u64,
    pub total: Option<u64>,
}

/// A transfer currently occupying a slot.
struct Active {
    token: CancellationToken,
    progress: Progress,
    /// Distinguishes successive runs of the same id. A task that has been
    /// superseded (paused then resumed as a new run) sees a different gen here
    /// and must not touch shared state on its way out.
    gen: u64,
}

pub struct AppState {
    queue: Mutex<Vec<Download>>,
    active: Mutex<HashMap<String, Active>>,
    settings: Mutex<Settings>,
    next_id: AtomicU64,
    /// Monotonic run counter; see `Active::gen`.
    gen: AtomicU64,
    /// Shared bandwidth limiter; rebuilt when the cap setting changes.
    throttle: Mutex<Throttle>,
    /// Captured-but-unconfirmed adds, keyed by token (see `PendingAdd`).
    pending: Mutex<HashMap<String, PendingAdd>>,
    /// Set when progress moved; a single flusher persists it (see `mark_dirty`).
    dirty: std::sync::atomic::AtomicBool,
    /// The BitTorrent session, started the first time a torrent is added.
    /// Never started otherwise: it opens a listening port and joins the DHT,
    /// which a user who only downloads over HTTP has not asked for.
    torrent: tokio::sync::OnceCell<Arc<librqbit::Session>>,
    data_dir: PathBuf,
    config_dir: PathBuf,
}

impl AppState {
    pub fn new(data_dir: PathBuf, config_dir: PathBuf) -> Result<Self, String> {
        let settings: Settings =
            queue::load_json(&config_dir.join("settings.json")).unwrap_or_default();

        let mut downloads: Vec<Download> = queue::load_queue(&data_dir.join("queue.json"));

        // Apply the restart state table before anything can observe the queue.
        for d in &mut downloads {
            d.status = queue::reconcile_on_launch(d.status);
        }

        let next = downloads
            .iter()
            .filter_map(|d| d.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0);

        Ok(AppState {
            queue: Mutex::new(downloads),
            active: Mutex::new(HashMap::new()),
            settings: Mutex::new(settings),
            next_id: AtomicU64::new(next + 1),
            gen: AtomicU64::new(1),
            torrent: tokio::sync::OnceCell::new(),
            // Built unlimited here (this runs off the async runtime, and
            // Throttle spawns a task); `rebuild_throttle` applies the saved cap
            // from a runtime context during setup.
            throttle: Mutex::new(Throttle::unlimited()),
            pending: Mutex::new(HashMap::new()),
            dirty: std::sync::atomic::AtomicBool::new(false),
            data_dir,
            config_dir,
        })
    }

    /// Build a client pair carrying whatever cookies apply to this URL.
    ///
    /// The session to use for `url`.
    ///
    /// Precedence: a session captured by the browser extension for this exact
    /// download wins, because it carries the live cookie that just cleared a
    /// challenge. Otherwise fall back to the manual `cookies.txt` matched
    /// against the URL. The extension path is the one that gets past an
    /// interactive challenge; the file is the manual equivalent.
    pub fn session_for(&self, url: &str, captured: Option<Session>) -> Session {
        if let Some(mut session) = captured {
            // The extension knows nothing about the proxy setting.
            let proxy = self.settings().proxy;
            session.proxy = Some(proxy).filter(|p| !p.is_empty());
            return session;
        }

        let settings = self.settings();
        let mut session = Session::with_agent(settings.user_agent.clone());
        session.proxy = Some(settings.proxy.clone()).filter(|p| !p.is_empty());

        if let Some(path) = &settings.cookies_file {
            if let Ok(parsed) = reqwest::Url::parse(url) {
                match cookies::load(path) {
                    Ok(jar) => session.cookie = cookies::header_for(&jar, &parsed),
                    Err(e) => eprintln!("spool: {e}"),
                }
            }
        }
        session
    }

    /// Build a client pair from a session. Built per download rather than
    /// shared, because `Cookie` and `Referer` are host-specific.
    pub fn clients_for(&self, session: &Session) -> Result<(Client, Client), String> {
        Ok((
            download::build_client(session)?,
            download::build_segment_client(session)?,
        ))
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    pub fn set_settings(&self, mut settings: Settings) {
        // Settings arrive over IPC; clamp them rather than trusting the UI.
        // An out-of-range concurrency would have `pump` spawn that many
        // transfers at once.
        settings.max_concurrent = settings.max_concurrent.clamp(1, 16);
        settings.segments = settings.segments.clamp(1, download::MAX_SEGMENTS);
        *self.settings.lock().unwrap() = settings;
        self.save_settings();
        self.rebuild_throttle();
    }

    /// Rebuild the shared limiter from the current bandwidth setting. Must be
    /// called from within the async runtime — `Throttle::new` spawns a refill
    /// task. A cap of 0 yields an unlimited (zero-overhead) throttle.
    /// The BitTorrent session, started on first use and shared from then on.
    ///
    /// One per app, not one per download: a session owns the listening port,
    /// the DHT node and the peer tables, and a second one would contend for
    /// all three.
    pub async fn torrent_session(
        &self,
        app: &AppHandle,
    ) -> Result<Arc<librqbit::Session>, String> {
        let dir = self.download_dir(app)?;
        self.torrent
            .get_or_try_init(|| async {
                librqbit::Session::new(dir)
                    .await
                    .map_err(|e| format!("cannot start the BitTorrent session: {e:#}"))
            })
            .await
            .map(Arc::clone)
    }

    pub fn rebuild_throttle(&self) {
        let kb = self.settings().bandwidth_kb;
        *self.throttle.lock().unwrap() = Throttle::new(kb);
    }

    fn current_throttle(&self) -> Throttle {
        self.throttle.lock().unwrap().clone()
    }

    pub fn download_dir(&self, app: &AppHandle) -> Result<PathBuf, String> {
        if let Some(dir) = self.settings().download_dir {
            return Ok(dir);
        }
        app.path()
            .download_dir()
            .map_err(|e| format!("cannot locate the downloads folder: {e}"))
    }

    fn next_id(&self) -> String {
        self.next_id.fetch_add(1, Ordering::Relaxed).to_string()
    }

    // -- queue access -------------------------------------------------------

    pub fn views(&self) -> Vec<DownloadView> {
        let active = self.active.lock().unwrap();
        self.queue
            .lock()
            .unwrap()
            .iter()
            .map(|d| {
                // A running transfer's counters are ahead of what has been
                // persisted, and its ranges may have been re-split since.
                let (ranges, done) = match active.get(&d.id) {
                    Some(a) => {
                        let (ranges, done) = a.progress.layout();
                        (ranges.unwrap_or_else(|| d.plan.ranges.clone()), done)
                    }
                    None => (d.plan.ranges.clone(), d.done.clone()),
                };
                // Connections actually open: finished pieces of a re-split
                // file are not connections.
                let open = ranges
                    .iter()
                    .zip(&done)
                    .filter(|&(&(start, end), &got)| start + got <= end)
                    .count();
                DownloadView {
                id: d.id.clone(),
                url: d.url.clone(),
                filename: d.filename(),
                status: d.status,
                // A running transfer's live counters are ahead of what has
                // been persisted, so prefer them when present.
                downloaded: active
                    .get(&d.id)
                    .map(|a| a.progress.total())
                    .unwrap_or_else(|| d.downloaded()),
                total: d.plan.total,
                segments: open.max(1),
                error: d.error.clone(),
                path: d.plan.final_path.display().to_string(),
                added_at: d.added_at,
                ranges,
                done,
                supports_ranges: d.plan.supports_ranges,
                user_agent: d.session.as_ref().map(|s| s.user_agent.clone()),
                referer: d.session.as_ref().and_then(|s| s.referer.clone()),
                has_cookie: d.session.as_ref().is_some_and(|s| s.cookie.is_some()),
                thumbnail: d.plan.thumbnail.clone(),
                engine: match d.plan.engine {
                    download::Engine::YtDlp => "ytdlp".into(),
                    download::Engine::Http => "http".into(),
                    download::Engine::Ftp => "ftp".into(),
                    download::Engine::Torrent => "torrent".into(),
                },
                }
            })
            .collect()
    }

    pub fn has_url(&self, url: &str) -> bool {
        self.queue
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.url == url && !d.is_terminal())
    }

    fn set_status(&self, id: &str, status: Status, error: Option<String>) {
        let mut queue = self.queue.lock().unwrap();
        if let Some(d) = queue.iter_mut().find(|d| d.id == id) {
            d.status = status;
            d.error = error;
        }
    }

    /// Persist durable offsets and, once segments have been re-split, the
    /// ranges they now cover. Both come from one read so they stay aligned.
    fn record_progress(&self, id: &str, progress: &Progress) {
        let (ranges, done) = progress.layout();
        let mut queue = self.queue.lock().unwrap();
        if let Some(d) = queue.iter_mut().find(|d| d.id == id) {
            d.done = done;
            if let Some(ranges) = ranges {
                d.plan.ranges = ranges;
            }
        }
    }

    /// Update only the displayed name (keeping the directory) while a yt-dlp
    /// download is still running, so the row shows the real title instead of the
    /// "…video" placeholder before it finishes.
    fn set_display_name(&self, id: &str, name: &str) {
        let changed = {
            let mut queue = self.queue.lock().unwrap();
            match queue.iter_mut().find(|d| d.id == id) {
                Some(d) => {
                    let next = d.plan.final_path.with_file_name(name);
                    let changed = d.plan.final_path != next;
                    d.plan.final_path = next;
                    changed
                }
                None => false,
            }
        };
        if changed {
            self.save_queue();
        }
    }

    /// Set the resolved output path once yt-dlp reports it (its filename is only
    /// known after resolution), and record the final size from disk so the
    /// completed row shows a real size instead of "unknown".
    fn set_final_path(&self, id: &str, path: PathBuf) {
        // Only a real file has a meaningful length. If yt-dlp errored before
        // naming an output, `path` falls back to the directory, whose metadata
        // len() is the inode block size (4096) — never record that as the size.
        let size = std::fs::metadata(&path)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        let mut queue = self.queue.lock().unwrap();
        if let Some(d) = queue.iter_mut().find(|d| d.id == id) {
            d.plan.final_path = path;
            if let Some(size) = size {
                d.plan.total = Some(size);
                // yt-dlp keeps no per-segment counters, so mark it fully done
                // here or the completed row would read 0 / total.
                d.done = vec![size];
            }
        }
    }

    /// Record the total size once yt-dlp reports it, never shrinking (yt-dlp's
    /// per-phase totals would otherwise flip video→audio size).
    fn set_total(&self, id: &str, total: u64) {
        let changed = {
            let mut queue = self.queue.lock().unwrap();
            if let Some(d) = queue.iter_mut().find(|d| d.id == id) {
                if d.plan.total.is_none_or(|t| total > t) {
                    d.plan.total = Some(total);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if changed {
            self.save_queue();
        }
    }

    /// Note that progress changed without writing to disk.
    ///
    /// Every active download used to persist the whole queue (with an fsync)
    /// on its own 2s checkpoint, so N downloads meant N full writes per tick.
    /// They now just mark the queue dirty and one flusher does a single write.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Persist once if anything marked the queue dirty since the last flush.
    pub fn flush_if_dirty(&self) {
        if self.dirty.swap(false, std::sync::atomic::Ordering::Relaxed) {
            self.save_queue();
        }
    }

    pub fn save_queue(&self) {
        let snapshot = self.queue.lock().unwrap().clone();
        if let Err(e) = queue::save_json(&self.data_dir.join("queue.json"), &snapshot) {
            eprintln!("spool: could not save queue: {e}");
        }
    }

    fn save_settings(&self) {
        let snapshot = self.settings.lock().unwrap().clone();
        if let Err(e) = queue::save_json(&self.config_dir.join("settings.json"), &snapshot) {
            eprintln!("spool: could not save settings: {e}");
        }
    }

    // -- commands -----------------------------------------------------------

    /// Park a captured request until the user answers the add dialog. Returns
    /// the token the frontend sends back.
    pub fn stash_pending(&self, pending: PendingAdd) -> String {
        let token = format!("p{}", self.gen.fetch_add(1, Ordering::Relaxed));
        let mut map = self.pending.lock().unwrap();
        // A dialog closed by shutting the window never answers, so sweep
        // anything abandoned rather than growing forever.
        let now = queue::now_secs();
        map.retain(|_, p| now.saturating_sub(p.added_at) < PENDING_TTL_SECS);
        map.insert(token.clone(), pending);
        token
    }

    pub fn take_pending(&self, token: &str) -> Option<PendingAdd> {
        self.pending.lock().unwrap().remove(token)
    }

    /// Add a URL typed into the app. Uses the manual cookies.txt session, if
    /// any.
    pub async fn add(&self, app: &AppHandle, url: &str) -> Result<String, String> {
        self.add_with_session(app, url, None, false, AddOptions::default()).await
    }

    /// Add a URL, optionally with a session the browser extension captured for
    /// it, and optionally forcing the yt-dlp video engine. The session is
    /// stored on the entry so a later resume replays the same cookie/UA/referer.
    pub async fn add_with_session(
        &self,
        app: &AppHandle,
        url: &str,
        captured: Option<Session>,
        force_video: bool,
        opts: AddOptions,
    ) -> Result<String, String> {
        // A per-download location from the add dialog wins over the default.
        let explicit_dir = opts.dir.as_deref().filter(|d| !d.trim().is_empty());
        let dir = match explicit_dir {
            Some(d) => PathBuf::from(d),
            None => self.download_dir(app)?,
        };
        // `https://user:pass@host/file` authenticates, but the credentials
        // travel as a header from here on: the URL below is displayed, copied
        // and duplicate-checked, and a password belongs in none of that.
        let (auth, stripped) = download::split_userinfo(url);
        let url = stripped.as_str();

        let custom_name = opts.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
        let settings = self.settings();
        // A folder chosen for this download is taken literally; category
        // sorting only shapes the default location.
        let categorize = settings.categorize && explicit_dir.is_none();
        let mut session = self.session_for(url, captured.clone());
        session.auth = auth;

        let plan = if force_video
            || crate::ytdlp::is_video_site(url)
            || crate::ytdlp::is_stream_manifest(url)
        {
            // Resolve title + thumbnail up front (with a timeout) so the row
            // shows the real name and a preview immediately, not a placeholder.
            let ytdlp = if settings.ytdlp_path.is_empty() { "yt-dlp".to_string() } else { settings.ytdlp_path.clone() };
            let cookies = crate::ytdlp::Cookies {
                file: settings.cookies_file.clone(),
                browser: if settings.cookies_browser.is_empty() { None } else { Some(settings.cookies_browser.clone()) },
            };
            let proxy = Some(settings.proxy.as_str()).filter(|p| !p.is_empty());
            let (title, thumbnail) =
                crate::ytdlp::resolve_meta(&ytdlp, url, &cookies, proxy).await;
            // yt-dlp names the file itself, so the category is decided by the
            // chosen quality rather than an extension.
            let dir = if categorize {
                let bucket = if opts.quality.as_deref().unwrap_or(&settings.video_quality) == "audio" {
                    "Audio"
                } else {
                    "Video"
                };
                dir.join(bucket)
            } else {
                dir
            };
            download::video_plan(url, &dir, title, thumbnail, custom_name)?
        } else if crate::torrent::is_torrent_url(url) {
            // No network here: a magnet knows nothing until it has found peers
            // with the metadata, so the row starts on its display name.
            let dir = if categorize { dir.join("Torrents") } else { dir };
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            crate::torrent::prepare(url, &dir, custom_name)?
        } else if crate::ftp::is_ftp_url(url) {
            crate::ftp::prepare(url, &session, &dir, categorize, custom_name).await?
        } else {
            let (client, _) = self.clients_for(&session)?;
            download::prepare(&client, url, &dir, settings.segments, categorize, custom_name).await?
        };

        let id = self.next_id();
        let mut entry = Download::new(id.clone(), plan);
        // Only persist a non-default session; a plain download carries none.
        if session.cookie.is_some() || session.referer.is_some() || session.auth.is_some() {
            entry.session = Some(session);
        }
        entry.quality = opts.quality.filter(|q| !q.trim().is_empty());
        entry.name = custom_name.map(str::to_string);
        // "Download later" parks the entry as Paused so `pump` skips it until
        // the user hits Resume.
        if opts.start == Some(false) {
            entry.status = Status::Paused;
        }
        self.queue.lock().unwrap().push(entry);
        self.save_queue();
        Ok(id)
    }

    /// Apply one action to a selection of entries.
    ///
    /// `pause` and `resume` set the status unconditionally, so a mixed
    /// selection is filtered here — otherwise "Pause" would mark a finished
    /// download Paused, and "Resume" would queue it for a second download.
    pub fn bulk(&self, ids: &[String], action: BulkAction) {
        let status: HashMap<String, Status> = {
            let queue = self.queue.lock().unwrap();
            queue.iter().map(|d| (d.id.clone(), d.status)).collect()
        };

        for id in ids {
            let Some(st) = status.get(id).copied() else { continue };
            match action {
                BulkAction::Pause if matches!(st, Status::Downloading | Status::Queued) => {
                    self.pause(id)
                }
                BulkAction::Resume
                    if matches!(st, Status::Paused | Status::Interrupted | Status::Failed) =>
                {
                    self.resume(id)
                }
                BulkAction::Remove => self.remove(id, false),
                BulkAction::RemoveWithFile => self.remove(id, true),
                _ => {}
            }
        }
    }

    /// Rename an entry's file. The extension is kept when the new name omits
    /// one, matching the add dialog's "Save as".
    ///
    /// A running transfer is refused rather than renamed under itself: its
    /// segment writers hold the `.part` open at the old path.
    pub fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        let name = download::sanitize_filename(name)
            .ok_or_else(|| "That is not a usable filename.".to_string())?;

        let mut queue = self.queue.lock().unwrap();
        let entry = queue
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or_else(|| "That download is no longer in the list.".to_string())?;

        if entry.status == Status::Downloading {
            return Err("Pause the download before renaming it.".into());
        }

        // yt-dlp keeps its own part files named after the output template
        // (`<name>.f137.mp4.part`), which this rename does not know about. A
        // half-finished video would silently restart from zero, so refuse.
        if entry.plan.engine == download::Engine::YtDlp
            && entry.status != Status::Completed
            && entry.downloaded() > 0
        {
            return Err("A part-downloaded video cannot be renamed. Rename it once it finishes.".into());
        }

        let old_final = entry.plan.final_path.clone();
        let dir = old_final
            .parent()
            .ok_or_else(|| "That download has no folder.".to_string())?
            .to_path_buf();
        let name = download::keep_extension(&name, &entry.plan.filename());
        let new_final = dir.join(&name);
        if new_final == old_final {
            return Ok(());
        }

        // Refuse rather than silently pick "name (1)": the user asked for a
        // specific name, so a clash is worth reporting.
        let old_part = entry.plan.part_path.clone();
        let new_part = download::part_path(&new_final);
        if new_final.exists() || new_part.exists() {
            return Err(format!("“{name}” already exists in that folder."));
        }

        // Move whichever file is actually on disk. A queued download that has
        // not started has neither, and only the plan needs updating.
        let moved = if entry.status == Status::Completed { &old_final } else { &old_part };
        if moved.exists() {
            let to = if entry.status == Status::Completed { &new_final } else { &new_part };
            std::fs::rename(moved, to).map_err(|e| format!("cannot rename: {e}"))?;
        }

        entry.plan.final_path = new_final;
        entry.plan.part_path = new_part;
        // yt-dlp is told the name again on resume; an HTTP download reads it
        // from the plan.
        entry.name = Some(name);
        drop(queue);
        self.save_queue();
        Ok(())
    }

    /// Move an entry up (`delta` negative) or down the queue.
    ///
    /// The queue is a list and `claim_next` takes the first waiting entry in
    /// it, so position *is* priority — reordering the vector is the whole
    /// feature, no priority field needed. A large delta clamps to the end,
    /// which is how "move to top" is expressed.
    pub fn move_entry(&self, id: &str, delta: i32) {
        let mut queue = self.queue.lock().unwrap();
        let Some(from) = queue.iter().position(|d| d.id == id) else { return };
        let to = (from as i64 + delta as i64).clamp(0, queue.len() as i64 - 1) as usize;
        if to == from {
            return;
        }
        let entry = queue.remove(from);
        queue.insert(to, entry);
        drop(queue);
        self.save_queue();
    }

    /// True once nothing is running and nothing is waiting to run — the
    /// condition the "when everything is done" action fires on.
    pub fn all_done(&self) -> bool {
        self.queue
            .lock()
            .unwrap()
            .iter()
            .all(|d| d.is_terminal() || d.status == Status::Paused)
    }

    /// Ask a running transfer to stop, leaving the partial file in place.
    pub fn pause(&self, id: &str) {
        if let Some(active) = self.active.lock().unwrap().remove(id) {
            active.token.cancel();
            self.record_progress(id, &active.progress);
        }
        self.set_status(id, Status::Paused, None);
        self.save_queue();
    }

    /// Stop and discard: the partial file(s) are deleted and the entry removed.
    pub fn cancel(&self, id: &str) {
        let plan = {
            let queue = self.queue.lock().unwrap();
            queue.iter().find(|d| d.id == id).map(|d| d.plan.clone())
        };

        if let Some(active) = self.active.lock().unwrap().remove(id) {
            active.token.cancel();
        }
        self.queue.lock().unwrap().retain(|d| d.id != id);

        if let Some(plan) = plan {
            delete_artifacts(&plan, true);
        }
        self.save_queue();
    }

    pub fn resume(&self, id: &str) {
        self.set_status(id, Status::Queued, None);
        self.save_queue();
    }

    /// Drop an entry from the list. With `delete_file`, also erase the finished
    /// file (and any leftover `.part`) from disk; otherwise the file is left in
    /// place and only the list entry goes.
    pub fn remove(&self, id: &str, delete_file: bool) {
        let entry = {
            let queue = self.queue.lock().unwrap();
            queue.iter().find(|d| d.id == id).map(|d| (d.plan.clone(), d.status))
        };

        if let Some(active) = self.active.lock().unwrap().remove(id) {
            active.token.cancel();
        }
        self.queue.lock().unwrap().retain(|d| d.id != id);

        if let Some((plan, status)) = entry {
            if delete_file {
                delete_artifacts(&plan, false);
            } else if status != Status::Completed {
                // Keep a finished file, but a partial is useless once its queue
                // entry is gone — and `prepare` reserves the `.part` up front,
                // so even a never-started download has one to clean up.
                delete_artifacts(&plan, true);
            }
        }
        self.save_queue();
    }

    pub fn pause_all(&self) {
        let ids: Vec<String> = self
            .queue
            .lock()
            .unwrap()
            .iter()
            .filter(|d| matches!(d.status, Status::Downloading | Status::Queued | Status::Interrupted))
            .map(|d| d.id.clone())
            .collect();
        for id in ids {
            self.pause(&id);
        }
    }

    /// The file a completed download produced, if it is still there.
    /// `None` while it is unfinished, or once the file has been moved away.
    pub fn finished_file(&self, id: &str) -> Option<PathBuf> {
        let queue = self.queue.lock().unwrap();
        let entry = queue.iter().find(|d| d.id == id)?;
        if entry.status != Status::Completed {
            return None;
        }
        let path = entry.plan.final_path.clone();
        std::fs::metadata(&path).ok().filter(|m| m.is_file())?;
        Some(path)
    }

    /// Where poster frames are cached, beside the queue rather than in the
    /// user's downloads.
    pub fn thumb_dir(&self) -> PathBuf {
        self.data_dir.join("thumbs")
    }

    /// The partner to `pause_all`: put everything that stopped back in the
    /// queue. Completed entries are left alone — "resume" must never mean
    /// "download it again".
    pub fn resume_all(&self) {
        let ids: Vec<String> = self
            .queue
            .lock()
            .unwrap()
            .iter()
            .filter(|d| matches!(d.status, Status::Paused | Status::Interrupted | Status::Failed))
            .map(|d| d.id.clone())
            .collect();
        for id in ids {
            self.resume(&id);
        }
    }

    /// Clean shutdown on quit: cancel all active transfer tasks, persist their
    /// latest durable offsets to queue.json, and leave their entries in place so
    /// `reconcile_on_launch` marks them `Interrupted` and auto-resumes them next time.
    pub fn shutdown(&self) {
        let active_entries: Vec<(String, Active)> = {
            let mut active = self.active.lock().unwrap();
            active.drain().collect()
        };
        for (id, active) in active_entries {
            active.token.cancel();
            self.record_progress(&id, &active.progress);
        }
        self.dirty.store(false, std::sync::atomic::Ordering::Relaxed);
        self.save_queue();
    }

    pub fn clear_history(&self) {
        self.queue.lock().unwrap().retain(|d| !d.is_terminal());
        self.save_queue();
    }

    /// Atomically claim the next waiting entry, or `None` when the slot limit
    /// is reached or nothing is waiting. Marking the chosen entry `Downloading`
    /// happens under the same lock as the check, so two concurrent callers can
    /// never claim the same id. Running slots are counted by status, which
    /// already includes whatever was just claimed.
    fn claim_next(&self, max: usize) -> Option<Download> {
        let mut queue = self.queue.lock().unwrap();
        let running = queue.iter().filter(|d| d.status == Status::Downloading).count();
        if running >= max {
            return None;
        }
        let d = queue
            .iter_mut()
            .find(|d| matches!(d.status, Status::Queued | Status::Interrupted))?;
        d.status = Status::Downloading;
        d.error = None;
        Some(d.clone())
    }
}

/// Start whatever the concurrency limit allows, then emit a batched update.
///
/// This is the single place a transfer is spawned; every command just changes
/// status and calls it. `Interrupted` is included because that is exactly the
/// state an unclean shutdown leaves behind, and the plan says it auto-resumes.
pub fn pump(app: &AppHandle, state: &Arc<AppState>) {
    let settings = state.settings();
    let max = settings.max_concurrent.max(1);

    // The claim in `claim_next` marks each entry Downloading under the queue
    // lock, so a second pump racing this one (e.g. one from a paused task's
    // completion against one from `resume`) cannot pick the same id and spawn a
    // duplicate transfer onto the same `.part`.
    while let Some(entry) = state.claim_next(max) {
        spawn_transfer(app.clone(), Arc::clone(state), entry);
    }

    emit_queue(app, state);
}

fn spawn_transfer(app: AppHandle, state: Arc<AppState>, entry: Download) {
    let id = entry.id.clone();
    let token = CancellationToken::new();

    // Resume from the persisted per-segment offsets. The length check guards
    // against a queue.json written when the plan had a different shape.
    let progress = if entry.done.len() == entry.plan.segment_count() {
        Progress::resumed(&entry.done)
    } else {
        Progress::new(entry.plan.segment_count())
    };

    // If an entry for this id is somehow still active (a prior task not yet
    // torn down), do not start a second writer on the same file. The claim in
    // `pump` already marked it Downloading; leave that task to finish.
    let generation = state.gen.fetch_add(1, Ordering::Relaxed);
    {
        let mut active = state.active.lock().unwrap();
        if active.contains_key(&id) {
            return;
        }
        active.insert(
            id.clone(),
            Active { token: token.clone(), progress: progress.clone(), gen: generation },
        );
    }
    state.save_queue();
    emit_queue(&app, &state);

    // `tauri::async_runtime::spawn`, not `tokio::spawn`: this runs from sync
    // command handlers (pause/resume/cancel/retry) which are NOT on a Tokio
    // worker thread, so a bare `tokio::spawn` there panics "there is no reactor
    // running" and aborts the whole process. The async_runtime handle spawns
    // onto Tauri's runtime from any context.
    tauri::async_runtime::spawn(async move {
        let plan = entry.plan.clone();

        let result: Result<PathBuf, String> = if plan.engine == download::Engine::Torrent {
            run_torrent(&app, &state, &plan, &progress, &id, token.clone()).await
        } else if plan.engine == download::Engine::Ftp {
            run_ftp(&app, &state, &entry, &plan, &progress, &id, token.clone()).await
        } else if plan.engine == download::Engine::YtDlp {
            run_video(
                &app,
                &state,
                &plan,
                &progress,
                &id,
                entry.quality.clone(),
                entry.name.clone(),
                token.clone(),
            )
            .await
        } else {
            run_http(&app, &state, &entry, &plan, &progress, &id, token.clone()).await
        };

        // Only act if this task still owns the id. If it was paused and then
        // resumed as a fresh run, a newer task now owns `active[id]`, and this
        // stale task must not remove that entry, overwrite its progress, or
        // flip its status — doing so is what corrupted state on pause/resume.
        let owner = {
            let mut active = state.active.lock().unwrap();
            match active.get(&id) {
                Some(a) if a.gen == generation => {
                    active.remove(&id);
                    true
                }
                _ => false,
            }
        };
        if !owner {
            return;
        }

        state.record_progress(&id, &progress);

        match result {
            Ok(path) => {
                // yt-dlp only knows the real filename once it finishes.
                // Both of these only learn their real output path at the end:
                // yt-dlp picks the container, a torrent names itself from its
                // metadata.
                if matches!(
                    plan.engine,
                    download::Engine::YtDlp | download::Engine::Torrent
                ) {
                    state.set_final_path(&id, path.clone());
                }
                state.set_status(&id, Status::Completed, None);
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| entry.filename());
                notify_complete(&app, &name);
            }
            Err(e) if token.is_cancelled() => {
                // A cancelled transfer was paused or removed; whichever
                // command did it already set the right status.
                let _ = e;
            }
            Err(e) => state.set_status(&id, Status::Failed, Some(e)),
        }

        state.save_queue();
        emit_queue(&app, &state);

        // A finished transfer frees a slot.
        pump(&app, &state);
        finish_action(&app, &state);
    });
}

/// The HTTP-engine run: periodic checkpoint task, per-session clients, shared
/// throttle. Returns the final path (already known from the plan).
async fn run_http(
    app: &AppHandle,
    state: &Arc<AppState>,
    entry: &Download,
    plan: &download::DownloadPlan,
    progress: &Progress,
    id: &str,
    token: CancellationToken,
) -> Result<PathBuf, String> {
    // Persist offsets periodically so an unclean stop loses at most a few
    // seconds of progress rather than the whole transfer.
    let checkpoint = {
        let state = Arc::clone(state);
        let id = id.to_string();
        let progress = progress.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {
                        state.record_progress(&id, &progress);
                        state.mark_dirty();
                    }
                }
            }
        })
    };

    // Prefer the session captured for this entry (extension), falling back to
    // the manual cookies.txt.
    let session = state.session_for(&plan.url, entry.session.clone());
    let (client, segment_client) = match state.clients_for(&session) {
        Ok(pair) => pair,
        Err(e) => {
            checkpoint.abort();
            return Err(e);
        }
    };

    let throttle = state.current_throttle();
    let emitter = app.clone();
    let row_id = id.to_string();
    let result = download::run(
        &client,
        &segment_client,
        plan,
        progress,
        &throttle,
        token,
        move |downloaded, total| {
            let _ = emitter.emit(
                "download://progress",
                ProgressRow { id: row_id.clone(), downloaded, total },
            );
        },
    )
    .await;

    checkpoint.abort();
    result
}

/// The torrent run: librqbit owns the transfer, so this is a bridge between
/// its stats and the queue's counters.
async fn run_torrent(
    app: &AppHandle,
    state: &Arc<AppState>,
    plan: &download::DownloadPlan,
    progress: &Progress,
    id: &str,
    token: CancellationToken,
) -> Result<PathBuf, String> {
    let session = state.torrent_session(app).await?;

    let checkpoint = {
        let state = Arc::clone(state);
        let id = id.to_string();
        let progress = progress.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {
                        state.record_progress(&id, &progress);
                        state.mark_dirty();
                    }
                }
            }
        })
    };

    let emitter = app.clone();
    let row_id = id.to_string();
    let size_state = Arc::clone(state);
    let size_id = id.to_string();

    // A magnet's real name arrives with its metadata, well after the row does.
    let name_app = app.clone();
    let name_state = Arc::clone(state);
    let name_id = id.to_string();

    let result = crate::torrent::run(
        &session,
        plan,
        progress,
        token,
        move |downloaded, total| {
            if let Some(total) = total {
                size_state.set_total(&size_id, total);
            }
            let _ = emitter.emit(
                "download://progress",
                ProgressRow { id: row_id.clone(), downloaded, total },
            );
        },
        move |name| {
            name_state.set_display_name(&name_id, name);
            emit_queue(&name_app, &name_state);
        },
    )
    .await;

    checkpoint.abort();
    result
}

/// The FTP run: one connection, the same periodic checkpoint the HTTP engine
/// uses so a pause or a crash resumes from a real offset.
async fn run_ftp(
    app: &AppHandle,
    state: &Arc<AppState>,
    entry: &Download,
    plan: &download::DownloadPlan,
    progress: &Progress,
    id: &str,
    token: CancellationToken,
) -> Result<PathBuf, String> {
    let checkpoint = {
        let state = Arc::clone(state);
        let id = id.to_string();
        let progress = progress.clone();
        let token = token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => {
                        state.record_progress(&id, &progress);
                        state.mark_dirty();
                    }
                }
            }
        })
    };

    // Credentials reach the engine the same way the HTTP one gets cookies:
    // on the session stored with the entry.
    let session = state.session_for(&plan.url, entry.session.clone());
    let throttle = state.current_throttle();
    let emitter = app.clone();
    let row_id = id.to_string();

    let result = crate::ftp::run(
        plan,
        &session,
        progress,
        &throttle,
        token,
        move |downloaded, total| {
            let _ = emitter.emit(
                "download://progress",
                ProgressRow { id: row_id.clone(), downloaded, total },
            );
        },
    )
    .await;

    checkpoint.abort();
    result
}

/// The yt-dlp run: no checkpoint, no throttle, no segment clients. Cookies come
/// from the manual file or a configured browser.
#[allow(clippy::too_many_arguments)]
async fn run_video(
    app: &AppHandle,
    state: &Arc<AppState>,
    plan: &download::DownloadPlan,
    progress: &Progress,
    id: &str,
    quality_override: Option<String>,
    name_override: Option<String>,
    token: CancellationToken,
) -> Result<PathBuf, String> {
    let settings = state.settings();
    let ytdlp = if settings.ytdlp_path.is_empty() { "yt-dlp".to_string() } else { settings.ytdlp_path.clone() };
    let dir = plan
        .final_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let cookies = crate::ytdlp::Cookies {
        file: settings.cookies_file.clone(),
        browser: if settings.cookies_browser.is_empty() {
            None
        } else {
            Some(settings.cookies_browser.clone())
        },
    };

    // The add dialog's per-download choice wins over the global setting.
    let quality = quality_override
        .filter(|q| !q.is_empty())
        .unwrap_or_else(|| {
            if settings.video_quality.is_empty() {
                "best".to_string()
            } else {
                settings.video_quality.clone()
            }
        });

    let emitter = app.clone();
    let row_id = id.to_string();
    // Keep the shared progress and plan.total updated from yt-dlp ticks, so a
    // pause/interrupt records real bytes (not 0) and views() reflects progress
    // even when queue://changed fires mid-download.
    let prog = progress.clone();
    let prog_state = Arc::clone(state);
    let prog_id = id.to_string();

    // Update the row's title as soon as yt-dlp names a file.
    let name_app = app.clone();
    let name_state = Arc::clone(state);
    let name_id = id.to_string();
    let on_file = move |path: &std::path::Path| {
        if let Some(name) = display_name(path) {
            name_state.set_display_name(&name_id, &name);
            emit_queue(&name_app, &name_state);
        }
    };

    // Honour the global speed cap for video downloads too.
    let limit_kb = if settings.bandwidth_kb > 0 { Some(settings.bandwidth_kb) } else { None };

    // yt-dlp fetches a video stream and then an audio stream, restarting its
    // byte counter for each. Reported raw, the bar would run 0->100% twice.
    // Accumulate finished phases so the figures only ever move forward.
    //
    // On a resume yt-dlp's count already includes the bytes already on disk
    // (verified: it logs "Resuming download at byte N" and reports N+), so no
    // offset is added. But resuming *after* the video stream finished reports
    // only the audio stream, which is below what was persisted — so never
    // report less than the progress we started with.
    let floor = progress.total();
    let base_done = Arc::new(AtomicU64::new(0));
    let last_done = Arc::new(AtomicU64::new(0));
    let base_total = Arc::new(AtomicU64::new(0));
    let last_total = Arc::new(AtomicU64::new(0));

    crate::ytdlp::run(
        &ytdlp,
        &plan.url,
        &dir,
        &cookies,
        &quality,
        name_override.as_deref(),
        limit_kb,
        Some(settings.proxy.as_str()).filter(|p| !p.is_empty()),
        token,
        move |t| {
            // A drop in the reported byte count means yt-dlp moved on to the
            // next stream; bank the phase that just finished.
            let prev = last_done.swap(t.downloaded, Ordering::Relaxed);
            if t.downloaded < prev {
                base_done.fetch_add(prev, Ordering::Relaxed);
                base_total.fetch_add(last_total.load(Ordering::Relaxed), Ordering::Relaxed);
            }
            if let Some(total) = t.total {
                last_total.store(total, Ordering::Relaxed);
            }

            let downloaded = (base_done.load(Ordering::Relaxed) + t.downloaded).max(floor);
            let total = t
                .total
                .map(|tt| (base_total.load(Ordering::Relaxed) + tt).max(downloaded));

            prog.set_absolute(downloaded);
            if let Some(total) = total {
                prog_state.set_total(&prog_id, total);
            }
            let _ = emitter.emit(
                "download://progress",
                ProgressRow { id: row_id.clone(), downloaded, total },
            );
        },
        on_file,
    )
    .await
}

/// Carry out the "when everything is finished" setting, once everything
/// actually is. Called after each transfer ends, so the last one to finish is
/// the one that triggers it.
///
/// Quitting goes through `shutdown` for the same reason the close button does:
/// in-flight offsets have to be checkpointed. Nothing is in flight here by
/// definition, but the queue still needs its final write.
fn finish_action(app: &AppHandle, state: &Arc<AppState>) {
    let action = state.settings().on_all_done;
    if action.is_empty() || action == "none" || !state.all_done() {
        return;
    }

    state.shutdown();
    if action == "shutdown" {
        // Ask the session manager rather than the kernel: `systemctl poweroff`
        // is the one command that works without root on a systemd desktop.
        // A failure is reported and the app still quits — refusing to exit
        // because the machine would not power off helps no one.
        if let Err(e) = std::process::Command::new("systemctl").arg("poweroff").spawn() {
            eprintln!("spool: could not power the machine off: {e}");
        }
    }
    app.exit(0);
}

pub fn emit_queue(app: &AppHandle, state: &Arc<AppState>) {
    let _ = app.emit("queue://changed", state.views());
}

/// Delete a download's on-disk files. HTTP has a single `.part` (plus the final
/// file); yt-dlp leaves several intermediates (`.fNNN.<ext>`, `.part`, `.ytdl`)
/// which all share the resolved name stem, so those are cleaned by prefix.
/// `partial_only` (cancel of an unfinished download) skips the final file for
/// the HTTP path, where it does not exist yet.
fn delete_artifacts(plan: &download::DownloadPlan, partial_only: bool) {
    match plan.engine {
        download::Engine::YtDlp => cleanup_by_stem(&plan.final_path),
        // A torrent writes whatever its metadata described — one file or a
        // whole folder — under the output path, and librqbit keeps no separate
        // `.part`. An unfinished one has files worth removing too, so
        // `partial_only` does not spare it.
        download::Engine::Torrent => {
            if plan.final_path.is_dir() {
                let _ = std::fs::remove_dir_all(&plan.final_path);
            } else {
                let _ = std::fs::remove_file(&plan.final_path);
            }
        }
        // FTP writes the same single `.part` the HTTP engine does, so it is
        // cleaned the same way.
        download::Engine::Http | download::Engine::Ftp => {
            let _ = std::fs::remove_file(&plan.part_path);
            if !partial_only {
                let _ = std::fs::remove_file(&plan.final_path);
            }
        }
    }
}

/// Remove every file in `path`'s directory whose name starts with `path`'s file
/// stem — the set of yt-dlp intermediates for one download. Guarded so a short
/// or placeholder stem can't sweep unrelated files.
fn cleanup_by_stem(path: &std::path::Path) {
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem()) else {
        return;
    };
    let stem = stem.to_string_lossy();
    if stem.len() < 4 {
        let _ = std::fs::remove_file(path);
        return;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&*stem)
                && entry.path().is_file()
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Turn a yt-dlp intermediate path into a clean display name: drop a trailing
/// `.part`, and the per-stream `.fNNN` format tag (e.g.
/// `Title [id].f398.mp4.part` → `Title [id].mp4`).
fn display_name(path: &std::path::Path) -> Option<String> {
    let name = path.file_name()?.to_string_lossy();
    let name = name.strip_suffix(".part").unwrap_or(&name);
    let parts: Vec<&str> = name
        .split('.')
        .filter(|seg| !(seg.len() >= 2 && seg.starts_with('f') && seg[1..].chars().all(|c| c.is_ascii_digit())))
        .collect();
    let cleaned = parts.join(".");
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

#[cfg(test)]
mod display_tests {
    use super::display_name;
    use std::path::Path;

    #[test]
    fn cleans_ytdlp_intermediate_names() {
        assert_eq!(
            display_name(Path::new("/d/Big Buck Bunny [id].f398.mp4.part")).as_deref(),
            Some("Big Buck Bunny [id].mp4")
        );
        assert_eq!(
            display_name(Path::new("/d/Song [id].mp3")).as_deref(),
            Some("Song [id].mp3")
        );
        // No false-positive on a normal name component.
        assert_eq!(
            display_name(Path::new("/d/final.mkv")).as_deref(),
            Some("final.mkv")
        );
    }
}

/// Notify only when the window is hidden/minimized — a visible window already
/// shows the row flip to Completed, so a notification then would be noise.
fn notify_complete(app: &AppHandle, filename: &str) {
    let hidden = app
        .get_webview_window("main")
        .map(|w| !w.is_visible().unwrap_or(true))
        .unwrap_or(true);
    if !hidden {
        return;
    }
    let _ = app
        .notification()
        .builder()
        .title("Download complete")
        .body(format!("{filename} finished downloading."))
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::DownloadPlan;

    fn app() -> AppState {
        let dir = std::env::temp_dir().join(format!("spool-state-{}", uid()));
        std::fs::create_dir_all(&dir).unwrap();
        AppState::new(dir.clone(), dir).unwrap()
    }

    fn uid() -> u64 {
        use std::sync::atomic::AtomicU64;
        static N: AtomicU64 = AtomicU64::new(0);
        std::process::id() as u64 * 100_000 + N.fetch_add(1, Ordering::Relaxed)
    }

    fn push(state: &AppState, id: &str) {
        let plan = DownloadPlan {
            url: format!("https://example.com/{id}.bin"),
            final_path: format!("/tmp/{id}.bin").into(),
            part_path: format!("/tmp/{id}.bin.part").into(),
            total: Some(1000),
            supports_ranges: true,
            validator: None,
            ranges: vec![(0, 999)],
            engine: crate::download::Engine::Http,
            thumbnail: None,
        };
        state.queue.lock().unwrap().push(Download::new(id.into(), plan));
    }

    fn status_of(state: &AppState, id: &str) -> Status {
        state.queue.lock().unwrap().iter().find(|d| d.id == id).unwrap().status
    }

    /// A plan pointing at real files in `dir`, so the disk-touching paths can
    /// be exercised for real rather than mocked.
    fn plan_in(dir: &std::path::Path, filename: &str) -> DownloadPlan {
        DownloadPlan {
            url: format!("https://example.com/{filename}"),
            final_path: dir.join(filename),
            part_path: dir.join(format!("{filename}.part")),
            total: Some(1000),
            supports_ranges: true,
            validator: None,
            ranges: vec![(0, 999)],
            engine: crate::download::Engine::Http,
            thumbnail: None,
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spool-{name}-{}", uid()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // -- progress and name bookkeeping --------------------------------------

    /// yt-dlp reports a per-phase total: the video stream's size, then the
    /// (much smaller) audio stream's. Taking the latest would shrink the bar's
    /// denominator mid-download, so the recorded total only ever grows.
    #[test]
    fn total_only_ever_grows() {
        let state = app();
        push(&state, "t1");

        state.set_total("t1", 5_000);
        state.set_total("t1", 9_000);
        state.set_total("t1", 1_200); // audio phase: must be ignored
        assert_eq!(state.queue.lock().unwrap()[0].plan.total, Some(9_000));

        // An unknown id is a no-op, not a panic.
        state.set_total("nope", 1);
    }

    /// The display name changes while a yt-dlp download runs. It must replace
    /// only the file name — moving the file to a different folder mid-transfer
    /// would orphan the partial.
    #[test]
    fn display_name_keeps_the_directory() {
        let state = app();
        let dir = scratch("display");
        state.queue.lock().unwrap().push(Download::new("d1".into(), plan_in(&dir, "placeholder")));

        state.set_display_name("d1", "Real Title.mp4");
        let path = state.queue.lock().unwrap()[0].plan.final_path.clone();
        assert_eq!(path.parent().unwrap(), dir.as_path());
        assert_eq!(path.file_name().unwrap(), "Real Title.mp4");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `set_final_path` reads the size off disk. A directory's metadata length
    /// is the inode block size (4096 on ext4), so recording it would show a
    /// failed video download as a completed 4 KB file.
    #[test]
    fn final_path_records_a_size_only_for_a_real_file() {
        let state = app();
        let dir = scratch("finalpath");
        push(&state, "f1");

        // yt-dlp errored before naming an output: the path falls back to the
        // directory.
        state.set_final_path("f1", dir.clone());
        assert_eq!(state.queue.lock().unwrap()[0].plan.total, Some(1000), "unchanged");

        // A path that does not exist at all is equally not a size.
        state.set_final_path("f1", dir.join("ghost.mp4"));
        assert_eq!(state.queue.lock().unwrap()[0].plan.total, Some(1000));

        // A real file records its real length and marks the entry fully done.
        let real = dir.join("video.mp4");
        std::fs::write(&real, vec![7u8; 4242]).unwrap();
        state.set_final_path("f1", real.clone());
        {
            let queue = state.queue.lock().unwrap();
            assert_eq!(queue[0].plan.total, Some(4242));
            assert_eq!(queue[0].downloaded(), 4242, "a completed row must not read 0 / total");
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    // -- queue queries ------------------------------------------------------

    /// The duplicate warning is about work in flight. A finished download of
    /// the same URL is not a duplicate — asking for it again is a re-download.
    #[test]
    fn duplicate_check_ignores_finished_entries() {
        let state = app();
        push(&state, "u1");
        let url = state.queue.lock().unwrap()[0].url.clone();

        assert!(state.has_url(&url));
        state.set_status("u1", Status::Completed, None);
        assert!(!state.has_url(&url));
        state.set_status("u1", Status::Failed, None);
        assert!(!state.has_url(&url));
        state.set_status("u1", Status::Paused, None);
        assert!(state.has_url(&url), "a paused download is still in the queue");
    }

    #[test]
    fn clear_history_removes_only_finished_entries() {
        let state = app();
        for i in 0..5 {
            push(&state, &format!("c{i}"));
        }
        state.set_status("c0", Status::Completed, None);
        state.set_status("c1", Status::Failed, None);
        state.set_status("c2", Status::Paused, None);
        state.set_status("c3", Status::Downloading, None);
        // c4 stays Queued.

        state.clear_history();
        let left: Vec<String> = state.queue.lock().unwrap().iter().map(|d| d.id.clone()).collect();
        assert_eq!(left, vec!["c2", "c3", "c4"]);
    }

    #[test]
    fn pause_all_leaves_finished_entries_alone() {
        let state = app();
        for i in 0..4 {
            push(&state, &format!("p{i}"));
        }
        state.set_status("p0", Status::Downloading, None);
        state.set_status("p1", Status::Interrupted, None);
        state.set_status("p2", Status::Completed, None);
        state.set_status("p3", Status::Failed, None);

        state.pause_all();
        assert_eq!(status_of(&state, "p0"), Status::Paused);
        assert_eq!(status_of(&state, "p1"), Status::Paused);
        assert_eq!(status_of(&state, "p2"), Status::Completed);
        assert_eq!(status_of(&state, "p3"), Status::Failed);
    }

    /// The partner to pause_all. A finished download must not be re-queued:
    /// "resume all" would silently re-download the user's whole history.
    #[test]
    fn resume_all_requeues_only_what_stopped() {
        let state = app();
        for i in 0..5 {
            push(&state, &format!("r{i}"));
        }
        state.set_status("r0", Status::Paused, None);
        state.set_status("r1", Status::Interrupted, None);
        state.set_status("r2", Status::Failed, None);
        state.set_status("r3", Status::Completed, None);
        state.set_status("r4", Status::Downloading, None);

        state.resume_all();
        assert_eq!(status_of(&state, "r0"), Status::Queued);
        assert_eq!(status_of(&state, "r1"), Status::Queued);
        assert_eq!(status_of(&state, "r2"), Status::Queued);
        assert_eq!(status_of(&state, "r3"), Status::Completed, "a finished entry must not re-queue");
        assert_eq!(status_of(&state, "r4"), Status::Downloading, "a running entry is untouched");
    }

    #[test]
    fn views_report_what_the_row_shows() {
        let state = app();
        let dir = scratch("views");
        state.queue.lock().unwrap().push(Download::new("v1".into(), plan_in(&dir, "movie.mkv")));
        state.record_progress("v1", &Progress::resumed(&[400]));
        state.set_status("v1", Status::Paused, None);

        let views = state.views();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].filename, "movie.mkv");
        assert_eq!(views[0].downloaded, 400);
        assert_eq!(views[0].total, Some(1000));
        assert_eq!(views[0].status, Status::Paused);
        assert_eq!(views[0].engine, "http");
        assert!(!views[0].has_cookie, "a plain download carries no session");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ids_never_repeat() {
        let state = app();
        let ids: std::collections::HashSet<String> = (0..500).map(|_| state.next_id()).collect();
        assert_eq!(ids.len(), 500);
    }

    // -- parked extension requests ------------------------------------------

    #[test]
    fn pending_tokens_are_unique_and_single_use() {
        let state = app();
        let mk = |url: &str| PendingAdd {
            url: url.into(),
            session: None,
            force_video: false,
            added_at: crate::queue::now_secs(),
        };

        let a = state.stash_pending(mk("https://example.com/a"));
        let b = state.stash_pending(mk("https://example.com/b"));
        assert_ne!(a, b);

        assert_eq!(state.take_pending(&a).unwrap().url, "https://example.com/a");
        assert!(state.take_pending(&a).is_none(), "a token must not be redeemable twice");
        assert!(state.take_pending("never-issued").is_none());
        assert!(state.take_pending(&b).is_some());
    }

    /// A dialog closed by shutting the window never answers. Without the sweep
    /// the map would grow for the life of the process.
    #[test]
    fn pending_requests_expire() {
        let state = app();
        let now = crate::queue::now_secs();

        let stale = state.stash_pending(PendingAdd {
            url: "https://example.com/old".into(),
            session: None,
            force_video: false,
            added_at: now - PENDING_TTL_SECS - 1,
        });
        let fresh = state.stash_pending(PendingAdd {
            url: "https://example.com/new".into(),
            session: None,
            force_video: false,
            added_at: now,
        });

        // The sweep runs on the next stash, so the stale entry is gone by now.
        assert!(state.take_pending(&stale).is_none());
        assert!(state.take_pending(&fresh).is_some());
    }

    // -- session precedence -------------------------------------------------

    /// A session the extension captured wins over the cookies file: it carries
    /// the live cookie that just cleared a challenge. The proxy is the one
    /// thing the extension cannot know, so it is always taken from settings.
    #[tokio::test]
    async fn captured_session_wins_and_still_gets_the_proxy() {
        let state = app();
        let dir = scratch("session");
        let jar = dir.join("cookies.txt");
        std::fs::write(&jar, ".example.com\tTRUE\t/\tFALSE\t2000000000\tfromfile\tv\n").unwrap();

        state.set_settings(Settings {
            cookies_file: Some(jar.clone()),
            proxy: "http://127.0.0.1:8080".into(),
            ..Settings::default()
        });

        let captured = Session {
            user_agent: "BrowserAgent/1.0".into(),
            cookie: Some("cf_clearance=live".into()),
            referer: Some("https://example.com/page".into()),
            proxy: None,
            auth: None,
        };
        let session = state.session_for("https://example.com/a.zip", Some(captured));
        assert_eq!(session.cookie.as_deref(), Some("cf_clearance=live"), "the file must not win");
        assert_eq!(session.user_agent, "BrowserAgent/1.0");
        assert_eq!(session.proxy.as_deref(), Some("http://127.0.0.1:8080"));

        // With nothing captured, the cookies file is matched against the URL.
        let manual = state.session_for("https://example.com/a.zip", None);
        assert_eq!(manual.cookie.as_deref(), Some("fromfile=v"));
        assert_eq!(manual.proxy.as_deref(), Some("http://127.0.0.1:8080"));

        // A URL the file has no cookies for gets none.
        let other = state.session_for("https://elsewhere.test/a.zip", None);
        assert!(other.cookie.is_none());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn an_empty_proxy_setting_means_direct() {
        let state = app();
        state.set_settings(Settings { proxy: String::new(), ..Settings::default() });
        assert!(state.session_for("https://example.com/a", None).proxy.is_none());
        assert!(
            state
                .session_for("https://example.com/a", Some(Session::default()))
                .proxy
                .is_none()
        );
    }

    /// A cookies file that does not exist, or is unreadable, must leave the
    /// download running without cookies rather than failing the add.
    #[tokio::test]
    async fn a_missing_cookies_file_is_not_fatal() {
        let state = app();
        state.set_settings(Settings {
            cookies_file: Some("/nonexistent/cookies.txt".into()),
            ..Settings::default()
        });
        let session = state.session_for("https://example.com/a.zip", None);
        assert!(session.cookie.is_none());
        assert_eq!(session.user_agent, crate::download::USER_AGENT);
    }

    // -- settings -----------------------------------------------------------

    /// The startup settings are plain booleans, but they gained defaults after
    /// the file format already existed — so a settings.json written before
    /// them must still load, and must not silently turn tray-running off.
    #[test]
    fn settings_from_an_older_build_keep_running_in_the_tray() {
        let legacy = r#"{
            "download_dir": null,
            "max_concurrent": 3,
            "segments": 4,
            "theme": "dark",
            "cookies_file": null,
            "user_agent": null
        }"#;
        let s: Settings = serde_json::from_str(legacy).expect("an older settings file must load");
        assert_eq!(s.max_concurrent, 3);
        assert_eq!(s.theme, "dark");

        // The point of the test: a bool's serde default is `false`, which
        // would have made the close button quit for every existing user.
        assert!(s.run_in_background, "an absent field must not change how close behaves");
        assert!(s.start_minimised, "an absent field must not change how a login launch behaves");
        assert!(s.start_on_login, "an absent field must not disable autostart");
    }

    /// Settings arrive over IPC. An out-of-range concurrency would have `pump`
    /// spawn that many transfers at once, and 0 would stall the queue entirely.
    #[tokio::test]
    async fn settings_are_clamped_not_trusted() {
        let state = app();

        state.set_settings(Settings { max_concurrent: 0, segments: 0, ..Settings::default() });
        assert_eq!(state.settings().max_concurrent, 1);
        assert_eq!(state.settings().segments, 1);

        state.set_settings(Settings { max_concurrent: 9999, segments: 9999, ..Settings::default() });
        assert_eq!(state.settings().max_concurrent, 16);
        assert_eq!(state.settings().segments, crate::download::MAX_SEGMENTS);

        // A value already in range is left alone.
        state.set_settings(Settings { max_concurrent: 3, segments: 4, ..Settings::default() });
        assert_eq!(state.settings().max_concurrent, 3);
        assert_eq!(state.settings().segments, 4);
    }

    #[test]
    fn dirty_flag_flushes_once_then_stays_quiet() {
        let state = app();
        push(&state, "f1");

        // Nothing marked: no write is owed.
        assert!(!state.dirty.load(Ordering::Relaxed));
        state.mark_dirty();
        assert!(state.dirty.load(Ordering::Relaxed));

        state.flush_if_dirty();
        assert!(!state.dirty.load(Ordering::Relaxed), "the flag is consumed by the flush");

        // A second flush with nothing dirty must not write again — that is the
        // whole point of the flag: N downloads cost one write per tick, not N.
        state.flush_if_dirty();
        assert!(!state.dirty.load(Ordering::Relaxed));
    }

    // -- deleting files -----------------------------------------------------

    #[test]
    fn artifacts_deletion_respects_partial_only() {
        let dir = scratch("artifacts");
        let plan = plan_in(&dir, "movie.mkv");
        std::fs::write(&plan.final_path, b"done").unwrap();
        std::fs::write(&plan.part_path, b"partial").unwrap();

        // "Remove from list" on an unfinished download: the partial goes, the
        // finished file (if any) stays.
        delete_artifacts(&plan, true);
        assert!(!plan.part_path.exists());
        assert!(plan.final_path.exists());

        // "Delete file" takes both.
        std::fs::write(&plan.part_path, b"partial").unwrap();
        delete_artifacts(&plan, false);
        assert!(!plan.part_path.exists());
        assert!(!plan.final_path.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// yt-dlp leaves a spread of intermediates (`.f137.mp4`, `.f251.webm`,
    /// `.part`, `.ytdl`), so they are swept by stem.
    #[test]
    fn ytdlp_artifacts_are_swept_by_stem() {
        let dir = scratch("ytdlp-sweep");
        let mut plan = plan_in(&dir, "Big Buck Bunny [abc123].mp4");
        plan.engine = crate::download::Engine::YtDlp;

        for name in [
            "Big Buck Bunny [abc123].mp4",
            "Big Buck Bunny [abc123].f137.mp4.part",
            "Big Buck Bunny [abc123].f251.webm",
            "Big Buck Bunny [abc123].mp4.ytdl",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        // An unrelated download in the same folder must survive.
        std::fs::write(dir.join("Someone Else.mp4"), b"x").unwrap();
        std::fs::create_dir(dir.join("Big Buck Bunny [abc123] subdir")).unwrap();

        delete_artifacts(&plan, true);

        assert!(dir.join("Someone Else.mp4").exists(), "swept an unrelated file");
        assert!(
            dir.join("Big Buck Bunny [abc123] subdir").exists(),
            "the sweep must not remove directories"
        );
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("Big Buck Bunny") && !n.ends_with("subdir"))
            .collect();
        assert!(left.is_empty(), "intermediates left behind: {left:?}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A placeholder name like "vid" would match half the folder, so a short
    /// stem deletes only the exact path.
    #[test]
    fn short_stems_do_not_sweep_the_folder() {
        let dir = scratch("short-stem");
        std::fs::write(dir.join("ab"), b"x").unwrap();
        std::fs::write(dir.join("ab.f137.mp4"), b"x").unwrap();
        std::fs::write(dir.join("abcdef.mp4"), b"x").unwrap();

        cleanup_by_stem(&dir.join("ab"));

        assert!(!dir.join("ab").exists(), "the exact path still goes");
        assert!(dir.join("ab.f137.mp4").exists(), "a 2-char stem must not sweep");
        assert!(dir.join("abcdef.mp4").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A bulk action must not act on entries the action makes no sense for: a
    /// finished download is neither paused nor re-queued by a mixed selection.
    #[test]
    fn bulk_skips_entries_the_action_does_not_apply_to() {
        let state = app();
        for i in 0..4 {
            push(&state, &format!("b{i}"));
        }
        state.set_status("b0", Status::Downloading, None);
        state.set_status("b1", Status::Completed, None);
        state.set_status("b2", Status::Paused, None);
        state.set_status("b3", Status::Failed, None);

        let all: Vec<String> = (0..4).map(|i| format!("b{i}")).collect();

        state.bulk(&all, BulkAction::Pause);
        assert_eq!(status_of(&state, "b0"), Status::Paused, "a running entry pauses");
        assert_eq!(status_of(&state, "b1"), Status::Completed, "a finished entry must not pause");
        assert_eq!(status_of(&state, "b3"), Status::Failed, "a failed entry must not pause");

        state.bulk(&all, BulkAction::Resume);
        assert_eq!(status_of(&state, "b0"), Status::Queued, "a paused entry resumes");
        assert_eq!(status_of(&state, "b2"), Status::Queued);
        assert_eq!(status_of(&state, "b3"), Status::Queued, "a failed entry retries");
        assert_eq!(status_of(&state, "b1"), Status::Completed, "a finished entry must not re-queue");

        // An id that has already left the queue is skipped, not a panic.
        state.bulk(&["gone".to_string(), "b2".to_string()], BulkAction::Remove);
        let left: Vec<String> = state.queue.lock().unwrap().iter().map(|d| d.id.clone()).collect();
        assert_eq!(left, vec!["b0", "b1", "b3"]);
    }

    /// Renaming moves the real file, keeps the extension when none is typed,
    /// refuses a collision, and refuses a running transfer.
    #[test]
    fn rename_moves_the_file_on_disk() {
        let state = app();
        let dir = std::env::temp_dir().join(format!("spool-rename-{}", uid()));
        std::fs::create_dir_all(&dir).unwrap();

        let plan = DownloadPlan {
            url: "https://example.com/invoice.pdf".into(),
            final_path: dir.join("invoice.pdf"),
            part_path: dir.join("invoice.pdf.part"),
            total: Some(4),
            supports_ranges: true,
            validator: None,
            ranges: vec![(0, 3)],
            engine: crate::download::Engine::Http,
            thumbnail: None,
        };
        std::fs::write(&plan.part_path, b"abcd").unwrap();
        state.queue.lock().unwrap().push(Download::new("r1".into(), plan.clone()));
        state.set_status("r1", Status::Paused, None);

        // No extension typed: the source's is kept, and the *partial* moves
        // because the download is unfinished.
        state.rename("r1", "report").unwrap();
        assert!(dir.join("report.pdf.part").exists());
        assert!(!dir.join("invoice.pdf.part").exists());
        assert_eq!(
            state.queue.lock().unwrap()[0].plan.final_path,
            dir.join("report.pdf")
        );

        // A name that already exists is refused, and nothing moves.
        std::fs::write(dir.join("taken.pdf"), b"x").unwrap();
        let err = state.rename("r1", "taken.pdf").unwrap_err();
        assert!(err.contains("already exists"), "unexpected error: {err}");
        assert!(dir.join("report.pdf.part").exists());

        // A completed entry moves the finished file instead of the partial.
        std::fs::rename(dir.join("report.pdf.part"), dir.join("report.pdf")).unwrap();
        state.set_status("r1", Status::Completed, None);
        state.rename("r1", "final.pdf").unwrap();
        assert!(dir.join("final.pdf").exists());
        assert!(!dir.join("report.pdf").exists());

        // A running transfer holds the file open; renaming is refused.
        state.set_status("r1", Status::Downloading, None);
        assert!(state.rename("r1", "nope.pdf").is_err());

        // A half-finished video is refused: yt-dlp's own part files are named
        // after the old output template and would be abandoned.
        {
            let mut queue = state.queue.lock().unwrap();
            let e = queue.iter_mut().find(|d| d.id == "r1").unwrap();
            e.plan.engine = crate::download::Engine::YtDlp;
            e.status = Status::Paused;
            e.done = vec![512];
        }
        assert!(state.rename("r1", "half.mp4").is_err());
        {
            let mut queue = state.queue.lock().unwrap();
            let e = queue.iter_mut().find(|d| d.id == "r1").unwrap();
            e.plan.engine = crate::download::Engine::Http;
            e.done = vec![0];
        }

        // A path in the name cannot escape the download folder.
        state.set_status("r1", Status::Paused, None);
        state.rename("r1", "../escaped.pdf").unwrap();
        assert_eq!(
            state.queue.lock().unwrap()[0].plan.final_path,
            dir.join("escaped.pdf")
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The core of the pause/resume ANR fix: claiming is atomic and never hands
    /// the same id to two callers, and it stops at the concurrency limit.
    #[test]
    fn claim_is_atomic_and_bounded() {
        let state = app();
        for i in 0..5 {
            push(&state, &format!("d{i}"));
        }

        // With max = 2, only two entries may be claimed; the rest stay Queued.
        let a = state.claim_next(2).unwrap();
        let b = state.claim_next(2).unwrap();
        assert_ne!(a.id, b.id, "claim handed out the same id twice");
        assert!(state.claim_next(2).is_none(), "claim exceeded the slot limit");

        assert_eq!(status_of(&state, &a.id), Status::Downloading);
        assert_eq!(status_of(&state, &b.id), Status::Downloading);

        // A freed slot lets exactly one more through.
        state.set_status(&a.id, Status::Completed, None);
        let c = state.claim_next(2).unwrap();
        assert_ne!(c.id, b.id);
        assert!(state.claim_next(2).is_none());
    }

    /// A repeated claim of a single entry (what two racing pumps would attempt)
    /// yields it exactly once.
    #[test]
    fn a_single_entry_is_claimed_once() {
        let state = app();
        push(&state, "only");
        assert_eq!(state.claim_next(3).unwrap().id, "only");
        assert!(state.claim_next(3).is_none(), "same entry claimed twice");
    }

    #[test]
    fn interrupted_is_claimable_paused_is_not() {
        let state = app();
        push(&state, "x");
        push(&state, "y");
        state.set_status("x", Status::Interrupted, None);
        state.set_status("y", Status::Paused, None);

        // Interrupted auto-resumes; Paused is a user decision and must not.
        let claimed = state.claim_next(5).unwrap();
        assert_eq!(claimed.id, "x");
        assert!(state.claim_next(5).is_none());
        assert_eq!(status_of(&state, "y"), Status::Paused);
    }

    /// The add dialog's payload must deserialize exactly as the frontend sends
    /// it; a field-name mismatch here would silently drop the chosen folder.
    #[test]
    fn add_options_deserialize_from_dialog_payload() {
        let full: AddOptions = serde_json::from_str(
            r#"{"dir":"/tmp/x","name":"report.pdf","quality":"1080","start":false}"#,
        )
        .unwrap();
        assert_eq!(full.dir.as_deref(), Some("/tmp/x"));
        assert_eq!(full.name.as_deref(), Some("report.pdf"));
        assert_eq!(full.quality.as_deref(), Some("1080"));
        assert_eq!(full.start, Some(false));

        // "Use setting" sends nulls; everything falls back to the defaults.
        let nulls: AddOptions =
            serde_json::from_str(r#"{"dir":null,"name":null,"quality":null,"start":true}"#).unwrap();
        assert!(nulls.dir.is_none() && nulls.name.is_none() && nulls.quality.is_none());
        assert_eq!(nulls.start, Some(true));

        // Omitted entirely (extension bridge path).
        let empty: AddOptions = serde_json::from_str("{}").unwrap();
        assert!(empty.dir.is_none() && empty.name.is_none() && empty.quality.is_none());
        assert!(empty.start.is_none());
        // Absent `start` must mean "start now".
        assert!(empty.start != Some(false));
    }

    /// The frontend sends the action as a snake_case string. A rename on
    /// either side would turn every bulk button into a silent no-op.
    #[test]
    fn bulk_action_deserializes_from_the_ui_payload() {
        for (wire, expect) in [
            ("\"pause\"", BulkAction::Pause),
            ("\"resume\"", BulkAction::Resume),
            ("\"remove\"", BulkAction::Remove),
            ("\"remove_with_file\"", BulkAction::RemoveWithFile),
        ] {
            let parsed: BulkAction = serde_json::from_str(wire).expect(wire);
            assert_eq!(
                std::mem::discriminant(&parsed),
                std::mem::discriminant(&expect),
                "{wire}"
            );
        }
        assert!(serde_json::from_str::<BulkAction>("\"removeWithFile\"").is_err());
        assert!(serde_json::from_str::<BulkAction>("\"delete\"").is_err());
    }

    #[test]
    fn bulk_on_an_empty_selection_is_a_no_op() {
        let state = app();
        push(&state, "e1");
        state.bulk(&[], BulkAction::RemoveWithFile);
        assert_eq!(state.queue.lock().unwrap().len(), 1);
    }

    #[test]
    fn remove_keeps_or_deletes_file() {
        let state = app();
        let dir = std::env::temp_dir().join(format!("spool-rm-{}", uid()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("keep.bin");
        std::fs::write(&file, b"data").unwrap();

        let plan = DownloadPlan {
            url: "https://example.com/keep.bin".into(),
            final_path: file.clone(),
            part_path: dir.join("keep.bin.part"),
            total: Some(4),
            supports_ranges: false,
            validator: None,
            ranges: vec![],
            engine: crate::download::Engine::Http,
            thumbnail: None,
        };
        state.queue.lock().unwrap().push(Download::new("k".into(), plan));

        // Remove from list keeps the finished file but discards the partial:
        // `prepare` reserves the `.part`, so it would otherwise be orphaned.
        let part = dir.join("keep.bin.part");
        std::fs::write(&part, b"partial").unwrap();
        state.remove("k", false);
        assert!(file.exists(), "remove without delete must keep the file");
        assert!(!part.exists(), "remove must not orphan the .part");
        assert!(state.queue.lock().unwrap().is_empty());

        // Re-add and remove with delete.
        let plan2 = DownloadPlan {
            url: "https://example.com/keep.bin".into(),
            final_path: file.clone(),
            part_path: dir.join("keep.bin.part"),
            total: Some(4),
            supports_ranges: false,
            validator: None,
            ranges: vec![],
            engine: crate::download::Engine::Http,
            thumbnail: None,
        };
        state.queue.lock().unwrap().push(Download::new("k2".into(), plan2));
        state.remove("k2", true);
        assert!(!file.exists(), "remove with delete must erase the file");

        std::fs::remove_dir_all(&dir).ok();
    }
}
