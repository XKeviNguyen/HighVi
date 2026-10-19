//! One download job: discover → download → mux → publish.
//!
//! The job owns everything it learns. Media URLs, which may carry signed
//! tokens, never leave this module; callers see a title, progress, and the
//! final path.

use crate::{
    PublicError, SourceKind, ToolSignals, ValidatedSource,
    browser::{
        BrowserFinding, BrowserRun, Candidate, Check, CheckFuture, Ended, Mode, discover_in_browser,
    },
    classify::{FetchTarget, signals_from_output},
    diagnostics::Diagnostics,
    egress::{EgressProxy, EgressStats, RefusalCounts, Resolver},
    filesystem::{
        DiskBudget, JobDir, Reservation, cleanup_stale_jobs, free_bytes, output_dir_error,
        prepare_output_dir, publish_unique,
    },
    formats::{MediaInfo, select_media},
    mux::{Container, MuxInput, SNIFF_BYTES, mux_args, sniff},
    process::{
        CancelToken, DiskFloor, RunError, StallPolicy, Stream, Watch, run_captured, run_streaming,
        tool_command,
    },
    tools::{
        MINIMUM_YT_DLP, YtDlpVersion, extractor_related, resolve_python, today, update_advice,
    },
    ytdlp::{INFO_JSON, YtDlpPolicy, prepare_tool_path, ytdlp_env},
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::io::AsyncReadExt;

const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 256 * 1024;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
const MUX_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const OUTPUT_FILE: &str = "output.mp4";
const MIB: u64 = 1024 * 1024;
/// 16 TiB: no single track is larger.
const MAX_PROGRESS_BYTES: u64 = 16 * 1024 * 1024 * MIB;

/// Limits on one job's transfers and disk use.
#[derive(Clone, Copy, Debug)]
pub struct JobLimits {
    /// A track download that makes less progress than this is stopped.
    pub stall: StallPolicy,
    /// Longest a single track download may run.
    pub track_deadline: Duration,
    /// Free space the disk keeps while HighVi writes; a download stops
    /// rather than fill it.
    pub disk_floor: u64,
    /// How long a visible HighVi browser window waits for a video.
    pub interactive_budget: Duration,
    /// How long a window HighVi opens by itself waits, when automatic
    /// discovery fails in a way such a window may get past.
    pub auto_window_budget: Duration,
    /// How long the row says a window is about to open before it does, so
    /// the window never comes as a surprise.
    pub window_notice: Duration,
}

impl Default for JobLimits {
    fn default() -> Self {
        Self {
            // 64 KiB in 2 minutes (≈ 0.5 KB/s): far below any usable
            // connection, far above a server trickling bytes to hold a job.
            stall: StallPolicy {
                window: Duration::from_secs(120),
                min_bytes: 64 * 1024,
            },
            // The stall policy is what catches stuck transfers; this only
            // bounds a job that keeps crawling (about 20 GB at 0.25 MB/s).
            track_deadline: Duration::from_secs(24 * 60 * 60),
            disk_floor: 512 * MIB,
            interactive_budget: crate::browser::INTERACTIVE_BUDGET,
            auto_window_budget: crate::browser::AUTO_WINDOW_BUDGET,
            window_notice: crate::browser::WINDOW_NOTICE,
        }
    }
}

/// At most this many isolated browsers run at once, whatever the number of
/// jobs: Chrome is the heaviest process HighVi starts.
pub const MAX_BROWSERS: usize = 1;
/// FFmpeg muxes are short and disk-bound; two keep the disk busy without
/// competing for it.
pub const MAX_MUXES: usize = 2;

/// Limits and settings shared by every job of one HighVi process. A job
/// holds at most one of these permits at a time, so waiting for one cannot
/// deadlock.
#[derive(Debug)]
pub struct SharedResources {
    pub disk: Arc<DiskBudget>,
    pub browsers: tokio::sync::Semaphore,
    pub muxers: tokio::sync::Semaphore,
    /// The user's setting: open the HighVi browser window by itself when a
    /// page needs it (on by default). Read when the window is about to
    /// open, so switching it off applies to jobs already running.
    pub auto_window: AtomicBool,
}

impl Default for SharedResources {
    fn default() -> Self {
        Self {
            disk: Arc::default(),
            browsers: tokio::sync::Semaphore::new(MAX_BROWSERS),
            muxers: tokio::sync::Semaphore::new(MAX_MUXES),
            auto_window: AtomicBool::new(true),
        }
    }
}

/// What a job needs from the machine it runs on.
pub struct JobEnvironment {
    pub yt_dlp: PathBuf,
    pub ffmpeg: PathBuf,
    /// Chrome/Chromium for pages whose player is built by JavaScript.
    /// Without it, those pages fail as yt-dlp reports them.
    pub chrome: Option<PathBuf>,
    /// Linked into yt-dlp's private `PATH` for builds that need it.
    pub python: Option<PathBuf>,
    /// The fixed output directory, e.g. `~/Downloads/HighVi`.
    pub output_dir: PathBuf,
    pub limits: JobLimits,
    /// Shared by every job of the process.
    pub shared: Arc<SharedResources>,
    #[cfg(test)]
    pub(crate) fixture: Option<Resolver>,
    #[cfg(test)]
    pub(crate) free_space: Option<Arc<dyn Fn() -> Option<u64> + Send + Sync>>,
}

impl JobEnvironment {
    pub fn new(
        yt_dlp: PathBuf,
        ffmpeg: PathBuf,
        chrome: Option<PathBuf>,
        output_dir: PathBuf,
    ) -> Self {
        Self {
            yt_dlp,
            ffmpeg,
            chrome,
            python: find_executable("python3"),
            output_dir,
            limits: JobLimits::default(),
            shared: Arc::default(),
            #[cfg(test)]
            fixture: None,
            #[cfg(test)]
            free_space: None,
        }
    }

    /// Free bytes in the output directory's filesystem, if known.
    fn free_space(&self) -> Option<u64> {
        #[cfg(test)]
        if let Some(free_space) = &self.free_space {
            return free_space();
        }
        free_bytes(&self.output_dir)
    }

    fn resolver(&self) -> Resolver {
        #[cfg(test)]
        if let Some(fixture) = &self.fixture {
            return fixture.clone();
        }
        Resolver::System
    }

    /// Whether the user lets the HighVi browser window open by itself.
    fn auto_window(&self) -> bool {
        self.shared.auto_window.load(Ordering::Relaxed)
    }

    fn policy<'a>(&self, proxy_url: &'a str, referer: Option<&'a str>) -> YtDlpPolicy<'a> {
        YtDlpPolicy {
            proxy_url,
            referer,
            #[cfg(test)]
            insecure_tls: self.fixture.is_some(),
        }
    }
}

/// How a job looks for media on a page.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Discovery {
    /// yt-dlp, then the page's own player in a headless browser; then, if
    /// the site refused those or the player needs the user, a visible
    /// window that opens by itself.
    #[default]
    Automatic,
    /// Straight to a visible HighVi browser window that the user browses
    /// in; HighVi takes the first video stream that passes its checks.
    Interactive,
}

