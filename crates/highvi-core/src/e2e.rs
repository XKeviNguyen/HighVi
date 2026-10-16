//! End-to-end tests: real yt-dlp and FFmpeg download synthetic media from a
//! local HTTPS fixture through the egress proxy. Fixture host names reach the
//! fixture only through the `#[cfg(test)]` resolver; everything else goes
//! through the production address policy.
//!
//! Without the tools these tests skip, unless `HIGHVI_REQUIRE_E2E` is set
//! (as in CI), in which case they fail.

use crate::{
    PublicError, SourceRequest,
    egress::{FixtureHost, Resolver},
    filesystem::temp_directory,
    job::{JobEnvironment, JobOutcome, JobUpdate, Stage, find_executable, run_job},
    process::cancel_pair,
    validate_source,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        self,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
    },
};

pub(crate) struct Tools {
    pub yt_dlp: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub chrome: Option<PathBuf>,
}

pub(crate) fn tools() -> Option<Tools> {
    match (
        find_executable("yt-dlp"),
        find_executable("ffmpeg"),
        find_executable("ffprobe"),
    ) {
        (Some(yt_dlp), Some(ffmpeg), Some(ffprobe)) => Some(Tools {
            yt_dlp,
            ffmpeg,
            ffprobe,
            chrome: crate::find_browser(),
        }),
        _ if std::env::var_os("HIGHVI_REQUIRE_E2E").is_some() => {
            panic!("HIGHVI_REQUIRE_E2E is set but yt-dlp, ffmpeg or ffprobe is missing")
        }
        _ => {
            eprintln!("skipping end-to-end test: yt-dlp, ffmpeg or ffprobe not found");
            None
        }
    }
}

// ---------------------------------------------------------------- media

static MEDIA: OnceLock<PathBuf> = OnceLock::new();

/// Four seconds of synthetic video and a 440 Hz tone, packaged as HLS in
/// several layouts. Generated once per test process.
pub(crate) fn media(ffmpeg: &Path) -> &'static Path {
    MEDIA.get_or_init(|| generate_media(ffmpeg))
}

fn generate_media(ffmpeg: &Path) -> PathBuf {
    let root = temp_directory("media");
    let x264 = Command::new(ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains("libx264"));
    let video_codec: &[&str] = if x264 {
        &[
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ]
    } else {
        &["-c:v", "mpeg4", "-q:v", "5"]
    };
    std::fs::write(root.join("aes.key"), b"0123456789abcdef").unwrap();
    std::fs::write(
        root.join("aes.keyinfo"),
        format!(
            "https://keys.test/aes/k.bin\n{}\n",
            root.join("aes.key").display()
        ),
    )
    .unwrap();

    let layouts: [(&str, &[&str], &[&str]); 5] = [
        ("muxed", &["-map", "0:v", "-map", "1:a"], &[]),
        ("video", &["-map", "0:v"], &[]),
        ("audio", &["-map", "1:a"], &[]),
        (
            "fmp4",
            &["-map", "0:v", "-map", "1:a"],
            &[
                "-hls_segment_type",
                "fmp4",
                "-hls_fmp4_init_filename",
                "init.mp4",
            ],
        ),
        (
            "aes",
            &["-map", "0:v", "-map", "1:a"],
            &["-hls_key_info_file"],
        ),
    ];
    for (name, maps, extra) in layouts {
        let directory = root.join(name);
        std::fs::create_dir(&directory).unwrap();
        let extension = if name == "fmp4" { "m4s" } else { "ts" };
        let mut command = Command::new(ffmpeg);
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=25:duration=4",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=4",
        ]);
        command.args(maps).args(video_codec).args([
            "-g",
            "25",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_list_size",
            "0",
            "-hls_playlist_type",
            "vod",
        ]);
        command.args(extra);
        if name == "aes" {
            command.arg(root.join("aes.keyinfo"));
        }
        command
            .arg("-hls_segment_filename")
            .arg(directory.join(format!("s%d.{extension}")))
            .arg(directory.join("index.m3u8"));
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "ffmpeg fixture {name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    root
}

// --------------------------------------------------------------- server

#[derive(Clone)]
pub(crate) struct Route {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Arc<Vec<u8>>,
    pub location: Option<String>,
    pub delay: Duration,
    /// Answer 403 unless the request carries exactly this Referer.
    pub required_referer: Option<&'static str>,
    pub headers: Vec<(&'static str, &'static str)>,
    /// Answered instead when the client is headless Chrome, as bot checks
    /// that single out automated browsers do.
    pub for_headless: Option<Arc<Route>>,
    /// Send the body in pieces of this size with this pause between them.
    pub trickle: Option<(usize, Duration)>,
}

impl Route {
    pub fn ok(content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            content_type,
            body: Arc::new(body.into()),
            location: None,
            delay: Duration::ZERO,
            required_referer: None,
            headers: Vec::new(),
            for_headless: None,
            trickle: None,
        }
    }

    pub fn trickled(self, chunk: usize, pause: Duration) -> Self {
        Self {
            trickle: Some((chunk, pause)),
            ..self
        }
    }

    pub fn status(status: u16, content_type: &'static str, body: &str) -> Self {
        Self {
            status,
            ..Self::ok(content_type, body)
        }
    }

    pub fn header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }

    pub fn delayed(self, delay: Duration) -> Self {
        Self { delay, ..self }
    }

    pub fn headless_gets(self, route: Route) -> Self {
        Self {
            for_headless: Some(Arc::new(route)),
            ..self
        }
    }

    pub fn requiring_referer(self, referer: &'static str) -> Self {
        Self {
            required_referer: Some(referer),
            ..self
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            location: Some(location.to_string()),
            ..Self::ok("text/plain", "")
        }
    }

    pub fn playlist(body: &str) -> Self {
        Self::ok("application/vnd.apple.mpegurl", body)
    }

    pub fn html(body: &str) -> Self {
        Self::ok("text/html; charset=utf-8", body)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Hit {
    pub host: String,
    pub path: String,
    pub referer: Option<String>,
}

/// Requests the fixture received, with a signal on every new one so tests
/// can wait for a request instead of sleeping.
#[derive(Default)]
pub(crate) struct HitLog {
    list: Mutex<Vec<Hit>>,
    count: tokio::sync::watch::Sender<usize>,
}

impl HitLog {
    fn push(&self, hit: Hit) {
        self.list.lock().unwrap().push(hit);
        self.count.send_modify(|count| *count += 1);
    }

    /// Resolves once a request for `path` has arrived.
    pub async fn wait_for(&self, path: &str) {
        let mut count = self.count.subscribe();
        let _ = count
            .wait_for(|_| self.list.lock().unwrap().iter().any(|hit| hit.path == path))
            .await;
    }
}

pub(crate) struct Server {
    pub address: SocketAddr,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    pub hits_log: Arc<HitLog>,
}

pub(crate) const FIXTURE_HOSTS: &[&str] = &[
    "media.test",
    "cdn.test",
    "page.test",
    "keys.test",
    "player.test",
    "ads.test",
];

impl Server {
    pub async fn start() -> Self {
        let mut names: Vec<String> = FIXTURE_HOSTS.iter().map(|host| host.to_string()).collect();
        names.extend(["rebind.test".to_string(), "127.0.0.1".to_string()]);
        let certified = rcgen::generate_simple_self_signed(names).unwrap();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
            certified.signing_key.serialize_der(),
        ));
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![certified.cert.der().clone()], key)
        .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let routes: Arc<Mutex<HashMap<String, Route>>> = Arc::default();
        let hits: Arc<HitLog> = Arc::default();
        let (task_routes, task_hits) = (Arc::clone(&routes), Arc::clone(&hits));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let routes = Arc::clone(&task_routes);
                let hits = Arc::clone(&task_hits);
                tokio::spawn(async move {
                    let _ = serve(stream, acceptor, routes, hits).await;
                });
            }
        });
        Self {
            address,
            routes,
            hits_log: hits,
        }
    }

    /// `key` is `host/path`, e.g. `media.test/muxed/index.m3u8`.
    pub fn route(&self, key: &str, route: Route) {
        self.routes.lock().unwrap().insert(key.to_string(), route);
    }

    /// Serves every file of a generated media layout under `media.test/<name>/`.
    pub fn media_dir(&self, root: &Path, name: &str) {
        for entry in std::fs::read_dir(root.join(name)).unwrap() {
            let path = entry.unwrap().path();
            let file = path.file_name().unwrap().to_str().unwrap().to_string();
            let content_type = match path.extension().and_then(|value| value.to_str()) {
                Some("m3u8") => "application/vnd.apple.mpegurl",
                Some("ts") => "video/mp2t",
                _ => "video/mp4",
            };
            self.route(
                &format!("media.test/{name}/{file}"),
                Route::ok(content_type, std::fs::read(&path).unwrap()),
            );
        }
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.hits_log.list.lock().unwrap().clone()
    }

    /// Fixture hosts resolve to this server; `rebind.test` does too, but is
    /// untrusted, like a public name whose DNS answer points at loopback.
    pub fn resolver(&self) -> Resolver {
        let mut hosts: HashMap<String, FixtureHost> = FIXTURE_HOSTS
            .iter()
            .map(|host| {
                (
                    host.to_string(),
                    FixtureHost {
                        addresses: vec![self.address],
                        trusted: true,
                    },
                )
            })
            .collect();
        hosts.insert(
            "rebind.test".into(),
            FixtureHost {
                addresses: vec![self.address],
                trusted: false,
            },
        );
        hosts.insert(
            "private.test".into(),
            FixtureHost {
                addresses: vec!["10.0.0.1:443".parse().unwrap()],
                trusted: false,
            },
        );
        Resolver::Fixture(Arc::new(hosts))
    }
}

