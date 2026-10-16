<div align="center">
  <img src="docs/assets/matrix-banner.svg" alt="HighVi — green digital rain" width="100%" />

  <p>
    <strong>English</strong> ·
    <a href="README.vi.md">Tiếng Việt</a> ·
    <a href="README.ja.md">日本語</a>
  </p>

  <p>
    <a href="https://tauri.app/"><img src="https://img.shields.io/badge/Tauri-v2-00ff41?style=flat-square&logo=tauri&logoColor=00ff41&labelColor=000000" alt="Tauri v2" /></a>
    <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-2024-00ff41?style=flat-square&logo=rust&logoColor=00ff41&labelColor=000000" alt="Rust 2024" /></a>
    <img src="https://img.shields.io/badge/Ubuntu-desktop-00ff41?style=flat-square&logo=ubuntu&logoColor=00ff41&labelColor=000000" alt="Ubuntu desktop" />
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-00ff41?style=flat-square&labelColor=000000" alt="MIT License" /></a>
  </p>

  <p><code>&gt; wake up... the video is waiting_</code></p>
</div>

## 🟩 What is HighVi?

Lots of videos on the internet aren't one big file. They're chopped into **lots of tiny pieces**, and a small list called a **playlist** (a file ending in `.m3u8`) says where every piece lives. Think of it as a **treasure map**.

HighVi reads the treasure map, collects every piece, and glues them back together into **one MP4 file** you can watch anywhere.

<div align="center">
  <img src="docs/assets/screenshot-en.png" alt="HighVi window in the green Matrix theme" width="720" />
  <br />
  <sub>The real HighVi window on Ubuntu.</sub>
</div>

## 🕹️ Two ways to use it

| Mode | You paste | Good for |
| --- | --- | --- |
| **Video page link** | The link to the video page | Almost everything: sites `yt-dlp` knows, and pages whose player is built with JavaScript |
| **Playlist link (.m3u8)** | The playlist link **and** the page you found it on | When you already have the `.m3u8` link |

HighVi speaks **English, Tiếng Việt, and 日本語**. Pick one with the buttons in the top corner, and HighVi will remember it.

**Why does “Playlist link” ask for the page too?**
Some websites only hand out the treasure map if you say where you came from, like showing a ticket at the door. That note is called the **Referer**. HighVi only tells them the website's front-door address (for example `https://example.com/`). It never sends the rest of the page link, your cookies, or your logins.

**Many videos at once?** Paste one link per line (up to 100 at a time) and press **Start queue**. HighVi works on up to **four** at once; you choose 1–4. Each row shows what it is doing, including when it is waiting for the private browser (one page at a time). You can cancel, retry, or remove any row. A link that is already in the queue isn't added twice.

**Do I need DevTools or F12?** No. Paste the page link. If `yt-dlp` doesn't know the site, HighVi opens the page in a private, throw-away Chrome (not your profile, no cookies, every request through the network guard), lets the page's own player load, and picks the main video, not the adverts. The **Playlist link** mode is only an advanced fallback for when you already have the `.m3u8` link.

**The site refuses HighVi?** Some sites, often behind Cloudflare, turn away hidden, automated browsers while showing the same page to a normal one. The failed row then offers **Open in HighVi Browser**: a visible, private HighVi window where you start the video yourself (see below).

## ✅ What HighVi does

1. You paste a link and press **Download MP4**.
2. HighVi checks that it's an `https://` link, then starts a private **network guard**: every request `yt-dlp` makes (the page, redirects, playlists, pieces, keys) must go to a public internet address. Computers inside your home or office network are always blocked.
3. `yt-dlp` looks for the treasure map. If it finds nothing, HighVi watches the page's own player in a private browser to see which playlist it loads. Either way, HighVi picks the best picture (and the sound, if it's separate).
4. The pieces are downloaded and glued into one MP4 on your computer. You see progress, speed, and time left, and you can press **Cancel** at any time.
5. The finished video lands in **`~/Downloads/HighVi`**, named after the video. If the name is taken, HighVi adds ` (2)`. It never writes over a file. Press **Open folder** to see it.

