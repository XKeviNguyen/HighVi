//! What the local tools are: versions for diagnostics, and whether yt-dlp is
//! recent enough for the site a job failed on.
//!
//! Version probes run `--version` only, with a cleared environment and a
//! short timeout. Their output is reduced to a fixed character set before it
//! goes anywhere.

use crate::{
    error::Hint,
    process::{cancel_pair, run_captured, tool_command},
    ytdlp::{YT_DLP_FIXED_ENV, prepare_tool_path, ytdlp_env},
};
use std::{
    ffi::OsString,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
static PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_VERSION_CHARS: usize = 60;
/// yt-dlp itself warns once a build is this old.
const STALE_AFTER_DAYS: i64 = 90;
/// The newest yt-dlp HighVi is tested with (CI matrix). Older builds still
/// work for plain HLS, but sites change and old extractors stop working.
pub const RECOMMENDED_YT_DLP: (u16, u8, u8) = (2026, 8, 19);
/// The oldest yt-dlp HighVi runs (CI matrix; Ubuntu 26.04 APT). Older
/// builds lack options HighVi's isolation depends on (`--no-plugin-dirs`,
/// `--no-js-runtimes`), so they are refused rather than run without them.
pub const MINIMUM_YT_DLP: (u16, u8, u8) = (2026, 3, 17);
/// Sites whose extractor is known to fail below a version, by yt-dlp
/// `extractor_key` (lower case). YouTube: since 2026-08-17 every format of
/// the `android_vr` client — the only one older builds use without a
/// JavaScript runtime — is refused with HTTP 403 (yt-dlp PR #17461).
const SITE_MINIMUMS: &[(&str, (u16, u8, u8))] = &[("youtube", (2026, 8, 19))];

/// A yt-dlp release, e.g. `2026.08.19` or a nightly `2026.09.27.232945`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct YtDlpVersion {
    pub date: (u16, u8, u8),
}

impl YtDlpVersion {
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split('.');
        let year: u16 = parts.next()?.parse().ok()?;
        let month: u8 = parts.next()?.parse().ok()?;
        let day: u8 = parts.next()?.parse().ok()?;
        ((2000..=2200).contains(&year) && (1..=12).contains(&month) && (1..=31).contains(&day))
            .then_some(Self {
                date: (year, month, day),
            })
    }

    fn days(self) -> i64 {
        let (year, month, day) = self.date;
        days_from_civil(i64::from(year), i64::from(month), i64::from(day))
    }

    /// Days between this release and `today` (days since the Unix epoch).
    pub fn age_days(self, today: i64) -> i64 {
        today - self.days()
    }
}

impl std::fmt::Display for YtDlpVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (year, month, day) = self.date;
        write!(formatter, "{year}.{month:02}.{day:02}")
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

pub fn today() -> i64 {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    i64::try_from(seconds / 86_400).unwrap_or(0)
}

/// Advice to update yt-dlp, when the installed build is older than what
/// `extractor` is known to need, or than HighVi's tested version, or simply
/// stale. `None` when updating would not be a reasonable suggestion.
pub fn update_advice(installed: YtDlpVersion, extractor: Option<&str>, today: i64) -> Option<Hint> {
    let site_minimum = extractor.and_then(|key| {
        let key = key.to_ascii_lowercase();
        SITE_MINIMUMS
            .iter()
            .find(|(site, _)| *site == key)
            .map(|(_, date)| *date)
    });
    let wanted = site_minimum.map_or(RECOMMENDED_YT_DLP, |date| date.max(RECOMMENDED_YT_DLP));
    let wanted = YtDlpVersion { date: wanted };
    let minimum = (installed < wanted).then(|| wanted.to_string());
    (minimum.is_some() || installed.age_days(today) > STALE_AFTER_DAYS).then(|| Hint::UpdateYtDlp {
        installed: installed.to_string(),
        minimum,
    })
}

/// The yt-dlp extractor a host belongs to, for sites with a known minimum,
/// so advice still applies when extraction failed before naming it.
pub fn site_for_host(host: &str) -> Option<&'static str> {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ["youtube.com", "youtu.be", "youtube-nocookie.com"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
        .then_some("youtube")
}

