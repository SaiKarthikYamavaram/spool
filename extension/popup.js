function fmtSize(n) {
  if (!n) return "size unknown";
  const u = ["KB", "MB", "GB", "TB"];
  let v = n / 1024, i = 0;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${u[i]}`;
}

// Short, clear type labels for the tag chip.
const TYPE_LABEL = {
  video: "VID", audio: "AUD", archive: "ZIP",
  document: "DOC", image: "IMG", other: "BIN",
};

const listEl = document.getElementById("list");
const emptyEl = document.getElementById("empty");
const allBar = document.getElementById("allBar");

// A video card counts as content, so the "nothing detected" line must not show.
let hasVideo = false;
const dot = document.getElementById("dot");
const statusText = document.getElementById("statusText");

let tab, referer;

async function init() {
  [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab) {
    dot.className = "dot down";
    statusText.textContent = "No active tab";
    return;
  }
  referer = tab.url || "";

  const { items, alive } = await chrome.runtime.sendMessage({ type: "getDetected", tabId: tab.id });
  dot.className = `dot ${alive ? "up" : "down"}`;
  statusText.textContent = alive ? "Connected" : "App not running";

  render(items || []);

  // Known video site → offer a direct yt-dlp download of the page itself,
  // since adaptive streams never appear as a detectable file.
  if (isVideoSite(referer)) {
    hasVideo = true;
    document.getElementById("videoSection").hidden = false;
    emptyEl.hidden = true;
    const title = videoTitle(tab.title);
    if (title) {
      document.getElementById("videoName").textContent = title;
    }
    // Pull the page's og:image for a preview thumbnail.
    try {
      const [res] = await chrome.scripting.executeScript({
        target: { tabId: tab.id },
        func: () =>
          document.querySelector('meta[property="og:image"]')?.content ||
          document.querySelector('meta[name="twitter:image"]')?.content ||
          null,
      });
      const src = res?.result;
      if (src) {
        const img = document.getElementById("videoThumb");
        img.src = src;
        img.hidden = false;
        document.getElementById("videoTag").hidden = true;
      }
    } catch {
      /* page blocked script access; keep the text tag */
    }
    document.getElementById("downloadVideo").addEventListener("click", async (e) => {
      const btn = e.currentTarget;
      btn.disabled = true;
      btn.textContent = "Sent";
      await chrome.runtime.sendMessage({ type: "download", url: referer, referer, video: true });
    });
  }

  const s = await getSettings();
  document.getElementById("intercept").checked = s.intercept;
  document.getElementById("ask").checked = s.askBeforeDownload;
}

function render(items) {
  listEl.innerHTML = "";
  if (!items.length) {
    emptyEl.hidden = hasVideo; // a video card already fills the popup
    allBar.hidden = true;
    return;
  }
  emptyEl.hidden = true;
  allBar.hidden = false;

  for (const it of items) {
    const isStream = it.isStream || isStreamManifest(it.url);
    const tagText = isStream ? "HLS" : (it.type === "video" ? "VID" : TYPE_LABEL[it.type] || "BIN");
    const subText = isStream ? "stream · via yt-dlp" : `${it.type} · ${fmtSize(it.size)}`;
    const row = document.createElement("div");
    row.className = "item";
    row.innerHTML = `
      <span class="tag" data-type="${isStream ? "video" : escapeAttr(it.type)}">${tagText}</span>
      <div class="meta">
        <div class="name" title="${escapeAttr(it.url)}">${escapeHtml(it.filename)}</div>
        <div class="sub">${subText}</div>
      </div>
      <button class="dl">Download</button>`;
    row.querySelector(".dl").addEventListener("click", async (e) => {
      const btn = e.currentTarget;
      btn.disabled = true; btn.textContent = "Sent";
      await chrome.runtime.sendMessage({
        type: "download",
        url: it.url,
        referer: it.referer || referer,
        // A manifest or video stream must be handed to yt-dlp
        video: isStream || it.type === "video",
      });
    });
    listEl.appendChild(row);
  }
}

document.getElementById("downloadAll").addEventListener("click", async (e) => {
  e.currentTarget.disabled = true;
  const res = await chrome.runtime.sendMessage({ type: "downloadAll", tabId: tab.id, referer });
  e.currentTarget.textContent = `Sent ${res.ok}/${res.total}`;
});

document.getElementById("clear").addEventListener("click", async () => {
  await chrome.runtime.sendMessage({ type: "clear", tabId: tab.id });
  render([]);
});

document.getElementById("intercept").addEventListener("change", async (e) => {
  await setSettings({ intercept: e.currentTarget.checked });
});

document.getElementById("ask").addEventListener("change", async (e) => {
  await setSettings({ askBeforeDownload: e.currentTarget.checked });
});

document.getElementById("options").addEventListener("click", () => chrome.runtime.openOptionsPage());

function escapeHtml(s) { return s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c])); }
function escapeAttr(s) { return s.replace(/"/g, "&quot;"); }

init();
