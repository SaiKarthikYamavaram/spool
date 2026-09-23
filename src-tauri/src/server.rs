//! Native-messaging bridge for the browser extension.
//!
//! This is the mechanism a real download manager uses to get past an
//! interactive anti-bot challenge: it does not solve the challenge, the
//! browser does. The extension watches for a download, reads the cookies the
//! browser already holds for that site (including whatever `cf_clearance` it
//! earned), and sends the URL plus that session here. spool then replays a
//! session the browser established.
//!
//! The extension cannot open a socket. It asks the browser to launch a
//! registered "native host" and talks to it over stdin/stdout. The host is this
//! same binary, started with the extension's origin as its first argument
//! (`run_host`): it connects to a Unix socket the running app listens on and
//! relays bytes both ways, so the app speaks the native-messaging framing — a
//! native-endian `u32` length, then that many bytes of JSON — directly.
//!
//! Only the extension named in the host manifest's `allowed_origins` can have
//! the browser launch the host, and the app answers only connections from its
//! own user, so neither a web page nor another account can reach it — which
//! the localhost HTTP port this replaced could not promise.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::download::Session;
use crate::state::{self, AppState, ConfirmRequest, PendingAdd};

/// The name the extension passes to `chrome.runtime.sendNativeMessage`.
pub const HOST_NAME: &str = "com.saikarthik.spool";

/// Pinned by the `key` in `extension/manifest.json`, so an unpacked load gets
/// the same ID on every machine and the host manifest can name it.
pub const EXTENSION_ID: &str = "mlddhjdhcladccjcgffcmeonbjhkhhlo";

/// A captured session is a URL and a cookie header; anything near this size is
/// not one.
const MAX_MESSAGE: usize = 64 * 1024;

/// What the extension sends.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Message {
    /// Health check so the extension can tell whether spool is up.
    Ping,
    Add(AddRequest),
}

/// A download handed over by the extension.
#[derive(Debug, Deserialize)]
struct AddRequest {
    url: String,
    cookie: Option<String>,
    #[serde(rename = "userAgent")]
    user_agent: Option<String>,
    referer: Option<String>,
    /// Force the yt-dlp video engine (from the extension's "Download video").
    #[serde(default)]
    video: bool,
    /// Open the app's add dialog instead of queuing straight away, so the user
    /// can pick a folder and options for this download.
    #[serde(default)]
    ask: bool,
}

/// Where the app listens and the host connects. The runtime dir is private to
/// the user; `~/.cache` is the fallback where there is none. Never the shared
/// temp dir: another account could bind the path first and collect cookies.
fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .map(|dir| dir.join("spool.sock"))
}

/// Whether the browser launched this process as the native host. Chrome passes
/// the calling extension's origin as the first argument.
pub fn launched_as_host() -> bool {
    std::env::args().nth(1).is_some_and(|a| a.starts_with("chrome-extension://"))
}

/// Relay the browser's stdin/stdout to the running app until either side
/// closes. Exits without a reply when the app is not running, which the
/// extension reads as "not reachable" and leaves the download to the browser.
#[cfg(unix)]
pub fn run_host() {
    use std::os::unix::net::UnixStream;

    let Some(Ok(socket)) = socket_path().map(UnixStream::connect) else {
        eprintln!("spool: the app is not running");
        std::process::exit(1);
    };
    let Ok(up) = socket.try_clone() else { std::process::exit(1) };
    std::thread::spawn(move || {
        relay(std::io::stdin().lock(), &up);
        // Tell the app no more requests are coming, so it closes its end and
        // the reply direction below finishes.
        let _ = up.shutdown(std::net::Shutdown::Write);
    });
    relay(&socket, std::io::stdout().lock());
}

/// Copy until EOF, flushing each chunk. `io::copy` into stdout would sit on a
/// reply that has no trailing newline until the process exits, and the
/// browser waits for that reply before it closes stdin — a deadlock.
fn relay(mut from: impl std::io::Read, mut to: impl std::io::Write) {
    let mut buf = [0u8; 8192];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if to.write_all(&buf[..n]).and_then(|_| to.flush()).is_err() {
                    return;
                }
            }
        }
    }
}

