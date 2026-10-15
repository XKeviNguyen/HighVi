//! A short troubleshooting report for a failed job, safe to copy into a bug
//! report: stage timings, fixed-vocabulary outcomes, tool versions and the
//! website's host name. It never holds a URL path, query, cookie, header,
//! title or raw tool output, and it lives in memory only.

use crate::{
    PublicError, SourceKind, ValidatedSource,
    browser::PageReport,
    egress::RefusalCounts,
    job::Stage,
    tools::{self, ToolVersions, YtDlpVersion},
};
use std::{fmt::Write as _, time::Instant};

pub const MAX_REPORT_BYTES: usize = 4096;
const MAX_STAGES: usize = 32;
const MAX_SIGNALS: usize = 16;

#[derive(Debug)]
pub struct Diagnostics {
    started: Instant,
    kind: SourceKind,
    hosts: Vec<String>,
    stages: Vec<(Stage, u64)>,
    pub(crate) extractor: Option<String>,
    formats: Option<String>,
    signals: Vec<&'static str>,
    page: Option<PageReport>,
    candidates_checked: usize,
    egress: Option<(u64, RefusalCounts)>,
    outcome: Option<(String, Option<Stage>, u64)>,
    tools: ToolVersions,
    pub(crate) yt_dlp: Option<YtDlpVersion>,
}

