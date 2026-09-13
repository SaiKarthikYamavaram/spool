//! Site spider: collect the files a page points at, optionally following it a
//! level or two deeper.
//!
//! This is the "download all links" of a classic download manager, moved out
//! of the browser extension (which can only see the page already open) and
//! into the app, where it can walk further.
//!
//! ponytail: links are found by scanning `href=`/`src=` attributes rather than
//! by parsing HTML. A link grabber is not a browser: it does not need DOM
//! recovery, and a real parser would pull in the whole html5ever tree. Pages
//! that build their links in JavaScript are invisible either way — use the
//! extension on those, which sees the rendered page.

use std::collections::HashSet;

use reqwest::{Client, Url};

/// Pages fetched in one crawl. A depth-2 walk of a big index would otherwise
/// run until the server got bored.
pub const MAX_PAGES: usize = 50;

/// Links returned from one crawl.
pub const MAX_LINKS: usize = 500;

/// Only this much of a page is scanned. An HTML page listing files is tens of
/// KB; anything past this is not an index.
const MAX_BODY: usize = 4 * 1024 * 1024;

/// How long one page may take before the crawl moves on without it.
const PAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Extensions that are pages to follow rather than files to download.
const PAGE_EXTENSIONS: [&str; 6] = ["html", "htm", "php", "asp", "aspx", "jsp"];

/// Pull the values of one attribute out of a blob of HTML.
///
/// Quoted or bare, single or double quotes, any case. The attribute must start
/// a word, so `data-href=` is not mistaken for `href=`.
pub fn extract_attr(html: &str, attr: &str) -> Vec<String> {
    // `to_ascii_lowercase` changes no byte's length, so offsets found here
    // index the original string correctly.
    let lower = html.to_ascii_lowercase();
    let needle = format!("{attr}=");
    let bytes = html.as_bytes();

    let mut out = Vec::new();
    let mut at = 0usize;

    while let Some(found) = lower[at..].find(&needle) {
        let start = at + found;
        let after = start + needle.len();
        at = after;

        // `href=` must begin an attribute, not end another one's name.
        let standalone = start == 0 || bytes[start - 1].is_ascii_whitespace();
        if !standalone || after >= bytes.len() {
            continue;
        }

        let quote = bytes[after];
        let (value_start, value_end) = if quote == b'"' || quote == b'\'' {
            let s = after + 1;
            match html[s..].find(quote as char) {
                Some(len) => (s, s + len),
                None => break, // unterminated: the rest is not parseable
            }
        } else {
            let s = after;
            let end = html[s..]
                .find(|c: char| c.is_whitespace() || c == '>')
                .map(|len| s + len)
                .unwrap_or(html.len());
            (s, end)
        };

        at = value_end;
        let value = html[value_start..value_end].trim();
        if !value.is_empty() {
            // `&amp;` is how a query string is written in HTML; sent as-is it
            // would be a different URL.
            out.push(value.replace("&amp;", "&"));
        }
    }
    out
}