/// Tell each installed Chromium-family browser where the host is. Rewritten on
/// every launch, like the autostart entry, so a moved or reinstalled binary is
/// picked up without a reinstall step. Not fatal: the app works without the
/// extension.
#[cfg(target_os = "linux")]
pub fn register_host() {
    const BROWSERS: &[&str] = &[
        "google-chrome",
        "google-chrome-beta",
        "google-chrome-unstable",
        "chromium",
        "BraveSoftware/Brave-Browser",
        "microsoft-edge",
        "vivaldi",
    ];

    let Some(config) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    else {
        return;
    };
    // An AppImage runs from a mount point that changes every launch; the
    // browser has to be pointed at the AppImage file itself.
    let Some(exe) = std::env::var_os("APPIMAGE")
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
    else {
        return;
    };
    let manifest = host_manifest(&exe);

    for browser in BROWSERS {
        let profile = config.join(browser);
        if !profile.is_dir() {
            continue; // not installed
        }
        let hosts = profile.join("NativeMessagingHosts");
        let written = std::fs::create_dir_all(&hosts)
            .and_then(|_| std::fs::write(hosts.join(format!("{HOST_NAME}.json")), &manifest));
        if let Err(e) = written {
            eprintln!("spool: could not register the extension host for {browser}: {e}");
        }
    }
}

// ponytail: Linux only; macOS wants ~/Library/Application Support/<browser>/
// NativeMessagingHosts and Windows a registry key, add them if spool ships there.
#[cfg(not(target_os = "linux"))]
pub fn register_host() {}

fn host_manifest(exe: &std::path::Path) -> String {
    serde_json::to_string_pretty(&json!({
        "name": HOST_NAME,
        "description": "spool download manager",
        "path": exe,
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{EXTENSION_ID}/")],
    }))
    .unwrap()
}

/// Listen for host connections for the life of the app. A bind failure is not
/// fatal — the app still works without the extension — so it is logged, not
/// propagated.
#[cfg(unix)]
pub fn start(app: AppHandle, state: Arc<AppState>) {
    use tokio::net::UnixListener;

    let Some(path) = socket_path() else {
        eprintln!("spool: extension bridge disabled, no runtime or home directory");
        return;
    };
    tauri::async_runtime::spawn(async move {
        // The single-instance plugin has already turned away a second launch,
        // so a socket file here is a crashed run's leftover.
        let _ = std::fs::remove_file(&path);
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("spool: extension bridge disabled, cannot bind {}: {e}", path.display());
                return;
            }
        };
        // SAFETY: `getuid` has no preconditions and cannot fail.
        let me = unsafe { libc::getuid() };
        loop {
            let stream = match listener.accept().await {
                Ok((stream, _)) => stream,
                Err(e) => {
                    // Out of file descriptors, most likely; back off rather
                    // than spin on the same error.
                    eprintln!("spool: extension bridge accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                }
            };
            // The socket may sit in a directory other users can enter.
            if !stream.peer_cred().is_ok_and(|c| c.uid() == me) {
                continue;
            }
            // A task per connection: a video add waits ~15s on a yt-dlp probe,
            // and the popup's ping must not queue behind it.
            tauri::async_runtime::spawn(serve(app.clone(), Arc::clone(&state), stream));
        }
    });
}

// ponytail: Unix only; a Windows build needs a named pipe here and a registry
// entry in `register_host`.
#[cfg(not(unix))]
pub fn start(_app: AppHandle, _state: Arc<AppState>) {
    eprintln!("spool: extension bridge is only built for Unix");
}

