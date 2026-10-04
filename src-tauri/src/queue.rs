//! Queue model and persistence.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::download::{DownloadPlan, Session};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    /// The app stopped without a clean shutdown while this was downloading.
    /// Distinct from `Paused` because only this state auto-resumes.
    Interrupted,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    pub id: String,
    pub url: String,
    pub plan: DownloadPlan,
    pub status: Status,
    /// Per-segment bytes confirmed on disk. Index-aligned with `plan.ranges`.
    pub done: Vec<u64>,
    pub error: Option<String>,
    pub added_at: u64,
    /// Browser session captured by the extension for this download, replayed
    /// on start and resume. `None` for a plain typed-in URL.
    #[serde(default)]
    pub session: Option<Session>,
    /// Per-download yt-dlp quality override chosen in the add dialog. Falls
    /// back to the global setting when `None`.
    #[serde(default)]
    pub quality: Option<String>,
    /// Filename chosen in the add dialog. Only the video engine needs it at run
    /// time — for an HTTP download the name is already baked into the plan.
    #[serde(default)]
    pub name: Option<String>,
}

impl Download {
    pub fn new(id: String, plan: DownloadPlan) -> Self {
        let segments = plan.segment_count();
        Download {
            id,
            url: plan.url.clone(),
            status: Status::Queued,
            done: vec![0; segments],
            error: None,
            added_at: now_secs(),
            session: None,
            quality: None,
            name: None,
            plan,
        }
    }

    pub fn downloaded(&self) -> u64 {
        self.done.iter().sum()
    }

    pub fn filename(&self) -> String {
        self.plan.filename()
    }

    /// Finished for good, one way or the other.
    pub fn is_terminal(&self) -> bool {
        matches!(self.status, Status::Completed | Status::Failed)
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What a relaunch does to each state.
///
/// A user who explicitly paused (to free bandwidth, say) must not have that
/// decision overridden simply by reopening the app, and a download that has
/// already failed should not silently retry. Only an *unplanned* stop —
/// crash, forced quit, power cut — resumes on its own.
pub fn reconcile_on_launch(status: Status) -> Status {
    match status {
        // Was mid-transfer when the process died.
        Status::Downloading => Status::Interrupted,
        // Explicit user decisions survive a restart untouched.
        Status::Paused => Status::Paused,
        Status::Failed => Status::Failed,
        Status::Completed => Status::Completed,
        Status::Queued => Status::Queued,
        Status::Interrupted => Status::Interrupted,
    }
}

/// Write JSON through a temp file and rename it into place.
///
/// A direct write can be interrupted after truncating and before the new
/// content lands, leaving a zero-byte `queue.json` and losing the entire
/// queue. Rename is atomic, so a reader sees either the old file or the new
/// one and never a half-written one.
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let body = serde_json::to_vec_pretty(value).map_err(|e| format!("serialize failed: {e}"))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    let tmp = tmp_path(path);
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp)
            .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        file.write_all(&body).map_err(|e| format!("write failed: {e}"))?;
        // Rename only guarantees atomicity of the *directory entry*. Without
        // this the new file can be visible but empty after a power cut.
        file.sync_all().map_err(|e| format!("sync failed: {e}"))?;
    }

    std::fs::rename(&tmp, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

pub fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    let body = std::fs::read(path).ok()?;
    serde_json::from_slice(&body).ok()
}

