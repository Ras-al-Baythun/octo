//! Generated playlists through getPlaylists, getPlaylist and updatePlaylist, repeated
//! parameters on updatePlaylist, the song lengths a client receives, and checks of ping,
//! getRandomSongs, the info endpoints, getSimilarSongs and the radio stream's route.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, StatusCode};
use chrono::{TimeDelta, Utc};
use octo_core::last_fm::last_fm_radio_state_store::station_id;
use octo_core::models::domain::Album;
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioStationKind, LastFmRadioTrack};
use octo_core::outside::{OutsideRouting, RoutingKind};
use octo_core::settings::{AppSettings, GeneratedPlaylistSettings, LastFmSettings, SubsonicSettings};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::{TestMetadata, app, get, get_string, query_value, send};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;

fn ok_json(inner: &str) -> String {
    format!(
        r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1"{}}}}}"#,
        if inner.is_empty() {
            String::new()
        } else {
            format!(",{inner}")
        }
    )
}

fn json_reply(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "application/json; charset=utf-8")
}

// ---------------------------------------------------------------------------------------
// Generated playlists
// ---------------------------------------------------------------------------------------

/// A library of Rock songs.
#[derive(Clone, Default)]
struct LibraryNavidrome {
    calls: Arc<Mutex<Vec<String>>>,
}

fn library_songs(count: usize, first: usize, play_count: i32) -> String {
    (first..first + count)
        .map(|i| {
            format!(
                r#"{{"id":"lib{i}","title":"Track {i}","artist":"Artist {}","artistId":"ar{}","duration":200,"playCount":{play_count},"suffix":"flac","bitRate":900}}"#,
                i % 20,
                i % 20
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

impl Respond for LibraryNavidrome {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let path = request.url.path().trim_matches('/').to_string();
        self.calls
            .lock()
            .push(format!("{path}?{}", request.url.query().unwrap_or_default()));
        let body = match path.as_str() {
            "rest/getGenres" => ok_json(
                r#""genres":{"genre":[{"value":"Rock","songCount":40},{"value":"Polka","songCount":3}]}"#,
            ),
            "rest/getSongsByGenre" => {
                let first = if query_value(request, "offset").as_deref() == Some("0") {
                    0
                } else {
                    1000
                };
                ok_json(&format!(
                    r#""songsByGenre":{{"song":[{}]}}"#,
                    library_songs(40, first, 5)
                ))
            }
            "rest/getRandomSongs" => match query_value(request, "fromYear").as_deref() {
                Some("1990") => ok_json(&format!(
                    r#""randomSongs":{{"song":[{}]}}"#,
                    library_songs(25, 2000, 5)
                )),
                None => ok_json(&format!(
                    r#""randomSongs":{{"song":[{}]}}"#,
                    library_songs(30, 3000, 0)
                )),
                Some(_) => ok_json(r#""randomSongs":{"song":[]}"#),
            },
            _ => ok_json(""),
        };
        json_reply(body)
    }
}

struct MixFixture {
    _dir: TempDir,
    _server: MockServer,
    navidrome: LibraryNavidrome,
    app: App,
}

async fn mix_fixture() -> MixFixture {
    let dir = TempDir::new().unwrap();
    let navidrome = LibraryNavidrome::default();
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(navidrome.clone())
        .mount(&server)
        .await;
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            generated_playlists: GeneratedPlaylistSettings {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            config_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        },
    );
    MixFixture {
        _dir: dir,
        _server: server,
        navidrome,
        app: app(state),
    }
}

const MIX_AUTH: &str = "u=alice&t=token&s=salt&v=1.16.1&c=test&f=json";

async fn rock_mix(app: &App) -> Value {
    let list: Value =
        serde_json::from_str(&get_string(app, &format!("/rest/getPlaylists.view?{MIX_AUTH}")).await).unwrap();
    list["subsonic-response"]["playlists"]["playlist"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "Rock Mix")
        .cloned()
        .expect("the Rock Mix is listed")
}

#[tokio::test]
async fn get_playlists_lists_the_listeners_mixes_read_only() {
    let fixture = mix_fixture().await;
    let mix = rock_mix(&fixture.app).await;
    assert!(mix["id"].as_str().unwrap().starts_with("og"));
    assert_eq!(mix["owner"], "alice");
    assert_eq!(mix["readonly"], true);
}

#[tokio::test]
async fn get_playlist_a_mix_is_its_draw() {
    let fixture = mix_fixture().await;
    let id = rock_mix(&fixture.app).await["id"].as_str().unwrap().to_string();

    let detail: Value = serde_json::from_str(
        &get_string(
            &fixture.app,
            &format!("/rest/getPlaylist.view?{MIX_AUTH}&id={id}"),
        )
        .await,
    )
    .unwrap();
    let playlist = &detail["subsonic-response"]["playlist"];
    assert_eq!(playlist["name"], "Rock Mix");
    let entries = playlist["entry"].as_array().unwrap();
    assert_eq!(entries.len(), 40);
    assert!(
        entries
            .iter()
            .all(|e| e["id"].as_str().unwrap().starts_with("lib"))
    );
}

#[tokio::test]
async fn update_playlist_on_a_mix_is_refused() {
    let fixture = mix_fixture().await;
    let id = rock_mix(&fixture.app).await["id"].as_str().unwrap().to_string();

    let answer = get(
        &fixture.app,
        &format!("/rest/updatePlaylist.view?{MIX_AUTH}&playlistId={id}&name=Mine"),
    )
    .await
    .json();
    assert_eq!(answer["subsonic-response"]["error"]["code"], 70);
    assert!(
        !fixture
            .navidrome
            .calls
            .lock()
            .iter()
            .any(|call| call.starts_with("rest/updatePlaylist"))
    );
}

// ---------------------------------------------------------------------------------------
// Repeated parameters on updatePlaylist
// ---------------------------------------------------------------------------------------

/// One request as Navidrome saw it: its path, its query, and its form body if any.
#[derive(Clone, Default)]
struct SeenRequests(Arc<Mutex<Vec<(String, Vec<(String, String)>)>>>);

impl SeenRequests {
    fn only(&self, path: &str) -> Vec<(String, String)> {
        let seen = self.0.lock();
        let matching: Vec<_> = seen.iter().filter(|(p, _)| p == path).collect();
        assert_eq!(matching.len(), 1, "{path}: {:?}", *seen);
        matching[0].1.clone()
    }
}

impl Respond for SeenRequests {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let mut pairs: Vec<(String, String)> = request.url.query_pairs().into_owned().collect();
        let is_form = request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));
        if is_form {
            pairs.extend(url::form_urlencoded::parse(&request.body).into_owned());
        }
        self.0.lock().push((request.url.path().to_string(), pairs));
        json_reply(ok_json(""))
    }
}

