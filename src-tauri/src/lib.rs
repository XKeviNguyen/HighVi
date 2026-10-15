use highvi_core::{
    CancelHandle, JobEnvironment, JobUpdate, PublicError, RECOMMENDED_YT_DLP, SourceRequest,
    YtDlpVersion, cancel_pair, find_browser, find_executable, prepare_output_dir, run_job,
    validate_source, yt_dlp_version,
};
use serde::Serialize;
use std::{path::PathBuf, sync::Mutex, time::Duration};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
use tokio::sync::watch;

const MAX_JOB_ID_CHARS: usize = 64;
const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The single job that may run at a time.
#[derive(Default)]
struct AppState {
    active: Mutex<Option<ActiveJob>>,
}

struct ActiveJob {
    cancel: CancelHandle,
    finished: watch::Receiver<bool>,
}

/// Frees the job slot when the job's command ends, however it ends.
struct SlotGuard<'a> {
    state: &'a AppState,
    finished: watch::Sender<bool>,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        self.state
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        self.finished.send_replace(true);
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobResult {
    saved_to: String,
    title: String,
}

/// A failed job: the error, plus a report the user can copy into a bug
/// report (no URLs, cookies or tokens; see `highvi_core::Diagnostics`).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobError {
    #[serde(flatten)]
    error: PublicError,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<String>,
}

impl From<PublicError> for JobError {
    fn from(error: PublicError) -> Self {
        Self {
            error,
            diagnostics: None,
        }
    }
}

/// A progress event. `job_id` lets the UI ignore events from an older job
/// that arrive late.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobEvent {
    job_id: String,
    #[serde(flatten)]
    update: JobUpdate,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DependencyStatus {
    yt_dlp: bool,
    ffmpeg: bool,
    /// Optional: only pages whose player is built by JavaScript need it.
    browser: bool,
    yt_dlp_version: Option<String>,
    /// Set when yt-dlp is older than the version HighVi is tested with.
    yt_dlp_recommended: Option<String>,
}

/// Discovers and downloads `source`. Resolves when the job ends. The webview
/// is untrusted: the source is validated here, and the job ID is only a
/// correlation tag for progress events.
#[tauri::command]
async fn start_job(
    app: AppHandle,
    state: State<'_, AppState>,
    job_id: String,
    source: SourceRequest,
) -> Result<JobResult, JobError> {
    if job_id.is_empty()
        || job_id.len() > MAX_JOB_ID_CHARS
        || !job_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(PublicError::new("invalid_job_id", "Invalid job identifier.").into());
    }
    let source = validate_source(&source)?;

    let (cancel, token) = cancel_pair();
    let (finished_sender, finished) = watch::channel(false);
    {
        let mut active = state
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if active.is_some() {
            return Err(PublicError::new(
                "download_already_active",
                "Another download is already running. Wait for it to finish.",
            )
            .into());
        }
        *active = Some(ActiveJob { cancel, finished });
    }
    let _slot = SlotGuard {
        state: &state,
        finished: finished_sender,
    };

    let environment = job_environment(&app)?;
    let report = |update: JobUpdate| {
        let _ = app.emit(
            "job-update",
            JobEvent {
                job_id: job_id.clone(),
                update,
            },
        );
    };
    let (outcome, diagnostics) = run_job(&environment, &source, token, &report).await;
    match outcome {
        Ok(outcome) => Ok(JobResult {
            saved_to: outcome.saved_to.to_string_lossy().into_owned(),
            title: outcome.title,
        }),
        Err(error) if error.code == "cancelled" => Err(error.into()),
        Err(error) => Err(JobError {
            error,
            diagnostics: Some(diagnostics.render()),
        }),
    }
}

/// Cancels the running job, if any. Idempotent.
#[tauri::command]
fn cancel_job(state: State<'_, AppState>) {
    if let Some(active) = state
        .active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        active.cancel.cancel();
    }
}

