//! Chooses what to download from yt-dlp's `--dump-single-json` output.
//!
//! The JSON describes remote media and is untrusted. HighVi picks the formats
//! itself and hands yt-dlp only their IDs, after checking that the IDs cannot
//! act as yt-dlp format-selector syntax.

use crate::{PublicError, suggested_filename};
use serde::Deserialize;
use serde_json::Value;

const MAX_FORMAT_ID_CHARS: usize = 128;
const MAX_TITLE_CHARS: usize = 200;

/// One video (and, when it has no audio, one audio track) to download.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Selection {
    pub video: String,
    pub audio: Option<String>,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub title: String,
    pub suggested_filename: String,
    pub selection: Selection,
    /// The single-video info JSON handed back to yt-dlp via
    /// `--load-info-json`, so the download uses exactly the formats chosen.
    pub info_json: Vec<u8>,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    title: Option<String>,
    id: Option<String>,
    has_drm: Option<bool>,
    is_live: Option<bool>,
    live_status: Option<String>,
    #[serde(default)]
    formats: Vec<Format>,
}

#[derive(Debug, Deserialize)]
struct Format {
    format_id: Option<String>,
    protocol: Option<String>,
    url: Option<String>,
    ext: Option<String>,
    vcodec: Option<String>,
    acodec: Option<String>,
    height: Option<u32>,
    tbr: Option<f64>,
    abr: Option<f64>,
    has_drm: Option<bool>,
}

impl Format {
    fn is_drm(&self) -> bool {
        self.has_drm == Some(true)
    }

    fn is_supported(&self) -> bool {
        let https = self
            .url
            .as_deref()
            .is_some_and(|url| url.starts_with("https://"));
        let protocol = match self.protocol.as_deref() {
            Some("m3u8" | "m3u8_native") => true,
            Some("https") => matches!(self.ext.as_deref(), Some("mp4" | "m4v" | "m4a")),
            _ => false,
        };
        https && protocol && self.format_id.as_deref().is_some_and(is_safe_format_id)
    }

    /// `None` means yt-dlp does not know; HLS variants without a CODECS
    /// attribute look like this, and they usually carry both tracks.
    fn has_video(&self) -> bool {
        self.vcodec.as_deref() != Some("none")
    }

    fn has_audio(&self) -> bool {
        self.acodec.as_deref() != Some("none")
    }

    fn video_rank(&self) -> (u32, u64, bool) {
        (
            self.height.unwrap_or(0),
            rate(self.tbr),
            self.acodec.as_deref().is_some_and(|codec| codec != "none"),
        )
    }

    fn audio_rank(&self) -> u64 {
        rate(self.abr.or(self.tbr))
    }

    fn id(&self) -> String {
        self.format_id.clone().unwrap_or_default()
    }
}

fn rate(value: Option<f64>) -> u64 {
    value
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| (value * 1000.0) as u64)
        .unwrap_or(0)
}

/// yt-dlp already replaces whitespace and `,/+[]()` in IDs; this also keeps
/// out every other ASCII symbol its selector language gives meaning to.
fn is_safe_format_id(id: &str) -> bool {
    let count = id.chars().count();
    (1..=MAX_FORMAT_ID_CHARS).contains(&count)
        && id
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '-' | '_' | '.'))
}

