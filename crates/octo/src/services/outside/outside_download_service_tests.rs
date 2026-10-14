//! Tests of the download service: the preview stream and the YouTube copy through the real
//! service against a fake yt-dlp shim (a wiremock responder that keeps the fake's state), and
//! the ways a download cannot be made as asked.

use std::sync::Arc;
use std::time::Duration;

use octo_core::models::domain::Song;
use octo_core::outside::{OutsideRouting, RoutingKind};
use octo_core::settings::{AppSettings, DownloadSettings, DownloadSource, SettingsStore, SubsonicSettings};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::common::{DownloadConcurrency, DownloadCore, DownloadServices};
use crate::services::fingerprint::{AcoustIdClient, AcoustIdRateLimitHandler, AcoustIdRateLimiter};
use crate::services::i_download_service::IDownloadService;
use crate::services::local::DownloadHistoryService;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::notifications::NotificationService;
use crate::services::subsonic::NavidromeIdentityService;
use octo_media::audio::AudioFingerprinter;

/// A folder under the temp directory, removed when the test ends.
struct Root(tempfile::TempDir);

impl Root {
    fn new(prefix: &str) -> Self {
        Root(
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir()
                .expect("a temp folder"),
        )
    }

    fn path(&self) -> String {
        self.0.path().to_string_lossy().into_owned()
    }
}

#[derive(Default)]
struct ShimState {
    /// What the yt-dlp shim was asked to fetch.
    you_tube_downloads: Vec<String>,
    /// The rows YouTube Music answers a songs search with; none answers 404.
    ytm_tracks: Option<Value>,
}

/// The yt-dlp shim as far as a download needs: one search answer, one stream, and every
/// download written straight to its destination.
#[derive(Clone)]
struct FakeShim {
    state: Arc<Mutex<ShimState>>,
}

fn json_answer(body: &Value, status: u16) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.to_string().into_bytes(), "application/json")
}

impl Respond for FakeShim {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        if path == "/ytm/search" {
            return match self.state.lock().ytm_tracks.clone() {
                Some(tracks) => json_answer(&json!({"tracks": tracks}), 200),
                None => ResponseTemplate::new(404),
            };
        }
        if path == "/search" {
            return json_answer(
                &json!({"video_id": "vid123", "title": "Started", "duration": 180,
                        "size": 12000, "itag": "140", "ext": "m4a", "abr": 129.5}),
                200,
            );
        }
        if path == "/stream" {
            return ResponseTemplate::new(206)
                .insert_header("content-range", "bytes 0-1/12345")
                .set_body_raw(vec![1u8, 2], "audio/mp4");
        }
        if path == "/download" {
            let dest = request
                .url
                .query_pairs()
                .find(|(k, _)| k == "dest")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            let file = format!("{dest}.mp3");
            std::fs::create_dir_all(std::path::Path::new(&file).parent().expect("a parent")).expect("made");
            std::fs::write(&file, crate::services::test_support::mp3()).expect("written");
            self.state.lock().you_tube_downloads.push(file.clone());
            return json_answer(&json!({"path": file}), 200);
        }
        json_answer(&json!({}), 200)
    }
}

struct Built {
    service: Arc<BaseDownloadService>,
    registry: Arc<ExternalIdRegistry>,
    shim: FakeShim,
    songs: Vec<Song>,
    _server: MockServer,
}

const TITLES: [(&str, i32); 2] = [("Intro", 90), ("Started", 180)];

