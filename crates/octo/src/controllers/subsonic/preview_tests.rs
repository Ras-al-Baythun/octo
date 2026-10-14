//! getPreviewChoices and setPreview: the videos a song from outside the library could play
//! from, and the pick that every later play reads; version 2, the same for a song that is a
//! file in the library, whose pick a replace downloads.

use octo_core::models::download::DownloadHistoryEntry;
use octo_core::outside::{OutsideRouting, RoutingKind};
use octo_core::settings::{AppSettings, SubsonicSettings};
use serde_json::{Value, json};
use wiremock::matchers::{any, method, path, query_param};
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::route_tests::{app, get};
use super::test_app::{navidrome_json, query_of};
use crate::app::{AppState, TestServices};
use crate::services::library::library_song_routing::routing_for_library_song;
use crate::services::outside::ExternalIdRegistry;

/// The library song: Navidrome's id, its path under the music folder, and its bytes.
const LIBRARY_ID: &str = "nd-passionfruit";
const LIBRARY_PATH: &str = "Drake/More Life/03 - Passionfruit.flac";
const LIBRARY_BYTES: &[u8] = b"fLaC not really, but the right size";

struct Fixture {
    _navidrome: MockServer,
    _shim: MockServer,
    _directory: tempfile::TempDir,
    state: AppState,
    app: crate::http::pipeline::App,
}

/// Navidrome as far as these calls need it: every sign-in accepted, an admin login for Octo's
/// own identity, one song the native API knows (so the resolver can prove its file), and
/// getSong for that song as Navidrome answers it.
struct LibraryNavidrome {
    library_path: String,
}

impl Respond for LibraryNavidrome {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let path = request.url.path();
        if path == "/auth/login" {
            return navidrome_json(
                r#"{"token":"admin-jwt","isAdmin":true,"username":"admin","subsonicToken":"st","subsonicSalt":"ss"}"#,
            );
        }
        if path == format!("/api/song/{LIBRARY_ID}") {
            return navidrome_json(
                &json!({
                    "id": LIBRARY_ID, "path": LIBRARY_PATH, "libraryPath": self.library_path,
                    "size": LIBRARY_BYTES.len(), "suffix": "flac", "title": "Passionfruit",
                    "artist": "Drake", "album": "More Life", "duration": 299,
                })
                .to_string(),
            );
        }
        if path.starts_with("/api/song/") {
            return ResponseTemplate::new(404);
        }
        if path.ends_with("/rest/getSong") {
            let id = query_of(request).get("id").cloned().unwrap_or_default();
            if id != LIBRARY_ID {
                return navidrome_json(
                    r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"Song not found"}}}"#,
                );
            }
            return navidrome_json(
                &json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "type": "navidrome", "song": {
                    "id": LIBRARY_ID, "title": "Passionfruit", "artist": "Drake", "album": "More Life",
                    "duration": 299, "path": LIBRARY_PATH,
                }}})
                .to_string(),
            );
        }
        navidrome_json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#)
    }
}

/// A shim whose YouTube Music knows the song's release and whose YouTube lists its video first
/// as well as that release, and a Navidrome that accepts every sign-in and holds one library
/// song, Drake's Passionfruit, whose file is in a temporary music folder.
async fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("a temp dir");
    let song = directory.path().join(LIBRARY_PATH);
    std::fs::create_dir_all(song.parent().expect("a folder")).expect("folders");
    std::fs::write(&song, LIBRARY_BYTES).expect("written");
    let navidrome = MockServer::start().await;
    Mock::given(any())
        .respond_with(LibraryNavidrome {
            library_path: directory.path().to_string_lossy().into_owned(),
        })
        .mount(&navidrome)
        .await;
    let shim = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ytm/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"tracks": [
            {"videoId": "ytm1", "title": "Passionfruit", "artists": ["Drake"], "album": "More Life",
             "durationSeconds": 299, "videoType": "MUSIC_VIDEO_TYPE_ATV"},
            {"videoId": "pod1", "title": "Passionfruit", "artists": ["A podcast"],
             "durationSeconds": 2990, "videoType": "MUSIC_VIDEO_TYPE_PODCAST_EPISODE"},
        ]})))
        .mount(&shim)
        .await;
    Mock::given(method("GET"))
        .and(path("/candidates"))
        .and(query_param("q", "Drake Passionfruit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"candidates": [
            {"video_id": "vid1", "title": "Drake - Passionfruit (Official Video)", "duration": 300, "channel": "Drake"},
            {"video_id": "ytm1", "title": "Passionfruit", "duration": 299, "channel": "Drake - Topic"},
        ]})))
        .mount(&shim)
        .await;
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                auto_detect_download_path: false,
                admin_username: Some("admin".into()),
                admin_password: Some("admin-password".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            raw_settings: vec![
                ("YouTube:ShimUrl".into(), shim.uri()),
                (
                    "Library:DownloadPath".into(),
                    directory.path().to_string_lossy().into_owned(),
                ),
            ],
            ..Default::default()
        },
    );
    Fixture {
        _navidrome: navidrome,
        _shim: shim,
        _directory: directory,
        app: app(state.clone()),
        state,
    }
}

