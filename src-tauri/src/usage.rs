//! CPU and memory of spool itself, for the performance overlay.
//!
//! Counts the app process plus its direct children, because the webview's
//! rendering happens in WebKitWebProcess (a child), not in spool. Linux only:
//! everything is read from /proc; elsewhere the overlay shows FPS alone.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Usage {
    /// Percent of one core since the previous sample, `top`-style, so two
    /// busy cores read 200. Null on the first sample, which has no baseline.
    pub cpu: Option<f64>,
    /// Resident memory in bytes.
    pub rss: u64,
}

/// utime + stime in clock ticks, and the parent pid, from a `/proc/<pid>/stat`
/// line. The command name sits in parentheses and may contain spaces or
/// parentheses itself, so fields are counted from the last `)`.
pub fn parse_stat(line: &str) -> Option<(u64, u32)> {
    let rest = &line[line.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // f[0] is field 3 (state): ppid is field 4, utime 14, stime 15.
    let ppid = f.get(1)?.parse().ok()?;
    let utime: u64 = f.get(11)?.parse().ok()?;
    let stime: u64 = f.get(12)?.parse().ok()?;
    Some((utime + stime, ppid))
}

#[cfg(target_os = "linux")]
pub fn sample() -> Usage {
    use std::sync::Mutex;
    use std::time::Instant;
    static LAST: Mutex<Option<(u64, Instant)>> = Mutex::new(None);

    let me = std::process::id();
    let stat = |pid: &str| std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
    let rss_pages = |pid: &str| {
        std::fs::read_to_string(format!("/proc/{pid}/statm"))
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .unwrap_or(0)
    };

    let mut pids = vec![me.to_string()];
    // ponytail: full /proc scan each second; fine for a debug overlay.
    if let Ok(dir) = std::fs::read_dir("/proc") {
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            if stat(&name).and_then(|s| parse_stat(&s)).is_some_and(|(_, ppid)| ppid == me) {
                pids.push(name);
            }
        }
    }

    let ticks: u64 = pids.iter().filter_map(|p| parse_stat(&stat(p)?)).map(|(t, _)| t).sum();
    // SAFETY: sysconf only reads configuration values.
    let (hz, page) = unsafe { (libc::sysconf(libc::_SC_CLK_TCK), libc::sysconf(libc::_SC_PAGESIZE)) };
    let rss = pids.iter().map(|p| rss_pages(p)).sum::<u64>() * page.max(1) as u64;

    let now = Instant::now();
    let mut last = LAST.lock().unwrap();
    let cpu = last.and_then(|(prev, at)| {
        let secs = now.duration_since(at).as_secs_f64();
        // A child that exits takes its ticks with it, so the sum can drop.
        (secs > 0.0).then(|| ticks.saturating_sub(prev) as f64 / hz.max(1) as f64 / secs * 100.0)
    });
    *last = Some((ticks, now));
    Usage { cpu, rss }
}

#[cfg(not(target_os = "linux"))]
pub fn sample() -> Usage {
    Usage { cpu: None, rss: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_fields_count_from_the_last_paren() {
        let line = "4242 (Web Content (1)) S 100 4242 4242 0 -1 4194560 10 0 0 0 7 3 0 0 20 0 1 0";
        assert_eq!(parse_stat(line), Some((10, 100)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sample_reads_this_process() {
        sample();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let u = sample();
        assert!(u.cpu.is_some_and(|c| c >= 0.0));
        assert!(u.rss > 0);
    }
}
