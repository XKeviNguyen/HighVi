//! Finds HLS playlists that a page's own JavaScript loads, by watching the
//! network traffic of an isolated headless Chrome.
//!
//! Isolation: a fresh profile inside the job directory, no user profile,
//! cookies or extensions; the DevTools protocol runs over a pipe
//! (`--remote-debugging-pipe`), so no debugging port is ever opened; every
//! request goes through the job's egress proxy, and local DNS resolution is
//! disabled so nothing can bypass it. Chrome's own sandbox stays on. The
//! page runs in Chrome only, never in HighVi's privileged webview, and found
//! URLs stay in Rust.

use crate::{
    PublicError,
    hls::{PlaylistKind, inspect_playlist},
    process::{CancelToken, kill_group},
    referer_origin,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::pipe,
    process::{Child, Command},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc},
    time::{Instant, sleep_until, timeout},
};
use url::Url;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
/// Hard limit for one page, from navigation to the last observed request.
const PAGE_BUDGET: Duration = Duration::from_secs(30);
/// How long a visible browser window waits for the user to start the video.
pub const INTERACTIVE_BUDGET: Duration = Duration::from_secs(10 * 60);
/// Without candidates this long after load, try starting the page's players.
const PLAY_AFTER_LOAD: Duration = Duration::from_secs(4);
/// …or this long after navigating when the page never finishes loading.
const PLAY_WITHOUT_LOAD: Duration = Duration::from_secs(12);
/// Give up this long after starting the players if still nothing appeared.
const GIVE_UP_AFTER_PLAY: Duration = Duration::from_secs(8);
/// Stop this long after the last new candidate.
const SETTLE: Duration = Duration::from_secs(2);
/// Replies to HighVi's own commands (playlist bodies, base64-encoded).
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
/// Events the page causes; larger ones are skipped without being parsed.
const MAX_EVENT_BYTES: usize = 1024 * 1024;
/// Parsed messages waiting in the channel and the backlog together, by
/// weight: a reply (one long string) weighs its length; an event weighs
/// [`EVENT_WEIGHT`] times its length, because parsed JSON with many small
/// values takes several times its text size.
const MAX_QUEUED_WEIGHT: usize = 64 * 1024 * 1024;
const MAX_BACKLOG_WEIGHT: usize = 16 * 1024 * 1024;
const EVENT_WEIGHT: usize = 8;
/// Request URLs held for tracking, in total.
const MAX_TRACKED_URL_TOTAL: usize = 8 * 1024 * 1024;
const MAX_SESSIONS: usize = 48;
const MAX_TRACKED_REQUESTS: usize = 4_000;
/// Body reads for responses named or typed as HLS, and separately for
/// generic responses that are only sniffed, so analytics calls cannot use
/// up the budget a real playlist needs.
const MAX_PLAYLIST_FETCHES: usize = 40;
const MAX_SNIFF_FETCHES: usize = 40;
/// Longer URLs are ignored: they are not playlists a player requests, and
/// a page could otherwise make HighVi hold gigabytes of them.
const MAX_TRACKED_URL_BYTES: usize = 8 * 1024;
const MAX_BACKLOG: usize = 256;
const MAX_SNIFF_BYTES: f64 = 512.0 * 1024.0;
const MAX_PLAYLIST_BYTES: f64 = 8.0 * 1024.0 * 1024.0;
const MAX_TITLE_CHARS: usize = 200;
pub const MAX_CANDIDATES: usize = 3;
/// Playlist bodies held at once (oldest dropped first), so a page that keeps
/// loading playlists in a 10-minute window cannot grow HighVi's memory.
const MAX_STORED_PLAYLISTS: usize = 40;
const MAX_STORED_PLAYLIST_BYTES: usize = 32 * 1024 * 1024;
/// Rejected rounds after which a window gets no fresh body budgets.
const MAX_REJECTED_ROUNDS: u32 = 5;
/// Time a window's checks may take on top of its budget (3 candidates,
/// each checked by yt-dlp within its own 60 s limit).
const CHECKS_ALLOWANCE: Duration = Duration::from_secs(MAX_CANDIDATES as u64 * 60);

const HLS_MIME_TYPES: &[&str] = &[
    "application/vnd.apple.mpegurl",
    "application/x-mpegurl",
    "audio/mpegurl",
    "audio/x-mpegurl",
];
/// Generic types a playlist is sometimes served as; their bodies are sniffed.
const SNIFF_MIME_TYPES: &[&str] = &[
    "text/plain",
    "application/octet-stream",
    "binary/octet-stream",
    "application/binary",
    "",
];

/// Starts every `<video>`/`<audio>` of a document (and same-origin frames),
/// muted. Nothing else on the page is clicked.
const PLAY_SCRIPT: &str = r#"(() => {
  let started = 0;
  const visit = (doc) => {
    for (const media of doc.querySelectorAll("video, audio")) {
      media.muted = true;
      const playing = media.play();
      if (playing && playing.catch) playing.catch(() => {});
      started += 1;
    }
    for (const frame of doc.querySelectorAll("iframe")) {
      try { if (frame.contentDocument) visit(frame.contentDocument); } catch (_) {}
    }
  };
  visit(document);
  return started;
})()"#;

/// A playlist the page loaded, best first, with the Referer Chrome sent.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub url: Url,
    pub referer: Option<String>,
}

#[derive(Debug)]
pub struct BrowserFinding {
    pub candidates: Vec<Candidate>,
    pub title: Option<String>,
    /// Only DRM-signalling playlists were seen.
    pub drm_only: bool,
    pub page: PageReport,
}

/// What happened to the page, without any URL: enough to tell "the site
/// refused the browser" from "the page had no video" from "too slow".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PageReport {
    /// HTTP status of the main document's last response.
    pub document_status: Option<u16>,
    /// The main document was a Cloudflare challenge (`cf-mitigated: challenge`).
    pub bot_check: bool,
    /// The main document never arrived (network error, refused by the proxy).
    pub document_failed: bool,
    pub loaded: bool,
    pub played: bool,
    pub frames: usize,
    pub workers: usize,
    pub requests: usize,
    /// Responses typed as audio, video or HLS.
    pub media_responses: usize,
    pub playlists: usize,
    /// Playlist requests (by name or type) the server answered with 4xx.
    pub refused_playlists: usize,
    /// Events that did not fit HighVi's queue (the page flooded it).
    pub events_dropped: u64,
    /// Events too large to read (one huge request is not a flood).
    pub events_oversized: u64,
    /// Requests not tracked because the tracking limits were reached.
    pub untracked_requests: usize,
    pub ended: Ended,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Ended {
    /// Candidates were found and no new ones followed.
    #[default]
    Settled,
    /// Nothing appeared within a few seconds of starting the players.
    GaveUp,
    /// The page budget ran out.
    Deadline,
    /// Chrome exited or the page crashed.
    BrowserGone,
    /// The user closed the visible browser window.
    Closed,
}

impl Ended {
    pub fn label(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::GaveUp => "gave_up_after_play",
            Self::Deadline => "page_budget",
            Self::BrowserGone => "browser_gone",
            Self::Closed => "closed_by_user",
        }
    }
}

/// Checks the playlists a visible window loaded (best first, with the page
/// title) while it stays open; `Some` ends the session with that result.
/// The future may borrow only what outlives the session, not the function.
pub type Check<'a, T> = dyn FnMut(Vec<Candidate>, Option<String>) -> CheckFuture<'a, T> + Send + 'a;
/// The answer of a [`Check`].
pub type CheckFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<T>> + Send + 'a>>;

pub struct BrowserRun<'a> {
    pub chrome: &'a Path,
    pub page: &'a Url,
    pub proxy_url: &'a str,
    /// A private (0700) directory; Chrome's profile goes inside it.
    pub work_dir: &'a Path,
    pub mode: Mode,
    /// How long a visible window waits ([`INTERACTIVE_BUDGET`] by default).
    pub interactive_budget: Duration,
    #[cfg(test)]
    pub insecure_tls: bool,
}

/// How the isolated browser runs. Both use the same fresh profile, proxy,
/// sandbox and limits; only whether a window is shown differs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// No window, a few seconds per page: the automatic pipeline.
    Headless,
    /// A window the user browses in themselves (sites may refuse headless
    /// browsers). HighVi watches until a video stream appears, the window
    /// is closed, or [`INTERACTIVE_BUDGET`] runs out.
    Interactive,
}

