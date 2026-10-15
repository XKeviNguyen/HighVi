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
    /// yt-dlp's extractor, e.g. `Youtube` or `Generic`.
    pub extractor: Option<String>,
    /// What was offered and chosen, in fixed vocabulary, for diagnostics.
    pub summary: String,
    /// Expected size of the chosen tracks, when yt-dlp knows or can tell
    /// from bitrate and duration.
    pub estimated_bytes: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    title: Option<String>,
    id: Option<String>,
    has_drm: Option<bool>,
    extractor_key: Option<String>,
    duration: Option<f64>,
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
    filesize: Option<f64>,
    filesize_approx: Option<f64>,
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
        && !is_selector_keyword(id)
}

/// Words yt-dlp's `--format` treats as selectors before it looks for a
/// matching ID: `all`, `mergeall`, and `(best|worst|b|w)(video|audio|v|a)?`
/// with an optional `*` or `.N` (YoutubeDL.py, `_build_selector_function`).
fn is_selector_keyword(id: &str) -> bool {
    let lower = id.to_ascii_lowercase();
    if matches!(lower.as_str(), "all" | "mergeall") {
        return true;
    }
    let base = match lower.split_once('.') {
        Some((base, index)) if !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()) => {
            base
        }
        Some(_) => return false,
        None => lower.as_str(),
    };
    let rest = ["best", "worst", "b", "w"]
        .into_iter()
        .find_map(|prefix| base.strip_prefix(prefix));
    matches!(rest, Some("" | "video" | "audio" | "v" | "a"))
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
    let summary = summarize(&metadata.formats, &selection);
    let estimated_bytes = estimate(&metadata.formats, &selection, metadata.duration);
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
    keep_only_selected(&mut entry, &selection);
    sanitize_http_headers(&mut entry);
    let info_json = serde_json::to_vec(&entry).map_err(|_| unreadable())?;
    Ok(MediaInfo {
        title,
        suggested_filename: suggested_filename(metadata.title.as_deref(), metadata.id.as_deref()),
        selection,
        info_json,
        extractor: metadata.extractor_key,
        summary,
        estimated_bytes,
    })
}

/// Larger claims are treated as unknown rather than trusted.
const MAX_ESTIMATE_BYTES: f64 = 16.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0;

/// Sum of the chosen formats' sizes; `None` when any is unknown. Sizes come
/// from untrusted JSON: non-finite, negative or absurd values count as
/// unknown.
fn estimate(formats: &[Format], selection: &Selection, duration: Option<f64>) -> Option<u64> {
    let valid = |value: Option<f64>| {
        value.filter(|value| value.is_finite() && *value > 0.0 && *value < MAX_ESTIMATE_BYTES)
    };
    let duration = valid(duration);
    let size = |id: &str| -> Option<f64> {
        let format = formats
            .iter()
            .find(|format| format.format_id.as_deref() == Some(id))?;
        valid(format.filesize)
            .or(valid(format.filesize_approx))
            // tbr is kbit/s.
            .or_else(|| valid(Some(valid(format.tbr)? * duration? * 125.0)))
    };
    let mut total = size(&selection.video)?;
    if let Some(audio) = &selection.audio {
        total += size(audio)?;
    }
    valid(Some(total)).map(|total| total as u64)
}

/// `"33 offered, 15 usable; video https mp4 2160p + audio https m4a"`.
/// Protocol and extension come from untrusted JSON, so they are reduced to
/// short identifiers.
fn summarize(formats: &[Format], selection: &Selection) -> String {
    let word = |value: Option<&str>| -> String {
        let word: String = value
            .unwrap_or("?")
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || *character == '_')
            .take(16)
            .collect();
        if word.is_empty() {
            "?".to_string()
        } else {
            word
        }
    };
    let describe = |id: &str| {
        formats
            .iter()
            .find(|format| format.format_id.as_deref() == Some(id))
            .map(|format| {
                let height = format
                    .height
                    .map(|height| format!(" {height}p"))
                    .unwrap_or_default();
                format!(
                    "{} {}{height}",
                    word(format.protocol.as_deref()),
                    word(format.ext.as_deref())
                )
            })
            .unwrap_or_default()
    };
    let usable = formats
        .iter()
        .filter(|format| format.is_supported())
        .count();
    let mut text = format!(
        "{} offered, {usable} usable; video {}",
        formats.len(),
        describe(&selection.video)
    );
    if let Some(audio) = &selection.audio {
        text.push_str(&format!(" + audio {}", describe(audio)));
    }
    text
}

