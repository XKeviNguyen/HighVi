use crate::{PublicError, suggested_filename, validate_playlist_url};
use serde::Deserialize;

/// Media found by yt-dlp. `playlist_url` may carry tokens; show it to the user
/// only through `redact_url_for_display`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Discovery {
    pub title: String,
    pub playlist_url: String,
    pub suggested_filename: String,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    title: Option<String>,
    id: Option<String>,
    has_drm: Option<bool>,
    protocol: Option<String>,
    url: Option<String>,
    manifest_url: Option<String>,
    #[serde(default)]
    formats: Vec<Format>,
}

#[derive(Debug, Deserialize)]
struct Format {
    protocol: Option<String>,
    url: Option<String>,
    manifest_url: Option<String>,
    has_drm: Option<bool>,
    height: Option<u32>,
}

struct Candidate<'a> {
    url: &'a str,
    score: (bool, u32),
}

pub fn extract_hls_from_json(raw_json: &[u8]) -> Result<Discovery, PublicError> {
    let metadata: Metadata = serde_json::from_slice(raw_json).map_err(|_| {
        PublicError::new(
            "invalid_extractor_output",
            "The media extractor returned an unreadable response.",
        )
    })?;

    if metadata.has_drm == Some(true) {
        return Err(drm_error());
    }

    let mut saw_drm_hls = false;
    let mut candidates = Vec::new();
    for format in &metadata.formats {
        if !is_hls(
            format.protocol.as_deref(),
            format.manifest_url.as_deref(),
            format.url.as_deref(),
        ) {
            continue;
        }
        if format.has_drm == Some(true) {
            saw_drm_hls = true;
            continue;
        }

        if let Some(url) = format.manifest_url.as_deref().or(format.url.as_deref()) {
            candidates.push(Candidate {
                url,
                score: (format.manifest_url.is_some(), format.height.unwrap_or(0)),
            });
        }
    }

    if candidates.is_empty()
        && is_hls(
            metadata.protocol.as_deref(),
            metadata.manifest_url.as_deref(),
            metadata.url.as_deref(),
        )
        && let Some(url) = metadata.manifest_url.as_deref().or(metadata.url.as_deref())
    {
        candidates.push(Candidate {
            url,
            score: (metadata.manifest_url.is_some(), 0),
        });
    }

    candidates.sort_by_key(|candidate| candidate.score);
    let selected = candidates.pop().ok_or_else(|| {
        if saw_drm_hls {
            drm_error()
        } else {
            PublicError::new(
                "hls_not_found",
                "No accessible HLS playlist was found on this page.",
            )
        }
    })?;

    let playlist = validate_playlist_url(selected.url)?;
    let title = metadata
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or(metadata.id.as_deref())
        .unwrap_or("Video")
        .trim()
        .to_string();

    Ok(Discovery {
        suggested_filename: suggested_filename(metadata.title.as_deref(), metadata.id.as_deref()),
        title,
        playlist_url: playlist.to_string(),
    })
}

fn is_hls(protocol: Option<&str>, manifest_url: Option<&str>, url: Option<&str>) -> bool {
    protocol.is_some_and(|value| value.to_ascii_lowercase().contains("m3u8"))
        || manifest_url.is_some_and(has_m3u8_path)
        || url.is_some_and(has_m3u8_path)
}

fn has_m3u8_path(raw: &str) -> bool {
    url::Url::parse(raw)
        .map(|url| url.path().to_ascii_lowercase().ends_with(".m3u8"))
        .unwrap_or(false)
}

fn drm_error() -> PublicError {
    PublicError::new(
        "drm_not_supported",
        "This media reports DRM protection. HighVi will not bypass DRM or access controls.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooses_master_manifest_from_clear_hls_formats() {
        let raw = br#"{
          "id": "abc",
          "title": "Sample / title",
          "formats": [
            {"protocol":"https", "url":"https://cdn.example/video.mp4", "height":1080},
            {"protocol":"m3u8_native", "url":"https://cdn.example/720.m3u8?token=x", "manifest_url":"https://cdn.example/master.m3u8?token=x", "height":720},
            {"protocol":"m3u8_native", "url":"https://cdn.example/1080.m3u8?token=x", "manifest_url":"https://cdn.example/master.m3u8?token=x", "height":1080}
          ]
        }"#;

        let result = extract_hls_from_json(raw).unwrap();
        assert_eq!(
            result.playlist_url,
            "https://cdn.example/master.m3u8?token=x"
        );
        assert_eq!(result.suggested_filename, "Sample _ title.mp4");
    }

    #[test]
    fn rejects_drm_only_hls() {
        let raw = br#"{
          "title": "Protected",
          "formats": [
            {"protocol":"m3u8_native", "url":"https://cdn.example/protected.m3u8", "has_drm":true}
          ]
        }"#;
        assert_eq!(
            extract_hls_from_json(raw).unwrap_err().code,
            "drm_not_supported"
        );
    }

    #[test]
    fn rejects_non_hls_and_malformed_output() {
        let non_hls = br#"{"title":"Direct", "formats":[{"protocol":"https", "url":"https://cdn.example/video.mp4"}]}"#;
        assert_eq!(
            extract_hls_from_json(non_hls).unwrap_err().code,
            "hls_not_found"
        );
        assert_eq!(
            extract_hls_from_json(b"not-json").unwrap_err().code,
            "invalid_extractor_output"
        );
    }

    #[test]
    fn rejects_insecure_extracted_playlist() {
        let raw =
            br#"{"formats":[{"protocol":"m3u8_native", "url":"http://cdn.example/video.m3u8"}]}"#;
        assert_eq!(
            extract_hls_from_json(raw).unwrap_err().code,
            "invalid_playlist_url"
        );
    }
}