/// Error codes for which an outdated extractor is a plausible cause.
pub fn extractor_related(code: &str) -> bool {
    matches!(
        code,
        "access_denied"
            | "playlist_access_denied"
            | "media_access_denied"
            | "authentication_required"
            | "yt_dlp_failed"
            | "no_formats"
            | "hls_not_found"
            | "unsupported_page"
            | "invalid_extractor_output"
            | "ytdlp_too_old"
    )
}

/// Versions of the local tools, for a diagnostic report.
#[derive(Clone, Debug, Default)]
pub struct ToolVersions {
    pub yt_dlp: Option<String>,
    pub ffmpeg: Option<String>,
    pub browser: Option<String>,
}

/// A fresh private (0700) directory for one probe, removed when dropped.
/// Creation fails rather than reuse a path someone else made.
struct ProbeDir(PathBuf);

impl ProbeDir {
    fn create() -> Option<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "highvi-probe-{}-{nanos}-{}",
            std::process::id(),
            PROBE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new().mode(0o700).create(&path).ok()?;
        Some(Self(path))
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The interpreter a `python3` on `PATH` really runs. Version managers
/// (pyenv, asdf, mise) install shell-script shims that need other programs,
/// which yt-dlp's private `PATH` does not have.
pub async fn resolve_python(python: &Path) -> Option<PathBuf> {
    let output = probe(
        python,
        &["-I", "-c", "import sys; print(sys.executable)"],
        None,
    )
    .await?;
    let path = PathBuf::from(output.trim());
    (path.is_absolute() && std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file()))
        .then_some(path)
}

/// yt-dlp's version, run exactly as jobs run it (private `PATH` and `HOME`),
/// so a build that cannot start in a job is not reported as working.
pub async fn yt_dlp_version(yt_dlp: &Path, python: Option<&Path>) -> Option<YtDlpVersion> {
    let directory = ProbeDir::create()?;
    prepare_tool_path(&directory.0, python).await.ok()?;
    let output = probe(
        yt_dlp,
        // Only options every release knows, so old builds are identified
        // instead of failing on an unknown option.
        &["--ignore-config", "--version"],
        Some(ytdlp_env(&directory.0)),
    )
    .await?;
    YtDlpVersion::parse(output.lines().next()?)
}

/// `ffmpeg version 8.0.1-3ubuntu1 Copyright…` → `8.0.1-3ubuntu1`.
pub async fn ffmpeg_version(ffmpeg: &Path) -> Option<String> {
    let output = probe(ffmpeg, &["-hide_banner", "-version"], None).await?;
    let rest = output.lines().next()?.strip_prefix("ffmpeg version ")?;
    clean(rest.split_whitespace().next()?)
}

/// `Google Chrome 154.0.7590.0` (or Chromium's equivalent).
pub async fn browser_version(browser: &Path) -> Option<String> {
    let output = probe(browser, &["--version"], None).await?;
    clean(output.lines().next()?)
}

/// Runs `program args` for its output. With `env: None` the tool gets `PATH`
/// (interpreters, wrapper scripts), `LANG` and a fresh private `HOME`;
/// version flags never reach the network.
async fn probe(
    program: &Path,
    args: &[&str],
    env: Option<Vec<(&'static str, OsString)>>,
) -> Option<String> {
    let home = match env {
        Some(_) => None,
        None => Some(ProbeDir::create()?),
    };
    let env = env.unwrap_or_else(|| {
        let mut env: Vec<(&str, OsString)> = ["PATH", "LANG"]
            .into_iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (name, value)))
            .collect();
        env.extend(
            YT_DLP_FIXED_ENV
                .iter()
                .map(|(name, value)| (*name, OsString::from(value))),
        );
        if let Some(home) = &home {
            env.push(("HOME", home.0.clone().into_os_string()));
        }
        env
    });
    let (_handle, mut token) = cancel_pair();
    let captured = run_captured(
        tool_command(program, args, env),
        4096,
        4096,
        PROBE_TIMEOUT,
        &mut token,
    )
    .await
    .ok()?;
    captured
        .success
        .then(|| String::from_utf8_lossy(&captured.stdout.bytes).into_owned())
}