/// The lowercase extension of a URL's path, if it has one.
fn extension_of(url: &Url) -> Option<String> {
    let last = url.path_segments()?.next_back()?;
    let (_, ext) = last.rsplit_once('.')?;
    (!ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Whether a found URL is a file the user asked for.
///
/// `filter` is a comma-separated extension list ("zip, pdf"). Empty means
/// every link that is not itself a page.
pub fn matches_filter(url: &Url, filter: &str) -> bool {
    let wanted: Vec<String> = filter
        .split(',')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    if url.scheme() == "magnet" {
        return wanted.is_empty() || wanted.iter().any(|w| w == "torrent" || w == "magnet");
    }

    match extension_of(url) {
        Some(ext) => {
            if wanted.is_empty() {
                !PAGE_EXTENSIONS.contains(&ext.as_str())
            } else {
                wanted.contains(&ext)
            }
        }
        // No extension: a directory listing or a script. Only worth taking
        // when nothing specific was asked for... and even then it is far more
        // likely a page than a file, so leave it out.
        None => false,
    }
}

/// Whether a link is a page worth following: same host, and not obviously a
/// file. Staying on one host is what keeps "grab this index" from walking out
/// onto the open web.
fn is_followable(link: &Url, base: &Url, filter: &str) -> bool {
    if link.host_str() != base.host_str() || !matches!(link.scheme(), "http" | "https") {
        return false;
    }
    match extension_of(link) {
        Some(ext) => PAGE_EXTENSIONS.contains(&ext.as_str()),
        // Extensionless paths are directory listings, which is exactly what a
        // file index looks like.
        None => !matches_filter(link, filter),
    }
}

/// Walk `start` (and, with `depth` above 0, the same-host pages it links to)
/// and return every file link that passes the filter, in discovery order.
pub async fn crawl(
    client: &Client,
    start: &str,
    depth: u32,
    filter: &str,
) -> Result<Vec<String>, String> {
    let base = crate::download::validate_url(start)?;

    let mut queue: Vec<(Url, u32)> = vec![(base.clone(), 0)];
    let mut seen_pages: HashSet<String> = HashSet::new();
    let mut found: Vec<String> = Vec::new();
    let mut seen_links: HashSet<String> = HashSet::new();
    let mut fetched = 0usize;

    while let Some((page, page_depth)) = queue.pop() {
        if fetched >= MAX_PAGES || found.len() >= MAX_LINKS {
            break;
        }
        if !seen_pages.insert(page.as_str().to_string()) {
            continue;
        }

        let Some(html) = fetch_page(client, &page).await else {
            // One unreachable page must not end the crawl — an index full of
            // links is still worth what it did return.
            continue;
        };
        fetched += 1;

        let mut links = extract_attr(&html, "href");
        links.extend(extract_attr(&html, "src"));

        for raw in links {
            // Relative links resolve against the page they were found on, not
            // against the URL the crawl started from.
            let Ok(link) = page.join(&raw) else { continue };
            if !matches!(link.scheme(), "http" | "https" | "ftp" | "magnet") {
                continue;
            }

            let mut link = link;
            if link.scheme() != "magnet" {
                // A fragment is a position on a page, never a different file.
                link.set_fragment(None);
            }

            if matches_filter(&link, filter) {
                let as_str = link.as_str().to_string();
                if seen_links.insert(as_str.clone()) && found.len() < MAX_LINKS {
                    found.push(as_str);
                }
            } else if page_depth < depth && is_followable(&link, &base, filter) {
                queue.push((link, page_depth + 1));
            }
        }
    }

    Ok(found)
}

/// Fetch one page, or `None` for anything that is not readable HTML.
async fn fetch_page(client: &Client, url: &Url) -> Option<String> {
    use futures_util::StreamExt;

    let response = tokio::time::timeout(PAGE_TIMEOUT, client.get(url.clone()).send())
        .await
        .ok()?
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    // A crawl that followed a link to a 4 GB ISO and read it into memory would
    // be a bug with a memory footprint.
    let is_html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("text/html") || v.contains("application/xhtml"));
    if !is_html {
        return None;
    }

    let mut stream = response.bytes_stream();
    let mut body_bytes = Vec::new();

    tokio::time::timeout(PAGE_TIMEOUT, async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.ok()?;
            let remaining = MAX_BODY.saturating_sub(body_bytes.len());
            if remaining == 0 {
                break;
            }
            let take = chunk.len().min(remaining);
            body_bytes.extend_from_slice(&chunk[..take]);
            if body_bytes.len() >= MAX_BODY {
                break;
            }
        }
        Some(())
    })
    .await
    .ok()??;

    Some(String::from_utf8_lossy(&body_bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn quoted_attributes_are_read_either_way() {
        let html = r#"<a href="one.zip">1</a><a href='two.zip'>2</a>"#;
        assert_eq!(extract_attr(html, "href"), vec!["one.zip", "two.zip"]);
    }

    #[test]
    fn bare_attributes_are_read_too() {
        let html = "<a href=plain.zip>x</a>";
        assert_eq!(extract_attr(html, "href"), vec!["plain.zip"]);
    }

    #[test]
    fn the_attribute_name_must_stand_alone() {
        let html = r#"<a data-href="no.zip" href="yes.zip">x</a>"#;
        assert_eq!(extract_attr(html, "href"), vec!["yes.zip"]);
    }

    #[test]
    fn attribute_case_does_not_matter() {
        let html = r#"<A HREF="A.zip">x</A>"#;
        assert_eq!(extract_attr(html, "href"), vec!["A.zip"], "the value keeps its own case");
    }

    /// `&amp;` in HTML is `&` in the URL; sent literally it is a different
    /// query string and usually a 404.
    #[test]
    fn html_entities_in_a_query_string_are_decoded() {
        let html = r#"<a href="get?id=1&amp;key=2">x</a>"#;
        assert_eq!(extract_attr(html, "href"), vec!["get?id=1&key=2"]);
    }

    #[test]
    fn an_unterminated_attribute_stops_the_scan() {
        let html = r#"<a href="one.zip">ok</a><a href="never-closed"#;
        assert_eq!(extract_attr(html, "href"), vec!["one.zip"]);
    }

    #[test]
    fn an_empty_document_yields_nothing() {
        assert!(extract_attr("", "href").is_empty());
        assert!(extract_attr("<p>no links</p>", "href").is_empty());
    }

    #[test]
    fn an_empty_filter_takes_files_but_not_pages() {
        assert!(matches_filter(&url("https://e.test/a.zip"), ""));
        assert!(matches_filter(&url("https://e.test/a.iso"), ""));
        assert!(!matches_filter(&url("https://e.test/index.html"), ""));
        assert!(!matches_filter(&url("https://e.test/page.php"), ""));
        assert!(!matches_filter(&url("https://e.test/dir/"), ""), "no extension is not a file");
    }

    #[test]
    fn a_filter_takes_only_what_it_names() {
        assert!(matches_filter(&url("https://e.test/a.zip"), "zip,pdf"));
        assert!(matches_filter(&url("https://e.test/a.PDF"), "zip,pdf"), "case is ignored");
        assert!(!matches_filter(&url("https://e.test/a.mp3"), "zip,pdf"));
        // Written with or without the dot, spaced or not.
        assert!(matches_filter(&url("https://e.test/a.zip"), " .zip , .pdf "));
    }

    #[test]
    fn only_same_host_pages_are_followed() {
        let base = url("https://e.test/index.html");
        assert!(is_followable(&url("https://e.test/more.html"), &base, ""));
        assert!(is_followable(&url("https://e.test/pub/"), &base, ""));
        assert!(!is_followable(&url("https://other.test/more.html"), &base, ""));
        assert!(!is_followable(&url("https://e.test/a.zip"), &base, ""), "a file is not a page");
    }

    #[test]
    fn matches_ftp_and_magnets() {
        assert!(matches_filter(&url("ftp://example.com/archive.zip"), ""));
        assert!(matches_filter(&url("ftp://example.com/archive.zip"), "zip"));
        assert!(!matches_filter(&url("ftp://example.com/archive.tar"), "zip"));

        assert!(matches_filter(&url("magnet:?xt=urn:btih:abc"), ""));
        assert!(matches_filter(&url("magnet:?xt=urn:btih:abc"), "torrent"));
        assert!(matches_filter(&url("magnet:?xt=urn:btih:abc"), "magnet"));
        assert!(!matches_filter(&url("magnet:?xt=urn:btih:abc"), "zip"));
    }
}