impl Diagnostics {
    pub fn new(source: &ValidatedSource) -> Self {
        let hosts = match source {
            ValidatedSource::Page { page } => vec![page.host_str().unwrap_or_default()],
            ValidatedSource::DirectHls {
                playlist,
                referer_page,
            } => vec![
                playlist.host_str().unwrap_or_default(),
                referer_page.host_str().unwrap_or_default(),
            ],
        };
        Self {
            started: Instant::now(),
            kind: source.kind(),
            hosts: hosts.into_iter().map(clean_host).collect(),
            stages: Vec::new(),
            extractor: None,
            formats: None,
            signals: Vec::new(),
            page: None,
            candidates_checked: 0,
            egress: None,
            outcome: None,
            tools: ToolVersions::default(),
            yt_dlp: None,
        }
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn enter(&mut self, stage: Stage) {
        if self.stages.last().map(|(last, _)| *last) != Some(stage)
            && self.stages.len() < MAX_STAGES
        {
            let at = self.elapsed_ms();
            self.stages.push((stage, at));
        }
    }

    pub(crate) fn signals(&mut self, labels: Vec<&'static str>) {
        for label in labels {
            if !self.signals.contains(&label) && self.signals.len() < MAX_SIGNALS {
                self.signals.push(label);
            }
        }
    }

    pub(crate) fn media(&mut self, extractor: Option<&str>, formats: String) {
        self.extractor = extractor.map(clean_word);
        self.formats = Some(formats);
    }

    pub(crate) fn page(&mut self, page: PageReport) {
        self.page = Some(page);
    }

    pub(crate) fn candidate_checked(&mut self) {
        self.candidates_checked += 1;
    }

    pub(crate) fn egress(&mut self, tunnels: u64, refused: RefusalCounts) {
        self.egress = Some((tunnels, refused));
    }

    /// The site's yt-dlp extractor, or what the host name implies when
    /// extraction never got that far.
    pub(crate) fn site(&self) -> Option<&str> {
        self.extractor.as_deref().or_else(|| {
            self.hosts
                .first()
                .and_then(|host| tools::site_for_host(host))
        })
    }

    pub(crate) async fn collect_tools(&mut self, env: &crate::JobEnvironment) {
        let browser_used = self
            .stages
            .iter()
            .any(|(stage, _)| *stage == Stage::Browsing);
        let browser = env.chrome.as_deref().filter(|_| browser_used);
        let python = match &env.python {
            Some(python) => tools::resolve_python(python).await,
            None => None,
        };
        let (yt_dlp, ffmpeg, browser) = tokio::join!(
            tools::yt_dlp_version(&env.yt_dlp, python.as_deref()),
            tools::ffmpeg_version(&env.ffmpeg),
            async {
                match browser {
                    Some(path) => tools::browser_version(path).await,
                    None => None,
                }
            }
        );
        self.yt_dlp = yt_dlp;
        self.tools = ToolVersions {
            yt_dlp: yt_dlp.map(|version| version.to_string()),
            ffmpeg,
            browser,
        };
    }

    /// Whether the job was in `stage` when it failed.
    pub(crate) fn failed_in(&self, stage: Stage) -> bool {
        self.stages.last().map(|(last, _)| *last) == Some(stage)
    }

    pub(crate) fn finish(&mut self, error: &PublicError) {
        let stage = self.stages.last().map(|(stage, _)| *stage);
        self.outcome = Some((clean_word(&error.code), stage, self.elapsed_ms()));
    }

    /// The report, at most [`MAX_REPORT_BYTES`].
    pub fn render(&self) -> String {
        let mut text = String::new();
        let _ = writeln!(
            text,
            "HighVi {} diagnostics (no links, cookies or tokens)",
            env!("CARGO_PKG_VERSION")
        );
        let _ = writeln!(text, "system: {}", system_summary());
        let _ = writeln!(
            text,
            "tools: yt-dlp {}{}; ffmpeg {}{}",
            self.tools.yt_dlp.as_deref().unwrap_or("unknown"),
            self.yt_dlp
                .map(|version| format!(" ({} days old)", version.age_days(tools::today())))
                .unwrap_or_default(),
            self.tools.ffmpeg.as_deref().unwrap_or("unknown"),
            self.tools
                .browser
                .as_deref()
                .map(|browser| format!("; browser {browser}"))
                .unwrap_or_default(),
        );
        let kind = match self.kind {
            SourceKind::Page => "page",
            SourceKind::BarePlaylist => "playlist link",
            SourceKind::DirectHls => "playlist + page",
        };
        let _ = writeln!(text, "source: {kind} on {}", self.hosts.join(" / "));
        if let Some((code, stage, at)) = &self.outcome {
            let _ = writeln!(
                text,
                "result: {code} during {} after {:.1} s",
                stage.map_or("start", stage_name),
                *at as f64 / 1000.0
            );
        }
        let timeline: Vec<String> = self
            .stages
            .iter()
            .map(|(stage, at)| format!("{} @{:.1}s", stage_name(*stage), *at as f64 / 1000.0))
            .collect();
        let _ = writeln!(text, "stages: {}", timeline.join(", "));
        if let Some(extractor) = &self.extractor {
            let _ = writeln!(text, "extractor: {extractor}");
        }
        if let Some(formats) = &self.formats {
            let _ = writeln!(text, "formats: {formats}");
        }
        if !self.signals.is_empty() {
            let _ = writeln!(text, "tool signals: {}", self.signals.join(", "));
        }
        if let Some(page) = &self.page {
            let _ = writeln!(
                text,
                "browser: document {}{}{}, loaded {}, played {}, frames {}, workers {}, requests {}, media responses {}, playlists {} (refused {}), candidates checked {}, events dropped {}, untracked requests {}, ended {}",
                page.document_status
                    .map_or_else(|| "none".to_string(), |status| status.to_string()),
                if page.bot_check {
                    " (Cloudflare bot check)"
                } else {
                    ""
                },
                if page.document_failed {
                    " (failed)"
                } else {
                    ""
                },
                yes(page.loaded),
                yes(page.played),
                page.frames,
                page.workers,
                page.requests,
                page.media_responses,
                page.playlists,
                page.refused_playlists,
                self.candidates_checked,
                page.events_dropped,
                page.untracked_requests,
                page.ended.label(),
            );
        }
        if let Some((tunnels, refused)) = &self.egress {
            let _ = writeln!(
                text,
                "network guard: {tunnels} connections; refused {} private, {} plain-http, {} unresolvable",
                refused.address, refused.plain_http, refused.unresolvable
            );
        }
        truncate(text)
    }
}

fn yes(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Discovering => "discovering",
        Stage::Browsing => "browsing",
        Stage::DownloadingVideo => "downloadingVideo",
        Stage::DownloadingAudio => "downloadingAudio",
        Stage::Merging => "merging",
        Stage::Saving => "saving",
    }
}