async fn serve(
    stream: TcpStream,
    acceptor: TlsAcceptor,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    hits: Arc<HitLog>,
) -> std::io::Result<()> {
    let mut tls = acceptor.accept(stream).await?;
    let mut head = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = tls.read(&mut chunk).await?;
        if read == 0 || head.len() > 64 * 1024 {
            return Ok(());
        }
        head.extend_from_slice(&chunk[..read]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let mut request = lines.next().unwrap_or_default().split(' ');
    let method = request.next().unwrap_or_default().to_string();
    let target = request.next().unwrap_or_default();
    let path = target.split('?').next().unwrap_or_default().to_string();
    let mut host = String::new();
    let mut referer = None;
    let mut headless = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "host" => {
                    host = value
                        .trim()
                        .split(':')
                        .next()
                        .unwrap_or_default()
                        .to_string()
                }
                "referer" => referer = Some(value.trim().to_string()),
                "user-agent" => headless = value.contains("HeadlessChrome"),
                _ => {}
            }
        }
    }
    hits.push(Hit {
        host: host.clone(),
        path: path.clone(),
        referer: referer.clone(),
    });
    let route = routes
        .lock()
        .unwrap()
        .get(&format!("{host}{path}"))
        .cloned();
    let route = match route {
        Some(route)
            if route
                .required_referer
                .is_some_and(|required| referer.as_deref() != Some(required)) =>
        {
            Route {
                status: 403,
                ..Route::ok("text/plain", "forbidden")
            }
        }
        Some(Route {
            for_headless: Some(variant),
            ..
        }) if headless => (*variant).clone(),
        Some(route) => route,
        None => Route {
            status: 404,
            ..Route::ok("text/plain", "not found")
        },
    };
    tokio::time::sleep(route.delay).await;
    let mut response = format!(
        // CDNs that serve web players allow cross-origin reads.
        "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n",
        route.status,
        route.content_type,
        route.body.len()
    );
    if let Some(location) = &route.location {
        response.push_str(&format!("Location: {location}\r\n"));
    }
    for (name, value) in &route.headers {
        response.push_str(&format!("{name}: {value}\r\n"));
    }
    response.push_str("\r\n");
    tls.write_all(response.as_bytes()).await?;
    if method != "HEAD" {
        match route.trickle {
            Some((chunk, pause)) => {
                for piece in route.body.chunks(chunk.max(1)) {
                    tls.write_all(piece).await?;
                    tls.flush().await?;
                    tokio::time::sleep(pause).await;
                }
            }
            None => tls.write_all(&route.body).await?,
        }
    }
    tls.shutdown().await
}

// ---------------------------------------------------------------- helpers

pub(crate) struct Fixture {
    pub tools: Tools,
    pub server: Server,
    pub output_dir: PathBuf,
    /// The diagnostic report of the last job run through this fixture.
    pub last_report: Mutex<String>,
}

impl Fixture {
    pub async fn new() -> Option<Self> {
        let tools = tools()?;
        let media = media(&tools.ffmpeg);
        let server = Server::start().await;
        for name in ["muxed", "video", "audio", "fmp4", "aes"] {
            server.media_dir(media, name);
        }
        server.route(
            "keys.test/aes/k.bin",
            Route::ok("application/octet-stream", b"0123456789abcdef".to_vec()),
        );
        server.route(
            "media.test/muxed/master.m3u8",
            Route::playlist(MUXED_MASTER),
        );
        let output_dir = temp_directory("out").join("HighVi");
        Some(Self {
            tools,
            server,
            output_dir,
            last_report: Mutex::default(),
        })
    }

    pub fn environment(&self) -> JobEnvironment {
        JobEnvironment {
            fixture: Some(self.server.resolver()),
            ..JobEnvironment::new(
                self.tools.yt_dlp.clone(),
                self.tools.ffmpeg.clone(),
                self.tools.chrome.clone(),
                self.output_dir.clone(),
            )
        }
    }

    pub async fn run(
        &self,
        request: SourceRequest,
    ) -> (Result<JobOutcome, PublicError>, Vec<JobUpdate>) {
        self.run_with(&self.environment(), request, None).await
    }

    pub async fn run_with(
        &self,
        environment: &JobEnvironment,
        request: SourceRequest,
        cancel_after: Option<(Stage, Duration)>,
    ) -> (Result<JobOutcome, PublicError>, Vec<JobUpdate>) {
        let source = validate_source(&request).unwrap();
        let (handle, token) = cancel_pair();
        let handle = Arc::new(handle);
        let armed = std::sync::atomic::AtomicBool::new(false);
        let updates = Mutex::new(Vec::new());
        let report = |update: JobUpdate| {
            if let Some((stage, delay)) = cancel_after
                && update.stage == stage
                && !armed.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                if delay.is_zero() {
                    handle.cancel();
                }
                let handle = Arc::clone(&handle);
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    handle.cancel();
                });
            }
            updates.lock().unwrap().push(update);
        };
        let (result, diagnostics) = run_job(environment, &source, token, &report).await;
        *self.last_report.lock().unwrap() = diagnostics.render();
        (result, updates.into_inner().unwrap())
    }

    /// Stream types and duration of an MP4, and whether it decodes cleanly.
    pub fn inspect(&self, path: &Path) -> (Vec<String>, f64, bool) {
        let probe = Command::new(&self.tools.ffprobe)
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type:format=duration",
                "-of",
                "default=nw=1:nk=1",
            ])
            .arg(path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).into_owned();
        let mut streams = Vec::new();
        let mut duration = 0.0;
        for line in text.lines() {
            match line.parse::<f64>() {
                Ok(value) => duration = value,
                Err(_) => streams.push(line.to_string()),
            }
        }
        let decode = Command::new(&self.tools.ffmpeg)
            .args(["-v", "error", "-xerror", "-i"])
            .arg(path)
            .args(["-f", "null", "-"])
            .output()
            .unwrap();
        let clean = decode.status.success() && decode.stderr.is_empty();
        (streams, duration, clean)
    }

    pub fn assert_no_job_dirs(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.output_dir) {
            for entry in entries {
                let name = entry.unwrap().file_name();
                assert!(
                    !name.to_string_lossy().starts_with(".highvi-job-"),
                    "job directory left behind: {name:?}"
                );
            }
        }
    }

    pub fn saved_files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.output_dir)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                    .filter(|name| !name.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

pub(crate) const MUXED_MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=600000,RESOLUTION=320x240,CODECS=\"avc1.64000d,mp4a.40.2\"\nindex.m3u8\n";

pub(crate) fn direct(playlist: &str) -> SourceRequest {
    SourceRequest::DirectHls {
        playlist_url: playlist.to_string(),
        referer_page_url: "https://page.test/watch/1?session=secret".to_string(),
    }
}

pub(crate) fn page(url: &str) -> SourceRequest {
    SourceRequest::Page {
        page_url: url.to_string(),
    }
}

fn media_playlist(segments: &[&str], extra_head: &str) -> String {
    let mut body = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-PLAYLIST-TYPE:VOD\n{extra_head}"
    );
    for segment in segments {
        body.push_str(&format!("#EXTINF:1.0,\n{segment}\n"));
    }
    body.push_str("#EXT-X-ENDLIST\n");
    body
}

fn assert_code(result: &Result<JobOutcome, PublicError>, code: &str) {
    match result {
        Ok(outcome) => panic!("expected {code}, got success: {outcome:?}"),
        Err(error) => assert_eq!(error.code, code, "{error:?}"),
    }
}

// ------------------------------------------------------------------ tests

#[tokio::test]
async fn direct_hls_master_becomes_a_playable_mp4_with_an_origin_only_referer() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (result, updates) = fixture
        .run(direct("https://media.test/muxed/master.m3u8"))
        .await;
    let outcome = result.unwrap();
    // "master" says nothing; the playlist's folder names the file.
    assert_eq!(outcome.saved_to, fixture.output_dir.join("muxed.mp4"));
    let (streams, duration, clean) = fixture.inspect(&outcome.saved_to);
    assert_eq!(streams, ["video", "audio"]);
    assert!((3.5..=4.6).contains(&duration), "{duration}");
    assert!(clean, "output does not decode cleanly");

    let hits = fixture.server.hits();
    assert!(hits.iter().any(|hit| hit.path.ends_with(".ts")));
    for hit in &hits {
        assert_eq!(
            hit.referer.as_deref(),
            Some("https://page.test/"),
            "{hit:?}"
        );
    }
    let stages: Vec<Stage> = updates.iter().map(|update| update.stage).collect();
    for stage in [
        Stage::Discovering,
        Stage::DownloadingVideo,
        Stage::Merging,
        Stage::Saving,
    ] {
        assert!(stages.contains(&stage), "{stage:?} missing from {stages:?}");
    }
    assert!(updates.iter().any(|update| update.percent.is_some()));
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn separate_audio_rendition_is_downloaded_and_muxed() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.server.route(
        "media.test/split/master.m3u8",
        Route::playlist(
            "#EXTM3U\n\
             #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,AUTOSELECT=YES,URI=\"https://media.test/audio/index.m3u8\"\n\
             #EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=320x240,CODECS=\"avc1.64000d,mp4a.40.2\",AUDIO=\"aud\"\n\
             https://media.test/video/index.m3u8\n",
        ),
    );
    let (result, updates) = fixture
        .run(direct("https://media.test/split/master.m3u8"))
        .await;
    let outcome = result.unwrap();
    let (streams, _, clean) = fixture.inspect(&outcome.saved_to);
    assert_eq!(streams, ["video", "audio"]);
    assert!(clean);
    assert!(
        updates
            .iter()
            .any(|update| update.stage == Stage::DownloadingAudio)
    );
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn fragmented_mp4_and_aes_128_streams_are_supported() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for name in ["fmp4", "aes"] {
        let (result, _) = fixture
            .run(direct(&format!("https://media.test/{name}/index.m3u8")))
            .await;
        let outcome = result.unwrap_or_else(|error| panic!("{name}: {error:?}"));
        let (streams, _, clean) = fixture.inspect(&outcome.saved_to);
        assert_eq!(streams, ["video", "audio"], "{name}");
        assert!(clean, "{name}");
    }
    assert!(
        fixture
            .server
            .hits()
            .iter()
            .any(|hit| hit.host == "keys.test")
    );
    // Both playlists are called "index"; their folders name the files.
    assert_eq!(fixture.saved_files(), ["aes.mp4", "fmp4.mp4"]);
}