/// Failures a visible browser window may get past: the site refused or
/// hid the page from the headless browser, or the player needs the user.
pub fn browser_can_help(code: &str) -> bool {
    matches!(
        code,
        "host_firewall_block"
            | "access_denied"
            | "no_media_found"
            | "media_discovery_timeout"
            | "discovery_overloaded"
            | "playlist_access_denied"
            | "page_unavailable"
            | "unsupported_page"
            | "hls_not_found"
            | "no_formats"
            | "yt_dlp_failed"
            | "browser_closed"
            | "interactive_timeout"
    )
}

/// Failures after which HighVi opens its browser window by itself, sparing
/// the user a click: [`browser_can_help`], except a page that is simply not
/// there (the user can still open it), and the window's own endings.
fn window_opens_unasked(code: &str) -> bool {
    browser_can_help(code)
        && !matches!(
            code,
            "page_unavailable" | "browser_closed" | "interactive_timeout"
        )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    Discovering,
    /// Waiting for the isolated browser, which one job uses at a time.
    WaitingForBrowser,
    /// A visible HighVi browser window is about to open by itself.
    OpeningWindow,
    /// A visible HighVi browser window is open; waiting for the user to
    /// start the video there.
    WaitingForUser,
    /// Watching the page in the isolated browser.
    Browsing,
    DownloadingVideo,
    DownloadingAudio,
    /// Waiting for a free mux slot.
    WaitingToMerge,
    Merging,
    Saving,
}

/// A progress report. Values are short display strings from yt-dlp's own
/// progress fields; nothing here can contain a URL.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobUpdate {
    pub stage: Stage,
    pub title: Option<String>,
    pub percent: Option<String>,
    pub speed: Option<String>,
    pub eta: Option<String>,
}

impl JobUpdate {
    fn stage(stage: Stage) -> Self {
        Self {
            stage,
            title: None,
            percent: None,
            speed: None,
            eta: None,
        }
    }
}

#[derive(Debug)]
pub struct JobOutcome {
    pub saved_to: PathBuf,
    pub title: String,
}

/// Runs one job. On failure the diagnostics describe what happened, in a
/// form that is safe to show and copy (see [`Diagnostics::render`]).
pub async fn run_job(
    env: &JobEnvironment,
    source: &ValidatedSource,
    discovery: Discovery,
    cancel: CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
) -> (Result<JobOutcome, PublicError>, Diagnostics) {
    let recorder = Mutex::new(Diagnostics::new(source));
    let mut late_cancel = cancel.clone();
    let tracked = |update: JobUpdate| {
        record(&recorder, |diagnostics| diagnostics.enter(update.stage));
        report(update);
    };
    let result = Box::pin(run(env, source, discovery, cancel, &tracked, &recorder)).await;
    let mut diagnostics = recorder
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let result = match result {
        Ok(outcome) => Ok(outcome),
        Err(mut error) => {
            if error.code != "cancelled" {
                // A cancel while versions are probed still means "cancelled".
                tokio::select! {
                    () = diagnostics.collect_tools(env) => {}
                    () = late_cancel.cancelled() => error = cancelled_error(),
                }
                // Output can imitate yt-dlp's usage error; only the
                // installed version says yt-dlp is really too old, which
                // pauses the whole queue.
                if error.code == "ytdlp_too_old" {
                    let minimum = YtDlpVersion {
                        date: MINIMUM_YT_DLP,
                    };
                    match diagnostics.yt_dlp {
                        Some(installed) if installed < minimum => {
                            error.hint = Some(crate::Hint::UpdateYtDlp {
                                installed: installed.to_string(),
                                minimum: Some(minimum.to_string()),
                            });
                        }
                        _ => error = crate::classify::yt_dlp_failed(),
                    }
                }
                // Refusals seen by the browser are the site's answer to
                // Chrome; a newer yt-dlp would not change them.
                if extractor_related(&error.code)
                    && !diagnostics.failed_in(Stage::Browsing)
                    && let Some(installed) = diagnostics.yt_dlp
                {
                    error.hint = update_advice(installed, diagnostics.site(), today());
                }
            }
            diagnostics.finish(&error);
            Err(error)
        }
    };
    (result, diagnostics)
}