/// The path the library song's file is at.
fn library_file(fixture: &Fixture) -> String {
    fixture
        ._directory
        .path()
        .join(LIBRARY_PATH)
        .to_string_lossy()
        .into_owned()
}

fn song(fixture: &Fixture, pinned: Option<&str>) -> String {
    fixture.state.external_id_registry.register(OutsideRouting {
        kind: RoutingKind::Song,
        artist: Some("Drake".into()),
        title: Some("Passionfruit".into()),
        duration: Some(299),
        you_tube_id: pinned.map(str::to_string),
        ..Default::default()
    })
}

async fn call(fixture: &Fixture, rest: &str) -> Value {
    get(
        &fixture.app,
        &format!("/rest/{rest}&u=alice&p=secret&v=1.16.1&c=test&f=json"),
    )
    .await
    .json()["subsonic-response"]
        .clone()
}

#[tokio::test]
async fn the_choices_are_youtube_musics_rows_then_youtubes_each_video_once() {
    let fixture = fixture().await;
    let id = song(&fixture, None);
    let answer = call(&fixture, &format!("getPreviewChoices?id={id}")).await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(answer["previewChoices"]["id"], id);
    let choices = answer["previewChoices"]["choice"].as_array().expect("choices");
    let ids: Vec<&str> = choices.iter().map(|c| c["videoId"].as_str().unwrap()).collect();
    assert_eq!(ids, ["ytm1", "vid1"], "{answer}");
    assert_eq!(choices[0]["source"], "YouTube Music");
    assert_eq!(choices[0]["channel"], "Drake");
    assert_eq!(choices[0]["lengthSeconds"], 299);
    assert_eq!(choices[0]["current"], false);
    assert_eq!(choices[1]["source"], "YouTube");
    assert_eq!(choices[1]["title"], "Drake - Passionfruit (Official Video)");
}

#[tokio::test]
async fn the_video_that_plays_now_is_marked_and_listed_first_when_unknown() {
    let fixture = fixture().await;
    let listed = song(&fixture, Some("vid1"));
    let answer = call(&fixture, &format!("getPreviewChoices?id={listed}")).await;
    let choices = answer["previewChoices"]["choice"].as_array().expect("choices");
    assert_eq!(choices[1]["videoId"], "vid1");
    assert_eq!(choices[1]["current"], true);
    assert_eq!(choices[0]["current"], false);

    let elsewhere = fixture.state.external_id_registry.register(OutsideRouting {
        kind: RoutingKind::Song,
        artist: Some("Drake".into()),
        title: Some("Passionfruit".into()),
        duration: Some(298),
        you_tube_id: Some("old".into()),
        ..Default::default()
    });
    let answer = call(&fixture, &format!("getPreviewChoices?id={elsewhere}")).await;
    let choices = answer["previewChoices"]["choice"].as_array().expect("choices");
    assert_eq!(choices[0]["videoId"], "old", "{answer}");
    assert_eq!(choices[0]["current"], true);
    assert_eq!(choices[0]["title"], "Passionfruit");
    assert_eq!(choices.len(), 3);
}

#[tokio::test]
async fn the_pick_is_pinned_and_answered_as_the_song_with_its_length() {
    let fixture = fixture().await;
    let id = song(&fixture, Some("vid1"));
    let answer = call(&fixture, &format!("setPreview?id={id}&videoId=ytm1&length=299")).await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(answer["song"]["id"], id);
    assert_eq!(answer["song"]["duration"], 299, "{answer}");
    let routing = fixture
        .state
        .external_id_registry
        .lookup(&id)
        .expect("there")
        .snapshot();
    assert_eq!(routing.you_tube_id.as_deref(), Some("ytm1"));
    assert_eq!(routing.duration, Some(299));
    // The pick is what the choices say plays now.
    let answer = call(&fixture, &format!("getPreviewChoices?id={id}")).await;
    let choices = answer["previewChoices"]["choice"].as_array().expect("choices");
    assert_eq!(choices[0]["videoId"], "ytm1");
    assert_eq!(choices[0]["current"], true);
}

#[tokio::test]
async fn an_outside_songs_choices_do_not_say_library() {
    let fixture = fixture().await;
    let id = song(&fixture, None);
    let answer = call(&fixture, &format!("getPreviewChoices?id={id}")).await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert!(answer["previewChoices"].get("library").is_none(), "{answer}");
}

// Version 2: a song that is a file in the library.