async fn build(root: &Root, source: DownloadSource) -> Built {
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let songs: Vec<Song> = TITLES
        .iter()
        .enumerate()
        .map(|(i, (title, seconds))| {
            let id = registry.register(OutsideRouting {
                kind: RoutingKind::Song,
                artist: Some("Drake".into()),
                title: Some(title.to_string()),
                album: Some("Nothing Was the Same".into()),
                duration: Some(*seconds),
                track: Some(i as i32 + 1),
                ..Default::default()
            });
            Song {
                id: format!("ext-outside-{id}"),
                external_provider: Some("outside".into()),
                external_id: Some(id),
                title: title.to_string(),
                artist: "Drake".into(),
                album: "Nothing Was the Same".into(),
                duration: Some(*seconds),
                track: Some(i as i32 + 1),
                ..Default::default()
            }
        })
        .collect();

    let shim = FakeShim {
        state: Arc::new(Mutex::new(ShimState::default())),
    };
    let server = MockServer::builder().start().await;
    Mock::given(any()).respond_with(shim.clone()).mount(&server).await;

    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        downloads: DownloadSettings {
            detect_transcodes: false,
            verify_downloads: false,
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            auto_detect_download_path: false,
            download_source: source,
            ..Default::default()
        },
        ..Default::default()
    }));
    settings.set_raw("Library:DownloadPath", Some(&root.path()));

    let metadata = FakeMetadata::default();
    // A song whose id the registry never minted: nothing says what to search for.
    metadata.songs.lock().insert(
        ("outside".into(), "unknown-id".into()),
        Song {
            title: "Mystery".into(),
            artist: "Nobody".into(),
            external_provider: Some("outside".into()),
            external_id: Some("unknown-id".into()),
            ..Default::default()
        },
    );
    for song in &songs {
        metadata.songs.lock().insert(
            ("outside".into(), song.external_id.clone().expect("an id")),
            song.clone(),
        );
    }

    let core = DownloadCore {
        settings: Arc::clone(&settings),
        local_library: Arc::new(FakeLocalLibrary::default()),
        metadata: Arc::new(metadata),
        navidrome_identity: NavidromeIdentityService::new(Arc::clone(&settings), reqwest::Client::new()),
        history: Arc::new(DownloadHistoryService::new(root.0.path().join("history.json"))),
        notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
    };
    let services = DownloadServices {
        concurrency: Some(Arc::new(DownloadConcurrency::new(Arc::clone(&settings)))),
        ..Default::default()
    };
    let verification = Arc::new(DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        Arc::new(AcoustIdClient::new(Arc::new(AcoustIdRateLimitHandler::new(
            Arc::new(AcoustIdRateLimiter::new()),
        )))),
        Arc::clone(&settings),
        None,
        None,
    ));
    let parts = OutsideDownloadParts {
        verification,
        youtube: Arc::new(YouTubeResolver::with_base_url(Some(&server.uri()))),
        id_registry: registry.clone(),
        lidarr_imports: None,
        lidarr_fetcher: None,
    };
    let (service, _) = OutsideDownloadService::build(core, services, parts);
    Built {
        service,
        registry,
        shim,
        songs,
        _server: server,
    }
}

async fn acquire(built: &Built, external_id: &str, source: Option<DownloadSource>) -> anyhow::Result<String> {
    tokio::time::timeout(
        Duration::from_secs(60),
        built.service.execute_acquisition(
            "outside",
            external_id,
            false,
            true,
            source,
            &CancellationToken::new(),
            None,
            false,
            None,
        ),
    )
    .await
    .expect("within a minute")
}

async fn asked_paths(built: &Built) -> Vec<String> {
    built
        ._server
        .received_requests()
        .await
        .expect("recording")
        .iter()
        .map(|r| r.url.path().to_string())
        .collect()
}

#[test]
fn a_missing_artist_or_title_is_an_invalid_operation() {
    let error: anyhow::Error = InvalidOperationException("Cannot download".into()).into();
    assert!(error.downcast_ref::<InvalidOperationException>().is_some());
    assert_eq!(error.to_string(), "Cannot download");
}

#[tokio::test]
async fn a_song_without_a_routing_is_an_invalid_operation() {
    let root = Root::new("octo-attempts-");
    let built = build(&root, DownloadSource::YouTube).await;

    let error = acquire(&built, "unknown-id", None)
        .await
        .expect_err("nothing to search for");

    assert!(
        error.downcast_ref::<InvalidOperationException>().is_some(),
        "{error:#}"
    );
    assert_eq!(
        error.to_string(),
        "Cannot download 'Nobody - Mystery': missing artist/title in external id"
    );
}

/// Lidarr is only ever asked for by name, and without it the download says so instead of
/// falling back to a copy nobody asked for.
#[tokio::test]
async fn lidarr_asked_for_by_name_without_lidarr_is_an_invalid_operation() {
    let root = Root::new("octo-attempts-");
    let built = build(&root, DownloadSource::YouTube).await;

    let error = acquire(
        &built,
        built.songs[1].external_id.as_deref().expect("an id"),
        Some(DownloadSource::Lidarr),
    )
    .await
    .expect_err("no Lidarr here");

    assert_eq!(error.to_string(), "Lidarr is not available on this server.");
    assert!(built.shim.state.lock().you_tube_downloads.is_empty());
}