/// Updates the job's diagnostics. The lock is never held across an await.
fn record(recorder: &Mutex<Diagnostics>, update: impl FnOnce(&mut Diagnostics)) {
    update(
        &mut recorder
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
}

async fn run(
    env: &JobEnvironment,
    source: &ValidatedSource,
    discovery: Discovery,
    mut cancel: CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
    recorder: &Mutex<Diagnostics>,
) -> Result<JobOutcome, PublicError> {
    report(JobUpdate::stage(Stage::Discovering));
    prepare_output_dir(&env.output_dir).await?;
    cleanup_stale_jobs(&env.output_dir).await;
    let mut job = JobDir::create(&env.output_dir).await?;
    let python = match &env.python {
        Some(python) => tokio::select! {
            resolved = resolve_python(python) => resolved,
            () = cancel.cancelled() => return Err(cancelled_error()),
        },
        None => None,
    };
    prepare_tool_path(job.path(), python.as_deref())
        .await
        .map_err(|_| output_dir_error())?;
    let proxy = EgressProxy::start_with(env.resolver()).await.map_err(|_| {
        PublicError::new("proxy_failed", "HighVi could not start its network guard.")
    })?;
    let result = Box::pin(fetch(
        env,
        &job,
        &proxy,
        source,
        discovery,
        &mut cancel,
        report,
        recorder,
    ))
    .await;
    let refused = proxy.stats().counts();
    let tunnels = proxy.stats().tunnels();
    record(recorder, |diagnostics| diagnostics.egress(tunnels, refused));
    drop(proxy);
    let (media, _reservation) = result?;
    // The info JSON holds the media URLs, which may be signed; nothing after
    // this point needs it, and a kept recovery directory must not hold it.
    let _ = tokio::fs::remove_file(job.path().join(INFO_JSON)).await;

    report(JobUpdate::stage(Stage::WaitingToMerge));
    let _muxer = tokio::select! {
        permit = env.shared.muxers.acquire() => permit.map_err(|_| mux_error())?,
        () = cancel.cancelled() => return Err(cancelled_error()),
    };
    report(JobUpdate::stage(Stage::Merging));
    Box::pin(mux(env, &job, media.selection.audio.is_some(), &mut cancel)).await?;

    report(JobUpdate::stage(Stage::Saving));
    // Publishing is one atomic rename; a cancel that arrives before it wins.
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    let stem = media
        .suggested_filename
        .strip_suffix(".mp4")
        .unwrap_or(&media.suggested_filename)
        .to_string();
    let completed = job.path().join(OUTPUT_FILE);
    let directory = env.output_dir.clone();
    let token = cancel.clone();
    let published = tokio::task::spawn_blocking(move || {
        publish_unique(&completed, &directory, &stem, &|| token.is_cancelled())
    })
    .await
    .map_err(|_| output_dir_error())?;
    if let Err(error) = &published
        && error.kind() == std::io::ErrorKind::Interrupted
    {
        return Err(cancelled_error());
    }
    match published {
        Ok(saved_to) => Ok(JobOutcome {
            saved_to,
            title: media.title,
        }),
        Err(_) => {
            job.keep();
            Err(PublicError::new(
                "output_publish_failed",
                format!(
                    "The completed video could not be published. It remains at {} for recovery.",
                    job.path().join(OUTPUT_FILE).to_string_lossy()
                ),
            ))
        }
    }
}

/// Everything that needs the network: discovery and the track downloads.
#[allow(clippy::too_many_arguments)]
async fn fetch(
    env: &JobEnvironment,
    job: &JobDir,
    proxy: &EgressProxy,
    source: &ValidatedSource,
    discovery: Discovery,
    cancel: &mut CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
    recorder: &Mutex<Diagnostics>,
) -> Result<(MediaInfo, Reservation), PublicError> {
    let proxy_url = proxy.url();
    let discovered = Box::pin(discover(
        env, job, proxy, source, discovery, cancel, report, recorder,
    ))
    .await?;
    let mut media = discovered.media;
    // A playlist link gives yt-dlp only a file name, often "master" or
    // "index"; the folder it sits in usually names the video.
    if source.kind() != SourceKind::Page
        && is_generic_playlist_name(&media.title)
        && let Some(name) = playlist_folder_name(source.discovery_target())
    {
        media.suggested_filename = crate::suggested_filename(Some(&name), None);
        media.title = name;
    }
    record(recorder, |diagnostics| {
        diagnostics.media(media.extractor.as_deref(), media.summary.clone());
    });
    report(JobUpdate {
        title: Some(media.title.clone()),
        ..JobUpdate::stage(Stage::Discovering)
    });
    // The tracks and the muxed copy exist together for a moment: twice the
    // estimate. An unknown size reserves nothing; the floor still applies
    // while downloading.
    let needed = media
        .estimated_bytes
        .map_or(0, |bytes| bytes.saturating_mul(2));
    let free = env.free_space().unwrap_or(u64::MAX);
    let reservation = env
        .shared
        .disk
        .reserve(free, needed, env.limits.disk_floor)
        .ok_or_else(|| no_space_error(Some(needed), free))?;
    tokio::fs::write(job.path().join(INFO_JSON), &media.info_json)
        .await
        .map_err(|_| output_dir_error())?;

    let policy = env.policy(&proxy_url, discovered.referer.as_deref());
    let tracks = [
        Some((
            media.selection.video.as_str(),
            "video",
            Stage::DownloadingVideo,
        )),
        media
            .selection
            .audio
            .as_deref()
            .map(|id| (id, "audio", Stage::DownloadingAudio)),
    ];
    for (format_id, stem, stage) in tracks.into_iter().flatten() {
        Box::pin(download_track(
            env,
            job,
            &policy,
            format_id,
            stem,
            stage,
            source.kind(),
            proxy.stats(),
            cancel,
            report,
            recorder,
        ))
        .await?;
    }
    Ok((media, reservation))
}

/// A visible window: how long it waits for the user, and how long the row
/// says it is about to open first (for a window the user did not ask for).
#[derive(Clone, Copy)]
struct Window {
    budget: Duration,
    notice: Option<Duration>,
}

/// The user switched off unasked windows; mapped back to the original
/// refusal, so it is never shown.
fn window_not_opened() -> PublicError {
    PublicError::new(
        "window_not_opened",
        "Opening the HighVi browser by itself is switched off.",
    )
}

/// What discovery settled on, and the Referer its media needs.
struct Discovered {
    media: MediaInfo,
    referer: Option<String>,
}

/// yt-dlp first; when it finds nothing on a page, the page's own player in
/// an isolated browser, with each playlist it loads checked by yt-dlp. When
/// the site refuses that browser or its player needs the user, a visible
/// browser window opens by itself. An interactive attempt goes straight to
/// a visible browser window.
#[allow(clippy::too_many_arguments)]
async fn discover(
    env: &JobEnvironment,
    job: &JobDir,
    proxy: &EgressProxy,
    source: &ValidatedSource,
    discovery: Discovery,
    cancel: &mut CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
    recorder: &Mutex<Diagnostics>,
) -> Result<Discovered, PublicError> {
    let proxy_url = proxy.url();
    if discovery == Discovery::Interactive {
        // The queue only offers this for pages.
        let ValidatedSource::Page { page } = source else {
            return Err(PublicError::new(
                "internal_error",
                "Only a video page can be opened in the HighVi browser.",
            ));
        };
        let Some(chrome) = &env.chrome else {
            return Err(crate::browser::browser_unavailable());
        };
        return Box::pin(discover_in_window(
            env,
            job,
            &proxy_url,
            proxy,
            chrome,
            page,
            Window {
                budget: env.limits.interactive_budget,
                notice: None,
            },
            cancel,
            report,
            recorder,
        ))
        .await;
    }
    let error = match Box::pin(discover_automatically(
        env, job, proxy, source, cancel, report, recorder,
    ))
    .await
    {
        Err(error) => error,
        found => return found,
    };
    match (&env.chrome, source) {
        (Some(chrome), ValidatedSource::Page { page })
            if source.kind() == SourceKind::Page
                && window_opens_unasked(&error.code)
                && env.auto_window()
                && !cancel.is_cancelled()
                && crate::browser::can_open_window() =>
        {
            let window = Window {
                budget: env.limits.auto_window_budget,
                notice: Some(env.limits.window_notice),
            };
            match Box::pin(discover_in_window(
                env, job, &proxy_url, proxy, chrome, page, window, cancel, report, recorder,
            ))
            .await
            {
                // A window that could not run (crash, unusable display) or
                // that the user switched off says less than the refusal,
                // which still offers the button.
                Err(window) if window.code != "cancelled" && !browser_can_help(&window.code) => {
                    Err(error)
                }
                ended => ended,
            }
        }
        _ => Err(error),
    }
}

/// yt-dlp, then the page's own player in a headless browser.
async fn discover_automatically(
    env: &JobEnvironment,
    job: &JobDir,
    proxy: &EgressProxy,
    source: &ValidatedSource,
    cancel: &mut CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
    recorder: &Mutex<Diagnostics>,
) -> Result<Discovered, PublicError> {
    let proxy_url = proxy.url();
    let referer = source.discovery_referer();
    let first = probe(
        env,
        job,
        &env.policy(&proxy_url, referer.as_deref()),
        source.discovery_target(),
        (source.kind(), FetchTarget::Source),
        proxy.stats(),
        cancel,
        recorder,
    )
    .await;
    let error = match first {
        Ok(media) => {
            return Ok(Discovered {
                media,
                referer: source.download_referer(),
            });
        }
        Err(error) => error,
    };
    let (Some(chrome), ValidatedSource::Page { page }) = (&env.chrome, source) else {
        return Err(error);
    };
    if source.kind() != SourceKind::Page || !worth_a_browser(&error) {
        return Err(error);
    }

    // Headless: the session ends once playlists settle; they are checked
    // after Chrome is gone.
    let mut later = |_: Vec<Candidate>, _: Option<String>| -> CheckFuture<'static, ()> {
        Box::pin(async { None })
    };
    let finding = match browse(
        env, job, &proxy_url, chrome, page, None, cancel, report, &mut later,
    )
    .await
    {
        Ok((finding, _)) => finding,
        // No browser installed: the original explanation is the useful one.
        Err(browser) if browser.code == "browser_unavailable" => return Err(error),
        Err(browser) => return Err(browser),
    };
    record(recorder, |diagnostics| {
        diagnostics.page(finding.page.clone())
    });
    if finding.candidates.is_empty() {
        return Err(empty_browser_result(&finding, Mode::Headless));
    }
    check_candidates(
        env,
        job,
        &proxy_url,
        proxy,
        finding.candidates,
        finding.title,
        error,
        cancel,
        report,
        recorder,
    )
    .await
}

