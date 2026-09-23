import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import { beforeEach, describe, expect, it, vi } from "vitest";

const HERE = dirname(fileURLToPath(import.meta.url));
const SOURCE = readFileSync(join(HERE, "common.js"), "utf8");

/// The extension is a classic script, not a module: `common.js` defines bare
/// globals that `background.js` and the pages pick up via `importScripts` and
/// `<script>`. Evaluate the real shipped file in a sandbox with the browser
/// APIs stubbed, so these tests exercise what actually ships rather than a
/// copy.
/// `native` stands in for `chrome.runtime.sendNativeMessage`, kept apart from
/// `chrome` so a test can swap the storage or cookies stubs without it.
function load({ native = async () => ({ ok: true }), ...overrides } = {}) {
  const sandbox = {
    navigator: { userAgent: "TestAgent/1.0" },
    chrome: {
      storage: { local: { get: async () => ({}), set: async () => {} } },
      cookies: { getAll: async () => [] },
    },
    console,
    URL,
    ...overrides,
  };
  sandbox.chrome = { ...sandbox.chrome, runtime: { sendNativeMessage: native } };
  vm.createContext(sandbox);
  vm.runInContext(SOURCE, sandbox);
  return sandbox;
}

let ext;
beforeEach(() => {
  ext = load();
});

describe("classify", () => {
  it("prefers the Content-Type when the server sends one", () => {
    expect(ext.classify("video/mp4", "download")).toBe("video");
    expect(ext.classify("audio/mpeg", "x")).toBe("audio");
    expect(ext.classify("image/png", "x")).toBe("image");
    expect(ext.classify("application/pdf", "x")).toBe("document");
    expect(ext.classify("application/zip", "x")).toBe("archive");
  });

  it("ignores Content-Type parameters", () => {
    expect(ext.classify("video/mp4; codecs=avc1", "x")).toBe("video");
    expect(ext.classify(" VIDEO/MP4 ", "x")).toBe("video");
  });

  it("falls back to the extension when the type is unhelpful", () => {
    // Plenty of CDNs serve everything as octet-stream.
    expect(ext.classify("application/octet-stream", "movie.mkv")).toBe("video");
    expect(ext.classify("", "album.flac")).toBe("audio");
    expect(ext.classify("", "book.epub")).toBe("document");
    expect(ext.classify("", "backup.tar.gz")).toBe("archive");
  });

  it("is case-insensitive on the extension", () => {
    expect(ext.classify("", "CLIP.MP4")).toBe("video");
    expect(ext.classify("", "Photo.JPEG")).toBe("image");
  });

  it("returns other for a generic binary, and null for page furniture", () => {
    expect(ext.classify("application/octet-stream", "blob")).toBe("other");
    expect(ext.classify("", "installer.AppImage")).toBe("other");
    // Nothing worth handing to a download manager.
    expect(ext.classify("text/html", "index.html")).toBeNull();
    expect(ext.classify("application/json", "api")).toBeNull();
    expect(ext.classify("text/css", "site.css")).toBeNull();
    expect(ext.classify("", "")).toBeNull();
    expect(ext.classify(null, null)).toBeNull();
  });

  it("treats a manifest as video so it is routed to yt-dlp", () => {
    expect(ext.classify("application/vnd.apple.mpegurl", "live.m3u8")).toBe("video");
  });
});

