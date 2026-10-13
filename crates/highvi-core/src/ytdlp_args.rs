use crate::YT_DLP_HERMETIC_ARGS;
use std::{ffi::OsString, path::Path};
use url::Url;

const SOCKET_TIMEOUT_SECONDS: &str = "20";

/// Arguments for the metadata-only discovery run. The URL always follows `--`
/// so it can never be parsed as an option.
pub fn discovery_args(target: &Url, referer: Option<&str>) -> Vec<OsString> {
    let mut args = hermetic_args();
    push_all(
        &mut args,
        &[
            "--dump-single-json",
            "--no-playlist",
            "--skip-download",
            "--socket-timeout",
            SOCKET_TIMEOUT_SECONDS,
        ],
    );
    push_referer(&mut args, referer);
    args.push("--".into());
    args.push(target.as_str().into());
    args
}

/// Arguments for downloading `playlist` into `output_template`.
pub fn download_args(
    playlist: &Url,
    referer: Option<&str>,
    output_template: &Path,
) -> Vec<OsString> {
    let mut args = hermetic_args();
    push_all(
        &mut args,
        &[
            "--no-playlist",
            "--newline",
            "--progress",
            "--progress-template",
            "download:HIGHVI_PROGRESS|%(progress.status)s|%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s",
            "--progress-template",
            "postprocess:HIGHVI_POSTPROCESS|%(progress.status)s|%(progress.postprocessor)s",
            "--socket-timeout",
            SOCKET_TIMEOUT_SECONDS,
            // Fetch HLS in yt-dlp itself rather than handing URLs to FFmpeg.
            "--downloader",
            "m3u8:native",
            "--no-overwrites",
            "--no-post-overwrites",
            "--merge-output-format",
            "mp4",
            "--remux-video",
            "mp4",
        ],
    );
    push_referer(&mut args, referer);
    args.push("--output".into());
    args.push(output_template.as_os_str().to_owned());
    args.push("--".into());
    args.push(playlist.as_str().into());
    args
}

fn hermetic_args() -> Vec<OsString> {
    YT_DLP_HERMETIC_ARGS.iter().map(OsString::from).collect()
}

fn push_all(args: &mut Vec<OsString>, values: &[&str]) {
    args.extend(values.iter().map(OsString::from));
}

// `--referer` is deprecated in favour of `--add-headers "Referer:URL"`. The
// header is a single argv element; callers derive it from a parsed `Url`, so
// it cannot contain CR or LF.
fn push_referer(args: &mut Vec<OsString>, referer: Option<&str>) {
    if let Some(referer) = referer {
        args.push("--add-headers".into());
        args.push(format!("Referer:{referer}").into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn every_invocation_starts_with_the_hermetic_policy() {
        let discovery = strings(&discovery_args(&url("https://site.example/w"), None));
        let download = strings(&download_args(
            &url("https://cdn.example/a.m3u8"),
            None,
            Path::new("/tmp/job/out.%(ext)s"),
        ));
        for args in [&discovery, &download] {
            assert_eq!(&args[..YT_DLP_HERMETIC_ARGS.len()], YT_DLP_HERMETIC_ARGS);
            for forbidden in [
                "--cookies",
                "--cookies-from-browser",
                "--exec",
                "--plugin-dirs",
                "--impersonate",
                "--referer",
            ] {
                assert!(
                    !args.iter().any(|arg| arg == forbidden),
                    "{forbidden} in {args:?}"
                );
            }
        }
    }

    #[test]
    fn url_is_the_last_argument_after_a_separator() {
        let target = url("https://site.example/-watch?q=--exec");
        let args = strings(&discovery_args(&target, Some("https://site.example/")));
        assert_eq!(&args[args.len() - 2..], ["--", target.as_str()]);

        let playlist = url("https://cdn.example/a.m3u8?token=--exec");
        let args = strings(&download_args(&playlist, None, Path::new("/tmp/o.%(ext)s")));
        assert_eq!(&args[args.len() - 2..], ["--", playlist.as_str()]);
    }

    #[test]
    fn referer_is_sent_as_one_header_argument_in_both_stages() {
        let referer = "https://site.example/";
        let discovery = strings(&discovery_args(
            &url("https://cdn.example/a.m3u8"),
            Some(referer),
        ));
        let download = strings(&download_args(
            &url("https://cdn.example/a.m3u8"),
            Some(referer),
            Path::new("/tmp/o.%(ext)s"),
        ));
        for args in [&discovery, &download] {
            let index = args.iter().position(|arg| arg == "--add-headers").unwrap();
            assert_eq!(args[index + 1], "Referer:https://site.example/");
        }
    }

    #[test]
    fn no_referer_header_without_a_referring_page() {
        let args = strings(&discovery_args(&url("https://site.example/w"), None));
        assert!(!args.iter().any(|arg| arg == "--add-headers"));
    }

    #[test]
    fn download_forces_native_hls_and_never_overwrites() {
        let args = strings(&download_args(
            &url("https://cdn.example/a.m3u8"),
            None,
            Path::new("/tmp/o.%(ext)s"),
        ));
        let index = args.iter().position(|arg| arg == "--downloader").unwrap();
        assert_eq!(args[index + 1], "m3u8:native");
        assert!(args.iter().any(|arg| arg == "--no-overwrites"));
        let index = args.iter().position(|arg| arg == "--output").unwrap();
        assert_eq!(args[index + 1], "/tmp/o.%(ext)s");
    }
}