#[tokio::test]
async fn page_mode_finds_html5_media_and_uses_a_unicode_title() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.server.route(
        "page.test/watch/1",
        Route::html(
            "<!doctype html><html><head><title>Phim hay 日本語 🎬</title></head><body>\
             <video controls><source src=\"https://media.test/muxed/master.m3u8\" type=\"application/x-mpegURL\"></video>\
             </body></html>",
        ),
    );
    // yt-dlp titles the media of a generic page "<page title> (1)".
    let title = "Phim hay 日本語 🎬 (1)";
    std::fs::create_dir_all(&fixture.output_dir).unwrap();
    std::fs::write(fixture.output_dir.join(format!("{title}.mp4")), b"existing").unwrap();

    let (result, updates) = fixture.run(page("https://page.test/watch/1")).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.title, title);
    assert_eq!(
        outcome.saved_to,
        fixture.output_dir.join(format!("{title} (2).mp4"))
    );
    assert_eq!(
        std::fs::read(fixture.output_dir.join(format!("{title}.mp4"))).unwrap(),
        b"existing"
    );
    assert!(
        updates
            .iter()
            .any(|update| update.title.as_deref() == Some(title))
    );
    // yt-dlp's generic extractor records the full page URL as the media
    // Referer; HighVi must still send only the origin.
    let media_hits: Vec<Hit> = fixture
        .server
        .hits()
        .into_iter()
        .filter(|hit| hit.host == "media.test" && !hit.path.ends_with("master.m3u8"))
        .collect();
    assert!(media_hits.iter().any(|hit| hit.path.ends_with(".ts")));
    for hit in media_hits {
        assert_eq!(
            hit.referer.as_deref(),
            Some("https://page.test/"),
            "{hit:?}"
        );
    }
}

/// Each case points one downstream reference at a blocked destination. The
/// job must fail with `unsafe_network_target`, and no request may reach the
/// fixture through the blocked name.
#[tokio::test]
async fn downstream_references_to_private_networks_are_blocked() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let port = fixture.server.address.port();
    let loopback = format!("https://127.0.0.1:{port}");
    let cases = [
        (
            "variant-loopback",
            format!("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n{loopback}/canary/v.m3u8\n"),
        ),
        (
            "variant-private-dns",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nhttps://private.test/canary/v.m3u8\n"
                .to_string(),
        ),
        (
            "segment-loopback",
            media_playlist(&[&format!("{loopback}/canary/s0.ts")], ""),
        ),
        (
            "segment-rebind",
            media_playlist(&["https://rebind.test/canary/s0.ts"], ""),
        ),
        (
            "segment-ipv6-loopback",
            media_playlist(&[&format!("https://[::1]:{port}/canary/s0.ts")], ""),
        ),
        (
            "segment-metadata-ip",
            media_playlist(&["https://169.254.169.254/canary/s0.ts"], ""),
        ),
        (
            "key-rebind",
            media_playlist(
                &["https://media.test/aes/s0.ts"],
                "#EXT-X-KEY:METHOD=AES-128,URI=\"https://rebind.test/canary/k.bin\"\n",
            ),
        ),
    ];
    for (name, body) in &cases {
        fixture
            .server
            .route(&format!("media.test/{name}.m3u8"), Route::playlist(body));
    }
    fixture.server.route(
        "media.test/redirect.m3u8",
        Route::redirect(&format!("{loopback}/canary/master.m3u8")),
    );

    for name in cases.iter().map(|(name, _)| *name).chain(["redirect"]) {
        let (result, _) = fixture
            .run(direct(&format!("https://media.test/{name}.m3u8")))
            .await;
        assert_code(&result, "unsafe_network_target");
        let leaked: Vec<Hit> = fixture
            .server
            .hits()
            .into_iter()
            .filter(|hit| hit.path.starts_with("/canary"))
            .collect();
        assert!(
            leaked.is_empty(),
            "{name}: blocked destination reached: {leaked:?}"
        );
    }
    assert!(fixture.saved_files().is_empty());
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn user_entered_private_urls_are_blocked_by_the_proxy() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let port = fixture.server.address.port();
    for url in [
        format!("https://127.0.0.1:{port}/canary/page"),
        "https://rebind.test/canary/page".to_string(),
        "https://[::ffff:127.0.0.1]/canary/page".to_string(),
    ] {
        let (result, _) = fixture.run(page(&url)).await;
        assert_code(&result, "unsafe_network_target");
    }
    assert!(fixture.server.hits().is_empty());
}

#[tokio::test]
async fn allowed_redirects_are_followed() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.server.route(
        "cdn.test/go.m3u8",
        Route::redirect("https://media.test/muxed/master.m3u8"),
    );
    let (result, _) = fixture.run(direct("https://cdn.test/go.m3u8")).await;
    let outcome = result.unwrap();
    let (streams, _, clean) = fixture.inspect(&outcome.saved_to);
    assert_eq!(streams, ["video", "audio"]);
    assert!(clean);
}

#[tokio::test]
async fn plain_http_segments_are_refused() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.server.route(
        "media.test/http.m3u8",
        Route::playlist(&media_playlist(&["http://media.test/muxed/s0.ts"], "")),
    );
    let (result, _) = fixture.run(direct("https://media.test/http.m3u8")).await;
    assert_code(&result, "insecure_media_url");
}

#[tokio::test]
async fn missing_segments_fail_instead_of_leaving_gaps() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.server.route(
        "media.test/gap.m3u8",
        Route::playlist(&media_playlist(
            &[
                "https://media.test/muxed/s0.ts",
                "https://media.test/muxed/missing.ts",
                "https://media.test/muxed/s2.ts",
            ],
            "",
        )),
    );
    let (result, _) = fixture.run(direct("https://media.test/gap.m3u8")).await;
    assert!(
        result.is_err(),
        "a playlist with a missing segment produced a file"
    );
    assert!(fixture.saved_files().is_empty());
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn drm_live_and_malformed_playlists_are_rejected() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.server.route(
        "media.test/drm.m3u8",
        Route::playlist(&media_playlist(
            &["https://media.test/muxed/s0.ts"],
            "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://key\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n",
        )),
    );
    fixture.server.route(
        "media.test/live.m3u8",
        Route::playlist(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:7\n\
             #EXTINF:1.0,\nhttps://media.test/muxed/s0.ts\n#EXTINF:1.0,\nhttps://media.test/muxed/s1.ts\n",
        ),
    );
    fixture.server.route(
        "media.test/broken.m3u8",
        Route::playlist("this is not a playlist"),
    );

    let (result, _) = fixture.run(direct("https://media.test/drm.m3u8")).await;
    assert_code(&result, "drm_not_supported");
    for name in ["live", "broken"] {
        let (result, _) = fixture
            .run(direct(&format!("https://media.test/{name}.m3u8")))
            .await;
        assert!(result.is_err(), "{name} produced a file");
    }
    assert!(fixture.saved_files().is_empty());
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn cancellation_stops_the_download_and_cleans_up() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let slow = Route {
        delay: Duration::from_secs(30),
        ..Route::ok("video/mp2t", Vec::new())
    };
    fixture.server.route("media.test/slow/s0.ts", slow);
    fixture.server.route(
        "media.test/slow.m3u8",
        Route::playlist(&media_playlist(&["https://media.test/slow/s0.ts"], "")),
    );
    // Cancel while the segment request is in flight (its answer takes 30 s).
    let source = validate_source(&direct("https://media.test/slow.m3u8")).unwrap();
    let (handle, token) = cancel_pair();
    let environment = fixture.environment();
    let job = run_job(&environment, &source, token, &|_| {});
    let trigger = async {
        fixture.server.hits_log.wait_for("/slow/s0.ts").await;
        handle.cancel();
        Instant::now()
    };
    let ((result, _), cancelled_at) = tokio::join!(job, trigger);
    assert_code(&result, "cancelled");
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(5),
        "{:?}",
        cancelled_at.elapsed()
    );
    assert!(fixture.saved_files().is_empty());
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn cancelling_while_saving_publishes_nothing() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let (result, _) = fixture
        .run_with(
            &fixture.environment(),
            direct("https://media.test/muxed/master.m3u8"),
            Some((Stage::Saving, Duration::ZERO)),
        )
        .await;
    assert_code(&result, "cancelled");
    assert!(fixture.saved_files().is_empty());
    fixture.assert_no_job_dirs();
}