/// `PRETTY_NAME` from os-release and the session type, reduced to a safe
/// character set.
fn system_summary() -> String {
    let os = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME="))
                .map(|value| clean_text(value.trim_matches('"'), 60))
        })
        .unwrap_or_else(|| "Linux".to_string());
    let session = std::env::var("XDG_SESSION_TYPE")
        .map(|value| clean_word(&value))
        .unwrap_or_else(|_| "unknown".to_string());
    format!("{os}; session {session}")
}

fn clean_text(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, ' ' | '.' | '-' | '_' | '(' | ')')
        })
        .take(limit)
        .collect()
}

/// Identifiers: extractor keys, error codes, session types.
fn clean_word(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '-')
        })
        .take(40)
        .collect()
}

/// Host names are already parsed (and IDNA-encoded) by `url`.
fn clean_host(host: &str) -> String {
    host.chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | ':' | '[' | ']')
        })
        .take(253)
        .collect()
}

fn truncate(mut text: String) -> String {
    if text.len() > MAX_REPORT_BYTES {
        let mut end = MAX_REPORT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourceRequest, browser::Ended, validate_source};

    fn source(request: SourceRequest) -> ValidatedSource {
        validate_source(&request).unwrap()
    }

    #[test]
    fn report_holds_hosts_and_fixed_labels_but_no_paths_or_tokens() {
        let mut diagnostics = Diagnostics::new(&source(SourceRequest::DirectHls {
            playlist_url: "https://cdn.example/secret/path/a.m3u8?token=abc123".into(),
            referer_page_url: "https://site.example/watch/9?session=zzz".into(),
        }));
        diagnostics.enter(Stage::Discovering);
        diagnostics.enter(Stage::Discovering);
        diagnostics.enter(Stage::DownloadingVideo);
        diagnostics.media(
            Some("Generic<script>"),
            "2 total, 1 usable; video m3u8_native mp4 720p".into(),
        );
        diagnostics.signals(vec!["http_403", "http_403"]);
        diagnostics.page(PageReport {
            document_status: Some(403),
            bot_check: true,
            ended: Ended::GaveUp,
            ..PageReport::default()
        });
        diagnostics.egress(4, RefusalCounts::default());
        diagnostics.finish(&PublicError::new("media_access_denied", "x"));
        let text = diagnostics.render();
        for expected in [
            "source: playlist + page on cdn.example / site.example",
            "result: media_access_denied during downloadingVideo",
            "stages: discovering @",
            "extractor: Genericscript",
            "tool signals: http_403\n",
            "document 403 (Cloudflare bot check)",
            "ended gave_up_after_play",
            "network guard: 4 connections",
        ] {
            assert!(text.contains(expected), "{expected} missing from:\n{text}");
        }
        for secret in [
            "secret", "path", "token=", "abc123", "watch", "session=", "zzz", "<",
        ] {
            assert!(!text.contains(secret), "{secret} leaked:\n{text}");
        }
        assert_eq!(text.matches("discovering").count(), 1);
    }

    #[test]
    fn report_size_is_bounded() {
        let mut diagnostics = Diagnostics::new(&source(SourceRequest::Page {
            page_url: format!("https://{}.example/", "a".repeat(60)),
        }));
        for _ in 0..1_000 {
            diagnostics.enter(Stage::Discovering);
            diagnostics.enter(Stage::Browsing);
            diagnostics.signals(vec!["http_403", "drm", "timeout"]);
        }
        diagnostics.media(Some(&"x".repeat(10_000)), "y".repeat(MAX_REPORT_BYTES * 2));
        let text = diagnostics.render();
        assert!(text.len() <= MAX_REPORT_BYTES, "{}", text.len());
        assert!(diagnostics.stages.len() <= MAX_STAGES);
    }
}