## 🧭 Using the queue

1. Paste one or more video page links, one per line (up to 100 at a time), and press **Add to queue**. Duplicates and invalid lines are listed, not added.
2. Press **Start queue**. Use **At once** to run 1 to 4 downloads together; the default is 2. Lowering it never stops a running download: the extra ones finish first.
3. Each row shows its step: looking for the video, *waiting for the private browser* (pages that need it go one at a time), downloading, *waiting to put it together*, saving.
4. Per row: **Cancel**, **Retry**, **Remove**, and, for pages a visible window may help, **Open in HighVi Browser**. For the whole queue: **Pause queue**, **Cancel all**, **Retry failed**, **Clear completed**.
5. A failed row can show its diagnostics (steps, error codes, tool versions, host names; never links, cookies or passwords) to copy into a bug report.

The queue lives in memory: closing HighVi stops running downloads (finished files stay) and forgets the list.

## 🌐 Open in HighVi Browser

When a page refuses HighVi's hidden browser, or its player needs you to press play, **Open in HighVi Browser** opens the page in a visible HighVi window:

1. The window is a fresh, private Chrome profile made for this one page. It is not your browser: no history, logins, cookies or extensions, and it is deleted afterwards.
2. Use the page normally and start the video. HighVi watches the window's network traffic for the video stream. No DevTools, no copying links.
3. As soon as a stream passes HighVi's checks, the window closes and the download continues in the queue as usual. An advert or preview that fails them does not close it.

- The window's web traffic still goes through HighVi's network guard (public `https://` only), with Chrome's sandbox on. Links it would hand to other apps (`mailto:`, `magnet:`, …) go nowhere, and file uploads are off.
- HighVi never presses play or clicks for you in the window.
- The window waits up to 10 minutes. Only one HighVi browser runs at a time: if another page is using it, the window opens when it is free. Downloads that don't need the browser keep going.
- Closing the window ends the attempt (`browser_closed`). **Cancel** works too.
- It is a normal desktop window: it shares your clipboard like any app, and on X11 (not Wayland), apps on the same display can see each other's windows.

> [!NOTE]
> A site can still refuse a visible browser, or ask you to prove you are human. HighVi never answers such checks for you. If the site lets you through, it is because you were allowed to, not because HighVi got around anything.

## 🛑 What HighVi will never do

- Break DRM, paywalls, or logins.
- Use your browser's cookies or sessions.
- Pretend to be a web browser to sneak past bot protection.
- Run plugins or extra commands from your `yt-dlp` settings.
- Solve bot checks or CAPTCHAs for you, or copy cookies out of your own browser.

## 🔒 Privacy & safety

- **Nothing leaves your computer except the downloads themselves.** HighVi has no account, no telemetry and no update check.
- **Links stay inside HighVi.** Signed video links never reach HighVi's own window, logs, or other programs' command lines. Saved videos carry no source links or tags.
- **One guard for all traffic.** `yt-dlp` and the browsers can only reach public `https://` addresses through HighVi's network guard. Your home or office network is unreachable to them.
- **Throw-away workspace.** Every download works in a private folder (`~/Downloads/HighVi/.highvi-job-…`, mode `0700`) that is removed when it ends, and after a crash when HighVi starts again. If HighVi is killed, its tools stop with it.
- **No surprises on disk.** HighVi never replaces a file; a taken name gets ` (2)`.
- **Diagnostics are yours.** They are shown only when you ask, and are never sent anywhere.

> [!IMPORTANT]
> Only download videos you own or have permission to download.

## 🚀 Get started

You need **Ubuntu**, **Rust**, **Node.js**, **`yt-dlp`**, and **FFmpeg**. **Google Chrome or Chromium** is optional: without it, pages whose player is built with JavaScript can't be searched.

Install the system libraries Tauri needs:

```bash
sudo apt update
sudo apt install build-essential curl wget file libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev libwebkit2gtk-4.1-dev ffmpeg
```

Install `yt-dlp` **2025.11.12 or newer** (HighVi is tested with 2026.03.17 and 2026.08.19), and keep it up to date. Ubuntu 26.04's package is new enough; **Ubuntu 24.04's is not** (2024.04.09). There, install it with pipx, then **log out and back in** so the new one is found first:

