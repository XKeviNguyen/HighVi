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
}

impl Ended {
    pub fn label(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::GaveUp => "gave_up_after_play",
            Self::Deadline => "page_budget",
            Self::BrowserGone => "browser_gone",
        }
    }
}

pub struct BrowserRun<'a> {
    pub chrome: &'a Path,
    pub page: &'a Url,
    pub proxy_url: &'a str,
    /// A private (0700) directory; Chrome's profile goes inside it.
    pub work_dir: &'a Path,
    #[cfg(test)]
    pub insecure_tls: bool,
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

pub async fn discover_in_browser(
    run: BrowserRun<'_>,
    cancel: &mut CancelToken,
) -> Result<BrowserFinding, PublicError> {
    let profile = run.work_dir.join("chrome-profile");
    write_profile_preferences(&profile)
        .await
        .map_err(|_| browser_failed())?;
    let (mut child, mut cdp) = launch(&run, &profile).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            PublicError::new(
                "browser_unavailable",
                "Google Chrome or Chromium is needed to find videos on this page.",
            )
        } else {
            browser_failed()
        }
    })?;

    // Every CDP wait inside is bounded too; this is the backstop for a
    // browser that stops answering altogether.
    let bounded = timeout(
        STARTUP_TIMEOUT + PAGE_BUDGET + Duration::from_secs(10),
        observe(&mut cdp, run.page),
    );
    let result = tokio::select! {
        result = bounded => result.unwrap_or_else(|_| Err(browser_failed())),
        () = cancel.cancelled() => Err(PublicError::new("cancelled", "The download was cancelled.")),
    };
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

fn browser_failed() -> PublicError {
    PublicError::new(
        "browser_failed",
        "The isolated browser HighVi uses to find videos stopped unexpectedly.",
    )
}

pub fn chrome_args(run: &BrowserRun<'_>, profile: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "--headless=new",
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

fn launch(run: &BrowserRun<'_>, profile: &Path) -> io::Result<(Child, Cdp)> {
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
            ["PATH", "LANG", "LC_ALL", "TMPDIR"]
                .into_iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (name, value))),
        )
        .env("HOME", profile)
        .env("XDG_CONFIG_HOME", profile.join("config"))
        .env("XDG_CACHE_HOME", profile.join("cache"))
        .args(chrome_args(run, profile));
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

async fn observe(cdp: &mut Cdp, page: &Url) -> Result<BrowserFinding, PublicError> {
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
        (
            "Target.setAutoAttach",
            json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
        ),
    ] {
        cdp.call(Some(&main), method, params)
            .await
            .ok_or_else(browser_failed)?;
    }
    cdp.send(
        Some(&main),
        "Page.navigate",
        json!({ "url": page.as_str() }),
    )
    .await
    .map_err(|_| browser_failed())?;

    let started = Instant::now();
    let deadline = started + PAGE_BUDGET;
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

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if let Some(found) = last_found
            && now >= found + SETTLE
            && pending_bodies.is_empty()
        {
            report.ended = Ended::Settled;
            break;
        }
        let nothing_yet = observation.playlists.is_empty() && pending_bodies.is_empty();
        if let Some(at) = played_at
            && nothing_yet
            && now >= at + GIVE_UP_AFTER_PLAY
        {
            report.ended = Ended::GaveUp;
            break;
        }
        let play_at = loaded_at.map_or(started + PLAY_WITHOUT_LOAD, |at| at + PLAY_AFTER_LOAD);
        if played_at.is_none() && nothing_yet && now >= play_at {
            played_at = Some(now);
            for session in &sessions {
                let _ = cdp
                    .send(
                        Some(session),
                        "Runtime.evaluate",
                        json!({ "expression": PLAY_SCRIPT, "userGesture": true }),
                    )
                    .await;
            }
        }
        let wake = [
            Some(deadline),
            last_found.map(|found| found + SETTLE),
            if played_at.is_none() {
                Some(play_at)
            } else {
                None
            },
            played_at.map(|at| at + GIVE_UP_AFTER_PLAY),
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
            // Chrome exited. Keep what was seen, if anything.
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
                observation.playlists.push((url, referer, Some(body)));
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
            "Page.loadEventFired" if session == main => {
                loaded_at.get_or_insert_with(Instant::now);
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

    let title = timeout(
        Duration::from_secs(3),
        cdp.call(
            Some(&main),
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
    .filter(|title| !title.is_empty());

    report.events_dropped = cdp.dropped();
    report.events_oversized = cdp.oversized();
    report.loaded = loaded_at.is_some();
    report.played = played_at.is_some();
    report.playlists = observation.playlists.len();
    let (candidates, drm_only) = rank(&observation.playlists);
    Ok(BrowserFinding {
        candidates,
        title,
        drm_only,
        page: report,
    })
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
            insecure_tls: false,
        };
        let args: Vec<String> = chrome_args(&run, Path::new("/job/chrome-profile"))
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        for required in [
            "--remote-debugging-pipe",
            "--proxy-server=http://127.0.0.1:4000",
            "--proxy-bypass-list=<-loopback>",
            "--user-data-dir=/job/chrome-profile",
            "--disable-quic",
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
        ] {
            assert!(
                !args.iter().any(|arg| arg.starts_with(forbidden)),
                "{forbidden}"
            );
        }
        // The page is never on Chrome's command line; it is navigated to over
        // the pipe after the network is being watched.
        assert_eq!(args.last().unwrap(), "about:blank");
        assert!(!args.iter().any(|arg| arg.contains("site.example")));
    }
}
