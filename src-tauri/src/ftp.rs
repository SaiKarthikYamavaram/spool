//! The FTP engine.
//!
//! FTP predates every assumption the HTTP engine makes: there are no headers,
//! no `Content-Disposition`, no `Range`. What it does have is `SIZE` for the
//! length and `REST` for resuming at an offset, which is enough for the two
//! things that matter — an honest progress bar and a transfer that survives
//! being paused.
//!
//! ponytail: one connection per download, no segmentation. FTP would need a
//! second control connection per segment and many servers cap concurrent
//! logins per account; split it only if a real server turns out to be the
//! bottleneck.

use std::path::{Path, PathBuf};

use reqwest::Url;
use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::download::{
    self, percent_decode, part_path, resolve_unique_path, sanitize_filename, DownloadPlan, Engine,
    Progress, Session,
};
use crate::throttle::Throttle;

/// Read at most this much per chunk. Matches the HTTP engine's slice so the
/// speed cap behaves identically on both.
const CHUNK: usize = 64 * 1024;

/// The port an FTP URL means when it does not say.
const DEFAULT_PORT: u16 = 21;

/// Anonymous FTP's conventional credentials, used when the URL carries none.
const ANON_USER: &str = "anonymous";
const ANON_PASS: &str = "anonymous@";

/// Whether this URL belongs to the FTP engine.
///
/// Deliberately separate from `download::validate_url`, which still rejects
/// `ftp://`: the HTTP engine must never be handed one of these, and the
/// routing decision is made before either engine sees the URL.
pub fn is_ftp_url(raw: &str) -> bool {
    matches!(Url::parse(raw).as_ref().map(|u| u.scheme()), Ok("ftp"))
}

/// Host, port and the remote path, pulled out of an `ftp://` URL.
fn parts(raw: &str) -> Result<(String, u16, String, String), String> {
    let url = Url::parse(raw).map_err(|e| format!("invalid URL: {e}"))?;
    if url.scheme() != "ftp" {
        return Err(format!("not an FTP URL: {raw}"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| "that FTP URL has no host".to_string())?
        .to_string();
    let port = url.port().unwrap_or(DEFAULT_PORT);

    // The wire path is percent-decoded: `%20` in a URL is a space in a
    // filename, and the server is given the filename.
    let path = percent_decode(url.path());
    if path.is_empty() || path.ends_with('/') {
        return Err("that FTP URL points at a directory, not a file".into());
    }
    let name = path.rsplit('/').next().unwrap_or_default().to_string();
    Ok((host, port, path, name))
}

/// Log in, either with the URL's own credentials or anonymously.
async fn connect(raw: &str, session: &Session) -> Result<(AsyncFtpStream, String), String> {
    let (host, port, path, _) = parts(raw)?;

    let mut ftp = AsyncFtpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| format!("cannot reach {host}:{port}: {e}"))?;

    // `split_userinfo` put `user:pass` here when the URL carried them.
    let (user, pass) = match session.auth.as_deref() {
        Some(auth) => match auth.split_once(':') {
            Some((u, p)) => (u.to_string(), p.to_string()),
            None => (auth.to_string(), String::new()),
        },
        None => (ANON_USER.to_string(), ANON_PASS.to_string()),
    };

    ftp.login(&user, &pass)
        .await
        .map_err(|e| format!("FTP login failed on {host}: {e}"))?;

    // Binary, always. The default on many servers is ASCII, which rewrites
    // line endings in transit and silently corrupts every non-text file.
    ftp.transfer_type(FileType::Binary)
        .await
        .map_err(|e| format!("cannot switch {host} to binary mode: {e}"))?;

    Ok((ftp, path))
}

/// Probe the server and reserve a local name, transferring nothing.
pub async fn prepare(
    url: &str,
    session: &Session,
    dest_dir: &Path,
    categorize: bool,
    custom_name: Option<&str>,
) -> Result<DownloadPlan, String> {
    let (_, _, _, remote_name) = parts(url)?;
    let (mut ftp, path) = connect(url, session).await?;

    // `SIZE` is optional in the protocol and refused by some servers, which
    // costs the progress bar and nothing else — the transfer still runs.
    let total = ftp.size(&path).await.ok().map(|n| n as u64);
    let _ = ftp.quit().await;

    let filename = custom_name
        .and_then(sanitize_filename)
        .map(|name| download::keep_extension(&name, &remote_name))
        .or_else(|| sanitize_filename(&remote_name))
        .unwrap_or_else(|| "download.bin".to_string());

    let owned_dir;
    let dest_dir = if categorize {
        owned_dir = dest_dir.join(download::category_for(&filename));
        owned_dir.as_path()
    } else {
        dest_dir
    };
    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| format!("cannot create {}: {e}", dest_dir.display()))?;

    let final_path = resolve_unique_path(dest_dir, &filename);
    let part = part_path(&final_path);
    // Claim the name now, exactly as the HTTP engine does, so two adds racing
    // each other cannot pick the same one.
    let _ = tokio::fs::File::create(&part).await;

    Ok(DownloadPlan {
        url: url.to_string(),
        final_path,
        part_path: part,
        total,
        // `REST` resumes one stream at an offset; it does not let a second
        // connection take a different range, so there is nothing to split.
        supports_ranges: false,
        validator: None,
        ranges: Vec::new(),
        engine: Engine::Ftp,
        thumbnail: None,
    })
}

