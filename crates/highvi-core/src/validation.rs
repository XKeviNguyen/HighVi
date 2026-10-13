use crate::PublicError;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tokio::net::lookup_host;
use url::{Host, Url};

const MAX_PAGE_URL_BYTES: usize = 2_048;
const MAX_PLAYLIST_URL_BYTES: usize = 8_192;
const MAX_FILENAME_CHARS: usize = 120;

pub fn validate_page_url(raw: &str) -> Result<Url, PublicError> {
    validate_https_url(raw, MAX_PAGE_URL_BYTES, "page")
}

pub fn validate_playlist_url(raw: &str) -> Result<Url, PublicError> {
    let url = validate_https_url(raw, MAX_PLAYLIST_URL_BYTES, "playlist")?;
    let path = url.path().to_ascii_lowercase();
    if !path.ends_with(".m3u8") {
        return Err(PublicError::new(
            "invalid_playlist_url",
            "The media URL is not an HLS playlist (.m3u8).",
        ));
    }
    Ok(url)
}

/// The page a copied playlist came from. HighVi never requests it; it only
/// derives the Referer header from it.
pub fn validate_referer_page_url(raw: &str) -> Result<Url, PublicError> {
    validate_https_url(raw, MAX_PAGE_URL_BYTES, "referer")
}

/// A short form of `url` that is safe to show: scheme, host, port, and the
/// file extension. Any path segment, including the file name, and the query
/// may carry signed tokens, so they are masked.
pub fn redact_url_for_display(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    let port = url
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let last = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()));
    let path = match last {
        None => "/".to_string(),
        Some(last) => match last.rsplit_once('.') {
            Some((_, extension))
                if (1..=5).contains(&extension.len())
                    && extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) =>
            {
                format!("/…/*.{extension}")
            }
            _ => "/…".to_string(),
        },
    };
    let query = if url.query().is_some() { "?…" } else { "" };
    format!("{}://{host}{port}{path}{query}", url.scheme())
}

fn validate_https_url(raw: &str, max_bytes: usize, kind: &str) -> Result<Url, PublicError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > max_bytes {
        return Err(PublicError::new(
            format!("invalid_{kind}_url"),
            format!("Enter a valid {kind} URL."),
        ));
    }

    let mut url = Url::parse(trimmed).map_err(|_| {
        PublicError::new(
            format!("invalid_{kind}_url"),
            format!("Enter a valid {kind} URL."),
        )
    })?;

    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(PublicError::new(
            format!("invalid_{kind}_url"),
            format!("The {kind} URL must use HTTPS and must not contain credentials."),
        ));
    }

    url.set_fragment(None);
    Ok(url)
}

pub async fn ensure_public_network_target(url: &Url) -> Result<(), PublicError> {
    let host = url
        .host()
        .ok_or_else(|| PublicError::new("unsafe_network_target", "The URL has no valid host."))?;

    if let Host::Domain(domain) = host {
        let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
        if normalized == "localhost"
            || normalized.ends_with(".localhost")
            || normalized.ends_with(".local")
            || normalized.ends_with(".internal")
        {
            return Err(unsafe_target_error());
        }
    }

    let addresses: Vec<IpAddr> = match host {
        Host::Ipv4(address) => vec![IpAddr::V4(address)],
        Host::Ipv6(address) => vec![IpAddr::V6(address)],
        Host::Domain(domain) => {
            let port = url.port_or_known_default().unwrap_or(443);
            lookup_host((domain, port))
                .await
                .map_err(|_| {
                    PublicError::new(
                        "network_lookup_failed",
                        "The URL host could not be resolved.",
                    )
                })?
                .map(|address| address.ip())
                .collect()
        }
    };

    if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(*address)) {
        return Err(unsafe_target_error());
    }
    Ok(())
}

fn unsafe_target_error() -> PublicError {
    PublicError::new(
        "unsafe_network_target",
        "Local, private, and reserved network addresses are not allowed.",
    )
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, d] = address.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || (224..=239).contains(&a)
        || a >= 240
        || [a, b, c, d] == [255, 255, 255, 255])
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_ipv4(mapped);
    }

    let segments = address.segments();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

pub fn validate_output_filename(raw: &str) -> Result<String, PublicError> {
    let trimmed = raw.trim();
    let stem = trimmed
        .strip_suffix(".mp4")
        .or_else(|| trimmed.strip_suffix(".MP4"))
        .unwrap_or(trimmed)
        .trim();

    let invalid = stem.is_empty()
        || stem == "."
        || stem == ".."
        || stem.starts_with('.')
        || stem.chars().count() > MAX_FILENAME_CHARS
        || stem.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '%' | '$'
                )
        });

    if invalid {
        return Err(PublicError::new(
            "invalid_filename",
            "Choose a filename of 1–120 characters without path separators or reserved characters.",
        ));
    }

    Ok(format!("{stem}.mp4"))
}

