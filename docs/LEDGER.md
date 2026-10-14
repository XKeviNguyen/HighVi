# HighVi V1 execution ledger

Compact, resumable record of the V1 mission. Newest state first.

## Current state

- Phase: 2 (browser discovery) + 5 (packaging) in progress; PR A in final review
- PR A: #2 `feature/egress-boundary` — head 2d7416f, CI green on b2c92a6, Codex r4 pending
- PR B: `feature/browser-discovery` (stacked on PR A) — local gate green (91 tests), native
  smoke passed; not yet pushed
- Failed gates: —
- Next action: merge PR A when Codex r4 clean + CI green on its head; rebase/push PR B,
  Codex + CI; packaging verification (.deb/AppImage) + docs PR

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