describe("shouldTakeOver", () => {
  const settings = {
    enabled: true,
    intercept: true,
    minSizeKb: 512,
    excludeDomains: ["intranet.test"],
    types: { video: true, audio: true, archive: true, document: true, image: false, other: true },
  };
  const item = (over = {}) => ({
    url: "https://e.test/movie.mkv",
    finalUrl: "",
    mime: "video/x-matroska",
    filename: "",
    fileSize: 900 * 1024,
    ...over,
  });

  it("takes a matching download", () => {
    expect(ext.shouldTakeOver(item(), settings)).toBe(true);
  });

  it("is off unless both switches are on", () => {
    expect(ext.shouldTakeOver(item(), { ...settings, enabled: false })).toBe(false);
    expect(ext.shouldTakeOver(item(), { ...settings, intercept: false })).toBe(false);
    // A cold service worker has no settings yet; it must not decide blind.
    expect(ext.shouldTakeOver(item(), null)).toBe(false);
    expect(ext.shouldTakeOver(item(), undefined)).toBe(false);
  });

  it("prefers finalUrl, since that is what actually gets fetched", () => {
    const redirected = item({ finalUrl: "https://cdn.test/movie.mkv" });
    expect(ext.shouldTakeOver(redirected, settings)).toBe(true);
    // The exclusion applies to where it ended up, not where it started.
    expect(
      ext.shouldTakeOver(item({ finalUrl: "https://intranet.test/movie.mkv" }), settings),
    ).toBe(false);
  });

  it("leaves alone what the settings say to leave alone", () => {
    expect(ext.shouldTakeOver(item({ url: "https://intranet.test/movie.mkv", finalUrl: "" }), settings)).toBe(false);
    // Images are off in these settings.
    expect(ext.shouldTakeOver(item({ mime: "image/png", url: "https://e.test/a.png" }), settings)).toBe(false);
    // Nothing a download manager should be taking.
    expect(ext.shouldTakeOver(item({ mime: "text/html", url: "https://e.test/page.html" }), settings)).toBe(false);
    // Not http(s).
    expect(ext.shouldTakeOver(item({ url: "blob:https://e.test/abc", finalUrl: "" }), settings)).toBe(false);
  });

  it("skips a file that is known to be small, but not one of unknown size", () => {
    expect(ext.shouldTakeOver(item({ fileSize: 10 * 1024 }), settings)).toBe(false);
    // Chrome reports -1 or 0 until it knows; those must not be filtered out.
    expect(ext.shouldTakeOver(item({ fileSize: -1 }), settings)).toBe(true);
    expect(ext.shouldTakeOver(item({ fileSize: 0 }), settings)).toBe(true);
  });
});

describe("isStreamManifest", () => {
  it("matches a manifest with a query or fragment after it", () => {
    expect(ext.isStreamManifest("https://e.test/live.m3u8")).toBe(true);
    expect(ext.isStreamManifest("https://e.test/live.m3u8?token=abc")).toBe(true);
    expect(ext.isStreamManifest("https://e.test/v.mpd#t=10")).toBe(true);
    expect(ext.isStreamManifest("https://e.test/LIVE.M3U8")).toBe(true);
  });

  it("does not match a plain file or a manifest name inside a longer one", () => {
    expect(ext.isStreamManifest("https://e.test/video.mp4")).toBe(false);
    expect(ext.isStreamManifest("https://e.test/get?f=movie.m3u8.txt")).toBe(false);
    expect(ext.isStreamManifest("")).toBe(false);
    expect(ext.isStreamManifest(null)).toBe(false);
  });
});

describe("isVideoSite", () => {
  it("matches known hosts and their subdomains but not lookalikes", () => {
    expect(ext.isVideoSite("https://www.youtube.com/watch?v=abc")).toBe(true);
    expect(ext.isVideoSite("https://music.youtube.com/watch?v=abc")).toBe(true);
    expect(ext.isVideoSite("https://WWW.YouTube.COM/x")).toBe(true);
    expect(ext.isVideoSite("https://evilyoutube.com/x")).toBe(false);
    expect(ext.isVideoSite("https://youtube.com.evil.test/x")).toBe(false);
    expect(ext.isVideoSite("not a url")).toBe(false);
  });
});

