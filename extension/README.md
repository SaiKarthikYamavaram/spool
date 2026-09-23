# spool browser integration

Hands downloads to the spool desktop app, carrying the browser's own cookies,
User-Agent and Referer — so authenticated and anti-bot-challenged files
(Cloudflare `cf_clearance`, etc.) download through spool instead of failing.

## Install (Chromium / Chrome / Brave / Edge)

1. Start the spool app once. It registers itself as this extension's
   native-messaging host with each installed Chromium-family browser.
2. Open `chrome://extensions`, enable **Developer mode**.
3. **Load unpacked** → select this `extension/` folder.
4. The toolbar icon shows a green dot when the app is reachable.

## Features

- **Right-click a link / video / image** → *Download with spool*.
- **Media detection** — as a page loads, downloadable responses (video, audio,
  archives, documents, big binaries, `Content-Disposition: attachment`) are
  detected and counted on the toolbar badge. Open the popup to grab any of them,
  or **Download all**.
- **Download all links / all images on this page** — right-click the page.
- **Capture browser downloads** (opt-in) — anything the browser would download
  is handed to spool instead, filtered by file type and minimum size.
- **Options** (`chrome://extensions` → Details → Extension options): enable/
  disable, media detection, download capture, minimum file size, which file
  types to handle, and excluded domains.

## How the anti-bot bypass works

The extension does not solve challenges — the browser already did. It reads the
cookies the browser holds for the target URL (via `chrome.cookies`) and sends
them with the exact User-Agent that earned them, so spool replays a session the
browser established. Keep the User-Agent in the app's settings matching this
browser.
