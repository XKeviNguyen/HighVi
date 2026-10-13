use crate::{PublicError, validate_page_url, validate_playlist_url, validate_referer_page_url};
use serde::Deserialize;
use url::Url;

/// What the user asked HighVi to fetch. This comes from the webview and is
/// untrusted; always pass it through [`validate_source`] before use.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub enum SourceRequest {
    #[serde(rename_all = "camelCase")]
    Page { page_url: String },
    #[serde(rename_all = "camelCase")]
    DirectHls {
        playlist_url: String,
        referer_page_url: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ValidatedSource {
    /// A page that yt-dlp extracts media from.
    Page { page: Url },
    /// A playlist the user copied, plus the page they found it on.
    DirectHls { playlist: Url, referer_page: Url },
}

/// How a failure should be explained to the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    Page,
    /// A `.m3u8` URL entered in page mode, so HighVi knows no referring page.
    BarePlaylist,
    DirectHls,
}

pub fn validate_source(request: &SourceRequest) -> Result<ValidatedSource, PublicError> {
    match request {
        SourceRequest::Page { page_url } => Ok(ValidatedSource::Page {
            page: validate_page_url(page_url)?,
        }),
        SourceRequest::DirectHls {
            playlist_url,
            referer_page_url,
        } => {
            let playlist = validate_playlist_url(playlist_url)?;
            let referer_page = validate_referer_page_url(referer_page_url)?;
            if referer_page.path().to_ascii_lowercase().ends_with(".m3u8") {
                return Err(PublicError::new(
                    "invalid_referer_url",
                    "Enter the web page where you found the playlist, not a playlist URL.",
                ));
            }
            Ok(ValidatedSource::DirectHls {
                playlist,
                referer_page,
            })
        }
    }
}

impl ValidatedSource {
    /// URL handed to yt-dlp during discovery.
    pub fn discovery_target(&self) -> &Url {
        match self {
            Self::Page { page } => page,
            Self::DirectHls { playlist, .. } => playlist,
        }
    }

    /// Referer for the discovery request. A page is a top-level navigation,
    /// so it gets none.
    pub fn discovery_referer(&self) -> Option<String> {
        match self {
            Self::Page { .. } => None,
            Self::DirectHls { referer_page, .. } => Some(referer_origin(referer_page)),
        }
    }

    /// Referer for downloading the HLS media chosen by discovery.
    pub fn download_referer(&self) -> Option<String> {
        match self {
            // A pasted playlist has no known referring page; it must never be
            // sent as its own Referer, even when discovery returns another URL.
            Self::Page { .. } if self.kind() == SourceKind::BarePlaylist => None,
            Self::Page { page } => Some(referer_origin(page)),
            Self::DirectHls { referer_page, .. } => Some(referer_origin(referer_page)),
        }
    }

    pub fn kind(&self) -> SourceKind {
        match self {
            Self::Page { page } if page.path().to_ascii_lowercase().ends_with(".m3u8") => {
                SourceKind::BarePlaylist
            }
            Self::Page { .. } => SourceKind::Page,
            Self::DirectHls { .. } => SourceKind::DirectHls,
        }
    }
}

