# HighVi execution ledger

Compact, resumable record. Newest mission first.

# V1.1 mission (started 2026-10-15)

## Current state

- PR #5 (A: failure attribution, diagnostics, helper policy) **merged** → main 2b65601.
- PR #6 (B: stall/deadline/disk, DevTools byte bounds) open; reviewer re-verified at 6012ac2
  (all closed; #5 unreachable delay, accepted); 86aa4db adds mux margin + stale text fix.
- `feature/v1.1-bulk-queue` (C: queue + UI + too-old yt-dlp) rebased on B; 156 tests locally.
- Next: merge B → open C (Codex, CI) → D (docs, packages, release checks).
- Failed gates: — . Open issue: `target/` was deleted by an unknown actor between
  11:20 and 11:22 local (not by any logged command of this session, its agents or
  Codex); only build artifacts lost (incl. V1 bundles; rebuildable from c990a01).

## Review log (V1.1)

- PR #5: Codex r1 (d2af6d4) clean. Opus security review: 5 findings (JS runtimes found
  next to the interpreter despite private PATH → `--no-js-runtimes`; pyenv shims; shared
  /tmp probe HOME; uncancellable diagnostics; misleading hints) → fixed, re-verified
  closed. Codex r2 (2a1754a) + r3 (880a2be) clean. CI caught a test portability bug
  (pipx shebang) → fixed. Merged 880a2be.
- PR #6: reviewer: counter restarts read as stalls, stderr progress, JSON-weight budget,
  6 h deadline, overload on one big event → fixed. Codex r1 P2 mux not watched → fixed;
  r2 P2 redirect lost when tracking full → RequestTracker + unit test; r3 P2 `abr`
  estimate → fixed; r4 clean (6012ac2).
- PR C (pre-PR): Opus flow/concurrency breaker: no critical/high; 24k-call 6-thread
  stress clean; M1 waiting-cancel not tested, L1 misleading Browsing/Merging while
  waiting, L2 sync runner panic leaks slot, L3 random-ID folder titles → all fixed with
  tests proven to fail on the previous code.

## Release evidence so far

- APT (docker, `.deb` from C): Ubuntu 24.04.5 and 26.04.1 — install with deps, binary,
  desktop entry, icon, starts under Xvfb as a user, remove keeps `~/Downloads/HighVi`.
  24.04 APT yt-dlp 2024.04.09 is too old (rejects `--no-plugin-dirs`) → `ytdlp_too_old`.
- Wayland (headless GNOME Shell 50.1, isolated HOME/XDG dirs/session bus): runs as a
  native Wayland client (titled xdg_toplevel, up 15 s, no panic). Monitor scale 2 →
  `wl_output.scale(2)` and `wl_surface.set_buffer_scale(2)`. GNOME refuses screenshots,
  so the 2× layout was checked on Xvfb with GDK_SCALE=2: sharp, nothing clipped.
- Native (Xvfb, release binary, real IPC): batch add (5 added, 1 duplicate, 1 invalid line),
  Start, target 2→4 (running jobs untouched), YouTube CC-BY → media 403 + update hint,
  missav → `host_firewall_block` with report "document 403 (Cloudflare bot check)",
  3 public test streams saved and fully decoded (1080p 634 s, 720p 1800 s, ToS 734 s),
  SIGTERM mid-download → exit 57 ms, no yt-dlp/Chrome left, job dir removed, files kept.
- Profile (fixture, release): 8 jobs at targets 1–4 → 40.7/11.6/6.1/4.9 s; peak threads
  8–12, fds 21–42, RSS 10–11 MiB (core); Cancel All with 4 in flight 11 ms.
- Native resource samples: Rust process 172–176 MB RSS, 59–69 threads, ≤ 99 fds;
  ≤ 4 yt-dlp, ≤ 1 FFmpeg, ≤ 1 Chrome instance; idle CPU 0 %.