/// Signed media URLs live in the job's info JSON; it must be gone before
/// anything in the job directory can be kept for recovery.
#[tokio::test]
async fn signed_urls_do_not_outlive_the_download() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let output_dir = fixture.output_dir.clone();
    let probe = Arc::clone(&seen);
    let source = validate_source(&direct("https://media.test/muxed/master.m3u8")).unwrap();
    let (_handle, token) = cancel_pair();
    let report = move |update: JobUpdate| {
        if update.stage == Stage::Merging {
            for entry in std::fs::read_dir(&output_dir).unwrap().flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".highvi-job-")
                {
                    let names: Vec<String> = std::fs::read_dir(entry.path())
                        .unwrap()
                        .flatten()
                        .map(|file| file.file_name().to_string_lossy().into_owned())
                        .collect();
                    probe.lock().unwrap().extend(names);
                }
            }
        }
    };
    run_job(&fixture.environment(), &source, token, &report)
        .await
        .0
        .unwrap();
    let names = seen.lock().unwrap().clone();
    assert!(!names.is_empty(), "the job directory was not inspected");
    assert!(
        !names.iter().any(|name| name.ends_with(".json")),
        "{names:?}"
    );
}

#[tokio::test]
async fn missing_tools_are_reported_as_dependencies() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let mut environment = fixture.environment();
    environment.yt_dlp = PathBuf::from("/nonexistent/yt-dlp");
    let (result, _) = fixture
        .run_with(
            &environment,
            direct("https://media.test/muxed/master.m3u8"),
            None,
        )
        .await;
    assert_code(&result, "missing_dependency");

    let mut environment = fixture.environment();
    environment.ffmpeg = PathBuf::from("/nonexistent/ffmpeg");
    let (result, _) = fixture
        .run_with(
            &environment,
            direct("https://media.test/muxed/master.m3u8"),
            None,
        )
        .await;
    assert_code(&result, "missing_dependency");
    fixture.assert_no_job_dirs();
}

// ------------------------------------------------------- browser discovery

/// Browser tests need Chrome; it is mandatory when `HIGHVI_REQUIRE_E2E` is set.
async fn browser_fixture() -> Option<Fixture> {
    let fixture = Fixture::new().await?;
    if fixture.tools.chrome.is_none() {
        assert!(
            std::env::var_os("HIGHVI_REQUIRE_E2E").is_none(),
            "HIGHVI_REQUIRE_E2E is set but Chrome/Chromium is missing"
        );
        eprintln!("skipping browser test: Chrome/Chromium not found");
        return None;
    }
    Some(fixture)
}

/// Chrome processes still running with a profile under this fixture.
fn chrome_survivors(fixture: &Fixture) -> Vec<String> {
    let needle = fixture.output_dir.to_string_lossy().into_owned();
    let mut found = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) {
            let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            if cmdline.contains(&needle) && cmdline.contains("chrome") {
                // Whole: snap Chromium puts a long path and its own flags
                // first, so HighVi's flags can come late.
                found.push(cmdline);
            }
        }
    }
    found
}

async fn assert_no_chrome_left(fixture: &Fixture) {
    for _ in 0..50 {
        if chrome_survivors(fixture).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let survivors: Vec<String> = chrome_survivors(fixture)
        .iter()
        .map(|cmdline| cmdline.chars().take(160).collect())
        .collect();
    panic!("browser processes survived: {survivors:?}");
}

const SPLIT_MASTER_ABSOLUTE: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=600000,RESOLUTION=320x240,CODECS=\"avc1.64000d,mp4a.40.2\"\nhttps://media.test/muxed/index.m3u8\n";

#[tokio::test]
async fn javascript_built_player_is_found_in_the_browser() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    // The URL only exists once the script runs, so yt-dlp alone cannot see
    // it; the playlist host also insists on the page's Referer.
    fixture.server.route(
        "page.test/js",
        Route::html(
            "<!doctype html><title>Bản tin 夜 🎬</title><script>\
             const parts = ['https://media.test', 'js', 'master.m3u8'];\
             fetch(parts.join('/')).then(r => r.text()).then(() =>\
               fetch('https://media.test/muxed/index.m3u8'));\
             </script>",
        ),
    );
    fixture.server.route(
        "media.test/js/master.m3u8",
        Route::playlist(SPLIT_MASTER_ABSOLUTE).requiring_referer("https://page.test/"),
    );
    let started = Instant::now();
    let (result, updates) = fixture.run(page("https://page.test/js")).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.title, "Bản tin 夜 🎬");
    assert_eq!(
        outcome.saved_to,
        fixture.output_dir.join("Bản tin 夜 🎬.mp4")
    );
    let (streams, _, clean) = fixture.inspect(&outcome.saved_to);
    assert_eq!(streams, ["video", "audio"]);
    assert!(clean);
    assert!(updates.iter().any(|update| update.stage == Stage::Browsing));
    assert!(
        started.elapsed() < Duration::from_secs(40),
        "{:?}",
        started.elapsed()
    );
    for hit in fixture
        .server
        .hits()
        .iter()
        .filter(|hit| hit.host == "media.test")
    {
        assert_eq!(
            hit.referer.as_deref(),
            Some("https://page.test/"),
            "{hit:?}"
        );
    }
    assert_no_chrome_left(&fixture).await;
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn delayed_player_in_an_iframe_beats_an_advert() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    fixture.server.route(
        "page.test/embed",
        Route::html(
            "<!doctype html><title>Main video</title>\
             <iframe src=\"https://ads.test/ad\"></iframe>\
             <iframe src=\"https://player.test/embed?id=7\"></iframe>",
        ),
    );
    fixture.server.route(
        "ads.test/ad",
        Route::html("<script>fetch('https://ads.test/ad.m3u8').then(r => r.text())</script>"),
    );
    fixture.server.route(
        "ads.test/ad.m3u8",
        Route::playlist(&media_playlist(&["https://media.test/muxed/s0.ts"], "")),
    );
    // Served without a .m3u8 suffix; only the MIME type says it is HLS.
    fixture.server.route(
        "player.test/embed",
        Route::html(
            "<script>setTimeout(() => fetch('https://media.test/api/stream').then(r => r.text()), 1500)</script>",
        ),
    );
    fixture.server.route(
        "media.test/api/stream",
        Route::playlist(SPLIT_MASTER_ABSOLUTE).requiring_referer("https://player.test/"),
    );
    let (result, _) = fixture.run(page("https://page.test/embed")).await;
    let outcome = result.unwrap();
    assert_eq!(outcome.title, "Main video");
    let (_, duration, clean) = fixture.inspect(&outcome.saved_to);
    assert!(duration > 3.5, "picked the advert: {duration}");
    assert!(clean);
    // The media was fetched with the iframe's origin, as the browser did.
    let segment_referers: Vec<_> = fixture
        .server
        .hits()
        .into_iter()
        .filter(|hit| hit.path.starts_with("/muxed/s"))
        .map(|hit| hit.referer)
        .collect();
    assert!(segment_referers.contains(&Some("https://player.test/".to_string())));
    assert_no_chrome_left(&fixture).await;
}

#[tokio::test]
async fn playlists_are_recognised_by_content_and_after_pressing_play() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    // Generic content type, no suffix: recognised by its #EXTM3U body.
    fixture.server.route(
        "page.test/sniff",
        Route::html(
            "<script>fetch('https://media.test/api/manifest').then(r => r.text())</script>",
        ),
    );
    fixture.server.route(
        "media.test/api/manifest",
        Route::ok("text/plain", SPLIT_MASTER_ABSOLUTE),
    );
    // Nothing loads until the video is played.
    fixture.server.route(
        "page.test/click",
        Route::html(
            "<video id=v></video><script>\
             document.getElementById('v').addEventListener('play', () =>\
               fetch('https://media.test/muxed/master.m3u8').then(r => r.text()));\
             </script>",
        ),
    );
    for url in ["https://page.test/sniff", "https://page.test/click"] {
        let (result, _) = fixture.run(page(url)).await;
        let outcome = result.unwrap_or_else(|error| panic!("{url}: {error:?}"));
        let (streams, _, clean) = fixture.inspect(&outcome.saved_to);
        assert_eq!(streams, ["video", "audio"], "{url}");
        assert!(clean, "{url}");
    }
    assert_no_chrome_left(&fixture).await;
}