fn all(pairs: &[(String, String)], key: &str) -> Vec<String> {
    pairs
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .collect()
}

async fn repeats_fixture() -> (MockServer, SeenRequests, App) {
    let seen = SeenRequests::default();
    let server = MockServer::builder().start().await;
    Mock::given(any()).respond_with(seen.clone()).mount(&server).await;
    let state = AppState::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            auto_detect_download_path: false,
            ..Default::default()
        },
        ..Default::default()
    });
    (server, seen, app(state))
}

const A: &str = "3vXkQ9mTz2LbW8rYcN1pDf";
const B: &str = "7HqRs4uVw0XyZaBcDeFgHi";
const C: &str = "1JkLmNoPqRsTuVwXyZ0a2b";
const REPEATS_AUTH: &str = "u=alice&t=good&s=salt&v=1.16.1&c=test&f=json";

#[tokio::test]
async fn update_playlist_get_adds_every_song() {
    let (_server, seen, app) = repeats_fixture().await;
    let response = get(
        &app,
        &format!(
            "/rest/updatePlaylist?{REPEATS_AUTH}&playlistId=pl1&songIdToAdd={A}&songIdToAdd={B}&songIdToAdd={C}\
             &songIndexToRemove=0&songIndexToRemove=4"
        ),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let pairs = seen.only("/rest/updatePlaylist");
    assert_eq!(all(&pairs, "songIdToAdd"), [A, B, C]);
    assert_eq!(all(&pairs, "songIndexToRemove"), ["0", "4"]);
    assert_eq!(all(&pairs, "playlistId"), ["pl1"]);
}

#[tokio::test]
async fn update_playlist_form_post_adds_every_song() {
    let (_server, seen, app) = repeats_fixture().await;
    let form = format!("playlistId=pl1&songIdToAdd={A}&songIdToAdd={B}&songIdToAdd={C}");
    let response = send(
        &app,
        Method::POST,
        &format!("/rest/updatePlaylist.view?{REPEATS_AUTH}"),
        &[("Content-Type", "application/x-www-form-urlencoded")],
        Body::from(form),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let pairs = seen.only("/rest/updatePlaylist");
    assert_eq!(all(&pairs, "songIdToAdd"), [A, B, C]);
    assert_eq!(all(&pairs, "playlistId"), ["pl1"]);
}

/// The relay target keeps the client's own spelling of the endpoint, less `.view`.
#[tokio::test]
async fn a_playlist_mutation_is_relayed_under_the_clients_spelling() {
    let (_server, seen, app) = repeats_fixture().await;
    get(&app, &format!("/REST/CreatePlaylist.view?{REPEATS_AUTH}&name=x")).await;
    seen.only("/rest/CreatePlaylist");
}

// ---------------------------------------------------------------------------------------
// Song lengths: what a client actually receives, search3 and a station's getPlaylist, through the
// real metadata service, with Navidrome owning none of the songs.
// ---------------------------------------------------------------------------------------

/// Navidrome with an empty library, Last.fm's track.search and track.getInfo (under `/2.0/`),
/// Deezer (under `/deezer`) and the shim (under `/shim`), from the fixture's tables.
#[derive(Clone, Default)]
struct LengthUpstream {
    deezer: Arc<Mutex<HashMap<String, i32>>>,
    last_fm: Arc<Mutex<HashMap<String, i32>>>,
    video: Arc<Mutex<HashMap<String, i32>>>,
    /// Videos the shim's full search (the one a play makes) finds; its quick lookup does not.
    played_video: Arc<Mutex<HashMap<String, i32>>>,
    /// Last.fm's artist.getInfo by the name asked: (the name Last.fm answers for, the summary).
    artist_bio: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// Deezer's artists by name (their catalog id), and the artists related to each id.
    deezer_artists: Arc<Mutex<HashMap<String, i64>>>,
    deezer_related: Arc<Mutex<HashMap<i64, Vec<(i64, String)>>>>,
    search_tracks: Arc<Mutex<Vec<(String, String)>>>,
    /// YouTube Music's song search (`/shim/ytm/search`), whatever the words.
    ytm_songs: Arc<Mutex<Vec<Value>>>,
    /// How long YouTube Music's search takes to answer.
    ytm_delay: Arc<Mutex<Option<std::time::Duration>>>,
    /// How long Last.fm's track.search and Deezer's search take to answer.
    rows_delay: Arc<Mutex<Option<std::time::Duration>>>,
    /// Every path asked of the shim with its words (`q`), in order.
    shim_calls: Arc<Mutex<Vec<String>>>,
}

fn find_ignore_case(map: &HashMap<String, i32>, key: &str) -> Option<(String, i32)> {
    map.iter()
        .find(|(k, _)| k.to_lowercase() == key.to_lowercase())
        .map(|(k, v)| (k.clone(), *v))
}

impl Respond for LengthUpstream {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let path = request.url.path().to_string();
        if path.starts_with("/shim/") {
            let words = query_value(request, "q").unwrap_or_default();
            self.shim_calls.lock().push(format!("{path} {words}"));
        }
        let reply = self.answer(request, &path);
        let delay = if path == "/shim/ytm/search" {
            *self.ytm_delay.lock()
        } else if path == "/deezer/search"
            || (path.starts_with("/2.0") && query_value(request, "method").as_deref() == Some("track.search"))
        {
            *self.rows_delay.lock()
        } else {
            None
        };
        match delay {
            Some(delay) => reply.set_delay(delay),
            None => reply,
        }
    }
}

impl LengthUpstream {
    fn answer(&self, request: &wiremock::Request, path: &str) -> ResponseTemplate {
        let path = path.to_string();
        if path == "/shim/ytm/search" {
            return json_reply(json!({ "tracks": self.ytm_songs.lock().clone() }).to_string());
        }
        if path.starts_with("/rest/") {
            let fields = if path.contains("search3") {
                r#""searchResult3":{"song":[],"album":[],"artist":[]}"#
            } else {
                ""
            };
            return json_reply(ok_json(fields));
        }
        if path.starts_with("/2.0") {
            match query_value(request, "method").as_deref() {
                Some("track.search") => {
                    let tracks: Vec<Value> = self
                        .search_tracks
                        .lock()
                        .iter()
                        .map(|(artist, title)| json!({"name": title, "artist": artist}))
                        .collect();
                    return json_reply(json!({"results": {"trackmatches": {"track": tracks}}}).to_string());
                }
                Some("artist.gettoptracks") => return json_reply(r#"{"toptracks":{"track":[]}}"#.into()),
                Some("artist.getInfo") => {
                    let artist = query_value(request, "artist").unwrap_or_default();
                    if let Some((name, summary)) = self.artist_bio.lock().get(&artist) {
                        return json_reply(
                            json!({"artist": {"name": name, "bio": {"summary": summary, "content": summary}}})
                                .to_string(),
                        );
                    }
                    return json_reply(
                        r#"{"error":6,"message":"The artist you supplied could not be found"}"#.into(),
                    );
                }
                Some("track.getInfo") => {
                    let artist = query_value(request, "artist").unwrap_or_default();
                    let track = query_value(request, "track").unwrap_or_default();
                    if let Some(seconds) = self.last_fm.lock().get(&format!("{artist}|{track}")) {
                        return json_reply(format!(
                            r#"{{"track":{{"name":"{track}","duration":"{}","artist":{{"name":"{artist}"}}}}}}"#,
                            seconds * 1000
                        ));
                    }
                    return json_reply(r#"{"error":6,"message":"Track not found"}"#.into());
                }
                _ => return json_reply(r#"{"error":6,"message":"Track not found"}"#.into()),
            }
        }
        let q = query_value(request, "q").unwrap_or_default();
        if path == "/deezer/search/artist" {
            let found: Vec<Value> = self
                .deezer_artists
                .lock()
                .iter()
                .filter(|(name, _)| name.to_lowercase() == q.to_lowercase())
                .map(|(name, id)| json!({"id": id, "name": name, "nb_fan": 1000, "nb_album": 4}))
                .collect();
            return json_reply(json!({ "data": found }).to_string());
        }
        if let Some(id) = path
            .strip_prefix("/deezer/artist/")
            .and_then(|rest| rest.strip_suffix("/related"))
            .and_then(|id| id.parse::<i64>().ok())
        {
            let related: Vec<Value> = self
                .deezer_related
                .lock()
                .get(&id)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|(id, name)| json!({"id": id, "name": name, "nb_fan": 10, "nb_album": 2}))
                .collect();
            return json_reply(json!({ "data": related }).to_string());
        }
        if let Some(path) = path.strip_prefix("/deezer") {
            return match find_ignore_case(&self.deezer.lock(), &q) {
                Some((key, duration)) if path == "/search" => {
                    let split = key.rfind(' ').unwrap();
                    json_reply(
                        json!({"data": [{"title": &key[split + 1..], "duration": duration, "artist": {"name": &key[..split]}}]})
                            .to_string(),
                    )
                }
                _ => json_reply(r#"{"data":[]}"#.into()),
            };
        }
        if path == "/shim/meta"
            && let Some(length) = self.video.lock().get(&q)
        {
            return json_reply(format!(r#"{{"video_id":"vid-{q}","duration":{length}}}"#));
        }
        if path == "/shim/search"
            && let Some(length) = self.played_video.lock().get(&q)
        {
            return json_reply(format!(r#"{{"video_id":"vid-{q}","duration":{length}}}"#));
        }
        if path == "/shim/stream" {
            return ResponseTemplate::new(200).set_body_raw(vec![0u8; 64], "audio/mp4");
        }
        ResponseTemplate::new(404)
    }
}

struct LengthFixture {
    _server: MockServer,
    upstream: LengthUpstream,
    state: AppState,
    app: App,
}

async fn length_fixture() -> LengthFixture {
    let upstream = LengthUpstream::default();
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(upstream.clone())
        .mount(&server)
        .await;
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            last_fm: LastFmSettings {
                api_key: "key".into(),
                enable_radio: true,
                enable_personalized_stations: true,
                expose_radio_as_playlists: true,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            last_fm_base_url: Some(format!("{}/2.0/", server.uri())),
            deezer_base_url: Some(format!("{}/deezer", server.uri())),
            raw_settings: vec![("YouTube:ShimUrl".into(), format!("{}/shim", server.uri()))],
            ..Default::default()
        },
    );
    LengthFixture {
        _server: server,
        upstream,
        app: app(state.clone()),
        state,
    }
}

/// Each row's length by "artist|title"; None where the row carries no `duration` at all.
fn lengths(songs: &Value) -> HashMap<String, Option<i64>> {
    songs
        .as_array()
        .unwrap()
        .iter()
        .map(|song| {
            (
                format!(
                    "{}|{}",
                    song["artist"].as_str().unwrap(),
                    song["title"].as_str().unwrap()
                ),
                song.get("duration").map(|d| d.as_i64().unwrap()),
            )
        })
        .collect()
}

async fn search3_lengths(fixture: &LengthFixture) -> HashMap<String, Option<i64>> {
    let body = get(
        &fixture.app,
        "/rest/search3?query=genesis&songCount=50&albumCount=0&artistCount=0&u=alice&t=token&s=salt&f=json",
    )
    .await
    .json();
    lengths(&body["subsonic-response"]["searchResult3"]["song"])
}

async fn playlist_lengths(fixture: &LengthFixture) -> HashMap<String, Option<i64>> {
    let body = get(
        &fixture.app,
        &format!(
            "/rest/getPlaylist?id={}&u=alice&t=token&s=salt&f=json",
            station_id("alice", "your-mix")
        ),
    )
    .await
    .json();
    lengths(&body["subsonic-response"]["playlist"]["entry"])
}

#[tokio::test]
async fn search3_rows_without_a_metadata_length_carry_one_in_the_next_response() {
    let fixture = length_fixture().await;
    // Nine ordinary rows with Deezer lengths, so the ones under test sit past the rows
    // whose YouTube length the search resolves itself.
    for i in 1..=9 {
        fixture
            .upstream
            .deezer
            .lock()
            .insert(format!("Filler Song{i}"), 200 + i);
        fixture
            .upstream
            .search_tracks
            .lock()
            .push(("Filler".into(), format!("Song{i}")));
    }
    for (artist, title) in [
        ("Daft Punk", "Emotion"),
        ("Kavinsky", "Prelude"),
        ("Nobody", "Nothing"),
        ("Justice", "Genesis"),
    ] {
        fixture
            .upstream
            .search_tracks
            .lock()
            .push((artist.into(), title.into()));
    }
    fixture
        .upstream
        .video
        .lock()
        .insert("Daft Punk Emotion".into(), 417);
    fixture
        .upstream
        .last_fm
        .lock()
        .insert("Kavinsky|Prelude".into(), 95);
    fixture
        .upstream
        .video
        .lock()
        .insert("Nobody Nothing".into(), 3600);
    fixture
        .upstream
        .deezer
        .lock()
        .insert("Justice Genesis".into(), 234);

    let first = search3_lengths(&fixture).await;
    assert_eq!(first["Filler|Song1"], Some(201));
    assert_eq!(first["Daft Punk|Emotion"], None);
    // A first-page row the search's one track search missed is finished after the answer,
    // and only then handed to the length lookup.
    fixture.state.music_metadata.last_rows_finish().await;
    fixture.state.music_metadata.last_length_warm().await;

    let next = search3_lengths(&fixture).await;
    assert_eq!(next["Filler|Song1"], Some(201));
    assert_eq!(next["Daft Punk|Emotion"], Some(417));
    assert_eq!(next["Kavinsky|Prelude"], Some(95));
    assert_eq!(next["Justice|Genesis"], Some(234));
    assert_eq!(next["Nobody|Nothing"], None); // still unknown: nothing plausible
}

#[tokio::test]
async fn get_playlist_station_rows_without_a_length_carry_one_in_the_next_response() {
    let fixture = length_fixture().await;
    fixture
        .upstream
        .deezer
        .lock()
        .insert("Justice Genesis".into(), 234);
    fixture
        .upstream
        .last_fm
        .lock()
        .insert("Kavinsky|Prelude".into(), 95);
    fixture
        .upstream
        .video
        .lock()
        .insert("Daft Punk Emotion".into(), 417);
    let track = |artist: &str, title: &str, duration: Option<i32>| LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        duration,
        ..Default::default()
    };
    fixture.state.last_fm_radio_state.replace_stations(
        "alice",
        &[LastFmRadioStation {
            id: station_id("alice", "your-mix"),
            key: "your-mix".into(),
            name: "Your Mix".into(),
            owner: "alice".into(),
            kind: LastFmRadioStationKind::YourMix,
            personalized: true,
            created_utc: Utc::now() - TimeDelta::hours(1),
            changed_utc: Utc::now() - TimeDelta::hours(1),
            valid_until_utc: Utc::now() + TimeDelta::days(1),
            tracks: vec![
                track("Justice", "Genesis", None),
                track("Kavinsky", "Prelude", None),
                track("Daft Punk", "Emotion", None),
                track("Mr. Oizo", "Positif", Some(207)),
                track("Nobody", "Nothing", None),
            ],
            ..Default::default()
        }],
    );

    let first = playlist_lengths(&fixture).await;
    assert_eq!(first["Mr. Oizo|Positif"], Some(207));
    assert_eq!(first["Justice|Genesis"], None);
    fixture.state.music_metadata.last_length_warm().await;

    let next = playlist_lengths(&fixture).await;
    assert_eq!(next["Justice|Genesis"], Some(234));
    assert_eq!(next["Kavinsky|Prelude"], Some(95));
    assert_eq!(next["Daft Punk|Emotion"], Some(417));
    assert_eq!(next["Mr. Oizo|Positif"], Some(207));
    assert_eq!(next["Nobody|Nothing"], None);
}

#[tokio::test]
async fn get_song_an_outside_song_carries_its_remembered_length_or_none() {
    let fixture = length_fixture().await;
    fixture
        .upstream
        .deezer
        .lock()
        .insert("Justice Genesis".into(), 234);
    let track = |artist: &str, title: &str| LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    };
    fixture.state.last_fm_radio_state.replace_stations(
        "alice",
        &[LastFmRadioStation {
            id: station_id("alice", "your-mix"),
            key: "your-mix".into(),
            name: "Your Mix".into(),
            owner: "alice".into(),
            kind: LastFmRadioStationKind::YourMix,
            personalized: true,
            created_utc: Utc::now() - TimeDelta::hours(1),
            changed_utc: Utc::now() - TimeDelta::hours(1),
            valid_until_utc: Utc::now() + TimeDelta::days(1),
            tracks: vec![track("Justice", "Genesis"), track("Nobody", "Nothing")],
            ..Default::default()
        }],
    );
    let body = get(
        &fixture.app,
        &format!(
            "/rest/getPlaylist?id={}&u=alice&t=token&s=salt&f=json",
            station_id("alice", "your-mix")
        ),
    )
    .await
    .json();
    fixture.state.music_metadata.last_length_warm().await;
    let ids: HashMap<String, String> = body["subsonic-response"]["playlist"]["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|song| {
            (
                song["title"].as_str().unwrap().to_string(),
                song["id"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    let song = |id: &str| {
        let url = format!("/rest/getSong?id={id}&u=alice&t=token&s=salt&f=json");
        let app = &fixture.app;
        async move { get(app, &url).await.json()["subsonic-response"]["song"].clone() }
    };
    let known = song(&ids["Genesis"]).await;
    assert_eq!(known["duration"], 234);
    let unknown = song(&ids["Nothing"]).await;
    assert_eq!(unknown["title"], "Nothing");
    assert!(unknown.get("duration").is_none(), "{unknown}");
}

// ---------------------------------------------------------------------------------------
// ping, getRandomSongs, the info endpoints, getSimilarSongs and the radio stream
// ---------------------------------------------------------------------------------------

fn unconfigured() -> App {
    app(AppState::for_tests(AppSettings::default()))
}

#[tokio::test]
async fn ping_without_a_navidrome_url_explains_how_to_set_one_up() {
    let app = unconfigured();
    let xml = send(
        &app,
        Method::GET,
        "/rest/ping.view?u=a&p=b",
        &[("Host", "octo.lan:4040"), ("X-Forwarded-Proto", "https")],
        Body::empty(),
    )
    .await;
    assert_eq!(xml.status, StatusCode::OK);
    assert_eq!(xml.header("content-type"), Some("application/xml"));
    assert!(
        xml.body.contains(
            "message=\"Octo isn't configured yet. Open https://octo.lan:4040/admin and set your Navidrome URL \
             (SUBSONIC_URL), then point this client at Octo instead of Navidrome.\""
        ),
        "{}",
        xml.body
    );
    let json = send(
        &app,
        Method::GET,
        "/rest/ping?f=json",
        &[("Host", "h")],
        Body::empty(),
    )
    .await;
    assert_eq!(
        json.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    assert!(
        json.body
            .contains(r#"Octo isn\u0027t configured yet. Open http://h/admin"#),
        "{}",
        json.body
    );
}

#[tokio::test]
async fn ping_that_cannot_reach_navidrome_says_where_it_looked() {
    let app = app(AppState::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some("http://127.0.0.1:1".into()),
            ..Default::default()
        },
        ..Default::default()
    }));
    let body = get(&app, "/rest/ping?f=json").await.json();
    assert_eq!(body["subsonic-response"]["error"]["code"], 0);
    assert_eq!(
        body["subsonic-response"]["error"]["message"],
        "Octo can't reach Navidrome at http://127.0.0.1:1. Check the URL is correct and reachable from the Octo \
         container (use a LAN IP or service name, not localhost)."
    );
}

/// getRandomSongs lets a relay failure escape: the global handler's JSON, whatever `f` says.
#[tokio::test]
async fn random_songs_relay_failures_reach_the_global_handler() {
    let reply = get(&unconfigured(), "/rest/getRandomSongs?f=xml").await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        reply
            .body
            .starts_with(r#"{"subsonic-response":{"status":"failed""#),
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn random_songs_are_navidromes_body_as_text() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("<subsonic-response status=\"ok\"/>", "text/xml; charset=utf-8"),
        )
        .mount(&server)
        .await;
    let app = app(AppState::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            ..Default::default()
        },
        ..Default::default()
    }));
    let reply = get(&app, "/rest/getRandomSongs.view?size=2").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.header("content-type"), Some("text/xml; charset=utf-8"));
    assert_eq!(reply.body, "<subsonic-response status=\"ok\"/>");
}

#[tokio::test]
async fn info_for_outside_ids_carries_the_catalog_picture_in_both_formats() {
    let metadata = TestMetadata::default();
    let state = AppState::for_tests_with(
        AppSettings::default(),
        TestServices {
            metadata: Some(Arc::new(metadata)),
            ..Default::default()
        },
    );
    let app = app(state);
    // The fake knows no album: the URLs are empty, and the elements are still written.
    let xml = get(&app, "/rest/getAlbumInfo.view?id=ext-deezer-album-1")
        .await
        .body;
    assert_eq!(
        xml,
        "<subsonic-response status=\"ok\" version=\"1.16.1\" xmlns=\"http://subsonic.org/restapi\">\n  \
         <albumInfo>\n    <notes></notes>\n    <smallImageUrl></smallImageUrl>\n    <mediumImageUrl></mediumImageUrl>\n    \
         <largeImageUrl></largeImageUrl>\n  </albumInfo>\n</subsonic-response>"
    );
    let json = get(&app, "/rest/getArtistInfo?id=ext-deezer-artist-1&f=json")
        .await
        .body;
    assert_eq!(
        json,
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1","artistInfo2":{"biography":"","smallImageUrl":"","mediumImageUrl":"","largeImageUrl":""}}}"#
    );
    // A local id with Navidrome out of reach: the empty ok.
    let local = get(&app, "/rest/getArtistInfo2?id=ar-1&f=json").await.body;
    assert_eq!(
        local,
        r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#
    );
    let _ = Album::default();
}

#[tokio::test]
async fn similar_songs_need_an_id_and_relay_their_own_path_with_radio_off() {
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(json_reply(ok_json(r#""similarSongs2":{}"#)))
        .mount(&server)
        .await;
    // Radio is on by default, and since 2026.10.05 it answers without a Last.fm key (ListenBrainz
    // and Sounds alike need none), so this test turns it off.
    let app = app(AppState::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            ..Default::default()
        },
        last_fm: octo_core::settings::LastFmSettings {
            enable_radio: false,
            ..Default::default()
        },
        ..Default::default()
    }));
    let missing = get(&app, "/rest/getSimilarSongs2?f=json").await.json();
    assert_eq!(missing["subsonic-response"]["error"]["code"], 10);
    assert_eq!(
        missing["subsonic-response"]["error"]["message"],
        "Missing id parameter"
    );

    get(&app, "/rest/GetSimilarSongs2.view?id=x&f=json").await;
    let asked = server.received_requests().await.unwrap();
    assert_eq!(asked.len(), 1);
    // The path's own leading slash after the server's: `{Url}//rest/...`, as the client spelled it.
    assert_eq!(asked[0].url.path(), "//rest/GetSimilarSongs2.view");
}

#[tokio::test]
async fn radio_stream_tokens_of_another_length_are_relayed_and_unknown_ones_are_404() {
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_string("relayed"))
        .mount(&server)
        .await;
    let app = app(AppState::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(server.uri()),
            ..Default::default()
        },
        ..Default::default()
    }));
    let token = "0".repeat(48);
    for method in [Method::GET, Method::HEAD] {
        let reply = send(
            &app,
            method.clone(),
            &format!("/radio/stream/{token}"),
            &[],
            Body::empty(),
        )
        .await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{method}");
        assert_eq!(reply.body, "", "{method}");
        assert_eq!(reply.header("content-length"), Some("0"), "{method}");
    }
    let short = get(&app, "/Radio/Stream/tooshort").await;
    assert_eq!(short.body, "relayed");
    let asked = server.received_requests().await.unwrap();
    assert_eq!(asked[0].url.path(), "/Radio/Stream/tooshort");
}

