use highvi_core::{
    Discovery, PublicError, SourceRequest, ToolSignals, ValidatedSource, YT_DLP_ENV_PASSTHROUGH,
    YT_DLP_FIXED_ENV, classify_tool_failure, discovery_args, download_args,
    ensure_public_network_target, extract_hls_from_json, read_capped, redact_url_for_display,
    rename_without_replacement, validate_output_filename, validate_playlist_url, validate_source,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex, time::timeout};

const MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 256 * 1024;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PROGRESS_LINE_BYTES: usize = 16 * 1024;
const STALE_JOB_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const JOB_PREFIX: &str = ".highvi-job-";
const JOB_MARKER: &str = ".highvi-owned-job";
static JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct AppState {
    download_active: Mutex<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    source: SourceRequest,
    filename: String,
}

/// What the webview may see about a discovery. URLs are redacted because the
/// full playlist URL can carry access tokens.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DiscoveryView {
    title: String,
    suggested_filename: String,
    playlist_display: String,
    referer_display: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadResult {
    saved_to: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    stage: String,
    percent: Option<String>,
    speed: Option<String>,
    eta: Option<String>,
    message: String,
}

#[tauri::command]
async fn discover_video(source: SourceRequest) -> Result<DiscoveryView, PublicError> {
    let source = validate_source(&source)?;
    let discovery = inspect_source(&source).await?;
    let playlist = validate_playlist_url(&discovery.playlist_url)?;
    Ok(DiscoveryView {
        title: discovery.title,
        suggested_filename: discovery.suggested_filename,
        playlist_display: redact_url_for_display(&playlist),
        referer_display: source
            .download_referer()
            .and_then(|referer| url::Url::parse(&referer).ok())
            .map(|referer| redact_url_for_display(&referer)),
    })
}

#[tauri::command]
async fn download_video(
    app: AppHandle,
    state: State<'_, AppState>,
    request: DownloadRequest,
) -> Result<DownloadResult, PublicError> {
    let filename = validate_output_filename(&request.filename)?;
    {
        let mut active = state.download_active.lock().await;
        if *active {
            return Err(PublicError::new(
                "download_already_active",
                "Another download is already running. Wait for it to finish.",
            ));
        }
        *active = true;
    }

    let result = match validate_source(&request.source) {
        Ok(source) => run_download(&app, &source, &filename).await,
        Err(error) => Err(error),
    };
    *state.download_active.lock().await = false;
    result
}

/// A yt-dlp process with a cleared environment and no shell. The arguments
/// come from `highvi_core::ytdlp_args`, which starts them with the hermetic
/// policy.
fn ytdlp_command(args: Vec<OsString>) -> Command {
    let mut command = Command::new("yt-dlp");
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_clear()
        .envs(
            YT_DLP_ENV_PASSTHROUGH
                .iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (*name, value))),
        )
        .envs(YT_DLP_FIXED_ENV.iter().copied())
        .args(args);
    command
}

async fn inspect_source(source: &ValidatedSource) -> Result<Discovery, PublicError> {
    let target = source.discovery_target();
    ensure_public_network_target(target).await?;

    let referer = source.discovery_referer();
    let mut child = ytdlp_command(discovery_args(target, referer.as_deref()))
        .spawn()
        .map_err(|error| missing_tool_error(error, "yt-dlp"))?;
    let stdout = child.stdout.take().ok_or_else(process_start_error)?;
    let stderr = child.stderr.take().ok_or_else(process_start_error)?;
    let stdout_task = tokio::spawn(read_capped(stdout, MAX_METADATA_BYTES));
    let stderr_task = tokio::spawn(read_capped(stderr, MAX_DIAGNOSTIC_BYTES));

    let collected = timeout(DISCOVERY_TIMEOUT, async {
        let (status, stdout, stderr) = tokio::join!(child.wait(), stdout_task, stderr_task);
        (status, stdout, stderr)
    })
    .await;

    let (status, stdout, stderr) = match collected {
        Ok((status, stdout, stderr)) => (
            status.map_err(|_| process_start_error())?,
            stdout.map_err(|_| process_stream_error())??,
            stderr.map_err(|_| process_stream_error())??,
        ),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(PublicError::new(
                "discovery_timeout",
                "Media discovery timed out after 60 seconds.",
            ));
        }
    };

    // Oversized stderr is only truncated diagnostics; oversized metadata is unsafe.
    if stdout.overflowed {
        return Err(PublicError::new(
            "extractor_output_too_large",
            "The media page returned more metadata than HighVi can safely process.",
        ));
    }
    if !status.success() {
        return Err(classify_tool_failure(
            &String::from_utf8_lossy(&stderr.bytes),
            source.kind(),
        ));
    }

    let discovery = extract_hls_from_json(&stdout.bytes)?;
    let playlist_url = validate_playlist_url(&discovery.playlist_url)?;
    ensure_public_network_target(&playlist_url).await?;
    Ok(discovery)
}