#[tokio::test]
async fn browser_requests_to_private_networks_are_blocked() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    let port = fixture.server.address.port();
    fixture.server.route(
        "page.test/probe",
        Route::html(&format!(
            "<iframe src=\"https://127.0.0.1:{port}/canary/frame\"></iframe>\
             <img src=\"https://[::1]:{port}/canary/pixel\">\
             <script>\
             fetch('https://127.0.0.1:{port}/canary/a.m3u8').catch(() => {{}});\
             fetch('https://rebind.test/canary/b.m3u8').catch(() => {{}});\
             fetch('http://media.test/canary/c.m3u8').catch(() => {{}});\
             new WebSocket('wss://127.0.0.1:{port}/canary/ws');\
             </script>"
        )),
    );
    let started = Instant::now();
    let (result, _) = fixture.run(page("https://page.test/probe")).await;
    assert_code(&result, "no_media_found");
    assert!(
        started.elapsed() < Duration::from_secs(40),
        "{:?}",
        started.elapsed()
    );
    let leaked: Vec<Hit> = fixture
        .server
        .hits()
        .into_iter()
        .filter(|hit| hit.path.starts_with("/canary"))
        .collect();
    assert!(leaked.is_empty(), "blocked destination reached: {leaked:?}");
    assert_no_chrome_left(&fixture).await;
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn browser_reports_drm_and_never_downloads_it() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    fixture.server.route(
        "page.test/drm",
        Route::html("<script>fetch('https://media.test/drm/p.m3u8').then(r => r.text())</script>"),
    );
    fixture.server.route(
        "media.test/drm/p.m3u8",
        Route::playlist(&media_playlist(
            &["https://media.test/muxed/s0.ts"],
            "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://key\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n",
        )),
    );
    let (result, _) = fixture.run(page("https://page.test/drm")).await;
    assert_code(&result, "drm_not_supported");
    assert!(
        !fixture
            .server
            .hits()
            .iter()
            .any(|hit| hit.path.ends_with(".ts")),
        "DRM media was fetched"
    );
}

#[tokio::test]
async fn pages_without_media_and_broken_browsers_fail_cleanly() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    fixture.server.route(
        "page.test/empty",
        Route::html("<title>Nothing</title><p>hi</p>"),
    );
    let started = Instant::now();
    let (result, _) = fixture.run(page("https://page.test/empty")).await;
    assert_code(&result, "no_media_found");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );

    let mut crashing = fixture.environment();
    crashing.chrome = Some(PathBuf::from("/bin/false"));
    let (result, _) = fixture
        .run_with(&crashing, page("https://page.test/empty"), None)
        .await;
    assert_code(&result, "browser_failed");

    // Without a browser, yt-dlp's own explanation is kept.
    let mut without = fixture.environment();
    without.chrome = None;
    let (result, _) = fixture
        .run_with(&without, page("https://page.test/empty"), None)
        .await;
    assert_code(&result, "unsupported_page");
    assert_no_chrome_left(&fixture).await;
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn cancelling_during_browser_discovery_stops_chrome() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    fixture
        .server
        .route("page.test/slow", Route::html("<title>slow</title>"));
    let started = Instant::now();
    let (result, _) = fixture
        .run_with(
            &fixture.environment(),
            page("https://page.test/slow"),
            Some((Stage::Browsing, Duration::from_secs(2))),
        )
        .await;
    assert_code(&result, "cancelled");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert_no_chrome_left(&fixture).await;
    fixture.assert_no_job_dirs();
}

/// The machine's first non-loopback IPv4 address, where a LAN service would
/// listen. WebRTC is the classic way for a page to reach one without HTTP.
fn lan_ipv4() -> Option<std::net::Ipv4Addr> {
    crate::netpolicy::local_addresses()?
        .into_iter()
        .find_map(|address| match address {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => Some(v4),
            _ => None,
        })
}

#[tokio::test]
async fn webrtc_cannot_send_udp_around_the_proxy() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    let mut targets = vec![std::net::Ipv4Addr::LOCALHOST];
    targets.extend(lan_ipv4());
    let mut sockets = Vec::new();
    let mut servers = Vec::new();
    for address in targets {
        let socket = tokio::net::UdpSocket::bind((address, 0)).await.unwrap();
        servers.push(format!("{{urls:'stun:{}'}}", socket.local_addr().unwrap()));
        sockets.push(socket);
    }
    fixture.server.route(
        "page.test/rtc",
        Route::html(&format!(
            "<script>\
             const pc = new RTCPeerConnection({{iceServers:[{}]}});\
             pc.createDataChannel('x');\
             pc.createOffer().then(o => pc.setLocalDescription(o));\
             </script>",
            servers.join(",")
        )),
    );
    let (result, _) = fixture.run(page("https://page.test/rtc")).await;
    assert_code(&result, "no_media_found");
    let mut buffer = [0_u8; 1500];
    for socket in &sockets {
        let received = socket.try_recv_from(&mut buffer);
        assert!(
            received.is_err(),
            "a STUN packet reached {} directly",
            socket.local_addr().unwrap()
        );
    }
    assert_no_chrome_left(&fixture).await;
}

// ------------------------------------------- failure attribution (V1.1 A1/A2)

/// The page as plain HTTP clients (yt-dlp) see it: no player in the HTML.
const STATIC_PAGE: &str = "<!doctype html><title>Guarded</title><p>player loads later</p>";

/// Incident A1: yt-dlp gets the page, but the site shows headless Chrome a
/// bot check (Cloudflare: HTTP 403 + `cf-mitigated: challenge`). That is a
/// refusal, not a page without video, and nothing must be fetched past it.
#[tokio::test]
async fn a_bot_check_in_the_browser_is_reported_as_a_refusal() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    let challenge = "<!doctype html><title>Just a moment...</title>\
        <iframe src=\"https://ads.test/turnstile\"></iframe>\
        <script>fetch('https://ads.test/challenge-platform', {method: 'POST'})</script>";
    let cases = [
        (
            "page.test/bot",
            Route::status(403, "text/html", challenge).header("cf-mitigated", "challenge"),
            "host_firewall_block",
            "document 403 (Cloudflare bot check)",
        ),
        (
            "page.test/forbidden",
            Route::status(403, "text/html", "<title>Forbidden</title>"),
            "access_denied",
            "document 403,",
        ),
        (
            "page.test/busy",
            Route::status(429, "text/html", "<title>Slow down</title>"),
            "rate_limited",
            "document 429,",
        ),
        (
            "page.test/gone",
            Route::status(404, "text/html", "<title>Not here</title>"),
            "page_unavailable",
            "document 404,",
        ),
    ];
    for (key, headless, _, _) in &cases {
        fixture.server.route(
            key,
            Route::html(STATIC_PAGE).headless_gets(headless.clone()),
        );
    }
    let run = |index: usize| {
        let (key, _, code, evidence) = &cases[index];
        let fixture = &fixture;
        async move {
            let mut environment = fixture.environment();
            environment.output_dir = fixture.output_dir.join(key.replace('/', "-"));
            let source = validate_source(&page(&format!("https://{key}"))).unwrap();
            let (_handle, token) = cancel_pair();
            let (result, diagnostics) = run_job(&environment, &source, token, &|_| {}).await;
            (result, diagnostics.render(), *code, *evidence)
        }
    };
    for index in 0..cases.len() {
        let (result, report, code, evidence) = run(index).await;
        assert_code(&result, code);
        assert!(report.contains(evidence), "{evidence} missing:\n{report}");
        assert!(report.contains("during browsing"), "{report}");
    }
    assert!(
        !fixture
            .server
            .hits()
            .iter()
            .any(|hit| hit.host == "media.test"),
        "media was fetched past a refusal"
    );
    assert_no_chrome_left(&fixture).await;
}

/// The player asks for its playlist, but the media server refuses it
/// (geo-block, expired token): not "no video on this page".
#[tokio::test]
async fn a_refused_player_playlist_is_reported_as_such() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    fixture.server.route(
        "page.test/geo",
        Route::html(
            "<!doctype html><title>Geo</title><script>\
             fetch('https://media.test/geo/master.m3u8').catch(() => {});\
             </script>",
        ),
    );
    fixture.server.route(
        "media.test/geo/master.m3u8",
        Route::status(403, "application/vnd.apple.mpegurl", "forbidden"),
    );
    let (result, _) = fixture.run(page("https://page.test/geo")).await;
    assert_code(&result, "playlist_access_denied");
    let report = fixture.last_report.lock().unwrap().clone();
    assert!(report.contains("(refused 1)"), "{report}");
    assert_no_chrome_left(&fixture).await;
    fixture.assert_no_job_dirs();
}

/// The page never arrives in the browser within its budget: a timeout, not
/// "no video".
#[tokio::test]
async fn a_page_that_never_loads_is_a_discovery_timeout() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    fixture.server.route(
        "page.test/slow",
        Route::html(STATIC_PAGE)
            .headless_gets(Route::html(STATIC_PAGE).delayed(Duration::from_secs(45))),
    );
    let (result, _) = fixture.run(page("https://page.test/slow")).await;
    assert_code(&result, "media_discovery_timeout");
    let report = fixture.last_report.lock().unwrap().clone();
    assert!(report.contains("document none"), "{report}");
    assert_no_chrome_left(&fixture).await;
}

/// Incident A2: metadata succeeds, then the host refuses the media itself.
/// The error names the media, not the page, and the report says where.
#[tokio::test]
async fn a_refused_media_file_is_attributed_to_the_download() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // Progressive MP4 on a page (as YouTube serves it) and HLS segments.
    fixture.server.route(
        "page.test/progressive",
        Route::html(
            "<!doctype html><title>Progressive</title>\
             <video src=\"https://media.test/locked/video.mp4\"></video>",
        ),
    );
    fixture.server.route(
        "media.test/locked/video.mp4",
        Route::status(403, "video/mp4", "forbidden"),
    );
    fixture.server.route(
        "media.test/locked/index.m3u8",
        Route::playlist(&media_playlist(&["https://media.test/locked/s0.ts"], "")),
    );
    fixture.server.route(
        "media.test/locked/s0.ts",
        Route::status(403, "video/mp2t", "forbidden"),
    );
    for request in [
        page("https://page.test/progressive"),
        direct("https://media.test/locked/index.m3u8"),
    ] {
        let (result, updates) = fixture.run(request).await;
        assert_code(&result, "media_access_denied");
        assert!(
            updates
                .iter()
                .any(|update| update.stage == Stage::DownloadingVideo)
        );
        let report = fixture.last_report.lock().unwrap().clone();
        assert!(report.contains("during downloadingVideo"), "{report}");
        assert!(report.contains("tool signals: http_403"), "{report}");
        assert!(report.contains("tools: yt-dlp 20"), "{report}");
        for secret in ["locked", "session=secret", "watch/1", "s0.ts"] {
            assert!(!report.contains(secret), "{secret} leaked:\n{report}");
        }
        assert!(fixture.saved_files().is_empty());
        fixture.assert_no_job_dirs();
    }
}

