import { useEffect, useRef, useState } from "react";
import {
  Check,
  Clipboard,
  Clock,
  Copy,
  ExternalLink,
  Folder,
  FolderOpen,
  Gauge,
  Monitor,
  Moon,
  Power,
  Puzzle,
  ShieldAlert,
  Sparkles,
  Sun,
  Video,
} from "lucide-react";
import { listen } from "@tauri-apps/api/event";
import { api, type ExtensionStatus, type Settings } from "../lib/api";
import { applyTheme } from "../lib/theme";
import { cn } from "cn";
import { Badge } from "./ui/badge";
import { Button } from "./ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "./ui/card";
import { Input } from "./ui/input";
import { Label } from "./ui/label";
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from "./ui/select";
import { Switch } from "./ui/switch";

const THEME_OPTIONS = [
  { value: "system", label: "System", icon: <Monitor className="size-4" /> },
  { value: "light", label: "Light", icon: <Sun className="size-4" /> },
  { value: "dark", label: "Dark", icon: <Moon className="size-4" /> },
];

const BANDWIDTH_PRESETS = [
  { label: "Unlimited", kb: 0 },
  { label: "1 MB/s", kb: 1024 },
  { label: "5 MB/s", kb: 5120 },
  { label: "10 MB/s", kb: 10240 },
];

/// One settings section: an icon-badged card title, its fields as children.
/// Keeps the page a stack of named, scannable groups instead of one long
/// column of unrelated-looking rows.
function Section({
  icon, title, children,
}: { icon: React.ReactNode; title: string; children: React.ReactNode }) {
  return (
    <Card className="gap-3 sm:gap-4 border-border/80 bg-card/95 dark:bg-card/60 py-4 sm:py-5 shadow-xs dark:shadow-sm backdrop-blur-sm">
      <CardHeader className="px-3.5 sm:px-5">
        <CardTitle className="flex items-center gap-2.5 text-[13px] font-semibold uppercase tracking-wide text-muted-foreground">
          <span className="flex size-7 items-center justify-center rounded-full bg-primary/10 text-primary">
            {icon}
          </span>
          {title}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3.5 sm:space-y-4 px-3.5 sm:px-5">{children}</CardContent>
    </Card>
  );
}

