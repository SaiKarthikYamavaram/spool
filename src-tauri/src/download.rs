//! Download engine.
//!
//! Covers M1-M4: filename resolution, redirects, retry ladder, disk-space
//! check, HEAD-blocked probe fallback, segmented transfer with strict `206`
//! validation and single-connection downgrade, pre-allocated seek-writes,
//! durable checkpoints, and `If-Range` resume.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fs4::tokio::AsyncFileExt;
use futures_util::StreamExt;
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE,
};
use reqwest::{Client, StatusCode, Url};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::throttle::Throttle;

/// Many CDNs reject the default `reqwest/<version>` agent with 403.
pub const USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                              (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// A chunk read that produces nothing for this long is treated as a failure.
/// A dropped Wi-Fi link or a rebound CGNAT lease black-holes an established
/// socket without ever raising an error, so without this the task waits
/// forever and the retry ladder below is never entered.
pub const CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

/// Backoff between segment retries, in seconds. The old 1/2/4 schedule burned
/// its whole budget in 7 seconds and died on any Wi-Fi blip or lid close;
/// this survives roughly 67 seconds of disconnection.
pub const BACKOFF_SECS: [u64; 5] = [2, 5, 10, 20, 30];

/// A segment that transfers this much after a failure has its retry budget
/// reset. The budget is per stall episode, not per download lifetime.
pub const RETRY_RESET_BYTES: u64 = 8 * 1024 * 1024;

/// Checkpoint cadence. Each checkpoint costs an `fdatasync`, so this trades a
/// bounded amount of re-downloaded data against write throughput.
pub const CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;

/// Below this, the extra sockets cost more than they gain.
pub const MIN_SEGMENTED_SIZE: u64 = 4 * 1024 * 1024;

pub const DEFAULT_SEGMENTS: u32 = 8;
pub const MAX_SEGMENTS: u32 = 8;

/// A finished connection takes over half of the largest unfinished range, but
/// only when each half is at least this big: a fresh connection's handshake
/// costs more than a smaller tail saves.
pub const MIN_SPLIT_BYTES: u64 = 1024 * 1024;

/// A split must leave the old connection at least this many setup times of
/// work in the half it gives away. A fresh connection spends its setup (time
/// to first byte) before moving anything, then still has to ramp up.
const SPLIT_SETUP_FACTOR: f64 = 2.0;

/// Whether giving away half of `left` bytes pays for a new connection: the
/// old one would need longer for that half than the new one needs to start.
/// With no speed measured yet there is nothing to weigh, so split.
fn worth_splitting(left: u64, rate: Option<f64>, setup_secs: f64) -> bool {
    match rate {
        Some(rate) => (left as f64 / 2.0) / rate > SPLIT_SETUP_FACTOR * setup_secs,
        None => true,
    }
}

fn median(mut xs: Vec<f64>) -> Option<f64> {
    xs.sort_by(f64::total_cmp);
    xs.get(xs.len() / 2).copied()
}

/// Refuse to start unless this much space remains free beyond the file itself.
const DISK_HEADROOM: u64 = 64 * 1024 * 1024;

const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
const MAX_REDIRECTS: usize = 10;

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// A browser session captured for one download.
///
/// This is the whole mechanism behind getting past an interactive anti-bot
/// challenge: the browser solves the challenge and earns a cookie (a
/// `cf_clearance`, say), and spool replays that exact session. Because the
/// cookie is bound to the `User-Agent` and `Referer` that earned it, all three
/// travel together. A default session (no cookie, no referer) is the ordinary
/// case for links that need no authentication.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub user_agent: String,
    pub cookie: Option<String>,
    pub referer: Option<String>,
    /// Proxy URL applied to both clients; `None` is a direct connection.
    #[serde(default)]
    pub proxy: Option<String>,
    /// HTTP Basic credentials as `user:password`, taken from a URL written
    /// `https://user:pass@host/file`. Kept on the session rather than in the
    /// stored URL so a resume still authenticates and the UI never displays
    /// the password.
    #[serde(default)]
    pub auth: Option<String>,
}

impl Default for Session {
    fn default() -> Self {
        Session {
            user_agent: USER_AGENT.to_string(),
            cookie: None,
            referer: None,
            proxy: None,
            auth: None,
        }
    }
}

impl Session {
    pub fn with_agent(user_agent: Option<String>) -> Self {
        Session {
            user_agent: user_agent.unwrap_or_else(|| USER_AGENT.to_string()),
            ..Session::default()
        }
    }
}

/// Headers a real browser always sends. Some CDNs reject a request carrying a
/// browser `User-Agent` but none of its companions, so send the whole set.
///
/// `Accept-Encoding` is deliberately absent: advertising `gzip` would invite a
/// compressed body, and since decompression is off (see `build_client`) the
/// byte offsets would no longer line up with `Content-Length`.
fn browser_headers(session: &Session) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, COOKIE, REFERER};

    let mut headers = HeaderMap::new();

    // Replays a session a browser already established. reqwest strips both of
    // these automatically on a cross-host redirect or an HTTPS->HTTP
    // downgrade, so a cookie for one site cannot leak to another.
    if let Some(cookie) = &session.cookie {
        if let Ok(value) = HeaderValue::from_str(cookie) {
            headers.insert(COOKIE, value);
        }
    }
    if let Some(referer) = &session.referer {
        if let Ok(value) = HeaderValue::from_str(referer) {
            headers.insert(REFERER, value);
        }
    }

    // HTTP Basic, from credentials the URL carried. Sent up front rather than
    // after a 401: reqwest would have to replay the request, and a segmented
    // download would pay that round trip once per connection.
    if let Some(auth) = &session.auth {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD.encode(auth);
        if let Ok(value) = HeaderValue::from_str(&format!("Basic {encoded}")) {
            // Marked sensitive so it is redacted from any header debug dump.
            let mut value = value;
            value.set_sensitive(true);
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
    }

    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ),
    );
    headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    headers.insert("Sec-Fetch-Dest", HeaderValue::from_static("document"));
    headers.insert("Sec-Fetch-Mode", HeaderValue::from_static("navigate"));
    headers.insert("Sec-Fetch-Site", HeaderValue::from_static("none"));
    headers.insert("Upgrade-Insecure-Requests", HeaderValue::from_static("1"));
    headers
}

/// Turn an unsuccessful response into a message that says what to do next.
///
/// A Cloudflare/Akamai interactive challenge is not a transport failure and no
/// retry or header tweak will clear it: the server wants a browser to run a
/// script. Saying "403 Forbidden" hides that and sends the user hunting for a
/// bug in the downloader.
fn describe_http_error(status: StatusCode, headers: &reqwest::header::HeaderMap) -> String {
    let challenged = headers.contains_key("cf-mitigated")
        || headers.contains_key("cf-chl-bypass")
        || headers
            .get(reqwest::header::SERVER)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("cloudflare"))
            && matches!(status, StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE);

    if challenged {
        return format!(
            "{status}: the site is serving an anti-bot browser challenge. \
             This link cannot be fetched by any download manager without a \
             browser session; open it in a browser and download from there, \
             or use a direct link that is not behind the challenge."
        );
    }

    match status {
        StatusCode::FORBIDDEN => format!("{status}: the server refused the request. \
             The link may be expired, region-locked, or require a login."),
        StatusCode::NOT_FOUND => format!("{status}: no such file at this URL."),
        StatusCode::UNAUTHORIZED => format!("{status}: this URL needs authentication."),
        other => format!("server returned an error: {other}"),
    }
}

/// Client for probes and single-connection transfers.
///
/// `gzip`/`brotli` are deliberately absent from the `reqwest` feature list:
/// automatic decompression desyncs the stream from the `Content-Length` and
/// `Content-Range` byte offsets that seek-writes depend on.
pub fn build_client(session: &Session) -> Result<Client, String> {
    apply_proxy(
        Client::builder(),
        session,
    )
        .user_agent(session.user_agent.clone())
        .default_headers(browser_headers(session))
        .connect_timeout(Duration::from_secs(10))
        // Some mirrors answer a browser User-Agent with a cookie and a 302 to
        // the same URL, and loop until the redirect limit for a client that
        // drops the cookie. Only for sessions without their own Cookie
        // header: reqwest skips the jar whenever one is already set.
        .cookie_store(session.cookie.is_none())
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

/// Client for segment workers.
///
/// `http1_only()` is the whole point of segmentation: `reqwest` negotiates
/// HTTP/2 by default, and HTTP/2 multiplexes every stream over one TCP
/// connection. Eight segments sharing one socket share one congestion window,
/// which is exactly what a download manager exists to avoid.
pub fn build_segment_client(session: &Session) -> Result<Client, String> {
    apply_proxy(
        Client::builder(),
        session,
    )
        .user_agent(session.user_agent.clone())
        .default_headers(browser_headers(session))
        .http1_only()
        .connect_timeout(Duration::from_secs(10))
        // Keeps sockets warm across retries. This is NOT what creates the
        // parallelism -- concurrent HTTP/1.1 requests already open their own.
        .pool_max_idle_per_host(MAX_SEGMENTS as usize)
        // Same cookie-bounce mirrors as `build_client`.
        .cookie_store(session.cookie.is_none())
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .build()
        .map_err(|e| format!("failed to build segment client: {e}"))
}

/// Route a client through the configured proxy, if any. An unparseable proxy
/// URL is ignored rather than failing every download.
fn apply_proxy(builder: reqwest::ClientBuilder, session: &Session) -> reqwest::ClientBuilder {
    match session.proxy.as_deref().filter(|p| !p.is_empty()) {
        Some(url) => match reqwest::Proxy::all(url) {
            Ok(proxy) => builder.proxy(proxy),
            Err(e) => {
                eprintln!("spool: ignoring invalid proxy {url}: {e}");
                builder
            }
        },
        None => builder,
    }
}

/// Sub-folder for a filename's type, used when category sorting is on.
/// Mirrors the type buckets the UI and extension already use.
pub fn category_for(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "flv" | "m4v" | "ts" => "Video",
        "mp3" | "flac" | "wav" | "aac" | "ogg" | "m4a" | "opus" => "Audio",
        "zip" | "tar" | "gz" | "xz" | "7z" | "rar" | "bz2" | "zst" => "Archives",
        "pdf" | "doc" | "docx" | "epub" | "txt" | "odt" | "rtf" => "Documents",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "avif" => "Images",
        "iso" | "img" | "dmg" | "exe" | "appimage" | "deb" | "rpm" | "msi" => "Programs",
        _ => "Other",
    }
}

/// Reject anything that is not plain HTTP(S) before touching the network.
pub fn validate_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|e| format!("invalid URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(format!("unsupported scheme `{other}` (only http/https)")),
    }
}

#[cfg(test)]
mod userinfo_tests {
    use super::*;

    #[test]
    fn an_ordinary_url_is_untouched() {
        let (auth, url) = split_userinfo("https://example.com/a.zip");
        assert!(auth.is_none());
        assert_eq!(url, "https://example.com/a.zip");
    }