#[tokio::test]
async fn a_rate_limited_playlist_is_reported_as_such() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.server.route(
        "media.test/busy.m3u8",
        Route::status(429, "text/plain", "slow down"),
    );
    let (result, _) = fixture.run(direct("https://media.test/busy.m3u8")).await;
    assert_code(&result, "rate_limited");
    fixture.assert_no_job_dirs();
}

/// yt-dlp can start helpers it finds on `PATH` — `phantomjs` (which ignores
/// the proxy), JavaScript runtimes, FFmpeg. With HighVi's environment it
/// finds none, while extraction itself still works.
#[tokio::test]
async fn yt_dlp_cannot_see_helper_programs() {
    let Some(tools) = tools() else {
        return;
    };
    let job = temp_directory("helpers");
    std::fs::create_dir_all(&job).unwrap();
    crate::ytdlp::prepare_tool_path(&job, find_executable("python3").as_deref())
        .await
        .unwrap();
    let output = Command::new(&tools.yt_dlp)
        .env_clear()
        .envs(crate::ytdlp::ytdlp_env(&job))
        .current_dir(&job)
        .args([
            "-v",
            "--ignore-config",
            "--no-plugin-dirs",
            // Nothing listens here; extraction fails without network use.
            "--proxy",
            "http://127.0.0.1:9",
            "--simulate",
            "--",
            "https://example.invalid/",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[debug] exe versions: none"), "{stderr}");
    assert!(stderr.contains("JS runtimes: none"), "{stderr}");
    assert!(stderr.contains("Unable to connect to proxy"), "{stderr}");
    let _ = std::fs::remove_dir_all(&job);
}

/// yt-dlp also looks for JavaScript runtimes next to its Python interpreter
/// (`sysconfig` scripts: `/usr/local/bin`, a pipx venv's `bin`), not only on
/// `PATH`. A fake `deno` placed there must not be found or run.
#[tokio::test]
async fn yt_dlp_ignores_js_runtimes_next_to_its_interpreter() {
    use std::os::unix::fs::PermissionsExt;
    let Some(tools) = tools() else {
        return;
    };
    let Some(python) = find_executable("python3") else {
        return;
    };
    let root = temp_directory("jsruntime");
    std::fs::create_dir_all(&root).unwrap();
    // Where the yt_dlp package is: an importable module, or a zipapp.
    let located = Command::new(&python)
        .args([
            "-I",
            "-c",
            "import yt_dlp, os; print(os.path.dirname(os.path.dirname(yt_dlp.__file__)))",
        ])
        .output()
        .unwrap();
    let interpreter_of_tool = std::fs::read(&tools.yt_dlp).unwrap();
    let package_path = if located.status.success() {
        PathBuf::from(String::from_utf8_lossy(&located.stdout).trim())
    } else if interpreter_of_tool
        .windows(4)
        .any(|window| window == b"PK\x03\x04")
    {
        tools.yt_dlp.clone()
    } else {
        // pipx: ask the tool's own interpreter.
        let shebang =
            String::from_utf8_lossy(&interpreter_of_tool[..interpreter_of_tool.len().min(256)])
                .into_owned();
        let interpreter = shebang
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("#!")
            .trim()
            .to_string();
        let output = Command::new(&interpreter)
            .args([
                "-I",
                "-c",
                "import yt_dlp, os; print(os.path.dirname(os.path.dirname(yt_dlp.__file__)))",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "cannot locate yt_dlp");
        PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
    };
    let venv = root.join("venv");
    let created = Command::new(&python)
        .args(["-m", "venv", "--without-pip"])
        .arg(&venv)
        .status()
        .unwrap();
    assert!(created.success());
    let marker = root.join("deno-ran");
    let deno = venv.join("bin/deno");
    std::fs::write(
        &deno,
        format!(
            "#!/bin/sh\n/usr/bin/touch '{}'\necho 'deno 2.5.0'\n",
            marker.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&deno, std::fs::Permissions::from_mode(0o755)).unwrap();
    let job = root.join("job");
    std::fs::create_dir_all(&job).unwrap();
    crate::ytdlp::prepare_tool_path(&job, None).await.unwrap();

    let run = |hermetic: bool| {
        let mut command = Command::new(venv.join("bin/python3"));
        command
            .env_clear()
            .envs(crate::ytdlp::ytdlp_env(&job))
            .env("PYTHONPATH", &package_path)
            .current_dir(&job)
            .args(["-m", "yt_dlp", "-v"]);
        if hermetic {
            command.args(crate::ytdlp::YT_DLP_HERMETIC_ARGS);
        } else {
            command.args(["--ignore-config", "--no-plugin-dirs"]);
        }
        let output = command
            .args([
                "--proxy",
                "http://127.0.0.1:9",
                "--simulate",
                "--",
                "https://www.youtube.com/watch?v=aqz-KE-bpKQ",
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stderr)
            .lines()
            .find(|line| line.contains("JS runtimes"))
            .unwrap_or_default()
            .to_string()
    };
    // Control: without HighVi's arguments, yt-dlp does find the fake.
    let control = run(false);
    assert!(
        control.contains("deno"),
        "control did not find the fake runtime: {control}"
    );
    assert!(marker.exists(), "control did not run the fake runtime");
    std::fs::remove_file(&marker).unwrap();
    let hermetic = run(true);
    assert!(hermetic.contains("JS runtimes: none"), "{hermetic}");
    assert!(!marker.exists(), "the fake deno was run");
    let _ = std::fs::remove_dir_all(&root);
}

// ------------------------------------------------- stalls and disk (V1.1 B)

/// One-segment playlist at `media.test/<name>.m3u8` whose segment is the
/// real muxed fixture segment, served by `route`.
fn single_segment(fixture: &Fixture, name: &str, route: impl FnOnce(Vec<u8>) -> Route) {
    let segment = std::fs::read(media(&fixture.tools.ffmpeg).join("muxed/s0.ts")).unwrap();
    fixture
        .server
        .route(&format!("media.test/{name}/s0.ts"), route(segment));
    fixture.server.route(
        &format!("media.test/{name}.m3u8"),
        Route::playlist(&media_playlist(
            &[&format!("https://media.test/{name}/s0.ts")],
            "",
        )),
    );
}

fn quick_limits(environment: &mut JobEnvironment, window: Duration, min_bytes: u64) {
    environment.limits.stall = crate::process::StallPolicy { window, min_bytes };
}

#[tokio::test]
async fn stalled_and_trickling_servers_are_stopped_and_cleaned_up() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // No answer at all for a minute, and a byte every 300 ms.
    single_segment(&fixture, "silent", |body| {
        Route::ok("video/mp2t", body).delayed(Duration::from_secs(60))
    });
    single_segment(&fixture, "trickle", |body| {
        Route::ok("video/mp2t", body).trickled(1, Duration::from_millis(300))
    });
    for name in ["silent", "trickle"] {
        let mut environment = fixture.environment();
        quick_limits(&mut environment, Duration::from_secs(4), 16 * 1024);
        let started = Instant::now();
        let (result, _) = fixture
            .run_with(
                &environment,
                direct(&format!("https://media.test/{name}.m3u8")),
                None,
            )
            .await;
        assert_code(&result, "download_stalled");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{name}: {:?}",
            started.elapsed()
        );
        assert!(fixture.saved_files().is_empty());
        fixture.assert_no_job_dirs();
    }
}

#[tokio::test]
async fn a_slow_but_steady_server_is_not_a_stall() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // 2 KiB every 0.25 s: the 106 KB segment takes ~13 s, over two 5 s
    // windows, so this passes only if progress inside a fragment counts.
    // The first window also absorbs yt-dlp's start-up on a busy machine.
    single_segment(&fixture, "steady", |body| {
        Route::ok("video/mp2t", body).trickled(2 * 1024, Duration::from_millis(250))
    });
    let mut environment = fixture.environment();
    quick_limits(&mut environment, Duration::from_secs(5), 8 * 1024);
    let (result, _) = fixture
        .run_with(&environment, direct("https://media.test/steady.m3u8"), None)
        .await;
    let outcome = result.unwrap();
    let (streams, _, clean) = fixture.inspect(&outcome.saved_to);
    assert_eq!(streams, ["video", "audio"]);
    assert!(clean);
}

#[tokio::test]
async fn a_download_over_its_deadline_is_stopped() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    single_segment(&fixture, "long", |body| {
        Route::ok("video/mp2t", body).trickled(4 * 1024, Duration::from_millis(100))
    });
    let mut environment = fixture.environment();
    environment.limits.track_deadline = Duration::from_secs(2);
    let (result, _) = fixture
        .run_with(&environment, direct("https://media.test/long.m3u8"), None)
        .await;
    assert_code(&result, "download_timeout");
    fixture.assert_no_job_dirs();
}

#[tokio::test]
async fn too_little_free_space_refuses_before_and_stops_during_a_download() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    // Before: less free space than the floor HighVi keeps.
    single_segment(&fixture, "space", |body| {
        Route::ok("video/mp2t", body).trickled(8 * 1024, Duration::from_millis(200))
    });
    let mut environment = fixture.environment();
    environment.free_space = Some(Arc::new(|| Some(100 * 1024 * 1024)));
    let (result, _) = fixture
        .run_with(&environment, direct("https://media.test/space.m3u8"), None)
        .await;
    assert_code(&result, "insufficient_disk_space");
    assert!(
        !fixture
            .server
            .hits()
            .iter()
            .any(|hit| hit.path == "/space/s0.ts"),
        "media was fetched although it could not fit"
    );

    // During: plenty of space at the start, then the disk fills up.
    let calls = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter = Arc::clone(&calls);
    environment.free_space = Some(Arc::new(move || {
        let call = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(if call == 0 {
            100 * 1024 * 1024 * 1024
        } else {
            1024
        })
    }));
    let started = Instant::now();
    let (result, updates) = fixture
        .run_with(&environment, direct("https://media.test/space.m3u8"), None)
        .await;
    assert_code(&result, "insufficient_disk_space");
    assert!(
        updates
            .iter()
            .any(|update| update.stage == Stage::DownloadingVideo)
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "{:?}",
        started.elapsed()
    );
    assert!(fixture.saved_files().is_empty());
    fixture.assert_no_job_dirs();
}

