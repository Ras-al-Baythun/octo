use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use octo_core::settings::{AppSettings, GeneratedPlaylistSettings, LastFmSettings};
use serde_json::json;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use octo_core::outside::routing::OutsideRouting;

use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::metadata::deezer_rate_limit_handler::DeezerRateLimitHandler;
use crate::services::metadata::deezer_rate_limiter::DeezerRateLimiter;
use crate::services::test_support::{Matching, Routes};

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

const DEEZER_CHART: &str = r#"{"data":[
  {"id":11,"title":"Dracula (with JENNIE)","duration":185,"explicit_lyrics":true,"explicit_content_lyrics":1,
   "artist":{"id":27,"name":"Tame Impala"},"album":{"id":302,"title":"Deadbeat","cover_xl":"https://cdn/xl.jpg"}},
  {"id":12,"title":"Choosin' Texas","duration":232,"explicit_lyrics":false,
   "artist":{"id":28,"name":"Ella Langley"},"album":{"id":303,"title":"Choosin' Texas","cover_xl":"https://cdn/ct.jpg"}}
]}"#;

struct Charts {
    service: TopSongsService,
    apple: MockServer,
    _deezer: MockServer,
    deezer_routes: Routes,
    hints: Arc<ArtworkHints>,
}

impl Charts {
    async fn new(country: &str, deezer_chart: Option<&str>, last_fm: Option<&MockServer>) -> Charts {
        let apple = MockServer::builder().start().await;
        let deezer_routes = Routes::new(
            deezer_chart
                .map(|body| vec![("/chart/0/tracks", vec![body])])
                .unwrap_or_default(),
            Matching::IgnoreCase,
        );
        let deezer_server = deezer_routes.serve().await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            generated_playlists: GeneratedPlaylistSettings {
                chart_country: country.to_string(),
                ..Default::default()
            },
            last_fm: LastFmSettings {
                api_key: if last_fm.is_some() {
                    "key".to_string()
                } else {
                    String::new()
                },
                ..Default::default()
            },
            ..Default::default()
        }));
        let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
        let deezer = DeezerMetadataService::with_base_url(http, Arc::clone(&settings), deezer_server.uri());
        let last_fm = last_fm.map(|server| {
            Arc::new(LastFmService::with_base_url(
                Arc::clone(&settings),
                &format!("{}/2.0/", server.uri()),
            ))
        });
        let hints = Arc::new(ArtworkHints::new());
        let service = TopSongsService::new(
            deezer,
            last_fm,
            Arc::new(FakeMetadata::default()),
            Arc::new(ExternalIdRegistry::in_memory()),
            Arc::new(AppleMusicPages::with_base_url(&apple.uri(), Duration::ZERO)),
            Arc::clone(&hints),
            settings,
        );
        Charts {
            service,
            apple,
            _deezer: deezer_server,
            deezer_routes,
            hints,
        }
    }

    async fn serve(&self, at: &str, genre: Option<&str>, body: &str) {
        let mock = Mock::given(method("GET")).and(path(at));
        let mock = match genre {
            Some(genre) => mock.and(query_param("genreId", genre)),
            None => mock.and(query_param_is_missing("genreId")),
        };
        mock.respond_with(ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "text/html"))
            .mount(&self.apple)
            .await;
    }

    async fn apple_down(&self) {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&self.apple)
            .await;
    }

    async fn apple_asked(&self) -> usize {
        self.apple.received_requests().await.map_or(0, |r| r.len())
    }

    async fn us_pages(&self) {
        self.serve("/us/new/top-charts/songs", None, fixture!("us_all"))
            .await;
        self.serve("/us/new/top-charts/songs/", Some("18"), fixture!("us_18"))
            .await;
        self.serve("/us/new", None, fixture!("us_new")).await;
        self.serve("/us/room/6818396128", None, fixture!("us_room_best_new"))
            .await;
        self.serve("/us/room/6818395867", None, fixture!("us_room_trending"))
            .await;
    }
}

fn titles(list: &TopList) -> Vec<&str> {
    list.songs.iter().map(|s| s.song.title.as_str()).collect()
}

#[tokio::test]
async fn the_chart_is_apples_in_apples_order() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("34").await;

    assert_eq!(list.source, "apple");
    assert_eq!(list.chart.as_deref(), Some("34"));
    assert_eq!(list.name.as_deref(), Some("Popular right now"));
    assert_eq!(list.country.as_deref(), Some("us"));
    assert_eq!(
        titles(&list),
        vec![
            "Solar Eclipse",
            "Quebec",
            "Cold Shoulder",
            "Choosin' Texas",
            "BbY WOW"
        ]
    );
    let ranks: Vec<usize> = list.songs.iter().map(|s| s.rank).collect();
    assert_eq!(ranks, vec![1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn a_genre_chart_is_named_top_and_the_genre() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("18").await;

    assert_eq!(list.name.as_deref(), Some("Top Hip-Hop/Rap"));
    assert_eq!(list.songs[0].song.title, "Janice STFU");
}

#[tokio::test]
async fn best_new_songs_comes_from_the_first_room_and_trending_from_the_second() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let best = charts.service.chart("new").await;
    let trending = charts.service.chart("trending").await;

    assert_eq!(best.name.as_deref(), Some("Best New Songs"));
    assert_eq!(best.room_id.as_deref(), Some("6818396128"));
    assert_eq!(best.songs[1].song.title, "Chelsea Boots (Bonus Track)");
    assert_eq!(trending.name.as_deref(), Some("Trending Songs"));
    assert_eq!(trending.songs[0].song.title, "Patient Zero");
}