    #[test]
    fn credentials_move_out_of_the_url() {
        let (auth, url) = split_userinfo("https://bob:hunter2@example.com/a.zip");
        assert_eq!(auth.as_deref(), Some("bob:hunter2"));
        assert_eq!(url, "https://example.com/a.zip", "the password must not stay in the URL");
    }

    /// A password is percent-encoded in a URL but plain in the header.
    #[test]
    fn credentials_are_percent_decoded() {
        let (auth, _) = split_userinfo("https://bob:p%40ss%3Aword@example.com/a");
        assert_eq!(auth.as_deref(), Some("bob:p@ss:word"));
    }

    #[test]
    fn a_username_with_no_password_still_authenticates() {
        let (auth, url) = split_userinfo("https://token@example.com/a");
        assert_eq!(auth.as_deref(), Some("token:"));
        assert_eq!(url, "https://example.com/a");
    }

    /// Unparseable input belongs to `validate_url` to reject, not to this.
    #[test]
    fn junk_passes_straight_through() {
        let (auth, url) = split_userinfo("not a url");
        assert!(auth.is_none());
        assert_eq!(url, "not a url");
    }

    #[test]
    fn the_header_carries_the_credentials() {
        let session = Session { auth: Some("bob:hunter2".into()), ..Session::default() };
        let headers = browser_headers(&session);
        let value = headers.get(reqwest::header::AUTHORIZATION).expect("Authorization must be set");
        // base64("bob:hunter2")
        assert_eq!(value.to_str().unwrap(), "Basic Ym9iOmh1bnRlcjI=");
        assert!(value.is_sensitive(), "credentials must not appear in a header dump");
    }

    #[test]
    fn no_credentials_means_no_header() {
        let headers = browser_headers(&Session::default());
        assert!(headers.get(reqwest::header::AUTHORIZATION).is_none());
    }
}

/// Split `https://user:pass@host/file` into the credentials and the URL with
/// them removed.
///
/// Credentials belong in an `Authorization` header, not in the stored URL: the
/// URL is displayed in the row, copied by "Copy URL" and matched by the
/// duplicate check, and a password has no business in any of those. Returns
/// `None` for the ordinary case of a URL with no userinfo.
pub fn split_userinfo(raw: &str) -> (Option<String>, String) {
    let Ok(mut url) = Url::parse(raw) else {
        return (None, raw.to_string());
    };
    if url.username().is_empty() && url.password().is_none() {
        return (None, raw.to_string());
    }
    // Percent-decoded, because that is what the header carries: a password
    // written `p%40ss` in a URL is `p@ss` to the server.
    let user = percent_decode(url.username());
    let auth = match url.password() {
        Some(pass) => format!("{user}:{}", percent_decode(pass)),
        None => format!("{user}:"),
    };
    // Both setters only fail on a URL that cannot have a host (`mailto:`),
    // which `validate_url` rejects anyway; leaving the userinfo in place is
    // the safe outcome either way.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    (Some(auth), url.to_string())
}

// ---------------------------------------------------------------------------
// Filename resolution
// ---------------------------------------------------------------------------

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Minimal percent-decoder for RFC 5987 `filename*` values and URL path
/// segments.
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split a header value on `;`, ignoring separators inside quoted strings.
fn split_params(header: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut escaped = false;

    for c in header.chars() {
        if escaped {
            current.push(c);
            escaped = false;
        } else if c == '\\' && in_quotes {
            escaped = true;
        } else if c == '"' {
            in_quotes = !in_quotes;
            current.push(c);
        } else if c == ';' && !in_quotes {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    parts.push(current);
    parts
}

fn unquote(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut escaped = false;
        for c in inner.chars() {
            if escaped {
                out.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else {
                out.push(c);
            }
        }
        out
    } else {
        trimmed.to_string()
    }
}

/// Extract a filename from a `Content-Disposition` header (RFC 6266).
///
/// `filename*` (RFC 5987) wins over a plain `filename` when both are present.
pub fn filename_from_content_disposition(header: &str) -> Option<String> {
    let mut plain = None;

    for part in split_params(header) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();

        if key == "filename*" {
            // ext-value is `charset'language'percent-encoded-value`.
            let encoded = match value.split_once('\'').and_then(|(_, r)| r.split_once('\'')) {
                Some((_lang, v)) => v,
                None => value,
            };
            let decoded = percent_decode(encoded.trim());
            if !decoded.is_empty() {
                return Some(decoded);
            }
        } else if key == "filename" && plain.is_none() {
            let v = unquote(value);
            if !v.is_empty() {
                plain = Some(v);
            }
        }
    }

    plain
}

/// Reduce an untrusted name to a single safe path component.
///
/// `Content-Disposition` is supplied by the remote server, so a value like
/// `../../.bashrc` must never escape the download directory. Stripping invalid
/// characters is not enough on its own -- it leaves `..` fully intact -- so the
/// directory portion is dropped first and traversal names are rejected
/// outright. Returns `None` when nothing usable survives.
pub fn sanitize_filename(raw: &str) -> Option<String> {
    // Drop any directory portion using both separators: a Windows-style
    // `..\..\evil.exe` is not split by `Path::file_name` on Unix.
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");

    let cleaned: String = last
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            other => other,
        })
        .collect();

    // Windows silently drops trailing dots and spaces; do it explicitly so the
    // name we record matches the name on disk.
    let name = cleaned.trim().trim_end_matches(['.', ' ']).trim();

    if name.is_empty() || name == "." || name == ".." || name.starts_with('.') {
        return None;
    }
    Some(name.to_string())
}

/// Pick a filename: `Content-Disposition`, then the final redirect URL's path,
/// then a fixed fallback.
pub fn resolve_filename(content_disposition: Option<&str>, final_url: &Url) -> String {
    content_disposition
        .and_then(filename_from_content_disposition)
        .and_then(|name| sanitize_filename(&name))
        .or_else(|| {
            let last = final_url.path().rsplit('/').next().unwrap_or("");
            sanitize_filename(&percent_decode(last))
        })
        .unwrap_or_else(|| "download.bin".to_string())
}

/// Auto-suffix on collision: `file.zip` -> `file (1).zip`, matching browsers.
pub fn resolve_unique_path(base_dir: &Path, filename: &str) -> PathBuf {
    let target = base_dir.join(filename);
    if !target.exists() && !part_path(&target).exists() {
        return target;
    }

    let path = Path::new(filename);
    // `file_stem` splits at the LAST dot, so `archive.tar.gz` becomes
    // `archive.tar` + `gz` and suffixes to `archive.tar (1).gz`.
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("download");
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");

    for counter in 1.. {
        let candidate = if ext.is_empty() {
            format!("{stem} ({counter})")
        } else {
            format!("{stem} ({counter}).{ext}")
        };
        let target = base_dir.join(candidate);
        if !target.exists() && !part_path(&target).exists() {
            return target;
        }
    }
    unreachable!("counter range is unbounded")
}

/// `foo.zip` -> `foo.zip.part`, kept in the same directory as the final file.
///
/// The `.part` must live on the destination volume: a rename across mounts
/// fails with `EXDEV` and degrades into a full multi-gigabyte copy, which is
/// exactly what the pre-allocated single-file design exists to avoid. It also
/// means the free-space check below measures the drive we actually write to.
pub fn part_path(final_path: &Path) -> PathBuf {
    let mut name = final_path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    final_path.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Probe
// ---------------------------------------------------------------------------

/// What the server will tell us before we commit to a transfer.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub total: Option<u64>,
    pub supports_ranges: bool,
    pub filename: String,
    /// `ETag` if present, else `Last-Modified`. Replayed as `If-Range`.
    pub validator: Option<String>,
}

/// Parse the total size out of a `Content-Range: bytes 0-0/12345` header.
/// A `/*` total means the server will not say, so the size stays unknown.
pub fn parse_content_range_total(value: &str) -> Option<u64> {
    let (_unit, rest) = value.trim().split_once(' ')?;
    let (_range, total) = rest.rsplit_once('/')?;
    let total = total.trim();
    if total == "*" {
        return None;
    }
    total.parse().ok()
}

fn header_str(headers: &reqwest::header::HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
}

/// Read `Content-Length` from the headers rather than calling
/// `Response::content_length()`.
///
/// The latter reports the length of the *body* being received, which for a
/// `HEAD` reply is always 0 — the header is the only place the real size
/// lives. Trusting the method there makes every probe report a zero-byte file.
fn content_length(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    header_str(headers, reqwest::header::CONTENT_LENGTH)?.trim().parse().ok()
}

/// Ask the server for size, range support and a filename.
///
/// Tries `HEAD` first. S3 pre-signed URLs and several CDNs answer `HEAD` with
/// 403/405/501 (or reject it outright), so on failure this falls back to a
/// `GET` with `Range: bytes=0-0`: a `206` reply proves range support and its
/// `Content-Range` carries the total size, for the cost of one byte.
pub async fn probe(client: &Client, url: &Url) -> Result<RemoteInfo, String> {
    if let Ok(response) = client.head(url.clone()).send().await {
        if response.status().is_success() {
            let headers = response.headers().clone();
            let cd = header_str(&headers, CONTENT_DISPOSITION);
            let supports_ranges = header_str(&headers, ACCEPT_RANGES)
                .map(|v| v.to_ascii_lowercase().contains("bytes"))
                .unwrap_or(false);

            return Ok(RemoteInfo {
                total: content_length(&headers),
                supports_ranges,
                filename: resolve_filename(cd.as_deref(), response.url()),
                validator: header_str(&headers, ETAG)
                    .or_else(|| header_str(&headers, LAST_MODIFIED)),
            });
        }
    }

    // HEAD blocked or errored: one-byte ranged GET tells us everything.
    let response = client
        .get(url.clone())
        .header(RANGE, "bytes=0-0")
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(describe_http_error(status, response.headers()));
    }
    let headers = response.headers().clone();
    let cd = header_str(&headers, CONTENT_DISPOSITION);
    let filename = resolve_filename(cd.as_deref(), response.url());
    let validator = header_str(&headers, ETAG).or_else(|| header_str(&headers, LAST_MODIFIED));

    if status == StatusCode::PARTIAL_CONTENT {
        let total = header_str(&headers, CONTENT_RANGE)
            .as_deref()
            .and_then(parse_content_range_total);
        Ok(RemoteInfo { total, supports_ranges: true, filename, validator })
    } else {
        // Server ignored the Range and sent the whole body: no range support.
        Ok(RemoteInfo {
            total: content_length(&headers),
            supports_ranges: false,
            filename,
            validator,
        })
    }
}

// ---------------------------------------------------------------------------
// Segment planning
// ---------------------------------------------------------------------------

