import { describe, expect, it } from "vitest";
import { isStreamManifest, isVideoUrl, suggestedName } from "./AddDialog";

describe("isVideoUrl", () => {
  // Decides only whether the quality picker is offered, so drift with the
  // backend's VIDEO_HOSTS hides an option rather than breaking a download.
  it("matches known hosts and their subdomains", () => {
    expect(isVideoUrl("https://www.youtube.com/watch?v=abc")).toBe(true);
    expect(isVideoUrl("https://m.youtube.com/watch?v=abc")).toBe(true);
    expect(isVideoUrl("https://youtu.be/abc")).toBe(true);
    expect(isVideoUrl("https://clips.twitch.tv/foo")).toBe(true);
  });

  it("is case-insensitive on the host", () => {
    expect(isVideoUrl("https://WWW.YouTube.COM/watch?v=abc")).toBe(true);
  });

  it("rejects lookalike hosts", () => {
    // The suffix must fall on a label boundary.
    expect(isVideoUrl("https://evilyoutube.com/watch?v=abc")).toBe(false);
    expect(isVideoUrl("https://youtube.com.evil.test/watch?v=abc")).toBe(false);
  });

  it("rejects ordinary files and unparseable input", () => {
    expect(isVideoUrl("https://example.com/file.zip")).toBe(false);
    expect(isVideoUrl("not a url")).toBe(false);
    expect(isVideoUrl("")).toBe(false);
  });
});

describe("isStreamManifest", () => {
  // The dialog offers the quality picker for anything yt-dlp will handle, so
  // this has to agree with is_stream_manifest in src-tauri/src/ytdlp.rs.
  it("matches a manifest whatever host it sits on", () => {
    expect(isStreamManifest("https://cdn.example.com/live/master.m3u8")).toBe(true);
    expect(isStreamManifest("https://cdn.example.com/dash/manifest.mpd")).toBe(true);
  });

  it("ignores case, query and fragment", () => {
    expect(isStreamManifest("https://e.test/LIVE/STREAM.M3U8")).toBe(true);
    expect(isStreamManifest("https://e.test/v.m3u8?token=abc")).toBe(true);
    expect(isStreamManifest("https://e.test/v.mpd#t=10")).toBe(true);
  });

  it("does not match a plain file, or a manifest name that is not the resource", () => {
    expect(isStreamManifest("https://e.test/video.mp4")).toBe(false);
    expect(isStreamManifest("https://e.test/get?f=movie.m3u8")).toBe(false);
    expect(isStreamManifest("https://e.test/x.m3u8/thumb.jpg")).toBe(false);
  });

  it("stays inside http and https, like the backend", () => {
    // Routing on this hands the URL to yt-dlp as a subprocess argument.
    expect(isStreamManifest("file:///tmp/x.m3u8")).toBe(false);
    expect(isStreamManifest("not a url")).toBe(false);
    expect(isStreamManifest("")).toBe(false);
  });
});

describe("suggestedName", () => {
  it("takes the last path segment", () => {
    expect(suggestedName("https://example.com/files/report.pdf")).toBe("report.pdf");
  });

  it("ignores the query and fragment", () => {
    expect(suggestedName("https://example.com/a/b.zip?token=xyz#top")).toBe("b.zip");
  });

  it("decodes percent-escapes so the hint is readable", () => {
    expect(suggestedName("https://example.com/my%20file.zip")).toBe("my file.zip");
  });

  it("ignores a trailing slash rather than suggesting an empty name", () => {
    expect(suggestedName("https://example.com/downloads/")).toBe("downloads");
  });

  it("is blank when there is nothing to suggest", () => {
    // The field then shows "Automatic", which is what a blank name means.
    expect(suggestedName("https://example.com/")).toBe("");
    expect(suggestedName("not a url")).toBe("");
  });

  it("does not throw on a malformed escape", () => {
    // decodeURIComponent throws on "%zz"; the caller must still get a string.
    expect(() => suggestedName("https://example.com/bad%zz.zip")).not.toThrow();
  });

  it("extracts display name from magnet links", () => {
    expect(
      suggestedName("magnet:?xt=urn:btih:abc&dn=Ubuntu+22.04+Desktop&tr=http://tracker"),
    ).toBe("Ubuntu 22.04 Desktop");
    expect(
      suggestedName("magnet:?xt=urn:btih:abc&dn=My%20Cool%20File.iso"),
    ).toBe("My Cool File.iso");
    expect(suggestedName("magnet:?xt=urn:btih:abc")).toBe("torrent");
  });
});