/// A hostile page floods discovery: thousands of requests with long URLs and
/// multi-megabyte events. HighVi stays within its bounds and says so,
/// instead of reporting "no video" as if it had seen everything.
#[tokio::test]
async fn a_request_flood_is_reported_as_overload() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    fixture.server.route(
        "page.test/flood",
        Route::html(
            "<!doctype html><title>Flood</title><script>\
             const pad = 'x'.repeat(7000);\
             for (let i = 0; i < 3000; i++) fetch('https://ads.test/' + i + '?' + pad).catch(() => {});\
             const big = 'data:image/png;base64,' + 'A'.repeat(2 * 1024 * 1024);\
             for (let i = 0; i < 20; i++) { const image = new Image(); image.src = big + i; }\
             </script>",
        ),
    );
    let started = Instant::now();
    let (result, _) = fixture.run(page("https://page.test/flood")).await;
    assert_code(&result, "discovery_overloaded");
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );
    let report = fixture.last_report.lock().unwrap().clone();
    assert!(!report.contains("untracked requests 0,"), "{report}");
    assert_no_chrome_left(&fixture).await;
    fixture.assert_no_job_dirs();
}

/// Measurement, not a pass/fail check: peak memory of the HighVi process
/// while a hostile page floods discovery. Run alone:
/// `cargo test -p highvi-core flood_peak_memory -- --ignored --nocapture`
#[tokio::test]
#[ignore = "measurement; run alone"]
async fn flood_peak_memory() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    fixture.server.route(
        "page.test/flood",
        Route::html(
            "<!doctype html><title>Flood</title><script>\
             const pad = 'x'.repeat(7000);\
             for (let i = 0; i < 3000; i++) fetch('https://ads.test/' + i + '?' + pad).catch(() => {});\
             const big = 'data:image/png;base64,' + 'A'.repeat(2 * 1024 * 1024);\
             for (let i = 0; i < 40; i++) { const image = new Image(); image.src = big + i; }\
             </script>",
        ),
    );
    let (result, _) = fixture.run(page("https://page.test/flood")).await;
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let peak = status
        .lines()
        .find(|line| line.starts_with("VmHWM"))
        .unwrap_or_default();
    eprintln!(
        "flood result: {:?}; {peak}",
        result.err().map(|error| error.code)
    );
    eprintln!("{}", fixture.last_report.lock().unwrap());
}

// ------------------------------------------------------ bulk queue (V1.1 C)

/// Records queue events; the e2e tests only need the last snapshot.
#[derive(Default)]
struct QueueLog(Mutex<Option<crate::QueueSnapshot>>);

impl crate::QueueEvents for QueueLog {
    fn changed(&self, snapshot: crate::QueueSnapshot) {
        *self.0.lock().unwrap() = Some(snapshot);
    }
    fn progress(&self, _: crate::ProgressEvent) {}
}

/// A scheduler running real jobs against the fixture, sharing one set of
/// limits, as the app does.
fn real_scheduler(fixture: &Fixture) -> (crate::Scheduler, Arc<QueueLog>) {
    let template = fixture.environment();
    let shared = Arc::new(crate::SharedResources::default());
    let environment = Arc::new(JobEnvironment {
        shared,
        fixture: template.fixture.clone(),
        ..JobEnvironment::new(
            template.yt_dlp.clone(),
            template.ffmpeg.clone(),
            template.chrome.clone(),
            template.output_dir.clone(),
        )
    });
    let runner: crate::Runner = Arc::new(move |source, cancel, report| {
        let environment = Arc::clone(&environment);
        Box::pin(async move {
            let (result, diagnostics) =
                run_job(&environment, &source, cancel, &|update| report(update)).await;
            let rendered = result.is_err().then(|| diagnostics.render());
            (result, rendered)
        })
    });
    let log = Arc::new(QueueLog::default());
    let scheduler = crate::Scheduler::new(
        tokio::runtime::Handle::current(),
        runner,
        Arc::clone(&log) as Arc<dyn crate::QueueEvents>,
    );
    (scheduler, log)
}

/// Four real jobs at once whose videos share a title: each is published
/// under its own name and nothing is overwritten.
#[tokio::test]
async fn four_concurrent_jobs_with_one_title_publish_four_files() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    for index in 0..5 {
        fixture.server.route(
            &format!("page.test/same/{index}"),
            Route::html(
                "<!doctype html><title>Same title</title>\
                 <video src=\"https://media.test/muxed/master.m3u8\" type=\"application/x-mpegURL\"></video>",
            ),
        );
    }
    std::fs::create_dir_all(&fixture.output_dir).unwrap();
    // An existing file with a name one of the jobs wants.
    std::fs::write(fixture.output_dir.join("Same title (1).mp4"), b"keep me").unwrap();
    let (scheduler, _) = real_scheduler(&fixture);
    scheduler.set_concurrency(4).unwrap();
    let report = scheduler.add(
        (0..5)
            .map(|index| page(&format!("https://page.test/same/{index}")))
            .collect(),
    );
    assert_eq!(report.added.len(), 5);
    scheduler.start();
    let mut peak = 0;
    let started = Instant::now();
    loop {
        let snapshot = scheduler.snapshot();
        // Each running job has its own working directory: count them, so
        // the overlap is measured, not inferred from the slot count.
        let working = std::fs::read_dir(&fixture.output_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".highvi-job-")
                    })
                    .count()
            })
            .unwrap_or(0);
        peak = peak.max(working);
        assert!(snapshot.active <= 4);
        if snapshot.counts.completed == 5 {
            break;
        }
        assert!(snapshot.counts.failed == 0, "{snapshot:?}");
        assert!(started.elapsed() < Duration::from_secs(90), "{snapshot:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(peak, 4, "four jobs never ran together");
    let files = fixture.saved_files();
    assert_eq!(files.len(), 6, "{files:?}");
    assert_eq!(
        std::fs::read(fixture.output_dir.join("Same title (1).mp4")).unwrap(),
        b"keep me"
    );
    for name in files
        .iter()
        .filter(|name| name.as_str() != "Same title (1).mp4")
    {
        let (streams, _, clean) = fixture.inspect(&fixture.output_dir.join(name));
        assert_eq!(streams, ["video", "audio"], "{name}");
        assert!(clean, "{name}");
    }
    fixture.assert_no_job_dirs();
}

