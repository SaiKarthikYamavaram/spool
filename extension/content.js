// In-page download panel — the IDM Integration Module's signature affordance.
//
// The popup already lists what was detected, but it only appears when the user
// thinks to open it. This surfaces a small button over the page itself when
// there is a video worth grabbing, and stays out of the way otherwise.

(() => {
  if (window.__spoolPanel) return; // survive re-injection
  window.__spoolPanel = true;

  // ---------------------------------------------------------------------------
  // Media Sniffing (all frames: top frame & player iframes)
  // ---------------------------------------------------------------------------
  const reportedUrls = new Set();

  function reportMedia(url, mime = "", title = "", isStream = false) {
    if (!url || !/^https?:/i.test(url)) return;
    if (reportedUrls.has(url)) return;
    reportedUrls.add(url);

    try {
      chrome.runtime.sendMessage({
        type: "mediaFound",
        url,
        mime,
        title: title || document.title || "",
        pageUrl: location.href,
        isStream,
      });
    } catch {
      // Worker suspended or extension context invalidated
    }
  }

  function inspectMedia(el) {
    if (!el) return;
    const isVideo = el.tagName?.toLowerCase() === "video";
    const title = el.getAttribute("title") || el.getAttribute("data-title") || document.title || "";

    // 1. Direct src attribute or currentSrc
    const src = el.currentSrc || el.src;
    if (src && /^https?:/i.test(src) && !src.startsWith("blob:")) {
      if (!/\.(html?|php|jsp|asp)(\?|#|$)/i.test(src) || /(?:m3u8|mpd)/i.test(src)) {
        const isStream = /mpegurl|dash|hls/i.test(el.type || "") || /\.(m3u8|mpd)/i.test(src);
        reportMedia(src, el.type || (isVideo ? "video/mp4" : "audio/mpeg"), title, isStream);
      }
    }

    // 2. <source> child elements
    const sources = el.querySelectorAll("source");
    for (const s of sources) {
      const sSrc = s.src || s.getAttribute("src");
      const sType = s.type || s.getAttribute("type") || "";
      if (sSrc && /^https?:/i.test(sSrc) && !sSrc.startsWith("blob:")) {
        if (!/\.(html?|php|jsp|asp)(\?|#|$)/i.test(sSrc) || /(?:m3u8|mpd)/i.test(sSrc)) {
          const isStream = /mpegurl|dash|hls/i.test(sType) || /\.(m3u8|mpd)/i.test(sSrc);
          reportMedia(sSrc, sType || (isVideo ? "video/mp4" : "audio/mpeg"), title, isStream);
        }
      }
    }
  }

  // Resolve direct stream from embedded Streamvin / PlayerLare frames
  function resolveStreamvin() {
    if (!/streamvin\.com/i.test(location.hostname)) return;
    const match = location.pathname.match(/\/video\/([a-zA-Z0-9]+)/);
    if (!match) return;
    const hash = match[1];
    fetch("/player/index.php?data=" + encodeURIComponent(hash) + "&do=getVideo", {
      method: "POST",
      headers: {
        "Content-Type": "application/x-www-form-urlencoded",
        "X-Requested-With": "XMLHttpRequest",
      },
      body: "hash=" + encodeURIComponent(hash) + "&r=" + encodeURIComponent(document.referrer || location.href),
    })
      .then((r) => r.json())
      .then((data) => {
        const streamUrl = data.securedLink || data.videoSource;
        if (streamUrl && /^https?:/i.test(streamUrl)) {
          const title = data.title || document.title || "Video";
          reportMedia(streamUrl, "application/x-mpegURL", title, true);
        }
      })
      .catch(() => {});
  }

  function scanMedia() {
    document.querySelectorAll("video, audio").forEach(inspectMedia);
    resolveStreamvin();
  }

  scanMedia();

  // Watch for dynamic video injection (e.g. user clicks Play button)
  const observer = new MutationObserver((mutations) => {
    for (const m of mutations) {
      for (const node of m.addedNodes) {
        if (node.nodeType !== 1) continue;
        const tag = node.tagName?.toLowerCase();
        if (tag === "video" || tag === "audio") {
          inspectMedia(node);
        } else if (tag === "source" && node.parentElement?.tagName?.toLowerCase() === "video") {
          inspectMedia(node.parentElement);
        } else if (node.querySelectorAll) {
          node.querySelectorAll("video, audio").forEach(inspectMedia);
        }
      }
      if (m.type === "attributes" && (m.attributeName === "src" || m.attributeName === "currentSrc")) {
        const tag = m.target.tagName?.toLowerCase();
        if (tag === "video" || tag === "audio") {
          inspectMedia(m.target);
        }
      }
    }
  });

  if (document.documentElement) {
    observer.observe(document.documentElement, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["src"],
    });
  }

  // Capture media events when playback starts or media is loaded
  ["play", "playing", "loadedmetadata", "canplay", "loadstart"].forEach((ev) => {
    document.addEventListener(ev, (e) => {
      if (e.target && (e.target.tagName === "VIDEO" || e.target.tagName === "AUDIO")) {
        inspectMedia(e.target);
      }
    }, true);
  });

  // ---------------------------------------------------------------------------
  // In-page download panel (Top frame only)
  // ---------------------------------------------------------------------------
  if (window !== window.top) return;

  const ID = "spool-panel";
  let panel = null;
  let hideTimer = null;

  // The panel lives in a CLOSED SHADOW ROOT, not in the page.
  //
  // `all: initial` on an inline style is not enough on its own: it has normal
  // priority, so any page rule carrying `!important` (`div, span { color:
  // crimson !important }` is a real pattern on styled sites) overrides it and
  // the panel inherits the page's type and colour. A shadow boundary excludes
  // page CSS structurally, so the styles below are the only ones that apply —
  // and real CSS means :hover and keyframes instead of JS mouse handlers.
  //
  // The host element still sits in the page, so its own few properties are
  // pinned with `!important`; an inline `!important` outranks any page rule.
  const HOST = [
    "all:initial!important",
    "position:fixed!important",
    "z-index:2147483647!important",
    "top:16px!important",
    "right:16px!important",
    "display:block!important",
  ].join(";");

  // Kept in sync with extension/theme.css and src/App.css by hand.
  const STYLE = `
    :host { all: initial; }
    .panel {
      display: flex;
      align-items: center;
      gap: 10px;
      padding: 9px 11px 9px 9px;
      font: 500 13px/1.3 Inter, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
      color: #EDEDEF;
      background: rgba(11, 11, 15, 0.92);
      border: 1px solid rgba(255, 255, 255, 0.10);
      border-radius: 12px;
      box-shadow: 0 0 0 1px rgba(0, 0, 0, 0.4), 0 10px 34px rgba(0, 0, 0, 0.55),
                  0 0 50px rgba(94, 106, 210, 0.28);
      backdrop-filter: saturate(1.4) blur(14px);
      -webkit-backdrop-filter: saturate(1.4) blur(14px);
      cursor: pointer;
      user-select: none;
      /* Entrance: 6px and 300ms of expo-out, the app's motion for everything. */
      opacity: 0;
      transform: translateY(-6px);
      transition: opacity .3s cubic-bezier(.16, 1, .3, 1),
                  transform .3s cubic-bezier(.16, 1, .3, 1),
                  box-shadow .2s cubic-bezier(.16, 1, .3, 1);
    }
    .panel.in { opacity: 1; transform: translateY(0); }
    .panel.in:hover {
      box-shadow: 0 0 0 1px rgba(0, 0, 0, 0.4), 0 12px 40px rgba(0, 0, 0, 0.6),
                  0 0 70px rgba(94, 106, 210, 0.4);
    }
    .panel.in:active { transform: scale(0.98); }

    /* The accent lives on this one tile rather than the whole panel, so the
       surface stays monochrome and the glow reads as a light source. */
    .mark {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      flex-shrink: 0;
      width: 24px;
      height: 24px;
      border-radius: 8px;
      background: linear-gradient(135deg, #5E6AD2, #8B5CF6);
      box-shadow: 0 3px 10px rgba(94, 106, 210, 0.5),
                  inset 0 1px 0 0 rgba(255, 255, 255, 0.2);
    }
    .label {
      font-weight: 560;
      letter-spacing: -0.005em;
      white-space: nowrap;
    }
    .close {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      flex-shrink: 0;
      width: 20px;
      height: 20px;
      margin-left: 2px;
      border-radius: 6px;
      font-size: 11px;
      color: #8A8F98;
      transition: color .2s cubic-bezier(.16, 1, .3, 1),
                  background .2s cubic-bezier(.16, 1, .3, 1);
    }
    .close:hover { color: #EDEDEF; background: rgba(255, 255, 255, 0.08); }

    @media (prefers-reduced-motion: reduce) {
      .panel { transition: none; opacity: 1; transform: none; }
      .panel.in:active { transform: none; }
    }
  `;

  // The spool mark, same geometry as IconMark in src/components/icons.tsx: a
  // ring broken into four segments, heavier stroke and butt caps so the gaps
  // survive at 15px.
  const GLYPH =
    '<svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="#fff" ' +
    'stroke-width="4.2">' +
    '<circle cx="12" cy="12" r="9.5" stroke-dasharray="12.435 2.487" ' +
    'transform="rotate(-90 12 12)"/></svg>';

  function build(label) {
    const host = document.createElement("div");
    host.id = ID;
    host.style.cssText = HOST;

    const root = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = STYLE;

    const el = document.createElement("div");
    el.className = "panel";
    el.setAttribute("role", "dialog");
    el.setAttribute("aria-label", "Download with spool");
    el.innerHTML =
      `<span class="mark">${GLYPH}</span>` +
      `<span class="label"></span>` +
      `<span class="close" role="button" title="Hide">✕</span>`;

    const text = el.querySelector(".label");
    text.textContent = label;

    el.querySelector(".close").addEventListener("click", (e) => {
      e.stopPropagation();
      remove();
      // Don't nag again for this page view.
      window.__spoolDismissed = true;
    });

    root.append(style, el);
    return { host, el, text };
  }

  function remove() {
    clearTimeout(hideTimer);
    panel?.host.remove();
    panel = null;
  }

  function show(label, onClick) {
    if (window.__spoolDismissed) return;
    if (!panel) {
      panel = build(label);
      panel.el.addEventListener("click", async () => {
        panel.text.textContent = "Sending…";
        const ok = await onClick();
        panel.text.textContent = ok ? "Sent to spool" : "spool not running";
        hideTimer = setTimeout(remove, 2500);
      });
      document.documentElement.appendChild(panel.host);
      // Next frame, so the transition has a starting value to animate from.
      requestAnimationFrame(() => panel?.el.classList.add("in"));
    } else {
      panel.text.textContent = label;
    }
  }

  chrome.runtime.onMessage.addListener((msg, _s, sendResponse) => {
    if (msg.type === "showPanel") {
      show(msg.label || "Download this video", async () => {
        const res = await chrome.runtime.sendMessage({
          type: "download",
          url: msg.url,
          referer: msg.referer || location.href,
          video: msg.video !== undefined ? msg.video : true,
        });
        return !!res?.ok;
      });
      sendResponse({ shown: true });
    } else if (msg.type === "hidePanel") {
      remove();
      sendResponse({ shown: false });
    }
    return true;
  });
})();