pub fn select_media(raw_json: &[u8]) -> Result<MediaInfo, PublicError> {
    let value: Value = serde_json::from_slice(raw_json).map_err(|_| unreadable())?;
    let page_title = (value.get("_type").and_then(Value::as_str) == Some("playlist"))
        .then(|| {
            value
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .flatten();
    let entry = pick_entry(value)?;
    let mut metadata: Metadata = serde_json::from_value(entry.clone()).map_err(|_| unreadable())?;
    // yt-dlp names the media of a page "<page title> (1)"; the page title
    // alone is the better file name.
    if let (Some(page_title), Some(title)) = (page_title, metadata.title.as_deref())
        && !page_title.trim().is_empty()
        && title.starts_with(page_title.as_str())
    {
        metadata.title = Some(page_title);
    }

    if metadata.has_drm == Some(true) {
        return Err(drm_error());
    }
    if metadata.is_live == Some(true)
        || matches!(
            metadata.live_status.as_deref(),
            Some("is_live" | "is_upcoming" | "post_live")
        )
    {
        return Err(PublicError::new(
            "live_not_supported",
            "This is a live stream. HighVi downloads finished videos only.",
        ));
    }

    let selection = choose_formats(&metadata.formats)?;
    let title = metadata
        .title
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or(metadata.id.as_deref())
        .unwrap_or("Video")
        .trim()
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect();
    let mut entry = entry;
    origin_only_referers(&mut entry);
    let info_json = serde_json::to_vec(&entry).map_err(|_| unreadable())?;
    Ok(MediaInfo {
        title,
        suggested_filename: suggested_filename(metadata.title.as_deref(), metadata.id.as_deref()),
        selection,
        info_json,
    })
}

/// Extractors may record the full page URL as a format's `Referer`, and
/// yt-dlp replays those headers on download. Reduce every Referer to its
/// origin (as browsers do cross-origin), or drop it when it is not HTTPS, so
/// page paths and tokens never reach media hosts.
fn origin_only_referers(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Object(headers)) = map.get_mut("http_headers") {
                let keys: Vec<String> = headers
                    .keys()
                    .filter(|key| key.eq_ignore_ascii_case("referer"))
                    .cloned()
                    .collect();
                for key in keys {
                    let origin = headers
                        .get(&key)
                        .and_then(Value::as_str)
                        .and_then(|raw| url::Url::parse(raw).ok())
                        .filter(|url| url.scheme() == "https" && url.host().is_some())
                        .map(|url| crate::referer_origin(&url));
                    match origin {
                        Some(origin) => {
                            headers.insert(key, Value::String(origin));
                        }
                        None => {
                            headers.remove(&key);
                        }
                    }
                }
            }
            map.values_mut().for_each(origin_only_referers);
        }
        Value::Array(items) => items.iter_mut().for_each(origin_only_referers),
        _ => {}
    }
}

/// A page may yield a playlist of embeds; keep the longest entry that has
/// formats, which is the main video rather than a short teaser or advert.
fn pick_entry(value: Value) -> Result<Value, PublicError> {
    if value.get("_type").and_then(Value::as_str) != Some("playlist") {
        return Ok(value);
    }
    let Some(Value::Array(entries)) = value.get("entries") else {
        return Err(not_found());
    };
    entries
        .iter()
        .filter(|entry| {
            entry
                .get("formats")
                .and_then(Value::as_array)
                .is_some_and(|formats| !formats.is_empty())
        })
        .max_by(|left, right| {
            let duration = |entry: &Value| {
                entry
                    .get("duration")
                    .and_then(Value::as_f64)
                    .filter(|value| value.is_finite())
                    .unwrap_or(0.0)
            };
            duration(left).total_cmp(&duration(right))
        })
        .cloned()
        .ok_or_else(not_found)
}

fn choose_formats(formats: &[Format]) -> Result<Selection, PublicError> {
    let supported: Vec<&Format> = formats
        .iter()
        .filter(|format| format.is_supported())
        .collect();
    let usable: Vec<&Format> = supported.iter().copied().filter(|f| !f.is_drm()).collect();

    let Some(video) = usable
        .iter()
        .copied()
        .filter(|format| format.has_video())
        .max_by_key(|format| format.video_rank())
    else {
        return Err(if supported.iter().any(|format| format.is_drm()) {
            drm_error()
        } else {
            not_found()
        });
    };

    if video.has_audio() {
        return Ok(Selection {
            video: video.id(),
            audio: None,
        });
    }
    // The best video is video-only. Pair it with the best audio-only track;
    // failing that, fall back to the best format that carries audio.
    if let Some(audio) = usable
        .iter()
        .copied()
        .filter(|format| !format.has_video() && format.has_audio())
        .max_by_key(|format| format.audio_rank())
    {
        return Ok(Selection {
            video: video.id(),
            audio: Some(audio.id()),
        });
    }
    let muxed = usable
        .iter()
        .copied()
        .filter(|format| format.has_video() && format.has_audio())
        .max_by_key(|format| format.video_rank());
    Ok(Selection {
        video: muxed.unwrap_or(video).id(),
        audio: None,
    })
}