/// Split `total` bytes into inclusive `[start, end]` byte ranges.
///
/// `Range` headers are inclusive at both ends, so the last byte of segment N
/// is one less than the first byte of segment N+1. The final segment absorbs
/// the remainder when the size does not divide evenly.
pub fn plan_segments(total: u64, segments: u32) -> Vec<(u64, u64)> {
    if total == 0 {
        return Vec::new();
    }
    let n = if total < MIN_SEGMENTED_SIZE {
        1
    } else {
        segments.clamp(1, MAX_SEGMENTS).min(total as u32).max(1)
    } as u64;

    let per = total / n;
    let mut ranges = Vec::with_capacity(n as usize);
    for i in 0..n {
        let start = i * per;
        let end = if i == n - 1 { total - 1 } else { start + per - 1 };
        ranges.push((start, end));
    }
    ranges
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum SegErr {
    /// Transient: worth another attempt after a backoff.
    Retryable(String),
    /// Permanent: no amount of retrying will help.
    Fatal(String),
    /// The server answered a ranged request with `200 OK`, meaning it ignored
    /// `Range` entirely and is streaming the whole file from byte 0. Writing
    /// that stream at a segment offset would corrupt the file.
    RangeIgnored,
    Cancelled,
}

impl SegErr {
    fn message(&self) -> String {
        match self {
            SegErr::Retryable(m) | SegErr::Fatal(m) => m.clone(),
            SegErr::RangeIgnored => "server ignored Range header".into(),
            SegErr::Cancelled => "cancelled".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Transfer
// ---------------------------------------------------------------------------

/// One segment's byte counts.
///
/// `live` counts everything handed to the kernel; `durable` counts only what
/// an `fdatasync` has confirmed is on the device. They exist separately
/// because they answer different questions and have different consequences:
///
/// - The UI wants `live`. Showing only synced bytes would make the progress
///   bar jump in 8 MB steps.
/// - `queue.json` must record `durable`. An offset ahead of the device is the
///   dangerous direction: after a power cut, resume would seek past bytes that
///   were never written and leave a permanent zero-filled hole in the file.
///   Recording behind the frontier merely re-downloads a little.
#[derive(Debug)]
pub struct SegmentProgress {
    live: AtomicU64,
    durable: AtomicU64,
    /// Where this segment sits in the file. `end` shrinks when another
    /// connection takes over its tail, so it is read under the same lock that
    /// guards the claim frontier.
    bounds: Mutex<Bounds>,
    /// Time to first byte of this segment's latest request, in ms; 0 until
    /// one has answered.
    ttfb_ms: AtomicU64,
}

/// `frontier` is the absolute offset up to which the worker has claimed bytes
/// (written or about to be). A split always cuts above it, so a chunk already
/// in flight can never land in the range handed to another connection.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    start: u64,
    end: u64,
    frontier: u64,
    /// When the segment was placed and how many bytes it had then, so its
    /// average speed can be measured.
    since: Instant,
    base: u64,
}

impl Default for SegmentProgress {
    fn default() -> Self {
        SegmentProgress::starting_at(0)
    }
}

impl SegmentProgress {
    fn starting_at(offset: u64) -> Self {
        SegmentProgress {
            live: AtomicU64::new(offset),
            durable: AtomicU64::new(offset),
            bounds: Mutex::new(Bounds { start: 0, end: u64::MAX, frontier: 0, since: Instant::now(), base: offset }),
            ttfb_ms: AtomicU64::new(0),
        }
    }

    fn with_range(start: u64, end: u64) -> Self {
        let seg = SegmentProgress::starting_at(0);
        seg.place(start, end);
        seg
    }

    fn place(&self, start: u64, end: u64) {
        let mut b = self.bounds.lock().unwrap();
        let live = self.live();
        *b = Bounds { start, end, frontier: start + live, since: Instant::now(), base: live };
    }

    fn start(&self) -> u64 {
        self.bounds.lock().unwrap().start
    }

    fn end(&self) -> u64 {
        self.bounds.lock().unwrap().end
    }

    fn note_ttfb(&self, waited: Duration) {
        self.ttfb_ms.store(waited.as_millis().max(1) as u64, Ordering::Relaxed);
    }

    fn ttfb(&self) -> Option<f64> {
        match self.ttfb_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms as f64 / 1000.0),
        }
    }

    /// Average bytes/s since this segment was placed, once there is enough to
    /// say anything. Stalls and retry backoff count against it, which is the
    /// point: a stuck segment is exactly the one worth splitting.
    fn rate(&self) -> Option<f64> {
        let b = self.bounds.lock().unwrap();
        let got = self.live().saturating_sub(b.base);
        let secs = b.since.elapsed().as_secs_f64();
        (got > 0 && secs >= 0.25).then(|| got as f64 / secs)
    }

    /// Reserve up to `len` bytes at the write position, clamped to the
    /// (possibly shrunk) end. Returns how many may be written; 0 means the
    /// segment is complete.
    fn claim(&self, len: usize) -> usize {
        let mut b = self.bounds.lock().unwrap();
        let pos = b.start + self.live();
        let room = b.end.saturating_add(1).saturating_sub(pos);
        let n = room.min(len as u64);
        b.frontier = b.frontier.max(pos + n);
        n as usize
    }

    /// Bytes not yet claimed. Unplaced counters (yt-dlp, whole-file) have no
    /// end and report 0, so they are never split.
    fn remaining(&self) -> u64 {
        let b = self.bounds.lock().unwrap();
        if b.end == u64::MAX {
            return 0;
        }
        let pos = b.frontier.max(b.start + self.live());
        b.end.saturating_add(1).saturating_sub(pos)
    }

    /// Give away the upper half of what is left. Returns the range given away.
    fn split(&self) -> Option<(u64, u64)> {
        let mut b = self.bounds.lock().unwrap();
        if b.end == u64::MAX {
            return None;
        }
        let pos = b.frontier.max(b.start + self.live());
        let left = b.end.saturating_add(1).saturating_sub(pos);
        if left < 2 * MIN_SPLIT_BYTES {
            return None;
        }
        let mid = pos + left / 2;
        let taken = (mid, b.end);
        b.end = mid - 1;
        Some(taken)
    }

    fn add(&self, n: u64) {
        self.live.fetch_add(n, Ordering::Relaxed);
    }

    /// Set both counters to an absolute value. Used by the yt-dlp path, which
    /// reports cumulative downloaded bytes rather than deltas.
    fn set(&self, n: u64) {
        self.live.store(n, Ordering::Relaxed);
        self.durable.store(n, Ordering::Relaxed);
    }

    /// Call only after the write is on the device.
    fn commit(&self) {
        self.durable.store(self.live.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    fn live(&self) -> u64 {
        self.live.load(Ordering::Relaxed)
    }

    fn durable(&self) -> u64 {
        self.durable.load(Ordering::Relaxed)
    }

    /// A retry restarts from the last durable point, so anything written but
    /// not yet synced is discarded and fetched again.
    fn rewind_to_durable(&self) {
        self.live.store(self.durable.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    fn reset(&self) {
        self.live.store(0, Ordering::Relaxed);
        self.durable.store(0, Ordering::Relaxed);
    }
}

/// Lock-free progress: one counter pair per segment.
///
/// Per-segment rather than one shared total, so a retry that resumes at
/// `start + done` neither loses nor double-counts. A single shared counter
/// would over-report after any retry — enough to make the final length check
/// reject a perfectly good file.
#[derive(Clone)]
pub struct Progress {
    /// Grows when a finished connection takes over half of another's range.
    counters: Arc<Mutex<Vec<Arc<SegmentProgress>>>>,
    /// Set once `set_layout` has pinned each counter to a byte range. Until
    /// then (yt-dlp, whole-file) there is no layout worth persisting.
    placed: Arc<AtomicBool>,
}

impl Progress {
    pub fn new(n: usize) -> Self {
        Progress::from_counters((0..n.max(1)).map(|_| SegmentProgress::default()).collect())
    }

    /// Rebuild from persisted per-segment offsets so a resumed download picks
    /// up exactly where the last durable checkpoint left it.
    pub fn resumed(done: &[u64]) -> Self {
        Progress::from_counters(done.iter().map(|d| SegmentProgress::starting_at(*d)).collect())
    }

    fn from_counters(counters: Vec<SegmentProgress>) -> Self {
        Progress {
            counters: Arc::new(Mutex::new(counters.into_iter().map(Arc::new).collect())),
            placed: Arc::new(AtomicBool::new(false)),
        }
    }

    fn counter(&self, i: usize) -> Arc<SegmentProgress> {
        Arc::clone(&self.counters.lock().unwrap()[i])
    }

    /// Pin each counter to its byte range. Counters of the wrong shape (a
    /// queue.json from a different plan) are replaced with fresh zeros:
    /// re-downloading is safe, trusting mismatched offsets is not.
    fn set_layout(&self, ranges: &[(u64, u64)]) {
        let mut counters = self.counters.lock().unwrap();
        if counters.len() != ranges.len() {
            *counters = ranges.iter().map(|_| Arc::new(SegmentProgress::default())).collect();
        }
        for (c, &(start, end)) in counters.iter().zip(ranges) {
            c.place(start, end);
        }
        self.placed.store(true, Ordering::Relaxed);
    }

    /// Split the segment with the most bytes left whose split still pays for
    /// a new connection, and return the index of the counter covering its
    /// upper half. On a far host a fresh connection spends over a second
    /// before its first byte, so handing it a small tail slows the finish.
    fn split_largest(&self) -> Option<usize> {
        let mut counters = self.counters.lock().unwrap();
        let setup = median(counters.iter().filter_map(|c| c.ttfb()).collect()).unwrap_or(0.0);
        // A segment still waiting on its first byte has no speed yet; assume
        // the typical one rather than treating it as infinitely slow.
        let typical = median(counters.iter().filter_map(|c| c.rate()).collect());
        let mut candidates: Vec<_> = counters.iter().map(|c| (c.remaining(), Arc::clone(c))).collect();
        candidates.sort_by_key(|(left, _)| std::cmp::Reverse(*left));
        for (left, victim) in candidates {
            if !worth_splitting(left, victim.rate().or(typical), setup) {
                continue;
            }
            if let Some((start, end)) = victim.split() {
                counters.push(Arc::new(SegmentProgress::with_range(start, end)));
                return Some(counters.len() - 1);
            }
        }
        None
    }

    /// Set the first counter to an absolute byte count (yt-dlp path).
    pub fn set_absolute(&self, n: u64) {
        self.counters.lock().unwrap()[0].set(n);
    }

    /// What the user sees.
    pub fn total(&self) -> u64 {
        self.counters.lock().unwrap().iter().map(|c| c.live()).sum()
    }

    /// Durable bytes only. Persistence goes through `layout`, which also
    /// carries the ranges; this is the tests' shorthand.
    #[cfg(test)]
    pub fn snapshot(&self) -> Vec<u64> {
        self.counters.lock().unwrap().iter().map(|c| c.durable()).collect()
    }

    /// Ranges and durable offsets read together, so a split landing between
    /// two separate reads can never persist mismatched lengths. Ranges are
    /// `None` until `set_layout` ran; the plan's own ranges stand then.
    pub fn layout(&self) -> (Option<Vec<(u64, u64)>>, Vec<u64>) {
        let counters = self.counters.lock().unwrap();
        let done = counters.iter().map(|c| c.durable()).collect();
        let ranges = self.placed.load(Ordering::Relaxed).then(|| {
            counters
                .iter()
                .map(|c| {
                    let b = c.bounds.lock().unwrap();
                    (b.start, b.end)
                })
                .collect()
        });
        (ranges, done)
    }

    fn reset(&self) {
        for c in self.counters.lock().unwrap().iter() {
            c.reset();
        }
    }
}

/// Which engine handles a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    /// spool's own segmented HTTP engine.
    #[default]
    Http,
    /// Delegated to yt-dlp (streaming sites).
    YtDlp,
    /// Plain FTP, resumed with `REST` (see `ftp.rs`).
    Ftp,
}

/// Everything decided before the first byte moves: where the file goes, how
/// big it is, how it splits, and what validator proves it has not changed.
///
/// Persisted in `queue.json` so a resume after a restart reproduces the exact
/// same layout instead of re-probing and possibly choosing differently.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DownloadPlan {
    pub url: String,
    pub final_path: PathBuf,
    pub part_path: PathBuf,
    pub total: Option<u64>,
    pub supports_ranges: bool,
    pub validator: Option<String>,
    pub ranges: Vec<(u64, u64)>,
    #[serde(default)]
    pub engine: Engine,
    /// Remote thumbnail URL for a preview (video downloads). `None` for files.
    #[serde(default)]
    pub thumbnail: Option<String>,
}

impl DownloadPlan {
    pub fn filename(&self) -> String {
        self.final_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn segment_count(&self) -> usize {
        self.ranges.len().max(1)
    }
}

/// Build a yt-dlp plan. `title`/`thumbnail` come from a metadata resolve when
/// available; the display name falls back to a host placeholder and is replaced
/// with the real filename once yt-dlp finishes.
pub fn video_plan(
    url: &str,
    dest_dir: &Path,
    title: Option<String>,
    thumbnail: Option<String>,
    custom: Option<&str>,
) -> Result<DownloadPlan, String> {
    let parsed = validate_url(url)?;
    let host = parsed.host_str().unwrap_or("video").to_string();
    // A name typed in the add dialog wins over the resolved title. yt-dlp still
    // picks the container, so the extension is appended once it names the file.
    let name = custom
        .and_then(sanitize_filename)
        .or_else(|| title.as_deref().and_then(sanitize_filename))
        .unwrap_or_else(|| format!("{host} video"));
    Ok(DownloadPlan {
        url: parsed.to_string(),
        final_path: dest_dir.join(&name),
        part_path: dest_dir.join(format!("{name}.part")),
        total: None,
        supports_ranges: false,
        validator: None,
        ranges: Vec::new(),
        engine: Engine::YtDlp,
        thumbnail,
    })
}

/// Give `name` the extension from `fallback` when the user did not type one.
/// Renaming "report" over "invoice.pdf" should still land a `.pdf`, but a
/// deliberate "notes.txt" is left exactly as typed.
pub fn keep_extension(name: &str, fallback: &str) -> String {
    if Path::new(name).extension().is_some() {
        return name.to_string();
    }
    match Path::new(fallback).extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{name}.{ext}"),
        None => name.to_string(),
    }
}

/// Probe the server and reserve a name, without transferring anything.
/// HTTP engine only — video routing happens before this in `state`.
pub async fn prepare(
    client: &Client,
    url: &str,
    dest_dir: &Path,
    segments: u32,
    categorize: bool,
    custom_name: Option<&str>,
) -> Result<DownloadPlan, String> {
    let parsed = validate_url(url)?;
    let mut info = probe(client, &parsed).await?;

    // A name typed in the add dialog replaces the one the server suggested.
    if let Some(name) = custom_name.and_then(sanitize_filename) {
        info.filename = keep_extension(&name, &info.filename);
    }

    // The type is only known once the probe has resolved a filename, so the
    // category sub-folder is chosen here rather than by the caller.
    let owned_dir;
    let dest_dir = if categorize {
        owned_dir = dest_dir.join(category_for(&info.filename));
        owned_dir.as_path()
    } else {
        dest_dir
    };

    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| format!("cannot create {}: {e}", dest_dir.display()))?;

    // Reject an oversized download at second 0 rather than hours in.
    if let Some(total) = info.total {
        check_disk_space(dest_dir, total)?;
    }

    let final_path = resolve_unique_path(dest_dir, &info.filename);
    let part = part_path(&final_path);

    // Claim the name now by creating the `.part`. `resolve_unique_path` treats
    // an existing `.part` as taken, so without this two adds started before
    // either begins transferring would pick the same name and write the same
    // file. `create_new` fails if someone won the race first, so re-resolve.
    let (final_path, part) = match tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&part)
        .await
    {
        Ok(_) => (final_path, part),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let retry = resolve_unique_path(dest_dir, &info.filename);
            let retry_part = part_path(&retry);
            let _ = tokio::fs::File::create(&retry_part).await;
            (retry, retry_part)
        }
        Err(e) => return Err(format!("cannot create {}: {e}", part.display())),
    };

    let ranges = match info.total {
        Some(total) if info.supports_ranges => plan_segments(total, segments),
        _ => Vec::new(),
    };

    Ok(DownloadPlan {
        url: parsed.to_string(),
        final_path,
        part_path: part,
        total: info.total,
        supports_ranges: info.supports_ranges,
        validator: info.validator,
        ranges,
        engine: Engine::Http,
        thumbnail: None,
    })
}