#[tokio::test]
async fn a_collaboration_is_filed_under_its_main_artist() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("34").await;

    let solar = &list.songs[0].song;
    assert_eq!(solar.artist, "Drake");
    assert_eq!(solar.artists, vec!["Drake", "Don Toliver"]);
    assert_eq!(solar.primary_artist.as_deref(), Some("Drake"));
    assert_eq!(solar.album, "HABIBTI (FOMO)");
    assert!(!solar.is_local);
    assert_eq!(solar.external_id.as_deref(), Some(solar.id.as_str()));
    assert!(
        list.songs[1].song.artists.is_empty(),
        "one artist leaves the list empty"
    );
}

#[tokio::test]
async fn explicit_badge_marks_the_song_explicit() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("34").await;

    assert_eq!(list.songs[0].song.explicit_content_lyrics, Some(1));
    assert_eq!(list.songs[2].song.explicit_content_lyrics, Some(0));
}

#[tokio::test]
async fn chart_songs_leave_artwork_hints() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    let list = charts.service.chart("34").await;

    let song = &list.songs[0].song;
    let routing = OutsideRouting {
        kind: RoutingKind::Song,
        artist: Some(song.artist.clone()),
        title: Some(song.title.clone()),
        ..Default::default()
    };
    let url = charts.hints.for_routing(&routing).unwrap();
    assert!(url.ends_with("/1200x1200bb.jpg"), "{url}");
    assert_eq!(song.cover_art_url.as_deref(), Some(url.as_str()));
}

#[tokio::test]
async fn a_whole_list_is_kept_and_asked_once() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;

    charts.service.chart("18").await;
    let asked = charts.apple_asked().await;
    charts.service.chart("18").await;

    assert_eq!(charts.apple_asked().await, asked);
}

#[tokio::test]
async fn apple_down_the_overall_is_deezers() {
    let charts = Charts::new("us", Some(DEEZER_CHART), None).await;
    charts.apple_down().await;

    let list = charts.service.chart("34").await;

    assert_eq!(list.source, "deezer");
    assert_eq!(list.name.as_deref(), Some("Popular right now"));
    assert_eq!(titles(&list), vec!["Dracula (with JENNIE)", "Choosin' Texas"]);
    let fetch = charts
        .service
        .fetches()
        .into_iter()
        .find(|f| f.chart == "34")
        .unwrap();
    assert_eq!(fetch.source, "deezer");
    assert!(
        fetch
            .error
            .unwrap()
            .starts_with("Apple Music could not be reached")
    );
}

#[tokio::test]
async fn apple_and_deezer_down_the_overall_is_last_fms() {
    let last_fm = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("method", "chart.gettoptracks"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!({"tracks": {"track": [{"name": "Dracula", "playcount": "10", "artist": {"name": "Tame Impala"}}]}})
                .to_string(),
        ))
        .mount(&last_fm)
        .await;
    let charts = Charts::new("us", None, Some(&last_fm)).await;
    charts.apple_down().await;

    let list = charts.service.chart("34").await;

    assert_eq!(list.source, "lastfm");
    // The fake catalog had no hit for it, so the row is dropped, not invented.
    assert!(list.songs.is_empty());
    assert_eq!(charts.deezer_routes.calls("/chart/0/tracks"), 0);
}

