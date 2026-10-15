//! One download job: discover → download → mux → publish.
//!
//! The job owns everything it learns. Media URLs, which may carry signed
//! tokens, never leave this module; callers see a title, progress, and the
//! final path.

use crate::{
    PublicError, SourceKind, ToolSignals, ValidatedSource,
    browser::{BrowserFinding, BrowserRun, discover_in_browser},
    classify::{FetchTarget, signals_from_output},
    diagnostics::Diagnostics,
    egress::{EgressProxy, EgressStats, RefusalCounts, Resolver},
    filesystem::{
        DiskBudget, JobDir, Reservation, cleanup_stale_jobs, free_bytes, output_dir_error,
        prepare_output_dir, publish_unique,
    },
    formats::{MediaInfo, select_media},
    mux::{MuxInput, SNIFF_BYTES, mux_args, sniff},
    process::{
        CancelToken, DiskFloor, RunError, StallPolicy, Stream, Watch, run_captured, run_streaming,
        tool_command,
    },
    tools::{extractor_related, resolve_python, today, update_advice},
    ytdlp::{INFO_JSON, YtDlpPolicy, prepare_tool_path, ytdlp_env},
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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
        }
    }
}

/// At most this many isolated browsers run at once, whatever the number of
/// jobs: Chrome is the heaviest process HighVi starts.
pub const MAX_BROWSERS: usize = 1;
/// FFmpeg muxes are short and disk-bound; two keep the disk busy without
/// competing for it.
pub const MAX_MUXES: usize = 2;

/// Limits shared by every job of one HighVi process. A job holds at most one
/// of these permits at a time, so waiting for one cannot deadlock.
#[derive(Debug)]
pub struct SharedResources {
    pub disk: Arc<DiskBudget>,
    pub browsers: tokio::sync::Semaphore,
    pub muxers: tokio::sync::Semaphore,
}

impl Default for SharedResources {
    fn default() -> Self {
        Self {
            disk: Arc::default(),
            browsers: tokio::sync::Semaphore::new(MAX_BROWSERS),
            muxers: tokio::sync::Semaphore::new(MAX_MUXES),
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

    fn policy<'a>(&self, proxy_url: &'a str, referer: Option<&'a str>) -> YtDlpPolicy<'a> {
        YtDlpPolicy {
            proxy_url,
            referer,
            #[cfg(test)]
            insecure_tls: self.fixture.is_some(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    Discovering,
    /// Watching the page in the isolated browser.
    Browsing,
    DownloadingVideo,
    DownloadingAudio,
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
    cancel: CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
) -> (Result<JobOutcome, PublicError>, Diagnostics) {
    let recorder = Mutex::new(Diagnostics::new(source));
    let mut late_cancel = cancel.clone();
    let tracked = |update: JobUpdate| {
        record(&recorder, |diagnostics| diagnostics.enter(update.stage));
        report(update);
    };
    let result = Box::pin(run(env, source, cancel, &tracked, &recorder)).await;
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

    report(JobUpdate::stage(Stage::Merging));
    let _muxer = tokio::select! {
        permit = env.shared.muxers.acquire() => permit.map_err(|_| mux_error())?,
        () = cancel.cancelled() => return Err(cancelled_error()),
    };
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
async fn fetch(
    env: &JobEnvironment,
    job: &JobDir,
    proxy: &EgressProxy,
    source: &ValidatedSource,
    cancel: &mut CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
    recorder: &Mutex<Diagnostics>,
) -> Result<(MediaInfo, Reservation), PublicError> {
    let proxy_url = proxy.url();
    let discovered = Box::pin(discover(env, job, proxy, source, cancel, report, recorder)).await?;
    let media = discovered.media;
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

/// What discovery settled on, and the Referer its media needs.
struct Discovered {
    media: MediaInfo,
    referer: Option<String>,
}

/// yt-dlp first; when it finds nothing on a page, the page's own player in
/// an isolated browser, with each playlist it loads checked by yt-dlp.
async fn discover(
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

    report(JobUpdate::stage(Stage::Browsing));
    let browser_permit = tokio::select! {
        permit = env.shared.browsers.acquire() => permit.map_err(|_| crate::browser::browser_failed())?,
        () = cancel.cancelled() => return Err(cancelled_error()),
    };
    let finding = discover_in_browser(
        BrowserRun {
            chrome,
            page,
            proxy_url: &proxy_url,
            work_dir: job.path(),
            #[cfg(test)]
            insecure_tls: env.fixture.is_some(),
        },
        cancel,
    )
    .await;
    // The profile holds whatever the page stored; it is not needed again.
    let _ = tokio::fs::remove_dir_all(job.path().join("chrome-profile")).await;
    // Chrome has exited (it is killed before discovery returns).
    drop(browser_permit);
    let finding = match finding {
        Ok(finding) => finding,
        // No browser installed: the original explanation is the useful one.
        Err(browser) if browser.code == "browser_unavailable" => return Err(error),
        Err(browser) => return Err(browser),
    };
    record(recorder, |diagnostics| {
        diagnostics.page(finding.page.clone())
    });
    if finding.candidates.is_empty() {
        return Err(empty_browser_result(&finding));
    }

    report(JobUpdate::stage(Stage::Discovering));
    let mut last_error = error;
    for candidate in finding.candidates {
        record(recorder, Diagnostics::candidate_checked);
        let checked = probe(
            env,
            job,
            &env.policy(&proxy_url, candidate.referer.as_deref()),
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
                if let Some(title) = &finding.title {
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
fn empty_browser_result(finding: &BrowserFinding) -> PublicError {
    let page = &finding.page;
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
/// (blocked address, sign-in, DRM, 403…) that a browser must not work around.
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
    let mut command = tool_command(
        &env.yt_dlp,
        policy.discovery_args(target),
        ytdlp_env(job.path()),
    );
    command.current_dir(job.path());
    let captured = run_captured(
        command,
        MAX_METADATA_BYTES,
        MAX_DIAGNOSTIC_BYTES,
        DISCOVERY_TIMEOUT,
        cancel,
    )
    .await
    .map_err(|error| run_error(error, "yt-dlp", "discovery_timeout"))?;

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
    let audio = match &audio_path {
        Some(path) => Some(MuxInput {
            container: sniff_file(path).await?,
            path,
        }),
        None => None,
    };
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

async fn sniff_file(path: &Path) -> Result<crate::mux::Container, PublicError> {
    let mut header = Vec::with_capacity(SNIFF_BYTES);
    let file = tokio::fs::File::open(path).await.map_err(|_| mux_error())?;
    file.take(SNIFF_BYTES as u64)
        .read_to_end(&mut header)
        .await
        .map_err(|_| mux_error())?;
    sniff(&header).ok_or_else(|| {
        PublicError::new(
            "unsupported_container",
            "The downloaded media is not MPEG-TS, MP4 or AAC, so HighVi cannot make an MP4 from it safely.",
        )
    })
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
    fn a_mux_reserves_a_margin_above_its_inputs() {
        // An output a few percent larger than its inputs still fits.
        let inputs = 1_000 * MIB;
        assert!(mux_space_needed(inputs) >= inputs + inputs / 20);
        assert_eq!(mux_space_needed(u64::MAX), u64::MAX);
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