/// One isolated browser session, while holding the browser (one job at a
/// time). Chrome has exited and its profile is gone when this returns.
#[allow(clippy::too_many_arguments)]
async fn browse<T>(
    env: &JobEnvironment,
    job: &JobDir,
    proxy_url: &str,
    chrome: &std::path::Path,
    page: &url::Url,
    window: Option<Window>,
    cancel: &mut CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
    check: &mut Check<'_, T>,
) -> Result<(BrowserFinding, Option<T>), PublicError> {
    report(JobUpdate::stage(Stage::WaitingForBrowser));
    let browser_permit = tokio::select! {
        permit = env.shared.browsers.acquire() => permit.map_err(|_| crate::browser::browser_failed())?,
        () = cancel.cancelled() => return Err(cancelled_error()),
    };
    let (mode, interactive_budget) = match window {
        None => (Mode::Headless, env.limits.interactive_budget),
        Some(window) => {
            // Said once the browser is free, right before the window
            // opens, however long the wait for it was.
            if let Some(notice) = window.notice {
                // Switched off while waiting for the browser: never
                // announce a window that will not open.
                if !env.auto_window() {
                    return Err(window_not_opened());
                }
                report(JobUpdate::stage(Stage::OpeningWindow));
                tokio::select! {
                    () = tokio::time::sleep(notice) => {}
                    () = cancel.cancelled() => return Err(cancelled_error()),
                }
                if !env.auto_window() {
                    return Err(window_not_opened());
                }
            }
            (Mode::Interactive, window.budget)
        }
    };
    report(JobUpdate::stage(match mode {
        Mode::Headless => Stage::Browsing,
        Mode::Interactive => Stage::WaitingForUser,
    }));
    let finding = discover_in_browser(
        BrowserRun {
            chrome,
            page,
            proxy_url,
            work_dir: job.path(),
            mode,
            interactive_budget,
            #[cfg(test)]
            insecure_tls: env.fixture.is_some(),
        },
        cancel,
        check,
    )
    .await;
    // The profile holds whatever the page stored; it is not needed again.
    let _ = tokio::fs::remove_dir_all(job.path().join("chrome-profile")).await;
    // Chrome has exited (it is killed before discovery returns).
    drop(browser_permit);
    finding
}

/// A visible window the user browses in. Each new set of playlists it
/// loads is checked while the window stays open (an advert or preview may
/// come before the real video); the first that passes is downloaded.
#[allow(clippy::too_many_arguments)]
async fn discover_in_window(
    env: &JobEnvironment,
    job: &JobDir,
    proxy_url: &str,
    proxy: &EgressProxy,
    chrome: &std::path::Path,
    page: &url::Url,
    window: Window,
    cancel: &mut CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
    recorder: &Mutex<Diagnostics>,
) -> Result<Discovered, PublicError> {
    let checking = cancel.clone();
    let rejected: Mutex<Option<PublicError>> = Mutex::new(None);
    let mut check = |candidates: Vec<Candidate>,
                     title: Option<String>|
     -> CheckFuture<'_, Result<Discovered, PublicError>> {
        let mut token = checking.clone();
        let rejected = &rejected;
        Box::pin(async move {
            let fallback = PublicError::new(
                "no_media_found",
                "The HighVi browser saw a video stream, but none of it could be downloaded.",
            );
            match check_candidates(
                env, job, proxy_url, proxy, candidates, title, fallback, &mut token, report,
                recorder,
            )
            .await
            {
                Ok(found) => Some(Ok(found)),
                Err(error) if error.code == "cancelled" => Some(Err(error)),
                Err(error) => {
                    // Not this one: keep watching the window.
                    report(JobUpdate::stage(Stage::WaitingForUser));
                    *rejected
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error);
                    None
                }
            }
        })
    };
    let (finding, accepted) = browse(
        env,
        job,
        proxy_url,
        chrome,
        page,
        Some(window),
        cancel,
        report,
        &mut check,
    )
    .await?;
    record(recorder, |diagnostics| {
        diagnostics.page(finding.page.clone())
    });
    match accepted {
        Some(result) => result,
        // Closed, or out of time: say so, even if some streams were seen
        // and rejected on the way.
        None if matches!(finding.page.ended, Ended::Closed | Ended::Deadline) => {
            Err(empty_browser_result(&finding, Mode::Interactive))
        }
        None => Err(rejected
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .unwrap_or_else(|| empty_browser_result(&finding, Mode::Interactive))),
    }
}