/// Leaves only the chosen formats in the info JSON handed back to yt-dlp,
/// so no selector, however yt-dlp interprets it, can reach a format HighVi
/// rejected. Top-level media fields are removed for the same reason, and so
/// is `webpage_url`: when a download fails, `--load-info-json` re-extracts
/// that URL and downloads whatever it yields, skipping HighVi's checks.
fn keep_only_selected(entry: &mut Value, selection: &Selection) {
    let chosen = |format: &Value| {
        let id = format.get("format_id").and_then(Value::as_str);
        id == Some(selection.video.as_str()) || id.is_some() && id == selection.audio.as_deref()
    };
    if let Some(Value::Array(formats)) = entry.get_mut("formats") {
        formats.retain(chosen);
    }
    if let Value::Object(map) = entry {
        for key in [
            "url",
            "manifest_url",
            "protocol",
            "format_id",
            "requested_formats",
            "requested_downloads",
            "entries",
            "webpage_url",
            "original_url",
        ] {
            map.remove(key);
        }
    }
}

/// yt-dlp replays recorded `http_headers` on download. Two kinds are unsafe:
/// - `Ytdl-*`/`Youtubedl-*` headers are instructions to yt-dlp itself (for
///   example `Ytdl-Request-Proxy` replaces `--proxy`), as are proxy and
///   hop-by-hop headers; they are dropped, as is any value with CR or LF.
/// - Extractors may record the full page URL as `Referer`; it is reduced to
///   the origin (as browsers do cross-origin) or dropped when not HTTPS, so
///   page paths and tokens never reach media hosts.
fn sanitize_http_headers(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::Object(headers)) = map.get_mut("http_headers") {
                headers.retain(|name, value| {
                    let lower = name.to_ascii_lowercase();
                    let control = lower.starts_with("ytdl-")
                        || lower.starts_with("youtubedl-")
                        || lower.starts_with("proxy-")
                        || matches!(
                            lower.as_str(),
                            "host" | "connection" | "transfer-encoding" | "content-length" | "te"
                        );
                    let clean = value
                        .as_str()
                        .is_some_and(|text| !text.contains(['\r', '\n', '\0']));
                    !control && clean && !name.contains(['\r', '\n', ':'])
                });
                let referers: Vec<String> = headers
                    .keys()
                    .filter(|key| key.eq_ignore_ascii_case("referer"))
                    .cloned()
                    .collect();
                for key in referers {
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
            map.values_mut().for_each(sanitize_http_headers);
        }
        Value::Array(items) => items.iter_mut().for_each(sanitize_http_headers),
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
    // The chosen formats are later picked out of the info JSON by ID, so an
    // ID shared with another (possibly rejected) format is never eligible.
    let mut id_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for format in formats {
        *id_counts
            .entry(format.format_id.as_deref().unwrap_or_default())
            .or_default() += 1;
    }
    let supported: Vec<&Format> = formats
        .iter()
        .filter(|format| format.is_supported())
        .filter(|format| id_counts.get(format.format_id.as_deref().unwrap_or_default()) == Some(&1))
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
        assert_eq!(
            media.summary,
            "3 offered, 2 usable; video m3u8_native ? 1080p"
        );
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
              "formats":[{"format_id":"h","protocol":"m3u8_native","url":"https://c.example/p.m3u8","height":720,
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
    fn info_json_holds_only_the_chosen_formats_and_no_control_headers() {
        let media = select_media(
            br#"{"url":"mms://192.168.1.1/x","webpage_url":"https://site.example/w","http_headers":{"Ytdl-Request-Proxy":"__noproxy__","YouTubeDL-No-Compression":"1","X-Ok":"1"},
              "formats":[
                {"format_id":"all","protocol":"https","url":"https://c.example/v.mp4","ext":"mp4","height":2160},
                {"format_id":"lan","protocol":"mms","url":"mms://192.168.1.1:8080/admin/reboot","height":4320},
                {"format_id":"hls-720","protocol":"m3u8_native","url":"https://c.example/p.m3u8","height":720,
                 "http_headers":{"ytdl-socks-proxy":"socks5://10.0.0.1","Proxy-Authorization":"x","Bad":"a\r\nX-Injected: 1","User-Agent":"UA"}}
              ]}"#,
        )
        .unwrap();
        assert_eq!(media.selection.video, "hls-720");
        let entry: Value = serde_json::from_slice(&media.info_json).unwrap();
        let ids: Vec<&str> = entry["formats"]
            .as_array()
            .unwrap()
            .iter()
            .map(|format| format["format_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["hls-720"]);
        assert!(entry.get("url").is_none());
        assert!(entry.get("webpage_url").is_none());
        let text = String::from_utf8(media.info_json)
            .unwrap()
            .to_ascii_lowercase();
        for leaked in [
            "ytdl-",
            "youtubedl-",
            "proxy-authorization",
            "x-injected",
            "mms://",
            "192.168",
        ] {
            assert!(!text.contains(leaked), "{leaked} survived");
        }
        assert_eq!(entry["formats"][0]["http_headers"]["User-Agent"], "UA");
        assert_eq!(entry["http_headers"]["X-Ok"], "1");
    }

    #[test]
    fn formats_sharing_an_id_are_never_chosen() {
        let media = select_media(
            br#"{"formats":[
              {"format_id":"x-0","protocol":"mms","url":"mms://192.168.1.1/a","height":4320},
              {"format_id":"x-0","protocol":"https","url":"https://c.example/v.mp4","ext":"mp4","height":2160},
              {"format_id":"y","protocol":"https","url":"https://c.example/y.mp4","ext":"mp4","height":360}
            ]}"#,
        )
        .unwrap();
        assert_eq!(media.selection.video, "y");
        let text = String::from_utf8(media.info_json).unwrap();
        assert!(!text.contains("mms://") && !text.contains("x-0"), "{text}");
    }

    #[test]
    fn sizes_are_estimated_from_untrusted_fields_conservatively() {
        let estimated = |raw: &str| select_media(raw.as_bytes()).unwrap().estimated_bytes;
        // Known sizes are added up for video + audio.
        assert_eq!(
            estimated(
                r#"{"formats":[
                  {"format_id":"v","protocol":"https","url":"https://c.example/v.mp4","ext":"mp4","vcodec":"avc1","acodec":"none","height":720,"filesize":1000},
                  {"format_id":"a","protocol":"https","url":"https://c.example/a.m4a","ext":"m4a","vcodec":"none","acodec":"mp4a","filesize_approx":200}
                ]}"#
            ),
            Some(1_200)
        );
        // HLS: bitrate (kbit/s) × duration.
        assert_eq!(
            estimated(
                r#"{"duration":10,"formats":[{"format_id":"h","protocol":"m3u8_native","url":"https://c.example/p.m3u8","tbr":800}]}"#
            ),
            Some(1_000_000)
        );
        // Unknown, invalid or absurd values give no estimate.
        for raw in [
            r#"{"formats":[{"format_id":"h","protocol":"m3u8_native","url":"https://c.example/p.m3u8"}]}"#,
            r#"{"duration":-5,"formats":[{"format_id":"h","protocol":"m3u8_native","url":"https://c.example/p.m3u8","tbr":800}]}"#,
            r#"{"formats":[{"format_id":"h","protocol":"m3u8_native","url":"https://c.example/p.m3u8","filesize":1e300}]}"#,
        ] {
            assert_eq!(estimated(raw), None, "{raw}");
        }
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