/// Where a visible window goes: only what the display server needs. On
/// Wayland the socket is passed as an absolute path, so Chrome gets neither
/// the user's runtime directory nor their session bus.
#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub wayland: bool,
    pub env: Vec<(&'static str, OsString)>,
}

/// The desktop HighVi itself runs on, from its environment (`var`).
/// Wayland is preferred when both are available.
pub fn desktop_display(var: impl Fn(&str) -> Option<OsString>) -> Option<Display> {
    let set = |name: &str| var(name).filter(|value| !value.is_empty());
    if let Some(name) = set("WAYLAND_DISPLAY") {
        let socket = std::path::PathBuf::from(name);
        let socket = if socket.is_absolute() {
            Some(socket)
        } else {
            set("XDG_RUNTIME_DIR").map(|runtime| std::path::PathBuf::from(runtime).join(socket))
        };
        if let Some(socket) = socket {
            return Some(Display {
                wayland: true,
                env: vec![("WAYLAND_DISPLAY", socket.into_os_string())],
            });
        }
    }
    let mut env = vec![("DISPLAY", set("DISPLAY")?)];
    // Chrome's HOME is its profile, so X11's default cookie path would be wrong.
    if let Some(cookie) = set("XAUTHORITY").or_else(|| {
        set("HOME").map(|home| {
            std::path::Path::new(&home)
                .join(".Xauthority")
                .into_os_string()
        })
    }) {
        env.push(("XAUTHORITY", cookie));
    }
    Some(Display {
        wayland: false,
        env,
    })
}

/// The first Chrome or Chromium on `PATH`.
pub fn find_browser() -> Option<std::path::PathBuf> {
    [
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
    ]
    .into_iter()
    .find_map(crate::find_executable)
}

/// Runs one browser session. In a visible window, `check` is given each new
/// set of playlists (best first) and the page title while the window stays
/// open; the session ends when it returns `Some`. Headless sessions never
/// call it: they end once playlists settle, and the caller checks them.
pub async fn discover_in_browser<T>(
    run: BrowserRun<'_>,
    cancel: &mut CancelToken,
    check: &mut Check<'_, T>,
) -> Result<(BrowserFinding, Option<T>), PublicError> {
    // Tests never use the desktop HighVi happens to run on: only a display
    // they name (an Xvfb), so no window opens on someone's screen.
    #[cfg(not(test))]
    let desktop = |name: &str| std::env::var_os(name);
    #[cfg(test)]
    let desktop = |name: &str| {
        (name == "DISPLAY")
            .then(|| std::env::var_os("HIGHVI_TEST_DISPLAY"))
            .flatten()
    };
    let display = match run.mode {
        Mode::Headless => None,
        Mode::Interactive => Some(desktop_display(desktop).ok_or_else(no_display)?),
    };
    let profile = run.work_dir.join("chrome-profile");
    write_profile_preferences(&profile)
        .await
        .map_err(|_| browser_failed())?;
    write_launch_guards(&profile)
        .await
        .map_err(|_| browser_failed())?;
    if display.is_some() {
        // Chrome's own runtime directory, instead of the user's.
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(profile.join("run"))
            .map_err(|_| browser_failed())?;
    }
    let (mut child, mut cdp) = launch(&run, &profile, display.as_ref()).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            browser_unavailable()
        } else {
            browser_failed()
        }
    })?;

    // Every CDP wait inside is bounded too; this is the backstop for a
    // browser that stops answering altogether.
    let budget = match run.mode {
        Mode::Headless => PAGE_BUDGET,
        Mode::Interactive => run.interactive_budget,
    };
    let allowance = match run.mode {
        Mode::Headless => Duration::ZERO,
        Mode::Interactive => CHECKS_ALLOWANCE,
    };
    let bounded = timeout(
        STARTUP_TIMEOUT + budget + allowance + Duration::from_secs(10),
        observe(&mut cdp, run.page, run.mode, budget, check),
    );
    let mut result = tokio::select! {
        result = bounded => result.unwrap_or_else(|_| Err(browser_failed())),
        () = cancel.cancelled() => Err(PublicError::new("cancelled", "The download was cancelled.")),
    };
    if run.mode == Mode::Interactive {
        // Closed by the user only if Chrome exited cleanly; a crash or a
        // kill (non-zero status, signal) is a failure. A window closed
        // while HighVi was still setting it up counts as closed too.
        let ended_closed =
            matches!(&result, Ok((finding, None)) if finding.page.ended == Ended::Closed);
        let setup_failed = matches!(&result, Err(error) if error.code == "browser_failed");
        if ended_closed || setup_failed {
            // Still running: the user closed the watched tab with others
            // open, which is a close too. Exited: only a clean exit is.
            let clean = match timeout(Duration::from_secs(2), child.wait()).await {
                Ok(Ok(status)) => status.success(),
                Ok(Err(_)) => false,
                Err(_) => ended_closed,
            };
            if setup_failed && clean {
                result = Err(browser_closed());
            } else if ended_closed && !clean {
                result = Err(browser_failed());
            }
        }
    }
    drop(cdp);
    kill_group(&mut child).await;
    result
}

/// A fresh profile whose preferences forbid WebRTC from sending UDP that
/// does not go through the proxy (Chrome's proxy only carries TCP).
async fn write_profile_preferences(profile: &Path) -> io::Result<()> {
    let default = profile.join("Default");
    tokio::fs::create_dir_all(&default).await?;
    let preferences = json!({
        "webrtc": {
            "ip_handling_policy": "disable_non_proxied_udp",
            "multiple_routes_enabled": false,
            "nonproxied_udp_enabled": false
        }
    });
    tokio::fs::write(default.join("Preferences"), preferences.to_string()).await
}

/// Programs Chrome hands links it cannot open itself to (`mailto:`,
/// `magnet:`, …). A page must not start desktop apps, which would reach the
/// network outside the egress guard: these stand-ins, first on Chrome's
/// `PATH`, refuse everything.
const LAUNCH_GUARDS: &[&str] = &[
    "xdg-open",
    "xdg-email",
    "xdg-mime",
    "xdg-settings",
    "gio",
    "gvfs-open",
    "gnome-open",
    "kde-open",
    "kde-open5",
    "kfmclient",
    "exo-open",
    "sensible-browser",
    "x-www-browser",
    "www-browser",
];

async fn write_launch_guards(profile: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let bin = profile.join("bin");
    tokio::fs::create_dir_all(&bin).await?;
    for name in LAUNCH_GUARDS {
        let path = bin.join(name);
        tokio::fs::write(&path, "#!/bin/sh\nexit 1\n").await?;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(())
}

/// A browser installed as a snap (`/snap/bin/chromium` is a link to
/// `/usr/bin/snap`).
fn is_snap(chrome: &Path) -> bool {
    chrome.starts_with("/snap/")
        || std::fs::canonicalize(chrome)
            .is_ok_and(|real| real.starts_with("/snap/") || real == Path::new("/usr/bin/snap"))
}

/// Chrome's `PATH`: the stand-ins first, then HighVi's own.
fn guarded_path(profile: &Path, inherited: Option<OsString>) -> OsString {
    let mut path = profile.join("bin").into_os_string();
    if let Some(inherited) = inherited.filter(|inherited| !inherited.is_empty()) {
        path.push(":");
        path.push(inherited);
    }
    path
}

pub(crate) fn browser_unavailable() -> PublicError {
    PublicError::new(
        "browser_unavailable",
        "Google Chrome or Chromium is needed to find videos on this page.",
    )
}

pub(crate) fn browser_closed() -> PublicError {
    PublicError::new(
        "browser_closed",
        "The HighVi browser window was closed before a video started playing.",
    )
}

fn no_display() -> PublicError {
    PublicError::new(
        "browser_no_display",
        "HighVi could not find your desktop's display to open its browser window.",
    )
}

pub(crate) fn browser_failed() -> PublicError {
    PublicError::new(
        "browser_failed",
        "The isolated browser HighVi uses to find videos stopped unexpectedly.",
    )
}

pub fn chrome_args(
    run: &BrowserRun<'_>,
    profile: &Path,
    display: Option<&Display>,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--remote-debugging-pipe",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-extensions",
        "--disable-component-extensions-with-background-pages",
        "--disable-default-apps",
        "--disable-sync",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-domain-reliability",
        "--disable-client-side-phishing-detection",
        "--disable-breakpad",
        "--no-pings",
        "--metrics-recording-only",
        "--password-store=basic",
        "--use-mock-keychain",
        "--deny-permission-prompts",
        "--mute-audio",
        "--autoplay-policy=no-user-gesture-required",
        "--disable-quic",
        // Chrome has no --force-… variant of this switch; the profile
        // preferences below repeat the policy as a second guard.
        "--webrtc-ip-handling-policy=disable_non_proxied_udp",
        "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication,DialMediaRouteProvider,NetworkTimeServiceQuerying",
        "--window-size=1280,720",
        // All traffic goes to the egress proxy, loopback included (Chrome
        // bypasses proxies for loopback unless told not to). Chrome never
        // needs to resolve a name itself, so local resolution is disabled.
        "--proxy-bypass-list=<-loopback>",
        "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    args.push(match display {
        None => "--headless=new".into(),
        Some(display) if display.wayland => "--ozone-platform=wayland".into(),
        Some(_) => "--ozone-platform=x11".into(),
    });
    args.push(format!("--proxy-server={}", run.proxy_url).into());
    let mut profile_arg = OsString::from("--user-data-dir=");
    profile_arg.push(profile.as_os_str());
    args.push(profile_arg);
    #[cfg(test)]
    if run.insecure_tls {
        args.push("--ignore-certificate-errors".into());
        if std::env::var_os("HIGHVI_TEST_CHROME_NO_SANDBOX").is_some() {
            args.push("--no-sandbox".into());
        }
    }
    args.push("about:blank".into());
    args
}

