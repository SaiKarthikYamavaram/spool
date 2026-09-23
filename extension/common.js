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

// Map a Content-Type / filename to one of the type buckets, or null if it is
// not something worth downloading (html, json, css, ...).
function classify(contentType, filename) {
  const ct = (contentType || "").toLowerCase().split(";")[0].trim();
  const ext = (filename || "").toLowerCase().split(".").pop() || "";

  if (ct.startsWith("video/") || ["mp4", "mkv", "webm", "avi", "mov", "flv", "m3u8", "ts"].includes(ext)) return "video";
  if (ct.startsWith("audio/") || ["mp3", "flac", "wav", "aac", "ogg", "m4a"].includes(ext)) return "audio";
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

// HLS/DASH manifests are playlists, not files — downloading one over HTTP just
// saves a few KB of text. They have to go through yt-dlp, which fetches the
// segments and muxes them.
function isStreamManifest(url) {
  return /\.(m3u8|mpd)(\?|#|$)/i.test(url || "");
}

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

  const type = classify(item.mime, item.filename || filenameFromUrl(url));
  if (!type || !settings.types[type]) return false;

  // fileSize is often -1 or 0 at this point; only a known, genuinely small
  // file is skipped.
  if (item.fileSize > 0 && item.fileSize < settings.minSizeKb * 1024) return false;

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
