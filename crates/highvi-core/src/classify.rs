use crate::{PublicError, SourceKind};

/// What the failing yt-dlp run was fetching, so a refusal can be attributed
/// to the right stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FetchTarget {
    /// The page or playlist the user entered (metadata extraction).
    Source,
    /// A playlist the page's own player loaded in the isolated browser.
    BrowserPlaylist,
    /// The media file or segments, after metadata extraction succeeded.
    Media,
}

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

    pub fn classify(&self, kind: SourceKind, target: FetchTarget) -> PublicError {
        if self.errors.any() {
            self.errors.classify(kind, target)
        } else {
            self.other.classify(kind, target)
        }
    }

    /// Fixed names of the hints seen, for a diagnostic report.
    pub fn labels(&self) -> Vec<&'static str> {
        let mut labels = self.errors.labels();
        for label in self.other.labels() {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        if self.other.no_js_runtime || self.errors.no_js_runtime {
            labels.push("no_js_runtime");
        }
        labels
    }
}

#[derive(Debug, Default)]
struct Signals {
    drm: bool,
    no_space: bool,
    authentication: bool,
    firewall_block: bool,
    forbidden: bool,
    rate_limited: bool,
    not_found: bool,
    unsupported: bool,
    no_formats: bool,
    timeout: bool,
    /// A warning only: some formats may be missing. Never an error by itself.
    no_js_runtime: bool,
}

impl Signals {
    fn observe(&mut self, lower: &str) {
        self.drm |= lower.contains("drm protected");
        self.no_space |= lower.contains("no space left on device");
        self.authentication |= contains_word(lower, "sign in")
            || contains_word(lower, "log in")
            || lower.contains("login required")
            || lower.contains("requires authentication");
        // yt-dlp prints this for any 403 whose body is a Cloudflare block or
        // challenge page; it does not prove a JavaScript challenge.
        self.firewall_block |= lower.contains("cloudflare anti-bot challenge");
        self.forbidden |= lower.contains("http error 403");
        self.rate_limited |= lower.contains("http error 429");
        self.not_found |= lower.contains("http error 404") || lower.contains("http error 410");
        self.unsupported |= lower.contains("unsupported url:");
        self.no_formats |= lower.contains("no video formats")
            || lower.contains("requested format is not available");
        self.timeout |= lower.contains("timed out");
        self.no_js_runtime |= lower.contains("no supported javascript runtime");
    }

    fn labels(&self) -> Vec<&'static str> {
        [
            (self.drm, "drm"),
            (self.no_space, "no_space"),
            (self.authentication, "sign_in"),
            (self.firewall_block, "cloudflare"),
            (self.forbidden, "http_403"),
            (self.rate_limited, "http_429"),
            (self.not_found, "http_404"),
            (self.unsupported, "unsupported_url"),
            (self.no_formats, "no_formats"),
            (self.timeout, "timeout"),
        ]
        .into_iter()
        .filter_map(|(seen, label)| seen.then_some(label))
        .collect()
    }

    fn any(&self) -> bool {
        self.drm
            || self.no_space
            || self.authentication
            || self.firewall_block
            || self.forbidden
            || self.rate_limited
            || self.not_found
            || self.unsupported
            || self.no_formats
            || self.timeout
    }

    fn classify(&self, kind: SourceKind, target: FetchTarget) -> PublicError {
        if self.drm {
            PublicError::new(
                "drm_not_supported",
                "This media reports DRM protection. HighVi will not bypass DRM or access controls.",
            )
        } else if self.no_space {
            PublicError::new(
                "insufficient_disk_space",
                "The disk ran out of free space during the download.",
            )
        } else if self.authentication {
            PublicError::new(
                "authentication_required",
                "This media requires sign-in or access that HighVi will not bypass.",
            )
        } else if self.firewall_block {
            PublicError::new("host_firewall_block", forbidden_message(kind, true))
        } else if self.forbidden {
            match target {
                FetchTarget::Source => {
                    PublicError::new("access_denied", forbidden_message(kind, false))
                }
                FetchTarget::BrowserPlaylist => PublicError::new(
                    "playlist_access_denied",
                    "The page's player loaded a video playlist, but its server refused HighVi (HTTP 403). HighVi does not use browser sessions or bypass access controls.",
                ),
                FetchTarget::Media => PublicError::new(
                    "media_access_denied",
                    "The site described the video, but its server refused to send the video file (HTTP 403).",
                ),
            }
        } else if self.rate_limited {
            PublicError::new(
                "rate_limited",
                "The site reports too many requests (HTTP 429). Wait a while before trying again.",
            )
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

pub fn signals_from_output(output: &str) -> ToolSignals {
    let mut signals = ToolSignals::default();
    for line in output.lines().take(2_000) {
        signals.observe(line);
    }
    signals
}

pub fn classify_tool_failure(output: &str, kind: SourceKind) -> PublicError {
    signals_from_output(output).classify(kind, FetchTarget::Source)
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
            signals
                .classify(SourceKind::DirectHls, FetchTarget::Source)
                .code,
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
    fn a_refusal_is_attributed_to_the_stage_that_was_refused() {
        let output = "ERROR: unable to download video data: HTTP Error 403: Forbidden";
        let signals = signals_from_output(output);
        for (target, code) in [
            (FetchTarget::Source, "access_denied"),
            (FetchTarget::BrowserPlaylist, "playlist_access_denied"),
            (FetchTarget::Media, "media_access_denied"),
        ] {
            assert_eq!(signals.classify(SourceKind::Page, target).code, code);
        }
        assert_eq!(signals.labels(), ["http_403"]);
        // A Cloudflare block is reported as such at every stage.
        let cloudflare = signals_from_output(CLOUDFLARE);
        assert_eq!(
            cloudflare
                .classify(SourceKind::Page, FetchTarget::Media)
                .code,
            "host_firewall_block"
        );
    }

    #[test]
    fn rate_limits_are_reported_and_a_missing_js_runtime_is_only_a_label() {
        let signals = signals_from_output(
            "WARNING: [youtube] No supported JavaScript runtime could be found.\nERROR: [youtube] abc: HTTP Error 429: Too Many Requests",
        );
        assert_eq!(
            signals.classify(SourceKind::Page, FetchTarget::Source).code,
            "rate_limited"
        );
        assert_eq!(signals.labels(), ["http_429", "no_js_runtime"]);
        let warning_only = signals_from_output(
            "WARNING: [youtube] No supported JavaScript runtime could be found.",
        );
        assert_eq!(
            warning_only
                .classify(SourceKind::Page, FetchTarget::Source)
                .code,
            "yt_dlp_failed"
        );
    }

    #[test]
    fn a_full_disk_is_not_reported_as_a_site_problem() {
        let error = classify_tool_failure(
            "ERROR: unable to write data: [Errno 28] No space left on device\nERROR: HTTP Error 403: Forbidden",
            SourceKind::Page,
        );
        assert_eq!(error.code, "insufficient_disk_space");
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