fn launch(
    run: &BrowserRun<'_>,
    profile: &Path,
    display: Option<&Display>,
) -> io::Result<(Child, Cdp)> {
    let (to_chrome_read, to_chrome_write) = pipe_pair()?;
    let (from_chrome_read, from_chrome_write) = pipe_pair()?;
    let child_read = to_chrome_read.as_raw_fd();
    let child_write = from_chrome_write.as_raw_fd();

    let mut command = Command::new(run.chrome);
    command
        .kill_on_drop(true)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_clear()
        .envs(
            ["LANG", "LC_ALL", "TMPDIR"]
                .into_iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (name, value))),
        )
        .env("PATH", guarded_path(profile, std::env::var_os("PATH")))
        .env("HOME", profile)
        .env("XDG_CONFIG_HOME", profile.join("config"))
        .env("XDG_CACHE_HOME", profile.join("cache"))
        .args(chrome_args(run, profile, display));
    if let Some(display) = display {
        command.envs(display.env.iter().map(|(name, value)| (*name, value)));
        // No session bus: no keyring, portals or other desktop services.
        // A snap is the exception: snapd needs the user's session to start
        // the app at all, so a snap gets what a headless one does (neither
        // variable), and its confinement decides what it may reach.
        if !is_snap(run.chrome) {
            command
                .env("XDG_RUNTIME_DIR", profile.join("run"))
                .env("DBUS_SESSION_BUS_ADDRESS", "disabled:");
        }
    }
    // Chrome also exits when its pipe closes; this covers the gap (a snap's
    // setuid launcher clears the setting, so both are kept).
    crate::process::die_with_parent(&mut command);
    // SAFETY: only async-signal-safe calls between fork and exec. The source
    // descriptors are >= 10 (see `pipe_pair`), so neither dup2 can clobber
    // the other's source; dup2 clears close-on-exec on 3 and 4 only.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(child_read, 3) < 0 || libc::dup2(child_write, 4) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    drop(to_chrome_read);
    drop(from_chrome_write);
    let cdp = Cdp::new(
        pipe::Sender::from_owned_fd(to_chrome_write)?,
        pipe::Receiver::from_owned_fd(from_chrome_read)?,
    );
    Ok((child, cdp))
}

/// A close-on-exec pipe whose descriptors are both >= 10.
fn pipe_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: `fds` is a valid two-element array.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 returned two fresh descriptors that we now own.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    Ok((raise(read)?, raise(write)?))
}

