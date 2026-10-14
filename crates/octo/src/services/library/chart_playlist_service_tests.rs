use std::time::Duration;

use chrono::TimeZone;
use octo_core::settings::{AppSettings, GeneratedPlaylistSettings, SubsonicSettings};
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::*;
use crate::services::charts::apple_music_pages::AppleMusicPages;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::cover_art::ArtworkHints;
use crate::services::metadata::deezer_metadata_service::DeezerMetadataService;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/apple-music/",
            $name,
            ".html"
        ))
    };
}

const ALICE: &str = "u=alice&t=good&s=salt&v=1.16.1&c=test";

struct Lists {
    service: ChartPlaylistService,
    top_songs: TopSongsService,
    settings: Arc<SettingsStore>,
    navidrome: MockServer,
    _apple: MockServer,
    now: Arc<Mutex<DateTime<Utc>>>,
}

fn settings_for(navidrome: &str, charts: &str, discovery: bool, filter: ExplicitFilter) -> AppSettings {
    AppSettings {
        subsonic: SubsonicSettings {
            url: Some(navidrome.to_string()),
            enable_search_discovery: discovery,
            explicit_filter: filter,
            ..Default::default()
        },
        generated_playlists: GeneratedPlaylistSettings {
            charts: charts.to_string(),
            chart_country: "us".to_string(),
            ..Default::default()
        },
        ..Default::default()
    }
}

impl Lists {
    async fn new(charts: &str) -> Lists {
        Self::with(charts, true, ExplicitFilter::All).await
    }

    async fn with(charts: &str, discovery: bool, filter: ExplicitFilter) -> Lists {
        let navidrome = MockServer::builder().start().await;
        let apple = MockServer::start().await;
        for (at, genre, body) in [
            ("/us/new/top-charts/songs", None, fixture!("us_all")),
            ("/us/new/top-charts/songs/", Some("18"), fixture!("us_18")),
            ("/us/new", None, fixture!("us_new")),
            ("/us/room/6818396128", None, fixture!("us_room_best_new")),
            ("/us/room/6818395867", None, fixture!("us_room_trending")),
        ] {
            let mock = Mock::given(method("GET")).and(path(at));
            let mock = match genre {
                Some(genre) => mock.and(query_param("genreId", genre)),
                None => mock.and(query_param_is_missing("genreId")),
            };
            mock.respond_with(ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/html"))
                .mount(&apple)
                .await;
        }
        let settings = Arc::new(SettingsStore::for_tests(settings_for(
            &navidrome.uri(),
            charts,
            discovery,
            filter,
        )));
        let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
        let deezer = DeezerMetadataService::with_base_url(
            http,
            Arc::clone(&settings),
            format!("http://{}", crate::services::test_support::refused_address()),
        );
        let registry = Arc::new(ExternalIdRegistry::in_memory());
        let top_songs = TopSongsService::new(
            deezer,
            None,
            Arc::new(FakeMetadata::default()),
            Arc::clone(&registry),
            Arc::new(AppleMusicPages::with_base_url(&apple.uri(), Duration::ZERO)),
            Arc::new(ArtworkHints::new()),
            Arc::clone(&settings),
        );
        let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 7, 9, 0, 0).unwrap()));
        let clock_now = Arc::clone(&now);
        let service = ChartPlaylistService::new(
            top_songs.clone(),
            Arc::new(
                crate::services::subsonic::subsonic_response_builder::new_subsonic_response_builder(
                    Arc::clone(&registry),
                ),
            ),
            registry,
            SubsonicProxyService::new(Arc::clone(&settings)),
            Arc::clone(&settings),
            Clock::new(move || *clock_now.lock()),
        );
        Lists {
            service,
            top_songs,
            settings,
            navidrome,
            _apple: apple,
            now,
        }
    }

    /// Navidrome's search3: the songs for a query, or nothing for any other query.
    async fn library(&self, answers: Vec<(&str, Value)>) {
        let answers: HashMap<String, Value> = answers.into_iter().map(|(q, v)| (q.to_string(), v)).collect();
        Mock::given(method("GET"))
            .and(path("/rest/search3"))
            .respond_with(move |request: &Request| {
                let query = request
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "query")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                let songs = answers.get(&query).cloned().unwrap_or(json!([]));
                ResponseTemplate::new(200).set_body_json(json!({
                    "subsonic-response": {"status": "ok", "version": "1.16.1", "searchResult3": {"song": songs}}
                }))
            })
            .mount(&self.navidrome)
            .await;
    }

    async fn searches_for(&self, query: &str) -> usize {
        self.navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.url.query_pairs().any(|(k, v)| k == "query" && v == query))
            .count()
    }

    fn advance(&self, hours: i64) {
        let mut now = self.now.lock();
        *now += TimeDelta::hours(hours);
    }
}