#[tokio::test]
async fn a_library_song_has_versions_too_and_its_choices_say_so() {
    let fixture = fixture().await;
    let answer = call(&fixture, &format!("getPreviewChoices?id={LIBRARY_ID}")).await;
    assert_eq!(answer["status"], "ok", "{answer}");
    assert_eq!(answer["previewChoices"]["id"], LIBRARY_ID);
    assert_eq!(answer["previewChoices"]["library"], true, "{answer}");
    let choices = answer["previewChoices"]["choice"].as_array().expect("choices");
    let ids: Vec<&str> = choices.iter().map(|c| c["videoId"].as_str().unwrap()).collect();
    assert_eq!(ids, ["ytm1", "vid1"], "{answer}");
    assert!(choices.iter().all(|c| c["current"] == false), "{answer}");
    // The routing that stands for the file: its facts as Navidrome reports them.
    let routing = routing_for_library_song(&fixture.state, LIBRARY_ID)
        .await
        .expect("a routing");
    let r = fixture
        .state
        .external_id_registry
        .lookup(&routing)
        .expect("registered")
        .snapshot();
    assert_eq!(r.artist.as_deref(), Some("Drake"));
    assert_eq!(r.title.as_deref(), Some("Passionfruit"));
    assert_eq!(r.album.as_deref(), Some("More Life"));
    assert_eq!(r.duration, Some(299));
    assert_eq!(r.you_tube_id, None);
}

#[tokio::test]
async fn a_library_songs_pick_is_pinned_on_its_routing_and_the_song_answered_as_navidrome_does() {
    let fixture = fixture().await;
    let answer = call(
        &fixture,
        &format!("setPreview?id={LIBRARY_ID}&videoId=vid1&length=300"),
    )
    .await;
    assert_eq!(answer["status"], "ok", "{answer}");
    // Navidrome's own getSong: the file is unchanged until a replace goes in.
    assert_eq!(answer["type"], "navidrome", "{answer}");
    assert_eq!(answer["song"]["id"], LIBRARY_ID);
    assert_eq!(answer["song"]["duration"], 299, "{answer}");
    let routing = routing_for_library_song(&fixture.state, LIBRARY_ID)
        .await
        .expect("a routing");
    let r = fixture
        .state
        .external_id_registry
        .lookup(&routing)
        .expect("registered")
        .snapshot();
    assert_eq!(r.you_tube_id.as_deref(), Some("vid1"));
    assert_eq!(r.duration, Some(300));
    // The pick is what the choices say plays now.
    let answer = call(&fixture, &format!("getPreviewChoices?id={LIBRARY_ID}")).await;
    assert_eq!(answer["previewChoices"]["library"], true, "{answer}");
    let choices = answer["previewChoices"]["choice"].as_array().expect("choices");
    assert_eq!(choices[1]["videoId"], "vid1");
    assert_eq!(choices[1]["current"], true, "{answer}");
}

/// A song Octo downloaded keeps the routing it came from, with the video it was fetched from.
#[tokio::test]
async fn a_downloaded_library_song_chooses_on_the_routing_it_came_from() {
    let fixture = fixture().await;
    let origin = fixture.state.external_id_registry.register(OutsideRouting {
        kind: RoutingKind::Song,
        artist: Some("Drake".into()),
        title: Some("Passionfruit".into()),
        duration: Some(298),
        you_tube_id: Some("old".into()),
        ..Default::default()
    });
    fixture.state.download_history.record(DownloadHistoryEntry {
        artist: "Drake".into(),
        title: "Passionfruit".into(),
        path: library_file(&fixture),
        key: Some(format!("outside:{origin}")),
        ..Default::default()
    });
    assert_eq!(
        routing_for_library_song(&fixture.state, LIBRARY_ID)
            .await
            .as_deref(),
        Some(origin.as_str())
    );
    let answer = call(&fixture, &format!("getPreviewChoices?id={LIBRARY_ID}")).await;
    assert_eq!(answer["previewChoices"]["library"], true, "{answer}");
    let choices = answer["previewChoices"]["choice"].as_array().expect("choices");
    assert_eq!(choices[0]["videoId"], "old", "{answer}");
    assert_eq!(choices[0]["current"], true);
    // A fresh registration of the file's facts would be another id; the origin is kept.
    let fresh = ExternalIdRegistry::make_short_id(&OutsideRouting {
        kind: RoutingKind::Song,
        artist: Some("Drake".into()),
        title: Some("Passionfruit".into()),
        duration: Some(299),
        ..Default::default()
    });
    assert!(fixture.state.external_id_registry.lookup(&fresh).is_none());
}

#[tokio::test]
async fn only_a_song_with_a_file_or_from_outside_the_library_has_versions() {
    let fixture = fixture().await;
    let answer = call(&fixture, "getPreviewChoices?id=4f3a2b1c0d9e8f7a6b5c4d3e").await;
    assert_eq!(answer["error"]["code"], 70, "{answer}");
    let answer = call(&fixture, "setPreview?id=4f3a2b1c0d9e8f7a6b5c4d3e&videoId=x").await;
    assert_eq!(answer["error"]["code"], 70, "{answer}");
    // Another provider's song is neither.
    let answer = call(&fixture, "getPreviewChoices?id=ext-deezer-song-1").await;
    assert_eq!(answer["error"]["code"], 70, "{answer}");
    let answer = call(&fixture, "getPreviewChoices?id=").await;
    assert_eq!(answer["error"]["code"], 10, "{answer}");
    let id = song(&fixture, None);
    let answer = call(&fixture, &format!("setPreview?id={id}")).await;
    assert_eq!(answer["error"]["code"], 10, "{answer}");
}