/// Each playlist the browser saw, best first, checked by yt-dlp through
/// the network guard; the first that passes is the job's media.
#[allow(clippy::too_many_arguments)]
async fn check_candidates(
    env: &JobEnvironment,
    job: &JobDir,
    proxy_url: &str,
    proxy: &EgressProxy,
    candidates: Vec<Candidate>,
    title: Option<String>,
    error: PublicError,
    cancel: &mut CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
    recorder: &Mutex<Diagnostics>,
) -> Result<Discovered, PublicError> {
    report(JobUpdate::stage(Stage::Discovering));
    let mut last_error = error;
    for candidate in candidates {
        record(recorder, Diagnostics::candidate_checked);
        let checked = probe(
            env,
            job,
            &env.policy(proxy_url, candidate.referer.as_deref()),
            &candidate.url,
            (SourceKind::Page, FetchTarget::BrowserPlaylist),
            proxy.stats(),
            cancel,
            recorder,
        )
        .await;
        match checked {
            Ok(mut media) => {
                // yt-dlp only sees the playlist, whose name is meaningless.
                if let Some(title) = &title {
                    media.title.clone_from(title);
                    media.suggested_filename = crate::suggested_filename(Some(title), None);
                }
                return Ok(Discovered {
                    media,
                    referer: candidate.referer,
                });
            }
            Err(error) if error.code == "cancelled" => return Err(error),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

/// Why the browser found nothing. A page the site refused to show (bot
/// check, 403) is reported as a refusal, which HighVi does not work around,
/// rather than as a page without video.
fn empty_browser_result(finding: &BrowserFinding, mode: Mode) -> PublicError {
    let page = &finding.page;
    match (mode, page.ended) {
        (_, Ended::Closed) => return crate::browser::browser_closed(),
        (Mode::Interactive, Ended::Deadline) => {
            return PublicError::new(
                "interactive_timeout",
                "No video started in the HighVi browser window in time, so HighVi closed it.",
            );
        }
        _ => {}
    }
    if finding.drm_only {
        return PublicError::new(
            "drm_not_supported",
            "This media reports DRM protection. HighVi will not bypass DRM or access controls.",
        );
    }
    if page.bot_check {
        return PublicError::new(
            "host_firewall_block",
            "The site showed HighVi's isolated browser a Cloudflare bot check instead of the page. HighVi does not use browser sessions or get past bot checks.",
        );
    }
    match page.document_status {
        Some(status @ (401 | 403 | 451)) => PublicError::new(
            "access_denied",
            format!(
                "The site refused to show the page to HighVi's isolated browser (HTTP {status}). HighVi does not use browser sessions or bypass bot protection."
            ),
        ),
        Some(429) => PublicError::new(
            "rate_limited",
            "The site reports too many requests (HTTP 429). Wait a while before trying again.",
        ),
        Some(status) if status >= 400 => PublicError::new(
            "page_unavailable",
            format!("The page did not load in the isolated browser (HTTP {status})."),
        ),
        _ if page.document_failed => PublicError::new(
            "page_unavailable",
            "The page did not load in the isolated browser.",
        ),
        _ if page.refused_playlists > 0 => PublicError::new(
            "playlist_access_denied",
            "The page's player asked for a video playlist, but its server refused (HTTP 4xx). HighVi does not use browser sessions or bypass access controls.",
        ),
        None => PublicError::new(
            "media_discovery_timeout",
            "The page did not arrive in the isolated browser in time, so no video could be found.",
        ),
        _ if page.events_dropped > 0 || page.untracked_requests > 0 => PublicError::new(
            "discovery_overloaded",
            "The page sent more network activity than HighVi watches safely, and no video stream was found in what it could watch.",
        ),
        _ => PublicError::new(
            "no_media_found",
            "HighVi opened the page in an isolated browser, but no video stream appeared.",
        ),
    }
}

/// yt-dlp failures that mean "nothing found here", as opposed to a refusal
/// (blocked address, sign-in, DRM, 403…) that a headless browser must not
/// work around. Some refusals open the visible window instead, where the
/// user acts ([`window_opens_unasked`]).
fn worth_a_browser(error: &PublicError) -> bool {
    matches!(
        error.code.as_str(),
        "unsupported_page" | "hls_not_found" | "no_formats" | "yt_dlp_failed"
    )
}

/// Runs yt-dlp's metadata extraction on `target` and picks formats.
#[allow(clippy::too_many_arguments)]
async fn probe(
    env: &JobEnvironment,
    job: &JobDir,
    policy: &YtDlpPolicy<'_>,
    target: &url::Url,
    (kind, fetching): (SourceKind, FetchTarget),
    stats: &EgressStats,
    cancel: &mut CancelToken,
    recorder: &Mutex<Diagnostics>,
) -> Result<MediaInfo, PublicError> {
    let before = stats.counts();
    let list = job.path().join(crate::ytdlp::SOURCE_LIST);
    tokio::fs::write(&list, crate::ytdlp::source_list(target))
        .await
        .map_err(|_| output_dir_error())?;
    let mut command = tool_command(&env.yt_dlp, policy.discovery_args(), ytdlp_env(job.path()));
    command.current_dir(job.path());
    let captured = run_captured(
        command,
        MAX_METADATA_BYTES,
        MAX_DIAGNOSTIC_BYTES,
        DISCOVERY_TIMEOUT,
        cancel,
    )
    .await;
    let _ = tokio::fs::remove_file(&list).await;
    let captured = captured.map_err(|error| run_error(error, "yt-dlp", "discovery_timeout"))?;

    let signals = signals_from_output(&String::from_utf8_lossy(&captured.stderr.bytes));
    record(recorder, |diagnostics| {
        diagnostics.signals(signals.labels())
    });
    if !captured.success {
        return Err(network_refusal(stats.counts().since(before))
            .unwrap_or_else(|| signals.classify(kind, fetching)));
    }
    // Oversized stderr is only truncated diagnostics; oversized metadata is unsafe.
    if captured.stdout.overflowed {
        return Err(PublicError::new(
            "extractor_output_too_large",
            "The media page returned more metadata than HighVi can safely process.",
        ));
    }
    select_media(&captured.stdout.bytes)
}

#[allow(clippy::too_many_arguments)]
async fn download_track(
    env: &JobEnvironment,
    job: &JobDir,
    policy: &YtDlpPolicy<'_>,
    format_id: &str,
    stem: &str,
    stage: Stage,
    kind: SourceKind,
    stats: &EgressStats,
    cancel: &mut CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
    recorder: &Mutex<Diagnostics>,
) -> Result<(), PublicError> {
    report(JobUpdate::stage(stage));
    let before = stats.counts();
    let mut command = tool_command(
        &env.yt_dlp,
        policy.download_args(format_id, stem),
        ytdlp_env(job.path()),
    );
    command.current_dir(job.path());
    let mut signals = ToolSignals::default();
    let free = || env.free_space();
    let watch = Watch {
        deadline: env.limits.track_deadline,
        stall: Some(env.limits.stall),
        disk: Some(DiskFloor {
            free: &free,
            floor: env.limits.disk_floor,
        }),
    };
    let success = run_streaming(command, watch, cancel, |line, stream| {
        // The progress template writes to stdout; a look-alike on stderr
        // (remote text in an error) must not move the stall meter.
        match parse_progress_line(line, stage).filter(|_| stream == Stream::Stdout) {
            Some((update, bytes)) => {
                report(update);
                bytes
            }
            None => {
                signals.observe(line);
                None
            }
        }
    })
    .await
    .map_err(|error| match error {
        RunError::Stalled => stalled_error(&env.limits),
        RunError::TimedOut => PublicError::new(
            "download_timeout",
            format!(
                "The download took longer than HighVi allows for one track ({} hours).",
                env.limits.track_deadline.as_secs() / 3600
            ),
        ),
        error => run_error(error, "yt-dlp", "download_timeout"),
    })?;
    record(recorder, |diagnostics| {
        diagnostics.signals(signals.labels())
    });
    if !success {
        return Err(network_refusal(stats.counts().since(before))
            .unwrap_or_else(|| signals.classify(kind, FetchTarget::Media)));
    }
    Ok(())
}

async fn mux(
    env: &JobEnvironment,
    job: &JobDir,
    has_audio: bool,
    cancel: &mut CancelToken,
) -> Result<(), PublicError> {
    let video_path = find_track(job.path(), "video").await?;
    let video = MuxInput {
        container: sniff_file(&video_path).await?,
        path: &video_path,
    };
    let audio_path = match has_audio {
        true => Some(find_track(job.path(), "audio").await?),
        false => None,
    };
    let audio_container = match &audio_path {
        Some(path) => Some(sniff_file(path).await?),
        None => None,
    };
    if let (Some(path), Some(Container::Ac3 | Container::Eac3)) = (&audio_path, audio_container) {
        unpack_dolby_in_place(path, cancel).await?;
    }
    let audio = audio_path
        .as_ref()
        .zip(audio_container)
        .map(|(path, container)| MuxInput { path, container });
    let mut inputs = 0;
    for path in [Some(&video_path), audio_path.as_ref()]
        .into_iter()
        .flatten()
    {
        inputs += tokio::fs::metadata(path)
            .await
            .map_or(0, |metadata| metadata.len());
    }
    let needed = mux_space_needed(inputs);
    if let Some(free) = env.free_space()
        && free < needed.saturating_add(env.limits.disk_floor)
    {
        return Err(no_space_error(Some(needed), free));
    }
    let output = job.path().join(OUTPUT_FILE);
    let env_vars = ["PATH", "LANG"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (name, value)));
    let mut command = tool_command(
        &env.ffmpeg,
        mux_args(&video, audio.as_ref(), &output),
        env_vars,
    );
    command.current_dir(job.path());
    // Watched like a download: the output can outgrow its inputs, and other
    // programs use the disk too, so the floor is enforced while it writes.
    let free = || env.free_space();
    let watch = Watch {
        deadline: MUX_TIMEOUT,
        stall: None,
        disk: Some(DiskFloor {
            free: &free,
            floor: env.limits.disk_floor,
        }),
    };
    let success = run_streaming(command, watch, cancel, |_, _| None)
        .await
        .map_err(|error| match error {
            RunError::NoSpace => no_space_error(None, u64::MAX),
            error => run_error(error, "FFmpeg", "mux_failed"),
        })?;
    let written = tokio::fs::symlink_metadata(&output)
        .await
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
    if !success || !written {
        return Err(match env.free_space() {
            Some(free) if free < env.limits.disk_floor => no_space_error(None, free),
            _ => mux_error(),
        });
    }
    for path in [Some(&video_path), audio_path.as_ref()]
        .into_iter()
        .flatten()
    {
        let _ = tokio::fs::remove_file(path).await;
    }
    Ok(())
}

/// The file yt-dlp wrote for `stem`, ignoring its partial/temporary files.
async fn find_track(directory: &Path, stem: &str) -> Result<PathBuf, PublicError> {
    let prefix = format!("{stem}.");
    let mut entries = tokio::fs::read_dir(directory)
        .await
        .map_err(|_| output_dir_error())?;
    let mut found = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(&prefix)
            && !name.ends_with(".part")
            && !name.ends_with(".ytdl")
            && !name.contains(".part-Frag")
        {
            found.push(entry.path());
        }
    }
    match found.as_slice() {
        [path] => Ok(path.clone()),
        _ => Err(PublicError::new(
            "output_missing",
            "yt-dlp finished, but the downloaded media file was not found.",
        )),
    }
}

/// Rewrites packed (E-)AC-3 audio as its plain frames, in place (see
/// [`crate::mux::unpack_dolby`]: no extra disk space), stopping on cancel.
async fn unpack_dolby_in_place(path: &Path, cancel: &CancelToken) -> Result<(), PublicError> {
    let path = path.to_path_buf();
    let token = cancel.clone();
    let unpacked = tokio::task::spawn_blocking(move || {
        crate::mux::unpack_dolby(&path, &|| token.is_cancelled())
    })
    .await
    .map_err(|_| mux_error())?;
    match unpacked {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => Err(cancelled_error()),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            Err(unsupported_container())
        }
        Err(_) => Err(mux_error()),
    }
}

