import { invoke } from "@tauri-apps/api/core";
import { openPath, revealItemInDir } from "@tauri-apps/plugin-opener";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

export type Status =
  | "queued"
  | "downloading"
  | "paused"
  | "interrupted"
  | "completed"
  | "failed";

export type DownloadView = {
  id: string;
  url: string;
  filename: string;
  status: Status;
  downloaded: number;
  total: number | null;
  segments: number;
  error: string | null;
  path: string;
  added_at: number;
  ranges: [number, number][];
  done: number[];
  supports_ranges: boolean;
  user_agent: string | null;
  referer: string | null;
  has_cookie: boolean;
  thumbnail: string | null;
  engine: string;
};

export type Settings = {
  download_dir: string | null;
  max_concurrent: number;
  segments: number;
  theme: string;
  cookies_file: string | null;
  user_agent: string | null;
  bandwidth_kb: number;
  ytdlp_path: string;
  cookies_browser: string;
  video_quality: string;
  proxy: string;
  categorize: boolean;
  run_in_background: boolean;
  start_on_login: boolean;
  start_minimised: boolean;
};

/// Payload of `download://confirm`: a captured URL awaiting the add dialog.
export type ConfirmRequest = {
  token: string;
  url: string;
  video: boolean;
};

export type ProgressRow = {
  id: string;
  downloaded: number;
  total: number | null;
};

/// Per-download choices from the add dialog; omitted fields use the settings.
export type AddOptions = {
  dir?: string | null;
  /// Save under this filename instead of the one the server or video title
  /// suggests. Blank means automatic.
  name?: string | null;
  quality?: string | null;
  start?: boolean;
};

export const api = {
  addDownload: (url: string, options?: AddOptions) =>
    invoke<string>("add_download", { url, options: options ?? null }),
  getDownloadDir: () => invoke<string>("get_download_dir"),
  /// Confirm a request the extension parked ("ask before download").
  addPending: (token: string, options?: AddOptions) =>
    invoke<string>("add_pending", { token, options: options ?? null }),
  cancelPending: (token: string) => invoke<void>("cancel_pending", { token }),
  /// Native folder picker for the add dialog's save location.
  pickFolder: async (defaultPath?: string) => {
    const picked = await openDialog({ directory: true, multiple: false, defaultPath });
    return typeof picked === "string" ? picked : null;
  },
  importUrls: (text: string) => invoke<string[]>("import_urls", { text }),
  /// Walk a page (and optionally the pages it links to) for downloadable
  /// links. Returns what it found; nothing is queued until the user confirms.
  grabLinks: (url: string, depth: number, filter: string) =>
    invoke<string[]>("grab_links", { url, depth, filter }),
  isDuplicate: (url: string) => invoke<boolean>("is_duplicate", { url }),
  /// Apply one action to a whole selection in a single call, so the list
  /// re-renders once instead of once per row.
  bulk: (ids: string[], action: "pause" | "resume" | "remove" | "remove_with_file") =>
    invoke<void>("bulk_action", { ids, action }),
  /// Rename a download's file. Rejected while it is running.
  rename: (id: string, name: string) => invoke<void>("rename_download", { id, name }),
  pause: (id: string) => invoke<void>("pause_download", { id }),
  resume: (id: string) => invoke<void>("resume_download", { id }),
  cancel: (id: string) => invoke<void>("cancel_download", { id }),
  retry: (id: string) => invoke<void>("retry_download", { id }),
  remove: (id: string, deleteFile: boolean) =>
    invoke<void>("remove_download", { id, deleteFile }),
  /// Reorder the queue: position is priority, so this is what moves a
  /// download ahead of the others. Negative is up; a big magnitude clamps to
  /// an end ("to the top").
  move: (id: string, delta: number) => invoke<void>("move_download", { id, delta }),
  /// Hash a finished file to check it against a published checksum.
  hashFile: (id: string, algo: "sha256" | "md5") =>
    invoke<string>("hash_file", { id, algo }),
  pauseAll: () => invoke<void>("pause_all"),
  resumeAll: () => invoke<void>("resume_all"),
  /// A poster frame for a finished video, as a data: URI. Null for anything
  /// that is not a readable video — the row shows its type icon instead.
  videoThumbnail: (id: string) => invoke<string | null>("video_thumbnail", { id }),
  getQueue: () => invoke<DownloadView[]>("get_queue"),
  clearHistory: () => invoke<void>("clear_history"),
  getSettings: () => invoke<Settings>("get_settings"),
  updateSettings: (settings: Settings) =>
    invoke<void>("update_settings", { settings }),

  // Open the finished file with the OS default app.
  openFile: (path: string) => openPath(path),
  // Open the containing folder with the file selected, like IDM's
  // "Open containing folder".
  revealFile: (path: string) => revealItemInDir(path),
};

export function formatBytes(n: number): string {
  // Never show raw bytes: the smallest unit is KB. One decimal below 10 (9.5
  // MB), whole numbers above (512 KB, 100 MB).
  const units = ["KB", "MB", "GB", "TB"];
  let value = n / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  const dec = value === 0 ? 0 : value < 10 ? 1 : 0;
  return `${value.toFixed(dec)} ${units[unit]}`;
}

export function formatDate(unixSecs: number): string {
  if (!unixSecs) return "—";
  return new Date(unixSecs * 1000).toLocaleString();
}

export function formatEta(seconds: number): string {
  if (!isFinite(seconds) || seconds <= 0) return "";
  if (seconds < 60) return `${Math.round(seconds)}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${Math.round(seconds % 60)}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}
