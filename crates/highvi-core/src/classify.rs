use crate::{PublicError, SourceKind};

/// Failure hints seen in yt-dlp output. Only these booleans leave the stream;
/// raw lines, which may contain signed URLs, are never stored or returned.
/// Hints from `ERROR:` lines take precedence, so a warning or retry line
/// cannot outrank the error that actually stopped yt-dlp.
#[derive(Debug, Default)]
pub struct ToolSignals {
    errors: Signals,
    other: Signals,
}

impl ToolSignals {
    pub fn observe(&mut self, line: &str) {
        let lower = line.trim_start().to_ascii_lowercase();
        if lower.starts_with("error:") {
            self.errors.observe(&lower);
        } else {
            self.other.observe(&lower);
        }
    }

    pub fn classify(&self, kind: SourceKind) -> PublicError {
        if self.errors.any() {
            self.errors.classify(kind)
        } else {
            self.other.classify(kind)
        }
    }
}

#[derive(Debug, Default)]
struct Signals {
    drm: bool,
    authentication: bool,
    firewall_block: bool,
    forbidden: bool,
    not_found: bool,
    unsupported: bool,
    no_formats: bool,
    timeout: bool,
}

impl Signals {
    fn observe(&mut self, lower: &str) {
        self.drm |= lower.contains("drm protected");
        self.authentication |= contains_word(lower, "sign in")
            || contains_word(lower, "log in")
            || lower.contains("login required")
            || lower.contains("requires authentication");
        // yt-dlp prints this for any 403 whose body is a Cloudflare block or
        // challenge page; it does not prove a JavaScript challenge.
        self.firewall_block |= lower.contains("cloudflare anti-bot challenge");
        self.forbidden |= lower.contains("http error 403");
        self.not_found |= lower.contains("http error 404") || lower.contains("http error 410");
        self.unsupported |= lower.contains("unsupported url:");
        self.no_formats |= lower.contains("no video formats")
            || lower.contains("requested format is not available");
        self.timeout |= lower.contains("timed out");
    }

    fn any(&self) -> bool {
        self.drm
            || self.authentication
            || self.firewall_block
            || self.forbidden
            || self.not_found
            || self.unsupported
            || self.no_formats
            || self.timeout
    }

    fn classify(&self, kind: SourceKind) -> PublicError {
        if self.drm {
            PublicError::new(
                "drm_not_supported",
                "This media reports DRM protection. HighVi will not bypass DRM or access controls.",
            )
        } else if self.authentication {
            PublicError::new(
                "authentication_required",
                "This media requires sign-in or access that HighVi will not bypass.",
            )
        } else if self.firewall_block || self.forbidden {
            let code = if self.firewall_block {
                "host_firewall_block"
            } else {
                "access_denied"
            };
            PublicError::new(code, forbidden_message(kind, self.firewall_block))
        } else if self.not_found {
            PublicError::new(
                "media_not_found",
                "The host reports that this media does not exist (HTTP 404/410). A copied playlist may have expired; copy a fresh one from its page.",
            )
        } else if self.unsupported {
            unsupported_error(kind)
        } else if self.no_formats {
            PublicError::new("no_formats", "No downloadable media format was available.")
        } else if self.timeout {
            PublicError::new("network_timeout", "The media host did not respond in time.")
        } else {
            PublicError::new(
                "yt_dlp_failed",
                "yt-dlp could not process this media. Check that the URL is accessible and that yt-dlp is up to date.",
            )
        }
    }
}

pub fn classify_tool_failure(output: &str, kind: SourceKind) -> PublicError {
    let mut signals = ToolSignals::default();
    for line in output.lines().take(2_000) {
        signals.observe(line);
    }
    signals.classify(kind)
}

fn forbidden_message(kind: SourceKind, firewall: bool) -> String {
    let refused = if firewall {
        "The host's firewall (Cloudflare) refused the request (HTTP 403)."
    } else {
        "The host refused the request (HTTP 403)."
    };
    let advice = match kind {
        SourceKind::DirectHls => {
            "Check that the Referer page is the page where you found this playlist, or copy a fresh playlist."
        }
        SourceKind::BarePlaylist => {
            "This playlist may only load from the page that embeds it: switch to Direct HLS and enter that page."
        }
        SourceKind::Page => "The site may block automated downloads.",
    };
    format!("{refused} {advice} HighVi does not use browser sessions or bypass bot protection.")
}