```bash
sudo apt install pipx && pipx install yt-dlp
```

Then check that everything is there:

```bash
yt-dlp --version && ffmpeg -version && node --version && cargo --version
```

Run the app:

```bash
npm install
npm run dev     # open HighVi
npm run build   # make a .deb and an AppImage
```

## 📦 Install, upgrade, remove

There is no published release yet; build the packages yourself (see **Get started**).

**Install the `.deb`** (Ubuntu/Debian; it also installs `yt-dlp` and FFmpeg):

```bash
sudo apt install ./target/release/bundle/deb/HighVi_0.1.0_amd64.deb
```

On Ubuntu 24.04, also install a newer `yt-dlp` with pipx (see **Get started**). For pages whose player is built with JavaScript, and for **Open in HighVi Browser**, install Google Chrome or Chromium; Ubuntu's Chromium snap works.

**Upgrade** to a newer build: install the new `.deb` the same way. If the version number did not change, add `--reinstall`, or apt keeps the old one:

```bash
sudo apt install --reinstall ./target/release/bundle/deb/HighVi_0.1.0_amd64.deb
```

**Remove**: `sudo apt remove high-vi`. Your videos in `~/Downloads/HighVi` are kept.

**AppImage** (needs `yt-dlp` and FFmpeg on your `PATH`):

```bash
chmod +x target/release/bundle/appimage/HighVi_0.1.0_amd64.AppImage
./target/release/bundle/appimage/HighVi_0.1.0_amd64.AppImage
```

If it complains about FUSE, run it with `APPIMAGE_EXTRACT_AND_RUN=1`.

