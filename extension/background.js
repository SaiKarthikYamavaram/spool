// spool browser integration — service worker.
//
// Beyond routing a single link, this mirrors what a download manager's browser
// module does: it watches page responses for downloadable media, keeps a
// per-tab list, badges the toolbar with the count, and lets the popup grab any
// of them (or every link/image on the page) — all replayed through spool with
// the browser's own cookies so authenticated and challenge-protected files
// work.

importScripts("common.js");

// ---------------------------------------------------------------------------
// Per-tab detected media, kept in session storage so it survives the service
// worker being suspended but clears when the browser closes.
// ---------------------------------------------------------------------------

const keyFor = (tabId) => `detected_${tabId}`;

async function getDetected(tabId) {
  const k = keyFor(tabId);
  const s = await chrome.storage.session.get(k);
  return s[k] || [];
}

async function addDetected(tabId, item) {
  const list = await getDetected(tabId);
  if (list.some((x) => x.url === item.url)) return; // dedup
  list.unshift(item);
  if (list.length > 50) list.pop();
  await chrome.storage.session.set({ [keyFor(tabId)]: list });
  updateBadge(tabId, list.length);
}

async function clearDetected(tabId) {
  await chrome.storage.session.remove(keyFor(tabId));
  updateBadge(tabId, 0);
}

function updateBadge(tabId, count) {
  chrome.action.setBadgeText({ tabId, text: count ? String(count) : "" });
  chrome.action.setBadgeBackgroundColor({ tabId, color: "#6366f1" });
}

// ---------------------------------------------------------------------------
// Detection: inspect response headers for downloadable content.
// ---------------------------------------------------------------------------

chrome.webRequest.onHeadersReceived.addListener(
  (details) => {
    // Fire-and-forget; the listener itself is synchronous (non-blocking).
    void maybeDetect(details);
  },
  { urls: ["<all_urls>"], types: ["main_frame", "sub_frame", "xmlhttprequest", "media", "other"] },
  ["responseHeaders"]
);

async function maybeDetect(details) {
  if (details.tabId < 0) return; // not tied to a tab
  const settings = await getSettings();
  if (!settings.enabled || !settings.grabMedia) return;
  if (isExcluded(details.url, settings)) return;

  const headers = Object.fromEntries(
    (details.responseHeaders || []).map((h) => [h.name.toLowerCase(), h.value || ""])
  );

  const disposition = headers["content-disposition"] || "";
  const contentType = headers["content-type"] || "";
  const length = parseInt(headers["content-length"] || "0", 10);

  const filename = filenameFromDisposition(disposition) || filenameFromUrl(details.url);
  const type = classify(contentType, filename);
  if (!type) return;

  // Type must be enabled, and size must clear the floor (unless the server
  // sent no length — common for streams, which we still want to surface).
  if (!settings.types[type]) return;
  // A manifest is tiny by nature; the size floor would always reject it.
  if (!isStreamManifest(details.url) && length && length < settings.minSizeKb * 1024) return;

  // An explicit attachment is always worth showing; otherwise require it to be
  // media or a sizeable binary, so ordinary page images/scripts don't flood.
  const isAttachment = /attachment/i.test(disposition);
  if (!isAttachment && (type === "image" || type === "document") && !length) return;

  await addDetected(details.tabId, {
    url: details.url,
    filename,
    type,
    size: length || 0,
  });
}

