use highvi_core::{
    AddReport, JobEnvironment, ProgressEvent, PublicError, QueueEvents, QueueSnapshot,
    RECOMMENDED_YT_DLP, Report, Runner, Scheduler, SharedResources, SourceRequest, YtDlpVersion,
    find_browser, find_executable, prepare_output_dir, resolve_python, run_job, yt_dlp_version,
};
use serde::Serialize;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};

const EXIT_GRACE: Duration = Duration::from_secs(5);

/// The queue owns every job. The webview only sends sources and item IDs;
/// it is told about progress through events and never sees a media URL.
struct AppState {
    queue: Scheduler,
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

/// Forwards queue changes to the webview. Emitting only queues a message
/// for the webview; it never calls back into the queue.
struct WebviewEvents(AppHandle);

impl QueueEvents for WebviewEvents {
    fn changed(&self, snapshot: QueueSnapshot) {
        let _ = self.0.emit("queue-changed", snapshot);
    }

    fn progress(&self, progress: ProgressEvent) {
        let _ = self.0.emit("queue-progress", progress);
    }
}

/// Adds sources to the queue. Each is validated in Rust; the webview is
/// untrusted.
#[tauri::command]
fn queue_add(state: State<'_, AppState>, sources: Vec<SourceRequest>) -> AddReport {
    state.queue.add(sources)
}

#[tauri::command]
fn queue_start(state: State<'_, AppState>) {
    state.queue.start();
}

#[tauri::command]
fn queue_pause(state: State<'_, AppState>) {
    state.queue.pause();
}

#[tauri::command]
fn queue_set_concurrency(state: State<'_, AppState>, desired: usize) -> Result<(), PublicError> {
    state.queue.set_concurrency(desired)
}

#[tauri::command]
fn queue_cancel(state: State<'_, AppState>, id: u64) {
    state.queue.cancel(id);
}

#[tauri::command]
fn queue_cancel_all(state: State<'_, AppState>) {
    state.queue.cancel_all();
}

#[tauri::command]
fn queue_retry(state: State<'_, AppState>, id: u64) {
    state.queue.retry(id);
}

#[tauri::command]
fn queue_retry_failed(state: State<'_, AppState>) {
    state.queue.retry_failed();
}

#[tauri::command]
fn queue_remove(state: State<'_, AppState>, id: u64) {
    state.queue.remove(id);
}

#[tauri::command]
fn queue_clear_completed(state: State<'_, AppState>) {
    state.queue.clear_completed();
}

#[tauri::command]
fn queue_snapshot(state: State<'_, AppState>) -> QueueSnapshot {
    state.queue.snapshot()
}

/// The diagnostic report of a failed item (no URLs, cookies or tokens; see
/// `highvi_core::Diagnostics`).
#[tauri::command]
fn queue_diagnostics(state: State<'_, AppState>, id: u64) -> Option<String> {
    state.queue.diagnostics(id)
}

#[tauri::command]
async fn dependency_status() -> DependencyStatus {
    let yt_dlp = find_executable("yt-dlp");
    let version = match &yt_dlp {
        Some(path) => {
            let python = match find_executable("python3") {
                Some(python) => resolve_python(&python).await,
                None => None,
            };
            yt_dlp_version(path, python.as_deref()).await
        }
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

/// Tools are looked up for each job, so installing one while HighVi runs
/// works without a restart. The shared limits are the same for all jobs.
fn job_environment(
    app: &AppHandle,
    shared: &Arc<SharedResources>,
) -> Result<JobEnvironment, PublicError> {
    let missing = |tool: &str| {
        PublicError::new(
            "missing_dependency",
            format!("{tool} is not installed or is not available in PATH."),
        )
    };
    Ok(JobEnvironment {
        shared: Arc::clone(shared),
        ..JobEnvironment::new(
            find_executable("yt-dlp").ok_or_else(|| missing("yt-dlp"))?,
            find_executable("ffmpeg").ok_or_else(|| missing("FFmpeg"))?,
            find_browser(),
            output_dir(app)?,
        )
    })
}

/// Runs one queued source with the real downloader.
fn job_runner(app: AppHandle) -> Runner {
    let shared = Arc::new(SharedResources::default());
    Arc::new(move |source, cancel, report: Report| {
        let app = app.clone();
        let shared = Arc::clone(&shared);
        Box::pin(async move {
            let environment = match job_environment(&app, &shared) {
                Ok(environment) => environment,
                Err(error) => return (Err(error), None),
            };
            let (result, diagnostics) =
                run_job(&environment, &source, cancel, &|update| report(update)).await;
            let report = match &result {
                Err(error) if error.code != "cancelled" => Some(diagnostics.render()),
                _ => None,
            };
            (result, report)
        })
    })
}

/// On exit: accept nothing, start nothing, cancel everything, and give the
/// jobs a moment to kill their tools and remove their working files.
/// Completed files are never touched.
fn finish_jobs(app: &AppHandle) {
    let queue = app.state::<AppState>().queue.clone();
    tauri::async_runtime::block_on(async {
        let _ = tokio::time::timeout(EXIT_GRACE, queue.shutdown()).await;
    });
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
        .setup(|app| {
            let handle = app.handle().clone();
            let runtime = tauri::async_runtime::handle().inner().clone();
            let queue = Scheduler::new(
                runtime,
                job_runner(handle.clone()),
                Arc::new(WebviewEvents(handle.clone())),
            );
            app.manage(AppState { queue });
            exit_on_termination_signals(handle);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            queue_add,
            queue_start,
            queue_pause,
            queue_set_concurrency,
            queue_cancel,
            queue_cancel_all,
            queue_retry,
            queue_retry_failed,
            queue_remove,
            queue_clear_completed,
            queue_snapshot,
            queue_diagnostics,
            dependency_status,
            open_output_folder
        ])
        .build(tauri::generate_context!())
        .expect("error while building HighVi")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                finish_jobs(app);
            }
        });
}