Videos are saved in **`~/Downloads/HighVi`** (or `Downloads/HighVi` in your language's Downloads folder).

## 🧯 When something goes wrong

| Message code | What it means | What to try |
| --- | --- | --- |
| `access_denied` / `host_firewall_block` | The website said **no** (HTTP 403). | If you pasted a `.m3u8` link as a page, switch to **Playlist link** and add the page you found it on. Check it's the right page, or copy a fresh playlist link. If the site needs you logged in through a browser, HighVi can't help. |
| `unsupported_page` | `yt-dlp` doesn't know this website, and no browser is installed to look further. | Install Google Chrome or Chromium, or use **Playlist link**. |
| `no_media_found` | The private browser opened the page, but no video loaded. | The player may need a sign-in or a special button. HighVi won't work around those. |
| `browser_failed` | The private browser stopped unexpectedly. | Try again. |
| `unsupported_playlist` | The link isn't a playlist HighVi can read. | Make sure it's the `.m3u8` link itself. |
| `media_not_found` | The video is gone (HTTP 404/410). | The link may have expired. Copy a fresh one. |
| `drm_not_supported` | The video is locked with DRM. | HighVi won't unlock it. |
| `authentication_required` | You have to sign in to watch it. | HighVi doesn't use your logins. |
| `network_timeout` | The website took too long to answer. | Try again later. |
| `unsafe_network_target` | The video tried to reach a computer on a home or office network. | HighVi blocked it on purpose. |
| `live_not_supported` | It's a live stream. | HighVi downloads finished videos only. |
| `missing_dependency` | `yt-dlp` or FFmpeg isn't installed. | Install it, then try again. |
| `ytdlp_too_old` | Your `yt-dlp` is older than 2025.11.12 and can't run HighVi's safety settings. | Install a newer one with pipx (see **Get started**), then log out and back in. |
| `rate_limited` | The website says you are asking too often (HTTP 429). | Wait a while, and download fewer videos at once. |
| `media_access_denied` | The page was found, but the video's server said **no** (HTTP 403). | Often an old `yt-dlp`: update it. Some sites only allow their own player. |
| `download_stalled` | The download stopped moving for 2 minutes. | Try again later. |
| `download_timeout` | One part took longer than 24 hours. | Try again on a faster connection. |
| `insufficient_disk_space` | There isn't enough free space for the video. | Free some space, then retry. |
| `discovery_overloaded` | The page made too many requests for the private browser to follow. | Use **Playlist link** if you have the `.m3u8`. |
| `browser_closed` | The HighVi browser window was closed before a video started. | Open it again and start the video in it, or leave the row as it is. |
| `interactive_timeout` | No video started in the HighVi browser window within 10 minutes. | Open it again; start the video in the window. |
| `browser_no_display` | HighVi could not find your desktop's display for its browser window. | Run HighVi from your normal desktop session. |
| `queue_full` / `batch_too_large` | The queue holds 200 videos, and you can add 100 at a time. | Clear finished rows, or add fewer links. |

## ❓ FAQ

**Why does a page play in my Chrome but fail in HighVi?** Most often the site refuses hidden, automated browsers (you will see `host_firewall_block` or `access_denied`). Use **Open in HighVi Browser**. If the site refuses that window too, HighVi cannot download from it.

**Does Open in HighVi Browser use my Chrome profile, passwords or cookies?** No. It is a separate, empty profile that is deleted afterwards. HighVi never reads your own browser's data.

**YouTube fails with 403.** Your `yt-dlp` is too old for YouTube (2026.08.19 or newer is needed). Update it as in **Get started**.

**Does it work on Wayland? On a HiDPI screen?** Yes. HighVi and its browser window run natively on Wayland and X11, and scale with the screen.

**How many downloads can run at once?** Up to four; you choose 1 to 4. Pages that need a browser use it one at a time.

**Can it download DRM-protected or live videos?** No, and it never tries.

## 🚧 Known limitations

- **HLS and plain MP4 only.** DASH-only sites, live streams, and DRM-protected video are refused on purpose.
- **No logins, no cookies.** Videos that need you to be signed in, or a cookie the page sets for the player, won't download. HighVi never borrows your browser session.
- **Browser search and Open in HighVi Browser need Google Chrome or Chromium.** HighVi doesn't ship a browser. Ubuntu's Chromium snap works too (all browser tests pass with Chromium 155 from the snap).
- **A site may refuse even the visible HighVi browser** (bot checks, sign-in, region locks). HighVi reports that; it does not work around it.
- **A player that needs a special click** (a custom "play" overlay, an age gate, a consent wall) won't be pressed. HighVi only starts normal `<video>` elements.
- **No system or company proxy.** HighVi's own network guard is the only way out. Behind a proxy that blocks direct connections, downloads fail.
- **Plain `http://` media is refused,** even when the page itself is `https://`.
- **Linux only.** Tested on Ubuntu 26.04 with Google Chrome 153/154, yt-dlp 2026.03.17 and 2026.08.19, and FFmpeg 8. On Ubuntu 24.04, only installing and starting the `.deb` was checked. On Wayland, only starting up and sharp HiDPI scaling were checked.
- **Up to four downloads at once,** and only one page in the private browser at a time. The queue is forgotten when HighVi closes.
- **Very long media URLs (over 8 KiB) seen in the browser are ignored.**

## 🧠 For developers

Architecture, pipeline and limits: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

```bash
HIGHVI_REQUIRE_E2E=1 cargo test --workspace --offline
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo fmt --all -- --check
node --check ui/app.js && node --check ui/i18n.js && node --check ui/rain.js
node scripts/check-ui.mjs   # translation parity
```

```text
crates/highvi-core/   Network guard (egress proxy), discovery, format choice, download job, MP4 mux, safe saving
                      (unit tests + end-to-end tests with real yt-dlp/FFmpeg against a local HTTPS fixture)
src-tauri/            Tauri commands: the download queue, progress events, open the folder
ui/                   The green window: index.html, styles.css, app.js, i18n.js (English · Tiếng Việt · 日本語), rain.js
docs/assets/          README banner and screenshots
```

[SECURITY.md](SECURITY.md) explains how HighVi keeps you safe and what its limits are today. In short: every request HighVi's tools make goes through a guard that only lets public HTTPS addresses through, and FFmpeg never touches the network.

## 📜 License

[MIT](LICENSE)

<div align="center"><sub><code>there is no spoon — only segments_</code></sub></div>
