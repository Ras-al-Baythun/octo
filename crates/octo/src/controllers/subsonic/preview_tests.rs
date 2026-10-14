//! getPreviewChoices and setPreview: the videos a song from outside the library could play
//! from, and the pick that every later play reads.

use octo_core::outside::{OutsideRouting, RoutingKind};
use octo_core::settings::{AppSettings, SubsonicSettings};
use serde_json::{Value, json};
use wiremock::matchers::{any, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::route_tests::{app, get};
use crate::app::{AppState, TestServices};

struct Fixture {
    _navidrome: MockServer,
    _shim: MockServer,
    state: AppState,
    app: crate::http::pipeline::App,
}

/// A shim whose YouTube Music knows the song's release and whose YouTube lists its video first
/// as well as that release, and a Navidrome that accepts every sign-in.
async fn fixture() -> Fixture {
    let navidrome = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#,
            "application/json",
        ))
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
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            raw_settings: vec![("YouTube:ShimUrl".into(), shim.uri())],
            ..Default::default()
        },
    );
    Fixture {
        _navidrome: navidrome,
        _shim: shim,
        app: app(state.clone()),
        state,
    }
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
async fn only_a_song_from_outside_the_library_has_versions() {
    let fixture = fixture().await;
    let answer = call(&fixture, "getPreviewChoices?id=4f3a2b1c0d9e8f7a6b5c4d3e").await;
    assert_eq!(answer["error"]["code"], 70, "{answer}");
    let answer = call(&fixture, "setPreview?id=4f3a2b1c0d9e8f7a6b5c4d3e&videoId=x").await;
    assert_eq!(answer["error"]["code"], 70, "{answer}");
    let answer = call(&fixture, "getPreviewChoices?id=").await;
    assert_eq!(answer["error"]["code"], 10, "{answer}");
    let id = song(&fixture, None);
    let answer = call(&fixture, &format!("setPreview?id={id}")).await;
    assert_eq!(answer["error"]["code"], 10, "{answer}");
}
