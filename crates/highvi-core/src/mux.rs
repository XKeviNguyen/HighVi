//! Turns the raw streams yt-dlp downloaded into one MP4, locally.
//!
//! The downloaded bytes came from the network and are untrusted. FFmpeg would
//! otherwise probe them and could, for example, parse a "segment" that is
//! really a playlist and fetch its URLs. HighVi therefore picks the demuxer
//! from magic bytes, limits FFmpeg to the `file` protocol, and copies streams
//! without re-encoding.

use std::{ffi::OsString, path::Path};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Container {
    MpegTs,
    Mp4,
    /// Raw AAC (ADTS), possibly after an ID3 tag: HLS "packed audio".
    Adts,
}

impl Container {
    fn demuxer(self) -> &'static str {
        match self {
            Self::MpegTs => "mpegts",
            Self::Mp4 => "mov",
            Self::Adts => "aac",
        }
    }
}

/// Bytes to read from the start of a file for [`sniff`].
pub const SNIFF_BYTES: usize = 512;

pub fn sniff(header: &[u8]) -> Option<Container> {
    const TS_PACKET: usize = 188;
    // Three sync bytes one packet apart; a lone 0x47 is just the letter G.
    if header.len() > 2 * TS_PACKET && (0..3).all(|packet| header[packet * TS_PACKET] == 0x47) {
        return Some(Container::MpegTs);
    }
    if header.len() >= 8
        && matches!(
            &header[4..8],
            b"ftyp" | b"styp" | b"moov" | b"moof" | b"sidx"
        )
    {
        return Some(Container::Mp4);
    }
    let audio = skip_id3(header)?;
    if audio.len() >= 2 && audio[0] == 0xff && (audio[1] & 0xf6) == 0xf0 {
        return Some(Container::Adts);
    }
    None
}

/// The bytes after a leading ID3v2 tag, or all of them when there is none.
/// `None` when the tag is longer than the header we were given.
fn skip_id3(header: &[u8]) -> Option<&[u8]> {
    if !header.starts_with(b"ID3") {
        return Some(header);
    }
    let size = header.get(6..10)?;
    if size.iter().any(|byte| byte & 0x80 != 0) {
        return None;
    }
    let length = size
        .iter()
        .fold(0_usize, |total, byte| (total << 7) | usize::from(*byte));
    let footer = if header.get(5).is_some_and(|flags| flags & 0x10 != 0) {
        10
    } else {
        0
    };
    header.get(10 + length + footer..)
}

pub struct MuxInput<'a> {
    pub path: &'a Path,
    pub container: Container,
}

/// FFmpeg arguments that copy the first video stream of `video` and the first
/// audio stream of `audio` (or of `video` when there is no separate audio)
/// into `output`. Every path is passed as an explicit `file:` URL.
pub fn mux_args(video: &MuxInput, audio: Option<&MuxInput>, output: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-nostdin", "-hide_banner", "-loglevel", "error"]
        .into_iter()
        .map(OsString::from)
        .collect();
    push_input(&mut args, video);
    if let Some(audio) = audio {
        push_input(&mut args, audio);
    }
    let audio_map = if audio.is_some() { "1:a:0" } else { "0:a:0?" };
    for value in [
        "-map",
        "0:v:0",
        "-map",
        audio_map,
        "-c",
        "copy",
        // Tags and chapters in a source file (comments, per-user
        // watermarks, URLs) are not carried into the saved video.
        "-map_metadata",
        "-1",
        "-map_chapters",
        "-1",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
        "-n",
    ] {
        args.push(value.into());
    }
    args.push(file_url(output));
    args
}

fn push_input(args: &mut Vec<OsString>, input: &MuxInput) {
    for value in [
        "-protocol_whitelist",
        "file",
        "-f",
        input.container.demuxer(),
        "-i",
    ] {
        args.push(value.into());
    }
    args.push(file_url(input.path));
}

fn file_url(path: &Path) -> OsString {
    let mut url = OsString::from("file:");
    url.push(path.as_os_str());
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn sniffs_containers_from_content_not_names() {
        let mut ts = vec![0_u8; 400];
        ts[0] = 0x47;
        ts[188] = 0x47;
        ts[376] = 0x47;
        assert_eq!(sniff(&ts), Some(Container::MpegTs));
        ts[188] = 0;
        assert_eq!(sniff(&ts), None);
        assert_eq!(sniff(b"GET / HTTP/1.1"), None);

        assert_eq!(sniff(b"\0\0\0\x18ftypisom"), Some(Container::Mp4));
        assert_eq!(sniff(b"\0\0\0\x18moofxxxx"), Some(Container::Mp4));
        assert_eq!(sniff(&[0xff, 0xf1, 0x50, 0x80]), Some(Container::Adts));
        let mut id3 = b"ID3\x04\x00\x00\x00\x00\x00\x02ab".to_vec();
        id3.extend_from_slice(&[0xff, 0xf1]);
        assert_eq!(sniff(&id3), Some(Container::Adts));

        for hostile in [
            &b"#EXTM3U\n#EXTINF:1,\nhttp://127.0.0.1/x.ts\n"[..],
            b"<html><body>",
            b"ffconcat version 1.0\nfile 'file:/etc/passwd'",
            b"",
            b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f",
        ] {
            assert_eq!(sniff(hostile), None, "{hostile:?}");
        }
    }

    #[test]
    fn mux_never_probes_and_reads_only_local_files() {
        let video = MuxInput {
            path: Path::new("/job/http:evil/video.mp4"),
            container: Container::MpegTs,
        };
        let audio = MuxInput {
            path: Path::new("/job/audio.mp4"),
            container: Container::Adts,
        };
        let args = strings(&mux_args(&video, Some(&audio), Path::new("/job/out.mp4")));
        let inputs: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, arg)| *arg == "-i")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(inputs.len(), 2);
        for index in inputs {
            assert_eq!(
                args[index - 4..index],
                [
                    "-protocol_whitelist",
                    "file",
                    "-f",
                    args[index - 1].as_str()
                ]
            );
            assert!(args[index + 1].starts_with("file:/job/"));
        }
        assert!(args.contains(&"1:a:0".to_string()));
        assert_eq!(args.last().unwrap(), "file:/job/out.mp4");
        assert!(args.contains(&"-n".to_string()));
        assert!(args.contains(&"copy".to_string()));

        let single = strings(&mux_args(&video, None, Path::new("/job/out.mp4")));
        assert!(single.contains(&"0:a:0?".to_string()));
    }
}
