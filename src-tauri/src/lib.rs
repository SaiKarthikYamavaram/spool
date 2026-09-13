mod cookies;
mod download;
mod ftp;
mod queue;
mod server;
mod spider;
mod state;
mod thumbs;
mod torrent;
mod throttle;
mod ytdlp;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State, WindowEvent,
};

use state::{AddOptions, AppState, DownloadView, Settings};

/// Bring the main window back from the tray and focus it.
pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

type Shared<'a> = State<'a, Arc<AppState>>;

#[tauri::command]
async fn add_download(
    app: AppHandle,
    state: Shared<'_>,
    url: String,
    options: Option<AddOptions>,
) -> Result<String, String> {
    let opts = options.unwrap_or_default();
    let start = opts.start != Some(false);
    let id = state.add_with_session(&app, &url, None, false, opts).await?;
    // "Download later" was already parked as Paused; don't start it.
    if start {
        state::pump(&app, &state);
    } else {
        state::emit_queue(&app, &state);
    }
    Ok(id)
}

/// Confirm a request the extension parked: add it with the chosen options,
/// replaying the browser session captured at capture time.
#[tauri::command]
async fn add_pending(
    app: AppHandle,
    state: Shared<'_>,
    token: String,
    options: Option<AddOptions>,
) -> Result<String, String> {
    let pending = state
        .take_pending(&token)
        .ok_or_else(|| "this request expired".to_string())?;
    let opts = options.unwrap_or_default();
    let start = opts.start != Some(false);
    let id = state
        .add_with_session(&app, &pending.url, pending.session, pending.force_video, opts)
        .await?;
    if start {
        state::pump(&app, &state);
    } else {
        state::emit_queue(&app, &state);
    }
    Ok(id)
}

/// Drop a parked request the user cancelled.
#[tauri::command]
fn cancel_pending(state: Shared<'_>, token: String) {
    state.take_pending(&token);
}

/// The folder a download would land in by default, for the add dialog to show.
#[tauri::command]
fn get_download_dir(app: AppHandle, state: Shared<'_>) -> Result<String, String> {
    state.download_dir(&app).map(|p| p.display().to_string())
}

/// Returns the URLs that were skipped, so the UI can say what did not make it
/// rather than silently dropping lines.
#[tauri::command]
async fn import_urls(
    app: AppHandle,
    state: Shared<'_>,
    text: String,
) -> Result<Vec<String>, String> {
    let mut skipped = Vec::new();

    for line in text.lines() {
        let url = line.trim();
        if url.is_empty() || url.starts_with('#') {
            continue;
        }
        if let Err(e) = state.add(&app, url).await {
            skipped.push(format!("{url}: {e}"));
        }
    }

    state::pump(&app, &state);
    Ok(skipped)
}

/// Walk a page for downloadable links and return what it found, without
/// queuing anything. The user confirms the list in the add dialog — a spider
/// that queued its own results would be a very fast way to fill a disk.
#[tauri::command]
async fn grab_links(
    state: Shared<'_>,
    url: String,
    depth: u32,
    filter: String,
) -> Result<Vec<String>, String> {
    let session = state.session_for(&url, None);
    let client = download::build_client(&session)?;
    // Two levels is already MAX_PAGES fetches; deeper is a web crawler, not a
    // download manager.
    spider::crawl(&client, &url, depth.min(2), &filter).await
}

#[tauri::command]
fn is_duplicate(state: Shared<'_>, url: String) -> bool {
    state.has_url(&url)
}

#[tauri::command]
fn pause_download(app: AppHandle, state: Shared<'_>, id: String) {
    state.pause(&id);
    state::pump(&app, &state);
}

#[tauri::command]
fn resume_download(app: AppHandle, state: Shared<'_>, id: String) {
    state.resume(&id);
    state::pump(&app, &state);
}

#[tauri::command]
fn cancel_download(app: AppHandle, state: Shared<'_>, id: String) {
    state.cancel(&id);
    state::pump(&app, &state);
}