/// The copy is the YouTube MP3, whatever the legacy source says, since Lidarr is asked for by
/// name only.
#[tokio::test]
async fn a_download_lands_the_youtube_mp3() {
    for source in [DownloadSource::YouTube, DownloadSource::Lidarr] {
        let root = Root::new("octo-attempts-");
        let built = build(&root, source).await;

        let path = acquire(
            &built,
            built.songs[1].external_id.as_deref().expect("an id"),
            None,
        )
        .await
        .expect("the YouTube copy lands");

        assert!(path.ends_with(".mp3"), "{path}");
        assert_eq!(built.shim.state.lock().you_tube_downloads.len(), 1);
        let asked = asked_paths(&built).await;
        assert!(asked.iter().any(|p| p == "/download"), "{asked:?}");
    }
}

#[test]
fn the_played_size_is_the_whole_body_or_the_total_after_the_slash() {
    assert_eq!(played_size(200, Some(6_755_585), None), Some(6_755_585));
    assert_eq!(played_size(206, Some(2), Some("bytes 0-1/12345")), Some(12_345));
    assert_eq!(played_size(416, None, Some("bytes */12345")), Some(12_345));
    assert_eq!(played_size(206, Some(2), Some("bytes 0-1/*")), None);
    assert_eq!(played_size(206, Some(2), None), None);
    assert_eq!(played_size(502, Some(10), None), None);
}

#[tokio::test]
async fn a_play_pins_the_found_video_and_learns_its_true_size_from_the_stream() {
    let root = Root::new("octo-stream-");
    let built = build(&root, DownloadSource::YouTube).await;
    let id = built.songs[0].external_id.clone().expect("an id");

    let stream = built
        .service
        .get_direct_stream("outside", &id, Some("bytes=0-1"), &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");
    assert_eq!(stream.status_code, 206);

    let routing = built.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("vid123"));
    // The search said 12000; the bytes on their way say 12345, and they win.
    let facts = built.registry.stream_facts(&id).expect("facts");
    assert_eq!(facts.size, Some(12_345));
    assert_eq!(facts.itag.as_deref(), Some("140"));
    assert_eq!(facts.bit_rate, Some(130));
}

#[tokio::test]
async fn a_play_takes_youtube_musics_video_for_the_song_before_the_yt_dlp_search() {
    let root = Root::new("octo-stream-");
    let built = build(&root, DownloadSource::YouTube).await;
    let song = built.songs[0].clone();
    let id = song.external_id.clone().expect("an id");
    let length = built.registry.lookup(&id).expect("known").snapshot().duration;
    built.shim.state.lock().ytm_tracks = Some(json!([{
        "videoId": "ytm-atv", "title": song.title, "artists": [song.artist],
        "durationSeconds": length.unwrap_or(200), "videoType": "MUSIC_VIDEO_TYPE_ATV",
    }]));

    built
        .service
        .get_direct_stream("outside", &id, Some("bytes=0-1"), &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");

    let routing = built.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("ytm-atv"));
    assert!(routing.shown_duration.is_some());
    let asked = asked_paths(&built).await;
    assert!(asked.iter().any(|p| p == "/ytm/search"), "{asked:?}");
    assert!(!asked.iter().any(|p| p == "/search"), "{asked:?}");
    // The stream opened is the picked video.
    assert!(
        built
            ._server
            .received_requests()
            .await
            .expect("recording")
            .iter()
            .any(|r| r.url.path() == "/stream" && r.url.query().unwrap_or("").contains("id=ytm-atv"))
    );
}

#[tokio::test]
async fn a_play_falls_back_to_the_yt_dlp_search_when_youtube_music_has_no_such_song() {
    let root = Root::new("octo-stream-");
    let built = build(&root, DownloadSource::YouTube).await;
    let id = built.songs[0].external_id.clone().expect("an id");
    built.shim.state.lock().ytm_tracks = Some(json!([{
        "videoId": "other", "title": "Another Song", "artists": ["Someone Else"],
        "durationSeconds": 200, "videoType": "MUSIC_VIDEO_TYPE_ATV",
    }]));

    built
        .service
        .get_direct_stream("outside", &id, Some("bytes=0-1"), &CancellationToken::new())
        .await
        .expect("no error")
        .expect("a stream");

    let routing = built.registry.lookup(&id).expect("known").snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("vid123"));
}

/// Octo's own staging is the one folder a finished download is never taken from.
#[test]
fn the_staging_folder_is_kept_out() {
    assert_eq!(excluded_folder_names(), [INCOMING_FOLDER_NAME]);
}
