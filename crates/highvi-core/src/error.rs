use serde::Serialize;
use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PublicError {
    pub code: String,
    pub message: String,
    /// Advice that depends on this machine rather than on the error itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<Hint>,
}

/// Machine-specific advice the UI translates. Values are fixed-format
/// strings from local tools, never from a website.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Hint {
    /// yt-dlp is older than `minimum` (what this site needs, or what HighVi
    /// is tested with), or, when `minimum` is absent, simply stale.
    #[serde(rename_all = "camelCase")]
    UpdateYtDlp {
        installed: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        minimum: Option<String>,
    },
}

impl PublicError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: None,
        }
    }
}

impl fmt::Display for PublicError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for PublicError {}
