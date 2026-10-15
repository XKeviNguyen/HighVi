//! HighVi's core: URL validation, the network boundary, and the download job.

#[cfg(not(target_os = "linux"))]
compile_error!("HighVi currently supports Linux only (renameat2, process groups).");

mod browser;
mod classify;
mod diagnostics;
mod egress;
mod error;
mod filesystem;
mod formats;
mod hls;
mod job;
mod mux;
mod netpolicy;
mod process;
mod process_output;
mod queue;
mod source;
mod tools;
mod validation;
mod ytdlp;

#[cfg(test)]
mod e2e;

pub use browser::find_browser;
pub use classify::{ToolSignals, classify_tool_failure};
pub use diagnostics::Diagnostics;
pub use error::{Hint, PublicError};
pub use filesystem::prepare_output_dir;
pub use job::{
    JobEnvironment, JobLimits, JobOutcome, JobUpdate, MAX_BROWSERS, MAX_MUXES, SharedResources,
    Stage, find_executable, run_job,
};
pub use process::{CancelHandle, CancelToken, cancel_pair};
pub use process_output::{CappedOutput, read_capped};
pub use queue::{
    AddReport, DEFAULT_CONCURRENCY, ItemStatus, ItemView, JobFuture, JobResult, MAX_ACTIVE_JOBS,
    ProgressEvent, QueueEvents, QueueSnapshot, Report, Runner, Scheduler,
};
pub use source::{SourceKind, SourceRequest, ValidatedSource, referer_origin, validate_source};
pub use tools::{RECOMMENDED_YT_DLP, YtDlpVersion, resolve_python, yt_dlp_version};
pub use validation::{
    suggested_filename, validate_page_url, validate_playlist_url, validate_referer_page_url,
};