/// Run (or resume) a prepared plan.
///
/// `progress` carries the starting offsets; build it with `Progress::resumed`
/// to continue an interrupted transfer, or `Progress::new` to start fresh.
#[allow(clippy::too_many_arguments)]
pub async fn run<F>(
    client: &Client,
    segment_client: &Client,
    plan: &DownloadPlan,
    progress: &Progress,
    throttle: &Throttle,
    token: CancellationToken,
    on_progress: F,
) -> Result<PathBuf, String>
where
    F: Fn(u64, Option<u64>) + Send + Sync + 'static,
{
    let url = validate_url(&plan.url)?;

    // Nothing to stream.
    if plan.total == Some(0) {
        File::create(&plan.final_path)
            .await
            .map_err(|e| format!("cannot create {}: {e}", plan.final_path.display()))?;
        on_progress(0, Some(0));
        return Ok(plan.final_path.clone());
    }

    let info = RemoteInfo {
        total: plan.total,
        supports_ranges: plan.supports_ranges,
        filename: plan.filename(),
        validator: plan.validator.clone(),
    };

    let on_progress = Arc::new(on_progress);
    let ticker = {
        let progress = progress.clone();
        let on_progress = Arc::clone(&on_progress);
        let total = plan.total;
        let token = token.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PROGRESS_INTERVAL);
            // Default is Burst: after a runtime stall the missed ticks fire
            // back to back, which would report a nonsense instantaneous rate.
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = interval.tick() => on_progress(progress.total(), total),
                }
            }
        })
    };

    let outcome = transfer(
        client,
        segment_client,
        &url,
        &plan.part_path,
        &info,
        plan.ranges.clone(),
        progress,
        throttle,
        &token,
    )
    .await;

    ticker.abort();
    let written = outcome?;

    // The `.part` is pre-allocated to full size, so its length on disk proves
    // nothing. This comparison is the only thing that catches a short read.
    if let Some(expected) = plan.total {
        if written != expected {
            return Err(format!(
                "incomplete download: got {written} bytes, expected {expected}. \
                 Partial file kept at {}",
                plan.part_path.display()
            ));
        }
    }

    tokio::fs::rename(&plan.part_path, &plan.final_path)
        .await
        .map_err(|e| format!("cannot finalize {}: {e}", plan.final_path.display()))?;

    on_progress(written, plan.total);
    Ok(plan.final_path.clone())
}

/// Free space on the volume that will hold the file.
fn check_disk_space(dest_dir: &Path, needed: u64) -> Result<(), String> {
    let available = fs4::available_space(dest_dir)
        .map_err(|e| format!("cannot check free space on {}: {e}", dest_dir.display()))?;
    if available < needed.saturating_add(DISK_HEADROOM) {
        return Err(format!(
            "not enough disk space: need {}, {} free",
            human_bytes(needed),
            human_bytes(available)
        ));
    }
    Ok(())
}

fn human_bytes(n: u64) -> String {
    // Floor at KB to match the UI: never surface raw bytes to the user.
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let dec = if value == 0.0 || value >= 10.0 { 0 } else { 1 };
    format!("{value:.dec$} {}", UNITS[unit])
}

