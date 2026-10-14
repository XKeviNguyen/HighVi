use crate::PublicError;
use url::Url;

const MAX_PAGE_URL_BYTES: usize = 2_048;
const MAX_PLAYLIST_URL_BYTES: usize = 8_192;

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

/// Linux allows 255 bytes per name; keep room for " (999).mp4".
const MAX_STEM_BYTES: usize = 240;
const MAX_STEM_CHARS: usize = 80;

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

        let next = if mapped.is_whitespace() { ' ' } else { mapped };
        if stem.chars().count() >= MAX_STEM_CHARS || stem.len() + next.len_utf8() > MAX_STEM_BYTES {
            break;
        }
        if mapped.is_whitespace() {
            if !previous_was_space && !stem.is_empty() {
                stem.push(' ');
            }
            previous_was_space = true;
        } else {
            stem.push(mapped);
            previous_was_space = false;
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

    #[test]
    fn suggested_filename_is_portable_and_bounded() {
        assert_eq!(
            suggested_filename(Some(" A/B: 100% $HOME? "), None),
            "A_B_ 100_ _HOME_.mp4"
        );
        for hostile in [
            "../../etc/passwd",
            ".hidden",
            "%(id)s",
            "a\0b\nc",
            "..",
            "   ",
        ] {
            let name = suggested_filename(Some(hostile), None);
            assert!(!name.contains(['/', '\\', '%', '$', '\0', '\n']), "{name}");
            assert!(!name.starts_with('.'), "{name}");
            assert!(name.ends_with(".mp4") && name.len() > 4, "{name}");
        }
        let long = "a".repeat(200);
        assert_eq!(suggested_filename(Some(&long), None).chars().count(), 84);
    }

    #[test]
    fn suggested_filename_fits_a_linux_name_with_a_collision_suffix() {
        for title in [
            "🎬".repeat(80),
            "日本語".repeat(40),
            "Tiếng Việt ".repeat(20),
        ] {
            let name = suggested_filename(Some(&title), None);
            let stem = name.strip_suffix(".mp4").unwrap();
            assert!(
                format!("{stem} (999).mp4").len() <= 255,
                "{} bytes",
                name.len()
            );
            assert!(!stem.is_empty());
        }
    }
}
