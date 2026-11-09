import { useEffect, useState, useSyncExternalStore } from "react";
import { api, formatBytes } from "../lib/api";

/// The overlay is a per-machine debugging aid, not a download setting, so it
/// lives in localStorage rather than settings.json. A window event keeps the
/// Settings switch and the overlay in step within the one window.
const KEY = "spool.perfOverlay";
const EVENT = "spool:perf-overlay";

function read(): boolean {
  try {
    return localStorage.getItem(KEY) === "1";
  } catch {
    return false;
  }
}

export function setPerfOverlay(on: boolean) {
  try {
    localStorage.setItem(KEY, on ? "1" : "0");
  } catch {
    /* storage blocked: the toggle just won't persist */
  }
  window.dispatchEvent(new Event(EVENT));
}

export function usePerfOverlay(): boolean {
  return useSyncExternalStore(
    (cb) => {
      window.addEventListener(EVENT, cb);
      return () => window.removeEventListener(EVENT, cb);
    },
    read,
  );
}

/// FPS, CPU and memory in a corner pill. FPS counts animation frames the
/// webview delivers; CPU and memory cover spool and its WebKit processes.
export function PerfOverlay() {
  const on = usePerfOverlay();
  const [stats, setStats] = useState<{ fps: number; cpu: number | null; rss: number } | null>(null);

  useEffect(() => {
    if (!on) {
      setStats(null);
      return;
    }
    let frames = 0;
    let raf = requestAnimationFrame(function tick() {
      frames++;
      raf = requestAnimationFrame(tick);
    });
    let since = performance.now();
    const timer = setInterval(async () => {
      const now = performance.now();
      const fps = Math.round((frames * 1000) / (now - since));
      frames = 0;
      since = now;
      const usage = await api.processUsage().catch(() => ({ cpu: null, rss: 0 }));
      setStats({ fps, ...usage });
    }, 1000);
    return () => {
      cancelAnimationFrame(raf);
      clearInterval(timer);
    };
  }, [on]);

  if (!on) return null;
  return (
    <div className="pointer-events-none fixed bottom-2 right-2 z-50 rounded-md border border-border bg-background/90 px-2 py-1 font-mono text-[11px] tabular-nums text-muted-foreground shadow-sm">
      {stats ? (
        <>
          {stats.fps} fps
          {stats.cpu !== null && ` · CPU ${stats.cpu.toFixed(0)}%`}
          {stats.rss > 0 && ` · ${formatBytes(stats.rss)}`}
        </>
      ) : (
        "measuring…"
      )}
    </div>
  );
}