fn raise(fd: OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: F_DUPFD_CLOEXEC on a descriptor we own returns a new one.
    let raised = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if raised < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raised` is a fresh descriptor that we now own.
    Ok(unsafe { OwnedFd::from_raw_fd(raised) })
}

/// A parsed message and its share of the queue's byte budget, returned when
/// the message is dropped.
struct Queued {
    value: Value,
    weight: usize,
    _permit: OwnedSemaphorePermit,
}

/// A minimal DevTools-protocol client: NUL-delimited JSON over two pipes.
///
/// Memory is bounded by bytes, not only message counts, because a hostile
/// page decides how large events are: events over [`MAX_EVENT_BYTES`] are
/// skipped unparsed (counted in `oversized`), everything queued (channel and
/// backlog) shares [`MAX_QUEUED_WEIGHT`], and the backlog keeps at most
/// [`MAX_BACKLOG_WEIGHT`]; events that do not fit are counted in `dropped`.
struct Cdp {
    writer: pipe::Sender,
    incoming: mpsc::Receiver<Queued>,
    backlog: VecDeque<Queued>,
    backlog_weight: usize,
    dropped: AtomicU64,
    oversized: Arc<AtomicU64>,
    next_id: u64,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Cdp {
    fn new(writer: pipe::Sender, receiver: pipe::Receiver) -> Self {
        // Small: Chrome then buffers in its own process, not in HighVi.
        let (sender, incoming) = mpsc::channel(64);
        let oversized = Arc::new(AtomicU64::new(0));
        let reader = tokio::spawn(read_messages(
            receiver,
            sender,
            Arc::new(Semaphore::new(MAX_QUEUED_WEIGHT)),
            Arc::clone(&oversized),
        ));
        Self {
            writer,
            incoming,
            backlog: VecDeque::new(),
            backlog_weight: 0,
            dropped: AtomicU64::new(0),
            oversized,
            next_id: 1,
            reader,
        }
    }

    /// Events that did not fit the queue.
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Events too large to read at all.
    fn oversized(&self) -> u64 {
        self.oversized.load(Ordering::Relaxed)
    }

    async fn send(
        &mut self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> io::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_string());
        }
        let mut bytes = serde_json::to_vec(&message).map_err(io::Error::other)?;
        bytes.push(0);
        self.writer.write_all(&bytes).await?;
        Ok(id)
    }

    /// Sends a command and waits for its result; other messages are kept for
    /// [`Cdp::next`].
    async fn call(&mut self, session: Option<&str>, method: &str, params: Value) -> Option<Value> {
        let id = self.send(session, method, params).await.ok()?;
        loop {
            let message = self.incoming.recv().await?;
            if message.value.get("id").and_then(Value::as_u64) == Some(id) {
                return message.value.get("result").cloned();
            }
            // Events that arrive while waiting are kept, within bounds.
            if self.backlog.len() < MAX_BACKLOG
                && self.backlog_weight + message.weight <= MAX_BACKLOG_WEIGHT
            {
                self.backlog_weight += message.weight;
                self.backlog.push_back(message);
            } else {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    async fn next(&mut self) -> Option<Value> {
        let message = match self.backlog.pop_front() {
            Some(message) => {
                self.backlog_weight -= message.weight;
                message
            }
            None => self.incoming.recv().await?,
        };
        Some(message.value)
    }
}

/// Splits Chrome's output into messages and queues them. Events longer
/// than [`MAX_EVENT_BYTES`] (and replies longer than [`MAX_RESPONSE_BYTES`])
/// are skipped up to their delimiter without being buffered or parsed.
async fn read_messages(
    mut receiver: pipe::Receiver,
    sender: mpsc::Sender<Queued>,
    budget: Arc<Semaphore>,
    oversized: Arc<AtomicU64>,
) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut skipping = false;
    let mut chunk = vec![0_u8; 64 * 1024];
    loop {
        let read = match receiver.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        let mut data = &chunk[..read];
        while !data.is_empty() {
            let Some(end) = data.iter().position(|byte| *byte == 0) else {
                if !skipping {
                    buffer.extend_from_slice(data);
                    if buffer.len() > message_limit(&buffer) {
                        buffer = Vec::new();
                        skipping = true;
                    }
                }
                break;
            };
            if skipping {
                oversized.fetch_add(1, Ordering::Relaxed);
                skipping = false;
            } else {
                buffer.extend_from_slice(&data[..end]);
                if buffer.len() > message_limit(&buffer) {
                    oversized.fetch_add(1, Ordering::Relaxed);
                } else if let Ok(value) = serde_json::from_slice::<Value>(&buffer) {
                    let weight = buffer.len().max(1)
                        * if message_limit(&buffer) == MAX_RESPONSE_BYTES {
                            1
                        } else {
                            EVENT_WEIGHT
                        };
                    let permits = u32::try_from(weight).unwrap_or(u32::MAX);
                    // Waits while the queue is full: Chrome then buffers.
                    let Ok(permit) = Arc::clone(&budget).acquire_many_owned(permits).await else {
                        return;
                    };
                    let queued = Queued {
                        value,
                        weight,
                        _permit: permit,
                    };
                    if sender.send(queued).await.is_err() {
                        return;
                    }
                }
                buffer.clear();
                if buffer.capacity() > MAX_EVENT_BYTES {
                    buffer = Vec::new();
                }
            }
            data = &data[end + 1..];
        }
    }
}

/// Replies to commands start with their `id`; anything else is an event.
fn message_limit(start: &[u8]) -> usize {
    if start.starts_with(b"{\"id\":") {
        MAX_RESPONSE_BYTES
    } else {
        MAX_EVENT_BYTES
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Tracked {
    Yes,
    /// Ignored by design (no URL, or one longer than any playlist URL).
    Ignored,
    /// Not tracked because the limits are reached: worth reporting.
    OverBudget,
}

/// Requests being watched, bounded by count and by total URL bytes.
#[derive(Debug, Default)]
struct RequestTracker {
    requests: HashMap<(String, String), Request>,
    url_bytes: usize,
}

impl RequestTracker {
    /// Starts tracking `key`. A redirect reuses its request's key: the old
    /// URL is dropped whatever happens to the new one, and since a redirect
    /// adds no request it only has to fit the byte budget, which an equal
    /// or shorter URL always does.
    fn start(
        &mut self,
        key: (String, String),
        url: &str,
        referer: Option<String>,
        resource_type: &str,
    ) -> Tracked {
        let redirected = self.finish(&key).is_some();
        if url.is_empty() || url.len() > MAX_TRACKED_URL_BYTES {
            return Tracked::Ignored;
        }
        if (!redirected && self.requests.len() >= MAX_TRACKED_REQUESTS)
            || self.url_bytes + url.len() > MAX_TRACKED_URL_TOTAL
        {
            return Tracked::OverBudget;
        }
        self.url_bytes += url.len();
        self.requests.insert(
            key,
            Request {
                url: url.to_string(),
                referer,
                resource_type: resource_type.to_string(),
                want: Want::Nothing,
            },
        );
        Tracked::Yes
    }

    fn get_mut(&mut self, key: &(String, String)) -> Option<&mut Request> {
        self.requests.get_mut(key)
    }

    /// Stops tracking `key` and returns what was known about it.
    /// Drops everything a session that went away was tracking.
    fn forget_session(&mut self, session: &str) {
        let mut freed = 0;
        self.requests.retain(|(owner, _), request| {
            let keep = owner != session;
            if !keep {
                freed += request.url.len();
            }
            keep
        });
        self.url_bytes -= freed;
    }

    fn finish(&mut self, key: &(String, String)) -> Option<Request> {
        let request = self.requests.remove(key)?;
        self.url_bytes -= request.url.len();
        Some(request)
    }
}

#[derive(Debug)]
struct Request {
    url: String,
    referer: Option<String>,
    resource_type: String,
    want: Want,
}

/// Whether a response's body should be read once it has loaded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Want {
    Nothing,
    /// Named or typed as HLS.
    Playlist,
    /// A small generic-typed response that might be a playlist.
    Sniff,
}

#[derive(Debug, Default)]
struct Observation {
    /// `(url, referer, body)`, in the order first seen.
    playlists: Vec<(Url, Option<String>, Option<String>)>,
    seen: HashSet<String>,
}

impl Observation {
    /// Keeps a playlist body, dropping the oldest beyond the limits.
    fn store(&mut self, url: Url, referer: Option<String>, body: String) {
        self.playlists.push((url, referer, Some(body)));
        let bytes = |playlists: &[(Url, Option<String>, Option<String>)]| {
            playlists
                .iter()
                .map(|(_, _, body)| body.as_ref().map_or(0, String::len))
                .sum::<usize>()
        };
        while self.playlists.len() > 1
            && (self.playlists.len() > MAX_STORED_PLAYLISTS
                || bytes(&self.playlists) > MAX_STORED_PLAYLIST_BYTES)
        {
            self.playlists.remove(0);
        }
    }
}

async fn observe<T>(
    cdp: &mut Cdp,
    page: &Url,
    mode: Mode,
    budget: Duration,
    check: &mut Check<'_, T>,
) -> Result<(BrowserFinding, Option<T>), PublicError> {
    let interactive = mode == Mode::Interactive;
    let target = timeout(
        STARTUP_TIMEOUT,
        cdp.call(None, "Target.createTarget", json!({ "url": "about:blank" })),
    )
    .await
    .ok()
    .flatten()
    .ok_or_else(browser_failed)?;
    let target_id = target["targetId"]
        .as_str()
        .ok_or_else(browser_failed)?
        .to_string();
    let attached = cdp
        .call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
        .await
        .ok_or_else(browser_failed)?;
    let main = attached["sessionId"]
        .as_str()
        .ok_or_else(browser_failed)?
        .to_string();
    if interactive {
        // One tab: Chrome's own start-up tab is closed, so the window shows
        // the page HighVi watches.
        if let Some(targets) = cdp.call(None, "Target.getTargets", json!({})).await
            && let Some(targets) = targets["targetInfos"].as_array()
        {
            for other in targets.iter().filter(|info| {
                info["type"].as_str() == Some("page")
                    && info["targetId"].as_str() != Some(target_id.as_str())
            }) {
                let _ = cdp
                    .call(
                        None,
                        "Target.closeTarget",
                        json!({ "targetId": other["targetId"] }),
                    )
                    .await;
            }
        }
    }
    // Pages may not save files, not even into the throw-away profile.
    cdp.call(
        None,
        "Browser.setDownloadBehavior",
        json!({ "behavior": "deny" }),
    )
    .await
    .ok_or_else(browser_failed)?;
    // Watch the network and attach to frames and workers before navigating,
    // so the first request of every frame is seen.
    for (method, params) in [
        ("Network.enable", json!({})),
        ("Page.enable", json!({})),
        // No native file dialog: a page must not get a window onto the
        // user's files. (The event it sends instead is ignored.)
        (
            "Page.setInterceptFileChooserDialog",
            json!({ "enabled": true }),
        ),
        (
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        ),
    ] {
        cdp.call(Some(&main), method, params)
            .await
            .ok_or_else(browser_failed)?;
    }
    // Popups and new tabs are pages of their own: hear about them, so a
    // player that opens in one is watched too.
    cdp.call(
        None,
        "Target.setDiscoverTargets",
        json!({ "discover": true }),
    )
    .await
    .ok_or_else(browser_failed)?;
    let mut known_pages: HashSet<String> = HashSet::from([target_id.clone()]);
    cdp.send(
        Some(&main),
        "Page.navigate",
        json!({ "url": page.as_str() }),
    )
    .await
    .map_err(|_| browser_failed())?;

    let started = Instant::now();
    let deadline = started + budget;
    let mut report = PageReport {
        ended: Ended::Deadline,
        ..PageReport::default()
    };
    // Chrome names a page's main frame after its target.
    let mut main_document: Option<String> = None;

    let mut sessions: Vec<String> = vec![main.clone()];
    let mut requests = RequestTracker::default();
    let mut pending_bodies: HashMap<u64, (Url, Option<String>)> = HashMap::new();
    let mut playlist_fetches = 0;
    let mut sniff_fetches = 0;
    let mut observation = Observation::default();
    let mut loaded_at: Option<Instant> = None;
    let mut played_at: Option<Instant> = None;
    let mut last_found: Option<Instant> = None;
    // Visible window: playlists already checked and rejected, and the
    // first that passed.
    let mut checked: HashSet<String> = HashSet::new();
    let mut accepted: Option<T> = None;
    let mut rejected_rounds = 0;

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if let Some(found) = last_found
            && now >= found + SETTLE
            && pending_bodies.is_empty()
        {
            if !interactive {
                report.ended = Ended::Settled;
                break;
            }
            // The window stays open while new playlists are checked: an
            // advert or preview may come first, and the user may still be
            // starting the real video.
            last_found = None;
            let fresh = rank_skipping(&observation.playlists, &checked).0;
            if !fresh.is_empty() {
                checked.extend(
                    fresh
                        .iter()
                        .map(|candidate| candidate.url.as_str().to_string()),
                );
                let title = page_title(cdp, &main).await;
                if let Some(found) = check(fresh, title).await {
                    accepted = Some(found);
                    report.ended = Ended::Settled;
                    break;
                }
                // Rejected: forget those bodies, and give what the user
                // does next fresh body budgets, a few times.
                observation
                    .playlists
                    .retain(|(url, _, _)| !checked.contains(url.as_str()));
                rejected_rounds += 1;
                if rejected_rounds <= MAX_REJECTED_ROUNDS {
                    playlist_fetches = 0;
                    sniff_fetches = 0;
                }
                // Playlists that ranked below the ones just rejected are
                // next, without waiting for a new one to appear.
                if !rank_skipping(&observation.playlists, &checked).0.is_empty() {
                    last_found = Instant::now().checked_sub(SETTLE);
                }
            }
            continue;
        }
        let nothing_yet = observation.playlists.is_empty() && pending_bodies.is_empty();
        // The user may need minutes to get the video playing.
        if let Some(at) = played_at
            && !interactive
            && nothing_yet
            && now >= at + GIVE_UP_AFTER_PLAY
        {
            report.ended = Ended::GaveUp;
            break;
        }
        let play_at = loaded_at.map_or(started + PLAY_WITHOUT_LOAD, |at| at + PLAY_AFTER_LOAD);
        // Only headless sessions start players, with a synthetic gesture.
        // In a visible window the user does: a gesture HighVi made would
        // let the page use it (clipboard, external links) without a click.
        if !interactive && played_at.is_none() && nothing_yet && now >= play_at {
            played_at = Some(now);
            for session in &sessions {
                let _ = cdp
                    .send(
                        Some(session),
                        "Runtime.evaluate",
                        // No user gesture: muted playback needs none
                        // (--autoplay-policy), and a gesture HighVi made
                        // would let the page open links in desktop apps
                        // or write the clipboard without a click.
                        json!({ "expression": PLAY_SCRIPT }),
                    )
                    .await;
            }
        }
        let wake = [
            Some(deadline),
            last_found.map(|found| found + SETTLE),
            if played_at.is_none() && !interactive {
                Some(play_at)
            } else {
                None
            },
            played_at
                .filter(|_| !interactive)
                .map(|at| at + GIVE_UP_AFTER_PLAY),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(deadline);

        let message = tokio::select! {
            message = cdp.next() => message,
            () = sleep_until(wake) => continue,
        };
        let Some(message) = message else {
            // Chrome exited: in a visible window, because the user closed
            // it. Keep what was seen, if anything.
            if interactive {
                report.ended = Ended::Closed;
                break;
            }
            if observation.playlists.is_empty() {
                return Err(browser_failed());
            }
            report.ended = Ended::BrowserGone;
            break;
        };

        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            if let Some((url, referer)) = pending_bodies.remove(&id)
                && let Some(body) = response_body(&message)
                && inspect_playlist(&body, &url).is_some()
                && observation.seen.insert(url.as_str().to_string())
            {
                observation.store(url, referer, body);
                last_found = Some(Instant::now());
            }
            continue;
        }
        let session = message
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = &message["params"];
        match message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "Target.attachedToTarget" => {
                let Some(child) = params["sessionId"].as_str().map(str::to_string) else {
                    continue;
                };
                let kind = params["targetInfo"]["type"].as_str().unwrap_or_default();
                match kind {
                    "iframe" => report.frames += 1,
                    "worker" | "shared_worker" | "service_worker" => report.workers += 1,
                    _ => {}
                }
                if sessions.len() < MAX_SESSIONS
                    && matches!(
                        kind,
                        "iframe" | "worker" | "shared_worker" | "service_worker" | "page"
                    )
                {
                    let _ = cdp.send(Some(&child), "Network.enable", json!({})).await;
                    if matches!(kind, "iframe" | "page") {
                        let _ = cdp
                            .send(
                                Some(&child),
                                "Page.setInterceptFileChooserDialog",
                                json!({ "enabled": true }),
                            )
                            .await;
                        let _ = cdp
                            .send(
                                Some(&child),
                                "Target.setAutoAttach",
                                json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
                            )
                            .await;
                    }
                    sessions.push(child.clone());
                }
                let _ = cdp
                    .send(Some(&child), "Runtime.runIfWaitingForDebugger", json!({}))
                    .await;
            }
            // A tab is attached once it shows a web page: a new tab starts
            // as chrome://newtab and only later navigates.
            "Target.targetCreated" | "Target.targetInfoChanged" => {
                let info = &params["targetInfo"];
                let internal = info["url"].as_str().is_some_and(|url| {
                    [
                        "chrome:",
                        "chrome-untrusted:",
                        "devtools:",
                        "chrome-extension:",
                    ]
                    .iter()
                    .any(|scheme| url.starts_with(scheme))
                });
                if info["type"].as_str() == Some("page")
                    && !internal
                    && let Some(id) = info["targetId"].as_str()
                    && sessions.len() < MAX_SESSIONS
                    && known_pages.insert(id.to_string())
                {
                    let _ = cdp
                        .send(
                            None,
                            "Target.attachToTarget",
                            json!({ "targetId": id, "flatten": true }),
                        )
                        .await;
                }
            }
            "Target.detachedFromTarget" => {
                // A frame, worker or tab went away: stop counting it.
                if let Some(gone) = params["sessionId"].as_str()
                    && gone != main
                {
                    sessions.retain(|session| session != gone);
                    requests.forget_session(gone);
                }
            }
            "Page.loadEventFired" if session == main => {
                loaded_at.get_or_insert_with(Instant::now);
            }
            "Inspector.detached" if session == main && interactive => {
                report.ended = Ended::Closed;
                break;
            }
            "Inspector.targetCrashed" | "Inspector.detached" if session == main => {
                report.ended = Ended::BrowserGone;
                break;
            }
            "Network.requestWillBeSent" => {
                report.requests += 1;
                if session == main
                    && params["type"].as_str() == Some("Document")
                    && params["frameId"].as_str() == Some(target_id.as_str())
                    && let Some(id) = params["requestId"].as_str()
                {
                    // Redirects keep the ID; a new navigation replaces it.
                    main_document = Some(id.to_string());
                }
                let Some(id) = params["requestId"].as_str() else {
                    continue;
                };
                let request = &params["request"];
                let tracked = requests.start(
                    (session, id.to_string()),
                    request["url"].as_str().unwrap_or_default(),
                    header(&request["headers"], "referer").and_then(origin_referer),
                    params["type"].as_str().unwrap_or_default(),
                );
                if tracked == Tracked::OverBudget {
                    report.untracked_requests += 1;
                }
            }
            "Network.loadingFailed" => {
                if let Some(id) = params["requestId"].as_str() {
                    // A navigation that replaces the document cancels it.
                    if session == main
                        && main_document.as_deref() == Some(id)
                        && params["canceled"].as_bool() != Some(true)
                    {
                        report.document_failed = true;
                    }
                    requests.finish(&(session, id.to_string()));
                }
            }
            "Network.responseReceived" => {
                let Some(id) = params["requestId"].as_str() else {
                    continue;
                };
                let response = &params["response"];
                if session == main && main_document.as_deref() == Some(id) {
                    report.document_status = response["status"]
                        .as_u64()
                        .and_then(|status| u16::try_from(status).ok());
                    report.bot_check = header(&response["headers"], "cf-mitigated")
                        .is_some_and(|value| value.trim().eq_ignore_ascii_case("challenge"));
                    report.document_failed = false;
                }
                let status = response["status"].as_u64().unwrap_or(0);
                let mime = response["mimeType"]
                    .as_str()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let by_type = HLS_MIME_TYPES.contains(&mime.as_str());
                if mime.starts_with("video/") || mime.starts_with("audio/") || by_type {
                    report.media_responses += 1;
                }
                let Some(request) = requests.get_mut(&(session.clone(), id.to_string())) else {
                    continue;
                };
                let Ok(url) = Url::parse(&request.url) else {
                    continue;
                };
                let by_name = url.path().to_ascii_lowercase().ends_with(".m3u8");
                if (by_name || by_type) && (400..500).contains(&status) {
                    report.refused_playlists += 1;
                }
                if !(200..300).contains(&status) || status == 204 || url.scheme() != "https" {
                    continue;
                }
                let sniffable = SNIFF_MIME_TYPES.contains(&mime.as_str())
                    && matches!(
                        request.resource_type.as_str(),
                        "XHR" | "Fetch" | "Other" | "Media"
                    );
                if !observation.seen.contains(url.as_str()) {
                    request.want = if by_name || by_type {
                        Want::Playlist
                    } else if sniffable {
                        Want::Sniff
                    } else {
                        Want::Nothing
                    };
                }
            }
            "Network.loadingFinished" => {
                let Some(id) = params["requestId"].as_str() else {
                    continue;
                };
                // Finished: nothing more to learn about this request.
                let Some(request) = requests.finish(&(session.clone(), id.to_string())) else {
                    continue;
                };
                let size = params["encodedDataLength"].as_f64().unwrap_or(0.0);
                let budget = match request.want {
                    Want::Playlist if size <= MAX_PLAYLIST_BYTES => {
                        (&mut playlist_fetches, MAX_PLAYLIST_FETCHES)
                    }
                    Want::Sniff if size > 0.0 && size <= MAX_SNIFF_BYTES => {
                        (&mut sniff_fetches, MAX_SNIFF_FETCHES)
                    }
                    _ => continue,
                };
                if *budget.0 >= budget.1 {
                    continue;
                }
                *budget.0 += 1;
                let Ok(url) = Url::parse(&request.url) else {
                    continue;
                };
                let referer = request.referer;
                if let Ok(command) = cdp
                    .send(
                        Some(&session),
                        "Network.getResponseBody",
                        json!({ "requestId": id }),
                    )
                    .await
                {
                    pending_bodies.insert(command, (url, referer));
                }
            }
            _ => {}
        }
    }

    let title = page_title(cdp, &main).await;

    report.events_dropped = cdp.dropped();
    report.events_oversized = cdp.oversized();
    report.loaded = loaded_at.is_some();
    report.played = played_at.is_some();
    report.playlists = observation.playlists.len();
    let (candidates, drm_only) = rank(&observation.playlists);
    Ok((
        BrowserFinding {
            candidates,
            title,
            drm_only,
            page: report,
        },
        accepted,
    ))
}

/// The main document's title, trimmed and bounded.
async fn page_title(cdp: &mut Cdp, main: &str) -> Option<String> {
    timeout(
        Duration::from_secs(3),
        cdp.call(
            Some(main),
            "Runtime.evaluate",
            json!({ "expression": "document.title", "returnByValue": true }),
        ),
    )
    .await
    .ok()
    .flatten()
    .and_then(|result| result["result"]["value"].as_str().map(str::to_string))
    .map(|title| {
        title
            .trim()
            .chars()
            .take(MAX_TITLE_CHARS)
            .collect::<String>()
    })
    .filter(|title| !title.is_empty())
}

fn header<'a>(headers: &'a Value, name: &str) -> Option<&'a str> {
    headers
        .as_object()?
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .and_then(|(_, value)| value.as_str())
}

fn origin_referer(raw: &str) -> Option<String> {
    Url::parse(raw)
        .ok()
        .filter(|url| url.scheme() == "https" && url.host().is_some())
        .map(|url| referer_origin(&url))
}

/// The text of a `Network.getResponseBody` result. Chrome base64-encodes
/// bodies whose MIME type it does not consider text, HLS types included.
fn response_body(message: &Value) -> Option<String> {
    let result = message.get("result")?;
    let body = result["body"].as_str()?;
    if result["base64Encoded"].as_bool() == Some(true) {
        String::from_utf8(decode_base64(body)?).ok()
    } else {
        Some(body.to_string())
    }
}

/// Standard base64 with optional padding; `None` on any invalid input.
fn decode_base64(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut output = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    (bits < 6).then_some(output)
}

/// Orders observed playlists: masters first, then finished media playlists,
/// longest first (adverts are short), then the order the page loaded them.
/// Media playlists that belong to an observed master are folded into it,
/// and DRM-signalling playlists are dropped.
fn rank(playlists: &[(Url, Option<String>, Option<String>)]) -> (Vec<Candidate>, bool) {
    rank_skipping(playlists, &HashSet::new())
}

/// [`rank`], leaving out playlists already checked, before the best few are
/// taken (so rejected ones never hide later ones).
fn rank_skipping(
    playlists: &[(Url, Option<String>, Option<String>)],
    skip: &HashSet<String>,
) -> (Vec<Candidate>, bool) {
    struct Scored {
        candidate: Candidate,
        class: u8,
        duration: f64,
        order: usize,
    }
    let inspected: Vec<_> = playlists
        .iter()
        .map(|(url, referer, body)| {
            let info = body.as_deref().and_then(|body| inspect_playlist(body, url));
            (url, referer, info)
        })
        .collect();
    let drm_seen = inspected
        .iter()
        .any(|(_, _, info)| info.as_ref().is_some_and(|info| info.drm));
    let durations: HashMap<&str, f64> = inspected
        .iter()
        .filter_map(|(url, _, info)| match info.as_ref()?.kind {
            PlaylistKind::Media { duration, .. } => Some((url.as_str(), duration)),
            PlaylistKind::Master { .. } => None,
        })
        .collect();
    // Variants of every master, including DRM-signalling ones: a clear
    // media playlist that belongs to a DRM master must not stand alone.
    let children: HashSet<String> = inspected
        .iter()
        .filter_map(|(_, _, info)| match &info.as_ref()?.kind {
            PlaylistKind::Master { variants } => Some(variants),
            PlaylistKind::Media { .. } => None,
        })
        .flatten()
        .map(|variant| variant.as_str().to_string())
        .collect();
    let drm_urls: HashSet<&str> = inspected
        .iter()
        .filter(|(_, _, info)| info.as_ref().is_some_and(|info| info.drm))
        .map(|(url, _, _)| url.as_str())
        .collect();
    let mut scored = Vec::new();
    for (order, (url, referer, info)) in inspected.iter().enumerate() {
        if info.as_ref().is_some_and(|info| info.drm) {
            continue;
        }
        // A master whose observed variant signals DRM is DRM too.
        if let Some(PlaylistKind::Master { variants }) = info.as_ref().map(|info| &info.kind)
            && variants
                .iter()
                .any(|variant| drm_urls.contains(variant.as_str()))
        {
            continue;
        }
        let (class, duration) = match info.as_ref().map(|info| &info.kind) {
            Some(PlaylistKind::Master { variants }) => {
                let longest = variants
                    .iter()
                    .filter_map(|variant| durations.get(variant.as_str()).copied())
                    .fold(0.0, f64::max);
                (3, longest)
            }
            Some(PlaylistKind::Media {
                duration,
                ended: true,
            }) => (2, *duration),
            None => (1, 0.0),
            Some(PlaylistKind::Media { .. }) => (0, 0.0),
        };
        scored.push(Scored {
            candidate: Candidate {
                url: (*url).clone(),
                referer: (*referer).clone(),
            },
            class,
            duration,
            order,
        });
    }
    scored.retain(|scored| scored.class == 3 || !children.contains(scored.candidate.url.as_str()));
    scored.retain(|scored| !skip.contains(scored.candidate.url.as_str()));
    scored.sort_by(|left, right| {
        right
            .class
            .cmp(&left.class)
            .then(right.duration.total_cmp(&left.duration))
            .then(left.order.cmp(&right.order))
    });
    let drm_only = scored.is_empty() && drm_seen;
    (
        scored
            .into_iter()
            .take(MAX_CANDIDATES)
            .map(|scored| scored.candidate)
            .collect(),
        drm_only,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    fn media(seconds: &[f64], ended: bool) -> String {
        let mut body = String::from("#EXTM3U\n");
        for seconds in seconds {
            body.push_str(&format!("#EXTINF:{seconds},\nseg.ts\n"));
        }
        if ended {
            body.push_str("#EXT-X-ENDLIST\n");
        }
        body
    }

    #[test]
    fn masters_win_over_adverts_and_absorb_their_variants() {
        let (ranked, drm_only) = rank(&[
            (
                url("https://ads.example/ad.m3u8"),
                Some("https://ads.example/".into()),
                Some(media(&[5.0, 5.0], true)),
            ),
            (
                url("https://cdn.example/main/master.m3u8"),
                Some("https://player.example/".into()),
                Some("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv/index.m3u8\n".into()),
            ),
            (
                url("https://cdn.example/main/v/index.m3u8"),
                None,
                Some(media(&[10.0; 60], true)),
            ),
        ]);
        assert!(!drm_only);
        assert_eq!(
            ranked,
            [
                Candidate {
                    url: url("https://cdn.example/main/master.m3u8"),
                    referer: Some("https://player.example/".into())
                },
                Candidate {
                    url: url("https://ads.example/ad.m3u8"),
                    referer: Some("https://ads.example/".into())
                },
            ]
        );
    }

    #[test]
    fn longer_media_playlists_rank_first_and_live_ones_last() {
        let (ranked, _) = rank(&[
            (
                url("https://c.example/live.m3u8"),
                None,
                Some(media(&[2.0], false)),
            ),
            (
                url("https://c.example/short.m3u8"),
                None,
                Some(media(&[15.0], true)),
            ),
            (
                url("https://c.example/long.m3u8"),
                None,
                Some(media(&[600.0], true)),
            ),
            (url("https://c.example/unknown.m3u8"), None, None),
        ]);
        let order: Vec<&str> = ranked
            .iter()
            .map(|candidate| candidate.url.path())
            .collect();
        assert_eq!(order, ["/long.m3u8", "/short.m3u8", "/unknown.m3u8"]);
    }

    #[test]
    fn drm_playlists_are_dropped_and_reported() {
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\"\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n";
        let (ranked, drm_only) = rank(&[(url("https://c.example/p.m3u8"), None, Some(drm.into()))]);
        assert!(ranked.is_empty());
        assert!(drm_only);
    }

    #[test]
    fn base64_bodies_are_decoded() {
        for (encoded, plain) in [
            ("I0VYVE0zVQo=", "#EXTM3U\n"),
            ("YQ==", "a"),
            ("YWI=", "ab"),
            ("YWJj", "abc"),
            ("", ""),
        ] {
            assert_eq!(
                decode_base64(encoded).unwrap(),
                plain.as_bytes(),
                "{encoded}"
            );
        }
        for invalid in ["Y", "YW!j", "Y\nWJj"] {
            assert!(decode_base64(invalid).is_none(), "{invalid}");
        }
        let message = json!({"id": 1, "result": {"body": "I0VYVE0zVQo=", "base64Encoded": true}});
        assert_eq!(response_body(&message).as_deref(), Some("#EXTM3U\n"));
    }

    #[test]
    fn clear_variants_of_a_drm_master_are_not_candidates() {
        let (ranked, drm_only) = rank(&[
            (
                url("https://c.example/master.m3u8"),
                None,
                Some("#EXTM3U\n#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n".into()),
            ),
            (url("https://c.example/v.m3u8"), None, Some(media(&[600.0], true))),
        ]);
        assert!(ranked.is_empty(), "{ranked:?}");
        assert!(drm_only);
    }

    #[test]
    fn masters_with_a_drm_variant_are_not_candidates() {
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\"\n#EXTINF:1,\na.ts\n#EXT-X-ENDLIST\n";
        let (ranked, drm_only) = rank(&[
            (
                url("https://c.example/master.m3u8"),
                None,
                Some("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n".into()),
            ),
            (url("https://c.example/v.m3u8"), None, Some(drm.into())),
        ]);
        assert!(ranked.is_empty(), "{ranked:?}");
        assert!(drm_only);
    }

    #[test]
    fn referers_are_reduced_to_https_origins() {
        assert_eq!(
            origin_referer("https://player.example/embed/1?token=x").as_deref(),
            Some("https://player.example/")
        );
        assert_eq!(origin_referer("http://player.example/embed"), None);
        assert_eq!(origin_referer("not a url"), None);
    }

    /// A `Cdp` reading what the test writes, and the test's ends of both
    /// pipes (what HighVi sends is read and discarded).
    fn fake_chrome() -> (Cdp, pipe::Sender) {
        let (to_chrome_read, to_chrome_write) = pipe_pair().unwrap();
        let (from_chrome_read, from_chrome_write) = pipe_pair().unwrap();
        let mut sink = pipe::Receiver::from_owned_fd(to_chrome_read).unwrap();
        tokio::spawn(async move {
            let mut buffer = vec![0_u8; 64 * 1024];
            while sink.read(&mut buffer).await.is_ok_and(|read| read > 0) {}
        });
        let cdp = Cdp::new(
            pipe::Sender::from_owned_fd(to_chrome_write).unwrap(),
            pipe::Receiver::from_owned_fd(from_chrome_read).unwrap(),
        );
        (cdp, pipe::Sender::from_owned_fd(from_chrome_write).unwrap())
    }

    fn framed(message: &str) -> Vec<u8> {
        let mut bytes = message.as_bytes().to_vec();
        bytes.push(0);
        bytes
    }

    #[tokio::test]
    async fn oversized_events_are_skipped_and_large_replies_kept() {
        let (mut cdp, mut chrome) = fake_chrome();
        let huge_event = format!(
            r#"{{"method":"Network.requestWillBeSent","params":{{"x":"{}"}}}}"#,
            "a".repeat(3 * 1024 * 1024)
        );
        let body = "b".repeat(9 * 1024 * 1024);
        let reply = format!(r#"{{"id":1,"result":{{"body":"{body}"}}}}"#);
        let writer = tokio::spawn(async move {
            chrome.write_all(&framed(&huge_event)).await.unwrap();
            chrome
                .write_all(&framed(r#"{"method":"Page.loadEventFired","params":{}}"#))
                .await
                .unwrap();
            chrome.write_all(&framed(&reply)).await.unwrap();
            chrome
        });
        let result = cdp
            .call(None, "Network.getResponseBody", json!({}))
            .await
            .unwrap();
        assert_eq!(result["body"].as_str().unwrap().len(), 9 * 1024 * 1024);
        assert_eq!((cdp.oversized(), cdp.dropped()), (1, 0));
        // The small event survived in the backlog.
        assert_eq!(cdp.next().await.unwrap()["method"], "Page.loadEventFired");
        drop(writer.await.unwrap());
    }

    #[tokio::test]
    async fn an_event_flood_stays_within_the_byte_bounds() {
        let (mut cdp, mut chrome) = fake_chrome();
        let event = framed(&format!(
            r#"{{"method":"Network.dataReceived","params":{{"x":"{}"}}}}"#,
            "c".repeat(900 * 1024)
        ));
        let writer = tokio::spawn(async move {
            for _ in 0..200 {
                chrome.write_all(&event).await.unwrap();
            }
            chrome
                .write_all(&framed(r#"{"id":1,"result":{}}"#))
                .await
                .unwrap();
            chrome
        });
        // 180 MiB of events arrive before the reply HighVi waits for.
        assert!(cdp.call(None, "Page.enable", json!({})).await.is_some());
        assert!(
            cdp.backlog_weight <= MAX_BACKLOG_WEIGHT,
            "{}",
            cdp.backlog_weight
        );
        // 900 KiB events weigh 7.2 MiB each: two fit the backlog.
        assert!(cdp.backlog.len() <= 2, "{}", cdp.backlog.len());
        assert!(cdp.dropped() >= 198, "{}", cdp.dropped());
        assert_eq!(cdp.oversized(), 0);
        drop(writer.await.unwrap());
    }

    #[test]
    fn redirects_are_followed_even_when_the_tracking_budget_is_full() {
        let key = |id: usize| ("s".to_string(), id.to_string());
        let mut tracker = RequestTracker::default();
        assert_eq!(
            tracker.start(
                key(0),
                "https://c.example/start-of-the-video",
                None,
                "Fetch"
            ),
            Tracked::Yes
        );
        // Fill the byte budget with long URLs.
        let long = format!(
            "https://ads.example/?{}",
            "x".repeat(MAX_TRACKED_URL_BYTES - 64)
        );
        let mut id = 1;
        while tracker.start(key(id), &long, None, "Fetch") == Tracked::Yes {
            id += 1;
        }
        while tracker.start(key(id), "https://ads.example/1", None, "Fetch") == Tracked::Yes {
            id += 1;
        }
        assert_eq!(
            tracker.start(key(id + 1), "https://c.example/new", None, "Fetch"),
            Tracked::OverBudget
        );
        // The early request is redirected: its new URL replaces the old one.
        assert_eq!(
            tracker.start(key(0), "https://c.example/master.m3u8", None, "Fetch"),
            Tracked::Yes
        );
        assert_eq!(
            tracker.get_mut(&key(0)).unwrap().url,
            "https://c.example/master.m3u8"
        );
        // A redirect to an ignored URL drops the stale entry.
        assert_eq!(
            tracker.start(
                key(0),
                &"y".repeat(MAX_TRACKED_URL_BYTES + 1),
                None,
                "Fetch"
            ),
            Tracked::Ignored
        );
        assert!(tracker.get_mut(&key(0)).is_none());
        // Byte accounting matches what is held.
        let held: usize = tracker
            .requests
            .values()
            .map(|request| request.url.len())
            .sum();
        assert_eq!(tracker.url_bytes, held);
        for index in 1..id {
            tracker.finish(&key(index));
        }
        assert_eq!(tracker.url_bytes, 0);
    }

    #[test]
    fn chrome_runs_isolated_and_only_through_the_proxy() {
        let run = BrowserRun {
            chrome: Path::new("/usr/bin/chrome"),
            page: &url("https://site.example/watch"),
            proxy_url: "http://127.0.0.1:4000",
            work_dir: Path::new("/job"),
            mode: Mode::Headless,
            interactive_budget: INTERACTIVE_BUDGET,
            insecure_tls: false,
        };
        let wayland = Display {
            wayland: true,
            env: vec![("WAYLAND_DISPLAY", "/run/user/1000/wayland-0".into())],
        };
        let x11 = Display {
            wayland: false,
            env: vec![("DISPLAY", ":0".into())],
        };
        // Headless and visible windows share every isolation flag.
        for (display, shown) in [
            (None, "--headless=new"),
            (Some(&wayland), "--ozone-platform=wayland"),
            (Some(&x11), "--ozone-platform=x11"),
        ] {
            let args: Vec<String> = chrome_args(&run, Path::new("/job/chrome-profile"), display)
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert!(args.contains(&shown.to_string()), "{shown}");
            assert_eq!(
                args.iter().any(|arg| arg == "--headless=new"),
                display.is_none()
            );
            for required in [
                "--remote-debugging-pipe",
                "--proxy-server=http://127.0.0.1:4000",
                "--proxy-bypass-list=<-loopback>",
                "--user-data-dir=/job/chrome-profile",
                "--disable-quic",
                "--disable-extensions",
                "--password-store=basic",
                "--deny-permission-prompts",
                "--webrtc-ip-handling-policy=disable_non_proxied_udp",
                "--host-resolver-rules=MAP * ~NOTFOUND, EXCLUDE 127.0.0.1",
            ] {
                assert!(args.contains(&required.to_string()), "{required}");
            }
            for forbidden in [
                "--remote-debugging-port",
                "--no-sandbox",
                "--ignore-certificate-errors",
                "--disable-web-security",
                "--profile-directory",
                "--load-extension",
                "--user-agent",
                "--disable-blink-features",
            ] {
                assert!(
                    !args.iter().any(|arg| arg.starts_with(forbidden)),
                    "{forbidden}"
                );
            }
            // The page is never on Chrome's command line; it is navigated to
            // over the pipe after the network is being watched.
            assert_eq!(args.last().unwrap(), "about:blank");
            assert!(!args.iter().any(|arg| arg.contains("site.example")));
        }
    }

    #[tokio::test]
    async fn links_for_desktop_apps_go_nowhere() {
        let profile = crate::filesystem::temp_directory("guards");
        write_launch_guards(&profile).await.unwrap();
        let path = guarded_path(&profile, Some("/usr/bin:/bin".into()));
        assert_eq!(
            path,
            OsString::from(format!("{}/bin:/usr/bin:/bin", profile.display()))
        );
        assert_eq!(
            guarded_path(&profile, None),
            profile.join("bin").into_os_string()
        );
        for name in LAUNCH_GUARDS {
            let status = std::process::Command::new(profile.join("bin").join(name))
                .arg("mailto:someone@example.com")
                .status()
                .unwrap();
            assert!(!status.success(), "{name}");
        }
        std::fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn rejected_playlists_never_hide_later_ones() {
        let playlist = |index: usize| {
            (
                url(&format!("https://c.example/{index}.m3u8")),
                None,
                Some(format!(
                    "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:{}.0,\ns.ts\n#EXT-X-ENDLIST\n",
                    100 - index
                )),
            )
        };
        let playlists: Vec<_> = (0..5).map(playlist).collect();
        let first = rank(&playlists).0;
        assert_eq!(first.len(), MAX_CANDIDATES);
        let rejected: HashSet<String> = first
            .iter()
            .map(|candidate| candidate.url.to_string())
            .collect();
        let next = rank_skipping(&playlists, &rejected).0;
        assert_eq!(next.len(), 2, "{next:?}");
        assert!(
            next.iter()
                .all(|candidate| !rejected.contains(candidate.url.as_str()))
        );
    }

    #[test]
    fn stored_playlists_stay_bounded_and_keep_the_newest() {
        let mut observation = Observation::default();
        let big = "#EXTM3U\n".to_string() + &"x".repeat(4 * 1024 * 1024);
        for index in 0..200 {
            observation.store(
                url(&format!("https://c.example/{index}.m3u8")),
                None,
                big.clone(),
            );
            let bytes: usize = observation
                .playlists
                .iter()
                .map(|(_, _, body)| body.as_ref().map_or(0, String::len))
                .sum();
            assert!(observation.playlists.len() <= MAX_STORED_PLAYLISTS);
            assert!(bytes <= MAX_STORED_PLAYLIST_BYTES);
        }
        assert_eq!(
            observation.playlists.last().unwrap().0.as_str(),
            "https://c.example/199.m3u8"
        );
    }

    #[test]
    fn a_session_that_went_away_frees_what_it_tracked() {
        let mut tracker = RequestTracker::default();
        for (session, id) in [("a", "1"), ("a", "2"), ("b", "1")] {
            tracker.start(
                (session.to_string(), id.to_string()),
                "https://c.example/x.m3u8",
                None,
                "Fetch",
            );
        }
        tracker.forget_session("a");
        assert_eq!(tracker.requests.len(), 1);
        assert_eq!(tracker.url_bytes, "https://c.example/x.m3u8".len());
    }

    #[test]
    fn a_window_gets_only_what_its_display_needs() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        // Wayland: an absolute socket, nothing else (no runtime dir, no bus).
        let wayland = desktop_display(env(&[
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DISPLAY", ":0"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        ]))
        .unwrap();
        assert!(wayland.wayland);
        assert_eq!(
            wayland.env,
            [(
                "WAYLAND_DISPLAY",
                OsString::from("/run/user/1000/wayland-0")
            )]
        );
        // X11: the display and its cookie (HOME's default, as Chrome's HOME
        // is its profile).
        let x11 = desktop_display(env(&[("DISPLAY", ":1"), ("HOME", "/home/u")])).unwrap();
        assert!(!x11.wayland);
        assert_eq!(
            x11.env,
            [
                ("DISPLAY", OsString::from(":1")),
                ("XAUTHORITY", OsString::from("/home/u/.Xauthority"))
            ]
        );
        assert_eq!(
            desktop_display(env(&[("DISPLAY", ":1"), ("XAUTHORITY", "/run/x/cookie")]))
                .unwrap()
                .env[1],
            ("XAUTHORITY", OsString::from("/run/x/cookie"))
        );
        // A relative Wayland name without a runtime directory falls back to X11.
        assert!(
            !desktop_display(env(&[("WAYLAND_DISPLAY", "wayland-0"), ("DISPLAY", ":0")]))
                .unwrap()
                .wayland
        );
        // No display at all: no window.
        assert_eq!(
            desktop_display(env(&[("WAYLAND_DISPLAY", ""), ("DISPLAY", "")])),
            None
        );
    }
}
