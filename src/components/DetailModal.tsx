import { Copy, ExternalLink, Folder } from "lucide-react";
import { api, formatBytes, formatDate, type DownloadView } from "../lib/api";
import { Button } from "./ui/button";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "./ui/dialog";
import { Progress } from "./ui/progress";

/// Full detail for one download — the "properties" screen. Live progress is
/// merged in from the parent so per-segment bars move while it runs.
export function DetailModal({
  row,
  liveBytes,
  liveTotal,
  speed,
  onClose,
}: {
  row: DownloadView;
  liveBytes?: number;
  liveTotal?: number;
  speed: number;
  onClose: () => void;
}) {
  const downloaded =
    row.status === "downloading" && liveBytes !== undefined ? liveBytes : row.downloaded;
  const total = row.total ?? liveTotal ?? null;
  const percent = total ? Math.min(100, (downloaded / total) * 100) : null;

  return (
    <Dialog open onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="max-h-[88vh] overflow-y-auto w-[calc(100vw-2rem)] sm:max-w-lg p-4 sm:p-6">
        <DialogHeader>
          <DialogTitle className="truncate text-base sm:text-lg" title={row.filename}>{row.filename}</DialogTitle>
        </DialogHeader>

        {row.thumbnail && (
          <img
            className="max-h-48 w-full rounded-md object-cover"
            src={row.thumbnail}
            alt=""
            onError={(e) => (e.currentTarget.style.display = "none")}
          />
        )}

        <div className="space-y-1.5">
          <Progress value={percent ?? (downloaded > 0 ? 15 : 0)} />
          <div className="flex flex-wrap items-center justify-between gap-1 text-xs text-muted-foreground">
            <span className="font-mono tabular-nums">{formatBytes(downloaded)}{total ? ` / ${formatBytes(total)}` : ""}</span>
            <span className="flex items-center gap-2 font-mono tabular-nums">
              {percent !== null ? `${percent.toFixed(1)}%` : "size unknown"}
              {row.status === "downloading" && <span>{formatBytes(speed)}/s</span>}
            </span>
          </div>
        </div>

        <dl className="grid grid-cols-1 xs:grid-cols-[auto_1fr] gap-x-3 sm:gap-x-4 gap-y-1.5 sm:gap-y-2 text-xs sm:text-sm">
          <Field label="Status" value={cap(row.status)} />
          <Field label="Saved to" value={row.path} mono copyable />
          <Field label="Source URL" value={row.url} mono copyable />
          <Field label="Size" value={total !== null ? formatBytes(total) : "unknown"} />
          <Field
            label="Connections"
            value={
              row.engine === "ytdlp"
                ? "yt-dlp engine"
                : row.engine === "ftp"
                ? "1 (FTP, resumed with REST)"
                : row.engine === "torrent"
                ? "BitTorrent swarm"
                : row.supports_ranges
                  ? `${row.segments} (server supports byte ranges)`
                  : "1 (server has no range support)"
            }
          />
          <Field label="Added" value={formatDate(row.added_at)} />
          {(row.user_agent || row.referer || row.has_cookie) && (
            <Field
              label="Browser session"
              value={[
                row.has_cookie ? "cookies attached" : null,
                row.referer ? `referer ${row.referer}` : null,
                row.user_agent ? `UA ${row.user_agent}` : null,
              ].filter(Boolean).join(" · ") || "—"}
              mono
            />
          )}
          {row.error && <Field label="Error" value={row.error} error />}
        </dl>

        {row.ranges.length > 1 && (
          <div className="space-y-2">
            <h3 className="text-sm font-medium">Segments</h3>
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-2">
              {row.ranges.map(([start, end], i) => {
                const size = end - start + 1;
                const got = row.done[i] ?? 0;
                const pct = size > 0 ? Math.min(100, (got / size) * 100) : 100;
                return (
                  <div className="space-y-1 rounded-md border border-border/60 bg-muted/30 p-2" key={i}>
                    <Progress value={pct} className="h-1.5" />
                    <span className="text-[11px] text-muted-foreground flex items-center justify-between">
                      <span>#{i + 1}</span>
                      <span className="font-mono tabular-nums">{formatBytes(got)} / {formatBytes(size)}</span>
                    </span>
                  </div>
                );
              })}
            </div>
          </div>
        )}

        <div className="flex flex-wrap gap-2">
          {row.status === "completed" && (
            <>
              <Button onClick={() => api.openFile(row.path)}>
                <ExternalLink /> Open file
              </Button>
              <Button variant="outline" onClick={() => api.revealFile(row.path)}>
                <Folder /> Open folder
              </Button>
            </>
          )}
          <Button variant="outline" onClick={() => navigator.clipboard.writeText(row.url)}>
            <Copy /> Copy URL
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function Field({
  label, value, mono, copyable, error,
}: { label: string; value: string; mono?: boolean; copyable?: boolean; error?: boolean }) {
  return (
    <>
      <dt className="text-muted-foreground">{label}</dt>
      <dd className={`flex items-center gap-1.5 truncate ${mono ? "font-mono" : ""} ${error ? "text-destructive" : ""}`}>
        <span className="truncate" title={value}>{value}</span>
        {copyable && (
          <Button
            variant="ghost"
            size="icon-xs"
            title="Copy"
            onClick={() => navigator.clipboard.writeText(value)}
          >
            <Copy />
          </Button>
        )}
      </dd>
    </>
  );
}

const cap = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);
