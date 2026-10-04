// Shared helpers for background, popup and options. Loaded via importScripts in
// the service worker and a plain <script> in the pages.

// The native host the app registers with each browser (see server.rs).
const HOST = "com.saikarthik.spool";

// Cookies bind to the exact User-Agent that earned them, so always send the
// browser's real one.
const UA = typeof navigator !== "undefined" ? navigator.userAgent : "";

const DEFAULTS = {
  enabled: true,
  intercept: false, // hand the browser's own downloads to spool
  // Let the app ask for a folder/quality per download instead of using the
  // defaults. The app window comes forward with its add dialog.
  askBeforeDownload: true,
  // Show the in-page button on video pages (IDM's floating panel).
  showPanel: true,
  grabMedia: true, // detect streamable/attachment media on pages
  minSizeKb: 512, // ignore anything smaller
  types: { video: true, audio: true, archive: true, document: true, image: false, other: true },
  excludeDomains: [], // hosts to never touch
};

async function getSettings() {
  // Storage can fail — during shutdown, or on a profile whose storage is
  // unavailable. The defaults are a usable answer, so return them rather than
  // making every caller handle a rejection.
  let stored = {};
  try {
    stored = await chrome.storage.local.get("settings");
  } catch {
    /* fall through to the defaults */
  }
  const saved = stored.settings || {};
  return { ...DEFAULTS, ...saved, types: { ...DEFAULTS.types, ...(saved.types || {}) } };
}

async function setSettings(patch) {
  const cur = await getSettings();
  const next = { ...cur, ...patch };
  await chrome.storage.local.set({ settings: next });
  return next;
}

const STREAM_MIMES = [
  "application/vnd.apple.mpegurl",
  "application/x-mpegurl",
  "audio/x-mpegurl",
  "application/dash+xml",
  "application/vnd.ms-sstr+xml",
  "application/x-hls",
];

const VIDEO_EXTS = [
  "mp4", "mkv", "webm", "avi", "mov", "flv", "m3u8", "ts", "mpd", "m4s", "3gp", "wmv", "asf",
];