describe("videoTitle", () => {
  it("strips the site suffix a tab title carries", () => {
    expect(ext.videoTitle("Big Buck Bunny - YouTube")).toBe("Big Buck Bunny");
    expect(ext.videoTitle("Some Clip | Twitch")).toBe("Some Clip");
    expect(ext.videoTitle("A Film on Vimeo")).toBe("A Film");
  });

  it("strips a leading unread-count badge", () => {
    expect(ext.videoTitle("(3) Big Buck Bunny - YouTube")).toBe("Big Buck Bunny");
    expect(ext.videoTitle("(12) Something")).toBe("Something");
  });

  it("leaves an ordinary title alone", () => {
    expect(ext.videoTitle("Just A Page")).toBe("Just A Page");
    // A dash mid-title is not a site suffix.
    expect(ext.videoTitle("Before - After Effects Tutorial")).toBe("Before - After Effects Tutorial");
  });

  it("returns null when nothing usable is left", () => {
    expect(ext.videoTitle("")).toBeNull();
    expect(ext.videoTitle(null)).toBeNull();
    expect(ext.videoTitle(undefined)).toBeNull();
    expect(ext.videoTitle("- YouTube")).toBeNull();
  });
});

describe("isExcluded", () => {
  const settings = { excludeDomains: ["intranet.test", "bank.example"] };

  it("matches the host and its subdomains", () => {
    expect(ext.isExcluded("https://intranet.test/a.zip", settings)).toBe(true);
    expect(ext.isExcluded("https://mail.intranet.test/a.zip", settings)).toBe(true);
  });

  it("does not match a lookalike or an unlisted host", () => {
    expect(ext.isExcluded("https://notintranet.test/a.zip", settings)).toBe(false);
    expect(ext.isExcluded("https://example.com/a.zip", settings)).toBe(false);
  });

  it("excludes nothing when the list is empty", () => {
    expect(ext.isExcluded("https://example.com/a", { excludeDomains: [] })).toBe(false);
  });
});

describe("filenameFromUrl", () => {
  it("takes the last path segment, decoded", () => {
    expect(ext.filenameFromUrl("https://e.test/files/my%20report.pdf")).toBe("my report.pdf");
    expect(ext.filenameFromUrl("https://e.test/a/b.zip?x=1")).toBe("b.zip");
  });

  it("falls back to the whole URL when there is no filename", () => {
    // Better a long label in the popup than a blank row.
    expect(ext.filenameFromUrl("https://e.test/")).toBe("https://e.test/");
    expect(ext.filenameFromUrl("not a url")).toBe("not a url");
  });
});

describe("getSettings", () => {
  it("fills in every default when nothing is stored", async () => {
    const e = load({
      chrome: {
        storage: { local: { get: async () => ({}), set: async () => {} } },
        cookies: { getAll: async () => [] },
      },
    });
    const s = await e.getSettings();
    expect(s.enabled).toBe(true);
    expect(s.askBeforeDownload).toBe(true);
    expect(s.showPanel).toBe(true);
    expect(s.minSizeKb).toBe(512);
    expect(s.types.video).toBe(true);
    expect(s.types.image).toBe(false);
  });

  it("falls back to the defaults when storage throws", async () => {
    // A rejection here used to reach a top-level caller with no handler, which
    // Chrome reports against the file itself with no usable line number.
    const e = load({
      chrome: {
        storage: {
          local: {
            get: async () => {
              throw new Error("storage unavailable");
            },
            set: async () => {},
          },
        },
        cookies: { getAll: async () => [] },
      },
    });
    const s = await e.getSettings();
    expect(s.enabled).toBe(true);
    expect(s.minSizeKb).toBe(512);
    expect(s.types.video).toBe(true);
  });

  it("merges the type map instead of replacing it", async () => {
    // A settings blob written before `image` existed must not lose the other
    // types, and must not turn every unknown type off.
    const e = load({
      chrome: {
        storage: {
          local: {
            get: async () => ({ settings: { minSizeKb: 0, types: { image: true } } }),
            set: async () => {},
          },
        },
        cookies: { getAll: async () => [] },
      },
    });
    const s = await e.getSettings();
    expect(s.minSizeKb).toBe(0);
    expect(s.types.image).toBe(true);
    // Other types must survive a partial blob.
    expect(s.types.video).toBe(true);
  });
});

