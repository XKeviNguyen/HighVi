# HighVi V1 execution ledger

Compact, resumable record of the V1 mission. Newest state first.

## Current state

- Phase: 1+3+4 (network boundary, job pipeline, one-click UI) — PR A in review
- Active branch: `feature/egress-boundary`
- PR: (opening)
- Last tested SHA: working tree before first commit (local gate green)
- Failed gates: —
- Next action: push PR A, Codex review + read-only security review, CI; then PR B (browser discovery)

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

## Log

- 2026-10-14 preflight + baseline recorded.