// Detect individual chunk / segment requests so we don't flood the detected list
// with hundreds of 2-second clips while an adaptive stream is playing.
function isStreamSegment(url) {
  if (!url) return false;
  if (/(?:[\?&](?:ts|segment|chunk|frag|range)=\d+)/i.test(url)) return true;
  if (/(?:seg|segment|chunk|frag|fragment|part)[-_]?\d+\.(?:ts|m4s|mp4|aac)/i.test(url)) return true;
  if (/\.(ts|m4s)(\?|#|$)/i.test(url) && /(?:hls|dash|playlist|stream)/i.test(url)) return true;
  return false;
}

// HLS/DASH manifests are playlists, not files — downloading one over HTTP just
// saves a few KB of text. They have to go through yt-dlp, which fetches the
// segments and muxes them.
function isStreamManifest(url, contentType = "") {
  if (!url && !contentType) return false;
  if (isStreamSegment(url)) return false;

  const ct = (contentType || "").toLowerCase().split(";")[0].trim();
  // An HTML document or styling/script is never a stream playlist
  if (["text/html", "text/css", "application/javascript", "text/javascript", "application/json"].includes(ct)) {
    return false;
  }

  if (STREAM_MIMES.includes(ct)) return true;

  const u = url || "";
  if (/\.(m3u8|mpd)(\?|#|$)/i.test(u)) return true;

  // Adaptive streaming paths or query parameters
  if (/(?:[\/=]hls[\/=\.]|[\/=]dash[\/=\.]|[\?&](?:t|format|type)=(?:hls|m3u8|mpd)\b|\/playlist|\/manifest)/i.test(u)) {
    if (/\.(js|css|json|html?|png|jpg|jpeg|gif|svg|woff2?|map)(\?|#|$)/i.test(u)) return false;
    return true;
  }
  return false;
}

// Map a Content-Type / filename to one of the type buckets, or null if it is
// not something worth downloading (html, json, css, ...).
function classify(contentType, filename, url = "") {
  const ct = (contentType || "").toLowerCase().split(";")[0].trim();
  const ext = (filename || "").toLowerCase().split(".").pop() || "";

  // Explicit non-downloadable page furniture — HTML is never a media file
  if (
    ["text/html", "text/css", "application/javascript", "text/javascript", "application/json", "text/json"].includes(ct)
  ) {
    return null;
  }

  if (
    ct.startsWith("video/") ||
    STREAM_MIMES.includes(ct) ||
    VIDEO_EXTS.includes(ext) ||
    (url && isStreamManifest(url, ct))
  ) {
    return "video";
  }

  if (ct.startsWith("audio/") || ["mp3", "flac", "wav", "aac", "ogg", "m4a", "opus", "wma"].includes(ext)) {
    return "audio";
  }

  if (
    ["application/zip", "application/x-tar", "application/x-7z-compressed", "application/x-rar-compressed",
     "application/gzip", "application/x-bzip2"].includes(ct) ||
    ["zip", "tar", "gz", "xz", "7z", "rar", "bz2"].includes(ext)
  ) return "archive";

  if (
    ["application/pdf", "application/msword", "application/epub+zip"].includes(ct) ||
    ["pdf", "doc", "docx", "epub"].includes(ext)
  ) return "document";

  if (ct.startsWith("image/") || ["png", "jpg", "jpeg", "gif", "webp", "svg", "bmp"].includes(ext)) return "image";

  // A generic binary or an explicit attachment is downloadable but untyped.
  if (ct === "application/octet-stream" || ["iso", "img", "dmg", "exe", "appimage", "deb", "rpm", "bin"].includes(ext)) return "other";

  return null;
}

function hostOf(url) {
  try { return new URL(url).hostname; } catch { return ""; }
}

// Sites whose videos are adaptive streams (DASH/HLS) that header-detection
// cannot surface as a file — they go through the yt-dlp engine instead. Kept in
// sync with VIDEO_HOSTS in src-tauri/src/ytdlp.rs.
const VIDEO_HOSTS = [
  "youtube.com", "youtu.be", "m.youtube.com", "music.youtube.com",
  "vimeo.com", "dailymotion.com", "twitch.tv", "clips.twitch.tv",
  "tiktok.com", "instagram.com", "facebook.com", "fb.watch",
  "twitter.com", "x.com", "reddit.com", "soundcloud.com",
  "bilibili.com", "nicovideo.jp", "streamable.com",
];

function isVideoSite(url) {
  const h = hostOf(url).toLowerCase();
  return VIDEO_HOSTS.some((d) => h === d || h.endsWith(`.${d}`));
}

// Clean a browser tab title into a video name: strip the trailing site suffix
// ("… - YouTube", "… on Vimeo") and a leading unread-count badge ("(3) …").
function videoTitle(tabTitle) {
  if (!tabTitle) return null;
  let t = tabTitle.replace(/^\(\d+\)\s*/, "");
  t = t.replace(/\s*[-|]\s*(YouTube|Vimeo|Dailymotion|Twitch|TikTok|Reddit|SoundCloud|Bilibili)\s*$/i, "");
  t = t.replace(/\s+on Vimeo$/i, "");
  return t.trim() || null;
}

function isExcluded(url, settings) {
  const host = hostOf(url);
  return settings.excludeDomains.some((d) => host === d || host.endsWith(`.${d}`));
}

function filenameFromUrl(url) {
  try {
    const name = decodeURIComponent(new URL(url).pathname.split("/").pop() || "");
    return name || url;
  } catch { return url; }
}

// Whether an intercepted browser download is one spool should take over.
//
// Pure and synchronous on purpose: `downloads.onCreated` is a race — Chrome
// does not wait for an async listener, so anything awaited before the cancel
// is time the browser spends putting up its own Save-As dialog and starting
// the transfer. The caller holds the settings; this only decides.
function shouldTakeOver(item, settings) {
  if (!settings || !settings.enabled || !settings.intercept) return false;

  const url = item.finalUrl || item.url;
  if (!url || !/^https?:/i.test(url)) return false;
  if (isExcluded(url, settings)) return false;

  const type = classify(item.mime, item.filename || filenameFromUrl(url), url);
  if (!type || !settings.types[type]) return false;

  // fileSize is often -1 or 0 at this point; only a known, genuinely small
  // file is skipped. Manifests bypass the size floor.
  if (!isStreamManifest(url, item.mime) && item.fileSize > 0 && item.fileSize < settings.minSizeKb * 1024) return false;

  return true;
}

async function cookieHeaderFor(url) {
  try {
    const cookies = await chrome.cookies.getAll({ url });
    if (!cookies.length) return null;
    return cookies.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch { return null; }
}

// Send one download to spool. `video` forces the yt-dlp engine (for streaming
// sites). `ask` overrides the "ask before download" setting — batch grabs pass
// false so a 30-link grab doesn't open 30 dialogs. Returns true on success.
async function sendToSpool(url, referer, video = false, ask = null) {
  const cookie = await cookieHeaderFor(url);
  const askBeforeDownload = ask === null ? (await getSettings()).askBeforeDownload : ask;
  const reply = await callSpool({
    type: "add", url, cookie, userAgent: UA, referer, video, ask: askBeforeDownload,
  });
  return reply.ok === true;
}

async function spoolAlive() {
  return (await callSpool({ type: "ping" })).ok === true;
}

// One request to the app through its native host. The browser rejects when the
// host is not registered or exits without answering (the app is not running),
// so every failure comes back as an empty reply rather than a throw.
async function callSpool(message) {
  try {
    return (await chrome.runtime.sendNativeMessage(HOST, message)) || {};
  } catch {
    return {};
  }
}
