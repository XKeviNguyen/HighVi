//! Just enough HLS parsing to rank playlists a web page loaded: master or
//! media, length, and whether it signals DRM. yt-dlp still does the real
//! parsing when the chosen playlist is downloaded.

use url::Url;

#[derive(Clone, Debug, PartialEq)]
pub enum PlaylistKind {
    /// Variant playlist URIs, resolved against the master's URL.
    Master {
        variants: Vec<Url>,
    },
    Media {
        duration: f64,
        ended: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaylistInfo {
    pub kind: PlaylistKind,
    /// Sample-AES/CENC or a non-identity key format (FairPlay, Widevine,
    /// PlayReady…). Plain AES-128 is ordinary encryption, not DRM.
    pub drm: bool,
}

/// `None` when `text` is not an HLS playlist.
pub fn inspect_playlist(text: &str, base: &Url) -> Option<PlaylistInfo> {
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if !text.starts_with("#EXTM3U") {
        return None;
    }
    let mut variants = Vec::new();
    let mut is_master = false;
    let mut expect_variant_uri = false;
    let mut duration = 0.0_f64;
    let mut ended = false;
    let mut drm = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            continue;
        }
        if let Some(tag) = line.strip_prefix('#') {
            if tag.starts_with("EXT-X-STREAM-INF:") {
                is_master = true;
                expect_variant_uri = true;
            } else if let Some(value) = tag.strip_prefix("EXTINF:") {
                let seconds = value.split(',').next().unwrap_or_default().trim();
                if let Ok(seconds) = seconds.parse::<f64>()
                    && seconds.is_finite()
                    && seconds > 0.0
                {
                    duration += seconds;
                }
            } else if tag == "EXT-X-ENDLIST" {
                ended = true;
            } else if let Some(attributes) = tag
                .strip_prefix("EXT-X-KEY:")
                .or_else(|| tag.strip_prefix("EXT-X-SESSION-KEY:"))
            {
                drm |= is_drm_key(attributes);
            }
            continue;
        }
        if expect_variant_uri {
            if let Ok(url) = base.join(line) {
                variants.push(url);
            }
            expect_variant_uri = false;
        }
    }
    let kind = if is_master {
        PlaylistKind::Master { variants }
    } else {
        PlaylistKind::Media { duration, ended }
    };
    Some(PlaylistInfo { kind, drm })
}

fn is_drm_key(attributes: &str) -> bool {
    let method = attribute(attributes, "METHOD").unwrap_or_default();
    let format = attribute(attributes, "KEYFORMAT").unwrap_or("identity");
    let method_is_drm = !matches!(method.to_ascii_uppercase().as_str(), "NONE" | "AES-128");
    method_is_drm || !format.eq_ignore_ascii_case("identity")
}

/// The value of `name` in an attribute list, unquoted. Quoted values may
/// contain commas, so the list is scanned rather than split.
fn attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attributes;
    while !rest.is_empty() {
        let (key, after_key) = rest.split_once('=')?;
        let (value, after_value) = if let Some(quoted) = after_key.strip_prefix('"') {
            let end = quoted.find('"')?;
            (&quoted[..end], quoted[end + 1..].trim_start_matches(','))
        } else {
            match after_key.split_once(',') {
                Some((value, after)) => (value, after),
                None => (after_key, ""),
            }
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim());
        }
        rest = after_value;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example/hls/master.m3u8?token=t").unwrap()
    }

    #[test]
    fn recognises_master_playlists_and_resolves_variants() {
        let info = inspect_playlist(
            "\u{feff}#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1,CODECS=\"avc1,mp4a\"\nlow/index.m3u8\n\
             #EXT-X-STREAM-INF:BANDWIDTH=2\nhttps://other.example/hi.m3u8\n",
            &base(),
        )
        .unwrap();
        assert!(!info.drm);
        let PlaylistKind::Master { variants } = info.kind else {
            panic!("not a master")
        };
        assert_eq!(
            variants.iter().map(Url::as_str).collect::<Vec<_>>(),
            [
                "https://cdn.example/hls/low/index.m3u8",
                "https://other.example/hi.m3u8"
            ]
        );
    }

    #[test]
    fn measures_media_playlists() {
        let info = inspect_playlist(
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\na.ts\n#EXTINF:5.5,title\nb.ts\n#EXTINF:bad,\nc.ts\n#EXT-X-ENDLIST\n",
            &base(),
        )
        .unwrap();
        assert_eq!(
            info.kind,
            PlaylistKind::Media {
                duration: 11.5,
                ended: true
            }
        );
    }

    #[test]
    fn distinguishes_drm_from_ordinary_aes() {
        let with_key = |key: &str| {
            inspect_playlist(&format!("#EXTM3U\n{key}\n#EXTINF:1,\na.ts\n"), &base())
                .unwrap()
                .drm
        };
        assert!(!with_key(
            "#EXT-X-KEY:METHOD=AES-128,URI=\"https://k.example/k,1\""
        ));
        assert!(!with_key("#EXT-X-KEY:METHOD=NONE"));
        assert!(!with_key(
            "#EXT-X-KEY:METHOD=AES-128,URI=\"k\",KEYFORMAT=\"identity\""
        ));
        assert!(with_key("#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\""));
        assert!(with_key("#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"data:x\""));
        assert!(with_key(
            "#EXT-X-SESSION-KEY:METHOD=AES-128,URI=\"x\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\""
        ));
        assert!(with_key("#EXT-X-KEY:METHOD=ISO-23001-7,URI=\"x\""));
    }

    #[test]
    fn non_playlists_are_rejected() {
        for text in ["", "<html>", "{\"hls\":1}", "EXTM3U", "#EXTINF:1,\na.ts"] {
            assert!(inspect_playlist(text, &base()).is_none(), "{text}");
        }
    }
}
