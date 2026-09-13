import { memo, useCallback, useDeferredValue, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { toast } from "sonner";
import {
  Archive, BookOpen, Captions, Check, CircleAlert, CircleCheckBig, Code2, Copy,
  Disc, Download, ExternalLink, File, FileText, Folder, Image, ListChecks,
  Loader2, Magnet, MoreVertical, Music, Network, Package, Pause, Pencil, Play, Plus,
  Presentation, RotateCcw, Search, Sheet, Trash2, Type as TypeIcon,
  Video, X,
} from "lucide-react";
import {
  api,
  formatBytes,
  formatEta,
  type DownloadView,
  type ConfirmRequest,
  type ProgressRow,
  type Status,
} from "./lib/api";
import { SettingsView } from "./components/SettingsView";
import { applyTheme } from "./lib/theme";
import { DetailModal } from "./components/DetailModal";
import { ConfirmDelete } from "./components/ConfirmDelete";
import { RenameDialog } from "./components/RenameDialog";
import * as selection from "./lib/selection";
import { categoryOf, kindOf, type Category, type Kind } from "./lib/filetype";
import { AddDialog } from "./components/AddDialog";
import { SpiderDialog } from "./components/SpiderDialog";
import { RingProgress } from "./components/Loaders";
import { Sidebar, type QueueFilter } from "./components/Sidebar";
import { Titlebar } from "./components/Titlebar";
import { Badge } from "./components/ui/badge";
import { Button } from "./components/ui/button";
import { Card } from "./components/ui/card";
import { Checkbox } from "./components/ui/checkbox";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "./components/ui/dropdown-menu";
import { Input } from "./components/ui/input";
import { Progress } from "./components/ui/progress";
import { Toaster } from "./components/ui/sonner";
import { cn } from "cn";
import "./App.css";

/// Smoothing factor for the speed readout. TCP delivers in bursts, so the raw
/// per-tick rate swings wildly; this keeps the number readable.
const ALPHA = 0.25;

const IS_MAC = /Mac/.test(navigator.userAgent);

type Sample = { at: number; bytes: number; speed: number };

const QUEUE_TITLE: Record<QueueFilter, string> = {
  all: "All downloads",
  active: "Active downloads",
  completed: "Completed",
  paused: "Paused",
  failed: "Failed",
};

const CATEGORY_TITLE: Record<Category | "all", string> = {
  all: "All",
  media: "Media",
  documents: "Documents",
  archives: "Archives",
  other: "Other",
};

const STATUS_LABEL: Record<Status, string> = {
  queued: "Queued",
  downloading: "Downloading",
  paused: "Paused",
  interrupted: "Interrupted",
  completed: "Completed",
  failed: "Failed",
};

const STATUS_DOT: Record<Status, string> = {
  queued: "bg-muted-foreground",
  downloading: "bg-primary",
  paused: "bg-amber-500",
  interrupted: "bg-amber-500",
  completed: "bg-emerald-500",
  failed: "bg-destructive",
};

// One tint per file kind, so the list reads by color before it reads by
// label — a shelf of genres, not a column of identical gray squares.
const KIND_STYLE: Record<Kind, string> = {
  video: "bg-violet-500/15 text-violet-600 dark:text-violet-400",
  audio: "bg-pink-500/15 text-pink-600 dark:text-pink-400",
  archive: "bg-amber-500/15 text-amber-600 dark:text-amber-400",
  image: "bg-emerald-500/15 text-emerald-600 dark:text-emerald-400",
  doc: "bg-blue-500/15 text-blue-600 dark:text-blue-400",
  sheet: "bg-green-500/15 text-green-600 dark:text-green-400",
  slides: "bg-orange-500/15 text-orange-600 dark:text-orange-400",
  book: "bg-yellow-500/15 text-yellow-700 dark:text-yellow-400",
  code: "bg-sky-500/15 text-sky-600 dark:text-sky-400",
  font: "bg-fuchsia-500/15 text-fuchsia-600 dark:text-fuchsia-400",
  subs: "bg-cyan-500/15 text-cyan-600 dark:text-cyan-400",
  disc: "bg-purple-500/15 text-purple-600 dark:text-purple-400",
  package: "bg-lime-500/15 text-lime-700 dark:text-lime-400",
  torrent: "bg-red-500/15 text-red-600 dark:text-red-400",
  file: "bg-muted text-muted-foreground",
};

function App() {
  const [rows, setRows] = useState<DownloadView[]>([]);
  const [showSettings, setShowSettings] = useState(false);
  // Two independent axes: which queue (status) and which category (file
  // kind) narrow the list. Neither excludes the other — "Failed" and
  // "Media" together means failed video/audio downloads.
  const [queue, setQueue] = useState<QueueFilter>("all");
  const [category, setCategory] = useState<Category | "all">("all");
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false);
  // Mirrors the persisted setting so the sidebar's quick cycle button can
  // read and flip it without opening Settings.
  const [theme, setTheme] = useState("system");
  const [detailId, setDetailId] = useState<string | null>(null);
  const [deleteId, setDeleteId] = useState<string | null>(null);
  const [renameId, setRenameId] = useState<string | null>(null);
  // Multi-select. Kept as ids rather than rows so it survives queue snapshots.
  // The rules (range extension, select-all, pruning) live in lib/selection.
  const [selected, setSelected] = useState<selection.Selection>(selection.EMPTY);
  // Selection is a mode, entered from the toolbar. Outside it the list carries
  // no checkboxes at all and a row click opens its details as usual.
  const [selectMode, setSelectMode] = useState(false);
  /// Every command below is fire-and-forget from a click handler, so a
  /// rejection has nowhere to go but an unhandled promise. Route them through
  /// here and the failure reaches the user as a toast instead of the console.
  const report = useCallback((e: unknown) => toast.error(String(e)), []);
  // Name filter. Narrows whatever the status pills already picked.
  const [query, setQuery] = useState("");
  const searchRef = useRef<HTMLInputElement>(null);
  // Set when the delete dialog is confirming the whole selection.
  const [deletingSelection, setDeletingSelection] = useState(false);
  // URL awaiting confirmation in the add dialog (location, quality, start).
  const [pendingUrl, setPendingUrl] = useState<string | null>(null);
  // Set when the extension parked the request; confirming replays its session.
  const [pendingToken, setPendingToken] = useState<string | null>(null);
  // Import opens the same dialog with its URL field as a list.
  const [pendingMulti, setPendingMulti] = useState(false);
  // The link grabber, which feeds its results into that same dialog.
  const [spiderOpen, setSpiderOpen] = useState(false);

  // Live bytes arrive far more often than the queue snapshot, so they are kept
  // out of React state and merged at render time.
  const live = useRef(new Map<string, number>());
  // Live total from progress events — for yt-dlp the size is only known once
  // the transfer is running, so the queue snapshot's total is null until then.
  const liveTotal = useRef(new Map<string, number>());
  const samples = useRef(new Map<string, Sample>());
  const [, forceRender] = useState(0);
  // Progress events arrive far faster than the display can paint. Firing a
  // re-render per event makes the window flicker, so coalesce to one per frame.
  const frame = useRef<number | null>(null);
  const scheduleRender = useCallback(() => {
    if (frame.current !== null) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = null;
      forceRender((n) => n + 1);
    });
  }, []);

  const refresh = useCallback(async () => {
    try {
      setRows(await api.getQueue());
    } catch (e) {
      report(e);
    }
  }, [report]);

  // Theme is stored in settings, so apply the saved choice on startup.
  useEffect(() => {
    api.getSettings().then((s) => {
      applyTheme(s.theme);
      setTheme(s.theme || "system");
    }).catch(() => {});
  }, []);

  // Auto-collapse sidebar on compact displays (< 768px) to maximize download workspace
  useEffect(() => {
    const handleResize = () => {
      if (window.innerWidth < 768) {
        setSidebarCollapsed(true);
      }
    };
    if (window.innerWidth < 768) {
      setSidebarCollapsed(true);
    }
    window.addEventListener("resize", handleResize);
    return () => window.removeEventListener("resize", handleResize);
  }, []);

  /// Quick-access cycle from the sidebar: system → light → dark → system.
  /// Settings has the full picker; this is just the fast path.
  const cycleTheme = useCallback(() => {
    setTheme((current) => {
      const next = current === "system" ? "light" : current === "light" ? "dark" : "system";
      applyTheme(next);
      api.getSettings().then((s) => api.updateSettings({ ...s, theme: next })).catch(() => {});
      return next;
    });
  }, []);

  useEffect(() => {
    refresh();
    const unlistenQueue = listen<DownloadView[]>("queue://changed", (e) => {
      setRows(e.payload);
      // Drop per-download tracking for rows that no longer exist, or these maps
      // grow for the life of the session.
      const alive = new Set(e.payload.map((r) => r.id));
      for (const map of [live.current, liveTotal.current, samples.current]) {
        for (const id of map.keys()) if (!alive.has(id)) map.delete(id);
      }
    });
    // The extension can ask the app to confirm a capture before queuing it.
    const unlistenConfirm = listen<ConfirmRequest>("download://confirm", (e) => {
      // A second capture while a dialog is open replaces it; release the one
      // being dropped so it is not parked on the backend forever.
      setPendingToken((old) => {
        if (old && old !== e.payload.token) api.cancelPending(old).catch(() => {});
        return e.payload.token;
      });
      setPendingUrl(e.payload.url);
    });
    const unlistenProgress = listen<ProgressRow>("download://progress", (event) => {
      const { id, downloaded, total } = event.payload;
      live.current.set(id, downloaded);
      if (total) liveTotal.current.set(id, total);
      const now = performance.now();
      const prev = samples.current.get(id);
      let speed = prev?.speed ?? 0;
      if (prev) {
        const dt = (now - prev.at) / 1000;
        if (dt > 0.05) {
          const instant = Math.max(0, downloaded - prev.bytes) / dt;
          speed = prev.speed === 0 ? instant : prev.speed * (1 - ALPHA) + instant * ALPHA;
        }
      }
      samples.current.set(id, { at: now, bytes: downloaded, speed });
      scheduleRender();
    });
    return () => {
      unlistenQueue.then((fn) => fn());
      unlistenConfirm.then((fn) => fn());
      unlistenProgress.then((fn) => fn());
      if (frame.current !== null) cancelAnimationFrame(frame.current);
    };
  }, [refresh, scheduleRender]);

  // Drop ids that have left the queue, so a stale selection cannot act on
  // entries that no longer exist or keep the selection bar open over nothing.
  useEffect(() => {
    setSelected((prev) => selection.prune(prev, rows.map((r) => r.id)));
  }, [rows]);

  const active = rows.filter((r) => r.status === "downloading" || r.status === "queued");
  const completed = rows.filter((r) => r.status === "completed");
  const paused = rows.filter((r) => r.status === "paused" || r.status === "interrupted");
  const failed = rows.filter((r) => r.status === "failed");
  // Everything Resume all would act on: stopped, but not finished.
  const stopped = rows.filter(
    (r) => r.status === "paused" || r.status === "interrupted" || r.status === "failed",
  );
  const downloading = active.filter((r) => r.status === "downloading");
  const totalSpeed = downloading.reduce((s, r) => s + (samples.current.get(r.id)?.speed ?? 0), 0);

  const queueRows = useMemo(() => {
    switch (queue) {
      case "active": return active;
      case "completed": return completed;
      case "paused": return paused;
      case "failed": return failed;
      default: return rows;
    }
    // active/completed/paused/failed are derived fresh from `rows` every
    // render, so depending on `rows` alone keeps this in sync with them.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [queue, rows]);

  const categoryRows = useMemo(() => {
    if (category === "all") return rows;
    return rows.filter((r) => categoryOf(r.filename) === category);
  }, [category, rows]);

  // Sidebar badge counts are contextual:
  // - Queue counts reflect items within the selected category.
  // - Category counts reflect items within the selected queue status.
  // When either filter is "all", it seamlessly displays the total counts.
  const queueCounts: Record<QueueFilter, number> = useMemo(() => ({
    all: categoryRows.length,
    active: categoryRows.filter((r) => r.status === "downloading" || r.status === "queued").length,
    completed: categoryRows.filter((r) => r.status === "completed").length,
    paused: categoryRows.filter((r) => r.status === "paused" || r.status === "interrupted").length,
    failed: categoryRows.filter((r) => r.status === "failed").length,
  }), [categoryRows]);

  const categoryCounts: Record<Category | "all", number> = useMemo(() => ({
    all: queueRows.length,
    media: queueRows.filter((r) => categoryOf(r.filename) === "media").length,
    documents: queueRows.filter((r) => categoryOf(r.filename) === "documents").length,
    archives: queueRows.filter((r) => categoryOf(r.filename) === "archives").length,
    other: queueRows.filter((r) => categoryOf(r.filename) === "other").length,
  }), [queueRows]);

  const sidebarCounts = { ...queueCounts, ...categoryCounts };

  const deferredQuery = useDeferredValue(query);

  const visible = useMemo(() => {
    const byCategory = category === "all"
      ? queueRows
      : queueRows.filter((r) => categoryOf(r.filename) === category);
    const q = deferredQuery.trim().toLowerCase();
    if (!q) return byCategory;
    // Match the URL too: a name that came back as "download.bin" is often only
    // findable by where it came from.
    return byCategory.filter(
      (r) => r.filename.toLowerCase().includes(q) || r.url.toLowerCase().includes(q),
    );
  }, [queueRows, category, deferredQuery]);

  // Look these up from the current rows each render so the open modals reflect
  // live status; close automatically if the entry is gone.
  const detailRow = detailId ? rows.find((r) => r.id === detailId) ?? null : null;
  const deleteRow = deleteId ? rows.find((r) => r.id === deleteId) ?? null : null;
  const renameRow = renameId ? rows.find((r) => r.id === renameId) ?? null : null;

  const selectedRows = visible.filter((r) => selected.ids.has(r.id));
  const selectedIds = selectedRows.map((r) => r.id);
  const allVisibleSelected = visible.length > 0 && selectedRows.length === visible.length;
  const canPause = selectedRows.some((r) => r.status === "downloading" || r.status === "queued");
  const canResume = selectedRows.some(
    (r) => r.status === "paused" || r.status === "interrupted" || r.status === "failed",
  );

  // Memoized so handleSelect stays stable and memo(Row) can skip idle rows.
  const order = useMemo(() => visible.map((r) => r.id), [visible]);

  function toggleAll() {
    setSelected((prev) => selection.toggleAll(prev, order));
  }

  function clearSelection() {
    setSelected(selection.EMPTY);
  }

  const handleOpen = useCallback((id: string) => setDetailId(id), []);
  const handleDelete = useCallback((id: string) => setDeleteId(id), []);
  const handleRename = useCallback((id: string) => setRenameId(id), []);
  const handleSelect = useCallback(
    (id: string, extend: boolean) => {
      setSelected((prev) => selection.toggle(prev, order, id, extend));
    },
    [order],
  );

  /// Leaving the mode drops the selection with it: a hidden selection that
  /// reappears next time you enter would act on rows nobody remembers picking.
  const exitSelectMode = useCallback(() => {
    setSelectMode(false);
    setSelected(selection.EMPTY);
  }, []);

  const modalOpen = Boolean(
    detailId || deleteId || renameId || pendingUrl !== null || deletingSelection,
  );

  // Window-level shortcuts. Nothing fires while a modal is up — each dialog
  // owns its own keys — and the list keys stay out of the way while the caret
  // is in a text field.
  //
  // Held in a ref and bound once: the handler closes over live rows, which
  // change several times a second while downloading, and re-subscribing a
  // window listener that often is pure waste.
  const onKeyRef = useRef<(e: KeyboardEvent) => void>(() => {});
  onKeyRef.current = (e: KeyboardEvent) => {
    if (modalOpen) return;
    {
      const el = e.target as HTMLElement | null;
      const typing = el?.tagName === "INPUT" || el?.tagName === "TEXTAREA" || el?.isContentEditable;
      const mod = e.ctrlKey || e.metaKey;

      // Toggle sidebar shortcut (Ctrl+B / Cmd+B)
      if (mod && (e.key === "b" || e.key === "B")) {
        e.preventDefault();
        setSidebarCollapsed((c) => !c);
        return;
      }

      // Focus the filter. Works from anywhere, typing included.
      if (mod && e.key === "f") {
        e.preventDefault();
        searchRef.current?.focus();
        searchRef.current?.select();
        return;
      }

      if (e.key === "Escape") {
        // Clear the filter first, leave the mode second: one Escape per
        // thing to undo, in the order they were set.
        if (typing && el === searchRef.current && query) {
          setQuery("");
          return;
        }
        if (selectMode) exitSelectMode();
        return;
      }

      if (typing) return;

      if (mod && e.key === "a" && selectMode) {
        e.preventDefault();
        setSelected((prev) => selection.toggleAll(prev, visible.map((r) => r.id)));
        return;
      }

      if ((e.key === "Delete" || e.key === "Backspace") && selectMode && selectedIds.length) {
        e.preventDefault();
        setDeletingSelection(true);
      }
    }
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => onKeyRef.current(e);
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const narrowed = queue !== "all" || category !== "all";
  const title = category === "all" ? QUEUE_TITLE[queue] : `${QUEUE_TITLE[queue]} · ${CATEGORY_TITLE[category]}`;

  return (
    <div className="relative flex h-screen flex-col overflow-hidden">
      {/* Ambient glow. Purely decorative, so hidden from assistive tech and
          pinned behind everything else. Gentle pastel sheen in light mode, luminous in dark mode. */}
      <div aria-hidden="true" className="pointer-events-none fixed -top-40 -left-40 -z-10 size-[28rem] rounded-full bg-primary/8 dark:bg-primary/25 blur-3xl" />
      <div aria-hidden="true" className="pointer-events-none fixed top-1/3 -right-40 -z-10 size-96 rounded-full bg-violet-500/6 dark:bg-violet-500/20 blur-3xl" />
      <div aria-hidden="true" className="pointer-events-none fixed -bottom-40 left-1/4 -z-10 size-96 rounded-full bg-fuchsia-500/4 dark:bg-fuchsia-500/10 blur-3xl" />

      <Toaster />
      <Titlebar
        onToggleSidebar={() => setSidebarCollapsed((c) => !c)}
        sidebarCollapsed={sidebarCollapsed}
      />

      <div className="flex flex-1 overflow-hidden">
      <Sidebar
        queue={queue}
        onQueue={(q) => { setQueue(q); if (q === "all") setCategory("all"); setShowSettings(false); }}
        category={category}
        onCategory={(c) => { setCategory(c); setShowSettings(false); }}
        counts={sidebarCounts}
        totalSpeed={totalSpeed}
        showSettings={showSettings}
        onToggleSettings={() => setShowSettings((s) => !s)}
        theme={theme}
        onCycleTheme={cycleTheme}
        collapsed={sidebarCollapsed}
      />

      <main className="flex-1 overflow-y-auto px-2.5 sm:px-4 py-3 sm:py-4">
        {showSettings ? (
          <SettingsView />
        ) : (
          <div className="mx-auto max-w-5xl w-full">
            <div className="flex flex-wrap sm:flex-nowrap items-start sm:items-center justify-between gap-2.5 sm:gap-3">
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <h1 className="text-base sm:text-lg font-semibold truncate">{title}</h1>
                  {narrowed && (
                    <Button
                      variant="ghost"
                      size="xs"
                      className="h-5 gap-1 rounded-full px-2 text-[11px] text-muted-foreground hover:text-foreground shrink-0"
                      onClick={() => { setQueue("all"); setCategory("all"); }}
                      title="Clear filter"
                    >
                      <X className="size-3" /> Reset
                    </Button>
                  )}
                </div>
                <p className="text-xs sm:text-sm text-muted-foreground">
                  {visible.length} {visible.length === 1 ? "item" : "items"}
                </p>
              </div>
              {/* Global queue controls: these act on everything, not just what
                  the current queue/category filter shows. */}
              <div className="flex items-center gap-1 sm:gap-1.5 shrink-0">
                <Button
                  variant="outline"
                  size="sm"
                  className="h-8 px-2 sm:px-3 text-xs sm:text-sm"
                  onClick={() => api.pauseAll().catch(report)}
                  disabled={!active.length}
                  title="Pause all downloads"
                >
                  <Pause className="size-3.5" />
                  <span className="hidden md:inline">Pause all</span>
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  className="h-8 px-2 sm:px-3 text-xs sm:text-sm"
                  onClick={() => api.resumeAll().catch(report)}
                  disabled={!stopped.length}
                  title="Resume all downloads"
                >
                  <Play className="size-3.5" />
                  <span className="hidden md:inline">Resume all</span>
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  className={cn(
                    "h-8 px-2 sm:px-3 text-xs sm:text-sm",
                    selectMode && "border-primary/40 bg-primary/10 text-primary hover:bg-primary/15 hover:text-primary"
                  )}
                  onClick={() => (selectMode ? exitSelectMode() : setSelectMode(true))}
                  disabled={rows.length === 0}
                  title={selectMode ? "Leave selection mode (Esc)" : "Pick several downloads to act on at once"}
                >
                  <ListChecks className="size-3.5" />
                  <span className="hidden sm:inline">Select</span>
                </Button>
                {completed.length + failed.length > 0 && (
                  <Button
                    variant="ghost"
                    size="sm"
                    className="h-8 px-2 sm:px-3 text-xs sm:text-sm"
                    onClick={() => api.clearHistory().catch(report)}
                    title="Clear finished downloads"
                  >
                    <span className="hidden sm:inline">Clear finished</span>
                    <span className="sm:hidden">Clear</span>
                  </Button>
                )}
              </div>
            </div>

            {/* The widest field on the screen belongs to the thing done most
                often. Adding is a deliberate act with several answers to give,
                so it opens the dialog that asks for them. One elevated bar
                rather than three loose controls floating on the page. */}
            <div className="mt-3 flex items-center gap-1 rounded-xl border border-border/80 bg-card/90 dark:bg-card/60 p-1.5 shadow-xs dark:shadow-sm backdrop-blur-sm transition-all focus-within:border-primary/50 focus-within:ring-2 focus-within:ring-primary/10">
              <div className="relative flex-1 min-w-0">
                <Search className="pointer-events-none absolute left-2.5 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" />
                <Input
                  ref={searchRef}
                  value={query}
                  onChange={(e) => setQuery(e.currentTarget.value)}
                  placeholder="Search downloads…"
                  spellCheck={false}
                  aria-label="Filter downloads"
                  autoFocus
                  className="border-0 bg-transparent pl-8 pr-12 sm:pr-16 shadow-none focus-visible:ring-0 text-sm"
                />
                <div className="absolute right-1.5 top-1/2 -translate-y-1/2 flex items-center gap-1">
                  {query ? (
                    <Button
                      type="button"
                      variant="ghost"
                      size="icon-xs"
                      onClick={() => { setQuery(""); searchRef.current?.focus(); }}
                      title="Clear filter"
                    >
                      <X className="size-3.5" />
                    </Button>
                  ) : (
                    <kbd className="hidden md:inline-flex h-4.5 select-none items-center gap-0.5 rounded border border-border/80 bg-muted/60 px-1.5 font-mono text-[10px] font-medium text-muted-foreground">
                      {IS_MAC ? <span className="text-[11px]">⌘</span> : "Ctrl+"}F
                    </kbd>
                  )}
                </div>
              </div>
              <div className="h-5 w-px shrink-0 bg-border" />
              {/* Both ways of bringing a download in, side by side and spelled
                  out. The toolbar keeps only what acts on the whole queue. */}
              <Button
                variant="ghost"
                type="button"
                className="shrink-0 rounded-lg h-8 px-2 sm:px-3 text-xs sm:text-sm"
                title="Grab every file a page links to"
                onClick={() => setSpiderOpen(true)}
              >
                <Network className="size-3.5" />
                <span className="hidden sm:inline">Grab</span>
              </Button>
              <Button
                variant="ghost"
                type="button"
                className="shrink-0 rounded-lg h-8 px-2 sm:px-3 text-xs sm:text-sm"
                title="Import URLs"
                onClick={() => { setPendingMulti(true); setPendingUrl(""); }}
              >
                <Download className="size-3.5" />
                <span className="hidden sm:inline">Import</span>
              </Button>
              {/* A plus, not another arrow: Import brings a file in, Add makes
                  a new entry, and the brand already owns the download glyph. */}
              <Button
                type="button"
                className="shrink-0 rounded-lg h-8 px-2.5 sm:px-3 text-xs sm:text-sm bg-gradient-to-r from-primary to-violet-500 shadow-sm transition-shadow hover:opacity-90 hover:shadow-md"
                title="Add download"
                onClick={() => { setPendingMulti(false); setPendingUrl(""); }}
              >
                <Plus className="size-3.5" />
                <span className="hidden sm:inline">Add</span>
              </Button>
            </div>

            {/* A floating glass ribbon that takes over once a selection
                starts, rather than a bar of its own that pushes the list
                down every time selection mode is entered. */}
            {selectMode && (
              <div className="mt-3 flex min-h-9 flex-wrap items-center justify-between gap-2 rounded-xl border border-border/80 bg-card/90 dark:bg-card/80 px-3 py-1 shadow-xs dark:shadow-sm backdrop-blur-sm">
                <div className="flex items-center gap-2">
                  <Checkbox
                    checked={allVisibleSelected ? true : selectedRows.length > 0 ? "indeterminate" : false}
                    onCheckedChange={toggleAll}
                    disabled={visible.length === 0}
                    title={allVisibleSelected ? "Deselect all" : "Select all"}
                  />
                  <span className="text-xs sm:text-sm text-muted-foreground font-medium">{selectedRows.length} selected</span>
                </div>
                <div className="flex items-center gap-1 sm:gap-1.5">
                  <Button size="sm" variant="outline" className="h-7 px-2 text-xs" onClick={() => api.bulk(selectedIds, "pause").catch(report)} disabled={!canPause} title="Pause selected">
                    <Pause className="size-3" />
                    <span className="hidden sm:inline">Pause</span>
                  </Button>
                  <Button size="sm" variant="outline" className="h-7 px-2 text-xs" onClick={() => api.bulk(selectedIds, "resume").catch(report)} disabled={!canResume} title="Resume selected">
                    <Play className="size-3" />
                    <span className="hidden sm:inline">Resume</span>
                  </Button>
                  <Button size="sm" variant="destructive" className="h-7 px-2 text-xs" onClick={() => setDeletingSelection(true)} disabled={selectedRows.length === 0} title="Remove selected">
                    <Trash2 className="size-3" />
                    <span className="hidden sm:inline">Remove</span>
                  </Button>
                  <Button size="sm" variant="ghost" className="h-7 px-2 text-xs" onClick={exitSelectMode} title="Leave selection mode (Esc)">
                    Done
                  </Button>
                </div>
              </div>
            )}

            <div className="mt-3 space-y-2">
              {visible.map((row) => (
                <Row
                  key={row.id}
                  row={row}
                  liveBytes={live.current.get(row.id)}
                  liveTotal={liveTotal.current.get(row.id)}
                  speed={samples.current.get(row.id)?.speed ?? 0}
                  onOpen={handleOpen}
                  onDelete={handleDelete}
                  onRename={handleRename}
                  onFail={report}
                  selectMode={selectMode}
                  selected={selected.ids.has(row.id)}
                  onSelect={handleSelect}
                />
              ))}
              {visible.length === 0 && (
                <div className="flex flex-col items-center gap-3 py-20 text-center">
                  <span className="flex size-16 items-center justify-center rounded-full bg-gradient-to-br from-primary/20 to-violet-500/20 ring-8 ring-primary/5">
                    <Download className="size-7 text-primary" />
                  </span>
                  <p className="font-medium">
                    {query
                      ? "Nothing matches that"
                      : narrowed && rows.length > 0
                        ? "No downloads match this filter"
                        : "No downloads yet"}
                  </p>
                  <p className="max-w-sm text-sm text-muted-foreground">
                    {query ? (
                      <>
                        No download matches <strong>{query}</strong>. Clear the filter, or try
                        part of a URL.
                      </>
                    ) : narrowed && rows.length > 0 ? (
                      <>
                        There {rows.length === 1 ? "is" : "are"}{" "}
                        <strong>
                          {rows.length} {rows.length === 1 ? "download" : "downloads"}
                        </strong>{" "}
                        in other categories or queues.
                      </>
                    ) : (
                      <>
                        Hit <strong>Add</strong> to paste a link, <strong>Import</strong> for a
                        list of them, or right-click any link in your browser and choose{" "}
                        <strong>Download with spool</strong>.
                      </>
                    )}
                  </p>
                  {query ? (
                    <Button
                      variant="outline"
                      size="sm"
                      className="mt-2"
                      onClick={() => setQuery("")}
                    >
                      Clear search
                    </Button>
                  ) : narrowed && rows.length > 0 ? (
                    <Button
                      variant="outline"
                      size="sm"
                      className="mt-2"
                      onClick={() => {
                        setQueue("all");
                        setCategory("all");
                        setQuery("");
                      }}
                    >
                      Show all downloads ({rows.length})
                    </Button>
                  ) : (
                    <div className="mt-2 flex items-center gap-2">
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => { setPendingMulti(false); setPendingUrl(""); }}
                      >
                        <Plus /> Add download
                      </Button>
                      <Button
                        variant="ghost"
                        size="sm"
                        onClick={() => { setPendingMulti(true); setPendingUrl(""); }}
                      >
                        <Download /> Import batch
                      </Button>
                    </div>
                  )}
                </div>
              )}
            </div>
          </div>
        )}
      </main>
      </div>

      {detailRow && (
        <DetailModal
          row={detailRow}
          liveBytes={live.current.get(detailRow.id)}
          liveTotal={liveTotal.current.get(detailRow.id)}
          speed={samples.current.get(detailRow.id)?.speed ?? 0}
          onClose={() => setDetailId(null)}
        />
      )}
      {deleteRow && (
        <ConfirmDelete rows={[deleteRow]} onClose={() => setDeleteId(null)} />
      )}
      {deletingSelection && selectedRows.length > 0 && (
        <ConfirmDelete
          rows={selectedRows}
          onClose={() => {
            setDeletingSelection(false);
            clearSelection();
          }}
        />
      )}
      {renameRow && (
        <RenameDialog row={renameRow} onClose={() => setRenameId(null)} />
      )}
      {spiderOpen && (
        <SpiderDialog
          onClose={() => setSpiderOpen(false)}
          onFound={(links) => {
            setPendingToken(null);
            setPendingMulti(true);
            setPendingUrl(links.join("\n"));
            toast.success(`Found ${links.length} links.`);
          }}
        />
      )}
      {pendingUrl !== null && (
        <AddDialog
          url={pendingUrl}
          multi={pendingMulti}
          token={pendingToken}
          onClose={() => {
            setPendingUrl(null);
            setPendingToken(null);
            setPendingMulti(false);
          }}
          onAdded={(msg) => {
            if (msg) toast.success(msg);
          }}
        />
      )}
    </div>
  );
}

/// The poster frame for a finished video, fetched once.
///
/// yt-dlp supplies a thumbnail URL for the sites it knows; this covers
/// everything else — a plain .mp4, or a stream pulled from a manifest — by
/// taking a frame from the file on disk. Null until it arrives, and null
/// forever if ffmpeg is not installed or cannot read the file, in which case
/// the row keeps its type icon.
function usePoster(row: DownloadView): string | null {
  const [poster, setPoster] = useState<string | null>(null);

  useEffect(() => {
    // Only ask once the file exists and only when there is nothing better.
    if (row.status !== "completed" || row.thumbnail || kindOf(row.filename) !== "video") {
      setPoster(null);
      return;
    }
    let live = true;
    api
      .videoThumbnail(row.id)
      .then((data) => { if (live) setPoster(data); })
      .catch(() => { /* no poster is a fine outcome */ });
    return () => { live = false; };
  }, [row.id, row.status, row.thumbnail, row.filename]);

  return poster;
}

/// The download's origin, for a small provenance pill next to the filename.
/// Null for anything unparseable rather than showing the raw URL.
function hostnameOf(url: string): string | null {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return null;
  }
}