/// One outside song from a station, by title: its id.
async fn station_song_ids(fixture: &LengthFixture, songs: &[(&str, &str)]) -> HashMap<String, String> {
    let track = |artist: &str, title: &str| LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    };
    fixture.state.last_fm_radio_state.replace_stations(
        "alice",
        &[LastFmRadioStation {
            id: station_id("alice", "your-mix"),
            key: "your-mix".into(),
            name: "Your Mix".into(),
            owner: "alice".into(),
            kind: LastFmRadioStationKind::YourMix,
            personalized: true,
            created_utc: Utc::now() - TimeDelta::hours(1),
            changed_utc: Utc::now() - TimeDelta::hours(1),
            valid_until_utc: Utc::now() + TimeDelta::days(1),
            tracks: songs.iter().map(|(artist, title)| track(artist, title)).collect(),
            ..Default::default()
        }],
    );
    let body = get(
        &fixture.app,
        &format!(
            "/rest/getPlaylist?id={}&u=alice&t=token&s=salt&f=json",
            station_id("alice", "your-mix")
        ),
    )
    .await
    .json();
    fixture.state.music_metadata.last_length_warm().await;
    body["subsonic-response"]["playlist"]["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|song| {
            (
                song["title"].as_str().unwrap().to_string(),
                song["id"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn song_by_id(fixture: &LengthFixture, id: &str) -> Value {
    let url = format!("/rest/getSong?id={id}&u=alice&t=token&s=salt&f=json");
    get(&fixture.app, &url).await.json()["subsonic-response"]["song"].clone()
}

#[tokio::test]
async fn get_song_an_outside_song_with_no_known_length_takes_its_videos_length() {
    let fixture = length_fixture().await;
    let ids = station_song_ids(&fixture, &[("Nobody", "Nothing")]).await;
    // Only the quick video lookup knows it, and nothing has looked yet.
    fixture.upstream.video.lock().insert("Nobody Nothing".into(), 211);

    let song = song_by_id(&fixture, &ids["Nothing"]).await;

    assert_eq!(song["duration"], 211, "{song}");
}

#[tokio::test]
async fn a_played_outside_song_remembers_the_length_of_the_video_that_played() {
    let fixture = length_fixture().await;
    let ids = station_song_ids(&fixture, &[("Nobody", "Nothing")]).await;
    fixture
        .upstream
        .played_video
        .lock()
        .insert("Nobody Nothing".into(), 199);
    let before = song_by_id(&fixture, &ids["Nothing"]).await;
    assert!(before.get("duration").is_none(), "{before}");

    let stream = get(
        &fixture.app,
        &format!("/rest/stream?id={}&u=alice&t=token&s=salt", ids["Nothing"]),
    )
    .await;
    assert_eq!(stream.status, StatusCode::OK);

    let after = song_by_id(&fixture, &ids["Nothing"]).await;
    assert_eq!(after["duration"], 199, "{after}");
}

/// An outside artist, as search registers one: its id.
fn outside_artist(fixture: &LengthFixture, name: &str) -> String {
    fixture.state.external_id_registry.register(OutsideRouting {
        kind: RoutingKind::Artist,
        artist: Some(name.into()),
        ..Default::default()
    })
}

fn justice_upstream(fixture: &LengthFixture, answered_as: &str) {
    fixture.upstream.artist_bio.lock().insert(
        "Justice".into(),
        (answered_as.into(), "French electronic duo.".into()),
    );
    fixture
        .upstream
        .deezer_artists
        .lock()
        .insert("Justice".into(), 27);
    fixture
        .upstream
        .deezer_related
        .lock()
        .insert(27, vec![(1, "Daft Punk".into()), (2, "Kavinsky".into())]);
}

#[tokio::test]
async fn get_artist_info2_an_outside_artist_has_last_fms_biography_and_related_artists() {
    let fixture = length_fixture().await;
    justice_upstream(&fixture, "Justice");
    let id = outside_artist(&fixture, "Justice");

    let body = get(
        &fixture.app,
        &format!("/rest/getArtistInfo2?id={id}&u=alice&t=token&s=salt&f=json"),
    )
    .await
    .json();

    let info = &body["subsonic-response"]["artistInfo2"];
    assert_eq!(info["biography"], "French electronic duo.", "{body}");
    let similar = info["similarArtist"].as_array().expect("related artists");
    let names: Vec<&str> = similar.iter().map(|a| a["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Daft Punk", "Kavinsky"]);
    assert_eq!(similar[0]["isExternal"], true);
    // Each opens that artist's own outside page.
    let daft_punk = similar[0]["id"].as_str().unwrap();
    let routing = fixture
        .state
        .external_id_registry
        .lookup(daft_punk)
        .expect("registered")
        .snapshot();
    assert_eq!(routing.artist.as_deref(), Some("Daft Punk"));
    assert_eq!(routing.external_artist_id.as_deref(), Some("1"));

    let xml = get(
        &fixture.app,
        &format!("/rest/getArtistInfo2?id={id}&count=1&u=alice&t=token&s=salt"),
    )
    .await
    .body;
    assert_eq!(xml.matches("<similarArtist ").count(), 1, "{xml}");
    assert!(xml.contains("name=\"Daft Punk\""), "{xml}");
}

#[tokio::test]
async fn get_artist_info2_a_biography_last_fm_gives_for_another_name_is_left_out() {
    let fixture = length_fixture().await;
    justice_upstream(&fixture, "Justice Crew");
    let id = outside_artist(&fixture, "Justice");

    let body = get(
        &fixture.app,
        &format!("/rest/getArtistInfo2?id={id}&u=alice&t=token&s=salt&f=json"),
    )
    .await
    .json();

    let info = &body["subsonic-response"]["artistInfo2"];
    assert_eq!(info["biography"], "", "{body}");
    assert_eq!(info["similarArtist"].as_array().map(Vec::len), Some(2));
}

// ---------------------------------------------------------------------------------------
// YouTube Music in search: its songs mixed with Last.fm's rows, and whichever is late left out.
// ---------------------------------------------------------------------------------------

fn ytm_song(id: &str, artist: &str, title: &str, seconds: i32, kind: &str) -> Value {
    json!({"videoId": id, "title": title, "artists": [artist], "durationSeconds": seconds,
        "videoType": kind, "thumbnail": format!("https://img.test/{id}")})
}

async fn search3_songs(fixture: &LengthFixture, query: &str) -> Vec<Value> {
    let body = get(
        &fixture.app,
        &format!("/rest/search3?query={query}&songCount=50&albumCount=0&artistCount=0&u=alice&t=token&s=salt&f=json"),
    )
    .await
    .json();
    body["subsonic-response"]["searchResult3"]["song"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn titles(songs: &[Value]) -> Vec<String> {
    songs
        .iter()
        .map(|s| s["title"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn search3_mixes_in_youtube_musics_songs_and_plays_them_without_a_lookup() {
    let fixture = length_fixture().await;
    fixture
        .upstream
        .search_tracks
        .lock()
        .push(("Svetlana Loboda".into(), "Moloko".into()));
    fixture
        .upstream
        .deezer
        .lock()
        .insert("Svetlana Loboda Moloko".into(), 200);
    *fixture.upstream.ytm_songs.lock() = vec![
        ytm_song("sv1", "NEXTIME", "СВЕТЛАНА!", 181, "MUSIC_VIDEO_TYPE_ATV"),
        ytm_song("omv", "Someone", "A Music Video", 240, "MUSIC_VIDEO_TYPE_OMV"),
    ];

    let songs = search3_songs(&fixture, "svetlana").await;
    let names = titles(&songs);
    assert!(names.contains(&"Moloko".to_string()), "{names:?}");
    assert!(names.contains(&"СВЕТЛАНА!".to_string()), "{names:?}");
    assert!(!names.contains(&"A Music Video".to_string()), "{names:?}");

    let found = songs.iter().find(|s| s["title"] == "СВЕТЛАНА!").unwrap();
    assert_eq!(found["duration"], 181);
    let id = found["id"].as_str().unwrap();
    let routing = fixture.state.external_id_registry.lookup(id).expect("registered");
    assert_eq!(routing.lock().you_tube_id.as_deref(), Some("sv1"));

    fixture.upstream.shim_calls.lock().clear();
    get(
        &fixture.app,
        &format!("/rest/stream?id={id}&u=alice&t=token&s=salt"),
    )
    .await;
    let calls = fixture.upstream.shim_calls.lock().clone();
    assert!(
        !calls
            .iter()
            .any(|c| c.contains("NEXTIME") || c.contains("СВЕТЛАНА")),
        "a pinned song is not looked up again: {calls:?}"
    );
    assert!(calls.iter().any(|c| c.starts_with("/shim/stream")), "{calls:?}");
}

#[tokio::test]
async fn a_late_youtube_music_leaves_last_fms_rows_alone() {
    let fixture = length_fixture().await;
    fixture
        .upstream
        .search_tracks
        .lock()
        .push(("Tom Odell".into(), "Another Love".into()));
    *fixture.upstream.ytm_songs.lock() =
        vec![ytm_song("late", "Late", "Too Late", 200, "MUSIC_VIDEO_TYPE_ATV")];
    *fixture.upstream.ytm_delay.lock() = Some(std::time::Duration::from_secs(5));

    let started = std::time::Instant::now();
    let songs = search3_songs(&fixture, "another%20love").await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(titles(&songs), ["Another Love"]);
}

#[tokio::test]
async fn late_last_fm_and_deezer_leave_youtube_musics_songs_alone() {
    let fixture = length_fixture().await;
    fixture
        .upstream
        .search_tracks
        .lock()
        .push(("Tom Odell".into(), "Another Love".into()));
    fixture
        .upstream
        .deezer
        .lock()
        .insert("Tom Odell Another".into(), 200);
    *fixture.upstream.ytm_songs.lock() = vec![ytm_song(
        "ol",
        "Tom Odell",
        "Another Love",
        244,
        "MUSIC_VIDEO_TYPE_ATV",
    )];
    *fixture.upstream.rows_delay.lock() = Some(std::time::Duration::from_secs(8));

    let started = std::time::Instant::now();
    let songs = search3_songs(&fixture, "another%20love").await;
    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(titles(&songs), ["Another Love"]);
    assert_eq!(songs[0]["duration"], 244);
}