/// The Referer HighVi sends: only the page origin. yt-dlp applies one header
/// value to every request (playlist, variants, segments, keys), and some of
/// those may go to other hosts, where a browser would also send only the
/// origin under its default `strict-origin-when-cross-origin` policy. Never
/// sending the page path or query keeps page tokens away from those hosts.
pub fn referer_origin(page: &Url) -> String {
    format!("{}/", page.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    fn direct(playlist_url: &str, referer_page_url: &str) -> SourceRequest {
        SourceRequest::DirectHls {
            playlist_url: playlist_url.to_string(),
            referer_page_url: referer_page_url.to_string(),
        }
    }

    #[test]
    fn deserializes_both_modes_from_camel_case_json() {
        let page: SourceRequest =
            serde_json::from_str(r#"{"mode":"page","pageUrl":"https://site.example/watch"}"#)
                .unwrap();
        assert!(matches!(page, SourceRequest::Page { .. }));

        let direct: SourceRequest = serde_json::from_str(
            r#"{"mode":"directHls","playlistUrl":"https://cdn.example/a.m3u8","refererPageUrl":"https://site.example/watch"}"#,
        )
        .unwrap();
        assert!(matches!(direct, SourceRequest::DirectHls { .. }));

        assert!(
            serde_json::from_str::<SourceRequest>(r#"{"mode":"other","pageUrl":"x"}"#).is_err()
        );
        assert!(
            serde_json::from_str::<SourceRequest>(r#"{"mode":"directHls","playlistUrl":"x"}"#)
                .is_err()
        );
    }

    #[test]
    fn referer_is_reduced_to_the_page_origin() {
        for (page, expected) in [
            (
                "https://site.example/ja/watch/123?session=secret#t=10",
                "https://site.example/",
            ),
            (
                "https://site.example:8443/watch",
                "https://site.example:8443/",
            ),
            ("https://site.example:443/watch", "https://site.example/"),
            (
                "https://bücher.example/watch",
                "https://xn--bcher-kva.example/",
            ),
        ] {
            assert_eq!(referer_origin(&url(page)), expected);
        }
    }

    #[test]
    fn direct_hls_sends_the_referring_page_origin_at_discovery_and_download() {
        let source = validate_source(&direct(
            "https://cdn.example/abc/playlist.m3u8",
            "https://site.example/ja/watch/123",
        ))
        .unwrap();
        assert_eq!(
            source.discovery_target().as_str(),
            "https://cdn.example/abc/playlist.m3u8"
        );
        assert_eq!(
            source.discovery_referer().as_deref(),
            Some("https://site.example/")
        );
        assert_eq!(
            source.download_referer().as_deref(),
            Some("https://site.example/")
        );
        assert_eq!(source.kind(), SourceKind::DirectHls);
    }

    #[test]
    fn page_mode_never_uses_the_playlist_as_its_own_referer() {
        let bare = validate_source(&SourceRequest::Page {
            page_url: "https://cdn.example/abc/playlist.m3u8".to_string(),
        })
        .unwrap();
        assert_eq!(bare.kind(), SourceKind::BarePlaylist);
        assert_eq!(bare.discovery_referer(), None);
        assert_eq!(bare.download_referer(), None);
        let signed = validate_source(&SourceRequest::Page {
            page_url: "https://cdn.example/abc/playlist.m3u8?token=T".to_string(),
        })
        .unwrap();
        assert_eq!(signed.download_referer(), None);

        let page = validate_source(&SourceRequest::Page {
            page_url: "https://site.example/watch/1".to_string(),
        })
        .unwrap();
        assert_eq!(page.kind(), SourceKind::Page);
        assert_eq!(page.discovery_referer(), None);
        assert_eq!(
            page.download_referer().as_deref(),
            Some("https://site.example/")
        );
    }

    #[test]
    fn direct_hls_rejects_invalid_or_playlist_referers() {
        for (playlist, referer, code) in [
            (
                "https://cdn.example/a.m3u8",
                "https://cdn.example/a.m3u8",
                "invalid_referer_url",
            ),
            (
                "https://cdn.example/a.m3u8",
                "https://site.example/b.M3U8?x=1",
                "invalid_referer_url",
            ),
            (
                "https://cdn.example/a.m3u8",
                "http://site.example/watch",
                "invalid_referer_url",
            ),
            (
                "https://cdn.example/a.m3u8",
                "https://u:p@site.example/watch",
                "invalid_referer_url",
            ),
            ("https://cdn.example/a.m3u8", "", "invalid_referer_url"),
            (
                "https://cdn.example/video.mp4",
                "https://site.example/watch",
                "invalid_playlist_url",
            ),
            (
                "http://cdn.example/a.m3u8",
                "https://site.example/watch",
                "invalid_playlist_url",
            ),
        ] {
            assert_eq!(
                validate_source(&direct(playlist, referer))
                    .unwrap_err()
                    .code,
                code,
                "{playlist} / {referer}"
            );
        }
    }

    #[test]
    fn referer_value_cannot_carry_header_injection() {
        // WHATWG URL parsing strips raw CR/LF/tab anywhere in the input,
        // including the host, and the Referer keeps only the origin.
        let source = validate_source(&direct(
            "https://cdn.example/a.m3u8",
            "https://site.exa\r\nmple/wa\r\ntch?x=%0d%0aX-Injected:%201\nY: 2",
        ))
        .unwrap();
        for referer in [
            source.discovery_referer().unwrap(),
            source.download_referer().unwrap(),
        ] {
            assert_eq!(referer, "https://site.example/");
        }

        // Percent-encoded control characters are not valid in a host.
        assert_eq!(
            validate_source(&direct(
                "https://cdn.example/a.m3u8",
                "https://site.example%0d%0aX-Injected:%201/watch",
            ))
            .unwrap_err()
            .code,
            "invalid_referer_url"
        );
    }

    #[test]
    fn same_origin_playlist_still_gets_only_the_page_origin() {
        // yt-dlp sends --add-headers on every request, including segments and
        // keys on other hosts, so the page path and query must never be sent.
        let source = validate_source(&direct(
            "https://site.example/media/master.m3u8",
            "https://site.example/watch/1?session=secret",
        ))
        .unwrap();
        for referer in [
            source.discovery_referer().unwrap(),
            source.download_referer().unwrap(),
        ] {
            assert_eq!(referer, "https://site.example/");
        }
    }

    #[test]
    fn signed_playlist_query_is_preserved() {
        let raw =
            "https://cdn.example/a.m3u8?Expires=1700000000&Signature=ab~c%2Bd%3D&Key-Pair-Id=K1";
        let source = validate_source(&direct(raw, "https://site.example/watch")).unwrap();
        assert_eq!(source.discovery_target().as_str(), raw);
    }
}