const Row = memo(function Row({
  row, liveBytes, liveTotal, speed, onOpen, onDelete, onRename,
  selectMode, selected, onSelect, onFail,
}: {
  row: DownloadView;
  liveBytes?: number;
  liveTotal?: number;
  speed: number;
  onOpen: (id: string) => void;
  onDelete: (id: string) => void;
  onRename: (id: string) => void;
  selectMode: boolean;
  selected: boolean;
  onSelect: (id: string, extend: boolean) => void;
  /// Surfaces a failed command; a click handler has nowhere else to put one.
  onFail: (e: unknown) => void;
}) {
  const downloaded = row.status === "downloading" && liveBytes !== undefined ? liveBytes : row.downloaded;
  // yt-dlp size is only known once running, so fall back to the live total.
  const total = row.total ?? (row.status === "downloading" ? liveTotal ?? null : null);
  const percent = total ? Math.min(100, (downloaded / total) * 100) : null;
  const remaining = total ? total - downloaded : 0;
  const eta = row.status === "downloading" && speed > 0 ? formatEta(remaining / speed) : "";
  // Only slide the placeholder bar when bytes are actually moving with no known
  // size. While a download is still resolving (yt-dlp probing, nothing
  // transferred yet) the bar sits at 0 rather than pretending to work.
  const indeterminate = percent === null && downloaded > 0;
  const running = row.status === "downloading";
  const kind = fileKind(row.filename);
  const domain = hostnameOf(row.url);
  const poster = usePoster(row);
  const preview = row.thumbnail ?? poster;

  return (
    <Card
      className={cn(
        "flex-row items-center gap-2.5 sm:gap-3 bg-card/95 dark:bg-card/75 p-2.5 sm:p-3 backdrop-blur-sm transition-all shadow-[0_1px_3px_rgba(0,0,0,0.05),0_1px_2px_rgba(0,0,0,0.02)] border-border/80 hover:-translate-y-0.5 hover:shadow-[0_8px_20px_-4px_rgba(0,0,0,0.08)] dark:hover:shadow-primary/5",
        selected ? "border-primary bg-accent/30 dark:bg-accent/40 ring-1 ring-primary/25" : "hover:border-primary/50",
        selectMode && "cursor-pointer",
      )}
      // In selection mode the whole row is the target, so the tile is an
      // indicator rather than the only thing you can hit.
      onClick={selectMode ? (e) => onSelect(row.id, e.shiftKey) : undefined}
    >
      {/* Selection has no column of its own: a picked row swaps its file-type
          tile for a filled check, so entering the mode never re-flows the row
          and an idle list carries no controls at all. */}
      <div className="relative flex size-9 sm:size-10 shrink-0 items-center justify-center">
        {selected ? (
          <span className="flex size-9 sm:size-10 items-center justify-center rounded-md bg-primary text-primary-foreground" role="img" aria-label="Selected">
            <Check className="size-4 sm:size-5" />
          </span>
        ) : preview ? (
          // Video preview thumbnail; overlay a ring while downloading.
          <>
            <img
              className="size-9 sm:size-10 rounded-md object-cover"
              src={preview}
              alt=""
              loading="lazy"
              onError={(e) => (e.currentTarget.style.display = "none")}
            />
            {running && percent !== null && (
              <span className="absolute -right-1 -bottom-1 rounded-full bg-background">
                <RingProgress percent={percent} size={18} />
              </span>
            )}
          </>
        ) : running && percent !== null ? (
          <span className="flex size-9 sm:size-10 items-center justify-center rounded-md bg-muted">
            <RingProgress percent={percent} size={32} />
          </span>
        ) : running ? (
          // Unknown size: no percentage to show, so a quiet spinner.
          <span className="flex size-9 sm:size-10 items-center justify-center rounded-md bg-muted">
            <Loader2 className="size-4 sm:size-5 animate-spin text-primary" />
          </span>
        ) : (
          <span className={cn("flex size-9 sm:size-10 items-center justify-center rounded-md", kind.className)}>
            {kind.icon}
          </span>
        )}
      </div>

      {/* A div with a click handler is invisible to the keyboard, so this
          carries the button role, a tab stop and the keys that go with it.
          In selection mode the row itself owns the click and this is inert. */}
      <div
        className={cn("min-w-0 flex-1", !selectMode && "cursor-pointer")}
        onClick={selectMode ? undefined : () => onOpen(row.id)}
        title={selectMode ? undefined : "View details"}
        role={selectMode ? undefined : "button"}
        tabIndex={selectMode ? undefined : 0}
        onKeyDown={
          selectMode
            ? undefined
            : (e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  onOpen(row.id);
                }
              }
        }
      >
        <div className="flex items-center justify-between gap-1.5 sm:gap-2">
          <span className="flex min-w-0 items-center gap-1.5">
            <span className="truncate text-xs sm:text-sm font-medium" title={row.url}>{row.filename}</span>
            {domain && (
              <span className="hidden xs:inline-block shrink-0 truncate rounded-full border border-border/60 bg-muted/70 px-1.5 sm:px-2 py-0.5 text-[9px] sm:text-[10px] font-mono text-muted-foreground max-w-24 sm:max-w-36">
                {domain}
              </span>
            )}
            {row.status === "completed" && (
              <CircleCheckBig className="size-3.5 shrink-0 text-emerald-500 sm:hidden" />
            )}
            {row.status === "failed" && (
              <CircleAlert className="size-3.5 shrink-0 text-destructive sm:hidden" />
            )}
          </span>
          {row.status !== "completed" && row.status !== "failed" && (
            <span className={cn("size-2 shrink-0 rounded-full", STATUS_DOT[row.status])} title={STATUS_LABEL[row.status]} />
          )}
        </div>

        {row.status !== "completed" && (
          <Progress
            value={indeterminate ? 40 : percent ?? 0}
            className={cn("mt-1.5 h-1.5", indeterminate && "animate-pulse")}
          />
        )}

        <div className="mt-1 flex flex-wrap items-center justify-between gap-x-2 gap-y-0.5 text-[11px] sm:text-xs text-muted-foreground">
          <span className="font-mono tabular-nums">
            {formatBytes(downloaded)}
            {total ? ` / ${formatBytes(total)}` : " · unknown size"}
          </span>
          <span className="flex items-center gap-1.5 sm:gap-2 font-mono tabular-nums">
            {running && (
              <>
                <span className="font-medium text-primary">↓ {formatBytes(speed)}/s</span>
                {eta && <span>{eta} left</span>}
                {row.segments > 1 && <span className="hidden xs:inline">{row.segments} conns</span>}
              </>
            )}
            {(row.status === "queued" || row.status === "interrupted") && (
              <span className="flex items-center gap-1">{STATUS_LABEL[row.status]} <Loader2 className="size-3 animate-spin" /></span>
            )}
            {row.status === "paused" && <span>{STATUS_LABEL[row.status]}</span>}
          </span>
        </div>

        {row.error && <p className="mt-1 truncate text-xs text-destructive">{row.error}</p>}
      </div>

      {/* A Card-level sibling, not part of the text block above — so it
          centers against the whole row exactly like the action icons next
          to it, instead of sitting wherever it lands inside a stack of
          text lines. */}
      {row.status === "completed" && (
        <Badge variant="secondary" className="shrink-0 hidden sm:inline-flex gap-1 border border-emerald-500/20 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400 text-[11px]">
          <CircleCheckBig className="size-3" /> Completed
        </Badge>
      )}
      {row.status === "failed" && (
        <Badge variant="secondary" className="shrink-0 hidden sm:inline-flex gap-1 border border-destructive/20 bg-destructive/10 text-destructive text-[11px]">
          <CircleAlert className="size-3" /> Failed
        </Badge>
      )}

      {/* Row actions must not toggle the row underneath them in selection
          mode; each button already handles its own click. */}
      <div className="flex items-center gap-0.5 sm:gap-1" onClick={(e) => e.stopPropagation()}>
        {(row.status === "downloading" || row.status === "queued") && (
          <Button variant="ghost" size="icon" className="size-8" title="Pause" onClick={() => api.pause(row.id).catch(onFail)}><Pause className="size-4" /></Button>
        )}
        {(row.status === "paused" || row.status === "interrupted") && (
          <Button variant="ghost" size="icon" className="size-8" title="Resume" onClick={() => api.resume(row.id).catch(onFail)}><Play className="size-4" /></Button>
        )}
        {row.status === "failed" && (
          <Button variant="ghost" size="icon" className="size-8" title="Retry" onClick={() => api.retry(row.id).catch(onFail)}><RotateCcw className="size-4" /></Button>
        )}
        {row.status === "completed" && (
          <Button variant="ghost" size="icon" className="size-8" title="Open file" onClick={() => api.openFile(row.path).catch(onFail)}><ExternalLink className="size-4" /></Button>
        )}
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" size="icon" className="size-8" title="More options">
              <MoreVertical className="size-4" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            {row.status === "completed" && (
              <DropdownMenuItem onClick={() => api.revealFile(row.path).catch(onFail)}>
                <Folder /> Open folder
              </DropdownMenuItem>
            )}
            <DropdownMenuItem onClick={() => navigator.clipboard.writeText(row.url)}>
              <Copy /> Copy URL
            </DropdownMenuItem>
            {/* A running transfer holds its `.part` open, so renaming needs a
                pause first — hide the option rather than offer a guaranteed
                error. */}
            {row.status !== "downloading" && (
              <DropdownMenuItem onClick={() => onRename(row.id)}>
                <Pencil /> Rename
              </DropdownMenuItem>
            )}
            <DropdownMenuSeparator />
            <DropdownMenuItem variant="destructive" onClick={() => onDelete(row.id)}>
              <Trash2 /> Remove
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
    </Card>
  );
});

/// Glyph per kind. The kind itself comes from lib/filetype, which is where the
/// extension table lives and is tested.
const KIND_ICON: Record<Kind, React.ReactNode> = {
  video: <Video className="size-5" />,
  audio: <Music className="size-5" />,
  archive: <Archive className="size-5" />,
  image: <Image className="size-5" />,
  doc: <FileText className="size-5" />,
  sheet: <Sheet className="size-5" />,
  slides: <Presentation className="size-5" />,
  book: <BookOpen className="size-5" />,
  code: <Code2 className="size-5" />,
  font: <TypeIcon className="size-5" />,
  subs: <Captions className="size-5" />,
  disc: <Disc className="size-5" />,
  package: <Package className="size-5" />,
  torrent: <Magnet className="size-5" />,
  file: <File className="size-5" />,
};

function fileKind(name: string): { icon: React.ReactNode; className: string } {
  const kind = kindOf(name);
  return { icon: KIND_ICON[kind], className: KIND_STYLE[kind] };
}

export default App;