async fn run_download(
    app: &AppHandle,
    source: &ValidatedSource,
    filename: &str,
) -> Result<DownloadResult, PublicError> {
    emit_progress(
        app,
        DownloadProgress {
            stage: "checking".to_string(),
            percent: None,
            speed: None,
            eta: None,
            message: "Checking the page and refreshing its playlist…".to_string(),
        },
    );

    // Rediscover here instead of trusting playlist data sent by the webview. This
    // catches expired URLs and keeps the DRM/network checks at the Rust boundary.
    let discovery = inspect_source(source).await?;
    let playlist_url = validate_playlist_url(&discovery.playlist_url)?;
    let referer = source.download_referer();

    let download_dir = app.path().download_dir().map_err(|_| {
        PublicError::new(
            "download_directory_unavailable",
            "The system Downloads directory is unavailable.",
        )
    })?;
    let highvi_dir = download_dir.join("HighVi");
    tokio::fs::create_dir_all(&highvi_dir).await.map_err(|_| {
        PublicError::new(
            "download_directory_unavailable",
            "HighVi could not create its folder inside Downloads.",
        )
    })?;
    let directory_metadata = tokio::fs::symlink_metadata(&highvi_dir)
        .await
        .map_err(|_| download_directory_error())?;
    if !directory_metadata.is_dir() || directory_metadata.file_type().is_symlink() {
        return Err(download_directory_error());
    }
    cleanup_stale_jobs(&highvi_dir).await;

    let stem = filename.strip_suffix(".mp4").unwrap_or(filename);
    let final_path = highvi_dir.join(filename);
    match tokio::fs::symlink_metadata(&final_path).await {
        Ok(_) => return Err(output_exists_error()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(download_directory_error()),
    }

    let job_dir = create_job_dir(&highvi_dir).await?;
    let output_template = job_dir.join(format!("{stem}.%(ext)s"));
    let job_output = job_dir.join(filename);

    let process_result: Result<(), PublicError> = async {
        let mut command = ytdlp_command(download_args(
            &playlist_url,
            referer.as_deref(),
            &output_template,
        ));

        let mut child = command
            .spawn()
            .map_err(|error| missing_tool_error(error, "yt-dlp"))?;
        let stdout = child.stdout.take().ok_or_else(process_start_error)?;
        let stderr = child.stderr.take().ok_or_else(process_start_error)?;
        let signals = Arc::new(StdMutex::new(ToolSignals::default()));

        let stdout_task = tokio::spawn(read_tool_stream(stdout, app.clone(), Arc::clone(&signals)));
        let stderr_task = tokio::spawn(read_tool_stream(stderr, app.clone(), Arc::clone(&signals)));

        let status = child.wait().await.map_err(|_| process_start_error())?;
        let (stdout_result, stderr_result) = tokio::join!(stdout_task, stderr_task);
        stdout_result.map_err(|_| process_stream_error())??;
        stderr_result.map_err(|_| process_stream_error())??;

        if !status.success() {
            let signals = signals
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            return Err(signals.classify(source.kind()));
        }
        if !job_output.is_file() {
            return Err(PublicError::new(
                "output_missing",
                "yt-dlp finished, but the expected MP4 file was not created.",
            ));
        }
        Ok(())
    }
    .await;

    if let Err(error) = process_result {
        let _ = tokio::fs::remove_dir_all(&job_dir).await;
        return Err(error);
    }

    let completed = job_output.clone();
    let destination = final_path.clone();
    let publish_result =
        tokio::task::spawn_blocking(move || rename_without_replacement(&completed, &destination))
            .await
            .map_err(|_| process_start_error())?;
    if let Err(error) = publish_result {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            let _ = tokio::fs::remove_dir_all(&job_dir).await;
            return Err(output_exists_error());
        }
        return Err(PublicError::new(
            "output_publish_failed",
            format!(
                "The completed video could not be published. It remains at {} for recovery.",
                job_output.to_string_lossy()
            ),
        ));
    }
    let _ = tokio::fs::remove_dir_all(&job_dir).await;

    emit_progress(
        app,
        DownloadProgress {
            stage: "finished".to_string(),
            percent: Some("100%".to_string()),
            speed: None,
            eta: None,
            message: "Download complete.".to_string(),
        },
    );

    Ok(DownloadResult {
        saved_to: final_path.to_string_lossy().into_owned(),
    })
}

async fn create_job_dir(parent: &Path) -> Result<PathBuf, PublicError> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for _ in 0..32 {
        let counter = JOB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            "{JOB_PREFIX}{}-{timestamp}-{counter}",
            std::process::id()
        ));
        match tokio::fs::create_dir(&path).await {
            Ok(()) => {
                let marker = path.join(JOB_MARKER);
                let marker_result = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(marker)
                    .await;
                if marker_result.is_ok() {
                    return Ok(path);
                }
                let _ = tokio::fs::remove_dir(&path).await;
                return Err(download_directory_error());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(download_directory_error()),
        }
    }
    Err(download_directory_error())
}