fn auth() -> Parameters {
    ALICE
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn every_list_on_is_a_playlist_overall_first() {
    let lists = Lists::new("new,18,trending").await;
    lists.library(Vec::new()).await;
    lists.service.warm("alice", &auth()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;

    let listed = lists.service.list("alice", &auth()).await;

    let names: Vec<&str> = listed.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "Popular right now",
            "Best New Songs",
            "Top Hip-Hop/Rap",
            "Trending Songs"
        ]
    );
    assert!(
        listed
            .iter()
            .all(|p| p.kind == "chart" && p.owner == "alice" && p.id.starts_with("og"))
    );
    assert_eq!(listed[0].pool_size, 5);
}

#[tokio::test]
async fn the_overall_keeps_its_old_id_and_each_listener_has_their_own() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;

    let listed = lists.service.list("alice", &auth()).await;

    assert_eq!(listed[0].id, playlist_id("alice", "chart:popular"));
    assert_ne!(
        playlist_id("alice", "chart:popular"),
        playlist_id("bob", "chart:popular")
    );
    assert_eq!(
        lists.service.which("alice", &listed[0].id).as_deref(),
        Some(OVERALL)
    );
    assert_eq!(lists.service.which("bob", &listed[0].id), None);
}

#[tokio::test]
async fn library_songs_are_the_listeners_own_the_rest_outside() {
    let lists = Lists::new("").await;
    lists
        .library(vec![("Drake Quebec", json!([{"id": "nd-quebec", "artist": "Drake", "title": "Quebec", "starred": "2026-10-01T00:00:00Z"}]))])
        .await;

    let songs = lists.service.songs("alice", OVERALL, &auth()).await;

    assert_eq!(songs.len(), 5);
    assert_eq!(songs[1]["id"], "nd-quebec");
    assert_eq!(songs[1]["starred"], "2026-10-01T00:00:00Z");
    assert_eq!(songs[0]["title"], "Solar Eclipse");
    assert_ne!(songs[0]["id"], "nd-quebec");
    let outside = lists.service.outside_songs("alice", OVERALL);
    assert_eq!(outside.len(), 4);
    assert!(outside.iter().all(|s| !s.is_local));
}

#[tokio::test]
async fn a_song_on_two_lists_is_searched_once() {
    let lists = Lists::new("new").await;
    lists.library(Vec::new()).await;

    lists.service.songs("alice", OVERALL, &auth()).await;
    lists.service.songs("alice", "new", &auth()).await;

    // Solar Eclipse leads both Popular right now and Best New Songs.
    assert_eq!(lists.searches_for("Drake Solar Eclipse").await, 1);
}

#[tokio::test]
async fn library_searches_carry_only_the_sign_in() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    let mut asked = auth();
    asked.insert("offset".into(), "500".into());

    lists.service.songs("alice", OVERALL, &asked).await;

    let request = &lists.navidrome.received_requests().await.unwrap()[0];
    let keys: Vec<String> = request.url.query_pairs().map(|(k, _)| k.into_owned()).collect();
    assert!(!keys.contains(&"offset".to_string()));
    assert!(keys.contains(&"t".to_string()) && keys.contains(&"query".to_string()));
}

#[tokio::test]
async fn with_search_discovery_off_only_the_listeners_own_songs() {
    let lists = Lists::with("", false, ExplicitFilter::All).await;
    lists
        .library(vec![(
            "Drake Quebec",
            json!([{"id": "nd-quebec", "artist": "Drake", "title": "Quebec"}]),
        )])
        .await;

    let songs = lists.service.songs("alice", OVERALL, &auth()).await;

    assert_eq!(songs.len(), 1);
    assert_eq!(songs[0]["id"], "nd-quebec");
}

