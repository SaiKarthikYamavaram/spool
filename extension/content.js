// In-page per-video overlay & download panel — IDM-style player integration.
//
// Attaches an elegant floating download badge directly to qualifying video
// players on the page. In multi-video pages (social feeds, video portals), each
// player gets its own independent overlay that reveals on hover or when playback
// starts. Works across top frames and embedded player iframes.

(() => {
  if (window.__spoolContentInjected) return;
  window.__spoolContentInjected = true;

  // ---------------------------------------------------------------------------
  // Helpers
  // ---------------------------------------------------------------------------
  const VIDEO_DOMAINS = [
    "youtube.com", "youtu.be", "vimeo.com", "dailymotion.com",
    "twitch.tv", "tiktok.com", "instagram.com", "facebook.com",
    "twitter.com", "x.com", "reddit.com", "soundcloud.com",
    "bilibili.com", "streamable.com",
  ];

  function isKnownVideoHost(hostname) {
    const h = (hostname || "").toLowerCase();
    return VIDEO_DOMAINS.some((d) => h === d || h.endsWith(`.${d}`));
  }

  function cleanVideoTitle(tabTitle) {
    if (!tabTitle) return "";
    let t = tabTitle.replace(/^\(\d+\)\s*/, "");
    t = t.replace(/\s*[-|]\s*(YouTube|Vimeo|Dailymotion|Twitch|TikTok|Reddit|SoundCloud|Bilibili)\s*$/i, "");
    t = t.replace(/\s+on Vimeo$/i, "");
    return t.trim() || "";
  }

  function isStreamManifestUrl(url) {
    if (!url) return false;
    if (/\.(m3u8|mpd)(\?|#|$)/i.test(url)) return true;
    if (/(?:[\/=]hls[\/=\.]|[\/=]dash[\/=\.]|[\?&](?:t|format|type)=(?:hls|m3u8|mpd)\b|\/playlist|\/manifest)/i.test(url)) {
      if (/\.(js|css|json|html?|png|jpg|jpeg|gif|svg|woff2?|map)(\?|#|$)/i.test(url)) return false;
      return true;
    }
    return false;
  }

  // Spool mark SVG glyph
  const GLYPH =
    '<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="#fff" stroke-width="4.2">' +
    '<circle cx="12" cy="12" r="9.5" stroke-dasharray="12.435 2.487" transform="rotate(-90 12 12)"/>' +
    '</svg>';

  // ---------------------------------------------------------------------------
  // Media Sniffing & Per-Video Association
  // ---------------------------------------------------------------------------
  const reportedUrls = new Set();
  const mediaInfoMap = new WeakMap(); // HTMLMediaElement -> { url, mime, title, isStream }
  let lastReportedMedia = null;

  function reportMedia(url, mime = "", title = "", isStream = false, el = null) {
    if (!url || !/^https?:/i.test(url)) return;

    const item = {
      url,
      mime,
      title: title || cleanVideoTitle(document.title) || "Video",
      isStream,
    };
    lastReportedMedia = item;

    if (el && el.tagName?.toLowerCase() === "video") {
      mediaInfoMap.set(el, item);
      if (typeof overlayManager !== "undefined" && overlayManager) {
        const overlay = overlayManager.overlays.get(el);
        if (overlay) overlay.setMedia(item);
      }
    }

    if (reportedUrls.has(url)) return;
    reportedUrls.add(url);

    try {
      chrome.runtime.sendMessage({
        type: "mediaFound",
        url,
        mime,
        title: item.title,
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
    const title = el.getAttribute("title") || el.getAttribute("data-title") || cleanVideoTitle(document.title) || "";

    // 1. Direct src attribute or currentSrc
    const src = el.currentSrc || el.src;
    if (src && /^https?:/i.test(src) && !src.startsWith("blob:")) {
      if (!/\.(html?|php|jsp|asp)(\?|#|$)/i.test(src) || /(?:m3u8|mpd)/i.test(src)) {
        const isStream = /mpegurl|dash|hls/i.test(el.type || "") || isStreamManifestUrl(src);
        reportMedia(src, el.type || (isVideo ? "video/mp4" : "audio/mpeg"), title, isStream, el);
      }
    }

    // 2. <source> child elements
    const sources = el.querySelectorAll("source");
    for (const s of sources) {
      const sSrc = s.src || s.getAttribute("src");
      const sType = s.type || s.getAttribute("type") || "";
      if (sSrc && /^https?:/i.test(sSrc) && !sSrc.startsWith("blob:")) {
        if (!/\.(html?|php|jsp|asp)(\?|#|$)/i.test(sSrc) || /(?:m3u8|mpd)/i.test(sSrc)) {
          const isStream = /mpegurl|dash|hls/i.test(sType) || isStreamManifestUrl(sSrc);
          reportMedia(sSrc, sType || (isVideo ? "video/mp4" : "audio/mpeg"), title, isStream, el);
        }
      }
    }

    if (isVideo && typeof overlayManager !== "undefined" && overlayManager) {
      overlayManager.register(el);
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
          const title = data.title || cleanVideoTitle(document.title) || "Video";
          const vid = document.querySelector("video");
          const media = { url: streamUrl, mime: "application/x-mpegURL", title, isStream: true };
          reportMedia(streamUrl, "application/x-mpegURL", title, true, vid);
          if (vid) {
            vid._spoolMedia = media;
            if (typeof overlayManager !== "undefined" && overlayManager) {
              overlayManager.register(vid);
              const overlay = overlayManager.overlays.get(vid);
              if (overlay) overlay.setMedia(media);
            }
          }
        }
      })
      .catch(() => {});
  }

  // ---------------------------------------------------------------------------
  // Per-Video Overlay Component
  // ---------------------------------------------------------------------------
  const OVERLAY_STYLE = `
    :host {
      all: initial !important;
      position: fixed !important;
      z-index: 2147483647 !important;
      pointer-events: none !important;
      display: block !important;
    }
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      height: 28px;
      padding: 0 8px 0 6px;
      font: 500 12px/1.2 Inter, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
      color: #EDEDEF;
      background: rgba(11, 11, 15, 0.90);
      border: 1px solid rgba(255, 255, 255, 0.12);
      border-radius: 8px;
      box-shadow: 0 2px 10px rgba(0, 0, 0, 0.45),
                  0 0 20px rgba(94, 106, 210, 0.2);
      backdrop-filter: blur(12px) saturate(1.4);
      -webkit-backdrop-filter: blur(12px) saturate(1.4);
      cursor: pointer;
      user-select: none;
      pointer-events: auto;
      transform: translateX(-100%);
      opacity: 0;
      transition: opacity .25s cubic-bezier(.16, 1, .3, 1),
                  transform .2s cubic-bezier(.16, 1, .3, 1),
                  background .15s ease,
                  border-color .15s ease,
                  box-shadow .2s ease;
      white-space: nowrap;
    }
    .badge.visible {
      opacity: 1;
    }
    .badge:hover {
      background: rgba(18, 18, 24, 0.96);
      border-color: rgba(139, 92, 246, 0.45);
      box-shadow: 0 4px 16px rgba(0, 0, 0, 0.55),
                  0 0 28px rgba(94, 106, 210, 0.45);
    }
    .badge:active {
      transform: translateX(-100%) scale(0.97);
    }
    .mark {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      flex-shrink: 0;
      width: 18px;
      height: 18px;
      border-radius: 5px;
      background: linear-gradient(135deg, #5E6AD2, #8B5CF6);
      box-shadow: 0 2px 6px rgba(94, 106, 210, 0.4),
                  inset 0 1px 0 0 rgba(255, 255, 255, 0.2);
    }
    .label {
      font-weight: 550;
      letter-spacing: -0.01em;
      color: #EDEDEF;
    }
    .close {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      flex-shrink: 0;
      width: 16px;
      height: 16px;
      margin-left: 2px;
      border-radius: 4px;
      font-size: 10px;
      color: #8A8F98;
      cursor: pointer;
      transition: color .15s ease, background .15s ease;
    }
    .close:hover {
      color: #EDEDEF;
      background: rgba(255, 255, 255, 0.12);
    }
    @media (prefers-reduced-motion: reduce) {
      .badge { transition: none !important; }
      .badge:active { transform: translateX(-100%) !important; }
    }
  `;

  class VideoOverlay {
    constructor(video) {
      this.video = video;
      this.dismissed = false;
      this.isHovered = false;
      this.isSending = false;
      this.hideTimer = null;
      this.mediaInfo = null;

      // Bound handlers for cleanup
      this._onMouseEnter = () => this.onMouseEnter();
      this._onMouseLeave = () => this.onMouseLeave();
      this._onPlay = () => this.peek(3500);
      this._onUpdatePos = () => this.updatePosition();

      this.build();
      this.attachListeners();
      this.updatePosition();
    }

    build() {
      this.host = document.createElement("div");
      this.host.className = "spool-video-overlay-host";

      const shadow = this.host.attachShadow({ mode: "closed" });
      const style = document.createElement("style");
      style.textContent = OVERLAY_STYLE;

      this.badge = document.createElement("div");
      this.badge.className = "badge";
      this.badge.setAttribute("role", "button");
      this.badge.setAttribute("tabindex", "0");
      this.badge.setAttribute("aria-label", "Download video with spool");
      this.badge.innerHTML =
        `<span class="mark">${GLYPH}</span>` +
        `<span class="label">Download</span>` +
        `<span class="close" role="button" title="Dismiss" aria-label="Dismiss">✕</span>`;

      this.label = this.badge.querySelector(".label");
      this.closeBtn = this.badge.querySelector(".close");

      // Badge mouse interaction
      this.badge.addEventListener("mouseenter", () => {
        this.isHovered = true;
        this.clearHideTimer();
        this.show();
      });

      this.badge.addEventListener("mouseleave", () => {
        this.isHovered = false;
        this.scheduleHide(1500);
      });

      this.badge.addEventListener("click", (e) => {
        e.preventDefault();
        e.stopPropagation();
        if (e.stopImmediatePropagation) e.stopImmediatePropagation();
        void this.onDownload();
      });

      this.badge.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          e.stopPropagation();
          this.badge.click();
        }
      });

      this.closeBtn?.addEventListener("click", (e) => {
        e.preventDefault();
        e.stopPropagation();
        if (e.stopImmediatePropagation) e.stopImmediatePropagation();
        this.dismiss();
      });

      shadow.append(style, this.badge);

      const targetParent = document.fullscreenElement || document.documentElement || document.body;
      if (targetParent) {
        targetParent.appendChild(this.host);
      }
    }

    attachListeners() {
      // Direct video listeners
      this.video.addEventListener("mouseenter", this._onMouseEnter, { passive: true });
      this.video.addEventListener("mouseleave", this._onMouseLeave, { passive: true });
      this.video.addEventListener("play", this._onPlay, { passive: true });
      this.video.addEventListener("playing", this._onPlay, { passive: true });
      this.video.addEventListener("loadedmetadata", this._onUpdatePos, { passive: true });
      this.video.addEventListener("canplay", this._onUpdatePos, { passive: true });

      // Player container listeners (e.g. YouTube, Video.js, JWPlayer wrappers)
      const container = this.video.closest(
        ".html5-video-player, .video-js, .jwplayer, [data-player], [class*='player'], [id*='player']"
      ) || this.video.parentElement;

      if (container && container !== document.body && container !== document.documentElement) {
        this.container = container;
        this.container.addEventListener("mouseenter", this._onMouseEnter, { passive: true });
        this.container.addEventListener("mouseleave", this._onMouseLeave, { passive: true });
      }

      // Track size changes
      if (typeof ResizeObserver !== "undefined") {
        this.resizeObserver = new ResizeObserver(() => this.updatePosition());
        this.resizeObserver.observe(this.video);
      }
    }

    setMedia(info) {
      if (!info) return;
      this.mediaInfo = info;
    }

    resolveUrl() {
      // 1. Explicit media detected on element
      if (this.mediaInfo?.url) return this.mediaInfo;
      const mapped = mediaInfoMap.get(this.video);
      if (mapped?.url) return mapped;
      if (this.video._spoolMedia?.url) return this.video._spoolMedia;

      // 2. Direct currentSrc (if http/https and not blob:)
      const curSrc = this.video.currentSrc || "";
      if (curSrc && /^https?:/i.test(curSrc) && !curSrc.startsWith("blob:")) {
        return {
          url: curSrc,
          isStream: isStreamManifestUrl(curSrc),
          title: cleanVideoTitle(document.title) || "Video",
        };
      }

      // 3. Child <source> elements
      const sources = this.video.querySelectorAll("source");
      for (const s of sources) {
        const sSrc = s.src || s.getAttribute("src");
        const sType = s.type || s.getAttribute("type") || "";
        if (sSrc && /^https?:/i.test(sSrc) && !sSrc.startsWith("blob:")) {
          return {
            url: sSrc,
            isStream: isStreamManifestUrl(sSrc) || /mpegurl|dash|hls/i.test(sType),
            title: cleanVideoTitle(document.title) || "Video",
          };
        }
      }

      // 4. video.src attribute
      const srcAttr = this.video.src || this.video.getAttribute("src") || "";
      if (srcAttr && /^https?:/i.test(srcAttr) && !srcAttr.startsWith("blob:")) {
        return {
          url: srcAttr,
          isStream: isStreamManifestUrl(srcAttr),
          title: cleanVideoTitle(document.title) || "Video",
        };
      }

      // 5. Last detected stream manifest in this frame
      if (lastReportedMedia?.url) {
        return lastReportedMedia;
      }

      // 6. Page URL fallback for video sites or embedded player frames
      if (window !== window.top || isKnownVideoHost(location.hostname)) {
        return {
          url: location.href,
          isStream: true,
          title: cleanVideoTitle(document.title) || "Video",
        };
      }

      return {
        url: location.href,
        isStream: true,
        title: cleanVideoTitle(document.title) || "Video",
      };
    }

    async onDownload() {
      if (this.isSending) return;
      this.isSending = true;

      const media = this.resolveUrl();
      if (!media?.url) {
        this.label.textContent = "No video source";
        this.show();
        this.scheduleHide(2500);
        this.isSending = false;
        return;
      }

      this.label.textContent = "Sending…";
      this.show();

      try {
        const res = await chrome.runtime.sendMessage({
          type: "download",
          url: media.url,
          referer: location.href,
          video: media.isStream !== undefined ? media.isStream : true,
        });
        this.label.textContent = res?.ok ? "Sent to spool ✓" : "spool not running";
      } catch {
        this.label.textContent = "spool not running";
      }

      this.isSending = false;
      this.scheduleHide(2500, () => {
        this.label.textContent = "Download";
      });
    }

    updatePosition() {
      if (this.dismissed || !this.video.isConnected) {
        if (this.host) this.host.style.display = "none";
        return;
      }

      const rect = this.video.getBoundingClientRect();

      // Filter out tiny thumbnails, audio-only players, or unrendered videos
      if (rect.width < 160 || rect.height < 90) {
        this.host.style.display = "none";
        return;
      }

      // Viewport clipping check
      const inViewport = (
        rect.bottom > 40 &&
        rect.top < window.innerHeight - 10 &&
        rect.right > 40 &&
        rect.left < window.innerWidth - 10
      );

      if (!inViewport) {
        this.host.style.display = "none";
        return;
      }

      // Computed style check
      const style = window.getComputedStyle(this.video);
      if (style.display === "none" || style.visibility === "hidden" || parseFloat(style.opacity || "1") < 0.1) {
        this.host.style.display = "none";
        return;
      }

      // Pin inside video boundaries & viewport
      const pinnedTop = Math.max(Math.round(rect.top + 8), 8);
      if (pinnedTop > rect.bottom - 32) {
        this.host.style.display = "none";
        return;
      }

      const pinnedRight = Math.min(Math.round(rect.right - 8), window.innerWidth - 8);

      this.host.style.display = "block";
      this.host.style.top = `${pinnedTop}px`;
      this.host.style.left = `${pinnedRight}px`;
    }

    onFullscreen(fsEl) {
      if (fsEl && typeof fsEl.contains === "function" && fsEl.contains(this.video)) {
        if (fsEl !== this.video && this.host.parentElement !== fsEl) {
          fsEl.appendChild(this.host);
        } else if (fsEl === this.video && fsEl.parentElement && this.host.parentElement !== fsEl.parentElement) {
          fsEl.parentElement.appendChild(this.host);
        }
      } else {
        const root = document.fullscreenElement || document.documentElement || document.body;
        if (root && this.host.parentElement !== root) {
          root.appendChild(this.host);
        }
      }
      this.updatePosition();
    }

    onMouseEnter() {
      this.isHovered = true;
      this.clearHideTimer();
      this.show();
    }

    onMouseLeave() {
      this.isHovered = false;
      this.scheduleHide(1500);
    }

    show() {
      if (this.dismissed) return;
      this.updatePosition();
      this.badge.classList.add("visible");
    }

    hide() {
      this.clearHideTimer();
      this.badge.classList.remove("visible");
    }

    peek(delay = 3500) {
      if (this.dismissed) return;
      this.show();
      this.scheduleHide(delay);
    }

    scheduleHide(delay = 1500, onHidden = null) {
      this.clearHideTimer();
      this.hideTimer = setTimeout(() => {
        if (!this.isHovered && !this.isSending) {
          this.badge.classList.remove("visible");
          if (onHidden) onHidden();
        }
      }, delay);
    }

    clearHideTimer() {
      if (this.hideTimer) {
        clearTimeout(this.hideTimer);
        this.hideTimer = null;
      }
    }

    dismiss() {
      this.dismissed = true;
      this.hide();
      this.destroy();
    }

    destroy() {
      this.clearHideTimer();
      if (this.resizeObserver) {
        this.resizeObserver.disconnect();
      }
      this.video.removeEventListener("mouseenter", this._onMouseEnter);
      this.video.removeEventListener("mouseleave", this._onMouseLeave);
      this.video.removeEventListener("play", this._onPlay);
      this.video.removeEventListener("playing", this._onPlay);
      this.video.removeEventListener("loadedmetadata", this._onUpdatePos);
      this.video.removeEventListener("canplay", this._onUpdatePos);

      if (this.container) {
        this.container.removeEventListener("mouseenter", this._onMouseEnter);
        this.container.removeEventListener("mouseleave", this._onMouseLeave);
      }

      this.host?.remove();
    }
  }

  // ---------------------------------------------------------------------------
  // Overlay Manager
  // ---------------------------------------------------------------------------
  class VideoOverlayManager {
    constructor() {
      this.overlays = new Map();
      this.rafId = null;
      this.lastMoveCheck = 0;
      this.init();
    }

    init() {
      const throttledUpdate = () => this.scheduleUpdate();
      window.addEventListener("scroll", throttledUpdate, { passive: true, capture: true });
      window.addEventListener("resize", throttledUpdate, { passive: true });

      const handleFs = () => this.onFullscreenChange();
      document.addEventListener("fullscreenchange", handleFs, { passive: true });
      document.addEventListener("webkitfullscreenchange", handleFs, { passive: true });

      // Document-level throttled mouse hit testing (handles transparent overlay divs)
      document.addEventListener("mousemove", (e) => {
        const now = Date.now();
        if (now - this.lastMoveCheck < 80) return;
        this.lastMoveCheck = now;

        const x = e.clientX;
        const y = e.clientY;

        for (const [video, overlay] of this.overlays) {
          if (overlay.dismissed || !video.isConnected) continue;
          const rect = video.getBoundingClientRect();
          if (rect.width >= 160 && rect.height >= 90 &&
              x >= rect.left && x <= rect.right &&
              y >= rect.top && y <= rect.bottom) {
            overlay.onMouseEnter();
          }
        }
      }, { passive: true });
    }

    register(video) {
      if (!video || video.tagName?.toLowerCase() !== "video") return;
      if (this.overlays.has(video)) {
        this.overlays.get(video).updatePosition();
        return;
      }
      const overlay = new VideoOverlay(video);
      this.overlays.set(video, overlay);
    }

    unregister(video) {
      const overlay = this.overlays.get(video);
      if (overlay) {
        overlay.destroy();
        this.overlays.delete(video);
      }
    }

    scan() {
      document.querySelectorAll("video").forEach((v) => this.register(v));
      for (const [video] of this.overlays) {
        if (!video.isConnected) {
          this.unregister(video);
        }
      }
    }

    scheduleUpdate() {
      if (this.rafId) return;
      this.rafId = window.requestAnimationFrame(() => {
        this.rafId = null;
        for (const overlay of this.overlays.values()) {
          overlay.updatePosition();
        }
      });
    }

    onFullscreenChange() {
      const fsEl = document.fullscreenElement || document.webkitFullscreenElement;
      for (const overlay of this.overlays.values()) {
        overlay.onFullscreen(fsEl);
      }
      this.scheduleUpdate();
    }

    findBestOverlay() {
      for (const [video, overlay] of this.overlays) {
        if (!overlay.dismissed && video.isConnected && !video.paused && video.currentTime > 0) {
          return overlay;
        }
      }
      for (const [video, overlay] of this.overlays) {
        if (!overlay.dismissed && video.isConnected) {
          const rect = video.getBoundingClientRect();
          if (rect.width >= 160 && rect.height >= 90) return overlay;
        }
      }
      return null;
    }

    hasVisibleVideos() {
      for (const [video, overlay] of this.overlays) {
        if (!overlay.dismissed && video.isConnected) {
          const rect = video.getBoundingClientRect();
          if (rect.width >= 160 && rect.height >= 90) return true;
        }
      }
      return false;
    }
  }

  const overlayManager = new VideoOverlayManager();

  // ---------------------------------------------------------------------------
  // Fallback Global Download Panel (Top frame only, when no video element exists)
  // ---------------------------------------------------------------------------
  let fallbackPanel = null;
  let fallbackHideTimer = null;

  const FALLBACK_HOST_STYLE = [
    "all:initial!important",
    "position:fixed!important",
    "z-index:2147483647!important",
    "top:16px!important",
    "right:16px!important",
    "display:block!important",
  ].join(";");

  const FALLBACK_PANEL_STYLE = `
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

  function buildFallbackPanel(label) {
    const host = document.createElement("div");
    host.id = "spool-panel";
    host.style.cssText = FALLBACK_HOST_STYLE;

    const root = host.attachShadow({ mode: "closed" });
    const style = document.createElement("style");
    style.textContent = FALLBACK_PANEL_STYLE;

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
      removeFallbackPanel();
      window.__spoolDismissed = true;
    });

    root.append(style, el);
    return { host, el, text };
  }

  function removeFallbackPanel() {
    clearTimeout(fallbackHideTimer);
    fallbackPanel?.host.remove();
    fallbackPanel = null;
  }

  function showFallbackPanel(label, onClick) {
    if (window !== window.top || window.__spoolDismissed) return;
    if (overlayManager.hasVisibleVideos()) return;

    if (!fallbackPanel) {
      fallbackPanel = buildFallbackPanel(label);
      fallbackPanel.el.addEventListener("click", async () => {
        fallbackPanel.text.textContent = "Sending…";
        const ok = await onClick();
        fallbackPanel.text.textContent = ok ? "Sent to spool ✓" : "spool not running";
        fallbackHideTimer = setTimeout(removeFallbackPanel, 2500);
      });
      document.documentElement.appendChild(fallbackPanel.host);
      window.requestAnimationFrame(() => fallbackPanel?.el.classList.add("in"));
    } else {
      fallbackPanel.text.textContent = label;
    }
  }

  // ---------------------------------------------------------------------------
  // Message Listener
  // ---------------------------------------------------------------------------
  chrome.runtime.onMessage.addListener((msg, _s, sendResponse) => {
    if (msg.type === "showPanel") {
      const best = overlayManager.findBestOverlay();
      if (best) {
        best.setMedia({
          url: msg.url,
          title: msg.title || msg.label,
          isStream: msg.video !== undefined ? msg.video : true,
        });
        best.peek();
        sendResponse({ shown: true });
        return true;
      }

      if (window === window.top) {
        showFallbackPanel(msg.label || "Download this video", async () => {
          const res = await chrome.runtime.sendMessage({
            type: "download",
            url: msg.url,
            referer: msg.referer || location.href,
            video: msg.video !== undefined ? msg.video : true,
          });
          return !!res?.ok;
        });
        sendResponse({ shown: true });
        return true;
      }
    } else if (msg.type === "hidePanel") {
      removeFallbackPanel();
      sendResponse({ shown: false });
    }
    return true;
  });

  // ---------------------------------------------------------------------------
  // Observers & Initial Scan
  // ---------------------------------------------------------------------------
  function scanMedia() {
    document.querySelectorAll("video, audio").forEach(inspectMedia);
    overlayManager.scan();
    resolveStreamvin();
  }

  scanMedia();

  const observer = new MutationObserver((mutations) => {
    let hasMediaChanges = false;
    for (const m of mutations) {
      for (const node of m.addedNodes) {
        if (node.nodeType !== 1) continue;
        const tag = node.tagName?.toLowerCase();
        if (tag === "video" || tag === "audio") {
          inspectMedia(node);
          hasMediaChanges = true;
        } else if (tag === "source" && node.parentElement?.tagName?.toLowerCase() === "video") {
          inspectMedia(node.parentElement);
          hasMediaChanges = true;
        } else if (node.querySelectorAll) {
          node.querySelectorAll("video, audio").forEach(inspectMedia);
          hasMediaChanges = true;
        }
      }
      for (const node of m.removedNodes) {
        if (node.nodeType !== 1) continue;
        const tag = node.tagName?.toLowerCase();
        if (tag === "video") {
          overlayManager.unregister(node);
        } else if (node.querySelectorAll) {
          node.querySelectorAll("video").forEach((v) => overlayManager.unregister(v));
        }
      }
      if (m.type === "attributes" && (m.attributeName === "src" || m.attributeName === "currentSrc")) {
        const tag = m.target.tagName?.toLowerCase();
        if (tag === "video" || tag === "audio") {
          inspectMedia(m.target);
          hasMediaChanges = true;
        }
      }
    }
    if (hasMediaChanges) {
      overlayManager.scheduleUpdate();
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

  ["play", "playing", "loadedmetadata", "canplay", "loadstart"].forEach((ev) => {
    document.addEventListener(ev, (e) => {
      if (e.target && (e.target.tagName === "VIDEO" || e.target.tagName === "AUDIO")) {
        inspectMedia(e.target);
        if (e.target.tagName === "VIDEO") {
          overlayManager.register(e.target);
          if (ev === "play" || ev === "playing") {
            const overlay = overlayManager.overlays.get(e.target);
            if (overlay) overlay.peek();
          }
        }
      }
    }, true);
  });
})();