fn unsupported_container() -> PublicError {
    PublicError::new(
        "unsupported_container",
        "The downloaded media is not MPEG-TS, MP4, AAC or Dolby (E-)AC-3, so HighVi cannot make an MP4 from it safely.",
    )
}

/// Longest leading ID3 tag read past to find what follows it.
const MAX_LEADING_ID3: u64 = 1024 * 1024;

async fn sniff_file(path: &Path) -> Result<crate::mux::Container, PublicError> {
    let mut start = [0_u8; 10];
    let mut file = tokio::fs::File::open(path).await.map_err(|_| mux_error())?;
    let read = file.read(&mut start).await.map_err(|_| mux_error())?;
    // Packed audio starts with an ID3 tag, which can be longer than the
    // sniffed bytes: read past all of it (bounded) to see the frames.
    let tag = if read == 10
        && start.starts_with(b"ID3")
        && start[6..10].iter().all(|byte| byte & 0x80 == 0)
    {
        let length = start[6..10]
            .iter()
            .fold(0_u64, |total, byte| (total << 7) | u64::from(*byte));
        let footer = if start[5] & 0x10 != 0 { 10 } else { 0 };
        (length + footer).min(MAX_LEADING_ID3)
    } else {
        0
    };
    let mut header = start[..read].to_vec();
    file.take(tag + SNIFF_BYTES as u64)
        .read_to_end(&mut header)
        .await
        .map_err(|_| mux_error())?;
    sniff(&header).ok_or_else(unsupported_container)
}