#[tauri::command]
async fn dependency_status() -> DependencyStatus {
    let yt_dlp = find_executable("yt-dlp");
    let version = match &yt_dlp {
        Some(path) => yt_dlp_version(path).await,
        None => None,
    };
    let recommended = YtDlpVersion {
        date: RECOMMENDED_YT_DLP,
    };
    DependencyStatus {
        yt_dlp: yt_dlp.is_some(),
        ffmpeg: find_executable("ffmpeg").is_some(),
        browser: find_browser().is_some(),
        yt_dlp_version: version.map(|version| version.to_string()),
        yt_dlp_recommended: version
            .filter(|version| *version < recommended)
            .map(|_| recommended.to_string()),
    }
}

/// Opens Downloads/HighVi in the file manager. The path is fixed; nothing
/// from the webview reaches the command line.
#[tauri::command]
async fn open_output_folder(app: AppHandle) -> Result<(), PublicError> {
    let directory = output_dir(&app)?;
    prepare_output_dir(&directory).await?;
    let opener = find_executable("xdg-open").ok_or_else(|| {
        PublicError::new(
            "open_folder_failed",
            "HighVi could not open the folder (xdg-open is missing).",
        )
    })?;
    let status = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(opener)
            .arg(&directory)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await;
    match status {
        Ok(Ok(status)) if status.success() => Ok(()),
        _ => Err(PublicError::new(
            "open_folder_failed",
            "HighVi could not open the folder.",
        )),
    }
}

/// `Downloads/HighVi`. Without an XDG Downloads entry (minimal desktops),
/// fall back to `~/Downloads`, as file managers do.
fn output_dir(app: &AppHandle) -> Result<PathBuf, PublicError> {
    let downloads = app
        .path()
        .download_dir()
        .or_else(|_| app.path().home_dir().map(|home| home.join("Downloads")))
        .map_err(|_| {
            PublicError::new(
                "download_directory_unavailable",
                "The system Downloads directory is unavailable.",
            )
        })?;
    Ok(downloads.join("HighVi"))
}

fn job_environment(app: &AppHandle) -> Result<JobEnvironment, PublicError> {
    let missing = |tool: &str| {
        PublicError::new(
            "missing_dependency",
            format!("{tool} is not installed or is not available in PATH."),
        )
    };
    Ok(JobEnvironment::new(
        find_executable("yt-dlp").ok_or_else(|| missing("yt-dlp"))?,
        find_executable("ffmpeg").ok_or_else(|| missing("FFmpeg"))?,
        find_browser(),
        output_dir(app)?,
    ))
}

/// On exit, cancel the running job and give it a moment to kill its tools
/// and remove its working files.
fn finish_active_job(app: &AppHandle) {
    let finished = app
        .state::<AppState>()
        .active
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|active| {
            active.cancel.cancel();
            active.finished.clone()
        });
    if let Some(mut finished) = finished {
        tauri::async_runtime::block_on(async {
            let _ = tokio::time::timeout(EXIT_GRACE, finished.wait_for(|done| *done)).await;
        });
    }
}

/// Logout and `kill` send SIGTERM/SIGHUP, which would end the process without
/// running the exit path. Route them through `exit` so jobs clean up.
fn exit_on_termination_signals(app: AppHandle) {
    use tokio::signal::unix::{SignalKind, signal};
    tauri::async_runtime::spawn(async move {
        let (Ok(mut term), Ok(mut hangup), Ok(mut interrupt)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
            signal(SignalKind::interrupt()),
        ) else {
            return;
        };
        tokio::select! {
            _ = term.recv() => {}
            _ = hangup.recv() => {}
            _ = interrupt.recv() => {}
        }
        app.exit(0);
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            exit_on_termination_signals(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_job,
            cancel_job,
            dependency_status,
            open_output_folder
        ])
        .build(tauri::generate_context!())
        .expect("error while building HighVi")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                finish_active_job(app);
            }
        });
}
