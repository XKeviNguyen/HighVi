# HighVi architecture

One page for contributors. Security details live in [SECURITY.md](../SECURITY.md); the
resumable work log and verification evidence live in [LEDGER.md](LEDGER.md).

## Components

| Path | Responsibility |
| --- | --- |
| `ui/` | Static webview UI (no framework). Sends a source and a job ID; renders progress events. Never sees media URLs. |
| `src-tauri/src/lib.rs` | Tauri commands: `start_job`, `cancel_job`, `dependency_status`, `open_output_folder`. One job at a time; cancels and waits on exit and on SIGTERM/SIGHUP/SIGINT. |
| `crates/highvi-core/src/job.rs` | The job: discover → choose formats → download tracks → mux → publish. |
| `egress.rs`, `netpolicy.rs` | Per-job `CONNECT` proxy on 127.0.0.1: the only network route for yt-dlp and Chrome. Public HTTPS destinations only. |
| `ytdlp.rs`, `formats.rs` | Hermetic yt-dlp arguments; parsing and sanitising its JSON; choosing formats. |
| `browser.rs`, `hls.rs` | Headless-Chrome discovery over a DevTools pipe; ranking observed playlists. |
| `mux.rs` | Magic-byte sniffing and the locked-down FFmpeg mux. |
| `process.rs` | Child processes: no shell, cleared env, own process group, bounded output, cancel/timeout kills the group. |
| `filesystem.rs` | `Downloads/HighVi`, private locked job directories, no-replace publishing. |
| `classify.rs` | yt-dlp output → error code, attributed to what was being fetched (page, browser-found playlist, media). |
| `tools.rs` | Tool versions; whether yt-dlp is older than a site needs (`SITE_MINIMUMS`) or than tested. |
| `diagnostics.rs` | Per-job report for a failed job: stages, codes, versions, host names only. |
| `e2e.rs` (tests only) | Local HTTPS fixture + synthetic media; real yt-dlp, FFmpeg and Chrome. |

## Pipeline

```text
URL ──► validate (https, no credentials)
    ──► job dir (0700, flock) + egress proxy (127.0.0.1:random)
    ──► A. yt-dlp -J  (through proxy, no FFmpeg)
          ├─ formats found ─────────────────────────────┐
          └─ "nothing here" (not: blocked/403/DRM/login)│
    ──► C. headless Chrome (fresh profile, pipe, proxy) │
          └─ observed HLS playlists, ranked; each checked by yt-dlp -J
                                                         ▼
       choose best video (+ separate audio) from yt-dlp's formats
    ──► info.json = only the chosen formats, sanitised headers, no webpage_url
    ──► yt-dlp download per track (native HLS, AES-128 ok, gaps fail, no FFmpeg)
    ──► FFmpeg mux (file: only, forced demuxer, -c copy) ──► rename(NOREPLACE) "<title>.mp4" / " (2)"
```

Stage B (static HTML scanning) is yt-dlp's generic extractor; HighVi does not duplicate it.
There is no separate discovery ticket: the job owns its discovery result end to end, so no
media URL ever crosses into the UI and nothing can be substituted.

## Failure attribution

| Where it failed | Code |
| --- | --- |
| Page refused (yt-dlp or browser: 401/403/451) | `access_denied` (`host_firewall_block` for Cloudflare / `cf-mitigated: challenge`) |
| Page or playlist rate-limited (429) | `rate_limited` |
| Browser: page never arrived / HTTP error | `media_discovery_timeout` / `page_unavailable` |
| Browser: the player's playlist was refused (4xx) | `playlist_access_denied` (also when yt-dlp is refused a browser-found playlist) |
| Browser: page loaded, no playlist | `no_media_found` |
| Media file or segments refused after metadata succeeded | `media_access_denied` |

Extractor-related failures carry a `hint` to update yt-dlp when it is older than the site needs
(YouTube: 2026.08.19, see LEDGER) or than `RECOMMENDED_YT_DLP`, or more than 90 days old.

## Bounds

| What | Limit |
| --- | --- |
| yt-dlp metadata | 8 MiB stdout, 60 s |
| Browser discovery | 30 s per page; gives up ~8 s after trying to play; ≤ 48 sessions, ≤ 4 000 tracked requests, ≤ 60 body reads |
| Proxy | ≤ 64 tunnels, 8 KiB request head, 10 s head/resolve, 15 s connect, 90 s idle |
| yt-dlp retries | 3 requests, 5 fragments, 1 extractor |
| yt-dlp `PATH` | `<job>/bin` with only a link to the real `python3`: no helpers (phantomjs, FFmpeg); JS runtimes off via `--no-js-runtimes` |
| Diagnostic report | 4 KiB, in memory, failed jobs only |
| Track download | 6 h each; stopped as `download_stalled` below 64 KiB of progress in any 2 min (rate floor, so trickles count as stalls) |
| Disk | before downloading: free − other jobs' reservations ≥ 2 × estimated size + 512 MiB floor; while downloading/muxing: stop below the floor (`insufficient_disk_space`) |
| DevTools messages | events ≤ 1 MiB (larger skipped unparsed), replies ≤ 16 MiB, 32 MiB queued in total, backlog ≤ 8 MiB, tracked URLs ≤ 8 MiB; saturation → `discovery_overloaded` |
| Mux | 30 min |
| Jobs | 1 at a time; Chrome never runs while idle |

## Tests

```bash
HIGHVI_REQUIRE_E2E=1 cargo test --workspace   # unit + end-to-end (needs yt-dlp, ffmpeg, ffprobe, Chrome)
node scripts/check-ui.mjs                      # EN/VI/JA catalogue parity, every Rust error code translated
```

Without `HIGHVI_REQUIRE_E2E`, end-to-end tests skip when a tool is missing. CI sets it, pins
yt-dlp, and runs Chrome with its sandbox on.
