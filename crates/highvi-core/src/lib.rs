mod classify;
mod discovery;
mod error;
mod filesystem;
mod process_output;
mod process_policy;
mod source;
mod validation;
mod ytdlp_args;

pub use classify::{ToolSignals, classify_tool_failure};
pub use discovery::{Discovery, extract_hls_from_json};
pub use error::PublicError;
#[cfg(target_os = "linux")]
pub use filesystem::rename_without_replacement;
pub use process_output::{CappedOutput, read_capped};
pub use process_policy::{YT_DLP_ENV_PASSTHROUGH, YT_DLP_FIXED_ENV, YT_DLP_HERMETIC_ARGS};
pub use source::{
    SourceKind, SourceRequest, ValidatedSource, referer_for_request, validate_source,
};
pub use validation::{
    ensure_public_network_target, redact_url_for_display, suggested_filename,
    validate_output_filename, validate_page_url, validate_playlist_url, validate_referer_page_url,
};
pub use ytdlp_args::{discovery_args, download_args};