/// Retry is resume: the saved offsets still apply, so a failed transfer picks
/// up where it stopped instead of starting over.
#[tauri::command]
fn retry_download(app: AppHandle, state: Shared<'_>, id: String) {
    state.resume(&id);
    state::pump(&app, &state);
}

/// Remove an entry. `delete_file` also erases the file from disk.
#[tauri::command]
fn remove_download(app: AppHandle, state: Shared<'_>, id: String, delete_file: bool) {
    state.remove(&id, delete_file);
    state::emit_queue(&app, &state);
}

#[tauri::command]
fn rename_download(app: AppHandle, state: Shared<'_>, id: String, name: String) -> Result<(), String> {
    state.rename(&id, &name)?;
    state::emit_queue(&app, &state);
    Ok(())
}

/// Move an entry through the queue. Negative moves it up; a large magnitude
/// clamps to an end, which is how the menu spells "to the top".
#[tauri::command]
fn move_download(app: AppHandle, state: Shared<'_>, id: String, delta: i32) {
    state.move_entry(&id, delta);
    // Order is priority, so a move can change what the free slots should be
    // running — not just what the list looks like.
    state::pump(&app, &state);
}

/// Hash a finished file so it can be checked against a published checksum.
///
/// Computed on demand rather than during the transfer: most downloads are
/// never verified, and hashing every one of them would spend CPU and a second
/// full read of the file for nothing.
#[tauri::command]
async fn hash_file(state: Shared<'_>, id: String, algo: String) -> Result<String, String> {
    use tokio::io::AsyncReadExt;

    let path = state
        .finished_file(&id)
        .ok_or_else(|| "That download has no finished file to hash.".to_string())?;

    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    // Streamed in chunks: a download manager's files are exactly the ones too
    // big to hold in memory.
    let mut buf = vec![0u8; 1024 * 1024];
    let mut sha = <sha2::Sha256 as sha2::Digest>::new();
    let mut md5 = <md5::Md5 as md5::Digest>::new();
    let want_sha = algo != "md5";

    loop {
        let n = file
            .read(&mut buf)
            .await
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        if want_sha {
            sha2::Digest::update(&mut sha, &buf[..n]);
        } else {
            md5::Digest::update(&mut md5, &buf[..n]);
        }
        // Hashing a multi-GB file would otherwise hold the runtime thread for
        // its whole read.
        tokio::task::yield_now().await;
    }

    let digest = if want_sha {
        hex(&sha2::Digest::finalize(sha))
    } else {
        hex(&md5::Digest::finalize(md5))
    };
    Ok(digest)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tauri::command]
fn bulk_action(app: AppHandle, state: Shared<'_>, ids: Vec<String>, action: state::BulkAction) {
    state.bulk(&ids, action);
    // `pump` starts anything the removals freed a slot for, and emits.
    state::pump(&app, &state);
}

#[tauri::command]
fn pause_all(app: AppHandle, state: Shared<'_>) {
    state.pause_all();
    state::emit_queue(&app, &state);
}

/// A poster frame for a finished video, as a `data:` URI.
///
/// Returned inline rather than as a file path: the webview cannot read an
/// arbitrary local file without opening the asset protocol to the whole disk,
/// and a 320px JPEG is a few KB. `None` covers everything that is not a video,
/// not finished, or that ffmpeg could not read — the row falls back to its
/// type icon, which is a perfectly good answer.
#[tauri::command]
async fn video_thumbnail(state: Shared<'_>, id: String) -> Result<Option<String>, String> {
    let Some(path) = state.finished_file(&id) else {
        return Ok(None);
    };
    if !thumbs::is_video_file(&path) {
        return Ok(None);
    }

    let cached = thumbs::cache_path(&state.thumb_dir(), &id);
    // Re-extracting on every render would launch a process per row per paint.
    if tokio::fs::metadata(&cached).await.map(|m| m.len() > 0).unwrap_or(false) {
        return Ok(encode_thumb(&cached).await);
    }

    let secs = thumbs::duration("ffprobe", &path).await;
    match thumbs::extract("ffmpeg", &path, &cached, secs).await {
        Some(_) => Ok(encode_thumb(&cached).await),
        None => Ok(None),
    }
}