/// Load the queue, and never silently discard a file we could not read.
///
/// `load_json` returns `None` both for "no file yet" and for "the file is
/// there but will not parse". Treating the second like the first starts the
/// app with an empty queue and the next checkpoint overwrites the original —
/// the user's whole download list, gone, with nothing to recover from. So an
/// unreadable file is moved aside first, and the path is reported.
pub fn load_queue(path: &Path) -> Vec<Download> {
    let body = match std::fs::read(path) {
        Ok(body) => body,
        Err(_) => return Vec::new(), // first run
    };
    match serde_json::from_slice(&body) {
        Ok(queue) => queue,
        Err(e) => {
            let aside = path.with_file_name(format!("queue.unreadable-{}.json", now_secs()));
            match std::fs::rename(path, &aside) {
                Ok(()) => eprintln!(
                    "spool: {} could not be read ({e}); kept a copy at {}",
                    path.display(),
                    aside.display()
                ),
                Err(move_err) => eprintln!(
                    "spool: {} could not be read ({e}) and could not be moved aside ({move_err})",
                    path.display()
                ),
            }
            Vec::new()
        }
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table from the plan, asserted directly. The two rows that matter
    /// are Paused and Failed: those are user decisions, and auto-resuming
    /// either one on launch would override a choice the user made deliberately.
    #[test]
    fn restart_state_table() {
        assert_eq!(reconcile_on_launch(Status::Downloading), Status::Interrupted);
        assert_eq!(reconcile_on_launch(Status::Paused), Status::Paused);
        assert_eq!(reconcile_on_launch(Status::Failed), Status::Failed);
        assert_eq!(reconcile_on_launch(Status::Completed), Status::Completed);
        assert_eq!(reconcile_on_launch(Status::Queued), Status::Queued);
    }

    /// A queue.json written by an older build has no `session`, `quality` or
    /// `name` key. Loading must fill them in rather than dropping the whole
    /// file — which `load_json` would do silently, losing every download.
    #[test]
    fn queue_from_an_older_build_still_loads() {
        let legacy = r#"[{
            "id": "7",
            "url": "https://example.com/a.zip",
            "plan": {
                "url": "https://example.com/a.zip",
                "final_path": "/tmp/a.zip",
                "part_path": "/tmp/a.zip.part",
                "total": 100,
                "supports_ranges": true,
                "validator": null,
                "ranges": [[0, 99]],
                "engine": "http"
            },
            "status": "paused",
            "done": [40],
            "error": null,
            "added_at": 1
        }]"#;

        let entries: Vec<Download> = serde_json::from_str(legacy).expect("legacy queue must load");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].session.is_none());
        assert!(entries[0].quality.is_none());
        assert!(entries[0].name.is_none());
        assert!(entries[0].plan.thumbnail.is_none());
        assert_eq!(entries[0].downloaded(), 40);
    }

    #[test]
    fn new_download_sizes_its_counters_to_the_plan() {
        let plan = DownloadPlan {
            url: "https://example.com/a.bin".into(),
            final_path: "/tmp/a.bin".into(),
            part_path: "/tmp/a.bin.part".into(),
            total: Some(300),
            supports_ranges: true,
            validator: None,
            ranges: vec![(0, 99), (100, 199), (200, 299)],
            engine: crate::download::Engine::Http,
            thumbnail: None,
            only_files: None,
        };
        let d = Download::new("1".into(), plan);
        assert_eq!(d.done.len(), 3, "one counter per segment");
        assert_eq!(d.downloaded(), 0);
        assert_eq!(d.status, Status::Queued);
        assert!(!d.is_terminal());

        // A plan with no ranges (yt-dlp) still gets one counter, or every
        // progress write would panic on an empty vector.
        let video = DownloadPlan {
            url: "https://example.com/v".into(),
            final_path: "/tmp/v".into(),
            part_path: "/tmp/v.part".into(),
            total: None,
            supports_ranges: false,
            validator: None,
            ranges: Vec::new(),
            engine: crate::download::Engine::YtDlp,
            thumbnail: None,
            only_files: None,
        };
        assert_eq!(Download::new("2".into(), video).done.len(), 1);
    }

    #[test]
    fn only_finished_entries_are_terminal() {
        for (status, terminal) in [
            (Status::Queued, false),
            (Status::Downloading, false),
            (Status::Paused, false),
            (Status::Interrupted, false),
            (Status::Completed, true),
            (Status::Failed, true),
        ] {
            let plan = DownloadPlan {
                url: "https://example.com/a".into(),
                final_path: "/tmp/a".into(),
                part_path: "/tmp/a.part".into(),
                total: None,
                supports_ranges: false,
                validator: None,
                ranges: Vec::new(),
                engine: crate::download::Engine::Http,
                thumbnail: None,
                only_files: None,
            };
            let mut d = Download::new("1".into(), plan);
            d.status = status;
            assert_eq!(d.is_terminal(), terminal, "{status:?}");
        }
    }

    /// `save_json` writes through a temp file. It must not leave that temp
    /// behind, or the data directory fills with `queue.json.tmp` copies.
    #[test]
    fn save_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join(format!("spool-tmpfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("queue.json");

        save_json(&path, &vec![1u8, 2, 3]).unwrap();
        save_json(&path, &vec![4u8, 5]).unwrap();

        assert!(!path.with_file_name("queue.json.tmp").exists());
        assert_eq!(load_json::<Vec<u8>>(&path), Some(vec![4, 5]), "the second write wins");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The whole point of the quarantine: a queue.json that will not parse must
    /// survive the launch that could not read it, because the next checkpoint
    /// overwrites the file.
    #[test]
    fn an_unreadable_queue_is_moved_aside_not_dropped() {
        let dir = std::env::temp_dir().join(format!("spool-quarantine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.json");
        std::fs::write(&path, b"{ this is not the queue }").unwrap();

        assert!(load_queue(&path).is_empty(), "an unreadable queue starts empty");
        assert!(!path.exists(), "the unreadable file must not be left in place to be overwritten");

        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("queue.unreadable-"))
            .collect();
        assert_eq!(kept.len(), 1, "expected one quarantined copy, found {kept:?}");
        assert_eq!(
            std::fs::read(dir.join(&kept[0])).unwrap(),
            b"{ this is not the queue }",
            "the original bytes must be preserved verbatim"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_missing_queue_file_is_simply_empty() {
        let dir = std::env::temp_dir().join(format!("spool-firstrun-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_queue(&dir.join("queue.json")).is_empty());
        // Nothing to quarantine on a first run.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_of_a_missing_file_is_none() {
        let missing = std::env::temp_dir().join("spool-does-not-exist-ever.json");
        let _ = std::fs::remove_file(&missing);
        assert!(load_json::<Vec<Download>>(&missing).is_none());
    }

    #[test]
    fn atomic_save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("spool-queue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("queue.json");

        let value = vec![1u64, 2, 3];
        save_json(&path, &value).unwrap();
        assert_eq!(load_json::<Vec<u64>>(&path), Some(vec![1, 2, 3]));

        // Overwriting leaves no temp file behind and no truncated remains.
        save_json(&path, &vec![9u64]).unwrap();
        assert_eq!(load_json::<Vec<u64>>(&path), Some(vec![9]));
        assert!(!dir.join("queue.json.tmp").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_of_corrupt_file_is_none_not_panic() {
        let dir = std::env::temp_dir().join(format!("spool-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.json");
        std::fs::write(&path, b"{ truncated").unwrap();

        assert_eq!(load_json::<Vec<u64>>(&path), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