/// Browser discovery is limited to one Chrome at a time, whatever the
/// number of jobs; a job waiting for the browser can still be cancelled.
#[tokio::test]
async fn jobs_share_one_browser_at_a_time() {
    let Some(fixture) = browser_fixture().await else {
        return;
    };
    for index in 0..3 {
        fixture.server.route(
            &format!("page.test/js/{index}"),
            Route::html(&format!(
                "<!doctype html><title>JS {index}</title><script>\
                 setTimeout(() => fetch('https://media.test/js{index}/master.m3u8'), 5000);\
                 </script>"
            )),
        );
        fixture.server.route(
            &format!("media.test/js{index}/master.m3u8"),
            Route::playlist(SPLIT_MASTER_ABSOLUTE),
        );
    }
    let (scheduler, _) = real_scheduler(&fixture);
    scheduler.set_concurrency(4).unwrap();
    let added = scheduler.add(
        (0..3)
            .map(|index| page(&format!("https://page.test/js/{index}")))
            .collect(),
    );
    assert_eq!(added.added.len(), 3);
    scheduler.start();
    // Sample the number of browser processes while the jobs run.
    let mut most = 0;
    let started = Instant::now();
    let mut cancelled_waiting = false;
    let mut waiting_id = None;
    let mut holder = None;
    let mut waiting_ended_while_browser_busy = false;
    loop {
        let browsers = chrome_browser_processes(&fixture);
        most = most.max(browsers);
        let snapshot = scheduler.snapshot();
        // Cancel a job that is waiting for the browser while another one
        // uses it: it must end at once, not after its turn comes.
        if !cancelled_waiting
            && let Some(browsing) = snapshot
                .items
                .iter()
                .find(|item| item.stage == Some(Stage::Browsing))
            && let Some(waiting) = snapshot
                .items
                .iter()
                .find(|item| item.stage == Some(Stage::WaitingForBrowser))
        {
            holder = Some(browsing.id);
            waiting_id = Some(waiting.id);
            scheduler.cancel(waiting.id);
            cancelled_waiting = true;
        }
        // The waiting job must be gone while the job that held the browser
        // when it was cancelled is still using it.
        if let (Some(waiting_id), Some(holder)) = (waiting_id, holder)
            && !waiting_ended_while_browser_busy
            && snapshot
                .items
                .iter()
                .any(|item| item.id == waiting_id && item.status == crate::ItemStatus::Cancelled)
        {
            waiting_ended_while_browser_busy = snapshot
                .items
                .iter()
                .any(|item| item.id == holder && item.stage == Some(Stage::Browsing));
        }
        let done = snapshot.counts.completed + snapshot.counts.failed + snapshot.counts.cancelled;
        if done == 3 && snapshot.active == 0 {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(120), "{snapshot:?}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let snapshot = scheduler.snapshot();
    assert_eq!(most, 1, "more than one browser ran at once");
    assert!(
        waiting_ended_while_browser_busy,
        "the job waiting for the browser was not cancelled while waiting"
    );
    assert_eq!(snapshot.counts.completed, 2, "{snapshot:?}");
    assert_eq!(snapshot.counts.cancelled, 1, "{snapshot:?}");
    assert_no_chrome_left(&fixture).await;
    fixture.assert_no_job_dirs();
}

/// Browsers running at once: distinct profiles (one per launch). Counting
/// processes would also count snap's `snap-confine` and the helper it forks
/// while starting, and snap Chromium's renderers, which repeat the flags.
fn chrome_browser_processes(fixture: &Fixture) -> usize {
    chrome_survivors(fixture)
        .iter()
        .filter(|cmdline| cmdline.contains("--remote-debugging-pipe"))
        .filter_map(|cmdline| {
            let profile = cmdline.split("--user-data-dir=").nth(1)?;
            Some(profile.split(" --").next().unwrap_or(profile).to_string())
        })
        .collect::<std::collections::HashSet<_>>()
        .len()
}

/// Measurement, not a pass/fail check: HighVi's own cost at each target
/// from 1 to 4 (8 jobs of ~6 MB each from the local fixture), and how long
/// Cancel All takes with four transfers in flight. Run alone:
/// `cargo test -p highvi-core queue_concurrency_profile -- --ignored --nocapture`
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; run alone"]
async fn queue_concurrency_profile() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let segments: Vec<String> = (0..60)
        .map(|_| "https://media.test/muxed/s0.ts".to_string())
        .collect();
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    for index in 0..8 {
        fixture.server.route(
            &format!("media.test/profile/{index}.m3u8"),
            Route::playlist(&media_playlist(&refs, "")),
        );
    }
    let children = || {
        let me = std::process::id().to_string();
        let mut count = 0;
        for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            if let Ok(stat) = std::fs::read_to_string(entry.path().join("stat"))
                && let Some(rest) = stat.rsplit(')').next()
                && rest.split_whitespace().nth(1) == Some(me.as_str())
            {
                count += 1;
            }
        }
        count
    };
    let status_value = |key: &str| -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .unwrap_or_default()
            .lines()
            .find(|line| line.starts_with(key))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    };
    eprintln!("target | wall s | peak active | peak threads | peak fds | peak children | RSS KiB");
    for target in 1..=4 {
        let (scheduler, _) = real_scheduler(&fixture);
        scheduler.set_concurrency(target).unwrap();
        let sources = (0..8)
            .map(|index| SourceRequest::DirectHls {
                playlist_url: format!("https://media.test/profile/{index}.m3u8?run={target}"),
                referer_page_url: "https://page.test/".into(),
            })
            .collect();
        assert_eq!(scheduler.add(sources).added.len(), 8);
        let started = Instant::now();
        scheduler.start();
        let (mut active, mut threads, mut fds, mut kids) = (0, 0, 0, 0);
        loop {
            let snapshot = scheduler.snapshot();
            active = active.max(snapshot.active);
            threads = threads.max(status_value("Threads:"));
            fds = fds.max(std::fs::read_dir("/proc/self/fd").map_or(0, |dir| dir.count()));
            kids = kids.max(children());
            if snapshot.counts.completed + snapshot.counts.failed == 8 {
                assert_eq!(snapshot.counts.completed, 8, "{snapshot:?}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        eprintln!(
            "{target} | {:.1} | {active} | {threads} | {fds} | {kids} | {}",
            started.elapsed().as_secs_f64(),
            status_value("VmRSS:")
        );
        for name in fixture.saved_files() {
            let _ = std::fs::remove_file(fixture.output_dir.join(name));
        }
    }
    // Cancel All with four slow transfers in flight.
    let slow: Vec<String> = (0..4)
        .map(|index| format!("https://media.test/slowp/{index}.ts"))
        .collect();
    for (index, url) in slow.iter().enumerate() {
        let segment = std::fs::read(media(&fixture.tools.ffmpeg).join("muxed/s0.ts")).unwrap();
        fixture.server.route(
            &format!("media.test/slowp/{index}.ts"),
            Route::ok("video/mp2t", segment).trickled(1024, Duration::from_millis(200)),
        );
        fixture.server.route(
            &format!("media.test/slowp/{index}.m3u8"),
            Route::playlist(&media_playlist(&[url.as_str()], "")),
        );
    }
    let (scheduler, _) = real_scheduler(&fixture);
    scheduler.set_concurrency(4).unwrap();
    scheduler.add(
        (0..4)
            .map(|index| direct(&format!("https://media.test/slowp/{index}.m3u8")))
            .collect(),
    );
    scheduler.start();
    for index in 0..4 {
        fixture
            .server
            .hits_log
            .wait_for(&format!("/slowp/{index}.ts"))
            .await;
    }
    let cancelled = Instant::now();
    scheduler.cancel_all();
    while scheduler.snapshot().active > 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    eprintln!(
        "cancel all (4 in flight) -> all slots free: {} ms; children left: {}",
        cancelled.elapsed().as_millis(),
        children()
    );
    fixture.assert_no_job_dirs();
}

/// Ubuntu 24.04's APT yt-dlp (2024.04.09) does not know options HighVi's
/// isolation needs. It is identified, and jobs fail with advice to update
/// instead of a generic yt-dlp failure.
#[tokio::test]
async fn a_too_old_yt_dlp_is_named_and_refused() {
    use std::os::unix::fs::PermissionsExt;
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let directory = temp_directory("old-ytdlp");
    std::fs::create_dir_all(&directory).unwrap();
    let fake = directory.join("yt-dlp");
    std::fs::write(
        &fake,
        "#!/bin/sh\n\
         for arg in \"$@\"; do case \"$arg\" in --no-plugin-dirs|--no-js-runtimes)\n\
           echo 'Usage: yt-dlp [OPTIONS] URL [URL...]' >&2\n\
           echo \"yt-dlp: error: no such option: $arg\" >&2; exit 2;; esac; done\n\
         for arg in \"$@\"; do [ \"$arg\" = --version ] && { echo 2024.04.09; exit 0; }; done\n\
         exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        crate::yt_dlp_version(&fake, None)
            .await
            .map(|version| version.to_string()),
        Some("2024.04.09".to_string())
    );
    let mut environment = fixture.environment();
    environment.yt_dlp = fake;
    let (result, _) = fixture
        .run_with(&environment, page("https://page.test/anything"), None)
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.code, "ytdlp_too_old");
    assert_eq!(
        error.hint,
        Some(crate::Hint::UpdateYtDlp {
            installed: "2024.04.09".into(),
            minimum: Some("2025.11.12".into())
        })
    );
    assert!(
        !fixture
            .server
            .hits()
            .iter()
            .any(|hit| hit.host == "page.test")
    );
    fixture.assert_no_job_dirs();
    let _ = std::fs::remove_dir_all(&directory);
}

/// Output that only imitates yt-dlp's usage error (a site's text on a line
/// of its own) does not make a current yt-dlp "too old", which would pause
/// the whole queue: the installed version decides.
#[tokio::test]
async fn an_imitated_usage_error_does_not_mean_too_old() {
    use std::os::unix::fs::PermissionsExt;
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let directory = temp_directory("forged-ytdlp");
    std::fs::create_dir_all(&directory).unwrap();
    let fake = directory.join("yt-dlp");
    std::fs::write(
        &fake,
        "#!/bin/sh\n\
         for arg in \"$@\"; do [ \"$arg\" = --version ] && { echo 2026.09.01; exit 0; }; done\n\
         echo 'ERROR: [generic] Unable to download: the site says' >&2\n\
         echo 'yt-dlp: error: no such option: --no-plugin-dirs' >&2\n\
         exit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut environment = fixture.environment();
    environment.yt_dlp = fake;
    let (result, _) = fixture
        .run_with(&environment, page("https://page.test/anything"), None)
        .await;
    let error = result.unwrap_err();
    assert_ne!(error.code, "ytdlp_too_old");
    fixture.assert_no_job_dirs();
    let _ = std::fs::remove_dir_all(&directory);
}
