//! The generated playlist service end to end against a stand-in Navidrome: counting, listing, the
//! draw a playlist read returns, and the Discovery blend.

use std::collections::HashMap;
use std::sync::Arc;

use octo_core::settings::{AppSettings, SubsonicSettings};
use parking_lot::Mutex;
use tempfile::TempDir;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;

/// A stand-in Navidrome that records the calls it gets.
#[derive(Clone, Default)]
struct LibraryNavidrome {
    calls: Arc<Mutex<Vec<String>>>,
}

impl LibraryNavidrome {
    fn genre_calls(&self) -> usize {
        self.calls
            .lock()
            .iter()
            .filter(|call| call.starts_with("rest/getGenres"))
            .count()
    }
}

fn ok(inner: &str) -> String {
    format!(
        r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1"{}}}}}"#,
        if inner.is_empty() {
            String::new()
        } else {
            format!(",{inner}")
        }
    )
}

fn songs(count: usize, first: usize, play_count: i32) -> String {
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
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path().trim_matches('/').to_string();
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        self.calls
            .lock()
            .push(format!("{path}?{}", request.url.query().unwrap_or_default()));
        let body = match path.as_str() {
            "rest/getGenres" => {
                ok(r#""genres":{"genre":[{"value":"Rock","songCount":40},{"value":"Polka","songCount":3}]}"#)
            }
            "rest/getSongsByGenre" => {
                let first = if query.get("offset").map(String::as_str) == Some("0") {
                    0
                } else {
                    1000
                };
                ok(&format!(r#""songsByGenre":{{"song":[{}]}}"#, songs(40, first, 5)))
            }
            "rest/getRandomSongs" => match query.get("fromYear").map(String::as_str) {
                Some("1990") => ok(&format!(r#""randomSongs":{{"song":[{}]}}"#, songs(25, 2000, 5))),
                None => ok(&format!(r#""randomSongs":{{"song":[{}]}}"#, songs(30, 3000, 0))),
                Some(_) => ok(r#""randomSongs":{"song":[]}"#),
            },
            // The whole library, only to the admin: 30 songs by an artist Navidrome has a
            // MusicBrainz id for, 25 by one it has none for, 5 by one nobody knows.
            "rest/search3" if query.get("u").map(String::as_str) == Some("admin") => {
                let rows: Vec<String> = (0..60)
                    .map(|i| {
                        let (artist, id) = match i {
                            0..30 => ("Rosalía", "arES"),
                            30..55 => ("Bad Bunny", "arPR"),
                            _ => ("Nobody Knows", "arXX"),
                        };
                        format!(
                            r#"{{"id":"c{i}","title":"Song {i}","artist":"{artist}","artistId":"{id}","playCount":9,"starred":"2026-01-01T00:00:00Z"}}"#
                        )
                    })
                    .collect();
                ok(&format!(r#""searchResult3":{{"song":[{}]}}"#, rows.join(",")))
            }
            // A listener's library as they see it, with their own plays and ratings: alice holds
            // 20 of the Rosalía songs and 20 of the Bad Bunny ones, bob only Bad Bunny's 25.
            "rest/search3" if query.get("songOffset").map(String::as_str) == Some("0") => {
                let alice = query.get("u").map(String::as_str) == Some("alice");
                let rows: Vec<String> = (0..60)
                    .filter(|i| if alice { (0..20).contains(i) || (30..50).contains(i) } else { (30..55).contains(i) })
                    .map(|i| {
                        let (artist, id) = if i < 30 { ("Rosalía", "arES") } else { ("Bad Bunny", "arPR") };
                        format!(
                            r#"{{"id":"c{i}","title":"Song {i}","artist":"{artist}","artistId":"{id}","playCount":2,"played":"2026-09-01T00:00:00Z","userRating":4}}"#
                        )
                    })
                    .collect();
                ok(&format!(r#""searchResult3":{{"song":[{}]}}"#, rows.join(",")))
            }
            "rest/getArtists" => ok(
                r#""artists":{"index":[{"name":"B","artist":[{"id":"arPR","name":"Bad Bunny","musicBrainzId":""}]},{"name":"R","artist":[{"id":"arES","name":"Rosalía","musicBrainzId":"mb-es"}]}]}"#,
            ),
            _ => ok(""),
        };
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "application/json")
            .set_body_string(body)
    }
}

struct Fixture {
    dir: TempDir,
    server: MockServer,
    navidrome: LibraryNavidrome,
}

async fn fixture() -> Fixture {
    let navidrome = LibraryNavidrome::default();
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(navidrome.clone())
        .mount(&server)
        .await;
    Fixture {
        dir: TempDir::new().unwrap(),
        server,
        navidrome,
    }
}

fn service(fixture: &Fixture, settings: GeneratedPlaylistSettings) -> Arc<GeneratedPlaylistService> {
    let store = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(fixture.server.uri()),
            ..Default::default()
        },
        generated_playlists: settings,
        ..Default::default()
    }));
    Arc::new(GeneratedPlaylistService::new(
        Some(fixture.dir.path().join("generated-playlists.json")),
        SubsonicProxyService::new(store.clone()),
        store,
    ))
}

fn auth() -> IndexMap<String, String> {
    [
        ("u", "alice"),
        ("t", "token"),
        ("s", "salt"),
        ("v", "1.16.1"),
        ("c", "test"),
        ("id", "not-for-navidrome"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn enabled() -> GeneratedPlaylistSettings {
    GeneratedPlaylistSettings {
        enabled: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn list_counts_the_library_and_offers_what_clears_the_threshold() {
    let fixture = fixture().await;
    let service = service(&fixture, enabled());

    let mixes = service.list("alice", &auth()).await;

    let names: Vec<&str> = mixes.iter().map(|mix| mix.name.as_str()).collect();
    assert_eq!(names, ["Rock Mix", "1990s Mix"]);
    assert!(mixes.iter().all(|mix| mix.owner == "alice"));
    assert!(
        !fixture
            .navidrome
            .calls
            .lock()
            .iter()
            .any(|call| call.contains("id=not-for-navidrome"))
    );
    assert_eq!(service.find("alice", &mixes[0].id), Some(mixes[0].clone()));
    assert!(service.find("bob", &mixes[0].id).is_none());
}

#[tokio::test]
async fn list_while_fresh_does_not_count_again_and_survives_a_restart() {
    let fixture = fixture().await;
    service(&fixture, enabled()).list("alice", &auth()).await;
    service(&fixture, enabled()).list("alice", &auth()).await;

    let restarted = service(&fixture, enabled());
    let mixes = restarted.list("alice", &auth()).await;

    assert_eq!(fixture.navidrome.genre_calls(), 1);
    assert_eq!(mixes.len(), 2);
}

#[tokio::test]
async fn list_off_is_nothing() {
    let fixture = fixture().await;
    assert!(
        service(&fixture, GeneratedPlaylistSettings::default())
            .list("alice", &auth())
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn materialize_draws_from_the_genre_and_holds_still_for_the_period() {
    let fixture = fixture().await;
    let service = service(
        &fixture,
        GeneratedPlaylistSettings {
            enabled: true,
            track_count: 10,
            max_per_artist: 1,
            ..Default::default()
        },
    );
    let rock = service
        .list("alice", &auth())
        .await
        .into_iter()
        .find(|mix| mix.kind == "genre")
        .expect("the rock mix");

    let none = CancellationToken::new();
    let first = service.materialize("alice", &rock, &auth(), &none).await.unwrap();
    let second = service.materialize("alice", &rock, &auth(), &none).await.unwrap();

    assert_eq!(first.len(), 10);
    let mut artists: Vec<&str> = first
        .iter()
        .map(|song| song.get("artist").and_then(Node::as_str).unwrap())
        .collect();
    artists.sort();
    artists.dedup();
    assert_eq!(artists.len(), 10);
    let text = |songs: &[Node]| {
        songs
            .iter()
            .map(|song| song.to_json_string(false))
            .collect::<Vec<_>>()
    };
    assert_eq!(text(&first), text(&second));
    assert_eq!(
        fixture
            .navidrome
            .calls
            .lock()
            .iter()
            .filter(|call| call.starts_with("rest/getSongsByGenre"))
            .count(),
        1
    );
    assert_eq!(service.drawn("alice", &rock).map(|songs| songs.len()), Some(10));
}

fn ids(songs: &[Song]) -> Vec<String> {
    songs.iter().map(|song| song.id.clone()).collect()
}

#[tokio::test]
async fn blend_only_discovery_and_only_with_a_share() {
    let fixture = fixture().await;
    let songs: Vec<Song> = (0..10)
        .map(|i| Song {
            id: format!("st{i}"),
            artist: format!("A{i}"),
            title: format!("T{i}"),
            ..Default::default()
        })
        .collect();
    let discovery = LastFmRadioStation {
        id: "orStation".into(),
        kind: LastFmRadioStationKind::Discovery,
        changed_utc: Utc::now(),
        valid_until_utc: Utc::now() + TimeDelta::hours(6),
        ..Default::default()
    };
    let your_mix = LastFmRadioStation {
        id: "orMix".into(),
        kind: LastFmRadioStationKind::YourMix,
        changed_utc: Utc::now(),
        ..Default::default()
    };

    let none = service(&fixture, GeneratedPlaylistSettings::default());
    assert_eq!(
        ids(&none
            .blend_into_discovery("alice", &discovery, songs.clone(), &auth())
            .await),
        ids(&songs)
    );

    let shared = service(
        &fixture,
        GeneratedPlaylistSettings {
            new_share: 20,
            ..Default::default()
        },
    );
    assert_eq!(
        ids(&shared
            .blend_into_discovery("alice", &your_mix, songs.clone(), &auth())
            .await),
        ids(&songs)
    );
    let blended = shared
        .blend_into_discovery("alice", &discovery, songs.clone(), &auth())
        .await;
    assert_eq!(blended.iter().filter(|song| song.is_local).count(), 2);
}

/// The file written is the compact state document, a bad file is set aside as `.corrupt-<ticks>`,
/// and the query keeps the caller's credentials in their order.
#[tokio::test]
async fn the_state_file_is_written_and_a_bad_one_is_set_aside() {
    let fixture = fixture().await;
    let path = fixture.dir.path().join("generated-playlists.json");
    service(&fixture, enabled()).list("Alice", &auth()).await;
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.starts_with(r#"{"Users":{"alice":{"Active":["genre:Rock","decade:1990"],"Counts":{"genre:Rock":40,"genre:Polka":3,"decade:1950":0"#), "{written}");

    let first = fixture.navidrome.calls.lock()[0].clone();
    assert_eq!(
        first,
        "rest/getGenres?u=alice&t=token&s=salt&v=1.16.1&c=test&f=json"
    );

    std::fs::write(&path, "not json").unwrap();
    let reloaded = service(&fixture, enabled());
    assert!(reloaded.find("alice", "og").is_none());
    let names: Vec<String> = std::fs::read_dir(fixture.dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names
            .iter()
            .any(|name| name.starts_with("generated-playlists.json.corrupt-")),
        "{names:?}"
    );
}

/// What a client is told about a mix and its songs, and that a library MP3 is an MP3.
mod response {
    use chrono::TimeZone;
    use octo_subsonic::subsonic_response_builder::{ReplyKind, SubsonicResponseBuilder};
    use serde_json::Value;

    use super::*;
    use crate::services::outside::ExternalIdRegistry;
    use crate::services::subsonic::subsonic_response_builder::{
        SubsonicResponseBuilderExt, new_subsonic_response_builder,
    };

    fn builder() -> SubsonicResponseBuilder {
        new_subsonic_response_builder(Arc::new(ExternalIdRegistry::in_memory()))
    }

    fn mix() -> GeneratedPlaylist {
        GeneratedPlaylist {
            id: format!("og{}", "a".repeat(20)),
            key: "genre:Rock".into(),
            kind: "genre".into(),
            label: "Rock".into(),
            name: "Rock Mix".into(),
            owner: "alice".into(),
            pool_size: 40,
            period_start_utc: Utc.with_ymd_and_hms(2026, 9, 25, 0, 0, 0).unwrap(),
            period_end_utc: Utc.with_ymd_and_hms(2026, 9, 26, 0, 0, 0).unwrap(),
        }
    }

    fn entry() -> Node {
        Node::parse(
            r#"{"id":"lib1","title":"Track","artist":"Artist","duration":200,"playCount":3,"genres":[{"name":"Rock"}],"replayGain":{"trackGain":-6.1}}"#,
        )
        .unwrap()
    }

    #[test]
    fn row_is_read_only_and_owned_by_the_listener() {
        let fields = builder().generated_playlist_fields(
            &mix(),
            &GeneratedPlaylistSettings {
                track_count: 100,
                ..Default::default()
            },
        );

        assert_eq!(fields["name"], "Rock Mix");
        assert_eq!(fields["owner"], "alice");
        assert_eq!(fields["readonly"], true);
        assert_eq!(fields["songCount"], 40);
        let version = octo_media::cover::CoverBook::shipped().version;
        assert_eq!(
            fields["coverArt"],
            Value::String(format!("{}_{version}c40", mix().id))
        );
    }

    #[test]
    fn playlist_json_passes_navidromes_songs_through() {
        let reply = builder().create_generated_playlist_response(
            "json",
            &mix(),
            &GeneratedPlaylistSettings::default(),
            &[entry()],
        );
        assert_eq!(reply.kind, ReplyKind::Json);
        let document: Value = serde_json::from_str(&reply.text()).unwrap();
        let playlist = &document["subsonic-response"]["playlist"];

        assert_eq!(playlist["songCount"], 1);
        assert_eq!(playlist["duration"], 200);
        let song = &playlist["entry"][0];
        assert_eq!(song["id"], "lib1");
        assert_eq!(song["replayGain"]["trackGain"], -6.1);
    }

    #[test]
    fn playlist_xml_carries_every_scalar_as_an_attribute() {
        let reply = builder().create_generated_playlist_response(
            "xml",
            &mix(),
            &GeneratedPlaylistSettings::default(),
            &[entry()],
        );
        assert_eq!(reply.kind, ReplyKind::Content);
        let text = reply.text();
        let playlist = text.lines().nth(1).unwrap();
        let entry = text.lines().nth(2).unwrap();

        assert!(playlist.contains(r#"readonly="true""#), "{playlist}");
        assert!(entry.contains(r#"id="lib1""#), "{entry}");
        assert!(entry.contains(r#"duration="200""#), "{entry}");
        assert!(!entry.contains("genres="), "{entry}");
    }

    /// A strict client prepares its decoder from these; an MP3 declared as FLAC fails.
    #[test]
    fn convert_song_local_mp3_is_declared_as_mp3() {
        let mp3 = builder().convert_song_to_json(&Song {
            id: "l1".into(),
            artist: "A".into(),
            title: "T".into(),
            is_local: true,
            suffix: Some("MP3".into()),
            bit_rate: Some(320),
            duration: Some(100),
            ..Default::default()
        });
        let unknown = builder().convert_song_to_json(&Song {
            id: "l2".into(),
            artist: "A".into(),
            title: "T".into(),
            is_local: true,
            duration: Some(100),
            ..Default::default()
        });

        assert_eq!(mp3["suffix"], "mp3");
        assert_eq!(mp3["contentType"], "audio/mpeg");
        assert_eq!(mp3["bitRate"], 320);
        assert_eq!(unknown["suffix"], "flac");
        assert_eq!(unknown["bitRate"], 1411);
    }
}

// ---- Marks put on drawn lists ----------------------------------------------------------------

fn song_node(text: &str) -> Node {
    Node::parse(text).expect("JSON")
}

#[tokio::test]
async fn the_overlay_puts_hearts_on_library_songs_and_leaves_outside_songs_alone() {
    let fixture = fixture().await;
    let lists = service(&fixture, GeneratedPlaylistSettings::default());
    let mut songs = vec![
        song_node(r#"{"id":"a","title":"A","starred":"2026-01-01T00:00:00Z","userRating":2}"#),
        song_node(r#"{"id":"b","title":"B"}"#),
        song_node(r#"{"id":"x","title":"X","isExternal":true,"starred":"2026-02-02T00:00:00Z"}"#),
    ];
    let marks: MarksById = [(
        "b".to_string(),
        Marks {
            starred: Some("2026-10-09T10:00:00Z".into()),
            rating: Some(5),
        },
    )]
    .into_iter()
    .collect();

    lists.overlay("alice", &mut songs, &marks);

    // Unhearted since: the heart goes, and a rating nobody knows is left as drawn.
    assert!(songs[0].get("starred").is_none());
    assert_eq!(songs[0].get("userRating").and_then(node_i64), Some(2));
    assert_eq!(
        songs[1].get("starred").and_then(Node::as_str),
        Some("2026-10-09T10:00:00Z")
    );
    assert_eq!(songs[1].get("userRating").and_then(node_i64), Some(5));
    // Navidrome does not hold Octo's own hearts on outside songs.
    assert_eq!(
        songs[2].get("starred").and_then(Node::as_str),
        Some("2026-02-02T00:00:00Z")
    );
}

#[tokio::test]
async fn a_rating_set_through_octo_reaches_unhearted_songs_and_zero_clears_it() {
    let fixture = fixture().await;
    let lists = service(&fixture, GeneratedPlaylistSettings::default());
    let mut songs = vec![
        song_node(r#"{"id":"a","userRating":2}"#),
        song_node(r#"{"id":"b"}"#),
    ];

    lists.rated("alice", "a", 0);
    lists.rated("alice", "b", 3);
    lists.overlay("alice", &mut songs, &MarksById::new());

    assert!(songs[0].get("userRating").is_none());
    assert_eq!(songs[1].get("userRating").and_then(node_i64), Some(3));
    // Someone else's ratings are theirs.
    let mut bobs = vec![song_node(r#"{"id":"b"}"#)];
    lists.overlay("bob", &mut bobs, &MarksById::new());
    assert!(bobs[0].get("userRating").is_none());
}

#[tokio::test]
async fn a_rating_set_through_octo_lapses_after_a_minute() {
    let fixture = fixture().await;
    let lists = service(&fixture, GeneratedPlaylistSettings::default());
    lists.rated("alice", "b", 3);

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    let mut songs = vec![song_node(r#"{"id":"b"}"#)];
    lists.overlay("alice", &mut songs, &MarksById::new());

    assert!(songs[0].get("userRating").is_none());
}

#[tokio::test]
async fn a_fresh_read_that_disagrees_wins_over_a_rating_set_through_octo() {
    let fixture = fixture().await;
    let lists = service(&fixture, GeneratedPlaylistSettings::default());
    lists.rated("alice", "a", 4);
    // Read since: hearted, and unrated on Navidrome (which leaves a rating of 0 out).
    let marks: MarksById = [(
        "a".to_string(),
        Marks {
            starred: Some("2026-10-09T10:00:00Z".into()),
            rating: None,
        },
    )]
    .into_iter()
    .collect();
    let mut songs = vec![song_node(r#"{"id":"a"}"#)];

    lists.overlay("alice", &mut songs, &marks);

    assert!(songs[0].get("userRating").is_none());
}

// ---- Country mixes ---------------------------------------------------------------------------

/// MusicBrainz as a table, counting what it is asked.
#[derive(Default)]
struct Countries {
    asked: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl super::super::artist_country::CountrySource for Countries {
    async fn by_mbid(&self, mbid: &str) -> Option<String> {
        self.asked.lock().push(format!("id {mbid}"));
        Some(if mbid == "mb-es" { "ES" } else { "" }.to_string())
    }

    async fn by_name(&self, name: &str) -> Option<String> {
        self.asked.lock().push(format!("name {name}"));
        Some(if name == "Bad Bunny" { "PR" } else { "" }.to_string())
    }
}

fn country_service(
    fixture: &Fixture,
    settings: GeneratedPlaylistSettings,
    source: &Arc<Countries>,
) -> Arc<GeneratedPlaylistService> {
    use super::super::artist_country::{ArtistCountryLookup, CountryLibrary, CountrySource, STORE_FILE};
    let store = Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic: SubsonicSettings {
            url: Some(fixture.server.uri()),
            ..Default::default()
        },
        generated_playlists: settings,
        ..Default::default()
    }));
    let library = Arc::new(CountryLibrary::new(
        Arc::new(ArtistCountryLookup::new(
            Arc::clone(source) as Arc<dyn CountrySource>,
            Some(fixture.dir.path().join(STORE_FILE)),
        )),
        SubsonicProxyService::new(store.clone()),
        Arc::new(|| {
            Some(
                [
                    ("u", "admin"),
                    ("t", "t"),
                    ("s", "s"),
                    ("v", "1.16.1"),
                    ("c", "octo"),
                ]
                .into_iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect(),
            )
        }),
    ));
    Arc::new(
        GeneratedPlaylistService::new(
            Some(fixture.dir.path().join("generated-playlists.json")),
            SubsonicProxyService::new(store.clone()),
            store,
        )
        .with_countries(library),
    )
}

fn countries_only() -> GeneratedPlaylistSettings {
    GeneratedPlaylistSettings {
        enabled: true,
        genres: false,
        decades: false,
        country_mixes: true,
        new_releases: false,
        rediscover: false,
        deep_cuts: false,
        track_count: 10,
        max_per_artist: 3,
        ..Default::default()
    }
}

/// The listener's mix names, once a background pass has made them (or after five seconds).
async fn names_once_counted(service: &Arc<GeneratedPlaylistService>) -> Vec<String> {
    for _ in 0..100 {
        let names: Vec<String> = service
            .list("alice", &auth())
            .await
            .into_iter()
            .map(|mix| mix.name)
            .collect();
        if !names.is_empty() {
            return names;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Vec::new()
}

#[tokio::test]
async fn country_mixes_come_from_where_the_artists_are_from_and_keep_the_artist_cap() {
    let fixture = fixture().await;
    let source = Arc::new(Countries::default());
    let service = country_service(&fixture, countries_only(), &source);

    let names = names_once_counted(&service).await;

    assert_eq!(names, ["Spain Mix", "Puerto Rico Mix"]);
    // Rosalía by her id, Bad Bunny and the unknown artist by name; each once.
    let mut asked = source.asked.lock().clone();
    asked.sort();
    assert_eq!(asked, ["id mb-es", "name Bad Bunny", "name Nobody Knows"]);
    // One walk as the admin, whoever asked.
    let calls = fixture.navidrome.calls.lock().clone();
    let walked = calls
        .iter()
        .filter(|call| call.starts_with("rest/search3") && call.contains("u=admin"))
        .count();
    assert_eq!(walked, 1);

    let spain = service
        .find("alice", &playlist_id("alice", "country:ES"))
        .expect("Spain Mix");
    assert_eq!(
        (spain.kind.as_str(), spain.label.as_str(), spain.pool_size),
        ("country", "Spain", 20)
    );
    let songs = service
        .materialize("alice", &spain, &auth(), &CancellationToken::new())
        .await
        .expect("drawn");
    // Every song is Rosalía's, so the artist cap of 3 is the mix.
    assert_eq!(songs.len(), 3);
    assert!(songs.iter().all(|song| str_of(song, "artist") == Some("Rosalía")));
    // The admin's own plays and hearts never reach a listener; the listener's own do.
    assert!(songs.iter().all(|song| {
        song.get("starred").is_none() && song.get("playCount").and_then(node_i64) == Some(2)
    }));
}

#[tokio::test]
async fn country_mixes_are_counted_and_drawn_from_the_listeners_own_songs_and_plays() {
    let fixture = fixture().await;
    let source = Arc::new(Countries::default());
    let service = country_service(&fixture, countries_only(), &source);
    names_once_counted(&service).await;

    // bob holds no Rosalía song, so he has no Spain Mix, and his Puerto Rico Mix counts his own.
    let mut bobs = Vec::new();
    for _ in 0..100 {
        bobs = service.list("bob", &bob()).await;
        if !bobs.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let bobs: Vec<(&str, i32)> = bobs
        .iter()
        .map(|mix| (mix.name.as_str(), mix.pool_size))
        .collect();
    assert_eq!(bobs, [("Puerto Rico Mix", 25)]);

    let spain = service
        .find("alice", &playlist_id("alice", "country:ES"))
        .expect("Spain Mix");
    let songs = service
        .materialize("alice", &spain, &auth(), &CancellationToken::new())
        .await
        .expect("drawn");
    assert!(!songs.is_empty());
    for song in &songs {
        let at: usize = str_of(song, "id").unwrap()[1..].parse().unwrap();
        assert!(at < 20, "not one of alice's songs: {song:?}");
        assert_eq!(song.get("playCount").and_then(node_i64), Some(2));
        assert_eq!(song.get("userRating").and_then(node_i64), Some(4));
        assert_eq!(str_of(song, "played"), Some("2026-09-01T00:00:00Z"));
    }
}

fn bob() -> IndexMap<String, String> {
    let mut auth = auth();
    auth.insert("u".into(), "bob".into());
    auth
}

#[tokio::test]
async fn only_the_countries_asked_for_get_a_mix() {
    let fixture = fixture().await;
    let source = Arc::new(Countries::default());
    let service = country_service(
        &fixture,
        GeneratedPlaylistSettings {
            countries: "pr".into(),
            ..countries_only()
        },
        &source,
    );

    assert_eq!(names_once_counted(&service).await, ["Puerto Rico Mix"]);
}

#[tokio::test]
async fn with_country_mixes_off_the_library_is_never_walked_and_musicbrainz_never_asked() {
    let fixture = fixture().await;
    let source = Arc::new(Countries::default());
    let service = country_service(&fixture, enabled(), &source);

    let names: Vec<String> = service
        .list("alice", &auth())
        .await
        .into_iter()
        .map(|mix| mix.name)
        .collect();
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(names, ["Rock Mix", "1990s Mix"]);
    assert!(source.asked.lock().is_empty());
    assert!(
        !fixture
            .navidrome
            .calls
            .lock()
            .iter()
            .any(|call| call.contains("u=admin") || call.starts_with("rest/getArtists"))
    );
    assert!(!fixture.dir.path().join("artist-countries.json").exists());
}
