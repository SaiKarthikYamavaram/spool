import {
  Archive, CircleAlert, CircleCheckBig, FileText, Inbox, LayoutGrid, Monitor, Moon,
  MoreHorizontal, Pause, Settings, Sun, Video, Zap,
} from "lucide-react";
import type { Category } from "../lib/filetype";
import { formatBytes } from "../lib/api";
import { Button } from "./ui/button";
import { Logo } from "./Logo";
import { cn } from "cn";

export type QueueFilter = "all" | "active" | "completed" | "paused" | "failed";

const QUEUES: { id: QueueFilter; label: string; icon: React.ReactNode }[] = [
  { id: "all", label: "All", icon: <Inbox className="size-4" /> },
  { id: "active", label: "Active", icon: <Zap className="size-4" /> },
  { id: "completed", label: "Completed", icon: <CircleCheckBig className="size-4" /> },
  { id: "paused", label: "Paused", icon: <Pause className="size-4" /> },
  { id: "failed", label: "Failed", icon: <CircleAlert className="size-4" /> },
];

const CATEGORIES: { id: Category | "all"; label: string; icon: React.ReactNode }[] = [
  { id: "all", label: "All", icon: <LayoutGrid className="size-4" /> },
  { id: "media", label: "Media", icon: <Video className="size-4" /> },
  { id: "documents", label: "Documents", icon: <FileText className="size-4" /> },
  { id: "archives", label: "Archives", icon: <Archive className="size-4" /> },
  { id: "other", label: "Other", icon: <MoreHorizontal className="size-4" /> },
];

const THEME_ICON: Record<string, React.ReactNode> = {
  system: <Monitor className="size-4" />,
  light: <Sun className="size-4" />,
  dark: <Moon className="size-4" />,
};

/// Left navigation rail: which queue and which category narrow the list, plus
/// the always-visible network HUD and app-level toggles. Collapses to an
/// icon-only strip so it costs little width when the window is narrow.
export function Sidebar({
  queue, onQueue, category, onCategory, counts, totalSpeed,
  showSettings, onToggleSettings, theme, onCycleTheme,
  collapsed,
}: {
  queue: QueueFilter;
  onQueue: (q: QueueFilter) => void;
  category: Category | "all";
  onCategory: (c: Category | "all") => void;
  counts: Record<QueueFilter, number> & Record<Category | "all", number>;
  totalSpeed: number;
  showSettings: boolean;
  onToggleSettings: () => void;
  theme: string;
  onCycleTheme: () => void;
  collapsed: boolean;
}) {
  return (
    <aside
      className={cn(
        "flex shrink-0 flex-col border-r border-border/60 bg-background/85 transition-all duration-200 ease-in-out",
        collapsed ? "w-14" : "w-52",
      )}
    >
      <div className={cn("flex items-center gap-2 px-3 py-3", collapsed && "justify-center px-0")}>
        <span className="flex size-8 shrink-0 items-center justify-center rounded-full bg-gradient-to-br from-primary to-violet-500 text-primary-foreground ring-4 ring-primary/10">
          <Logo size={17} />
        </span>
        {!collapsed && (
          <span className="bg-gradient-to-r from-primary to-violet-500 bg-clip-text text-[15px] font-bold tracking-tight text-transparent">
            spool
          </span>
        )}
      </div>

      <nav className="flex-1 space-y-3 sm:space-y-4 overflow-y-auto px-2 pb-2">
        <div className="space-y-0.5">
          {!collapsed && <p className="px-2 py-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground/70">Queues</p>}
          {QUEUES.map((q) => (
            <NavButton
              key={q.id}
              active={queue === q.id}
              collapsed={collapsed}
              icon={q.icon}
              label={q.label}
              count={counts[q.id]}
              onClick={() => onQueue(q.id)}
            />
          ))}
        </div>

        <div className="space-y-0.5">
          {!collapsed && <p className="px-2 py-1 text-[11px] font-semibold uppercase tracking-wide text-muted-foreground/70">Categories</p>}
          {CATEGORIES.map((c) => (
            <NavButton
              key={c.id}
              active={category === c.id}
              collapsed={collapsed}
              icon={c.icon}
              label={c.label}
              count={counts[c.id]}
              onClick={() => onCategory(c.id)}
            />
          ))}
        </div>
      </nav>

      <div className="space-y-1 border-t border-border/60 p-2">
        {totalSpeed > 0 && (
          <div
            title={collapsed ? `Speed: ${formatBytes(totalSpeed)}/s` : undefined}
            className={cn(
              "flex items-center gap-1.5 rounded-md bg-primary/10 px-2 py-1.5 text-xs font-medium text-primary",
              collapsed && "justify-center px-0",
            )}
          >
            <span className="size-1.5 shrink-0 rounded-full bg-primary animate-pulse" />
            {!collapsed && <span className="truncate font-mono tabular-nums">{formatBytes(totalSpeed)}/s</span>}
          </div>
        )}
        <div className={cn("flex items-center gap-1", collapsed && "flex-col")}>
          <Button variant="ghost" size="icon" className="rounded-full" title={`Theme: ${theme}`} onClick={onCycleTheme}>
            {THEME_ICON[theme] ?? THEME_ICON.system}
          </Button>
          <Button
            variant={showSettings ? "secondary" : "ghost"}
            size="icon"
            className="rounded-full"
            title="Settings"
            onClick={onToggleSettings}
          >
            <Settings />
          </Button>
        </div>
      </div>
    </aside>
  );
}

function NavButton({
  active, collapsed, icon, label, count, onClick,
}: {
  active: boolean;
  collapsed: boolean;
  icon: React.ReactNode;
  label: string;
  count: number;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={collapsed ? `${label} (${count})` : undefined}
      className={cn(
        "flex w-full items-center gap-2 rounded-md px-2 py-1.5 text-sm transition-colors",
        collapsed && "justify-center px-0",
        active
          ? "bg-primary/10 font-medium text-primary"
          : "text-muted-foreground hover:bg-accent hover:text-foreground",
      )}
    >
      <span className="shrink-0">{icon}</span>
      {!collapsed && (
        <>
          <span className="flex-1 truncate text-left">{label}</span>
          <span className="font-mono text-xs tabular-nums text-muted-foreground/70">{count}</span>
        </>
      )}
    </button>
  );
}
