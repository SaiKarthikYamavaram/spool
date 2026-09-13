import { useEffect, useState } from "react";
import { Download, Folder, Magnet } from "lucide-react";
import { api, type AddOptions } from "../lib/api";
import { expandAll } from "../lib/pattern";
import { Button } from "./ui/button";
import { Checkbox } from "./ui/checkbox";
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from "./ui/dialog";
import { Input } from "./ui/input";
import { Label } from "./ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "./ui/select";
import { Textarea } from "./ui/textarea";

/// Hosts spool routes to yt-dlp. Kept in sync with VIDEO_HOSTS in
/// src-tauri/src/ytdlp.rs — used only to decide whether to offer the quality
/// picker, so drift just hides an option rather than breaking a download.
const VIDEO_HOSTS = [
  "youtube.com", "youtu.be", "vimeo.com", "dailymotion.com", "twitch.tv",
  "tiktok.com", "instagram.com", "facebook.com", "fb.watch", "twitter.com",
  "x.com", "reddit.com", "soundcloud.com", "bilibili.com", "nicovideo.jp",
  "streamable.com",
];

export function isVideoUrl(url: string): boolean {
  try {
    const h = new URL(url).hostname.toLowerCase();
    return VIDEO_HOSTS.some((d) => h === d || h.endsWith(`.${d}`));
  } catch {
    return false;
  }
}

/// HLS/DASH manifests go through yt-dlp too, whatever host they sit on, so
/// they get the same options a known video site would. Kept in sync with
/// `is_stream_manifest` in src-tauri/src/ytdlp.rs — including the scheme
/// check, so the two agree on what counts.
export function isStreamManifest(url: string): boolean {
  try {
    const u = new URL(url);
    if (u.protocol !== "http:" && u.protocol !== "https:") return false;
    const path = u.pathname.toLowerCase();
    return path.endsWith(".m3u8") || path.endsWith(".mpd");
  } catch {
    return false;
  }
}

/// Best guess at the filename, shown as the placeholder so the field hints at
/// what "automatic" will produce. The real name can still differ — the server's
/// Content-Disposition or the video's title wins when the field is left blank.
export function suggestedName(url: string): string {
  try {
    const last = new URL(url).pathname.split("/").filter(Boolean).pop() ?? "";
    return decodeURIComponent(last);
  } catch {
    return "";
  }
}

/// The pre-download dialog: choose where the file lands and how it is fetched
/// before anything starts, instead of silently using the defaults.
/// Split a pasted blob into links. Whitespace-separated with comments skipped,
/// so the same text that works in the Import box works here too.
function splitUrls(text: string): string[] {
  return text
    .split(/\s+/)
    .map((s) => s.trim())
    .filter((s) => s && !s.startsWith("#"));
}