#[tokio::test]
async fn the_explicit_filter_leaves_out_what_it_should() {
    let lists = Lists::with("", true, ExplicitFilter::CleanOnly).await;
    lists.library(Vec::new()).await;

    let songs = lists.service.songs("alice", OVERALL, &auth()).await;

    // Solar Eclipse and Quebec carry Apple's explicit badge.
    let titles: Vec<&str> = songs.iter().filter_map(|s| s["title"].as_str()).collect();
    assert_eq!(titles, vec!["Cold Shoulder", "Choosin' Texas", "BbY WOW"]);
}

#[tokio::test]
async fn a_changed_explicit_filter_makes_the_list_again_on_the_next_ask() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    assert_eq!(lists.service.songs("alice", OVERALL, &auth()).await.len(), 5);

    let mut changed = (*lists.settings.current()).clone();
    changed.subsonic.explicit_filter = ExplicitFilter::CleanOnly;
    lists.settings.set(changed);

    let songs = lists.service.songs("alice", OVERALL, &auth()).await;
    let titles: Vec<&str> = songs.iter().filter_map(|s| s["title"].as_str()).collect();
    assert_eq!(titles, vec!["Cold Shoulder", "Choosin' Texas", "BbY WOW"]);
}

#[tokio::test]
async fn a_list_turned_off_is_gone_on_the_next_list() {
    let lists = Lists::new("18").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", "18", &auth()).await;
    assert_eq!(lists.service.list("alice", &auth()).await.len(), 2);

    let mut changed = (*lists.settings.current()).clone();
    changed.generated_playlists.charts = String::new();
    lists.settings.set(changed);

    let listed = lists.service.list("alice", &auth()).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "Popular right now");
}

#[tokio::test]
async fn a_genre_the_country_lacks_is_not_listed() {
    let lists = Lists::new("99").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;

    let listed = lists.service.list("alice", &auth()).await;

    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn a_list_is_listed_once_its_chart_is_known_while_discovery_is_on() {
    let lists = Lists::new("").await;
    // Navidrome is slow: the list cannot be made within the first look's wait.
    Mock::given(method("GET"))
        .and(path("/rest/search3"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&lists.navidrome)
        .await;
    lists.top_songs.chart(OVERALL).await;

    let listed = lists.service.list("alice", &auth()).await;

    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].pool_size, 5);
}

#[tokio::test]
async fn with_discovery_off_a_list_waits_until_made() {
    let lists = Lists::with("", false, ExplicitFilter::All).await;
    Mock::given(method("GET"))
        .and(path("/rest/search3"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(5)))
        .mount(&lists.navidrome)
        .await;
    lists.top_songs.chart(OVERALL).await;

    assert!(lists.service.list("alice", &auth()).await.is_empty());
}

#[tokio::test]
async fn a_library_that_stops_answering_leaves_the_last_list_alone() {
    let lists = Lists::new("").await;
    lists
        .library(vec![(
            "Drake Quebec",
            json!([{"id": "nd-quebec", "artist": "Drake", "title": "Quebec"}]),
        )])
        .await;
    let first = lists.service.songs("alice", OVERALL, &auth()).await;

    lists.navidrome.reset().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&lists.navidrome)
        .await;
    lists.advance(REFRESH_HOURS.into());
    let second = lists.service.songs("alice", OVERALL, &auth()).await;

    assert_eq!(first, second);
}

#[tokio::test]
async fn the_same_songs_keep_the_change_time() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;
    let first = lists.service.as_sync_stations("alice")[0].changed_utc;

    lists.advance(REFRESH_HOURS.into());
    lists.service.songs("alice", OVERALL, &auth()).await;
    let station = &lists.service.as_sync_stations("alice")[0];

    assert_eq!(station.changed_utc, first);
    assert!(station.created_utc > first);
}

#[tokio::test]
async fn every_list_rides_into_the_sync_catalog_under_its_own_ids() {
    let lists = Lists::new("new").await;
    lists.library(Vec::new()).await;
    lists.service.songs("alice", OVERALL, &auth()).await;

    let stations = lists.service.as_sync_stations("alice");

    assert_eq!(stations.len(), 2);
    assert_eq!(stations[0].id, playlist_id("alice", "chart:popular"));
    assert_eq!(stations[1].name, "Best New Songs");
    let outside = lists.service.outside_songs("alice", OVERALL);
    let track = &stations[0].tracks[0];
    assert_eq!(track.source, TRACK_SOURCE);
    assert_eq!(track.resolved_id.as_deref(), Some(outside[0].id.as_str()));
    assert_eq!(track.artist, "Drake");
}

