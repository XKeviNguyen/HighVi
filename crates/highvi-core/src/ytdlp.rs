//! How HighVi runs yt-dlp.
//!
//! Every run is hermetic (no config, plugins, cookies, cache or `--exec`),
//! reaches the network only through the job's egress proxy, and cannot find
//! FFmpeg: yt-dlp would otherwise hand some streams to FFmpeg, which fetches
//! URLs itself and bypasses the proxy. Runs happen inside the job directory
//! with relative paths, so no user path is ever parsed as an output template.
//!
//! yt-dlp's `PATH` is a private directory holding only a `python3` link, so
//! it cannot start helper programs installed on the machine: extractors run
//! `phantomjs` (with no proxy setting) on page JavaScript. JavaScript
//! runtimes (`deno`, `node`, `bun`) are also searched for next to the Python
//! interpreter, not only on `PATH`, so they are disabled explicitly with
//! `--no-js-runtimes`.

use std::{ffi::OsString, path::Path};
use url::Url;

/// File names inside the job directory.
pub const INFO_JSON: &str = "info.json";
/// The directory that is yt-dlp's entire `PATH`.
pub const TOOL_BIN: &str = "bin";
/// Never created. Pointing `--ffmpeg-location` here disables FFmpeg in yt-dlp.
const NO_FFMPEG: &str = "no-ffmpeg";

const SOCKET_TIMEOUT_SECONDS: &str = "20";

/// Environment variables passed through; everything else is cleared so
/// ambient Python or yt-dlp settings cannot change its behaviour. Proxy
/// variables are deliberately absent: the egress proxy is the only route.
/// `PATH` is replaced by [`TOOL_BIN`].
pub const YT_DLP_ENV_PASSTHROUGH: &[&str] = &[
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    // Custom CA bundles, e.g. behind a TLS-inspecting corporate gateway.
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
];

/// Set on every run. `HOME` is set to the job directory by the caller.
pub const YT_DLP_FIXED_ENV: &[(&str, &str)] = &[
    ("YTDLP_NO_PLUGINS", "1"),
    ("PYTHONNOUSERSITE", "1"),
    ("PYTHONDONTWRITEBYTECODE", "1"),
];

/// Arguments every run starts with.
pub const YT_DLP_HERMETIC_ARGS: &[&str] = &[
    "--ignore-config",
    "--no-plugin-dirs",
    "--no-exec",
    "--no-cookies",
    "--no-cookies-from-browser",
    "--no-cache-dir",
    "--no-js-runtimes",
    "--color",
    "never",
];

/// Settings shared by every yt-dlp run of one job.
pub struct YtDlpPolicy<'a> {
    pub proxy_url: &'a str,
    /// Origin-only Referer, already derived from a parsed URL.
    pub referer: Option<&'a str>,
    #[cfg(test)]
    pub insecure_tls: bool,
}

impl YtDlpPolicy<'_> {
    fn base_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = YT_DLP_HERMETIC_ARGS.iter().map(OsString::from).collect();
        args.push(format!("--proxy={}", self.proxy_url).into());
        args.push(format!("--ffmpeg-location={NO_FFMPEG}").into());
        push_all(
            &mut args,
            &[
                "--socket-timeout",
                SOCKET_TIMEOUT_SECONDS,
                "--retries",
                "3",
                "--fragment-retries",
                "5",
                "--extractor-retries",
                "1",
            ],
        );
        // `--referer` is deprecated in favour of `--add-headers`. The value is
        // one argv element derived from a parsed `Url`: it has no CR or LF.
        if let Some(referer) = self.referer {
            args.push("--add-headers".into());
            args.push(format!("Referer:{referer}").into());
        }
        #[cfg(test)]
        if self.insecure_tls {
            args.push("--no-check-certificates".into());
        }
        args
    }

    /// Metadata-only run. The URL follows `--` so it is never an option.
    pub fn discovery_args(&self, target: &Url) -> Vec<OsString> {
        let mut args = self.base_args();
        push_all(
            &mut args,
            &["--dump-single-json", "--no-playlist", "--skip-download"],
        );
        args.push("--".into());
        args.push(target.as_str().into());
        args
    }

    /// Downloads one already-chosen format from the job's info JSON into
    /// `<stem>.<ext>`. `format_id` must have passed `formats::select_media`.
    pub fn download_args(&self, format_id: &str, stem: &str) -> Vec<OsString> {
        let mut args = self.base_args();
        args.push(format!("--load-info-json={INFO_JSON}").into());
        args.push(format!("--format={format_id}").into());
        push_all(
            &mut args,
            &[
                "--no-playlist",
                "--newline",
                "--progress",
                "--progress-template",
                "download:HIGHVI_PROGRESS|%(progress.status)s|%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s",
                // Fetch HLS in yt-dlp itself, and fail rather than silently
                // leave gaps where a fragment could not be downloaded.
                "--downloader",
                "m3u8:native",
                "--abort-on-unavailable-fragments",
                "--no-overwrites",
                "--no-mtime",
                "--fixup",
                "never",
            ],
        );
        args.push(format!("--output={stem}.%(ext)s").into());
        args
    }
}

