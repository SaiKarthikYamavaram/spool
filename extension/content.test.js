import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { describe, expect, it, vi } from "vitest";

const HERE = dirname(fileURLToPath(import.meta.url));
const CONTENT_SOURCE = readFileSync(join(HERE, "content.js"), "utf8");

class MockElement {
  constructor(tagName = "div") {
    this.tagName = tagName.toUpperCase();
    this.children = [];
    this.parentElement = null;
    this.attributes = new Map();
    this.style = {};
    const classes = new Set();
    this.classList = {
      add: (c) => classes.add(c),
      remove: (c) => classes.delete(c),
      contains: (c) => classes.has(c),
      has: (c) => classes.has(c),
    };
    this.listeners = new Map();
    this.isConnected = true;
    this.rect = { top: 100, bottom: 460, left: 50, right: 690, width: 640, height: 360 };
    this._innerHTML = "";
  }

  get innerHTML() {
    return this._innerHTML;
  }

  set innerHTML(html) {
    this._innerHTML = html;
    this.children = [];
    const tagRegex = /<([a-z0-9]+)([^>]*)>(.*?)<\/\1>/gis;
    let match;
    while ((match = tagRegex.exec(html)) !== null) {
      const tag = match[1];
      const attrsStr = match[2];
      const text = match[3];
      const child = new MockElement(tag);
      const classMatch = attrsStr.match(/class=["']([^"']+)["']/i);
      if (classMatch) {
        child.className = classMatch[1];
        classMatch[1].split(/\s+/).forEach((c) => child.classList.add(c));
      }
      child.textContent = text;
      child.parentElement = this;
      this.children.push(child);
    }
  }

  getAttribute(name) {
    return this.attributes.get(name) || null;
  }

  setAttribute(name, val) {
    this.attributes.set(name, String(val));
  }

  removeAttribute(name) {
    this.attributes.delete(name);
  }

  getBoundingClientRect() {
    return this.rect;
  }

  addEventListener(ev, fn) {
    if (!this.listeners.has(ev)) this.listeners.set(ev, []);
    this.listeners.get(ev).push(fn);
  }

  removeEventListener(ev, fn) {
    if (!this.listeners.has(ev)) return;
    this.listeners.set(ev, this.listeners.get(ev).filter((cb) => cb !== fn));
  }

  dispatchEvent(event) {
    const handlers = this.listeners.get(event.type) || [];
    for (const h of handlers) {
      h(event);
    }
  }