function filenameFromDisposition(disposition) {
  // filename*=UTF-8''name  wins over  filename="name"
  const ext = /filename\*=(?:UTF-8'')?([^;]+)/i.exec(disposition);
  if (ext) { try { return decodeURIComponent(ext[1].trim().replace(/"/g, "")); } catch { /* fall through */ } }
  const plain = /filename="?([^";]+)"?/i.exec(disposition);
  return plain ? plain[1].trim() : null;
}

// Clear a tab's list when it navigates to a new page; flag video pages so the
// toolbar shows a cue even though adaptive streams are never header-detected.
chrome.tabs.onUpdated.addListener((tabId, info, tab) => {
  if (info.status === "loading" && info.url) clearDetected(tabId);
  if (info.status === "complete" && tab && tab.url && isVideoSite(tab.url)) {
    chrome.action.setBadgeText({ tabId, text: "▶" });
    chrome.action.setBadgeBackgroundColor({ tabId, color: "#ef4444" });
    void offerPanel(tabId, tab);
  }
});

/// Put the in-page button on a video page, injecting the content script first
/// (no static content_scripts entry, so nothing runs on pages we never use).
async function offerPanel(tabId, tab) {
  const settings = await getSettings();
  if (!settings.enabled || !settings.showPanel) return;
  if (isExcluded(tab.url, settings)) return;

  try {
    await chrome.scripting.executeScript({ target: { tabId }, files: ["content.js"] });
    await chrome.tabs.sendMessage(tabId, {
      type: "showPanel",
      url: tab.url,
      label: videoTitle(tab.title) ? "Download this video" : "Download with spool",
    });
  } catch {
    // Restricted page (store, PDF viewer, other extensions) — no panel there.
  }
}
chrome.tabs.onRemoved.addListener((tabId) => clearDetected(tabId));

// ---------------------------------------------------------------------------
// Context menus
// ---------------------------------------------------------------------------

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: "spool-link", title: "Download with spool",
      contexts: ["link", "audio", "video", "image"],
    });
    chrome.contextMenus.create({
      id: "spool-video", title: "Download video with spool (yt-dlp)",
      contexts: ["page", "link", "video"],
    });
    chrome.contextMenus.create({
      id: "spool-selection", title: "Download links in selection with spool",
      contexts: ["selection"],
    });
    chrome.contextMenus.create({
      id: "spool-all-links", title: "Download all links on this page",
      contexts: ["page"],
    });
    chrome.contextMenus.create({
      id: "spool-all-images", title: "Download all images on this page",
      contexts: ["page"],
    });
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const referer = info.pageUrl || (tab && tab.url) || "";
  if (info.menuItemId === "spool-link") {
    const url = info.linkUrl || info.srcUrl;
    if (url) await sendOne(url, referer);
  } else if (info.menuItemId === "spool-video") {
    // Prefer an explicit link/media target; otherwise the page URL itself
    // (yt-dlp resolves the video from a watch page).
    const url = info.linkUrl || info.srcUrl || info.pageUrl || (tab && tab.url);
    if (url) await sendOne(url, referer, true, videoTitle(tab && tab.title));
  } else if (info.menuItemId === "spool-selection") {
    await grabFromPage(tab, "selection", referer);
  } else if (info.menuItemId === "spool-all-links") {
    await grabFromPage(tab, "links", referer);
  } else if (info.menuItemId === "spool-all-images") {
    await grabFromPage(tab, "images", referer);
  }
});

// Pull every link or image URL out of the page DOM and send the ones whose
// type is enabled in settings.
async function grabFromPage(tab, mode, referer) {
  if (!tab) return;
  const label = mode === "selection" ? "links in the selection" : mode;
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: { tabId: tab.id },
      func: (m) => {
        if (m === "selection") {
          // Only anchors inside the highlighted range.
          const sel = window.getSelection();
          if (!sel || sel.isCollapsed) return [];
          const range = sel.getRangeAt(0);
          return Array.from(document.querySelectorAll("a[href]"))
            .filter((a) => range.intersectsNode(a))
            .map((a) => a.href)
            .filter((u) => /^(https?|ftp):|^magnet:\?/i.test(u));
        }
        const q = m === "images" ? "img[src]" : "a[href]";
        const attr = m === "images" ? "src" : "href";
        return Array.from(document.querySelectorAll(q))
          .map((el) => el[attr])
          .filter((u) => /^(https?|ftp):|^magnet:\?/i.test(u));
      },
      args: [mode],
    });
  } catch {
    notify("Cannot read this page", "The browser blocked script access here.");
    return;
  }

  const urls = [...new Set(results?.[0]?.result || [])];
  const settings = await getSettings();
  const wanted = urls.filter((u) => {
    // A magnet has no host to exclude and no filename to classify; a page's
    // magnet links are always torrents, which is what the grab is for.
    if (/^magnet:/i.test(u)) return true;
    if (isExcluded(u, settings)) return false;
    const t = classify("", filenameFromUrl(u));
    return t && settings.types[t];
  });

  if (!wanted.length) {
    notify("Nothing to download", `No matching ${label} found.`);
    return;
  }
  let ok = 0;
  for (const u of wanted) {
    if (await sendToSpool(u, referer, isStreamManifest(u), false)) ok++;
  }
  notify("Sent to spool", `${ok} of ${wanted.length} ${label} queued.`);
}

