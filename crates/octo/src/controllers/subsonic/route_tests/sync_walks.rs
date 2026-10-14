//! Sync walks through the app: a syncing client's empty-query search3 walk gets the library,
//! then the user's catalog, once each.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};

use chrono::{Duration as TimeDelta, TimeZone, Utc};
use octo_core::last_fm::last_fm_radio_state_store::station_id;
use octo_core::models::domain::Song;
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioStationKind, LastFmRadioTrack};
use octo_core::settings::{AppSettings, LastFmSettings, SubsonicSettings};
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::{TestMetadata, app, get_string, query_value};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;

/// The station tracks the library does not own.
fn expected_catalog_titles() -> Vec<String> {
    std::iter::once("Missing Hit".to_string())
        .chain((1..=12).map(|index| format!("Stranger Song {index}")))
        .chain(std::iter::once("Loner".to_string()))
        .collect()
}

const LIBRARY_ALBUM_COUNT: usize = 3;
const LIBRARY_ARTIST_COUNT: usize = 2;

/// Navidrome as far as a sync needs it: empty-query search3 pages over a fixed library, and
/// search3 by artist name for the catalog's library lookups.
#[derive(Clone)]
struct SyncUpstream {
    library_size: usize,
    /// The most rows one page returns, whatever was asked for.
    page_cap: Arc<AtomicI32>,
}

impl SyncUpstream {
    fn library_song_ids(&self) -> Vec<String> {
        (0..self.library_size)
            .map(|index| format!("lib-{index}"))
            .collect()
    }
}

fn result(songs: Vec<Value>, albums: Vec<Value>, artists: Vec<Value>) -> String {
    let mut result = serde_json::Map::new();
    if !artists.is_empty() {
        result.insert("artist".into(), Value::Array(artists));
    }
    if !albums.is_empty() {
        result.insert("album".into(), Value::Array(albums));
    }
    if !songs.is_empty() {
        result.insert("song".into(), Value::Array(songs));
    }
    json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "searchResult3": result}}).to_string()
}

impl Respond for SyncUpstream {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let ok = |body: String| {
            ResponseTemplate::new(200).set_body_raw(
                if body.is_empty() {
                    r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.to_string()
                } else {
                    body
                },
                "application/json; charset=utf-8",
            )
        };
        let path = request.url.path().trim_matches('/');
        if !path.starts_with("rest/search3") {
            return ok(String::new());
        }
        let cap = self.page_cap.load(Ordering::SeqCst);
        let number = |name: &str| -> usize {
            match query_value(request, name).and_then(|v| v.parse::<i32>().ok()) {
                Some(value) if name.ends_with("Count") => value.min(cap).max(0) as usize,
                Some(value) => value.max(0) as usize,
                None => 20,
            }
        };
        let term = query_value(request, "query").unwrap_or_default();
        let term = term.trim().trim_matches('"');
        if term.is_empty() {
            let songs = self
                .library_song_ids()
                .into_iter()
                .skip(number("songOffset"))
                .take(number("songCount"))
                .map(|id| json!({"id": id, "title": format!("Song {id}"), "artist": "Owned Artist"}))
                .collect();
            let albums = (0..LIBRARY_ALBUM_COUNT)
                .skip(number("albumOffset"))
                .take(number("albumCount"))
                .map(|index| {
                    if index == 0 {
                        json!({"id": "al-owned", "name": "Owned Album"})
                    } else {
                        json!({"id": format!("al-{index}"), "name": format!("Album {index}")})
                    }
                })
                .collect();
            let artists = (0..LIBRARY_ARTIST_COUNT)
                .skip(number("artistOffset"))
                .take(number("artistCount"))
                .map(|index| {
                    if index == 0 {
                        json!({"id": "ar-owned", "name": "Owned Artist"})
                    } else {
                        json!({"id": format!("ar-{index}"), "name": format!("Artist {index}")})
                    }
                })
                .collect();
            return ok(result(songs, albums, artists));
        }
        if term == "Owned Artist" {
            return ok(result(
                vec![json!({"id": "lib-owned-hit", "title": "Owned Hit", "artist": "Owned Artist"})],
                vec![
                    json!({"id": "al-owned", "name": "Owned Album", "artist": "Owned Artist", "artistId": "ar-owned"}),
                ],
                vec![json!({"id": "ar-owned", "name": "Owned Artist"})],
            ));
        }
        ok(result(Vec::new(), Vec::new(), Vec::new()))
    }
}