fn push_all(args: &mut Vec<OsString>, values: &[&str]) {
    args.extend(values.iter().map(OsString::from));
}

/// Creates `<job_dir>/bin` with a link to `python3` and nothing else. The
/// link serves yt-dlp builds started through `/usr/bin/env python3` (the
/// upstream zipapp); packaged builds name their interpreter absolutely.
/// `python` must be the real interpreter, not a shim script that needs
/// other programs (see `tools::resolve_python`).
pub async fn prepare_tool_path(job_dir: &Path, python: Option<&Path>) -> std::io::Result<()> {
    let bin = job_dir.join(TOOL_BIN);
    tokio::fs::create_dir(&bin).await?;
    if let Some(python) = python {
        tokio::fs::symlink(python, bin.join("python3")).await?;
    }
    Ok(())
}

/// The environment for a run in `job_dir`.
pub fn ytdlp_env(job_dir: &Path) -> Vec<(&'static str, OsString)> {
    let mut env: Vec<(&'static str, OsString)> = YT_DLP_ENV_PASSTHROUGH
        .iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (*name, value)))
        .collect();
    env.extend(
        YT_DLP_FIXED_ENV
            .iter()
            .map(|(name, value)| (*name, OsString::from(value))),
    );
    env.push(("PATH", job_dir.join(TOOL_BIN).into_os_string()));
    env.push(("HOME", job_dir.as_os_str().to_owned()));
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn policy(referer: Option<&str>) -> YtDlpPolicy<'_> {
        YtDlpPolicy {
            proxy_url: "http://127.0.0.1:4000",
            referer,
            insecure_tls: false,
        }
    }

    #[test]
    fn every_run_is_hermetic_proxied_and_has_no_ffmpeg() {
        let target = Url::parse("https://site.example/w").unwrap();
        for args in [
            strings(&policy(None).discovery_args(&target)),
            strings(&policy(None).download_args("hls-1", "video")),
        ] {
            assert_eq!(&args[..YT_DLP_HERMETIC_ARGS.len()], YT_DLP_HERMETIC_ARGS);
            assert!(args.contains(&"--proxy=http://127.0.0.1:4000".to_string()));
            assert!(args.contains(&"--ffmpeg-location=no-ffmpeg".to_string()));
            for forbidden in [
                "--cookies",
                "--cookies-from-browser",
                "--exec",
                "--plugin-dirs",
                "--impersonate",
                "--referer",
                "--no-check-certificates",
                "--merge-output-format",
                "--remux-video",
            ] {
                assert!(
                    !args.iter().any(|arg| arg.starts_with(forbidden)),
                    "{forbidden} in {args:?}"
                );
            }
        }
    }

    #[test]
    fn url_is_the_last_argument_after_a_separator() {
        let target = Url::parse("https://site.example/-watch?q=--exec").unwrap();
        let args = strings(&policy(Some("https://site.example/")).discovery_args(&target));
        assert_eq!(&args[args.len() - 2..], ["--", target.as_str()]);
    }

    #[test]
    fn referer_is_one_header_argument() {
        let target = Url::parse("https://cdn.example/a.m3u8").unwrap();
        let policy = policy(Some("https://site.example/"));
        for args in [
            strings(&policy.discovery_args(&target)),
            strings(&policy.download_args("hls-1", "video")),
        ] {
            let index = args.iter().position(|arg| arg == "--add-headers").unwrap();
            assert_eq!(args[index + 1], "Referer:https://site.example/");
        }
        let none = strings(&self::policy(None).discovery_args(&target));
        assert!(!none.iter().any(|arg| arg == "--add-headers"));
    }

    #[test]
    fn download_is_native_gapless_relative_and_never_overwrites() {
        let args = strings(&policy(None).download_args("-hls-1", "video"));
        for required in [
            "--abort-on-unavailable-fragments",
            "--no-overwrites",
            "--format=-hls-1",
            "--load-info-json=info.json",
            "--output=video.%(ext)s",
        ] {
            assert!(args.contains(&required.to_string()), "{required}");
        }
        let index = args.iter().position(|arg| arg == "--downloader").unwrap();
        assert_eq!(args[index + 1], "m3u8:native");
    }

    #[test]
    fn environment_drops_proxies_and_python_overrides() {
        for dropped in [
            "PYTHONPATH",
            "PYTHONSTARTUP",
            "PYTHONHOME",
            "XDG_CONFIG_HOME",
            "YTDLP_PLUGINS",
            "http_proxy",
            "https_proxy",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "no_proxy",
            "NO_PROXY",
            "HOME",
            "PATH",
        ] {
            assert!(!YT_DLP_ENV_PASSTHROUGH.contains(&dropped), "{dropped}");
        }
        let env = ytdlp_env(Path::new("/job"));
        assert!(env.contains(&("HOME", OsString::from("/job"))));
        let paths: Vec<_> = env.iter().filter(|(name, _)| *name == "PATH").collect();
        assert_eq!(paths, [&("PATH", OsString::from("/job/bin"))]);
        assert!(env.contains(&("PYTHONNOUSERSITE", OsString::from("1"))));
    }
}