export function SettingsView() {
  const [settings, setSettings] = useState<Settings | null>(null);
  const [saved, setSaved] = useState(false);
  const [extStatus, setExtStatus] = useState<ExtensionStatus | null>(null);
  const [copiedPath, setCopiedPath] = useState(false);
  const debounceTimer = useRef<number | null>(null);
  const latestSettings = useRef<Settings | null>(null);

  useEffect(() => {
    api.getExtensionStatus().then(setExtStatus).catch(() => {});
    const unlistenPromise = listen("extension://status", () => {
      api.getExtensionStatus().then(setExtStatus).catch(() => {});
    });
    const interval = window.setInterval(() => {
      api.getExtensionStatus().then(setExtStatus).catch(() => {});
    }, 4000);
    return () => {
      window.clearInterval(interval);
      unlistenPromise.then((u) => u());
    };
  }, []);

  async function copyExtensionPath() {
    if (!extStatus?.canonical_path) return;
    try {
      await navigator.clipboard.writeText(extStatus.canonical_path);
      setCopiedPath(true);
      window.setTimeout(() => setCopiedPath(false), 2000);
    } catch {
      // ignore
    }
  }

  useEffect(() => {
    api.getSettings().then((s) => {
      setSettings(s);
      latestSettings.current = s;
    });
  }, []);

  useEffect(() => {
    return () => {
      if (debounceTimer.current && latestSettings.current) {
        window.clearTimeout(debounceTimer.current);
        api.updateSettings(latestSettings.current);
      }
    };
  }, []);

  if (!settings) return null;

  function commit(next: Settings) {
    api.updateSettings(next);
    setSaved(true);
    window.setTimeout(() => setSaved(false), 1200);
  }

  function update(patch: Partial<Settings>, immediate = false) {
    const next = { ...settings!, ...patch };
    setSettings(next);
    latestSettings.current = next;

    if (debounceTimer.current) {
      window.clearTimeout(debounceTimer.current);
      debounceTimer.current = null;
    }

    if (immediate) {
      commit(next);
    } else {
      debounceTimer.current = window.setTimeout(() => {
        commit(next);
        debounceTimer.current = null;
      }, 400);
    }
  }

  return (
    <section className="mx-auto max-w-2xl space-y-4 pb-8">
      <div className="flex items-center gap-2 px-1">
        <h2 className="text-lg font-semibold">Settings</h2>
        {saved && <Badge variant="secondary" className="border border-primary/20 bg-primary/10 text-primary">Saved</Badge>}
      </div>

      <Section icon={<Folder className="size-3.5" />} title="Downloads">
        <div className="space-y-1.5">
          <Label>Download folder</Label>
          <div className="flex gap-2">
            <Input
              value={settings.download_dir ?? ""}
              placeholder="~/Downloads"
              spellCheck={false}
              onChange={(e) =>
                update({ download_dir: e.currentTarget.value.trim() || null })
              }
            />
            <Button
              type="button"
              variant="outline"
              onClick={async () => {
                const picked = await api.pickFolder(settings.download_dir ?? undefined);
                // Commit immediately: a picked path is a deliberate choice, not
                // mid-typing, so it should not wait on the debounce.
                if (picked) update({ download_dir: picked }, true);
              }}
            >
              <Folder /> Browse
            </Button>
          </div>
        </div>

        <Label className="flex items-center gap-2 font-normal">
          <Switch
            checked={settings.categorize}
            onCheckedChange={(v) => update({ categorize: v }, true)}
          />
          Sort into folders by type (Video, Audio, Archives, …)
        </Label>

        <div className="grid grid-cols-1 sm:grid-cols-2 gap-3 sm:gap-4">
          <div className="space-y-1.5">
            <Label>Concurrent downloads</Label>
            <Input
              type="number"
              min={1}
              max={10}
              value={settings.max_concurrent}
              onChange={(e) =>
                update({ max_concurrent: Math.max(1, Number(e.currentTarget.value) || 1) })
              }
            />
          </div>

          <div className="space-y-1.5">
            <Label>Connections per download</Label>
            <Input
              type="number"
              min={1}
              max={8}
              value={settings.segments}
              onChange={(e) =>
                update({
                  segments: Math.min(8, Math.max(1, Number(e.currentTarget.value) || 1)),
                })
              }
            />
          </div>
        </div>

        <div className="space-y-1.5">
          <Label>Appearance</Label>
          <div className="grid grid-cols-3 gap-2">
            {THEME_OPTIONS.map((opt) => (
              <button
                key={opt.value}
                type="button"
                onClick={() => {
                  applyTheme(opt.value); // repaint now, don't wait for the round trip
                  update({ theme: opt.value }, true);
                }}
                className={cn(
                  "flex flex-col items-center gap-1.5 rounded-lg border p-3 text-xs transition-colors",
                  (settings.theme || "system") === opt.value
                    ? "border-primary bg-primary/10 text-primary font-medium"
                    : "border-border/80 bg-card text-muted-foreground hover:bg-accent hover:text-foreground",
                )}
              >
                {opt.icon}
                {opt.label}
              </button>
            ))}
          </div>
        </div>
      </Section>

      <Section icon={<Puzzle className="size-3.5" />} title="Browser Integration & Extension">
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3 p-3.5 rounded-lg border border-border/70 bg-muted/20">
          <div className="space-y-1">
            <div className="flex items-center gap-2">
              <span className="text-sm font-semibold">Integration Status</span>
              {extStatus?.connected ? (
                <Badge className="bg-emerald-500/15 text-emerald-600 dark:text-emerald-400 border border-emerald-500/30 gap-1.5 py-0.5">
                  <span className="size-1.5 rounded-full bg-emerald-500 animate-pulse" />
                  Connected & Active
                </Badge>
              ) : (extStatus?.registered_browsers.length ?? 0) > 0 ? (
                <Badge className="bg-amber-500/15 text-amber-600 dark:text-amber-400 border border-amber-500/30 gap-1.5 py-0.5">
                  <span className="size-1.5 rounded-full bg-amber-500" />
                  Host Ready (Extension Needed)
                </Badge>
              ) : (
                <Badge variant="outline" className="gap-1.5 py-0.5 text-muted-foreground">
                  <span className="size-1.5 rounded-full bg-muted-foreground" />
                  Not Configured
                </Badge>
              )}
            </div>
            <p className="text-xs text-muted-foreground">
              {extStatus?.connected
                ? "The extension is communicating with spool. Browser downloads & cookies are forwarded automatically."
                : (extStatus?.registered_browsers.length ?? 0) > 0
                ? `Native host registered for ${extStatus!.registered_browsers.join(", ")}. Follow the quick steps below to load the extension.`
                : "No supported browsers detected. Spool registers native hosts for Chrome, Brave, Chromium, Edge, and Firefox."}
            </p>
          </div>

          <div className="flex items-center gap-2 shrink-0">
            <Button
              type="button"
              size="sm"
              variant="outline"
              onClick={() => api.openBrowserExtensions().catch(() => {})}
              className="text-xs h-8"
            >
              <ExternalLink className="size-3.5 mr-1" />
              Extensions Page
            </Button>
          </div>
        </div>

        <div className="space-y-1.5">
          <div className="flex items-center justify-between">
            <Label className="text-xs font-medium">Extension Folder Path</Label>
            <span className="text-[11px] text-muted-foreground">Stable permanent directory</span>
          </div>
          <div className="flex gap-2">
            <Input
              readOnly
              value={extStatus?.canonical_path ?? "Loading path..."}
              className="font-mono text-xs bg-muted/40 select-all"
            />
            <Button
              type="button"
              variant="secondary"
              onClick={copyExtensionPath}
              className="shrink-0 text-xs gap-1.5"
            >
              {copiedPath ? <Check className="size-3.5 text-emerald-500" /> : <Copy className="size-3.5" />}
              {copiedPath ? "Copied!" : "Copy"}
            </Button>
            <Button
              type="button"
              variant="outline"
              onClick={() => api.revealExtensionDir().catch(() => {})}
              className="shrink-0 text-xs gap-1.5"
            >
              <FolderOpen className="size-3.5" />
              Open
            </Button>
          </div>
        </div>

        <div className="rounded-lg border border-border/60 bg-card/40 p-3.5 space-y-3">
          <p className="text-xs font-semibold uppercase tracking-wider text-muted-foreground flex items-center gap-1.5">
            <Sparkles className="size-3.5 text-primary" />
            Quick Setup (Chrome, Brave, Edge & Firefox)
          </p>

          <div className="grid grid-cols-1 sm:grid-cols-3 gap-2.5">
            <div className="flex flex-col justify-between rounded-md border border-border/50 bg-background/50 p-2.5 space-y-2">
              <div className="space-y-1">
                <div className="flex items-center gap-1.5 font-semibold text-xs">
                  <span className="flex size-4 items-center justify-center rounded-full bg-primary/10 text-primary text-[10px]">1</span>
                  Open Extensions
                </div>
                <p className="text-[11px] text-muted-foreground leading-relaxed">
                  Open your browser's extension manager.
                </p>
              </div>
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => api.openBrowserExtensions().catch(() => {})}
                className="text-xs h-7 w-full gap-1 mt-1"
              >
                <ExternalLink className="size-3" />
                chrome://extensions
              </Button>
            </div>

            <div className="flex flex-col justify-between rounded-md border border-border/50 bg-background/50 p-2.5 space-y-2">
              <div className="space-y-1">
                <div className="flex items-center gap-1.5 font-semibold text-xs">
                  <span className="flex size-4 items-center justify-center rounded-full bg-primary/10 text-primary text-[10px]">2</span>
                  Enable Developer Mode
                </div>
                <p className="text-[11px] text-muted-foreground leading-relaxed">
                  Turn on the <strong>Developer mode</strong> toggle in the top-right corner.
                </p>
              </div>
            </div>

            <div className="flex flex-col justify-between rounded-md border border-border/50 bg-background/50 p-2.5 space-y-2">
              <div className="space-y-1">
                <div className="flex items-center gap-1.5 font-semibold text-xs">
                  <span className="flex size-4 items-center justify-center rounded-full bg-primary/10 text-primary text-[10px]">3</span>
                  Load Unpacked
                </div>
                <p className="text-[11px] text-muted-foreground leading-relaxed">
                  Click <strong>Load unpacked</strong> and select the copied folder path.
                </p>
              </div>
              <Button
                type="button"
                size="sm"
                variant="secondary"
                onClick={copyExtensionPath}
                className="text-xs h-7 w-full gap-1 mt-1"
              >
                {copiedPath ? <Check className="size-3 text-emerald-500" /> : <Copy className="size-3" />}
                {copiedPath ? "Path Copied" : "Copy Path"}
              </Button>
            </div>
          </div>

          {(extStatus?.registered_browsers.length ?? 0) > 0 && (
            <div className="pt-2 border-t border-border/40 flex flex-wrap items-center gap-1.5 text-xs text-muted-foreground">
              <span>Registered browsers:</span>
              {extStatus!.registered_browsers.map((b) => (
                <Badge key={b} variant="secondary" className="text-[11px] font-normal py-0 px-2">
                  {b}
                </Badge>
              ))}
            </div>
          )}
        </div>
      </Section>

      <Section icon={<Gauge className="size-3.5" />} title="Bandwidth & network">
        <div className="space-y-1.5">
          <Label>Speed limit (KB/s, 0 = unlimited)</Label>
          <div className="flex flex-wrap gap-1.5">
            {BANDWIDTH_PRESETS.map((preset) => (
              <Button
                key={preset.label}
                type="button"
                size="sm"
                variant={settings.bandwidth_kb === preset.kb ? "default" : "outline"}
                onClick={() => update({ bandwidth_kb: preset.kb }, true)}
              >
                {preset.label}
              </Button>
            ))}
          </div>
          <Input
            type="number"
            min={0}
            step={50}
            value={settings.bandwidth_kb}
            onChange={(e) =>
              update({ bandwidth_kb: Math.max(0, Number(e.currentTarget.value) || 0) })
            }
          />
        </div>

        <div className="space-y-1.5">
          <Label>Proxy (blank = direct)</Label>
          <Input
            value={settings.proxy}
            placeholder="http://host:8080  or  socks5://host:1080"
            spellCheck={false}
            onChange={(e) => update({ proxy: e.currentTarget.value.trim() })}
          />
          <p className="text-sm text-muted-foreground">
            Applies to file downloads and to yt-dlp. Takes effect on the next
            download; transfers already running keep their current connection.
          </p>
        </div>
      </Section>

      <Section icon={<Clock className="size-3.5" />} title="Schedule">
        <Label className="flex items-center gap-2 font-normal">
          <Switch
            checked={settings.schedule_enabled}
            onCheckedChange={(v) => update({ schedule_enabled: v }, true)}
          />
          Only download between these times
        </Label>
        <div className="grid grid-cols-2 gap-3 sm:gap-4">
          <div className="space-y-1.5">
            <Label>Start</Label>
            <Input
              type="time"
              value={settings.schedule_start}
              disabled={!settings.schedule_enabled}
              onChange={(e) => update({ schedule_start: e.currentTarget.value }, true)}
            />
          </div>
          <div className="space-y-1.5">
            <Label>Stop</Label>
            <Input
              type="time"
              value={settings.schedule_stop}
              disabled={!settings.schedule_enabled}
              onChange={(e) => update({ schedule_stop: e.currentTarget.value }, true)}
            />
          </div>
        </div>
        <p className="text-sm text-muted-foreground">
          Transfers pause when the window closes and resume when it opens. A start
          later than the stop runs overnight. Pausing or resuming by hand inside the
          window is left alone.
        </p>

        <div className="space-y-1.5">
          <Label>When every download has finished</Label>
          <Select
            value={settings.on_all_done || "none"}
            onValueChange={(v) => update({ on_all_done: v }, true)}
          >
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>
              <SelectItem value="none">Do nothing</SelectItem>
              <SelectItem value="quit">Quit spool</SelectItem>
              <SelectItem value="shutdown">Shut the computer down</SelectItem>
            </SelectContent>
          </Select>
          {settings.on_all_done === "shutdown" && (
            <p className="text-sm text-destructive">
              The machine powers off once the last download finishes. Anything else
              you have open goes with it.
            </p>
          )}
        </div>
      </Section>

      <Section icon={<Clipboard className="size-3.5" />} title="Clipboard">
        <Label className="flex items-center gap-2 font-normal">
          <Switch
            checked={settings.clipboard_watch}
            onCheckedChange={(v) => update({ clipboard_watch: v }, true)}
          />
          Offer to download links copied to the clipboard
        </Label>
        <p className="text-sm text-muted-foreground">
          Copying a downloadable link (HTTP, FTP, or magnet) opens the add dialog with it filled in.
          Links already in the queue are ignored.
        </p>
      </Section>

      <Section icon={<Power className="size-3.5" />} title="Startup & background">
        <Label className="flex items-center gap-2 font-normal">
          <Switch
            checked={settings.run_in_background}
            onCheckedChange={(v) => update({ run_in_background: v }, true)}
          />
          Keep running in the tray when the window is closed
        </Label>
        <p className="text-sm text-muted-foreground">
          {settings.run_in_background
            ? "Downloads carry on after you close the window. Quit from the tray icon to stop them."
            : "Closing the window quits spool. Anything still downloading is paused and resumes next launch."}
        </p>

        <Label className="flex items-center gap-2 font-normal">
          <Switch
            checked={settings.start_on_login}
            onCheckedChange={(v) => update({ start_on_login: v }, true)}
          />
          Start spool automatically when this computer starts
        </Label>
        {settings.start_on_login && (
          <Label className="flex items-center gap-2 font-normal">
            <Switch
              checked={settings.start_minimised}
              onCheckedChange={(v) => update({ start_minimised: v }, true)}
            />
            Start in the tray, without opening the window
          </Label>
        )}
      </Section>

      <Section icon={<ShieldAlert className="size-3.5" />} title="Sites that block downloaders">
        <p className="text-sm text-muted-foreground">
          Some hosts sit behind an interactive anti-bot challenge and answer with{" "}
          <code>403</code>. No download manager can solve one — not this app, not IDM.
          What IDM actually does is let the <em>browser</em> solve it and then reuse
          that session. Do the same here: export your cookies and point spool at
          the file.
        </p>
        <p className="text-sm text-muted-foreground">
          Export with a "cookies.txt" browser extension, or run{" "}
          <code>yt-dlp --cookies-from-browser brave --cookies ~/cookies.txt --skip-download URL</code>.
          The User-Agent below must match the browser the cookies came from — a{" "}
          <code>cf_clearance</code> cookie is bound to the exact agent that earned it.
        </p>

        <div className="space-y-1.5">
          <Label>Cookies file (Netscape format)</Label>
          <Input
            value={settings.cookies_file ?? ""}
            placeholder="/home/you/cookies.txt"
            spellCheck={false}
            onChange={(e) =>
              update({ cookies_file: e.currentTarget.value.trim() || null })
            }
          />
        </div>

        <div className="space-y-1.5">
          <Label>User-Agent</Label>
          <Input
            value={settings.user_agent ?? ""}
            placeholder="(default Chrome on Linux)"
            spellCheck={false}
            onChange={(e) =>
              update({ user_agent: e.currentTarget.value.trim() || null })
            }
          />
        </div>
      </Section>

      <Section icon={<Video className="size-3.5" />} title="Video downloads (yt-dlp)">
        <p className="text-sm text-muted-foreground">
          Streaming sites (YouTube, Vimeo, and ~1800 more) are handled by{" "}
          <code>yt-dlp</code>, which must be installed. Right-click a page and choose{" "}
          <em>Download video with spool</em>, or paste a video URL — known sites are
          auto-detected.
        </p>

        <div className="space-y-1.5">
          <Label>Quality</Label>
          <Select
            value={settings.video_quality || "best"}
            onValueChange={(v) => update({ video_quality: v }, true)}
          >
            <SelectTrigger className="w-full"><SelectValue /></SelectTrigger>
            <SelectContent>
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

        <div className="grid grid-cols-2 gap-4">
          <div className="space-y-1.5">
            <Label>yt-dlp path</Label>
            <Input
              value={settings.ytdlp_path}
              placeholder="yt-dlp"
              spellCheck={false}
              onChange={(e) => update({ ytdlp_path: e.currentTarget.value.trim() })}
            />
          </div>
          <div className="space-y-1.5">
            <Label>Cookies from browser</Label>
            <Input
              value={settings.cookies_browser}
              placeholder="e.g. brave, chrome, firefox"
              spellCheck={false}
              onChange={(e) => update({ cookies_browser: e.currentTarget.value.trim() })}
            />
          </div>
        </div>
        <p className="text-sm text-muted-foreground">
          Cookies from a browser let yt-dlp fetch age-restricted or members-only
          videos. Leave blank to use the cookies file above, or nothing.
        </p>
      </Section>
    </section>
  );
}