async fn encode_thumb(path: &std::path::Path) -> Option<String> {
    use base64::Engine;
    let bytes = tokio::fs::read(path).await.ok()?;
    Some(format!(
        "data:image/jpeg;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Minutes since local midnight, for the schedule window.
///
/// `libc::localtime_r` rather than a date crate: the only question being asked
/// is what the wall clock says, and libc is already a dependency. It is also
/// the only thing that knows the machine's timezone and its DST rules.
fn local_minutes() -> u32 {
    // SAFETY: `localtime_r` writes into a caller-owned `tm` and, unlike
    // `localtime`, touches no shared static, so it is safe to call from any
    // thread. The pointers are both valid for the call.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return 0;
        }
        (tm.tm_hour.clamp(0, 23) as u32) * 60 + tm.tm_min.clamp(0, 59) as u32
    }
}

/// Parse a `HH:MM` bound into minutes since midnight.
fn parse_hhmm(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// Whether `now` falls in `[start, stop)`, wrapping over midnight when the
/// start is later than the stop — which is the normal case for an off-peak
/// window like 22:00 to 06:00. `None` when either bound will not parse.
fn in_window(now: u32, start: &str, stop: &str) -> Option<bool> {
    let start = parse_hhmm(start)?;
    let stop = parse_hhmm(stop)?;
    // Equal bounds would otherwise mean "never", which no one sets on purpose.
    if start == stop {
        return Some(true);
    }
    Some(if start < stop {
        now >= start && now < stop
    } else {
        now >= start || now < stop
    })
}

#[cfg(test)]
mod schedule_tests {
    use super::*;

    #[test]
    fn bounds_parse_or_are_refused() {
        assert_eq!(parse_hhmm("22:00"), Some(22 * 60));
        assert_eq!(parse_hhmm(" 6:05 "), Some(6 * 60 + 5));
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("24:00"), None, "hour 24 is not a time of day");
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("noon"), None);
        assert_eq!(parse_hhmm(""), None);
    }

    #[test]
    fn a_daytime_window_contains_only_its_own_hours() {
        let (start, stop) = ("09:00", "17:00");
        assert_eq!(in_window(9 * 60, start, stop), Some(true), "the start is inside");
        assert_eq!(in_window(12 * 60, start, stop), Some(true));
        assert_eq!(in_window(17 * 60, start, stop), Some(false), "the stop is outside");
        assert_eq!(in_window(8 * 60 + 59, start, stop), Some(false));
        assert_eq!(in_window(2 * 60, start, stop), Some(false));
    }

    /// The case the feature exists for: downloading overnight.
    #[test]
    fn an_overnight_window_wraps_past_midnight() {
        let (start, stop) = ("22:00", "06:00");
        assert_eq!(in_window(23 * 60, start, stop), Some(true));
        assert_eq!(in_window(0, start, stop), Some(true), "midnight is inside");
        assert_eq!(in_window(5 * 60 + 59, start, stop), Some(true));
        assert_eq!(in_window(6 * 60, start, stop), Some(false));
        assert_eq!(in_window(12 * 60, start, stop), Some(false));
    }

    /// Equal bounds read as "all day". The alternative is a window that is
    /// never open, which pauses every download forever and looks like a bug.
    #[test]
    fn equal_bounds_mean_always() {
        assert_eq!(in_window(3 * 60, "08:00", "08:00"), Some(true));
    }

    /// Unparseable bounds must not resolve to "closed": the caller does
    /// nothing on `None`, rather than pausing everything the user has.
    #[test]
    fn junk_bounds_decide_nothing() {
        assert_eq!(in_window(60, "", "06:00"), None);
        assert_eq!(in_window(60, "22:00", "later"), None);
    }

    #[test]
    fn the_local_clock_is_a_time_of_day() {
        assert!(local_minutes() < 24 * 60);
    }
}