## Baseline (verified 2026-10-15)

- 97 tests listed in V1 report; CI = check (fmt, clippy, test+e2e, node, ui parity), audit, .deb.
- Dev box: Ubuntu 26.04, yt-dlp 2026.03.17 (apt), no deno/yt-dlp-ejs, node 22, Chrome (deb), FFmpeg 8,
  codex-cli 0.154, docker (user in group), session Wayland. 12 GB free disk.

## Incident evidence

- **A2 YouTube 403** (CC-BY Big Buck Bunny, aqz-KE-bpKQ): reproduced with `run_job` harness:
  metadata OK (2.1 s), `downloadingVideo` → 403. Plain yt-dlp 2026.03.17 without HighVi: same 403
  (proxy not the cause). Only client without a JS runtime is `ANDROID_VR`; googlevideo returns 403
  for its URLs (range requests above ~1–2 MiB on audio, unranged on all). Upstream PR #17461
  (2026.08.19): "Since 2026.08.17, ALL formats are 403'd with android_vr". yt-dlp 2026.08.19
  (official zipapp, SHA-256 verified, scratch only) uses `visionos`, no JS runtime needed:
  unmodified HighVi pipeline → 3840×2160 VP9+AAC MP4, 634.6 s, 1.37 GB.
  HighVi defects: 403 on the media file is reported as "site may block automated downloads";
  no yt-dlp version detection/advice.
- **A1 missav.live/ja/dldss-564 `no_media_found`**: ordinary request → 200 (Cloudflare). Page's
  player is hls.js loading on DOMContentLoaded (no click needed). Sanitised CDP trace in HighVi's
  headless Chrome (yt-dlp stubbed so nothing could be downloaded): main document **HTTP 403 +
  Cloudflare challenge** (challenges.cloudflare.com iframe, Turnstile workers); player never runs.
  HighVi defect: main-document status ignored → misreported as no_media_found. Site-side: bot
  check against headless browsers — not circumvented (non-goal).
- **P2.3 helpers**: yt-dlp `PhantomJSwrapper` runs `phantomjs` from PATH with no proxy argument
  (iqiyi, douyu, pornhub, openload family); deno is auto-used from PATH. HighVi passes the
  user's PATH → helpers would bypass the egress proxy if installed.

# V1 mission (2026-10-14)

## Final state

- Phase: 6 — release verification. PR #2 merged (de648d3), PR #3 merged (8a1c9bd).
- Active branch: `feature/v1-docs-packaging` (docs, screenshots, CI audit + .deb job)
- Failed gates: —
- Next action: Codex + CI on this PR, merge, verify main CI, final report.

## Review log (PR B, #3)

- Codex r1 (2d44559): P1 clear variant of a DRM master kept → fixed + test; P2 "title query
  unbounded" → already 3 s; added overall discovery deadline anyway (3957282).
- Opus security breaker (2d44559): High — `--force-webrtc-ip-handling-policy` does not exist,
  WebRTC STUN reached LAN/public UDP around the proxy (reproduced: 5 packets); Medium — URL
  flood memory; Low — page downloads, Google background traffic, DRM-variant master → fixed in
  7c79019 (correct switch + profile preference, e2e STUN test failing before/passing after,
  URL/backlog/channel bounds, downloads denied, docs). Re-check: all closed (0 UDP packets;
  peak RSS 203→88 MB under flood).
- Codex r2 (7c79019): no actionable defects. CI green on 7c79019 (Chrome 154, sandbox on).

## Packaging / performance evidence

- Release bundles: `.deb` 3.3 MiB (Depends yt-dlp, ffmpeg, libwebkit2gtk-4.1-0, libgtk-3-0;
  desktop entry, icon), AppImage 88 MiB. AppImage and the binary extracted from the .deb run
  in Xvfb with a fresh HOME; blocked-URL path verified through real IPC. `apt install` itself
  not run (needs sudo).