  appendChild(child) {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  remove() {
    if (this.parentElement) {
      this.parentElement.children = this.parentElement.children.filter((c) => c !== this);
      this.parentElement = null;
    }
  }

  attachShadow() {
    this.shadowRoot = new MockElement("shadow-root");
    this.shadowRoot.host = this;
    this.shadowRoot.append = (...nodes) => {
      for (const n of nodes) {
        this.shadowRoot.appendChild(n);
      }
    };
    return this.shadowRoot;
  }

  querySelector(selector) {
    const all = this.querySelectorAll(selector);
    return all[0] || null;
  }

  querySelectorAll(selector) {
    const results = [];
    const check = (node) => {
      if (matches(node, selector)) results.push(node);
      for (const child of node.children) {
        check(child);
      }
      if (node.shadowRoot) {
        for (const child of node.shadowRoot.children) {
          check(child);
        }
      }
    };
    for (const child of this.children) {
      check(child);
    }
    if (this.shadowRoot) {
      for (const child of this.shadowRoot.children) {
        check(child);
      }
    }
    return results;
  }

  closest(selector) {
    let cur = this;
    while (cur) {
      if (matches(cur, selector)) return cur;
      cur = cur.parentElement;
    }
    return null;
  }

  contains(node) {
    let cur = node;
    while (cur) {
      if (cur === this) return true;
      cur = cur.parentElement;
    }
    return false;
  }
}

function matches(node, selector) {
  if (!selector) return false;
  if (selector.includes(",")) {
    return selector.split(",").some((part) => matches(node, part.trim()));
  }
  if (selector.startsWith(".")) {
    const cls = selector.slice(1);
    return node.className?.split(" ").includes(cls) || node.classList.has(cls);
  }
  if (selector.startsWith("#")) {
    return node.id === selector.slice(1);
  }
  return node.tagName?.toLowerCase() === selector.toLowerCase();
}

function setupTestEnvironment({ isTop = true } = {}) {
  const docRoot = new MockElement("html");
  const docBody = new MockElement("body");
  docRoot.appendChild(docBody);

  const listeners = new Map();
  const sentMessages = [];
  const messageListeners = [];
  const activeTimers = [];

  const dom = {
    document: {
      documentElement: docRoot,
      body: docBody,
      title: "Test Page",
      createElement: (tag) => new MockElement(tag),
      addEventListener: (ev, fn) => {
        if (!listeners.has(ev)) listeners.set(ev, []);
        listeners.get(ev).push(fn);
      },
      removeEventListener: (ev, fn) => {
        if (!listeners.has(ev)) return;
        listeners.set(ev, listeners.get(ev).filter((cb) => cb !== fn));
      },
      querySelectorAll: (sel) => docRoot.querySelectorAll(sel),
      querySelector: (sel) => docRoot.querySelector(sel),
    },
    window: {
      getComputedStyle: () => ({ display: "block", visibility: "visible", opacity: "1" }),
      addEventListener: (ev, fn) => {
        if (!listeners.has(ev)) listeners.set(ev, []);
        listeners.get(ev).push(fn);
      },
      removeEventListener: () => {},
      innerWidth: 1920,
      innerHeight: 1080,
      requestAnimationFrame: (cb) => { cb(); return 1; },
    },
    location: {
      href: "https://example.com/watch",
      hostname: "example.com",
    },
    chrome: {
      runtime: {
        sendMessage: vi.fn(async (msg) => {
          sentMessages.push(msg);
          if (msg.type === "download") return { ok: true };
          return { ok: true };
        }),
        onMessage: {
          addListener: (fn) => messageListeners.push(fn),
        },
      },
    },
    MutationObserver: class {
      observe() {}
      disconnect() {}
    },
    ResizeObserver: class {
      observe() {}
      disconnect() {}
    },
    Set,
    WeakMap,
    Map,
    setTimeout: (cb, ms = 0) => {
      const id = activeTimers.length + 1;
      activeTimers.push({ id, cb, ms });
      return id;
    },
    clearTimeout: (id) => {
      const idx = activeTimers.findIndex((t) => t.id === id);
      if (idx !== -1) activeTimers.splice(idx, 1);
    },
    Date,
    console,
  };

  dom.window.window = dom.window;
  dom.window.top = isTop ? dom.window : {};
  dom.window.document = dom.document;
  dom.requestAnimationFrame = dom.window.requestAnimationFrame;

  const runAllTimers = () => {
    while (activeTimers.length > 0) {
      const { cb } = activeTimers.shift();
      cb();
    }
  };

  return { dom, sentMessages, messageListeners, listeners, docRoot, docBody, activeTimers, runAllTimers };
}

describe("content.js per-video overlay", () => {
  it("creates independent overlays for multiple videos on a page", () => {
    const { dom, docBody, sentMessages } = setupTestEnvironment();

    const video1 = new MockElement("video");
    video1.src = "https://example.com/video1.mp4";
    video1.rect = { top: 100, bottom: 460, left: 50, right: 690, width: 640, height: 360 };

    const video2 = new MockElement("video");
    video2.src = "https://example.com/video2.mp4";
    video2.rect = { top: 600, bottom: 960, left: 50, right: 690, width: 640, height: 360 };

    docBody.appendChild(video1);
    docBody.appendChild(video2);

    const context = vm.createContext(dom);
    vm.runInContext(CONTENT_SOURCE, context);

    // Should find 2 overlay hosts created
    const hosts = dom.document.querySelectorAll(".spool-video-overlay-host");
    expect(hosts.length).toBe(2);

    // Media found messages should be sent for both
    const mediaMsgs = sentMessages.filter((m) => m.type === "mediaFound");
    expect(mediaMsgs.map((m) => m.url)).toEqual([
      "https://example.com/video1.mp4",
      "https://example.com/video2.mp4",
    ]);

    // Position of host 1: top 108, left 682 (right - 8)
    expect(hosts[0].style.top).toBe("108px");
    expect(hosts[0].style.left).toBe("682px");

    // Position of host 2: top 608, left 682
    expect(hosts[1].style.top).toBe("608px");
    expect(hosts[1].style.left).toBe("682px");
  });

  it("does not display overlays for tiny/unrendered video elements", () => {
    const { dom, docBody } = setupTestEnvironment();

    const tinyVideo = new MockElement("video");
    tinyVideo.src = "https://example.com/tiny.mp4";
    tinyVideo.rect = { top: 10, bottom: 40, left: 10, right: 40, width: 30, height: 30 };
    docBody.appendChild(tinyVideo);

    const context = vm.createContext(dom);
    vm.runInContext(CONTENT_SOURCE, context);

    const hosts = dom.document.querySelectorAll(".spool-video-overlay-host");
    expect(hosts.length).toBe(1);
    expect(hosts[0].style.display).toBe("none");
  });

  it("clicking an overlay button downloads that specific video without bubbling", async () => {
    const { dom, docBody, sentMessages } = setupTestEnvironment();

    const video1 = new MockElement("video");
    video1.src = "https://example.com/video1.mp4";
    video1.rect = { top: 100, bottom: 460, left: 50, right: 690, width: 640, height: 360 };
    docBody.appendChild(video1);

    const video2 = new MockElement("video");
    video2.src = "https://example.com/video2.mp4";
    video2.rect = { top: 500, bottom: 860, left: 50, right: 690, width: 640, height: 360 };
    docBody.appendChild(video2);

    const context = vm.createContext(dom);
    vm.runInContext(CONTENT_SOURCE, context);

    const hosts = dom.document.querySelectorAll(".spool-video-overlay-host");
    const badge2 = hosts[1].shadowRoot.querySelector(".badge");
    expect(badge2).toBeDefined();

    let propagationStopped = false;
    let defaultPrevented = false;
    const fakeEvent = {
      type: "click",
      preventDefault: () => { defaultPrevented = true; },
      stopPropagation: () => { propagationStopped = true; },
      stopImmediatePropagation: () => { propagationStopped = true; },
    };

    badge2.dispatchEvent(fakeEvent);

    expect(propagationStopped).toBe(true);
    expect(defaultPrevented).toBe(true);

    const downloadMsgs = sentMessages.filter((m) => m.type === "download");
    expect(downloadMsgs.length).toBe(1);
    expect(downloadMsgs[0].url).toBe("https://example.com/video2.mp4");
  });

  it("works inside player iframes directly attaching to the iframe's video", () => {
    const { dom, docBody } = setupTestEnvironment({ isTop: false });

    const iframeVideo = new MockElement("video");
    iframeVideo.src = "https://cdn.test/stream.m3u8";
    iframeVideo.rect = { top: 0, bottom: 720, left: 0, right: 1280, width: 1280, height: 720 };
    docBody.appendChild(iframeVideo);

    const context = vm.createContext(dom);
    vm.runInContext(CONTENT_SOURCE, context);

    // Overlay is attached inside iframe document
    const hosts = dom.document.querySelectorAll(".spool-video-overlay-host");
    expect(hosts.length).toBe(1);
    expect(hosts[0].style.top).toBe("8px");
    expect(hosts[0].style.left).toBe("1272px");
  });

  it("peeks on play event and sets visible class", () => {
    const { dom, docBody, runAllTimers } = setupTestEnvironment();

    const video = new MockElement("video");
    video.src = "https://example.com/movie.mp4";
    video.rect = { top: 100, bottom: 460, left: 50, right: 690, width: 640, height: 360 };
    docBody.appendChild(video);

    const context = vm.createContext(dom);
    vm.runInContext(CONTENT_SOURCE, context);

    const host = dom.document.querySelector(".spool-video-overlay-host");
    const badge = host.shadowRoot.querySelector(".badge");

    expect(badge.classList.contains("visible")).toBe(false);

    // Trigger play -> peeks
    video.dispatchEvent({ type: "play" });
    expect(badge.classList.contains("visible")).toBe(true);

    // After peek timer runs -> hides
    runAllTimers();
    expect(badge.classList.contains("visible")).toBe(false);
  });

  it("dismissing an overlay hides and removes host", () => {
    const { dom, docBody } = setupTestEnvironment();

    const video = new MockElement("video");
    video.src = "https://example.com/movie.mp4";
    video.rect = { top: 100, bottom: 460, left: 50, right: 690, width: 640, height: 360 };
    docBody.appendChild(video);

    const context = vm.createContext(dom);
    vm.runInContext(CONTENT_SOURCE, context);

    const host = dom.document.querySelector(".spool-video-overlay-host");
    const closeBtn = host.shadowRoot.querySelector(".close");
    expect(closeBtn).toBeDefined();

    closeBtn.dispatchEvent({
      type: "click",
      preventDefault: () => {},
      stopPropagation: () => {},
      stopImmediatePropagation: () => {},
    });

    // After dismiss, host is removed from DOM
    const remaining = dom.document.querySelectorAll(".spool-video-overlay-host");
    expect(remaining.length).toBe(0);
  });

  it("reparents to fullscreenElement and restores on exit", () => {
    const { dom, docBody, listeners } = setupTestEnvironment();

    const playerContainer = new MockElement("div");
    playerContainer.className = "html5-video-player";
    const video = new MockElement("video");
    video.src = "https://example.com/movie.mp4";
    video.rect = { top: 100, bottom: 460, left: 50, right: 690, width: 640, height: 360 };
    playerContainer.appendChild(video);
    docBody.appendChild(playerContainer);

    const context = vm.createContext(dom);
    vm.runInContext(CONTENT_SOURCE, context);

    const host = dom.document.querySelector(".spool-video-overlay-host");
    expect(host.parentElement).toBe(dom.document.documentElement);

    // Enter fullscreen on playerContainer
    dom.document.fullscreenElement = playerContainer;
    const fsHandlers = listeners.get("fullscreenchange") || [];
    for (const h of fsHandlers) h();

    expect(host.parentElement).toBe(playerContainer);

    // Exit fullscreen
    dom.document.fullscreenElement = null;
    for (const h of fsHandlers) h();

    expect(host.parentElement).toBe(dom.document.documentElement);
  });

  it("hides overlay when video is scrolled outside viewport", () => {
    const { dom, docBody } = setupTestEnvironment();

    const video = new MockElement("video");
    video.src = "https://example.com/movie.mp4";
    // Scrolled far above viewport
    video.rect = { top: -800, bottom: -440, left: 50, right: 690, width: 640, height: 360 };
    docBody.appendChild(video);

    const context = vm.createContext(dom);
    vm.runInContext(CONTENT_SOURCE, context);

    const host = dom.document.querySelector(".spool-video-overlay-host");
    expect(host.style.display).toBe("none");
  });
});