fn cancelled_error() -> PublicError {
    PublicError::new("cancelled", "The download was cancelled.")
}

fn stalled_error(limits: &JobLimits) -> PublicError {
    PublicError::new(
        "download_stalled",
        format!(
            "The download stopped making progress (less than {} KiB in {} seconds), so HighVi stopped it.",
            limits.stall.min_bytes / 1024,
            limits.stall.window.as_secs()
        ),
    )
}

/// Space a mux needs: about its inputs, plus a margin for container
/// overhead, so the disk watch does not stop it just before it finishes.
fn mux_space_needed(inputs: u64) -> u64 {
    inputs.saturating_add(inputs / 16)
}

fn no_space_error(needed: Option<u64>, free: u64) -> PublicError {
    let need = needed
        .filter(|needed| *needed > 0)
        .map(|needed| format!(" This video needs about {} MiB;", needed.div_ceil(MIB)))
        .unwrap_or_default();
    let free = if free == u64::MAX {
        String::new()
    } else {
        format!(" {} MiB is free.", free / MIB)
    };
    PublicError::new(
        "insufficient_disk_space",
        format!(
            "There is not enough free disk space for this download.{need}{free} HighVi keeps some space free so the computer keeps working."
        ),
    )
}

fn mux_error() -> PublicError {
    PublicError::new(
        "mux_failed",
        "HighVi could not combine the downloaded streams into an MP4. No file was saved.",
    )
}

/// When the proxy refused a request during a tool's run, that refusal is why
/// the tool failed.
fn network_refusal(refused: RefusalCounts) -> Option<PublicError> {
    if refused.address > 0 {
        Some(PublicError::new(
            "unsafe_network_target",
            "The media tried to reach a local, private or reserved network address. HighVi blocked it.",
        ))
    } else if refused.plain_http > 0 {
        Some(PublicError::new(
            "insecure_media_url",
            "Part of this media is served over plain HTTP. HighVi only downloads over HTTPS.",
        ))
    } else if refused.unresolvable > 0 {
        Some(PublicError::new(
            "network_lookup_failed",
            "A host name used by this media could not be resolved. Check the address and your connection.",
        ))
    } else {
        None
    }
}

fn run_error(error: RunError, tool: &str, timeout_code: &str) -> PublicError {
    match error {
        RunError::Cancelled => cancelled_error(),
        RunError::Spawn(error) if error.kind() == std::io::ErrorKind::NotFound => PublicError::new(
            "missing_dependency",
            format!("{tool} is not installed or is not available in PATH."),
        ),
        RunError::Spawn(_) => PublicError::new(
            "process_start_failed",
            format!("HighVi could not start {tool}."),
        ),
        RunError::Io => PublicError::new(
            "process_output_failed",
            format!("HighVi could not read output from {tool}."),
        ),
        RunError::TimedOut => match timeout_code {
            "discovery_timeout" => PublicError::new(
                "discovery_timeout",
                "Media discovery timed out after 60 seconds.",
            ),
            "mux_failed" => mux_error(),
            _ => PublicError::new(
                "download_timeout",
                "The download took longer than HighVi allows for one track.",
            ),
        },
        RunError::Stalled => stalled_error(&JobLimits::default()),
        RunError::NoSpace => no_space_error(None, u64::MAX),
    }
}

/// A progress update and the cumulative bytes downloaded, from a line in
/// HighVi's own progress template.
fn parse_progress_line(line: &str, stage: Stage) -> Option<(JobUpdate, Option<u64>)> {
    let raw = line.strip_prefix("HIGHVI_PROGRESS|")?;
    let mut fields = raw.splitn(5, '|');
    let _status = fields.next();
    let update = JobUpdate {
        stage,
        title: None,
        percent: clean_progress_value(fields.next()),
        speed: clean_progress_value(fields.next()),
        eta: clean_progress_value(fields.next()),
    };
    // yt-dlp prints integers, or floats for some downloaders.
    let bytes = fields
        .next()
        .map(str::trim)
        .and_then(|value| value.split('.').next())
        .and_then(|value| value.parse::<u64>().ok())
        // Absurd values are ignored rather than trusted.
        .filter(|bytes| *bytes <= MAX_PROGRESS_BYTES);
    Some((update, bytes))
}

fn clean_progress_value(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("NA") || value.eq_ignore_ascii_case("Unknown")
    {
        None
    } else {
        Some(
            value
                .chars()
                .filter(|character| !character.is_control())
                .take(32)
                .collect(),
        )
    }
}

/// File names players give every playlist, which say nothing about the
/// video.
fn is_generic_playlist_name(title: &str) -> bool {
    let name = title.trim().trim_matches('.').to_ascii_lowercase();
    let name = name.strip_suffix(".m3u8").unwrap_or(&name);
    matches!(
        name,
        "" | "m3u8"
            | "index"
            | "master"
            | "playlist"
            | "prog_index"
            | "main"
            | "stream"
            | "video"
            | "chunklist"
            | "manifest"
    )
}

/// The nearest folder of the playlist URL that looks like a name: letters,
/// digits, `-`, `_`, `.` only, so query-like or token-like segments
/// (`exp=…`, long hex) never become a file name.
fn playlist_folder_name(playlist: &url::Url) -> Option<String> {
    let segments: Vec<&str> = playlist.path_segments()?.collect();
    segments
        .iter()
        .rev()
        .skip(1)
        .filter_map(|segment| {
            let stem = segment.rsplit_once('.').map_or(*segment, |(stem, _)| stem);
            let letters = stem.chars().filter(char::is_ascii_alphabetic).count();
            let named = (3..=48).contains(&stem.len())
                && stem.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
                })
                && letters >= 2
                && !looks_random(stem)
                && !stem
                    .chars()
                    .all(|character| character.is_ascii_hexdigit() || character == '-');
            named.then(|| stem.to_string())
        })
        .find(|stem| !is_generic_playlist_name(stem))
}