- Idle CPU (release, Xvfb software rendering, 15 s window): 52% → 0% of one core after the
  20 s intro (39% during intro). RSS ≈ 420–430 MiB (WebKitGTK). No Chrome process while idle
  or while downloading.
- `cargo audit`: 0 vulnerabilities; 2 warnings in Tauri's GTK stack (unmaintained
  proc-macro-error, unsound glib VariantStrIter — not used by HighVi).

## Review log (PR A)

- Codex r1 (53f6cb3, gpt-5.6-terra; configured gpt-6.1-sol unsupported on this account):
  4×P2 — filename bytes, late cancel, live-job cleanup, info.json retention → all fixed
  (afc73bf) with regression tests proven failing first.
- Opus security breaker (53f6cb3): High — format-ID selector keywords (`all`, `b`…) let
  yt-dlp select rejected formats incl. unproxied mms:// via mpv; Medium — `Ytdl-Request-Proxy`
  header overrides --proxy → fixed (d37ed29): keyword rejection, info.json reduced to chosen
  formats, yt-dlp control headers dropped; also own-interface addresses refused, per-run
  refusal deltas, kept-dir marker, accept back-off. Re-check: both repros closed; 2 Low
  (duplicate IDs, IPv4-mapped own address) → fixed (b2c92a6).
- Codex r2 (d37ed29): claimed 192.0.0.0/24 partly reachable → rejected with test evidence.
- Codex r3 (b2c92a6): cancel between Saving check and rename → fixed (2d7416f).

## Baseline (main @ bb9fc64, 2026-10-14)

- Tauri v2 + `highvi-core`; PR #1 (Direct HLS, origin-only Referer, i18n, CI) merged.
- Checks: `cargo fmt --check`, `clippy -D warnings`, `cargo test` (40 tests) all pass.
- Tools on dev box: Rust 1.98, Node 22, yt-dlp 2026.03.17 (Debian pkg, requests+urllib3+Cryptodome),
  FFmpeg 8.0.1, Google Chrome 153, codex-cli 0.154 (logged in), gh (XKeviNguyen).
- No branch protection (private repo, free plan); CI = one job (fmt/clippy/test/node --check).

### Verified defects / gaps

1. **SSRF beyond the first hop (release-blocking).** `ensure_public_network_target` checks only
   the page and the chosen playlist once, by DNS lookup in Rust. yt-dlp then re-resolves (DNS
   rebinding) and follows redirects, variant playlists, segments and `EXT-X-KEY` URIs to any
   address. `SECURITY.md` documents this as a non-goal.
2. **FFmpeg bypass.** yt-dlp `HlsFD` delegates "unsupported" streams (live, SAMPLE-AES, …) to
   FFmpeg, which fetches over the network on its own; yt-dlp post-processors run FFmpeg on
   downloaded (attacker-controlled) files with content probing (HLS demuxer → network).
3. **Proxy env passthrough** lets ambient `no_proxy`/`http_proxy` change egress.
4. **IPv6 policy gaps:** NAT64 `64:ff9b::/96`, 6to4 `2002::/16`, Teredo, IPv4-compatible
   `::/96`, site-local `fec0::/10`, `100::/64`, `2001::/23`; IPv4 `192.88.99.0/24` not blocked.
5. **Silent gaps:** yt-dlp default skips unavailable HLS fragments → "successful" truncated MP4.
6. **Redundant discovery:** download re-runs full discovery (2× extraction per job).
7. **No cancellation**, single global `bool` lifecycle, progress events carry no job id.
8. **No JavaScript-player discovery**: pages whose player builds the HLS URL in JS fail.
9. No open-folder, no dependency status, no live-stream guard (unbounded disk use).

## Architecture decisions