fn unreadable() -> PublicError {
    PublicError::new(
        "invalid_extractor_output",
        "The media extractor returned an unreadable response.",
    )
}

fn not_found() -> PublicError {
    PublicError::new(
        "hls_not_found",
        "No downloadable HLS or MP4 video was found on this page.",
    )
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

    fn select(raw: &str) -> Result<Selection, PublicError> {
        select_media(raw.as_bytes()).map(|media| media.selection)
    }

    #[test]
    fn picks_the_tallest_muxed_hls_variant() {
        let media = select_media(
            br#"{"id":"abc","title":"Sample / title","formats":[
              {"format_id":"mp4-1080","protocol":"http","url":"http://cdn.example/v.mp4","ext":"mp4","height":1080},
              {"format_id":"hls-720","protocol":"m3u8_native","url":"https://cdn.example/720.m3u8","height":720,"tbr":2000},
              {"format_id":"hls-1080","protocol":"m3u8_native","url":"https://cdn.example/1080.m3u8","height":1080,"tbr":5000}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            media.selection,
            Selection {
                video: "hls-1080".into(),
                audio: None
            }
        );
        assert_eq!(media.title, "Sample / title");
        assert_eq!(media.suggested_filename, "Sample _ title.mp4");
    }

    #[test]
    fn pairs_video_only_with_the_best_audio_rendition() {
        let selection = select(
            r#"{"formats":[
              {"format_id":"hls-audio-low","protocol":"m3u8_native","url":"https://c.example/a1.m3u8","vcodec":"none","acodec":"mp4a.40.2","abr":64},
              {"format_id":"hls-audio-日本語","protocol":"m3u8_native","url":"https://c.example/a2.m3u8","vcodec":"none","acodec":"mp4a.40.2","abr":128},
              {"format_id":"hls-1080","protocol":"m3u8_native","url":"https://c.example/v.m3u8","vcodec":"avc1","acodec":"none","height":1080},
              {"format_id":"hls-360","protocol":"m3u8_native","url":"https://c.example/m.m3u8","vcodec":"avc1","acodec":"mp4a","height":360}
            ]}"#,
        )
        .unwrap();
        assert_eq!(selection.video, "hls-1080");
        assert_eq!(selection.audio.as_deref(), Some("hls-audio-日本語"));
    }

    #[test]
    fn video_only_without_audio_tracks_falls_back_to_a_muxed_format() {
        let selection = select(
            r#"{"formats":[
              {"format_id":"v","protocol":"m3u8_native","url":"https://c.example/v.m3u8","acodec":"none","height":1080},
              {"format_id":"m","protocol":"m3u8_native","url":"https://c.example/m.m3u8","acodec":"aac","height":480}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            selection,
            Selection {
                video: "m".into(),
                audio: None
            }
        );
    }

    #[test]
    fn rejects_drm_live_and_unsupported_media() {
        let drm_only = r#"{"formats":[{"format_id":"h","protocol":"m3u8_native","url":"https://c.example/p.m3u8","has_drm":true}]}"#;
        assert_eq!(select(drm_only).unwrap_err().code, "drm_not_supported");
        let drm_top = r#"{"has_drm":true,"formats":[]}"#;
        assert_eq!(select(drm_top).unwrap_err().code, "drm_not_supported");
        for live in [
            r#"{"is_live":true,"formats":[]}"#,
            r#"{"live_status":"is_upcoming"}"#,
        ] {
            assert_eq!(select(live).unwrap_err().code, "live_not_supported");
        }
        for unsupported in [
            r#"{"formats":[{"format_id":"d","protocol":"http_dash_segments","url":"https://c.example/m.mpd"}]}"#,
            r#"{"formats":[{"format_id":"h","protocol":"m3u8_native","url":"http://c.example/p.m3u8"}]}"#,
            r#"{"formats":[{"format_id":"w","protocol":"https","url":"https://c.example/v.webm","ext":"webm"}]}"#,
            r#"{"formats":[]}"#,
        ] {
            assert_eq!(
                select(unsupported).unwrap_err().code,
                "hls_not_found",
                "{unsupported}"
            );
        }
        assert_eq!(select("nope").unwrap_err().code, "invalid_extractor_output");
    }

    #[test]
    fn replayed_referers_are_reduced_to_their_origin() {
        let media = select_media(
            br#"{"http_headers":{"Referer":"https://site.example/watch/1?session=s"},
              "formats":[{"format_id":"h","protocol":"m3u8_native","url":"https://c.example/p.m3u8",
                "http_headers":{"referer":"https://site.example/a/b?token=t","User-Agent":"UA"}},
                {"format_id":"x","protocol":"m3u8_native","url":"https://c.example/x.m3u8",
                "http_headers":{"Referer":"http://site.example/plain"}}]}"#,
        )
        .unwrap();
        let entry: Value = serde_json::from_slice(&media.info_json).unwrap();
        assert_eq!(entry["http_headers"]["Referer"], "https://site.example/");
        assert_eq!(
            entry["formats"][0]["http_headers"]["referer"],
            "https://site.example/"
        );
        assert_eq!(entry["formats"][0]["http_headers"]["User-Agent"], "UA");
        assert!(entry["formats"][1]["http_headers"].get("Referer").is_none());
        let text = String::from_utf8(media.info_json).unwrap();
        assert!(!text.contains("session") && !text.contains("token=t"));
    }

    #[test]
    fn progressive_https_mp4_is_accepted() {
        let selection = select(
            r#"{"formats":[{"format_id":"html5-0","protocol":"https","url":"https://c.example/v.mp4","ext":"mp4"}]}"#,
        )
        .unwrap();
        assert_eq!(selection.video, "html5-0");
    }

    #[test]
    fn selector_syntax_in_format_ids_is_never_selected() {
        for id in [
            "a/b",
            "b*",
            "best[height<720]",
            "a,b",
            "a+b",
            "x?",
            "",
            "a b",
            "$x",
        ] {
            assert!(!is_safe_format_id(id), "{id}");
        }
        let raw = format!(
            r#"{{"formats":[{{"format_id":"{}","protocol":"m3u8_native","url":"https://c.example/p.m3u8"}}]}}"#,
            "bv*"
        );
        assert_eq!(select(&raw).unwrap_err().code, "hls_not_found");
    }

    #[test]
    fn playlist_of_embeds_keeps_the_longest_entry_only() {
        let media = select_media(
            br#"{"_type":"playlist","title":"Page","entries":[
              {"id":"ad","duration":15,"formats":[{"format_id":"ad","protocol":"m3u8_native","url":"https://ads.example/a.m3u8"}]},
              {"id":"main","title":"Main","duration":600,"formats":[{"format_id":"main","protocol":"m3u8_native","url":"https://c.example/m.m3u8"}]},
              {"id":"empty","duration":9000,"formats":[]}
            ]}"#,
        )
        .unwrap();
        assert_eq!(media.selection.video, "main");
        assert_eq!(media.title, "Main");
        let named = select_media(
            br#"{"_type":"playlist","title":"Page","entries":[{"title":"Page (1)","formats":[{"format_id":"m","protocol":"m3u8_native","url":"https://c.example/m.m3u8"}]}]}"#,
        )
        .unwrap();
        assert_eq!(named.title, "Page");
        let entry: Value = serde_json::from_slice(&media.info_json).unwrap();
        assert_eq!(entry["id"], "main");
        assert!(entry.get("entries").is_none());
    }
}