/// Version text limited to characters a version string needs.
fn clean(text: &str) -> Option<String> {
    let cleaned: String = text
        .trim()
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ' ' | '.' | '-' | '_' | '+')
        })
        .take(MAX_VERSION_CHARS)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> YtDlpVersion {
        YtDlpVersion::parse(text).unwrap()
    }

    #[test]
    fn parses_release_and_nightly_versions() {
        assert_eq!(version("2026.03.17\n").date, (2026, 3, 17));
        assert_eq!(version("2026.09.27.232945").date, (2026, 9, 27));
        assert_eq!(version("2026.8.19").to_string(), "2026.08.19");
        for invalid in [
            "",
            "latest",
            "1.2",
            "2026.13.01",
            "abc.def.ghi",
            "99999.1.1",
        ] {
            assert!(YtDlpVersion::parse(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn civil_dates_count_days_since_the_epoch() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(2026, 10, 15), 20_741);
        assert_eq!(version("2026.08.19").age_days(20_741), 57);
    }

    #[test]
    fn youtube_needs_the_release_that_dropped_android_vr() {
        let today = days_from_civil(2026, 10, 15);
        let hint = update_advice(version("2026.03.17"), Some("Youtube"), today).unwrap();
        assert_eq!(
            hint,
            Hint::UpdateYtDlp {
                installed: "2026.03.17".into(),
                minimum: Some("2026.08.19".into())
            }
        );
        assert!(update_advice(version("2026.08.19"), Some("Youtube"), today).is_none());
        // Other sites: older than tested is still worth an update…
        assert!(update_advice(version("2026.03.17"), Some("generic"), today).is_some());
        // …but a tested, recent build gets no advice.
        assert!(update_advice(version("2026.09.27"), None, today).is_none());
    }

    #[test]
    fn youtube_hosts_map_to_its_extractor() {
        for host in [
            "www.youtube.com",
            "youtube.com",
            "m.youtube.com",
            "youtu.be",
            "YOUTUBE.COM.",
        ] {
            assert_eq!(site_for_host(host), Some("youtube"), "{host}");
        }
        for host in ["notyoutube.com", "youtube.com.evil.example", "example.com"] {
            assert_eq!(site_for_host(host), None, "{host}");
        }
    }

    #[test]
    fn stale_builds_are_flagged_even_when_newer_than_tested() {
        let later = days_from_civil(2027, 6, 1);
        let hint = update_advice(version("2026.09.27"), None, later).unwrap();
        assert_eq!(
            hint,
            Hint::UpdateYtDlp {
                installed: "2026.09.27".into(),
                minimum: None
            }
        );
    }

    #[test]
    fn probe_homes_are_private_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let directory = ProbeDir::create().unwrap();
        let path = directory.0.clone();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        drop(directory);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_shim_python_resolves_to_the_real_interpreter() {
        use std::os::unix::fs::PermissionsExt;
        let Some(python) = crate::find_executable("python3") else {
            return;
        };
        let directory = ProbeDir::create().unwrap();
        // A pyenv-style shim: a shell script that execs the interpreter.
        let shim = directory.0.join("python3");
        std::fs::write(
            &shim,
            format!("#!/bin/sh\nexec '{}' \"$@\"\n", python.display()),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resolved = resolve_python(&shim).await.unwrap();
        assert_ne!(resolved, shim);
        assert!(resolved.is_absolute());
        assert!(!std::fs::read(&resolved).unwrap().starts_with(b"#!"));
    }

    #[test]
    fn version_text_is_reduced_to_a_safe_character_set() {
        assert_eq!(
            clean("Google Chrome 154.0.7590.0 \u{1b}[31m<script>").as_deref(),
            Some("Google Chrome 154.0.7590.0 31mscript")
        );
        assert_eq!(clean("\u{0}\u{7}"), None);
        assert_eq!(clean(&"9".repeat(500)).unwrap().len(), MAX_VERSION_CHARS);
    }
}