/// Answer requests on one connection until the host hangs up.
async fn serve<S>(app: AppHandle, state: Arc<AppState>, mut stream: S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // An oversized or truncated frame ends the connection; the extension sees
    // the host exit and reports the handover as failed.
    while let Ok(Some(body)) = read_frame(&mut stream).await {
        let reply = match parse_message(&body) {
            Ok(Message::Ping) => json!({ "ok": true }),
            Ok(Message::Add(req)) => match handle_add(&app, &state, req).await {
                Ok(id) => json!({ "ok": true, "id": id }),
                Err(e) => json!({ "ok": false, "error": e }),
            },
            Err(e) => json!({ "ok": false, "error": e }),
        };
        if write_frame(&mut stream, &reply).await.is_err() {
            return;
        }
    }
}

/// One native-messaging frame, or `None` at a clean end of stream.
async fn read_frame<R>(r: &mut R) -> std::io::Result<Option<Vec<u8>>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message too large"));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Ok(Some(body))
}

async fn write_frame<W>(w: &mut W, value: &Value) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let body = serde_json::to_vec(value)?;
    w.write_all(&(body.len() as u32).to_ne_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

fn parse_message(body: &[u8]) -> Result<Message, String> {
    let msg: Message =
        serde_json::from_slice(body).map_err(|e| format!("invalid request JSON: {e}"))?;
    if let Message::Add(req) = &msg {
        if req.url.trim().is_empty() {
            return Err("empty URL".to_string());
        }
    }
    Ok(msg)
}

async fn handle_add(app: &AppHandle, state: &Arc<AppState>, req: AddRequest) -> Result<String, String> {
    let session = session_from(&req);
    let AddRequest { url, video: force_video, ask, .. } = req;

    // "Ask before download": park the captured session and let the UI collect
    // the destination and options. Returns immediately — no metadata probe,
    // no queue entry until the user confirms.
    if ask {
        let token = state.stash_pending(PendingAdd {
            url: url.clone(),
            session: Some(session),
            force_video,
            added_at: crate::queue::now_secs(),
        });
        crate::show_main(app);
        let _ = app.emit(
            "download://confirm",
            ConfirmRequest { token: token.clone(), url, video: force_video },
        );
        return Ok(token);
    }

    let id = state.add_with_session(app, &url, Some(session), force_video, Default::default()).await?;
    state::pump(app, state);
    Ok(id)
}

/// Turn the extension's payload into a replayable session.
///
/// The extension sends empty strings for headers it could not read, and an
/// empty `Cookie:` or `Referer:` header is worse than none — some hosts treat
/// it as a malformed request — so blanks become `None`. A missing agent falls
/// back to spool's default rather than sending none at all.
fn session_from(req: &AddRequest) -> Session {
    Session {
        user_agent: req
            .user_agent
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| crate::download::USER_AGENT.to_string()),
        cookie: req.cookie.clone().filter(|s| !s.is_empty()),
        referer: req.referer.clone().filter(|s| !s.is_empty()),
        // Filled in from settings by `session_for`.
        proxy: None,
        // Credentials come from the URL's own userinfo, stripped at add time.
        auth: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> AddRequest {
        serde_json::from_str(body).expect("payload must parse")
    }

    /// The extension's minimal payload: a URL and nothing else. Every other
    /// field has to default, or a plain right-click would be refused.
    #[test]
    fn minimal_payload_defaults_everything() {
        let req = parse(r#"{"url":"https://example.com/a.zip"}"#);
        assert_eq!(req.url, "https://example.com/a.zip");
        assert!(req.cookie.is_none());
        assert!(req.user_agent.is_none());
        assert!(req.referer.is_none());
        assert!(!req.video, "a plain link is not forced through yt-dlp");
        assert!(!req.ask, "queuing straight away is the default");
    }

    #[test]
    fn full_payload_maps_every_field() {
        let req = parse(
            r#"{
                "url": "https://example.com/v",
                "cookie": "a=1; b=2",
                "userAgent": "Mozilla/5.0 (X11)",
                "referer": "https://example.com/page",
                "video": true,
                "ask": true
            }"#,
        );
        assert_eq!(req.cookie.as_deref(), Some("a=1; b=2"));
        // The JSON key is camelCase; the field is not.
        assert_eq!(req.user_agent.as_deref(), Some("Mozilla/5.0 (X11)"));
        assert_eq!(req.referer.as_deref(), Some("https://example.com/page"));
        assert!(req.video);
        assert!(req.ask);
    }

    /// A newer extension against an older app (or the reverse) must not break
    /// on a key the other side does not know.
    #[test]
    fn unknown_fields_are_ignored() {
        let req = parse(r#"{"url":"https://example.com/a","futureOption":42}"#);
        assert_eq!(req.url, "https://example.com/a");
    }

    #[test]
    fn a_payload_without_a_url_is_rejected() {
        assert!(serde_json::from_str::<AddRequest>(r#"{"cookie":"a=1"}"#).is_err());
        assert!(serde_json::from_str::<AddRequest>("not json").is_err());
        assert!(serde_json::from_str::<AddRequest>(r#"{"url":42}"#).is_err());
    }

    #[test]
    fn messages_are_told_apart_by_type() {
        assert!(matches!(parse_message(br#"{"type":"ping"}"#), Ok(Message::Ping)));
        let Ok(Message::Add(req)) = parse_message(br#"{"type":"add","url":"https://e.test/a"}"#) else {
            panic!("an add must parse");
        };
        assert_eq!(req.url, "https://e.test/a");
        assert!(parse_message(br#"{"type":"delete","url":"x"}"#).is_err());
        assert!(parse_message(br#"{"url":"https://e.test/a"}"#).is_err(), "a message needs a type");
    }

    #[test]
    fn blank_url_is_rejected() {
        let err = parse_message(br#"{"type":"add","url":"   "}"#).unwrap_err();
        assert_eq!(err, "empty URL");
        let err = parse_message(br#"{"type":"add","url":""}"#).unwrap_err();
        assert_eq!(err, "empty URL");
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        write_frame(&mut a, &json!({ "ok": true })).await.unwrap();
        drop(a);
        let body = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({ "ok": true }));
        // The writer hung up between frames: a clean end, not an error.
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_before_reading_it() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        use tokio::io::AsyncWriteExt;
        a.write_all(&((MAX_MESSAGE + 1) as u32).to_ne_bytes()).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn a_truncated_frame_is_an_error() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        use tokio::io::AsyncWriteExt;
        a.write_all(&10u32.to_ne_bytes()).await.unwrap();
        a.write_all(b"{}").await.unwrap();
        drop(a);
        assert!(read_frame(&mut b).await.is_err());
    }

    /// The browser refuses a host manifest whose origin lacks the trailing
    /// slash, and only launches the host for the origins listed.
    #[test]
    fn host_manifest_allows_only_the_extension() {
        let m: Value = serde_json::from_str(&host_manifest(std::path::Path::new("/opt/spool"))).unwrap();
        assert_eq!(m["name"], HOST_NAME);
        assert_eq!(m["path"], "/opt/spool");
        assert_eq!(m["type"], "stdio");
        assert_eq!(m["allowed_origins"], json!([format!("chrome-extension://{EXTENSION_ID}/")]));
    }

    /// The extension sends "" for a header it could not read. An empty
    /// `Cookie:` or `Referer:` is worse than none, so blanks must not become
    /// headers.
    #[test]
    fn empty_strings_become_no_header() {
        let session = session_from(&parse(
            r#"{"url":"https://e.test/a","cookie":"","userAgent":"","referer":""}"#,
        ));
        assert!(session.cookie.is_none());
        assert!(session.referer.is_none());
        assert_eq!(session.user_agent, crate::download::USER_AGENT);
    }

    #[test]
    fn a_missing_agent_falls_back_to_the_default() {
        let session = session_from(&parse(r#"{"url":"https://e.test/a"}"#));
        assert_eq!(session.user_agent, crate::download::USER_AGENT);
        // The proxy is never taken from the extension; `session_for` fills it
        // in from settings.
        assert!(session.proxy.is_none());
    }

    #[test]
    fn a_supplied_agent_wins_over_the_default() {
        let session = session_from(&parse(
            r#"{"url":"https://e.test/a","userAgent":"CustomAgent/1.0"}"#,
        ));
        assert_eq!(session.user_agent, "CustomAgent/1.0");
    }
}