#[tokio::test]
async fn a_genre_chart_never_falls_back() {
    let charts = Charts::new("us", Some(DEEZER_CHART), None).await;
    charts
        .serve("/us/new/top-charts/songs", None, fixture!("us_all"))
        .await;
    Mock::given(method("GET"))
        .and(path("/us/new/top-charts/songs/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&charts.apple)
        .await;

    let list = charts.service.chart("18").await;

    assert!(list.songs.is_empty());
    assert_eq!(list.source, "");
    assert_eq!(charts.deezer_routes.calls("/chart/0/tracks"), 0);
}

#[tokio::test]
async fn an_unknown_genre_is_empty_without_asking_apple() {
    let charts = Charts::new("us", None, None).await;
    charts.us_pages().await;
    charts.service.menu().await;
    let asked = charts.apple_asked().await;

    assert!(charts.service.chart("99999").await.songs.is_empty());
    assert!(charts.service.chart("pop").await.songs.is_empty());

    assert_eq!(charts.apple_asked().await, asked);
}

#[tokio::test]
async fn auto_country_uses_the_detected_one() {
    let charts = Charts::new("auto", None, None).await;
    charts
        .serve("/new/top-charts/songs", None, fixture!("us_all"))
        .await;
    charts
        .serve("/us/new/top-charts/songs/", Some("18"), fixture!("us_18"))
        .await;

    // Learned by the warm worker, not by a request.
    charts.service.learn().await;
    assert_eq!(charts.service.country().await.as_deref(), Some("us"));
    let list = charts.service.chart("18").await;

    assert_eq!(list.country.as_deref(), Some("us"));
    assert_eq!(list.songs[0].song.title, "Janice STFU");
}

#[tokio::test]
async fn a_request_never_waits_to_learn_the_country() {
    let charts = Charts::new("auto", None, None).await;
    Mock::given(method("GET"))
        .and(path("/new/top-charts/songs"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture!("us_all").as_bytes().to_vec(), "text/html")
                .set_delay(Duration::from_secs(3)),
        )
        .mount(&charts.apple)
        .await;

    let started = std::time::Instant::now();
    let (country, menu) = charts.service.menu().await;

    // Apple's own redirect and Octo's English genres serve until the country is learned behind.
    assert_eq!(country, None);
    assert!(menu.iter().any(|g| g.id == "18"));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the request waited on Apple"
    );
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(charts.service.country().await.as_deref(), Some("us"));
}

#[tokio::test]
async fn a_fixed_country_is_asked_for() {
    let charts = Charts::new("GB", None, None).await;
    charts
        .serve("/gb/new/top-charts/songs", None, fixture!("us_all"))
        .await;
    charts
        .serve("/gb/new/top-charts/songs/", Some("18"), fixture!("gb_18"))
        .await;

    let list = charts.service.chart("18").await;

    assert_eq!(list.songs[0].song.title, "Raindance");
    let asked = charts.apple.received_requests().await.unwrap();
    assert!(asked.iter().all(|r| r.url.path().starts_with("/gb/")));
}

#[tokio::test]
async fn names_give_the_playlist_and_the_chip() {
    let menu = english_menu();
    assert_eq!(
        TopSongsService::names(&menu, "34"),
        Some(("Popular right now".into(), "Top songs".into()))
    );
    assert_eq!(
        TopSongsService::names(&menu, "new"),
        Some(("Best New Songs".into(), "Best New Songs".into()))
    );
    assert_eq!(
        TopSongsService::names(&menu, "18"),
        Some(("Top Hip-Hop/Rap".into(), "Hip-Hop/Rap".into()))
    );
    assert_eq!(TopSongsService::names(&menu, "99999"), None);
}

fn top(rank: usize, artist: &str, title: &str) -> TopSong {
    TopSong {
        rank,
        song: Song {
            id: format!("out-{rank}"),
            artist: artist.into(),
            title: title.into(),
            ..Default::default()
        },
        plays: None,
        listeners: None,
    }
}

#[tokio::test]
async fn match_library_finds_the_same_recording_only_and_gives_a_library_song_to_one_row() {
    let rows = vec![
        top(1, "Air", "Sexy Boy"),
        top(
            2,
            "Daft Punk",
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers)",
        ),
        top(3, "Daft Punk", "Get Lucky (Radio Edit)"),
        top(4, "Daft Punk", "Around the World"),
    ];
    let asked = AtomicUsize::new(0);
    let found = TopSongsService::match_library(&rows, |query| {
        asked.fetch_add(1, Ordering::SeqCst);
        async move {
            match query.as_str() {
                "Air Sexy Boy" => Some(vec![
                    json!({"id": "nd-airbourne", "artist": "Airbourne", "title": "Sexy Boy"}),
                ]),
                "Daft Punk Get Lucky" => Some(vec![
                    json!({"id": "nd-lucky", "artist": "Daft Punk", "title": "Get Lucky"}),
                ]),
                "Daft Punk Around the World" => None,
                _ => Some(Vec::new()),
            }
        }
    })
    .await;

    assert!(found[0].is_none());
    assert_eq!(found[1].as_ref().unwrap()["id"], "nd-lucky");
    assert!(found[2].is_none());
    assert!(found[3].is_none());
    assert_eq!(asked.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn match_library_never_searches_more_than_four_at_once() {
    let rows: Vec<TopSong> = (1..=20).map(|i| top(i, "Artist", &format!("Song {i}"))).collect();
    let running = Arc::new(AtomicUsize::new(0));
    let most = Arc::new(AtomicUsize::new(0));
    TopSongsService::match_library(&rows, |_| {
        let (running, most) = (Arc::clone(&running), Arc::clone(&most));
        async move {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            running.fetch_sub(1, Ordering::SeqCst);
            Some(Vec::new())
        }
    })
    .await;
    assert!(most.load(Ordering::SeqCst) <= LIBRARY_SEARCHES_AT_ONCE);
}
