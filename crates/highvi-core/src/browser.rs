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
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::unix::pipe,
    process::{Child, Command},
    sync::mpsc,
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
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SESSIONS: usize = 48;
const MAX_TRACKED_REQUESTS: usize = 4_000;
const MAX_BODY_FETCHES: usize = 60;
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
    tokio::fs::create_dir(&profile)
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
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
        "--disable-features=Translate,MediaRouter,OptimizationHints,AutofillServerCommunication,DialMediaRouteProvider",
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

/// A minimal DevTools-protocol client: NUL-delimited JSON over two pipes.
struct Cdp {
    writer: pipe::Sender,
    incoming: mpsc::Receiver<Value>,
    backlog: VecDeque<Value>,
    next_id: u64,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Cdp {
    fn new(writer: pipe::Sender, mut receiver: pipe::Receiver) -> Self {
        let (sender, incoming) = mpsc::channel(1024);
        let reader = tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut chunk = vec![0_u8; 64 * 1024];
            loop {
                let Ok(read) = receiver.read(&mut chunk).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
                while let Some(end) = buffer.iter().position(|byte| *byte == 0) {
                    let message: Vec<u8> = buffer.drain(..=end).collect();
                    if let Ok(value) = serde_json::from_slice::<Value>(&message[..end])
                        && sender.send(value).await.is_err()
                    {
                        return;
                    }
                }
                if buffer.len() > MAX_MESSAGE_BYTES {
                    return;
                }
            }
        });
        Self {
            writer,
            incoming,
            backlog: VecDeque::new(),
            next_id: 1,
            reader,
        }
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
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message.get("result").cloned();
            }
            self.backlog.push_back(message);
        }
    }

    async fn next(&mut self) -> Option<Value> {
        match self.backlog.pop_front() {
            Some(message) => Some(message),
            None => self.incoming.recv().await,
        }
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
    let mut sessions: Vec<String> = vec![main.clone()];
    let mut requests: HashMap<(String, String), Request> = HashMap::new();
    let mut pending_bodies: HashMap<u64, (Url, Option<String>)> = HashMap::new();
    let mut body_fetches = 0;
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
            break;
        }
        let nothing_yet = observation.playlists.is_empty() && pending_bodies.is_empty();
        if let Some(at) = played_at
            && nothing_yet
            && now >= at + GIVE_UP_AFTER_PLAY
        {
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
            "Inspector.targetCrashed" | "Inspector.detached" if session == main => break,
            "Network.requestWillBeSent" => {
                if requests.len() >= MAX_TRACKED_REQUESTS {
                    continue;
                }
                let request = &params["request"];
                let Some(url) = request["url"].as_str() else {
                    continue;
                };
                let referer = header(&request["headers"], "referer").and_then(origin_referer);
                if let Some(id) = params["requestId"].as_str() {
                    requests.insert(
                        (session, id.to_string()),
                        Request {
                            url: url.to_string(),
                            referer,
                            resource_type: params["type"].as_str().unwrap_or_default().to_string(),
                            want: Want::Nothing,
                        },
                    );
                }
            }
            "Network.responseReceived" => {
                let Some(id) = params["requestId"].as_str() else {
                    continue;
                };
                let Some(request) = requests.get_mut(&(session.clone(), id.to_string())) else {
                    continue;
                };
                let response = &params["response"];
                let status = response["status"].as_u64().unwrap_or(0);
                let mime = response["mimeType"]
                    .as_str()
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let Ok(url) = Url::parse(&request.url) else {
                    continue;
                };
                if !(200..300).contains(&status) || url.scheme() != "https" {
                    continue;
                }
                let by_name = url.path().to_ascii_lowercase().ends_with(".m3u8");
                let by_type = HLS_MIME_TYPES.contains(&mime.as_str());
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
                let Some(request) = requests.get(&(session.clone(), id.to_string())) else {
                    continue;
                };
                let size = params["encodedDataLength"].as_f64().unwrap_or(0.0);
                let wanted = match request.want {
                    Want::Playlist => size <= MAX_PLAYLIST_BYTES,
                    Want::Sniff => size <= MAX_SNIFF_BYTES,
                    Want::Nothing => false,
                };
                if !wanted || body_fetches >= MAX_BODY_FETCHES {
                    continue;
                }
                let Ok(url) = Url::parse(&request.url) else {
                    continue;
                };
                let referer = request.referer.clone();
                body_fetches += 1;
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

    let (candidates, drm_only) = rank(&observation.playlists);
    Ok(BrowserFinding {
        candidates,
        title,
        drm_only,
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
    let mut scored = Vec::new();
    for (order, (url, referer, info)) in inspected.iter().enumerate() {
        if info.as_ref().is_some_and(|info| info.drm) {
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
    fn referers_are_reduced_to_https_origins() {
        assert_eq!(
            origin_referer("https://player.example/embed/1?token=x").as_deref(),
            Some("https://player.example/")
        );
        assert_eq!(origin_referer("http://player.example/embed"), None);
        assert_eq!(origin_referer("not a url"), None);
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
            "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
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