#[tokio::test]
async fn keep_playable_makes_a_forgotten_outside_song_known_again_under_the_same_id() {
    let lists = Lists::new("").await;
    lists.library(Vec::new()).await;
    let songs = lists.service.songs("alice", OVERALL, &auth()).await;
    let outside = lists.service.outside_songs("alice", OVERALL);

    let fresh = Arc::new(ExternalIdRegistry::in_memory());
    let forgetful = ChartPlaylistService::new(
        lists.top_songs.clone(),
        Arc::new(
            crate::services::subsonic::subsonic_response_builder::new_subsonic_response_builder(Arc::clone(
                &fresh,
            )),
        ),
        Arc::clone(&fresh),
        SubsonicProxyService::new(Arc::clone(&lists.settings)),
        Arc::clone(&lists.settings),
        Clock::system(),
    );
    forgetful.keep_playable(&outside);

    assert!(fresh.lookup(&outside[0].id).is_some());
    assert_eq!(songs[0]["id"], outside[0].id.as_str());
}

#[test]
fn assemble_keeps_chart_order_the_librarys_copy_first() {
    let row = |title: &str, explicit: Option<i32>| TopSong {
        rank: 1,
        song: Song {
            id: format!("out-{title}"),
            title: title.into(),
            artist: "A".into(),
            explicit_content_lyrics: explicit,
            ..Default::default()
        },
        plays: None,
        listeners: None,
    };
    let rows = vec![
        row("One", Some(1)),
        row("Two", Some(3)),
        row("Three", Some(4)),
        row("Four", None),
    ];
    let library = vec![None, Some(json!({"id": "nd-two"})), None, None];
    let as_json = |s: &Song| json!({"id": s.id});

    let (all, _) = assemble(&rows, &library, true, ExplicitFilter::All, as_json);
    let ids: Vec<&str> = all.iter().filter_map(|e| e["id"].as_str()).collect();
    assert_eq!(ids, vec!["out-One", "nd-two", "out-Three", "out-Four"]);

    let (clean, outside) = assemble(&rows, &library, true, ExplicitFilter::CleanOnly, as_json);
    let ids: Vec<&str> = clean.iter().filter_map(|e| e["id"].as_str()).collect();
    assert_eq!(
        ids,
        vec!["nd-two", "out-Four"],
        "4 counts as explicit; library songs are never filtered"
    );
    assert_eq!(outside.len(), 1);

    let (none_outside, _) = assemble(&rows, &library, false, ExplicitFilter::All, as_json);
    assert_eq!(none_outside.len(), 1);
}

#[tokio::test]
async fn the_warm_worker_fetches_every_list_that_is_on_overall_first() {
    let lists = Lists::new("new,18,99").await;
    let worker = crate::services::charts::ChartsWarmWorker::new(
        lists.top_songs.clone(),
        lists.service.clone(),
        Clock::system(),
    );

    let fetched = worker.warm_once().await;

    assert_eq!(
        fetched, 3,
        "Popular right now, Best New Songs and Top Hip-Hop/Rap; 99 is no genre here"
    );
    assert!(lists.top_songs.peek_chart("new").await.is_some());
    assert!(
        lists
            .navidrome
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty(),
        "no listener, no library search"
    );
}

#[tokio::test]
async fn the_warm_worker_does_nothing_with_every_list_off() {
    let lists = Lists::new("").await;
    let mut off = (*lists.settings.current()).clone();
    off.generated_playlists.popular_now = false;
    lists.settings.set(off);
    let worker = crate::services::charts::ChartsWarmWorker::new(
        lists.top_songs.clone(),
        lists.service.clone(),
        Clock::system(),
    );

    assert_eq!(worker.warm_once().await, 0);
    assert!(
        lists
            ._apple
            .received_requests()
            .await
            .unwrap_or_default()
            .is_empty()
    );
}