fn unsupported_error(kind: SourceKind) -> PublicError {
    match kind {
        SourceKind::Page => PublicError::new(
            "unsupported_page",
            "yt-dlp has no support for this site and found no media in the page HTML. Pages that build their player with JavaScript are not supported. If you have the playlist URL and the page it came from, use Direct HLS.",
        ),
        SourceKind::BarePlaylist | SourceKind::DirectHls => PublicError::new(
            "unsupported_playlist",
            "yt-dlp could not read this URL as an HLS playlist. Check that it is the .m3u8 file itself and that it has not expired.",
        ),
    }
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(start, _)| {
        let end = start + needle.len();
        let before = haystack[..start].chars().next_back();
        let after = haystack[end..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLOUDFLARE: &str = "ERROR: [generic] Got HTTP Error 403 caused by Cloudflare anti-bot challenge; see  https://github.com/yt-dlp/yt-dlp#impersonation  for how to install the required impersonation dependency";

    #[test]
    fn unsupported_page_is_not_reported_as_forbidden() {
        let error = classify_tool_failure(
            "WARNING: [generic] Falling back on generic information extractor\nERROR: Unsupported URL: https://site.example/watch/1",
            SourceKind::Page,
        );
        assert_eq!(error.code, "unsupported_page");
        assert!(error.message.contains("Direct HLS"));
    }

    #[test]
    fn unsupported_playlist_does_not_suggest_direct_hls_again() {
        let error = classify_tool_failure(
            "ERROR: Unsupported URL: https://cdn.example/a.m3u8",
            SourceKind::DirectHls,
        );
        assert_eq!(error.code, "unsupported_playlist");
        assert!(!error.message.contains("use Direct HLS"));
    }

    #[test]
    fn error_lines_outrank_warnings_and_retries() {
        let error = classify_tool_failure(
            "WARNING: [generic] Request timed out, retrying\n[download] Got error: HTTP Error 404: Not Found. Retrying (1/3)\nERROR: Unsupported URL: https://site.example/w",
            SourceKind::Page,
        );
        assert_eq!(error.code, "unsupported_page");

        // Without a recognisable ERROR line, retry hints still classify.
        let mut signals = ToolSignals::default();
        signals
            .observe("[download] Got error: HTTP Error 403: Forbidden. Retrying fragment 1 (1/10)");
        signals.observe("ERROR: fragment 1 not found, unable to continue");
        assert_eq!(
            signals.classify(SourceKind::DirectHls).code,
            "access_denied"
        );
    }

    #[test]
    fn cloudflare_block_on_a_bare_playlist_points_to_direct_hls() {
        let error = classify_tool_failure(CLOUDFLARE, SourceKind::BarePlaylist);
        assert_eq!(error.code, "host_firewall_block");
        assert!(error.message.contains("Direct HLS"));
        assert!(!error.message.contains("browser session that"));
    }

    #[test]
    fn forbidden_advice_depends_on_the_source_kind() {
        let direct =
            classify_tool_failure("ERROR: HTTP Error 403: Forbidden", SourceKind::DirectHls);
        assert_eq!(direct.code, "access_denied");
        assert!(direct.message.contains("Referer page"));

        let page = classify_tool_failure("ERROR: HTTP Error 403: Forbidden", SourceKind::Page);
        assert_eq!(page.code, "access_denied");
        assert!(page.message.contains("block automated downloads"));
    }

    #[test]
    fn expired_or_missing_media_is_reported_separately() {
        for status in ["404: Not Found", "410: Gone"] {
            let error = classify_tool_failure(
                &format!("ERROR: HTTP Error {status}"),
                SourceKind::DirectHls,
            );
            assert_eq!(error.code, "media_not_found");
        }
    }

    #[test]
    fn sign_in_detection_requires_whole_words() {
        let design = classify_tool_failure("ERROR: unexpected design in player", SourceKind::Page);
        assert_eq!(design.code, "yt_dlp_failed");
        let sign_in = classify_tool_failure("ERROR: Sign in to confirm your age", SourceKind::Page);
        assert_eq!(sign_in.code, "authentication_required");
    }

    #[test]
    fn drm_wins_over_other_signals() {
        let error = classify_tool_failure(
            "ERROR: HTTP Error 403: Forbidden\nERROR: This video is DRM protected",
            SourceKind::Page,
        );
        assert_eq!(error.code, "drm_not_supported");
    }

    #[test]
    fn messages_never_echo_urls_or_tokens() {
        let output = "ERROR: [generic] Unable to download webpage: HTTP Error 403: Forbidden (caused by <HTTPError 403: https://secret.example/a.m3u8?token=abc>)\nERROR: Unsupported URL: https://secret.example/?token=abc\nERROR: HTTP Error 404 https://secret.example/x?sig=zzz";
        for kind in [
            SourceKind::Page,
            SourceKind::BarePlaylist,
            SourceKind::DirectHls,
        ] {
            let error = classify_tool_failure(output, kind);
            for secret in ["secret.example", "token", "abc", "sig=", "zzz"] {
                assert!(
                    !error.message.contains(secret),
                    "{secret} leaked: {}",
                    error.message
                );
                assert!(!error.code.contains(secret));
            }
        }
    }
}