// ---------------------------------------------------------------------------
// Intercept the browser's own downloads (opt-in).
// ---------------------------------------------------------------------------

// Settings, cached synchronously.
//
// The interception below cannot afford an `await` before it cancels, and
// reading storage is one. Kept fresh from the change event.
//
// Every call is caught: this runs at top level, where a rejected promise has
// no caller to receive it and surfaces as an unhandled error against the file
// itself. `chrome.storage` can genuinely reject — during shutdown, or if the
// profile's storage is unavailable — and losing the cache is not a reason to
// log an error the user cannot act on.
let cachedSettings = null;

function refreshSettings() {
  return getSettings().then(
    (s) => (cachedSettings = s),
    () => cachedSettings, // keep whatever was last known good
  );
}

refreshSettings();

// Prime the cache when the worker starts cold, so the first download of a
// session is decided synchronously like every other one.
chrome.runtime.onStartup?.addListener(refreshSettings);
chrome.runtime.onInstalled?.addListener(refreshSettings);

chrome.storage.onChanged.addListener((changes, area) => {
  if (area === "local" && changes.settings) refreshSettings();
});

chrome.downloads.onCreated.addListener((item) => {
  if (cachedSettings) {
    if (shouldTakeOver(item, cachedSettings)) takeOver(item);
    return;
  }
  // Cold worker with nothing primed yet: read storage, and accept that this
  // one download may lose the race. Every later one is decided in the same
  // tick the event arrives.
  refreshSettings().then((s) => {
    if (shouldTakeOver(item, s)) takeOver(item);
  });
});

/// Cancel first, ask questions after.
///
/// The old order checked that spool was reachable before cancelling, which
/// meant a round trip to 127.0.0.1 while the browser was already prompting for
/// a location and pulling bytes — so the download happened twice. Cancelling
/// is the first thing now, and the restore path below is what makes that safe:
/// if the handover fails for any reason, the browser gets the download back.
async function takeOver(item) {
  const url = item.finalUrl || item.url;

  try {
    await chrome.downloads.cancel(item.id);
  } catch {
    return; // already finished or uncancellable — do not double-download
  }
  // Erasing is cosmetic (it clears the cancelled row from the downloads page)
  // and must never abort the handover.
  try {
    await chrome.downloads.erase({ id: item.id });
  } catch {
    /* ignore */
  }

  const referer = item.referrer || url;
  if (await sendToSpool(url, referer)) {
    notify("Sent to spool", filenameFromUrl(url));
    return;
  }

  // Handover failed — spool is not running, or refused it. Give the download
  // back rather than silently losing it.
  try {
    await chrome.downloads.download({ url });
    notify("spool did not take it", "Restored the browser download.");
  } catch {
    notify("Download lost", "spool rejected it and the browser could not restart it.");
  }
}

// Keyboard shortcut: grab the current page's video without reaching for a menu.
chrome.commands?.onCommand.addListener(async (command) => {
  if (command !== "grab-video") return;
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab?.url) return;
  await sendOne(tab.url, tab.url, true, videoTitle(tab.title));
});

// ---------------------------------------------------------------------------
// Messages from popup
// ---------------------------------------------------------------------------

chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  (async () => {
    if (msg.type === "getDetected") {
      sendResponse({ items: await getDetected(msg.tabId), alive: await spoolAlive() });
    } else if (msg.type === "download") {
      sendResponse({ ok: await sendToSpool(msg.url, msg.referer, msg.video || false) });
    } else if (msg.type === "downloadAll") {
      const items = await getDetected(msg.tabId);
      let ok = 0;
      for (const it of items) {
        if (await sendToSpool(it.url, msg.referer, isStreamManifest(it.url), false)) ok++;
      }
      sendResponse({ ok, total: items.length });
    } else if (msg.type === "clear") {
      await clearDetected(msg.tabId);
      sendResponse({ ok: true });
    }
  })();
  return true; // async response
});

async function sendOne(url, referer, video = false, name = null) {
  const ok = await sendToSpool(url, referer, video);
  const label = video ? (name || "Video") : filenameFromUrl(url);
  notify(ok ? "Sent to spool" : "spool not reachable",
         ok ? label : "Start the spool app and try again.");
}

function notify(title, message) {
  chrome.notifications?.create({ type: "basic", iconUrl: "icon128.png", title, message: message || "" });
}