/// An Octo in front of a fake Navidrome with a library of `library_size` songs.
struct Fixture {
    _server: MockServer,
    upstream: SyncUpstream,
    state: AppState,
    app: App,
}

async fn fixture(library_size: usize) -> Fixture {
    let upstream = SyncUpstream {
        library_size,
        page_cap: Arc::new(AtomicI32::new(i32::MAX)),
    };
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(upstream.clone())
        .mount(&server)
        .await;
    let metadata = TestMetadata::answering(|artist, title, duration| {
        vec![Song {
            id: format!("ph-{}", format!("{artist}-{title}").replace(' ', "-")),
            artist: artist.into(),
            title: title.into(),
            album: String::new(),
            duration: Some(duration.unwrap_or(180)),
            is_local: false,
            external_provider: Some("outside".into()),
            ..Default::default()
        }]
    });
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                auto_detect_download_path: false,
                ..Default::default()
            },
            last_fm: LastFmSettings {
                enable_radio: true,
                enable_personalized_stations: true,
                expose_radio_as_playlists: true,
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            metadata: Some(Arc::new(metadata)),
            ..Default::default()
        },
    );
    Fixture {
        _server: server,
        upstream,
        app: app(state.clone()),
        state,
    }
}

fn install_stations(state: &AppState) {
    // Artists alternate at the front because a station playlist drops a track by the
    // artist it has just played.
    let track = |artist: &str, title: &str, album: Option<&str>, duration: i32| LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        album: album.map(str::to_string),
        duration: Some(duration),
        ..Default::default()
    };
    let mut tracks: Vec<LastFmRadioTrack> = (1..=12)
        .map(|index| {
            track(
                "Stranger",
                &format!("Stranger Song {index}"),
                Some(if index <= 6 {
                    "First Record"
                } else {
                    "Second Record"
                }),
                200,
            )
        })
        .collect();
    tracks.insert(0, track("Owned Artist", "Owned Hit", None, 180));
    tracks.insert(2, track("Owned Artist", "Missing Hit", Some("Owned Album"), 190));
    tracks.push(track("Stranger", "Loner", None, 150));

    state.last_fm_radio_state.replace_stations(
        "alice",
        &[LastFmRadioStation {
            id: station_id("alice", "your-mix"),
            key: "your-mix".into(),
            name: "Your Mix".into(),
            owner: "alice".into(),
            kind: LastFmRadioStationKind::YourMix,
            personalized: true,
            created_utc: Utc.with_ymd_and_hms(2026, 9, 1, 1, 0, 0).unwrap(),
            changed_utc: Utc.with_ymd_and_hms(2026, 9, 1, 2, 0, 0).unwrap(),
            valid_until_utc: Utc::now() + TimeDelta::days(1),
            tracks,
            ..Default::default()
        }],
    );
}

async fn walk(app: &App, kind: &str, page_size: usize, client: &str, extra: &str) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut offset = 0;
    loop {
        let counts = ["song", "album", "artist"]
            .iter()
            .map(|name| {
                let (count, at) = if *name == kind {
                    (page_size, offset)
                } else {
                    (0, 0)
                };
                format!("{name}Count={count}&{name}Offset={at}")
            })
            .collect::<Vec<_>>()
            .join("&");
        let body = get_string(
            app,
            &format!(
                "/rest/search3.view?query=%22%22&{counts}&u=alice&t=token&s=salt&v=1.13.0&c={client}&f=json{extra}"
            ),
        )
        .await;
        let document: Value = serde_json::from_str(&body).unwrap();
        let page = document["subsonic-response"]["searchResult3"][kind]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let got = page.len();
        rows.extend(page);
        if got < page_size {
            return rows;
        }
        offset += page_size;
        assert!(offset < 100_000, "walk never ended");
    }
}