/// The bundle identifier this app used before it was renamed to `spool`.
const LEGACY_ID: &str = "com.saikarthik.fetchd";

/// Carry one file or directory across from the pre-rename install.
///
/// Tauri derives the data and config directories from the bundle identifier, so
/// renaming the app moved them — leaving an existing install's queue, settings
/// and thumbnail cache at a path nothing will ever read again.
///
/// Moving the whole directory looks tidier and does not work: WebKitGTK creates
/// its own subdirectories inside the data directory before `setup` runs, so by
/// the time this could rename it the destination already exists. Only the
/// entries this app owns are moved, and only when nothing is at the new path —
/// overwriting a live queue with the pre-rename one is the single thing this
/// must never do, and it is also what makes a second launch a no-op.
fn migrate_legacy_entry(current_dir: &Path, name: &str) {
    let target = current_dir.join(name);
    if target.exists() {
        return;
    }
    let Some(legacy) = current_dir.parent().map(|p| p.join(LEGACY_ID).join(name)) else {
        return;
    };
    if !legacy.exists() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(current_dir) {
        eprintln!("spool: could not create {}: {e}", current_dir.display());
        return;
    }
    match std::fs::rename(&legacy, &target) {
        Ok(()) => eprintln!("spool: moved {} to {}", legacy.display(), target.display()),
        // Not fatal: the app starts without it rather than not at all, and the
        // old copy is still there to be moved by hand.
        Err(e) => eprintln!(
            "spool: could not move {} to {}: {e}",
            legacy.display(),
            target.display()
        ),
    }
}

