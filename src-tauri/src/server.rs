//! Localhost bridge for the browser extension.
//!
//! This is the mechanism a real download manager uses to get past an
//! interactive anti-bot challenge: it does not solve the challenge, the
//! browser does. The extension watches for a download, reads the cookies the
//! browser already holds for that site (including whatever `cf_clearance` it
//! earned), and POSTs the URL plus that session here. spool then replays a
//! session the browser established.
//!
//! `tiny_http` on a dedicated thread rather than the Tokio runtime: the server
//! is a slow, low-volume control channel (a click at a time), so a blocking
//! listener is simpler than wiring another async stack, and it stays entirely
//! out of the way of the download tasks.

use std::io::Read;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use serde::Deserialize;
use tauri::{AppHandle, Emitter};
use tiny_http::{Header, Method, Response, Server};

use crate::download::Session;
use crate::state::{self, AppState, ConfirmRequest, PendingAdd};

/// Fixed port so the extension has a constant target. Bound to loopback only.
pub const PORT: u16 = 47831;

/// Threads serving `/add`. Each can block for seconds on a metadata probe, so
/// a few give batch grabs some parallelism without unbounded spawning.
const ADD_WORKERS: usize = 4;

/// The JSON the extension sends.
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

/// Start the bridge on its own thread. Returns immediately; logs and keeps
/// running for the life of the process. A bind failure is not fatal — the app
/// still works without the extension — so it is logged, not propagated.
pub fn start(app: AppHandle, state: Arc<AppState>) {
    std::thread::spawn(move || {
        let addr = (Ipv4Addr::LOCALHOST, PORT);
        let server = match Server::http(addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("spool: extension bridge disabled, cannot bind 127.0.0.1:{PORT}: {e}");
                return;
            }
        };
        eprintln!("spool: extension bridge listening on 127.0.0.1:{PORT}");

        // A bounded pool rather than a thread per request: "download all links"
        // fires one POST per link, and any local process can hit this endpoint,
        // so unbounded spawning would be a cheap way to exhaust threads.
        let (work_tx, work_rx) = std::sync::mpsc::channel::<(tiny_http::Request, String)>();
        let work_rx = Arc::new(std::sync::Mutex::new(work_rx));
        for _ in 0..ADD_WORKERS {
            let rx = Arc::clone(&work_rx);
            let app = app.clone();
            let state = Arc::clone(&state);
            std::thread::spawn(move || loop {
                let job = { rx.lock().unwrap().recv() };
                let Ok((request, body)) = job else { return };
                let response = match handle_add(&app, &state, &body) {
                    Ok(id) => cors(Response::from_string(id)),
                    Err(e) => cors(Response::from_string(e).with_status_code(400)),
                };
                let _ = request.respond(response);
            });
        }

        for mut request in server.incoming_requests() {
            // Defence in depth: tiny_http is bound to loopback already, but a
            // request that somehow arrives from off-box is refused rather than
            // trusted. Any local process can still reach this — an accepted
            // limitation for a personal tool, matching how IDM's local bridge
            // works.
            // ponytail: loopback-only, no auth token; add a token handshake if
            // this ever runs somewhere multi-user.
            if !is_loopback(&request) {
                let _ = request.respond(cors(Response::empty(403)));
                continue;
            }

            match (request.method(), request.url()) {
                // Preflight for the extension's cross-origin POST.
                (Method::Options, _) => {
                    let _ = request.respond(cors(Response::empty(204)));
                }
                // Health check so the extension can tell whether spool is up.
                (Method::Get, "/ping") => {
                    let _ = request.respond(cors(Response::from_string("spool")));
                }
                (Method::Post, "/add") => {
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_err() {
                        let _ = request.respond(cors(Response::from_string("bad body").with_status_code(400)));
                        continue;
                    }
                    // Hand to the worker pool: a video /add blocks on a ~15s
                    // yt-dlp metadata probe, and the accept loop must stay free
                    // to answer /ping meanwhile.
                    if work_tx.send((request, body)).is_err() {
                        break; // workers gone; nothing left to serve
                    }
                }
                _ => {
                    let _ = request.respond(cors(Response::empty(404)));
                }
            }
        }
    });
}

fn handle_add(app: &AppHandle, state: &Arc<AppState>, body: &str) -> Result<String, String> {
    let req: AddRequest =
        serde_json::from_str(body).map_err(|e| format!("invalid request JSON: {e}"))?;

    let session = session_from(&req);

    // Hand off to the same async path the UI uses. The bridge thread is
    // blocking, so bounce onto the Tokio runtime and wait for the result to
    // report a real error back to the extension.
    let app = app.clone();
    let state = Arc::clone(state);
    let url = req.url.clone();

    let force_video = req.video;

    // "Ask before download": park the captured session and let the UI collect
    // the destination and options. Returns immediately — no metadata probe,
    // no queue entry until the user confirms.
    if req.ask {
        let token = state.stash_pending(PendingAdd {
            url: url.clone(),
            session: Some(session),
            force_video,
            added_at: crate::queue::now_secs(),
        });
        crate::show_main(&app);
        let _ = app.emit(
            "download://confirm",
            ConfirmRequest { token: token.clone(), url, video: force_video },
        );
        return Ok(token);
    }

    let (tx, rx) = std::sync::mpsc::channel();
    tauri::async_runtime::spawn(async move {
        let result = state.add_with_session(&app, &url, Some(session), force_video, Default::default()).await;
        if result.is_ok() {
            state::pump(&app, &state);
        }
        let _ = tx.send(result);
    });

    rx.recv().map_err(|_| "internal error".to_string())?
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

fn is_loopback(request: &tiny_http::Request) -> bool {
    match request.remote_addr() {
        Some(addr) => match addr.ip() {
            IpAddr::V4(ip) => ip.is_loopback(),
            IpAddr::V6(ip) => ip.is_loopback(),
        },
        // Unix socket or unknown transport: not a remote TCP peer, treat as local.
        None => true,
    }
}

/// The extension's service worker makes a cross-origin request, so every reply
/// needs permissive CORS or the browser discards it before the extension sees
/// it. There is nothing secret in these responses (an id or an error string).
fn cors<R>(response: Response<R>) -> Response<R>
where
    R: Read,
{
    let allow_origin = Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap();
    let allow_headers =
        Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"Content-Type"[..]).unwrap();
    let allow_methods =
        Header::from_bytes(&b"Access-Control-Allow-Methods"[..], &b"POST, GET, OPTIONS"[..]).unwrap();
    response
        .with_header(allow_origin)
        .with_header(allow_headers)
        .with_header(allow_methods)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> AddRequest {
        serde_json::from_str(body).expect("payload must parse")
    }

    /// The extension's minimal payload: a URL and nothing else. Every other
    /// field has to default, or a plain right-click would be a 400.
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
