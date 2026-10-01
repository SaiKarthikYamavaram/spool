# spool

A download manager for Linux, in the spirit of Internet Download Manager: it
splits a file across several connections, picks downloads up where they
stopped, hands streaming sites to `yt-dlp`, and takes downloads off the
browser through an extension that replays the browser's own session.

Tauri v2 (Rust) + React. One process, no daemon, no account.

---

## What it does

**Segmented HTTP downloads.** A file is split into ranges fetched in parallel
into one pre-allocated file. Byte offsets are checkpointed as they are
`fdatasync`-ed, never before, so a resume after a power cut never seeks past
bytes that were not written. `If-Range` guards the resume: if the file changed
on the server, the transfer restarts rather than stitching two different files
together.

**Video, through yt-dlp.** Known streaming sites and any `.m3u8` / `.mpd`
manifest are routed to `yt-dlp` instead of the HTTP engine — a manifest
fetched over HTTP is a few KB of playlist text, not a video. Quality is
per-download or global; the container is left to yt-dlp.

**A browser extension.** Detects media on the page, takes over the browser's
own downloads, grabs every link or image, and sends the page's video straight
to yt-dlp. Every hand-off carries the cookies the browser already holds, which
is the only way past an interactive anti-bot challenge: the browser solves it,
spool replays the result.

**The rest.** Bandwidth cap, proxy, per-type folders, rename on disk,
multi-select with bulk actions, poster frames extracted from finished video,
tray with pause-all and resume-all, and a queue that survives a crash.

---

## Requirements

| | |
|---|---|
| `yt-dlp` | Required for video sites and stream manifests. Everything else works without it. |
| `ffmpeg`, `ffprobe` | Optional. Used for poster frames, and by yt-dlp to mux video and audio streams. |
| WebKitGTK 4.1 | The webview Tauri renders into. |

On Arch: `sudo pacman -S yt-dlp ffmpeg webkit2gtk-4.1`

---

## Install

Build the release bundle and install it for your user — no root, nothing
outside `~/.local`:

```sh
npm ci
npm run tauri build
./install.sh
```

That puts the binary in `~/.local/bin/spool`, a desktop entry in
`~/.local/share/applications`, and icons under `~/.local/share/icons`. Make
sure `~/.local/bin` is on your `PATH`.

`npm run tauri build` also leaves `.deb` and `.rpm` packages under
`src-tauri/target/release/bundle/` if you would rather install one of those.
The AppImage target needs `linuxdeploy` and its GTK plugin on `PATH`; without
them that one bundle fails and the others still build.

To remove it: `./install.sh --uninstall`.

### Upgrading from `fetchd`

This was called `fetchd` until it was renamed. Nothing to do by hand:
`install.sh` removes the old binary, desktop entry and autostart entry, and the
first launch moves the queue, settings and thumbnail cache from
`com.saikarthik.fetchd` to `com.saikarthik.spool`. Nothing is overwritten, so
launching twice is a no-op. What stays behind at the old path is WebKit's own
cache, which you can delete. Re-load the extension from `extension/` to pick up
the new name.

### The browser extension

Load it unpacked into Chrome, Brave, Chromium, Edge, Vivaldi, or Firefox:

1. Launch spool once. It automatically installs the extension to
   `~/.local/share/com.saikarthik.spool/extension` and registers native messaging
   hosts across native, Flatpak, and Snap browser profiles.
2. Open **Settings → Browser Integration** in spool.
3. Click **Copy** to copy the extension folder path, then click
   **Extensions Page** (`chrome://extensions` or `about:addons`).
4. Turn on **Developer mode** and click **Load unpacked** (or **Load Temporary Add-on**
   in Firefox `about:debugging`), selecting the copied directory.
5. The integration badge turns green automatically once connected.

It talks to the app through native messaging: the browser launches spool as
the extension's native host, which relays to the running app over a socket in
`$XDG_RUNTIME_DIR`. The host manifest is restricted to this extension's pinned ID,
so web pages cannot reach the app. If spool is closed, the extension leaves the
download to the browser rather than losing it.

---

## Settings worth knowing

**Sites that answer `403`.** Some hosts sit behind an interactive challenge.
No download manager can solve one — not this, not IDM. What IDM actually does
is let the browser solve it and reuse that session, and so does this. Either
use the extension, or export a `cookies.txt` and point spool at it. The
User-Agent in settings must match the browser the cookies came from: a
`cf_clearance` cookie is bound to the exact agent that earned it.

**Run in the background** (on by default) decides what the window's close
button does — hide to the tray and keep downloading, or quit. Quitting
checkpoints progress first.

**Start automatically** writes an XDG autostart entry pointing at the
installed binary. It is reconciled at every launch, including rewriting an
entry left pointing at a binary that has moved.

**Connections per download** is capped at 8. More is not faster, and some
servers treat it as abuse.

---

## Development

```sh
npm install
npm run tauri dev     # app + vite, hot reload on both sides
npm test              # frontend and extension (vitest)
npm run build         # typecheck + bundle the frontend
cd src-tauri && cargo test          # backend
cd src-tauri && cargo test -- --ignored   # the ones that hit the network
```

Tests: 117 Rust unit + 11 network (behind `--ignored`), 89 JS. The network
ones are excluded by default because they download real files from a public
host.

### Layout

```
src/                  React UI
  lib/                pure logic — selection rules, file types, formatting
src-tauri/src/
  download.rs         the segmented HTTP engine
  ytdlp.rs            the video engine, and what routes to it
  state.rs            queue, settings, and the pump that starts transfers
  server.rs           the native-messaging bridge the extension talks to
  thumbs.rs           poster frames
  cookies.rs          Netscape cookies.txt
extension/            the MV3 browser extension
```

The icons are generated, not drawn by hand:

```sh
python3 src-tauri/icons/generate.py    # needs rsvg-convert and magick
```

### Notes for anyone changing this

- `queue.json` is written through a temp file and renamed, and an unreadable
  one is moved aside rather than replaced — losing a download list to a parse
  error is not an acceptable failure.
- Progress has two counters. `live` is what the UI shows; `durable` is what
  gets persisted, and only ever lags. Recording an offset ahead of the device
  is the dangerous direction.
- `--print` silently suppresses `--progress-template` in yt-dlp. The output
  path is parsed from its `Destination:` / `Merger` lines instead.
- WebKitGTK ignores CSS on native `<select>` and number spinners unless
  `appearance` is reset, which is why those controls are drawn by hand.
