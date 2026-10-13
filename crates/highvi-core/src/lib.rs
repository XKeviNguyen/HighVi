mod discovery;
mod error;
mod filesystem;
mod process_output;
mod process_policy;
mod validation;

pub use discovery::{Discovery, extract_hls_from_json};
pub use error::PublicError;
#[cfg(target_os = "linux")]
pub use filesystem::rename_without_replacement;
pub use process_output::{CappedOutput, read_capped};
pub use process_policy::YT_DLP_HERMETIC_ARGS;
pub use validation::{
    ensure_public_network_target, suggested_filename, validate_output_filename, validate_page_url,
    validate_playlist_url,
};