/// Stream one range into `path` at its own offset, retrying on transient
/// failures.
///
/// `done` counts bytes already written for this segment, so every attempt
/// resumes at `start + done` — no gap, no rewritten bytes, no double count.
#[allow(clippy::too_many_arguments)]
async fn run_segment(
    client: Client,
    url: Url,
    path: PathBuf,
    validator: Option<String>,
    done: Arc<SegmentProgress>,
    throttle: Throttle,
    token: CancellationToken,
) -> Result<(), SegErr> {
    let mut attempt = 0usize;

    loop {
        if token.is_cancelled() {
            return Err(SegErr::Cancelled);
        }
        // Re-read every attempt: another connection may have taken the tail.
        let end = done.end();
        let offset = done.start() + done.live();
        if offset > end {
            return Ok(());
        }

        let before = done.durable();
        let result = match stream_range(&client, &url, offset, end, &path, &validator, &done, &throttle, &token).await {
            // A body that ends before the range does is a dropped connection,
            // not a finished segment. Treating it as done left a hole that
            // only the final length check caught, failing the whole download.
            Ok(()) if done.start() + done.live() <= done.end() => {
                Err(SegErr::Retryable("server closed the connection early".into()))
            }
            other => other,
        };
        match result {
            Ok(()) => return Ok(()),
            Err(SegErr::Cancelled) => return Err(SegErr::Cancelled),
            Err(SegErr::Fatal(m)) => return Err(SegErr::Fatal(m)),
            Err(SegErr::RangeIgnored) => return Err(SegErr::RangeIgnored),
            Err(SegErr::Retryable(m)) => {
                // The failed attempt may have left unsynced bytes in flight.
                // Restart from the last point known to be on the device.
                done.rewind_to_durable();

                // A segment that made real headway since the last failure gets
                // a fresh budget: the ladder is per stall episode, not per
                // download lifetime, or a long transfer dies to a handful of
                // unrelated blips spread over hours.
                if done.durable().saturating_sub(before) >= RETRY_RESET_BYTES {
                    attempt = 0;
                }

                if attempt >= BACKOFF_SECS.len() {
                    return Err(SegErr::Fatal(format!(
                        "giving up after {} retries: {m}",
                        BACKOFF_SECS.len()
                    )));
                }
                let wait = Duration::from_secs(BACKOFF_SECS[attempt]);
                attempt += 1;

                tokio::select! {
                    _ = token.cancelled() => return Err(SegErr::Cancelled),
                    _ = tokio::time::sleep(wait) => {}
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_range(
    client: &Client,
    url: &Url,
    offset: u64,
    end: u64,
    path: &Path,
    validator: &Option<String>,
    done: &Arc<SegmentProgress>,
    throttle: &Throttle,
    token: &CancellationToken,
) -> Result<(), SegErr> {
    let mut request = client
        .get(url.clone())
        .header(RANGE, format!("bytes={offset}-{end}"));

    // `If-Range` is the standard way to make a resume safe: the server returns
    // 206 if the resource is unchanged, or 200 with the whole body if it is
    // not. The 200 branch below already treats that as "restart from zero", so
    // this replaces a second round trip plus a hand-rolled ETag comparison.
    if let Some(v) = validator {
        request = request.header(IF_RANGE, v.clone());
    }

    let asked = Instant::now();
    let response = request
        .send()
        .await
        .map_err(|e| SegErr::Retryable(format!("request failed: {e}")))?;
    done.note_ttfb(asked.elapsed());

    let status = response.status();
    if status == StatusCode::OK {
        // Range ignored or resource changed. Never write this at an offset.
        return Err(SegErr::RangeIgnored);
    }
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        return Err(SegErr::Fatal("server rejected the byte range".into()));
    }
    if status != StatusCode::PARTIAL_CONTENT {
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(SegErr::Retryable(format!("server returned {status}")));
        }
        return Err(SegErr::Fatal(describe_http_error(status, response.headers())));
    }

    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .map_err(|e| SegErr::Fatal(format!("cannot open {}: {e}", path.display())))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| SegErr::Fatal(format!("seek failed: {e}")))?;

    let mut stream = response.bytes_stream();
    let mut since_checkpoint = 0u64;

    loop {
        let next = tokio::select! {
            _ = token.cancelled() => return Err(SegErr::Cancelled),
            r = tokio::time::timeout(CHUNK_TIMEOUT, stream.next()) => r,
        };

        let next = next.map_err(|_| SegErr::Retryable("connection stalled: no data for 30s".into()))?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| SegErr::Retryable(format!("transfer failed: {e}")))?;

        // Claim before writing: if another connection took over our tail, the
        // part of this chunk past the new end belongs to it and is dropped.
        let n = done.claim(chunk.len());
        if n == 0 {
            break;
        }

        // Spend bandwidth budget before writing. Stays cancellable so a pause
        // does not hang waiting on permits.
        tokio::select! {
            _ = token.cancelled() => return Err(SegErr::Cancelled),
            _ = throttle.take(n) => {}
        }

        file.write_all(&chunk[..n])
            .await
            .map_err(|e| SegErr::Fatal(format!("write failed: {e}")))?;

        done.add(n as u64);
        since_checkpoint += n as u64;

        // Durability checkpoint. `flush` alone only reaches the OS page cache,
        // so the durable counter must not advance until an actual fdatasync
        // has returned — otherwise a power cut leaves the recorded offset
        // pointing at bytes that never made it to the device, and resume seeks
        // past a zero-filled hole.
        if since_checkpoint >= CHECKPOINT_BYTES {
            file.sync_data()
                .await
                .map_err(|e| SegErr::Fatal(format!("sync failed: {e}")))?;
            done.commit();
            since_checkpoint = 0;
        }

        if n < chunk.len() {
            break;
        }
    }

    file.sync_data()
        .await
        .map_err(|e| SegErr::Fatal(format!("sync failed: {e}")))?;
    done.commit();
    Ok(())
}

/// Single-connection transfer from byte 0. Used when the server has no range
/// support, when the size is unknown, and as the downgrade path when a server
/// answers a ranged request with `200`.
async fn stream_whole(
    client: &Client,
    url: &Url,
    path: &Path,
    progress: &Progress,
    throttle: &Throttle,
    token: &CancellationToken,
) -> Result<u64, String> {
    let mut attempt = 0usize;

    loop {
        // This path always restarts from byte 0, so the counter restarts too.
        progress.reset();

        let result = stream_whole_once(client, url, path, &progress.counter(0), throttle, token).await;
        match result {
            Ok(written) => return Ok(written),
            Err(SegErr::Cancelled) => return Err("cancelled".into()),
            Err(SegErr::Fatal(m)) => return Err(m),
            Err(e) => {
                if attempt >= BACKOFF_SECS.len() {
                    return Err(format!("giving up after {} retries: {}", BACKOFF_SECS.len(), e.message()));
                }
                let wait = Duration::from_secs(BACKOFF_SECS[attempt]);
                attempt += 1;
                tokio::select! {
                    _ = token.cancelled() => return Err("cancelled".into()),
                    _ = tokio::time::sleep(wait) => {}
                }
            }
        }
    }
}

async fn stream_whole_once(
    client: &Client,
    url: &Url,
    path: &Path,
    done: &Arc<SegmentProgress>,
    throttle: &Throttle,
    token: &CancellationToken,
) -> Result<u64, SegErr> {
    let response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| SegErr::Retryable(format!("request failed: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(SegErr::Retryable(format!("server returned {status}")));
        }
        return Err(SegErr::Fatal(describe_http_error(status, response.headers())));
    }

    let mut file = File::create(path)
        .await
        .map_err(|e| SegErr::Fatal(format!("cannot create {}: {e}", path.display())))?;

    let mut stream = response.bytes_stream();
    let mut written = 0u64;
    let mut since_checkpoint = 0u64;

    loop {
        let next = tokio::select! {
            _ = token.cancelled() => return Err(SegErr::Cancelled),
            r = tokio::time::timeout(CHUNK_TIMEOUT, stream.next()) => r,
        };

        let next = next.map_err(|_| SegErr::Retryable("connection stalled: no data for 30s".into()))?;
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|e| SegErr::Retryable(format!("transfer failed: {e}")))?;

        tokio::select! {
            _ = token.cancelled() => return Err(SegErr::Cancelled),
            _ = throttle.take(chunk.len()) => {}
        }

        file.write_all(&chunk)
            .await
            .map_err(|e| SegErr::Fatal(format!("write failed: {e}")))?;

        written += chunk.len() as u64;
        done.add(chunk.len() as u64);
        since_checkpoint += chunk.len() as u64;

        if since_checkpoint >= CHECKPOINT_BYTES {
            file.sync_data().await.map_err(|e| SegErr::Fatal(format!("sync failed: {e}")))?;
            done.commit();
            since_checkpoint = 0;
        }
    }

    file.sync_data().await.map_err(|e| SegErr::Fatal(format!("sync failed: {e}")))?;
    done.commit();
    Ok(written)
}