/// Random IDs switch between letters and digits, or between cases, many
/// times within one word; names do so a few times at most
/// (`Movie2024`, `S01E02`, `BigBuckBunny`).
fn looks_random(stem: &str) -> bool {
    stem.split(['-', '_', '.']).any(|word| {
        let characters: Vec<char> = word.chars().collect();
        let switches = characters
            .windows(2)
            .filter(|pair| pair[0].is_ascii_digit() != pair[1].is_ascii_digit())
            .count();
        let humps = characters
            .windows(2)
            .filter(|pair| pair[0].is_ascii_lowercase() && pair[1].is_ascii_uppercase())
            .count();
        switches > 3 || humps > 3 || (switches > 1 && humps > 1)
    })
}

/// Paths to the tools a job needs, found on `PATH`.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_failures_a_window_may_fix_open_one_unasked() {
        for code in ["host_firewall_block", "access_denied", "no_media_found"] {
            assert!(window_opens_unasked(code), "{code}");
        }
        for code in [
            "page_unavailable",
            "browser_closed",
            "interactive_timeout",
            "drm_not_supported",
            "rate_limited",
            "unsafe_network_target",
            "cancelled",
        ] {
            assert!(!window_opens_unasked(code), "{code}");
        }
    }

    #[tokio::test]
    async fn packed_audio_behind_a_long_id3_tag_is_recognised() {
        let directory = crate::filesystem::temp_directory("long-id3");
        // A 2000-byte tag: far past the sniffed bytes.
        let length = 2000_u32;
        let size = [
            (length >> 21) as u8 & 0x7f,
            (length >> 14) as u8 & 0x7f,
            (length >> 7) as u8 & 0x7f,
            length as u8 & 0x7f,
        ];
        let mut bytes = b"ID3\x04\x00\x00".to_vec();
        bytes.extend(size);
        bytes.extend(vec![0_u8; length as usize]);
        bytes.extend([0x0b, 0x77, 0xb8, 0xe0, 0x1c, 0x30]);
        bytes.resize(bytes.len() + 1530, 0);
        let path = directory.join("audio.ac3");
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(sniff_file(&path).await.unwrap(), Container::Ac3);
        let aac = directory.join("audio.aac");
        let mut adts = bytes[..10 + length as usize].to_vec();
        adts.extend([0xff, 0xf1, 0x50, 0x80, 0x02, 0x1f, 0xfc]);
        std::fs::write(&aac, &adts).unwrap();
        assert_eq!(sniff_file(&aac).await.unwrap(), Container::Adts);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_mux_reserves_a_margin_above_its_inputs() {
        // An output a few percent larger than its inputs still fits.
        let inputs = 1_000 * MIB;
        assert!(mux_space_needed(inputs) >= inputs + inputs / 20);
        assert_eq!(mux_space_needed(u64::MAX), u64::MAX);
    }

    #[test]
    fn generic_playlist_names_fall_back_to_their_folder() {
        let folder = |raw: &str| playlist_folder_name(&url::Url::parse(raw).unwrap());
        for generic in [".m3u8", "master", "index", "Playlist", "prog_index.m3u8"] {
            assert!(is_generic_playlist_name(generic), "{generic}");
        }
        assert!(!is_generic_playlist_name("x36xhzz"));
        assert_eq!(
            folder("https://demo.example/k8s/video/tears-of-steel/tears-of-steel.ism/.m3u8")
                .as_deref(),
            Some("tears-of-steel")
        );
        assert_eq!(
            folder("https://cdn.example/examples/img_bipbop_adv_example_ts/master.m3u8").as_deref(),
            Some("img_bipbop_adv_example_ts")
        );
        // Token-like and hash-like folders are never used.
        assert_eq!(
            folder("https://cdn.example/exp=1~hmac=ab12/master.m3u8"),
            None
        );
        assert_eq!(
            folder("https://cdn.example/3f2a9c1b-77d4-4e0a-9b1e-0c2d5e6f7a8b/index.m3u8"),
            None
        );
        assert_eq!(folder("https://cdn.example/master.m3u8"), None);
        // Short random IDs and non-names are not used either.
        for raw in [
            "https://cdn.example/s/Xk9fQ2LmP7wZ/master.m3u8",
            "https://cdn.example/10s/master.m3u8",
            "https://cdn.example/a1b2c3d4e5/index.m3u8",
            "https://cdn.example/x7k2mq9pzr/index.m3u8",
            "https://cdn.example/1080p/index.m3u8",
        ] {
            assert_eq!(folder(raw), None, "{raw}");
        }
        // Real names with numbers or capitals are kept.
        for name in [
            "big_buck_bunny",
            "Sintel",
            "episode-12",
            "2024-recap",
            "BBB_1080p",
            "Movie2024",
            "Season2",
            "S01E02",
            "Avengers_Endgame_2019",
            "Breaking-Bad-S01E03",
            "BigBuckBunny",
        ] {
            assert_eq!(
                folder(&format!("https://cdn.example/{name}/master.m3u8")).as_deref(),
                Some(name),
                "{name}"
            );
        }
    }

    #[test]
    fn parses_only_structured_progress_lines() {
        let (progress, bytes) = parse_progress_line(
            "HIGHVI_PROGRESS|downloading| 42.1%|2.3MiB/s|00:12|1048576",
            Stage::DownloadingVideo,
        )
        .unwrap();
        assert_eq!(bytes, Some(1_048_576));
        assert_eq!(progress.stage, Stage::DownloadingVideo);
        assert_eq!(progress.percent.as_deref(), Some("42.1%"));
        assert_eq!(progress.speed.as_deref(), Some("2.3MiB/s"));
        assert_eq!(progress.eta.as_deref(), Some("00:12"));
        let (unknown, bytes) = parse_progress_line(
            "HIGHVI_PROGRESS|downloading|NA|Unknown|NA|NA",
            Stage::DownloadingAudio,
        )
        .unwrap();
        assert_eq!(bytes, None);
        let (_, fractional) = parse_progress_line(
            "HIGHVI_PROGRESS|downloading|1%|NA|NA|2048.0",
            Stage::DownloadingAudio,
        )
        .unwrap();
        assert_eq!(fractional, Some(2048));
        assert_eq!(unknown.percent, None);
        assert_eq!(unknown.speed, None);
        assert!(
            parse_progress_line("https://secret.example/path?token=abc", Stage::Merging).is_none()
        );
    }
}
