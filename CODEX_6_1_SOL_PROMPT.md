# Prompt for Codex 6.1 Sol

You are working in `/home/hoang/Projects/HighVi`, a Tauri v2 Ubuntu desktop application. Rust is the primary language. The app accepts a permitted HTTPS video page URL, asks `yt-dlp` to discover a clear HLS `.m3u8` playlist, and downloads an MP4 with the page URL as the `Referer`.

Complete two separate tasks in this order:

1. Perform an evidence-based, read-only code review.
2. Replace the current `README.md` with a polished GitHub README that matches the verified code.

Do not change Rust, JavaScript, CSS, Tauri configuration, package manifests, lockfiles, or security documentation during the review. You may edit only `README.md`. If you find a code defect, report it. Do not fix it unless I approve a follow-up change.

## Start with repository facts

Before editing anything:

1. Read every applicable `AGENTS.md` file.
2. Run `git status --short --branch` and preserve all existing user changes.
3. Inspect `Cargo.toml`, `Cargo.lock`, `package.json`, `package-lock.json`, `src-tauri/`, `crates/highvi-core/`, `ui/`, `SECURITY.md`, `LICENSE`, and the current `README.md`.
4. Do not read `.env`, credentials, browser profiles, cookies, tokens, or unrelated files.
5. Do not contact the example adult-content URL or download any real media. Use deterministic local fixtures and unit tests only.

## Review the code before writing claims

Review the application as hostile input can reach it. Report only reproducible findings. Rank findings as P0, P1, P2, or P3. For each finding, include:

- the exact file and line;
- the input or state that triggers the problem;
- the user or security impact;
- the correct fix layer;
- the regression test that should fail before the fix.

Inspect these areas:

- Tauri IPC arguments and the assumption that the webview is untrusted.
- Shell and argument injection when Rust spawns `yt-dlp`.
- Ambient `yt-dlp` config, plugin, cookie, browser-session, and `--exec` behavior.
- URL parsing, HTTPS enforcement, credentials in URLs, DNS resolution, private or reserved IPs, redirects, DNS rebinding, HLS segment URLs, and `EXT-X-KEY` URLs.
- The stated SSRF limitation in `SECURITY.md`. Do not claim that preflight DNS checks enforce every network request.
- DRM and authentication boundaries. The app must not bypass DRM, sign-in, paywalls, or access controls.
- Filename validation, path traversal, yt-dlp template expansion, environment-variable expansion, symlinks, existing destinations, and atomic no-replace publication.
- Temporary job ownership, crash recovery, stale-job cleanup, and the risk of deleting a user-created path.
- Child-process lifecycle, timeouts, kill and reap behavior, bounded stdout and stderr, oversized lines, and error redaction.
- Download concurrency, progress ownership, duplicate requests, window closure, and stale UI events.
- Tauri capability scope, content security policy, global API exposure, and unsafe HTML rendering.
- Dependency versions, committed lockfiles, build reproducibility, and Linux-only assumptions.
- Whether the UI accurately explains where files go and what the app refuses to do.

Use a fake executable or local fixture if a subprocess test is needed. Never make a live provider request.

## Run the available checks

Run these checks from the repository root:

```bash
cargo fmt --all -- --check
cargo test -p highvi-core --offline
cargo clippy -p highvi-core --offline -- -D warnings
node --check ui/app.js
node -e "for (const f of ['package.json','src-tauri/tauri.conf.json','src-tauri/capabilities/default.json']) JSON.parse(require('fs').readFileSync(f, 'utf8'))"
```

If the GTK and WebKitGTK development packages are installed, also run:

```bash
cargo check -p highvi --locked
```

Do not install system packages, use `sudo`, change a quality gate, or remove a lockfile. If a check cannot run, report it as blocked and quote the missing prerequisite without dumping unrelated logs.

## Write a visual README without inventing evidence

After the review, rewrite `README.md` in clear English for a public GitHub audience. A short Vietnamese summary is welcome, but keep one primary language and avoid duplicating the whole document.

The README must look polished in GitHub's renderer without remote scripts, generated marketing copy, or a large stack of decorative badges. Use the committed `src-tauri/icons/icon.png` as the project mark. Use centered HTML only for the compact hero section. Use normal Markdown for the rest.

Include these sections when the verified repository supports them:

1. A compact hero with the HighVi icon, the project name, one factual sentence, and a small set of useful badges.
2. One screenshot or short GIF only if you can capture the real app. Do not fabricate a UI image. If runtime capture is blocked, omit the screenshot and state the missing prerequisite in your final report, not in the README hero.
3. What HighVi does, expressed as the actual user flow.
4. Safety and permission boundaries near the top. State that users must have permission to download the media. State that HighVi does not bypass DRM, authentication, paywalls, or other access controls.
5. Current platform scope. Call this an Ubuntu desktop MVP. Do not imply that web, Android, iOS, macOS, or Windows builds work.
6. Prerequisites, including Rust, Node.js, `yt-dlp`, FFmpeg, and the Ubuntu packages required by Tauri v2.
7. Exact development, test, and release-build commands copied from the repository.
8. A small Mermaid diagram showing the verified flow: webview input, Rust validation, yt-dlp discovery, HLS validation, download job, atomic publication, and progress events.
9. The output location and no-overwrite behavior.
10. Security design with a link to `SECURITY.md` and an honest explanation of the redirect and HLS child-request SSRF limitation.
11. A concise repository layout. Include only files and directories that exist.
12. Troubleshooting for missing `yt-dlp`, FFmpeg, GTK or WebKitGTK, HTTP 403, no HLS format, and an existing output filename.
13. A focused roadmap that labels ideas as unimplemented. Do not promise mobile or web support.
14. Contributing guidance that asks for sanitized reproduction steps and forbids secrets, signed URLs, cookies, or copyrighted sample media in issues.
15. The MIT license with a link to `LICENSE`.

Use relative links that work on GitHub. Do not use absolute filesystem paths in the README. Do not add a badge for CI, coverage, releases, downloads, or supported platforms unless the repository has evidence for that badge. Do not add fake performance numbers, endorsements, compatibility claims, or screenshots.

## Writing standard

- Lead with what the program does.
- Use short, concrete sentences and sentence-case headings.
- Prefer commands and observable results over adjectives.
- Avoid filler, hype, decorative emoji, repeated warnings, and phrases such as "powerful", "seamless", "revolutionary", or "production-ready".
- Keep the README useful when read without this prompt.
- Make every command copyable.
- Keep architecture and security claims consistent with the code and `SECURITY.md`.
- Use alt text for every image.
- Check every relative link and every referenced path before finishing.

## Acceptance criteria

The task is complete only when:

- only `README.md` changed;
- the code review covers the trust boundaries above;
- every README feature claim is backed by code or a passing check;
- the README renders with a clear hierarchy on GitHub;
- all available checks pass, or the final report names the exact blocked check;
- no real media site was contacted;
- no secrets, cookies, signed URLs, or raw provider errors appear in the diff or report.

In your final response, list:

1. code-review findings first, ordered by severity;
2. the README sections you changed;
3. every command you ran and its result;
4. blocked coverage;
5. residual security risks;
6. confirmation that you did not contact a real media URL and changed only `README.md`.

Do not commit, push, publish, merge, release, or change repository visibility.