fn ids(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn song_walk_returns_the_whole_library_then_the_catalog_once_each() {
    for (library_size, page_size) in [(2500, 1000), (2000, 1000), (12, 5), (0, 100)] {
        let fixture = fixture(library_size).await;
        install_stations(&fixture.state);

        let ids = ids(&walk(&fixture.app, "song", page_size, "Symfonium", "").await);

        let distinct: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(distinct.len(), ids.len(), "{library_size}/{page_size}");
        assert_eq!(
            fixture.upstream.library_song_ids(),
            ids[..library_size],
            "{library_size}/{page_size}"
        );
        let catalog = &ids[library_size..];
        assert_eq!(
            catalog.len(),
            expected_catalog_titles().len(),
            "{library_size}/{page_size}: {catalog:?}"
        );
        assert!(catalog.iter().all(|id| id.starts_with("ph-")), "{catalog:?}");
    }
}

#[tokio::test]
async fn song_walk_leaves_out_what_the_library_owns_and_files_under_the_librarys_artist_and_album() {
    let fixture = fixture(30).await;
    install_stations(&fixture.state);

    let catalog: Vec<Value> = walk(&fixture.app, "song", 1000, "Symfonium", "")
        .await
        .into_iter()
        .skip(30)
        .collect();
    let mut titles: Vec<String> = catalog
        .iter()
        .map(|row| row["title"].as_str().unwrap().to_string())
        .collect();
    assert!(!titles.contains(&"Owned Hit".to_string()));
    titles.sort();
    let mut expected = expected_catalog_titles();
    expected.sort();
    assert_eq!(titles, expected);

    let missing = catalog.iter().find(|row| row["title"] == "Missing Hit").unwrap();
    assert_eq!(missing["artistId"], "ar-owned");
    assert_eq!(missing["albumId"], "al-owned");

    let stranger = catalog.iter().find(|row| row["artist"] == "Stranger").unwrap();
    assert_ne!(stranger["artistId"], "ar-owned");
}

#[tokio::test]
async fn album_and_artist_walks_add_only_what_the_library_does_not_have_and_match_the_songs() {
    let fixture = fixture(30).await;
    install_stations(&fixture.state);

    let songs: Vec<Value> = walk(&fixture.app, "song", 1000, "Symfonium", "")
        .await
        .into_iter()
        .skip(30)
        .collect();
    let albums: Vec<Value> = walk(&fixture.app, "album", 500, "Symfonium", "")
        .await
        .into_iter()
        .skip(LIBRARY_ALBUM_COUNT)
        .collect();
    let artists: Vec<Value> = walk(&fixture.app, "artist", 500, "Symfonium", "")
        .await
        .into_iter()
        .skip(LIBRARY_ARTIST_COUNT)
        .collect();

    assert!(!ids(&albums).contains(&"al-owned".to_string()));
    assert!(!ids(&artists).contains(&"ar-owned".to_string()));
    let names: Vec<&str> = artists.iter().map(|row| row["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Stranger"]);

    let mut song_albums: Vec<String> = songs
        .iter()
        .map(|row| row["albumId"].as_str().unwrap().to_string())
        .filter(|id| id != "al-owned")
        .collect();
    song_albums.sort();
    song_albums.dedup();
    let mut album_ids = ids(&albums);
    album_ids.sort();
    assert_eq!(song_albums, album_ids);
}

#[tokio::test]
async fn other_clients_and_folder_walks_get_the_library_unchanged() {
    for (client, extra) in [("Feishin", ""), ("Symfonium", "&musicFolderId=1")] {
        let fixture = fixture(120).await;
        install_stations(&fixture.state);
        let ids = ids(&walk(&fixture.app, "song", 50, client, extra).await);
        assert_eq!(fixture.upstream.library_song_ids(), ids, "{client}{extra}");
    }
}

#[tokio::test]
async fn a_short_page_from_a_server_cap_is_not_mistaken_for_the_end_of_the_library() {
    let fixture = fixture(100).await;
    fixture.upstream.page_cap.store(30, Ordering::SeqCst);
    install_stations(&fixture.state);

    let ids = ids(&walk(&fixture.app, "song", 50, "Symfonium", "").await);

    assert_eq!(fixture.upstream.library_song_ids()[..30], ids);
}

#[tokio::test]
async fn station_playlist_describes_its_songs_as_the_sync_did() {
    let fixture = fixture(10).await;
    install_stations(&fixture.state);
    let synced = walk(&fixture.app, "song", 1000, "Symfonium", "")
        .await
        .into_iter()
        .skip(10)
        .find(|row| row["title"] == "Missing Hit")
        .expect("synced");

    let body = get_string(
        &fixture.app,
        &format!(
            "/rest/getPlaylist?id={}&u=alice&t=token&s=salt&f=json",
            station_id("alice", "your-mix")
        ),
    )
    .await;
    let document: Value = serde_json::from_str(&body).unwrap();
    let entry = document["subsonic-response"]["playlist"]["entry"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["title"] == "Missing Hit")
        .cloned()
        .expect("the station lists it");

    assert_eq!(synced["id"], entry["id"]);
    assert_eq!(entry["albumId"], "al-owned");
    assert_eq!(entry["artistId"], "ar-owned");
}