async fn cleanup_stale_jobs(parent: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(parent).await else {
        return;
    };
    let now = SystemTime::now();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(JOB_PREFIX) {
            continue;
        }

        let path = entry.path();
        let Ok(metadata) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            continue;
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if now.duration_since(modified).unwrap_or_default() < STALE_JOB_AGE {
            continue;
        }

        let Ok(marker) = tokio::fs::symlink_metadata(path.join(JOB_MARKER)).await else {
            continue;
        };
        if marker.is_file() && !marker.file_type().is_symlink() {
            let _ = tokio::fs::remove_dir_all(path).await;
        }
    }
}

fn download_directory_error() -> PublicError {
    PublicError::new(
        "download_directory_unavailable",
        "HighVi could not safely use its folder inside Downloads.",
    )
}

fn output_exists_error() -> PublicError {
    PublicError::new(
        "output_exists",
        "A file with this name already exists in Downloads/HighVi. Choose another filename.",
    )
}

async fn read_tool_stream<R>(
    mut reader: R,
    app: AppHandle,
    signals: Arc<StdMutex<ToolSignals>>,
) -> Result<(), PublicError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 8 * 1024];
    let mut line = Vec::with_capacity(512);
    let mut dropping_oversized_line = false;
    loop {
        let bytes_read = reader
            .read(&mut chunk)
            .await
            .map_err(|_| process_stream_error())?;
        if bytes_read == 0 {
            break;
        }
        for byte in &chunk[..bytes_read] {
            if *byte == b'\n' {
                if !dropping_oversized_line {
                    process_tool_line(&line, &app, &signals);
                }
                line.clear();
                dropping_oversized_line = false;
            } else if !dropping_oversized_line {
                if line.len() < MAX_PROGRESS_LINE_BYTES {
                    line.push(*byte);
                } else {
                    line.clear();
                    dropping_oversized_line = true;
                }
            }
        }
    }
    if !line.is_empty() && !dropping_oversized_line {
        process_tool_line(&line, &app, &signals);
    }
    Ok(())
}

fn process_tool_line(line: &[u8], app: &AppHandle, signals: &StdMutex<ToolSignals>) {
    let line = String::from_utf8_lossy(line);
    let line = line.trim_end_matches('\r');
    if let Some(progress) = parse_progress_line(line) {
        emit_progress(app, progress);
    } else {
        observe_tool_line(signals, line);
    }
}

fn parse_progress_line(line: &str) -> Option<DownloadProgress> {
    if let Some(raw) = line.strip_prefix("HIGHVI_PROGRESS|") {
        let mut fields = raw.splitn(4, '|');
        let status = fields.next().unwrap_or_default();
        let percent = clean_progress_value(fields.next());
        let speed = clean_progress_value(fields.next());
        let eta = clean_progress_value(fields.next());
        return Some(DownloadProgress {
            stage: "downloading".to_string(),
            percent,
            speed,
            eta,
            message: if status == "finished" {
                "Download finished; preparing the MP4…".to_string()
            } else {
                "Downloading media…".to_string()
            },
        });
    }
    if line.starts_with("HIGHVI_POSTPROCESS|") {
        return Some(DownloadProgress {
            stage: "processing".to_string(),
            percent: None,
            speed: None,
            eta: None,
            message: "Preparing the MP4…".to_string(),
        });
    }
    None
}

fn clean_progress_value(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("NA") {
        None
    } else {
        Some(value.chars().take(32).collect())
    }
}

fn observe_tool_line(signals: &StdMutex<ToolSignals>, line: &str) {
    signals
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .observe(line);
}

fn missing_tool_error(error: std::io::Error, tool: &str) -> PublicError {
    if error.kind() == std::io::ErrorKind::NotFound {
        PublicError::new(
            "missing_dependency",
            format!("{tool} is not installed or is not available in PATH."),
        )
    } else {
        process_start_error()
    }
}

fn process_start_error() -> PublicError {
    PublicError::new(
        "process_start_failed",
        "HighVi could not start the media downloader.",
    )
}

fn process_stream_error() -> PublicError {
    PublicError::new(
        "process_output_failed",
        "HighVi could not read progress from the media downloader.",
    )
}

fn emit_progress(app: &AppHandle, payload: DownloadProgress) {
    let _ = app.emit("download-progress", payload);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![discover_video, download_video])
        .run(tauri::generate_context!())
        .expect("error while running HighVi");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_structured_progress_lines() {
        let progress =
            parse_progress_line("HIGHVI_PROGRESS|downloading| 42.1%|2.3MiB/s|00:12").unwrap();
        assert_eq!(progress.stage, "downloading");
        assert_eq!(progress.percent.as_deref(), Some("42.1%"));
        assert_eq!(progress.speed.as_deref(), Some("2.3MiB/s"));
        assert_eq!(progress.eta.as_deref(), Some("00:12"));
        assert!(parse_progress_line("https://secret.example/path?token=abc").is_none());
    }
}