/// Run or resume a prepared FTP plan.
///
/// Resume is `REST <offset>` before `RETR`. A server that refuses it answers
/// the `RETR` from byte 0, so the offset is only trusted once `REST` itself
/// was accepted — otherwise the partial file is truncated and refetched,
/// which is slow but never corrupt.
pub async fn run<F>(
    plan: &DownloadPlan,
    session: &Session,
    progress: &Progress,
    throttle: &Throttle,
    token: CancellationToken,
    on_progress: F,
) -> Result<PathBuf, String>
where
    F: Fn(u64, Option<u64>) + Send + Sync + 'static,
{
    let (mut ftp, path) = connect(&plan.url, session).await?;

    // What a previous run durably wrote. The file on disk is the authority:
    // a counter ahead of it would resume past bytes that were never written.
    let on_disk = tokio::fs::metadata(&plan.part_path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut offset = progress.total().min(on_disk);

    if offset > 0 && ftp.resume_transfer(offset as usize).await.is_err() {
        // The server will not resume; start over rather than stitch a whole
        // file onto a partial one.
        offset = 0;
    }
    progress.set_absolute(offset);

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&plan.part_path)
        .await
        .map_err(|e| format!("cannot open {}: {e}", plan.part_path.display()))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("cannot seek {}: {e}", plan.part_path.display()))?;

    let mut stream = ftp
        .retr_as_stream(&path)
        .await
        .map_err(|e| format!("cannot download {path}: {e}"))?;

    let mut buf = vec![0u8; CHUNK];
    let mut done = offset;
    let result = loop {
        let read = tokio::select! {
            biased;
            _ = token.cancelled() => break Err("cancelled".to_string()),
            read = stream.read(&mut buf) => read,
        };
        let n = match read {
            Ok(0) => break Ok(()),
            Ok(n) => n,
            Err(e) => break Err(format!("transfer failed: {e}")),
        };

        // Charged before the write, like the HTTP engine, so the cap applies
        // to what comes off the socket.
        throttle.take(n).await;

        if let Err(e) = file.write_all(&buf[..n]).await {
            break Err(format!("cannot write {}: {e}", plan.part_path.display()));
        }
        done += n as u64;
        progress.set_absolute(done);
        on_progress(done, plan.total);
    };

    // Flush before anything claims those bytes are on disk: the checkpoint
    // that follows a pause records this offset.
    let _ = file.sync_data().await;
    // Closes the data connection and reads the completion reply. A cancelled
    // transfer drops it instead — the control connection goes with it.
    if result.is_ok() {
        stream
            .finish()
            .await
            .map_err(|e| format!("the server did not confirm the transfer: {e}"))?;
        let _ = ftp.quit().await;
    }
    result?;

    if let Some(expected) = plan.total {
        if done != expected {
            return Err(format!(
                "incomplete download: got {done} bytes, expected {expected}. \
                 Partial file kept at {}",
                plan.part_path.display()
            ));
        }
    }

    tokio::fs::rename(&plan.part_path, &plan.final_path)
        .await
        .map_err(|e| format!("cannot finalize {}: {e}", plan.final_path.display()))?;
    on_progress(done, plan.total);
    Ok(plan.final_path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ftp_urls_are_claimed() {
        assert!(is_ftp_url("ftp://example.com/a.zip"));
        assert!(is_ftp_url("ftp://user:pass@example.com/a.zip"));
        assert!(!is_ftp_url("https://example.com/a.zip"));
        assert!(!is_ftp_url("ftps://example.com/a.zip"), "implicit TLS is not supported");
        assert!(!is_ftp_url("not a url"));
    }

    #[test]
    fn the_default_port_is_filled_in() {
        let (host, port, path, name) = parts("ftp://example.com/pub/a.zip").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 21);
        assert_eq!(path, "/pub/a.zip");
        assert_eq!(name, "a.zip");
    }

    #[test]
    fn an_explicit_port_wins() {
        let (_, port, _, _) = parts("ftp://example.com:2121/a.zip").unwrap();
        assert_eq!(port, 2121);
    }

    /// The server is given a filename, not a URL, so the escape has to go.
    #[test]
    fn the_remote_path_is_percent_decoded() {
        let (_, _, path, name) = parts("ftp://example.com/pub/my%20file.zip").unwrap();
        assert_eq!(path, "/pub/my file.zip");
        assert_eq!(name, "my file.zip");
    }

    #[test]
    fn a_directory_is_refused() {
        assert!(parts("ftp://example.com/pub/").is_err());
        assert!(parts("ftp://example.com").is_err(), "no path is no file");
    }

    #[test]
    fn a_non_ftp_url_is_refused() {
        assert!(parts("https://example.com/a.zip").is_err());
    }
}
