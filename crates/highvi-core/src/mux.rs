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
    /// Raw AC-3 or E-AC-3 (Dolby Digital / Plus), possibly after an ID3
    /// tag: the other HLS packed audio formats (Apple's examples use them).
    Ac3,
    Eac3,
}

impl Container {
    fn demuxer(self) -> &'static str {
        match self {
            Self::MpegTs => "mpegts",
            Self::Mp4 => "mov",
            Self::Adts => "aac",
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
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
    // AC-3 sync word, then the bitstream ID in the top 5 bits of byte 5:
    // up to 10 is AC-3, 11 to 16 E-AC-3.
    if audio.len() >= 6 && audio[..2] == [0x0b, 0x77] {
        return match audio[5] >> 3 {
            0..=10 => Some(Container::Ac3),
            11..=16 => Some(Container::Eac3),
            _ => None,
        };
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

/// AC-3 frame sizes in 16-bit words by `frmsizecod`, for 48, 44.1 and
/// 32 kHz (ATSC A/52, table 5.18).
const AC3_FRAME_WORDS: [[u16; 38]; 3] = [
    [
        64, 64, 80, 80, 96, 96, 112, 112, 128, 128, 160, 160, 192, 192, 224, 224, 256, 256, 320,
        320, 384, 384, 448, 448, 512, 512, 640, 640, 768, 768, 896, 896, 1024, 1024, 1152, 1152,
        1280, 1280,
    ],
    [
        69, 70, 87, 88, 104, 105, 121, 122, 139, 140, 174, 175, 208, 209, 243, 244, 278, 279, 348,
        349, 417, 418, 487, 488, 557, 558, 696, 697, 835, 836, 975, 976, 1114, 1115, 1253, 1254,
        1393, 1394,
    ],
    [
        96, 96, 120, 120, 144, 144, 168, 168, 192, 192, 240, 240, 288, 288, 336, 336, 384, 384,
        480, 480, 576, 576, 672, 672, 768, 768, 960, 960, 1152, 1152, 1344, 1344, 1536, 1536, 1728,
        1728, 1920, 1920,
    ],
];

/// The length in bytes of the (E-)AC-3 frame starting with `header`
/// (at least 6 bytes), or `None` when it is not one.
fn dolby_frame_len(header: &[u8]) -> Option<usize> {
    if header.len() < 6 || header[..2] != [0x0b, 0x77] {
        return None;
    }
    match header[5] >> 3 {
        0..=10 => {
            let rate = usize::from(header[4] >> 6);
            let code = usize::from(header[4] & 0x3f);
            let words = AC3_FRAME_WORDS.get(rate)?.get(code)?;
            Some(usize::from(*words) * 2)
        }
        11..=16 => {
            let words = (usize::from(header[2] & 0x07) << 8 | usize::from(header[3])) + 1;
            Some(words * 2)
        }
        _ => None,
    }
}

/// Rewrites packed AC-3/E-AC-3 audio (HLS segments joined back to back,
/// each starting with an ID3 tag) as plain frames. FFmpeg's AC-3 reader
/// does not skip tags between frames: it glues each one to the frame
/// before, which damages the audio at every segment boundary. Anything
/// that is neither a whole frame nor an ID3 tag is refused.
pub fn unpack_dolby(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
    let invalid = || std::io::Error::new(ErrorKind::InvalidData, "not packed (E-)AC-3");
    let mut input = BufReader::new(std::fs::File::open(source)?);
    let mut output = BufWriter::new(std::fs::File::create_new(destination)?);
    let mut header = [0_u8; 10];
    let mut frame = Vec::new();
    let mut frames = 0_u64;
    loop {
        match input.read_exact(&mut header[..6]) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        }
        if header.starts_with(b"ID3") {
            input.read_exact(&mut header[6..10])?;
            if header[6..10].iter().any(|byte| byte & 0x80 != 0) {
                return Err(invalid());
            }
            let length = header[6..10]
                .iter()
                .fold(0_u64, |total, byte| (total << 7) | u64::from(*byte));
            let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
            let skipped = std::io::copy(
                &mut (&mut input).take(length + footer),
                &mut std::io::sink(),
            )?;
            if skipped != length + footer {
                return Err(invalid());
            }
            continue;
        }
        let length = dolby_frame_len(&header[..6]).ok_or_else(invalid)?;
        frame.clear();
        frame.extend_from_slice(&header[..6]);
        frame.resize(length, 0);
        input.read_exact(&mut frame[6..]).map_err(|_| invalid())?;
        output.write_all(&frame)?;
        frames += 1;
    }
    if frames == 0 {
        return Err(invalid());
    }
    output.flush()
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
    fn dolby_frames_are_measured_from_their_headers() {
        // Apple's examples: AC-3 at 384 kbit/s, 48 kHz; E-AC-3 at 384 words.
        assert_eq!(
            dolby_frame_len(&[0x0b, 0x77, 0xb8, 0xe0, 0x1c, 0x30]),
            Some(1536)
        );
        assert_eq!(
            dolby_frame_len(&[0x0b, 0x77, 0x01, 0x7f, 0x3f, 0x86]),
            Some(768)
        );
        assert_eq!(
            dolby_frame_len(&[0x0b, 0x77, 0, 0, 0x3f, 0x30]),
            None,
            "bad frmsizecod"
        );
        assert_eq!(
            dolby_frame_len(&[0x0b, 0x77, 0, 0, 0xc0, 0x30]),
            None,
            "reserved rate"
        );
        assert_eq!(dolby_frame_len(b"ID3\x04\x00\x00"), None);
    }

    #[test]
    fn packed_dolby_loses_its_tags_and_keeps_whole_frames() {
        let directory = crate::filesystem::temp_directory("unpack");
        let frame = |fill: u8| {
            let mut frame = vec![0x0b, 0x77, 0xb8, 0xe0, 0x1c, 0x30];
            frame.resize(1536, fill);
            frame
        };
        let tag = b"ID3\x04\x00\x00\x00\x00\x00\x03xyz".to_vec();
        let mut packed = Vec::new();
        for segment in 0..3_u8 {
            packed.extend(&tag);
            packed.extend(frame(segment));
            packed.extend(frame(segment + 10));
        }
        let source = directory.join("audio.ac3");
        let plain = directory.join("audio.plain");
        std::fs::write(&source, &packed).unwrap();
        unpack_dolby(&source, &plain).unwrap();
        let out = std::fs::read(&plain).unwrap();
        assert_eq!(out.len(), 6 * 1536);
        assert!(out.chunks(1536).all(|chunk| chunk[..2] == [0x0b, 0x77]));
        // A truncated frame, junk between frames, or no frame at all: refused.
        for (name, bytes) in [
            ("cut", [&tag[..], &frame(1)[..100]].concat()),
            ("junk", [&frame(1)[..], b"<html>junk-bytes"].concat()),
            ("empty", tag.clone()),
        ] {
            let source = directory.join(format!("{name}.ac3"));
            let plain = directory.join(format!("{name}.plain"));
            std::fs::write(&source, &bytes).unwrap();
            assert!(unpack_dolby(&source, &plain).is_err(), "{name}");
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// Real packed AC-3 (HLS segments joined as yt-dlp saves them), e.g.
    /// Apple's example: `HIGHVI_TEST_PACKED_DOLBY=<file> cargo test -p
    /// highvi-core real_packed_dolby -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real packed (E-)AC-3 file"]
    fn real_packed_dolby_unpacks_to_whole_frames() {
        let source =
            std::path::PathBuf::from(std::env::var_os("HIGHVI_TEST_PACKED_DOLBY").unwrap());
        let plain = source.with_extension("plain");
        let _ = std::fs::remove_file(&plain);
        unpack_dolby(&source, &plain).unwrap();
        let (before, after) = (
            std::fs::metadata(&source).unwrap().len(),
            std::fs::metadata(&plain).unwrap().len(),
        );
        println!(
            "{before} bytes packed -> {after} bytes of frames ({} bytes of tags removed)",
            before - after
        );
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
        // Packed AC-3 and E-AC-3 as Apple's examples serve them (after ID3).
        let ac3 = [0x0b, 0x77, 0xb8, 0xe0, 0x1c, 0x30];
        let eac3 = [0x0b, 0x77, 0x01, 0x7f, 0x3f, 0x86];
        assert_eq!(sniff(&ac3), Some(Container::Ac3));
        assert_eq!(sniff(&eac3), Some(Container::Eac3));
        let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x02ab".to_vec();
        tagged.extend_from_slice(&eac3);
        assert_eq!(sniff(&tagged), Some(Container::Eac3));
        // A sync word with an unknown bitstream ID is not trusted.
        assert_eq!(sniff(&[0x0b, 0x77, 0, 0, 0, 0xff]), None);

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