/// Delete the autostart entry left by the pre-rename app.
///
/// It names a binary that the rename replaced, so at the next login it either
/// does nothing or — worse, if an old build is still installed — starts a
/// second download manager alongside this one, both writing the same files.
/// The current entry is written separately by `apply_autostart`.
#[cfg(target_os = "linux")]
fn remove_legacy_autostart() {
    let Some(home) = dirs_home() else { return };
    let entry = home.join(".config/autostart/fetchd.desktop");
    if entry.exists() {
        if let Err(e) = std::fs::remove_file(&entry) {
            eprintln!("spool: could not remove {}: {e}", entry.display());
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn remove_legacy_autostart() {}

/// Whether the autostart entry on disk still points at the binary running now.
///
/// `is_enabled` only asks whether the entry file exists. It cannot tell that
/// the entry names a path this app no longer lives at — a release build
/// replacing a dev one, a reinstall to a different prefix, or the project
/// simply being moved. The entry survives, `Exec=` points at nothing, and
/// autostart silently does nothing at the next boot with the setting still
/// showing "on".
#[cfg(target_os = "linux")]
fn autostart_entry_is_current() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return true; // cannot compare; leave the entry alone
    };
    let Some(home) = dirs_home() else { return true };
    let entry = home.join(".config/autostart/spool.desktop");
    let Ok(text) = std::fs::read_to_string(&entry) else {
        return true; // no entry to be stale
    };
    let exe = exe.to_string_lossy();
    text.lines()
        .find_map(|l| l.strip_prefix("Exec="))
        .is_some_and(|cmd| cmd.split_whitespace().next() == Some(exe.as_ref()))
}

#[cfg(not(target_os = "linux"))]
fn autostart_entry_is_current() -> bool {
    // Only the XDG entry is a plain file we can read back; the macOS and
    // Windows mechanisms are managed by the OS.
    true
}

#[cfg(target_os = "linux")]
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

/// Put the autostart entry in whatever state the settings ask for.
///
/// Reconciled rather than toggled: the entry is a file on disk that the user
/// (or another app, or a reinstall) can change behind our back, so the setting
/// is the intent and this makes the disk match it — including rewriting an
/// entry that still exists but points at a path this app has moved away from.
fn apply_autostart(app: &AppHandle, want: bool) {
    use tauri_plugin_autostart::ManagerExt;

    let manager = app.autolaunch();
    let enabled = manager.is_enabled().unwrap_or(false);
    let stale = enabled && want && !autostart_entry_is_current();

    if enabled == want && !stale {
        return;
    }
    if stale {
        // Rewrite by removing first: `enable` will not replace an entry it
        // believes is already correct.
        if let Err(e) = manager.disable() {
            eprintln!("spool: could not replace the stale autostart entry: {e}");
            return;
        }
    }

    let result = if want { manager.enable() } else { manager.disable() };
    if let Err(e) = result {
        // Not fatal: a desktop without an autostart directory, or a sandbox
        // that forbids writing one, should not break saving settings.
        eprintln!("spool: could not update the autostart entry: {e}");
    }
}

#[tauri::command]
fn resume_all(app: AppHandle, state: Shared<'_>) {
    state.resume_all();
    state::pump(&app, &state);
}

#[tauri::command]
fn get_queue(state: Shared<'_>) -> Vec<DownloadView> {
    state.views()
}

#[tauri::command]
fn clear_history(app: AppHandle, state: Shared<'_>) {
    state.clear_history();
    state::emit_queue(&app, &state);
}

#[tauri::command]
fn get_settings(state: Shared<'_>) -> Settings {
    state.settings()
}

// Async so it runs on the Tokio runtime: `set_settings` rebuilds the bandwidth
// throttle, which spawns a refill task and would panic off-runtime.
#[tauri::command]
async fn update_settings(app: AppHandle, state: Shared<'_>, settings: Settings) -> Result<(), ()> {
    let want_autostart = settings.start_on_login;
    state.set_settings(settings);
    apply_autostart(&app, want_autostart);
    state::pump(&app, &state);
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be registered first: a second launch is intercepted here and
        // focuses the running window instead of starting another process.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main(app);
        }))
        // Remembers the window's size and position across launches. Without
        // it every launch reopens at the configured default, which is smaller
        // than the add dialog needs. Decorations are excluded: they're a
        // build-time choice (see tauri.conf.json), not a per-session one, and
        // restoring a stale `decorated: true` from before the custom
        // titlebar existed would resurrect the native chrome.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        - tauri_plugin_window_state::StateFlags::DECORATIONS,
                )
                .build(),
        )
        // `--hidden` is what the autostart entry passes, so a login launch can
        // go straight to the tray while a launcher launch shows itself.
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // Read from the backend only (the clipboard watcher), so the webview
        // needs no clipboard permission of its own.
        .plugin(tauri_plugin_clipboard_manager::init())
        .setup(|app| {
            let handle = app.handle().clone();
            let data_dir = handle.path().app_data_dir()?;
            let config_dir = handle.path().app_config_dir()?;

            // The app was called `fetchd` until it was renamed; carry an
            // existing install's queue and settings over before reading them.
            for name in ["queue.json", "thumbs"] {
                migrate_legacy_entry(&data_dir, name);
            }
            for name in ["settings.json", ".window-state.json"] {
                migrate_legacy_entry(&config_dir, name);
            }
            remove_legacy_autostart();

            let state = Arc::new(AppState::new(data_dir, config_dir).map_err(std::io::Error::other)?);
            app.manage(Arc::clone(&state));

            // The autostart entry can drift from the setting — a reinstall, or
            // the user editing their desktop's startup list — so make the disk
            // match the intent on every launch.
            apply_autostart(&handle, state.settings().start_on_login);

            // A login launch passes --hidden; start in the tray rather than
            // opening a window the user did not ask for. Ignored unless the
            // setting agrees, so a stale autostart entry cannot silently
            // swallow a manual launch.
            let hidden = std::env::args().any(|a| a == "--hidden")
                && state.settings().start_minimised;
            if hidden {
                if let Some(window) = handle.get_webview_window("main") {
                    let _ = window.hide();
                }
            }

            // System tray: left-click restores the window; the menu offers an
            // explicit Show and a real Quit (the window's close button only
            // hides to tray, so Quit is the one way to actually exit).
            //
            // Pause all and Resume all are here too: the app spends most of a
            // long download minimised, and reaching for the queue's brakes
            // should not mean raising the window first.
            let show = MenuItem::with_id(app, "show", "Show spool", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause_all", "Pause all", true, None::<&str>)?;
            let resume = MenuItem::with_id(app, "resume_all", "Resume all", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let sep = PredefinedMenuItem::separator(app)?;
            let menu = Menu::with_items(app, &[&show, &sep, &pause, &resume, &sep, &quit])?;

            let tray_state = Arc::clone(&state);
            let quit_state = Arc::clone(&state);
            TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("spool")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "pause_all" => {
                        tray_state.pause_all();
                        state::emit_queue(app, &tray_state);
                    }
                    "resume_all" => {
                        tray_state.resume_all();
                        state::pump(app, &tray_state);
                    }
                    "quit" => {
                        quit_state.shutdown();
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            // Localhost bridge the browser extension POSTs captured sessions to.
            server::start(handle.clone(), Arc::clone(&state));

            // One flusher for the whole queue: active downloads only mark it
            // dirty, so progress is persisted with a single write per tick
            // rather than one per download.
            let flush_state = Arc::clone(&state);
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(2));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    flush_state.flush_if_dirty();
                }
            });

            // Transfer only inside the configured window, when there is one.
            let schedule_state = Arc::clone(&state);
            let schedule_app = handle.clone();
            tauri::async_runtime::spawn(async move {
                // Edge-triggered: the window opening resumes and the window
                // closing pauses, and in between the user's own pause and
                // resume are left alone. Level-triggering would re-pause a
                // download the user had just started by hand.
                let mut was_inside: Option<bool> = None;
                let mut interval = tokio::time::interval(Duration::from_secs(30));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    let settings = schedule_state.settings();
                    if !settings.schedule_enabled {
                        was_inside = None;
                        continue;
                    }
                    let Some(inside) =
                        in_window(local_minutes(), &settings.schedule_start, &settings.schedule_stop)
                    else {
                        continue; // unparseable bounds: do nothing rather than guess
                    };
                    if was_inside == Some(inside) {
                        continue;
                    }
                    was_inside = Some(inside);
                    if inside {
                        schedule_state.resume_all();
                        state::pump(&schedule_app, &schedule_state);
                    } else {
                        schedule_state.pause_all();
                        state::emit_queue(&schedule_app, &schedule_state);
                    }
                }
            });

            // Clipboard watcher: copying a link is how a download usually
            // starts, so offer it rather than making the user paste it back.
            let clip_state = Arc::clone(&state);
            let clip_app = handle.clone();
            tauri::async_runtime::spawn(async move {
                use tauri_plugin_clipboard_manager::ClipboardExt;

                // Seeded with whatever is on the clipboard at launch, so
                // enabling the setting does not immediately offer something
                // copied hours ago.
                let mut last = clip_app.clipboard().read_text().unwrap_or_default();
                let mut interval = tokio::time::interval(Duration::from_millis(1500));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    interval.tick().await;
                    if !clip_state.settings().clipboard_watch {
                        // Still track the text: turning the setting on should
                        // not fire on whatever was copied while it was off.
                        last = clip_app.clipboard().read_text().unwrap_or_default();
                        continue;
                    }
                    let Ok(text) = clip_app.clipboard().read_text() else { continue };
                    if text == last {
                        continue;
                    }
                    last = text.clone();

                    let url = text.trim();
                    if crate::download::validate_url(url).is_err() || clip_state.has_url(url) {
                        continue;
                    }
                    // The same path the extension's "ask before download" uses:
                    // park it and let the add dialog collect the answers.
                    let token = clip_state.stash_pending(state::PendingAdd {
                        url: url.to_string(),
                        session: None,
                        force_video: false,
                        added_at: queue::now_secs(),
                    });
                    show_main(&clip_app);
                    let _ = clip_app.emit(
                        "download://confirm",
                        state::ConfirmRequest {
                            token,
                            url: url.to_string(),
                            video: false,
                        },
                    );
                }
            });

            // Auto-resume what was interrupted, but not immediately: on a cold
            // start the network stack may not be up yet, and firing straight
            // into a retry ladder would burn attempts on a link that is about
            // to work.
            tauri::async_runtime::spawn(async move {
                // Apply the saved bandwidth cap now that we are on the runtime
                // (Throttle spawns a refill task).
                state.rebuild_throttle();
                tokio::time::sleep(Duration::from_secs(3)).await;
                state::pump(&handle, &state);
            });

            Ok(())
        })
        // Close button hides to tray instead of quitting, so downloads keep
        // running in the background. Quit is via the tray menu.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // With background running on, the close button hides to the
                // tray and transfers carry on. With it off, close means quit —
                // and quitting has to go through `shutdown` so in-flight
                // progress is checkpointed rather than lost.
                let background = window
                    .app_handle()
                    .try_state::<Arc<AppState>>()
                    .map(|s| s.settings().run_in_background)
                    .unwrap_or(true);

                if background {
                    let _ = window.hide();
                    api.prevent_close();
                } else if let Some(state) = window.app_handle().try_state::<Arc<AppState>>() {
                    state.shutdown();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            add_download,
            add_pending,
            cancel_pending,
            get_download_dir,
            import_urls,
            grab_links,
            is_duplicate,
            pause_download,
            resume_download,
            cancel_download,
            retry_download,
            remove_download,
            rename_download,
            bulk_action,
            move_download,
            hash_file,
            pause_all,
            resume_all,
            video_thumbnail,
            get_queue,
            clear_history,
            get_settings,
            update_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parent holding both identifiers' directories, with `queue.json`
    /// already sitting in the old one.
    fn legacy_install(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("spool-migrate-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let legacy = root.join(LEGACY_ID);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("queue.json"), "old").unwrap();
        let current = root.join("com.saikarthik.spool");
        (root, current)
    }

    #[test]
    fn moves_the_queue_when_there_is_nothing_to_lose() {
        let (root, current) = legacy_install("move");

        migrate_legacy_entry(&current, "queue.json");

        assert_eq!(
            std::fs::read_to_string(current.join("queue.json")).unwrap(),
            "old"
        );
        assert!(!root.join(LEGACY_ID).join("queue.json").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    // WebKitGTK creates the data directory before this runs, so the directory
    // existing must not stop the queue coming across.
    #[test]
    fn moves_the_queue_into_a_directory_someone_else_created() {
        let (root, current) = legacy_install("exists");
        std::fs::create_dir_all(current.join("WebKitCache")).unwrap();

        migrate_legacy_entry(&current, "queue.json");

        assert_eq!(
            std::fs::read_to_string(current.join("queue.json")).unwrap(),
            "old"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    // The whole reason the function checks before it moves: this runs on every
    // launch, and the second one must not put a stale queue over the live one.
    #[test]
    fn leaves_a_queue_that_is_already_there_alone() {
        let (root, current) = legacy_install("keep");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("queue.json"), "live").unwrap();

        migrate_legacy_entry(&current, "queue.json");

        assert_eq!(
            std::fs::read_to_string(current.join("queue.json")).unwrap(),
            "live"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn does_nothing_for_a_fresh_install() {
        let root = std::env::temp_dir().join(format!("spool-migrate-fresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let current = root.join("com.saikarthik.spool");

        migrate_legacy_entry(&current, "queue.json");

        assert!(!current.exists(), "no directory should be created for nothing");
        let _ = std::fs::remove_dir_all(&root);
    }
}