/// Pick a strategy and run it, returning the byte count actually written.
#[allow(clippy::too_many_arguments)]
async fn transfer(
    client: &Client,
    segment_client: &Client,
    url: &Url,
    part: &Path,
    info: &RemoteInfo,
    ranges: Vec<(u64, u64)>,
    progress: &Progress,
    throttle: &Throttle,
    token: &CancellationToken,
) -> Result<u64, String> {
    // No size, no range support, or a file too small to be worth splitting:
    // one connection, streamed to EOF.
    if ranges.len() <= 1 {
        return stream_whole(client, url, part, progress, throttle, token).await;
    }

    let total = info.total.expect("ranges implies a known total");

    // Physically reserve the blocks. `set_len` alone would make a sparse file
    // that succeeds instantly and then dies with ENOSPC hours later.
    //
    // `create(true).write(true)` without `truncate`: on a resume the `.part`
    // already holds real bytes, and truncating it here would silently discard
    // everything the saved offsets say we already have.
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(part)
        .await
        .map_err(|e| format!("cannot open {}: {e}", part.display()))?;
    file.allocate(total)
        .await
        .map_err(|e| format!("cannot reserve {} on disk: {e}", human_bytes(total)))?;
    file.sync_all().await.map_err(|e| format!("sync failed: {e}"))?;
    drop(file);

    // One root token per download, a child per segment: cancelling the root
    // tears down every segment's network loop at once, with no dangling
    // sockets left behind.
    let segment_token = token.child_token();
    let range_ignored = Arc::new(AtomicBool::new(false));
    progress.set_layout(&ranges);

    let spawn = |tasks: &mut tokio::task::JoinSet<Result<(), SegErr>>, i: usize| {
        tasks.spawn(run_segment(
            segment_client.clone(),
            url.clone(),
            part.to_path_buf(),
            info.validator.clone(),
            progress.counter(i),
            throttle.clone(),
            segment_token.child_token(),
        ));
    };
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..ranges.len() {
        spawn(&mut tasks, i);
    }

    let mut first_error: Option<String> = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            // A connection just freed up. Rather than idle while the slowest
            // segment crawls to the finish, it takes half of whatever range has
            // the most left, so the tail no longer dictates the finish time.
            Ok(Ok(())) => {
                if !segment_token.is_cancelled() {
                    if let Some(i) = progress.split_largest() {
                        spawn(&mut tasks, i);
                    }
                }
            }
            Ok(Err(SegErr::RangeIgnored)) => {
                range_ignored.store(true, Ordering::Relaxed);
                segment_token.cancel();
            }
            Ok(Err(e)) => {
                if first_error.is_none() && !matches!(e, SegErr::Cancelled) {
                    first_error = Some(e.message());
                }
                segment_token.cancel();
            }
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(format!("segment task failed: {e}"));
                }
                segment_token.cancel();
            }
        }
    }

    if range_ignored.load(Ordering::Relaxed) {
        // The server ignored Range (or the resource changed under an
        // If-Range). Every segment offset is now meaningless, so throw the
        // partial away and take the whole file down one connection.
        if token.is_cancelled() {
            return Err("cancelled".into());
        }
        return stream_whole(client, url, part, progress, throttle, token).await;
    }

    if let Some(err) = first_error {
        return Err(err);
    }
    if token.is_cancelled() {
        return Err("cancelled".into());
    }

    Ok(progress.total())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_quoted() {
        let got = filename_from_content_disposition(r#"attachment; filename="report.pdf""#);
        assert_eq!(got.as_deref(), Some("report.pdf"));
    }

    #[test]
    fn content_disposition_unquoted() {
        let got = filename_from_content_disposition("attachment; filename=report.pdf");
        assert_eq!(got.as_deref(), Some("report.pdf"));
    }

    #[test]
    fn content_disposition_semicolon_inside_quotes() {
        let got = filename_from_content_disposition(r#"attachment; filename="a;b.zip""#);
        assert_eq!(got.as_deref(), Some("a;b.zip"));
    }

    #[test]
    fn content_disposition_rfc5987() {
        let got =
            filename_from_content_disposition("attachment; filename*=UTF-8''caf%C3%A9%20menu.pdf");
        assert_eq!(got.as_deref(), Some("café menu.pdf"));
    }

    #[test]
    fn content_disposition_extended_wins_over_plain() {
        let got = filename_from_content_disposition(
            r#"attachment; filename="fallback.bin"; filename*=UTF-8''real%20name.zip"#,
        );
        assert_eq!(got.as_deref(), Some("real name.zip"));
    }

    #[test]
    fn content_disposition_absent_filename() {
        assert_eq!(filename_from_content_disposition("inline"), None);
    }

    #[test]
    fn sanitize_rejects_traversal() {
        assert_eq!(sanitize_filename("../../.bashrc"), None);
        assert_eq!(sanitize_filename(".."), None);
        assert_eq!(sanitize_filename("."), None);
        assert_eq!(sanitize_filename(""), None);
        assert_eq!(sanitize_filename("/etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(sanitize_filename(r"..\..\evil.exe").as_deref(), Some("evil.exe"));
    }

    #[test]
    fn sanitize_replaces_invalid_chars() {
        assert_eq!(
            sanitize_filename(r#"a:b*c?d"e<f>g|h"#).as_deref(),
            Some("a_b_c_d_e_f_g_h")
        );
    }

    #[test]
    fn sanitize_strips_trailing_dots_and_control_chars() {
        assert_eq!(sanitize_filename("file.txt...").as_deref(), Some("file.txt"));
        assert_eq!(sanitize_filename("fi\u{7}le.txt").as_deref(), Some("file.txt"));
    }

    #[test]
    fn sanitize_keeps_ordinary_names() {
        assert_eq!(sanitize_filename("archive.tar.gz").as_deref(), Some("archive.tar.gz"));
    }

    #[test]
    fn resolve_filename_prefers_content_disposition() {
        let url = Url::parse("https://example.com/dl?token=xyz987").unwrap();
        assert_eq!(
            resolve_filename(Some(r#"attachment; filename="real.zip""#), &url),
            "real.zip"
        );
    }

    #[test]
    fn resolve_filename_falls_back_to_url_path() {
        let url = Url::parse("https://example.com/files/report%20final.pdf").unwrap();
        assert_eq!(resolve_filename(None, &url), "report final.pdf");
    }

    #[test]
    fn resolve_filename_falls_back_to_default() {
        let url = Url::parse("https://example.com/").unwrap();
        assert_eq!(resolve_filename(None, &url), "download.bin");
        let url2 = Url::parse("https://example.com/x").unwrap();
        assert_eq!(resolve_filename(Some("attachment; filename=\"../..\""), &url2), "x");
    }

    #[test]
    fn custom_name_keeps_source_extension() {
        // No extension typed: the source's is appended.
        assert_eq!(keep_extension("report", "invoice.pdf"), "report.pdf");
        // An extension typed: taken literally, even a different one.
        assert_eq!(keep_extension("notes.txt", "invoice.pdf"), "notes.txt");
        // Nothing to borrow.
        assert_eq!(keep_extension("report", "download"), "report");
        // A dotted name whose last part is the extension.
        assert_eq!(keep_extension("v1.2.3", "app.tar"), "v1.2.3");
    }

    #[test]
    fn custom_name_is_sanitised() {
        // A path in the name field must not escape the download folder.
        assert_eq!(sanitize_filename("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(sanitize_filename("a/b/c.bin").as_deref(), Some("c.bin"));
        assert_eq!(sanitize_filename("  ").as_deref(), None);
    }

    #[test]
    fn unique_path_suffixes_on_collision() {
        let dir = std::env::temp_dir().join(format!("spool-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let first = resolve_unique_path(&dir, "archive.tar.gz");
        assert_eq!(first.file_name().unwrap(), "archive.tar.gz");
        std::fs::write(&first, b"x").unwrap();

        // Multi-dot names keep the full stem: NOT `archive (1).gz`.
        let second = resolve_unique_path(&dir, "archive.tar.gz");
        assert_eq!(second.file_name().unwrap(), "archive.tar (1).gz");
        std::fs::write(&second, b"x").unwrap();

        let third = resolve_unique_path(&dir, "archive.tar.gz");
        assert_eq!(third.file_name().unwrap(), "archive.tar (2).gz");

        let plain = dir.join("README");
        std::fs::write(&plain, b"x").unwrap();
        assert_eq!(
            resolve_unique_path(&dir, "README").file_name().unwrap(),
            "README (1)"
        );

        // An in-flight `.part` also reserves its final name, so two concurrent
        // downloads of the same URL never write to the same file.
        std::fs::write(dir.join("busy.zip.part"), b"x").unwrap();
        assert_eq!(
            resolve_unique_path(&dir, "busy.zip").file_name().unwrap(),
            "busy (1).zip"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn part_path_stays_beside_final_file() {
        let p = part_path(Path::new("/home/u/Downloads/a.zip"));
        assert_eq!(p, PathBuf::from("/home/u/Downloads/a.zip.part"));
        assert_eq!(p.parent(), Path::new("/home/u/Downloads/a.zip").parent());
    }

    #[test]
    fn url_scheme_allowlist() {
        assert!(validate_url("https://example.com/f.zip").is_ok());
        assert!(validate_url("http://example.com/f.zip").is_ok());
        assert!(validate_url("ftp://example.com/f.zip").is_err());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("not a url").is_err());
    }

    #[test]
    fn content_range_total_parsing() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes 100-199/200"), Some(200));
        // Server declines to state the total.
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("bytes */1234"), Some(1234));
        assert_eq!(parse_content_range_total("garbage"), None);
    }

    /// Range headers are inclusive at both ends, so the seam between segments
    /// is the classic off-by-one. These assertions are the reason this
    /// function is separate from the transfer code.
    #[test]
    fn segments_tile_the_file_exactly() {
        for (total, n) in [
            (100u64, 1u32),
            (8 * 1024 * 1024, 8),
            (8 * 1024 * 1024 + 7, 8), // remainder must land on the last segment
            (5 * 1024 * 1024, 3),
            (4 * 1024 * 1024, 8),
        ] {
            let ranges = plan_segments(total, n);
            assert!(!ranges.is_empty(), "total={total} n={n}");
            assert_eq!(ranges[0].0, 0, "first segment must start at 0");
            assert_eq!(
                ranges.last().unwrap().1,
                total - 1,
                "last segment must end at the final byte (total={total})"
            );

            let mut covered = 0u64;
            for (i, (start, end)) in ranges.iter().enumerate() {
                assert!(start <= end, "inverted range at {i}: {start}..{end}");
                if i > 0 {
                    // No gap and no overlap at the seam.
                    assert_eq!(*start, ranges[i - 1].1 + 1, "seam broken at {i}");
                }
                covered += end - start + 1;
            }
            assert_eq!(covered, total, "coverage mismatch for total={total}");
        }
    }

    #[test]
    fn segments_respect_the_small_file_threshold() {
        // Below the threshold, extra sockets cost more than they gain.
        assert_eq!(plan_segments(1024, 8), vec![(0, 1023)]);
        assert_eq!(plan_segments(MIN_SEGMENTED_SIZE - 1, 8).len(), 1);
        assert_eq!(plan_segments(MIN_SEGMENTED_SIZE, 8).len(), 8);
        // Nothing to fetch.
        assert!(plan_segments(0, 8).is_empty());
        // Never more segments than the cap.
        assert_eq!(plan_segments(100 * 1024 * 1024, 99).len(), MAX_SEGMENTS as usize);
    }

    /// `set_len` would pass every assertion here except the last one: a sparse
    /// file reports the right length while occupying no blocks, which is how a
    /// too-small disk fails hours into a download instead of at second 0.
    #[cfg(unix)]
    #[tokio::test]
    async fn allocate_reserves_real_disk_blocks() {
        use std::os::unix::fs::MetadataExt;

        let dir = std::env::temp_dir().join(format!("spool-alloc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("reserved.bin");
        let size: u64 = 4 * 1024 * 1024;

        let file = File::create(&path).await.unwrap();
        file.allocate(size).await.unwrap();
        file.sync_all().await.unwrap();
        drop(file);

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), size, "logical size");
        // st_blocks counts 512-byte units actually committed on the device.
        assert!(
            meta.blocks() * 512 >= size,
            "file is sparse: {} bytes reserved for a {size}-byte file",
            meta.blocks() * 512
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn categories_map_by_extension() {
        assert_eq!(category_for("clip.MP4"), "Video");
        assert_eq!(category_for("song.flac"), "Audio");
        assert_eq!(category_for("archive.tar.gz"), "Archives");
        assert_eq!(category_for("paper.pdf"), "Documents");
        assert_eq!(category_for("shot.jpeg"), "Images");
        assert_eq!(category_for("distro.iso"), "Programs");
        // Unknown and extensionless both fall through to Other.
        assert_eq!(category_for("data.xyz"), "Other");
        assert_eq!(category_for("README"), "Other");
    }

    #[test]
    fn human_bytes_reads_sensibly() {
        assert_eq!(human_bytes(0), "0 KB");
        assert_eq!(human_bytes(512), "0.5 KB");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(512 * 1024), "512 KB");
        assert_eq!(human_bytes(1536 * 1024), "1.5 MB");
        assert_eq!(human_bytes(100 * 1024 * 1024), "100 MB");
    }

    // Network tests. Excluded from the default run because they depend on
    // third-party hosts staying up: `cargo test -- --ignored --nocapture`.

    const SMALL: &str = "https://proof.ovh.net/files/1Mb.dat";
    const BIG: &str = "https://proof.ovh.net/files/10Mb.dat";

    fn clients() -> (Client, Client) {
        let s = Session::default();
        (build_client(&s).unwrap(), build_segment_client(&s).unwrap())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("spool-net-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// `prepare` + `run` in one call, the way the tests want it.
    async fn download_file<F>(
        client: &Client,
        segment_client: &Client,
        url: &str,
        dest_dir: &Path,
        segments: u32,
        token: CancellationToken,
        on_progress: F,
    ) -> Result<PathBuf, String>
    where
        F: Fn(u64, Option<u64>) + Send + Sync + 'static,
    {
        let plan = prepare(client, url, dest_dir, segments, false, None).await?;
        let progress = Progress::new(plan.segment_count());
        run(client, segment_client, &plan, &progress, &Throttle::unlimited(), token, on_progress).await
    }

    #[tokio::test]
    #[ignore]
    async fn network_probe_reports_size_and_ranges() {
        let (client, _) = clients();
        let info = probe(&client, &Url::parse(SMALL).unwrap()).await.unwrap();
        assert_eq!(info.total, Some(1_048_576));
        assert!(info.supports_ranges, "host should advertise byte ranges");
        assert_eq!(info.filename, "1Mb.dat");
    }

    #[tokio::test]
    #[ignore]
    async fn network_single_connection_download() {
        let dir = scratch("single");
        let (client, seg) = clients();

        // 1 MB is under MIN_SEGMENTED_SIZE, so this exercises the
        // single-connection path even though the server supports ranges.
        let path = download_file(&client, &seg, SMALL, &dir, 8, CancellationToken::new(), |_, _| {})
            .await
            .unwrap();

        assert_eq!(path.file_name().unwrap(), "1Mb.dat");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 1_048_576);
        assert!(!part_path(&path).exists(), ".part must be renamed, not left behind");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn network_custom_name_lands_on_disk() {
        let dir = scratch("custom-name");
        let (client, _) = clients();

        // No extension typed: the source's ".dat" is kept.
        let plan = prepare(&client, SMALL, &dir, 4, false, Some("my report"))
            .await
            .unwrap();
        assert_eq!(plan.filename(), "my report.dat");
        assert!(plan.part_path.exists(), "the name should be reserved up front");

        // A path in the name must not escape the download folder.
        let escaped = prepare(&client, SMALL, &dir, 4, false, Some("../../evil.bin"))
            .await
            .unwrap();
        assert_eq!(escaped.filename(), "evil.bin");
        assert_eq!(escaped.final_path.parent().unwrap(), dir.as_path());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn network_segmented_download_matches_byte_for_byte() {
        let dir = scratch("segmented");
        let (client, seg) = clients();

        let path = download_file(&client, &seg, BIG, &dir, 8, CancellationToken::new(), |_, _| {})
            .await
            .unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 10 * 1024 * 1024);

        // The real proof that the segment offsets are right: fetch the same
        // file down one connection and compare every byte.
        let reference = dir.join("reference.bin");
        let progress = Progress::new(1);
        let written = stream_whole(
            &client,
            &Url::parse(BIG).unwrap(),
            &reference,
            &progress,
            &Throttle::unlimited(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(written, 10 * 1024 * 1024);

        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(&reference).unwrap(),
            "segmented output differs from single-connection output"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Two adds racing for the same URL must not resolve to the same file.
    /// Before the `.part` was reserved in `prepare`, both saw a free name.
    #[tokio::test]
    #[ignore]
    async fn network_concurrent_prepares_get_distinct_paths() {
        let dir = scratch("race");
        let (client, _) = clients();

        let (a, b) = tokio::join!(
            prepare(&client, SMALL, &dir, 8, false, None),
            prepare(&client, SMALL, &dir, 8, false, None),
        );
        let (a, b) = (a.unwrap(), b.unwrap());

        assert_ne!(a.final_path, b.final_path, "both adds claimed the same file");
        assert_ne!(a.part_path, b.part_path, "both adds claimed the same .part");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn network_suffixes_on_second_download() {
        let dir = scratch("suffix");
        let (client, seg) = clients();
        let t = CancellationToken::new();

        let first = download_file(&client, &seg, SMALL, &dir, 8, t.clone(), |_, _| {}).await.unwrap();
        let second = download_file(&client, &seg, SMALL, &dir, 8, t, |_, _| {}).await.unwrap();

        assert_eq!(first.file_name().unwrap(), "1Mb.dat");
        assert_eq!(second.file_name().unwrap(), "1Mb (1).dat");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn network_opaque_url_still_gets_a_name() {
        let dir = scratch("opaque");
        let (client, seg) = clients();

        let path = download_file(
            &client,
            &seg,
            "https://speed.cloudflare.com/__down?bytes=65536",
            &dir,
            8,
            CancellationToken::new(),
            |_, _| {},
        )
        .await
        .unwrap();

        assert_eq!(path.file_name().unwrap(), "__down");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 65_536);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn network_rejects_bad_scheme_before_touching_network() {
        let dir = scratch("scheme");
        let (client, seg) = clients();
        let err = download_file(&client, &seg, "ftp://example.com/f.zip", &dir, 8, CancellationToken::new(), |_, _| {})
            .await
            .unwrap_err();
        assert!(err.contains("unsupported scheme"), "got: {err}");
        assert!(!dir.exists(), "nothing should be created for a rejected URL");
    }

    #[tokio::test]
    #[ignore]
    async fn network_cancel_stops_the_transfer() {
        let dir = scratch("cancel");
        let (client, seg) = clients();
        let token = CancellationToken::new();

        let child = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            child.cancel();
        });

        let result = download_file(&client, &seg, BIG, &dir, 8, token, |_, _| {}).await;
        assert!(result.is_err(), "cancelled download must not report success");
        // The final file must never appear for a cancelled transfer.
        assert!(!dir.join("10Mb.dat").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The M4 claim, end to end: interrupt a segmented transfer, throw away
    /// the in-memory state, resume from nothing but the persisted per-segment
    /// offsets, and get a file identical to one fetched in a single pass.
    ///
    /// A resume that seeks to the wrong offset still produces a
    /// correctly-sized file, so only the byte comparison proves anything.
    #[tokio::test]
    #[ignore]
    async fn network_resume_from_persisted_offsets_is_byte_identical() {
        let dir = scratch("resume");
        let (client, seg) = clients();
        let total = 10 * 1024 * 1024u64;

        let plan = prepare(&client, BIG, &dir, 8, false, None).await.unwrap();
        assert_eq!(plan.ranges.len(), 8, "expected a segmented plan");

        // First attempt: cancel it mid-flight.
        let progress = Progress::new(plan.segment_count());
        let token = CancellationToken::new();
        let killer = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            killer.cancel();
        });
        let interrupted = run(&client, &seg, &plan, &progress, &Throttle::unlimited(), token, |_, _| {}).await;
        assert!(interrupted.is_err(), "cancelled run must not report success");

        // This is all that survives a crash: the durable offsets.
        let persisted = progress.snapshot();
        let got = persisted.iter().sum::<u64>();
        assert!(got < total, "should not have finished before the cancel");
        assert!(plan.part_path.exists(), ".part must survive an interruption");

        // Resume with a fresh Progress built only from those numbers.
        let resumed = Progress::resumed(&persisted);
        let path = run(&client, &seg, &plan, &resumed, &Throttle::unlimited(), CancellationToken::new(), |_, _| {})
            .await
            .unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().len(), total);

        let reference = dir.join("reference.bin");
        let fresh = Progress::new(1);
        stream_whole(
            &client,
            &Url::parse(BIG).unwrap(),
            &reference,
            &fresh,
            &Throttle::unlimited(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(&reference).unwrap(),
            "resumed file differs from a single-pass download"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // -- plan_segments edges ------------------------------------------------

    /// Every byte of the file must be covered exactly once, whatever the
    /// remainder: a gap leaves a hole, an overlap corrupts.
    #[test]
    fn segments_tile_awkward_totals_without_gap_or_overlap() {
        for total in [
            MIN_SEGMENTED_SIZE,
            MIN_SEGMENTED_SIZE + 1,
            MIN_SEGMENTED_SIZE + 7,
            10 * 1024 * 1024 + 3,
            u32::MAX as u64 + 12345,
        ] {
            for segments in 1..=MAX_SEGMENTS {
                let ranges = plan_segments(total, segments);
                assert_eq!(ranges[0].0, 0, "total={total} n={segments} must start at 0");
                assert_eq!(
                    ranges.last().unwrap().1,
                    total - 1,
                    "total={total} n={segments} must end at the last byte"
                );
                for w in ranges.windows(2) {
                    assert_eq!(w[1].0, w[0].1 + 1, "total={total} n={segments} has a gap or overlap");
                }
                let covered: u64 = ranges.iter().map(|(a, b)| b - a + 1).sum();
                assert_eq!(covered, total, "total={total} n={segments} miscounts bytes");
            }
        }
    }

    #[test]
    fn segments_handle_degenerate_inputs() {
        // Nothing to fetch: no ranges at all, so `run` takes its empty-file path.
        assert!(plan_segments(0, 8).is_empty());

        // A one-byte file is a single inclusive [0, 0] range, not an empty one.
        assert_eq!(plan_segments(1, 8), vec![(0, 0)]);

        // 0 segments would divide by zero; it is clamped to 1.
        assert_eq!(plan_segments(1024, 0), vec![(0, 1023)]);

        // More segments than bytes would produce empty ranges.
        let ranges = plan_segments(MIN_SEGMENTED_SIZE, MAX_SEGMENTS);
        assert!(ranges.iter().all(|(a, b)| b >= a), "no empty ranges");
    }

    // -- URL validation -----------------------------------------------------

    #[test]
    fn url_validation_rejects_non_absolute_and_odd_schemes() {
        assert!(validate_url("").is_err());
        assert!(validate_url("example.com/file.zip").is_err(), "no scheme");
        assert!(validate_url("/local/path").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("data:text/plain,hi").is_err());
        // The scheme is case-insensitive per RFC 3986; the parser lowercases it.
        assert!(validate_url("HTTPS://example.com/a.zip").is_ok());
    }

    // -- Progress -----------------------------------------------------------

    #[test]
    fn progress_resumes_from_persisted_offsets() {
        let p = Progress::resumed(&[100, 250, 0]);
        assert_eq!(p.total(), 350);
        assert_eq!(p.snapshot(), vec![100, 250, 0], "resumed offsets are already durable");

        // Fresh bytes count as live immediately but not as durable.
        p.counter(2).add(50);
        assert_eq!(p.total(), 400);
        assert_eq!(p.snapshot(), vec![100, 250, 0]);

        p.counter(2).commit();
        assert_eq!(p.snapshot(), vec![100, 250, 50]);
    }

    #[test]
    fn progress_always_has_at_least_one_counter() {
        // `Progress::new(0)` would otherwise panic in `set_absolute`, which the
        // yt-dlp path calls on a plan with no ranges.
        let p = Progress::new(0);
        p.set_absolute(4096);
        assert_eq!(p.total(), 4096);
        assert_eq!(p.snapshot().len(), 1);
    }

    #[test]
    fn set_absolute_replaces_rather_than_accumulates() {
        // yt-dlp reports a cumulative figure, so each tick overwrites; adding
        // would double-count the whole download.
        let p = Progress::new(1);
        p.set_absolute(1000);
        p.set_absolute(2500);
        assert_eq!(p.total(), 2500);
        // Absolute sets are durable at once: there is no separate fsync to wait
        // for, yt-dlp owns the file.
        assert_eq!(p.snapshot(), vec![2500]);

        // A backwards report is honoured, not clamped — the caller (run_video)
        // is what banks finished phases.
        p.set_absolute(10);
        assert_eq!(p.total(), 10);
    }

    #[test]
    fn split_hands_off_the_upper_half_above_the_claim_frontier() {
        let mib = 1024 * 1024;
        let seg = SegmentProgress::with_range(0, 10 * mib - 1);
        // An in-flight chunk is claimed but not yet counted as written.
        assert_eq!(seg.claim(mib as usize), mib as usize);
        let (start, end) = seg.split().expect("9 MiB left is worth splitting");
        assert!(start >= mib, "a split must never cut into claimed bytes");
        assert_eq!(end, 10 * mib - 1);
        assert_eq!(seg.end(), start - 1);

        // The old owner is clamped to its new end.
        seg.add(mib);
        let room = (seg.end() + 1 - mib) as usize;
        assert_eq!(seg.claim(room + 4096), room);
        seg.add(room as u64);
        assert_eq!(seg.claim(4096), 0, "a full segment claims nothing");
    }

    #[test]
    fn small_tails_and_unplaced_counters_are_not_split() {
        let seg = SegmentProgress::with_range(0, 2 * MIN_SPLIT_BYTES - 2);
        assert_eq!(seg.split(), None);
        assert_eq!(SegmentProgress::default().split(), None);
    }

    #[test]
    fn small_tails_are_not_worth_a_slow_new_connection() {
        let two_mib = 2 * 1024 * 1024;
        // Far host, 1.3 s before a new connection's first byte. 2 MiB left at
        // 1.4 MB/s: the half would take 0.75 s, less than 2 setups.
        assert!(!worth_splitting(two_mib, Some(1.4e6), 1.3));
        // 40 MiB left at the same speed: the half takes 15 s. Worth it.
        assert!(worth_splitting(20 * two_mib, Some(1.4e6), 1.3));
        // The same small tail on a LAN (10 ms setup) is worth it.
        assert!(worth_splitting(two_mib, Some(1.4e6), 0.01));
        // Nothing measured yet: split, as before.
        assert!(worth_splitting(two_mib, None, 1.3));
    }

    #[test]
    fn splits_keep_the_layout_tiling_the_file() {
        let total = 64 * 1024 * 1024u64;
        let p = Progress::new(4);
        p.set_layout(&plan_segments(total, 4));

        p.counter(0).add(total / 4); // first segment finished
        for _ in 0..6 {
            p.split_largest().expect("plenty left to split");
        }

        let (ranges, done) = p.layout();
        let mut ranges = ranges.expect("placed progress persists its ranges");
        assert_eq!(ranges.len(), done.len(), "persisted shapes must match");
        ranges.sort();
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges.last().unwrap().1, total - 1);
        for w in ranges.windows(2) {
            assert_eq!(w[0].1 + 1, w[1].0, "no gap, no overlap");
        }
    }

    #[test]
    fn layout_is_absent_until_placed() {
        let p = Progress::new(1);
        p.set_absolute(10);
        assert_eq!(p.layout(), (None, vec![10]));
    }

    /// Minimal HTTP/1.1 server for a file whose byte `i` is `i % 251`,
    /// honouring `Range`. The range starting at byte 0 is served slowly so the
    /// other connections finish first and have to take over its tail.
    async fn serve_pattern(listener: tokio::net::TcpListener, total: u64) {
        use tokio::io::{AsyncBufReadExt, BufReader};
        const HEAD: &str = "Accept-Ranges: bytes\r\nETag: \"v1\"\r\nConnection: close\r\n";
        loop {
            let Ok((sock, _)) = listener.accept().await else { return };
            tokio::spawn(async move {
                let (read, mut write) = sock.into_split();
                let mut lines = BufReader::new(read).lines();
                let Ok(Some(request)) = lines.next_line().await else { return };
                let mut range = None;
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        let (a, b) = v.split_once('-').unwrap();
                        range = Some((a.parse::<u64>().unwrap(), b.parse::<u64>().unwrap()));
                    }
                }
                if request.starts_with("HEAD") {
                    let reply = format!("HTTP/1.1 200 OK\r\n{HEAD}Content-Length: {total}\r\n\r\n");
                    let _ = write.write_all(reply.as_bytes()).await;
                    return;
                }
                let (start, end) = range.unwrap_or((0, total - 1));
                let reply = format!(
                    "HTTP/1.1 206 Partial Content\r\n{HEAD}Content-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\n\r\n",
                    end - start + 1
                );
                if write.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
                let mut at = start;
                while at <= end {
                    let n = (end + 1 - at).min(64 * 1024);
                    let body: Vec<u8> = (at..at + n).map(|i| (i % 251) as u8).collect();
                    if write.write_all(&body).await.is_err() {
                        return; // client dropped a shrunk segment's connection
                    }
                    at += n;
                    if start == 0 {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            });
        }
    }

    /// The slowest connection must not decide the finish time. Served locally
    /// so the split is forced, then checked byte for byte: a split that cut
    /// into claimed bytes or left a gap would show up here.
    #[tokio::test]
    async fn finished_connections_take_over_a_slow_segment() {
        let total: u64 = 16 * 1024 * 1024;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/pattern.bin", listener.local_addr().unwrap());
        tokio::spawn(serve_pattern(listener, total));

        let dir = scratch("split");
        let (client, seg) = clients();
        let plan = prepare(&client, &url, &dir, 4, false, None).await.unwrap();
        assert_eq!(plan.ranges.len(), 4, "expected a segmented plan");

        let progress = Progress::new(plan.segment_count());
        let path = run(&client, &seg, &plan, &progress, &Throttle::unlimited(), CancellationToken::new(), |_, _| {})
            .await
            .unwrap();

        let (ranges, done) = progress.layout();
        let ranges = ranges.unwrap();
        assert!(ranges.len() > 4, "the slow segment's tail should have been taken over");
        assert_eq!(done.iter().sum::<u64>(), total, "no byte counted twice");

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, total);
        assert!(
            bytes.iter().enumerate().all(|(i, &b)| b == (i % 251) as u8),
            "every byte where it belongs"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn progress_reset_clears_every_counter() {
        // Used when a server ignores Range and the whole file must restart.
        let p = Progress::resumed(&[10, 20]);
        p.counter(0).add(5);
        p.reset();
        assert_eq!(p.total(), 0);
        assert_eq!(p.snapshot(), vec![0, 0]);
    }

    /// A mirror that answers with a cookie and a 302 to the same URL until the
    /// cookie comes back, as mirror.nju.edu.cn does for browser agents.
    /// Without a cookie jar the client loops until the redirect limit.
    #[tokio::test]
    async fn cookie_bounce_redirect_is_followed() {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/f.bin", listener.local_addr().unwrap());
        let location = url.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { return };
                let location = location.clone();
                tokio::spawn(async move {
                    let (read, mut write) = sock.into_split();
                    let mut lines = BufReader::new(read).lines();
                    let mut has_cookie = false;
                    while let Ok(Some(line)) = lines.next_line().await {
                        if line.is_empty() {
                            break;
                        }
                        has_cookie |= line.to_ascii_lowercase().starts_with("cookie:") && line.contains("bcheck=true");
                    }
                    let reply = if has_cookie {
                        "HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc".to_string()
                    } else {
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: {location}\r\nSet-Cookie: bcheck=true; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                    };
                    let _ = write.write_all(reply.as_bytes()).await;
                });
            }
        });

        let (client, segment_client) = clients();
        for c in [&client, &segment_client] {
            let body = c.get(&url).send().await.unwrap().text().await.unwrap();
            assert_eq!(body, "abc");
        }
    }

    // -- Content-Disposition edges ------------------------------------------

    #[test]
    fn content_disposition_percent_escapes_and_bad_input() {
        // RFC 5987 percent-decoding, including a space.
        assert_eq!(
            filename_from_content_disposition("attachment; filename*=UTF-8''my%20file%2Ezip")
                .as_deref(),
            Some("my file.zip")
        );
        // A truncated escape must not panic or eat the rest of the name.
        assert!(
            filename_from_content_disposition("attachment; filename*=UTF-8''bad%2")
                .is_some_and(|n| !n.is_empty())
        );
        // Header present but no filename parameter at all.
        assert_eq!(filename_from_content_disposition("inline"), None);
        assert_eq!(filename_from_content_disposition(""), None);
    }

    #[test]
    fn resolve_filename_never_returns_a_traversal() {
        let url = Url::parse("https://example.com/dl").unwrap();
        // A hostile Content-Disposition is sanitised, then still used.
        assert_eq!(
            resolve_filename(Some(r#"attachment; filename="../../etc/passwd""#), &url),
            "passwd"
        );
        // A name that sanitises away entirely falls through to the URL path,
        // and then to the default.
        assert_eq!(resolve_filename(Some(r#"attachment; filename="..""#), &url), "dl");
        let bare = Url::parse("https://example.com/").unwrap();
        assert_eq!(resolve_filename(None, &bare), "download.bin");
    }

    #[test]
    fn category_of_awkward_names() {
        // The extension match is case-insensitive.
        assert_eq!(category_for("CLIP.MP4"), "Video");
        assert_eq!(category_for("Photo.JPEG"), "Images");
        // No extension, and a dotfile whose only dot starts the name.
        assert_eq!(category_for("README"), "Other");
        assert_eq!(category_for(".bashrc"), "Other");
        // Multi-dot names classify on the last extension.
        assert_eq!(category_for("backup.tar.gz"), "Archives");
    }

    // -- clients ------------------------------------------------------------

    /// A typo in the proxy box must not stop every download: an unusable proxy
    /// is logged and skipped, and the client still builds.
    #[test]
    fn a_bad_proxy_does_not_break_client_construction() {
        for proxy in [
            Some("not a proxy".to_string()),
            Some("://missing-scheme".to_string()),
            Some(String::new()), // an empty box means direct, not a proxy of ""
            None,
        ] {
            let session = Session { proxy, ..Session::default() };
            assert!(build_client(&session).is_ok(), "client must still build");
            assert!(build_segment_client(&session).is_ok());
        }

        // A usable proxy also builds (nothing connects until a request is made).
        let session = Session {
            proxy: Some("http://127.0.0.1:9".into()),
            ..Session::default()
        };
        assert!(build_client(&session).is_ok());
    }

    #[test]
    fn browser_headers_omit_blank_session_values() {
        // A `Referer:` header with no value is worse than no header at all.
        let bare = browser_headers(&Session::default());
        assert!(!bare.contains_key(reqwest::header::REFERER));
        assert!(!bare.contains_key(reqwest::header::COOKIE));

        let full = browser_headers(&Session {
            cookie: Some("a=1".into()),
            referer: Some("https://example.com/".into()),
            ..Session::default()
        });
        assert_eq!(full.get(reqwest::header::COOKIE).unwrap(), "a=1");
        assert_eq!(full.get(reqwest::header::REFERER).unwrap(), "https://example.com/");

        // A header value that cannot be encoded is skipped, not a panic.
        let weird = browser_headers(&Session {
            cookie: Some("bad\nvalue".into()),
            ..Session::default()
        });
        assert!(!weird.contains_key(reqwest::header::COOKIE));
    }

    /// Durable must never run ahead of live, because `queue.json` records
    /// durable and a resume trusts it absolutely.
    #[test]
    fn durable_never_exceeds_live() {
        let seg = SegmentProgress::default();
        seg.add(1000);
        assert_eq!(seg.live(), 1000);
        assert_eq!(seg.durable(), 0, "unsynced bytes must not count as durable");

        seg.commit();
        assert_eq!(seg.durable(), 1000);

        // Bytes written after the last sync are lost on rewind, not trusted.
        seg.add(500);
        seg.rewind_to_durable();
        assert_eq!(seg.live(), 1000);
        assert_eq!(seg.durable(), 1000);
    }

    #[tokio::test]
    #[ignore]
    async fn network_progress_is_reported() {
        let dir = scratch("progress");
        let (client, seg) = clients();
        let seen = Arc::new(AtomicU64::new(0));
        let sink = Arc::clone(&seen);

        let path = download_file(&client, &seg, BIG, &dir, 8, CancellationToken::new(), move |n, total| {
            assert_eq!(total, Some(10 * 1024 * 1024));
            sink.fetch_max(n, Ordering::Relaxed);
        })
        .await
        .unwrap();

        assert_eq!(seen.load(Ordering::Relaxed), 10 * 1024 * 1024);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