export function AddDialog({
  url,
  token,
  multi = false,
  onClose,
  onAdded,
}: {
  url: string;
  /// Set when the extension parked this request; confirming adds it with the
  /// browser session captured at capture time.
  token?: string | null;
  /// Opened from Import: the URL field starts as a list, and stays one even
  /// after the text is cleared.
  multi?: boolean;
  onClose: () => void;
  onAdded: (msg: string | null) => void;
}) {
  // A URL the extension parked is fixed — that is the request being confirmed.
  // One typed by hand is the whole point of the dialog, so it stays editable.
  const locked = Boolean(token);
  const [value, setValue] = useState(url);
  const [dir, setDir] = useState("");
  const [name, setName] = useState("");
  const [quality, setQuality] = useState("");
  const [start, setStart] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // `file[001-050].jpg` is fifty links. Expansion happens here rather than in
  // the backend so the dialog can show the real count before anything is
  // queued — and refuse an obvious typo instead of adding 10 million rows.
  let links: string[];
  let patternError: string | null = null;
  try {
    links = expandAll(splitUrls(value));
  } catch (e) {
    links = [];
    patternError = e instanceof Error ? e.message : String(e);
  }
  const typed = splitUrls(value);
  const expanded = links.length > typed.length;
  const batch = links.length > 1;
  // A list stays a list: shrinking the box back to one line the moment the
  // second URL is deleted is not helpful while editing.
  const asList = multi || batch;
  const one = links[0] ?? "";
  // Either route lands on yt-dlp, which is what the quality picker drives.
  const video = isVideoUrl(one) || isStreamManifest(one);

  useEffect(() => {
    // Show the real default folder rather than a vague placeholder.
    api.getDownloadDir().then(setDir).catch(() => setDir(""));
  }, []);

  /// Cancelling a parked (extension) request also frees it on the backend.
  function dismiss() {
    if (token) api.cancelPending(token).catch(() => {});
    onClose();
  }

  async function browse() {
    try {
      const picked = await api.pickFolder(dir || undefined);
      if (picked) setDir(picked);
    } catch (e) {
      setError(String(e));
    }
  }

  async function openTorrent() {
    try {
      const picked = await api.pickTorrent();
      if (!picked) return;
      // As a file:// URL, so a space in the path cannot split it into two
      // links. Appended, so a file can join links already pasted.
      const link = "file://" + picked.split("/").map(encodeURIComponent).join("/");
      setValue((v) => (v.trim() ? `${v.trimEnd()}\n${link}` : link));
    } catch (e) {
      setError(String(e));
    }
  }

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    if (busy) return;
    setBusy(true);
    setError(null);

    const options: AddOptions = {
      dir: dir.trim() || null,
      // A filename is a single-download answer; a batch takes the server's.
      name: batch ? null : name.trim() || null,
      quality: video && quality ? quality : null,
      start,
    };

    try {
      if (token) {
        const dup = await api.isDuplicate(one);
        await api.addPending(token, options);
        onAdded(dup ? "Already in the queue — added again." : null);
      } else if (batch) {
        // Queued one at a time rather than through import_urls, so the folder
        // and the start-now choice apply to every link in the batch.
        const failed: string[] = [];
        for (const link of links) {
          try {
            await api.addDownload(link, options);
          } catch {
            failed.push(link);
          }
        }
        const added = links.length - failed.length;
        if (added === 0) throw new Error("None of those links could be added.");
        onAdded(
          failed.length
            ? `Added ${added} of ${links.length}. ${failed.length} skipped.`
            : `Added ${added} downloads.`,
        );
      } else {
        const dup = await api.isDuplicate(one);
        await api.addDownload(one, options);
        onAdded(dup ? "Already in the queue — added again." : null);
      }
      onClose();
    } catch (e) {
      setError(String(e));
      setBusy(false);
    }
  }

  return (
    <Dialog open onOpenChange={(open) => !open && dismiss()}>
      <DialogContent className="max-h-[88vh] overflow-y-auto w-[calc(100vw-2rem)] sm:max-w-lg p-4 sm:p-6">
        <form onSubmit={submit} className="space-y-4">
          <DialogHeader>
            <DialogTitle>{multi ? "Import links" : "Add download"}</DialogTitle>
          </DialogHeader>

          <div className="space-y-1.5">
            <Label>{links.length > 1 ? `${links.length} links` : multi ? "Links" : "URL"}</Label>
            {asList ? (
              <Textarea
                value={value}
                onChange={(e) => setValue(e.currentTarget.value)}
                readOnly={locked}
                placeholder={"https://example.com/one.zip\nhttps://example.com/two.zip\n\n# lines starting with # are skipped"}
                spellCheck={false}
                rows={5}
                autoFocus={!locked}
              />
            ) : (
              <Input
                value={value}
                onChange={(e) => setValue(e.currentTarget.value)}
                readOnly={locked}
                placeholder="https://…  — or paste several at once"
                spellCheck={false}
                autoFocus={!locked}
              />
            )}
            {patternError ? (
              <p className="text-sm text-destructive">{patternError}</p>
            ) : expanded ? (
              <p className="text-sm text-muted-foreground">
                Expanded to {links.length} links. A range like{" "}
                <code>file[001-050].jpg</code> downloads every file in it.
              </p>
            ) : null}
            {!locked && (
              <Button type="button" variant="ghost" size="sm" className="h-7 px-2 text-xs" onClick={openTorrent}>
                <Magnet /> Open a .torrent file
              </Button>
            )}
          </div>

          <div className="space-y-1.5">
            <Label>Save to</Label>
            <div className="flex gap-2">
              <Input
                value={dir}
                onChange={(e) => setDir(e.currentTarget.value)}
                placeholder="Download folder"
                spellCheck={false}
              />
              <Button type="button" variant="outline" onClick={browse}>
                <Folder /> Browse
              </Button>
            </div>
          </div>

          <div className="space-y-1.5">
            <Label>Save as</Label>
            <Input
              value={asList ? "" : name}
              onChange={(e) => setName(e.currentTarget.value)}
              // A video URL's last path segment is routing ("watch", "video"),
              // never a filename — yt-dlp names it from the title instead.
              placeholder={
                asList
                  ? "One name cannot cover several links"
                  : (video ? "" : suggestedName(one)) || "Automatic"
              }
              spellCheck={false}
              disabled={asList}
            />
            <p className="text-sm text-muted-foreground">
              {asList
                ? "Every link goes to the folder above and keeps the name its server gives it."
                : video
                  ? "Leave blank to use the video's title. The container is picked by yt-dlp."
                  : "Leave blank to use the server's name. Without an extension, the source's is kept."}
            </p>
          </div>

          {video && (
            <div className="space-y-1.5">
              <Label>Video quality</Label>
              <Select value={quality || "setting"} onValueChange={(v) => setQuality(v === "setting" ? "" : v)}>
                <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
                <SelectContent>
                  <SelectItem value="setting">Use setting</SelectItem>
                  <SelectItem value="best">Best available</SelectItem>
                  <SelectItem value="2160">2160p (4K)</SelectItem>
                  <SelectItem value="1440">1440p</SelectItem>
                  <SelectItem value="1080">1080p</SelectItem>
                  <SelectItem value="720">720p</SelectItem>
                  <SelectItem value="480">480p</SelectItem>
                  <SelectItem value="audio">Audio only (mp3)</SelectItem>
                </SelectContent>
              </Select>
            </div>
          )}

          <Label className="flex items-center gap-2 font-normal">
            <Checkbox checked={start} onCheckedChange={(v) => setStart(v === true)} />
            Start now (uncheck to add it paused)
          </Label>

          {error && <p className="text-sm text-destructive">{error}</p>}

          <DialogFooter>
            <Button type="button" variant="outline" onClick={dismiss}>Cancel</Button>
            <Button type="submit" disabled={busy || links.length === 0 || Boolean(patternError)}>
              <Download />{" "}
              {busy
                ? "Adding…"
                : batch
                  ? `Download ${links.length}`
                  : multi
                    ? "Import"
                    : start
                      ? "Download"
                      : "Add paused"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