pub fn suggested_filename(title: Option<&str>, id: Option<&str>) -> String {
    let source = title
        .filter(|value| !value.trim().is_empty())
        .or(id)
        .unwrap_or("video");
    let mut stem = String::new();
    let mut previous_was_space = false;

    for character in source.trim().chars() {
        let mapped = if character.is_control()
            || matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '%' | '$'
            ) {
            '_'
        } else {
            character
        };

        if mapped.is_whitespace() {
            if !previous_was_space && !stem.is_empty() {
                stem.push(' ');
            }
            previous_was_space = true;
        } else {
            stem.push(mapped);
            previous_was_space = false;
        }

        if stem.chars().count() >= 80 {
            break;
        }
    }

    let stem = stem.trim_matches([' ', '.']).trim();
    if stem.is_empty() {
        "video.mp4".to_string()
    } else {
        format!("{stem}.mp4")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_url_requires_https_and_rejects_credentials() {
        assert_eq!(
            validate_page_url("http://example.com/video")
                .unwrap_err()
                .code,
            "invalid_page_url"
        );
        assert_eq!(
            validate_page_url("https://user:pass@example.com/video")
                .unwrap_err()
                .code,
            "invalid_page_url"
        );
        assert!(validate_page_url("https://example.com/video?q=1#player").is_ok());
    }

    #[tokio::test]
    async fn private_literal_addresses_are_rejected_without_network_access() {
        for raw in [
            "https://127.0.0.1/video",
            "https://10.0.0.8/video",
            "https://169.254.169.254/latest/meta-data",
            "https://[::1]/video",
            "https://[fd00::1]/video",
        ] {
            let url = validate_page_url(raw).unwrap();
            assert_eq!(
                ensure_public_network_target(&url).await.unwrap_err().code,
                "unsafe_network_target"
            );
        }
    }

    #[test]
    fn display_redaction_hides_a_signed_final_path_segment() {
        for raw in [
            "https://cdn.example/sig-secret.m3u8",
            "https://cdn.example/a/b/sig-secret.m3u8?x=1",
        ] {
            let shown = redact_url_for_display(&Url::parse(raw).unwrap());
            assert!(!shown.contains("secret"), "{shown}");
            assert!(shown.starts_with("https://cdn.example/"), "{shown}");
            assert!(shown.contains(".m3u8"), "{shown}");
        }
    }

    #[test]
    fn display_redaction_hides_path_tokens_and_query() {
        for (raw, expected) in [
            ("https://site.example/", "https://site.example/"),
            (
                "https://cdn.example/master.m3u8",
                "https://cdn.example/…/*.m3u8",
            ),
            (
                "https://cdn.example/secret-token/abc/playlist.m3u8?sig=secret",
                "https://cdn.example/…/*.m3u8?…",
            ),
            (
                "https://cdn.example:8443/a/b.m3u8",
                "https://cdn.example:8443/…/*.m3u8",
            ),
            ("https://cdn.example/a/secret", "https://cdn.example/…"),
            ("https://cdn.example/a/x.sec%2Fret", "https://cdn.example/…"),
        ] {
            assert_eq!(redact_url_for_display(&Url::parse(raw).unwrap()), expected);
        }
    }

    #[test]
    fn filename_cannot_escape_the_download_directory() {
        for raw in [
            "../secret",
            "folder/video",
            "folder\\video",
            ".hidden",
            "bad\0name",
            "%(id)s",
            "$HOME",
        ] {
            assert_eq!(
                validate_output_filename(raw).unwrap_err().code,
                "invalid_filename"
            );
        }
        assert_eq!(
            validate_output_filename("  My clip.mp4  ").unwrap(),
            "My clip.mp4"
        );
    }

    #[test]
    fn suggested_filename_is_portable_and_bounded() {
        assert_eq!(
            suggested_filename(Some(" A/B: 100% $HOME? "), None),
            "A_B_ 100_ _HOME_.mp4"
        );
        let long = "a".repeat(200);
        assert_eq!(suggested_filename(Some(&long), None).chars().count(), 84);
    }

    #[test]
    fn every_suggested_filename_round_trips_through_validation() {
        for title in [
            "Normal title",
            "100% Legal",
            "$HOME video",
            "../path\\attempt",
            "emoji 🎬 title",
            "\0\n",
        ] {
            let suggestion = suggested_filename(Some(title), Some("fallback"));
            assert_eq!(validate_output_filename(&suggestion).unwrap(), suggestion);
        }
    }
}