- **Egress boundary = per-job validating CONNECT proxy on 127.0.0.1** (Rust). It resolves
  each host itself, requires *every* resolved address to be public, and connects to exactly
  the validated address (no rebinding window). Plain-HTTP proxying is refused (HTTPS only).
  yt-dlp gets `--proxy`; Chrome gets `--proxy-server` + `<-loopback>` bypass removal.
- **yt-dlp never gets FFmpeg** (`--ffmpeg-location` → nonexistent path): delegation to FFmpeg
  fails closed. yt-dlp downloads raw streams natively (incl. AES-128) through the proxy.
- **HighVi muxes locally** with FFmpeg: `-protocol_whitelist file`, demuxer forced from
  magic-byte sniffing (never probe), explicit stream maps, `-c copy`.
- **Discovery pipeline:** A yt-dlp (`-J`) → C isolated headless Chrome over
  `--remote-debugging-pipe` (no debug port) observing Network events in all frames.
  Stage B (static HTML scan) is covered by yt-dlp's generic extractor; not duplicated.
- **Job owns its discovery result** (one Rust-side job per run; no URL ever returns to the
  UI) → no separate ticket store needed; rediscover once only on expiry signals.
- Tests use a `#[cfg(test)]`-only fixture resolver; production cannot construct it.

## Evidence (PR A, local, 2026-10-14)

- `cargo test --workspace` with `HIGHVI_REQUIRE_E2E=1`: 67 passed (12 end-to-end with real
  yt-dlp 2026.03.17 + FFmpeg 8 through the proxy against a local HTTPS fixture).
- E2E proves: playable MP4 (ffprobe streams + full decode) for muxed TS, split audio
  rendition, fMP4, AES-128; origin-only Referer on every request (incl. page mode where yt-dlp
  recorded the full page URL — found and fixed); blocked + zero canary hits for loopback
  variant, private-DNS variant, loopback/rebind/IPv6-loopback/metadata-IP segments, rebind key,
  redirect to loopback, user-entered loopback/rebind/mapped URLs; plain-HTTP refused; missing
  segment fails; DRM/live/malformed rejected; cancellation mid-request cleans up; missing tools.
- Native (debug binary, Xvfb :99, XTEST input, x11grab screenshots, isolated HOME):
  loopback URL → `unsafe_network_target` shown; Apple bipbop sample HLS downloaded to a
  1800 s 960×720 H.264/AAC MP4 (332 MB) that decodes cleanly; Cancel kills yt-dlp and removes
  the job dir; SIGTERM mid-download → app exits ~1 s, yt-dlp gone, job dir removed (after fix);
  EN/VI/JA rendered. Headless-Chrome (stubbed bridge, NOT native) renders at 410 px (=200% zoom)
  show no horizontal overflow; long titles wrap.
- Defects found and fixed during PR A: page-mode Referer leak via `--load-info-json`; async
  stack overflow (8 KiB stack buffers); missing XDG Downloads → fallback `~/Downloads`; status
  off-screen → scroll into view; SIGTERM left job dir → signals routed to app exit; deps banner
  styling.

## Evidence (PR B, local, 2026-10-14)

- 91 tests (`HIGHVI_REQUIRE_E2E=1`), incl. 7 browser e2e with real Chrome 153 against the
  HTTPS fixture: JS-built URL, delayed iframe player vs advert, MIME-only and content-sniffed
  playlists, click-to-play, Referer-enforcing hosts (403 otherwise), blocked private
  destinations from iframe/img/fetch/WebSocket/plain HTTP (zero canary hits), DRM, no media
  (~13 s), crashing/missing browser, cancellation, zero surviving Chrome processes.
- Fixture bug found: no CORS header (real CDNs send it). Product bug found: Chrome returns HLS
  bodies base64-encoded → decoder added.
- Native: hls.js public demo page (JS player, Mux test stream) → "hls.js demo.mp4",
  1920×1080 H.264/AAC, 634.6 s, 488 MB, full decode exit 0, no Chrome left.

## Log

- 2026-10-14 preflight + baseline recorded.
