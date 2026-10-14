//! One download job: discover → download → mux → publish.
//!
//! The job owns everything it learns. Media URLs, which may carry signed
//! tokens, never leave this module; callers see a title, progress, and the
//! final path.

use crate::{
    PublicError, SourceKind, ToolSignals, ValidatedSource,
    browser::{BrowserRun, discover_in_browser},
    classify_tool_failure,
    egress::{EgressProxy, EgressStats, RefusalCounts, Resolver},
    filesystem::{
        JobDir, cleanup_stale_jobs, output_dir_error, prepare_output_dir, publish_unique,
    },
    formats::{MediaInfo, select_media},
    mux::{MuxInput, SNIFF_BYTES, mux_args, sniff},
    process::{CancelToken, RunError, run_captured, run_streaming, tool_command},
    ytdlp::{INFO_JSON, YtDlpPolicy, ytdlp_env},
};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncReadExt;

const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 256 * 1024;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
const MUX_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const OUTPUT_FILE: &str = "output.mp4";

/// What a job needs from the machine it runs on.
pub struct JobEnvironment {
    pub yt_dlp: PathBuf,
    pub ffmpeg: PathBuf,
    /// Chrome/Chromium for pages whose player is built by JavaScript.
    /// Without it, those pages fail as yt-dlp reports them.
    pub chrome: Option<PathBuf>,
    /// The fixed output directory, e.g. `~/Downloads/HighVi`.
    pub output_dir: PathBuf,
    #[cfg(test)]
    pub(crate) fixture: Option<Resolver>,
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
            output_dir,
            #[cfg(test)]
            fixture: None,
        }
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

pub async fn run_job(
    env: &JobEnvironment,
    source: &ValidatedSource,
    mut cancel: CancelToken,
    report: &(dyn Fn(JobUpdate) + Send + Sync),
) -> Result<JobOutcome, PublicError> {
    report(JobUpdate::stage(Stage::Discovering));
    prepare_output_dir(&env.output_dir).await?;
    cleanup_stale_jobs(&env.output_dir).await;
    let mut job = JobDir::create(&env.output_dir).await?;
    let proxy = EgressProxy::start_with(env.resolver()).await.map_err(|_| {
        PublicError::new("proxy_failed", "HighVi could not start its network guard.")
    })?;
    let proxy_url = proxy.url();

    let discovered = Box::pin(discover(env, &job, &proxy, source, &mut cancel, report)).await?;
    let media = discovered.media;
    report(JobUpdate {
        title: Some(media.title.clone()),
        ..JobUpdate::stage(Stage::Discovering)
    });
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
            &job,
            &policy,
            format_id,
            stem,
            stage,
            source.kind(),
            proxy.stats(),
            &mut cancel,
            report,
        ))
        .await?;
    }
    drop(proxy);
    // The info JSON holds the media URLs, which may be signed; nothing after
    // this point needs it, and a kept recovery directory must not hold it.
    let _ = tokio::fs::remove_file(job.path().join(INFO_JSON)).await;

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
) -> Result<Discovered, PublicError> {
    let proxy_url = proxy.url();
    let referer = source.discovery_referer();
    let first = probe(
        env,
        job,
        &env.policy(&proxy_url, referer.as_deref()),
        source.discovery_target(),
        source.kind(),
        proxy.stats(),
        cancel,
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
    let finding = match finding {
        Ok(finding) => finding,
        // No browser installed: the original explanation is the useful one.
        Err(browser) if browser.code == "browser_unavailable" => return Err(error),
        Err(browser) => return Err(browser),
    };
    if finding.candidates.is_empty() {
        return Err(if finding.drm_only {
            PublicError::new(
                "drm_not_supported",
                "This media reports DRM protection. HighVi will not bypass DRM or access controls.",
            )
        } else {
            PublicError::new(
                "no_media_found",
                "HighVi opened the page in an isolated browser, but no video stream appeared.",
            )
        });
    }

    report(JobUpdate::stage(Stage::Discovering));
    let mut last_error = error;
    for candidate in finding.candidates {
        let checked = probe(
            env,
            job,
            &env.policy(&proxy_url, candidate.referer.as_deref()),
            &candidate.url,
            SourceKind::Page,
            proxy.stats(),
            cancel,
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

/// yt-dlp failures that mean "nothing found here", as opposed to a refusal
/// (blocked address, sign-in, DRM, 403…) that a browser must not work around.
fn worth_a_browser(error: &PublicError) -> bool {
    matches!(
        error.code.as_str(),
        "unsupported_page" | "hls_not_found" | "no_formats" | "yt_dlp_failed"
    )
}

/// Runs yt-dlp's metadata extraction on `target` and picks formats.
async fn probe(
    env: &JobEnvironment,
    job: &JobDir,
    policy: &YtDlpPolicy<'_>,
    target: &url::Url,
    kind: SourceKind,
    stats: &EgressStats,
    cancel: &mut CancelToken,
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

    if !captured.success {
        let output = String::from_utf8_lossy(&captured.stderr.bytes);
        return Err(network_refusal(stats.counts().since(before))
            .unwrap_or_else(|| classify_tool_failure(&output, kind)));
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
    let success = run_streaming(command, None, cancel, |line| {
        match parse_progress_line(line, stage) {
            Some(update) => report(update),
            None => signals.observe(line),
        }
    })
    .await
    .map_err(|error| run_error(error, "yt-dlp", "network_timeout"))?;
    if !success {
        return Err(
            network_refusal(stats.counts().since(before)).unwrap_or_else(|| signals.classify(kind))
        );
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
    let captured = run_captured(
        command,
        64 * 1024,
        MAX_DIAGNOSTIC_BYTES,
        MUX_TIMEOUT,
        cancel,
    )
    .await
    .map_err(|error| run_error(error, "FFmpeg", "mux_failed"))?;
    let written = tokio::fs::symlink_metadata(&output)
        .await
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
    if !captured.success || !written {
        return Err(mux_error());
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
            _ => PublicError::new("network_timeout", "The media host did not respond in time."),
        },
    }
}

fn parse_progress_line(line: &str, stage: Stage) -> Option<JobUpdate> {
    let raw = line.strip_prefix("HIGHVI_PROGRESS|")?;
    let mut fields = raw.splitn(4, '|');
    let _status = fields.next();
    Some(JobUpdate {
        stage,
        title: None,
        percent: clean_progress_value(fields.next()),
        speed: clean_progress_value(fields.next()),
        eta: clean_progress_value(fields.next()),
    })
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
    fn parses_only_structured_progress_lines() {
        let progress = parse_progress_line(
            "HIGHVI_PROGRESS|downloading| 42.1%|2.3MiB/s|00:12",
            Stage::DownloadingVideo,
        )
        .unwrap();
        assert_eq!(progress.stage, Stage::DownloadingVideo);
        assert_eq!(progress.percent.as_deref(), Some("42.1%"));
        assert_eq!(progress.speed.as_deref(), Some("2.3MiB/s"));
        assert_eq!(progress.eta.as_deref(), Some("00:12"));
        let unknown = parse_progress_line(
            "HIGHVI_PROGRESS|downloading|NA|Unknown|NA",
            Stage::DownloadingAudio,
        )
        .unwrap();
        assert_eq!(unknown.percent, None);
        assert_eq!(unknown.speed, None);
        assert!(
            parse_progress_line("https://secret.example/path?token=abc", Stage::Merging).is_none()
        );
    }
}