describe("sendToSpool", () => {
  function harness(overrides = {}) {
    const calls = [];
    const e = load({
      native: vi.fn(async (host, body) => {
        calls.push({ host, body });
        return { ok: true, id: "d1" };
      }),
      chrome: {
        storage: { local: { get: async () => ({}), set: async () => {} } },
        cookies: { getAll: async () => [{ name: "a", value: "1" }, { name: "b", value: "2" }] },
      },
      ...overrides,
    });
    return { e, calls };
  }

  it("posts the URL with the browser's cookies and agent", async () => {
    const { e, calls } = harness();
    await expect(e.sendToSpool("https://e.test/a.zip", "https://e.test/")).resolves.toBe(true);
    expect(calls[0].host).toBe("com.saikarthik.spool");
    expect(calls[0].body.type).toBe("add");
    expect(calls[0].body.cookie).toBe("a=1; b=2");
    expect(calls[0].body.userAgent).toBe("TestAgent/1.0");
    expect(calls[0].body.referer).toBe("https://e.test/");
    expect(calls[0].body.video).toBe(false);
  });

  it("takes the ask setting when the caller does not override it", async () => {
    const { e, calls } = harness();
    await e.sendToSpool("https://e.test/a.zip", null);
    // askBeforeDownload defaults on.
    expect(calls[0].body.ask).toBe(true);
  });

  it("lets a batch grab force ask off so 30 links do not open 30 dialogs", async () => {
    const { e, calls } = harness();
    await e.sendToSpool("https://e.test/a.zip", null, false, false);
    expect(calls[0].body.ask).toBe(false);
  });

  it("forwards the video flag for a manifest or a video site", async () => {
    const { e, calls } = harness();
    await e.sendToSpool("https://e.test/live.m3u8", null, true, false);
    expect(calls[0].body.video).toBe(true);
  });

  it("sends no cookie header when the browser holds none", async () => {
    const { e, calls } = harness({
      chrome: {
        storage: { local: { get: async () => ({}), set: async () => {} } },
        cookies: { getAll: async () => [] },
      },
    });
    await e.sendToSpool("https://e.test/a.zip", null);
    expect(calls[0].body.cookie).toBeNull();
  });

  it("reports failure instead of throwing when spool is not running", async () => {
    const { e } = harness({
      native: async () => {
        throw new Error("Native host has exited.");
      },
    });
    await expect(e.sendToSpool("https://e.test/a.zip", null)).resolves.toBe(false);
  });

  it("reports failure when the app refuses the download", async () => {
    const { e } = harness({ native: async () => ({ ok: false, error: "empty URL" }) });
    await expect(e.sendToSpool("https://e.test/a.zip", null)).resolves.toBe(false);
  });

  it("survives a cookies API that rejects", async () => {
    // The cookies permission can be revoked, or the URL can be one Chrome
    // refuses to read cookies for.
    const { e, calls } = harness({
      chrome: {
        storage: { local: { get: async () => ({}), set: async () => {} } },
        cookies: {
          getAll: async () => {
            throw new Error("no permission");
          },
        },
      },
    });
    await expect(e.sendToSpool("https://e.test/a.zip", null)).resolves.toBe(true);
    expect(calls[0].body.cookie).toBeNull();
  });
});

describe("spoolAlive", () => {
  it("is true only when the ping answers", async () => {
    const up = load({ native: async (_host, msg) => (msg.type === "ping" ? { ok: true } : {}) });
    await expect(up.spoolAlive()).resolves.toBe(true);

    // Host not registered, or registered but the app is not running.
    const refusing = load({
      native: async () => {
        throw new Error("Specified native messaging host not found.");
      },
    });
    await expect(refusing.spoolAlive()).resolves.toBe(false);

    // A host that answers nothing at all.
    const silent = load({ native: async () => undefined });
    await expect(silent.spoolAlive()).resolves.toBe(false);
  });
});
