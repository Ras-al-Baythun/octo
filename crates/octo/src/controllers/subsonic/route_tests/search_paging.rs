//! Paging through the real search3/search2 controller, and whose search order a page reads:
//! two API key users never share one, and a call with no identity neither reads nor writes one.
//!
//! A client scrolling a search's songs page by page. Page one is the library's best matches,
//! then outside songs; each later page has to carry on from there, where it used to go
//! straight to Navidrome and lose every outside song past page one.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, LastFmSettings, SubsonicSettings};
use octo_subsonic::xml::XElement;
use parking_lot::Mutex;
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::{TestMetadata, app, get_string, query_value};
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;
use crate::services::subsonic::{SearchSongOrder, SearchSongOrderCache};

fn library() -> Vec<String> {
    (0..30).map(|index| format!("lib-{index}")).collect()
}

/// Last.fm's answer. The third is a song the library has (lib-3).
fn outside() -> Vec<String> {
    (0..25)
        .map(|index| {
            if index == 2 {
                "Library Song 3".to_string()
            } else {
                format!("Outside Song {index}")
            }
        })
        .collect()
}

/// The library has 30 matches and Last.fm 25 outside songs, one of which (the third) is a
/// song the library already listed on page one, so the search leaves it out. The library
/// comes first and whole: with a 20-row page the search is l0-l19, the other 24 outside
/// songs, then l20-l29.
fn whole_search() -> Vec<String> {
    search_with_prefix(20)
}

/// The same search as Octo's apps see it: they keep the budget's split, so a 20-row page
/// is l0-l11, the other 24 outside songs, then l12-l29.
fn octo_app_search() -> Vec<String> {
    search_with_prefix(12)
}

fn search_with_prefix(prefix: usize) -> Vec<String> {
    let library = library();
    library
        .iter()
        .take(prefix)
        .cloned()
        .chain(
            outside()
                .into_iter()
                .enumerate()
                .filter(|(index, _)| *index != 2)
                .map(|(_, title)| format!("ph-{title}")),
        )
        .chain(library.iter().skip(prefix).cloned())
        .collect()
}

/// Navidrome with the library as the matches for any search, paged by songOffset/songCount in
/// either format and envelope, and Last.fm answering track.search with [`outside`].
#[derive(Clone, Default)]
struct Upstream {
    last_fm_calls: Arc<AtomicUsize>,
    token_info_calls: Arc<AtomicUsize>,
    song_pages: Arc<Mutex<Vec<(String, i32, i32)>>>,
    reshuffle: Arc<AtomicBool>,
    fail_later_song_pages: Arc<AtomicBool>,
}

impl Upstream {
    fn song_pages(&self, endpoint: &str) -> Vec<(i32, i32)> {
        self.song_pages
            .lock()
            .iter()
            .filter(|page| page.0 == endpoint)
            .map(|page| (page.1, page.2))
            .collect()
    }
}

fn ok(body: String, content_type: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, format!("{content_type}; charset=utf-8").as_str())
}

impl Respond for Upstream {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let path = request.url.path().trim_matches('/').to_string();
        if path.starts_with("2.0") {
            self.last_fm_calls.fetch_add(1, Ordering::SeqCst);
            if query_value(request, "method").as_deref() != Some("track.search") {
                return ok(r#"{"toptracks":{"track":[]}}"#.into(), "application/json");
            }
            let mut titles = outside();
            if self.reshuffle.load(Ordering::SeqCst) {
                titles.reverse();
            }
            let tracks: Vec<Value> = titles
                .iter()
                .map(|title| {
                    json!({
                        "name": title,
                        "artist": if title.starts_with("Library") { "Owned Artist" } else { "Outside Artist" },
                    })
                })
                .collect();
            return ok(
                json!({"results": {"trackmatches": {"track": tracks}}}).to_string(),
                "application/json",
            );
        }

        let endpoint = if path.starts_with("rest/search2") {
            Some("search2")
        } else if path.starts_with("rest/search3") {
            Some("search3")
        } else {
            None
        };
        let xml = query_value(request, "f").as_deref() == Some("xml");
        if path.starts_with("rest/tokenInfo") {
            self.token_info_calls.fetch_add(1, Ordering::SeqCst);
            let owners: HashMap<&str, &str> = [("alice-key", "alice"), ("bob-key", "bob")].into();
            let body = match query_value(request, "apiKey").and_then(|key| owners.get(key.as_str()).copied()) {
                Some(owner) => format!(
                    r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","tokenInfo":{{"username":"{owner}"}}}}}}"#
                ),
                None => r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":44,"message":"Invalid API key"}}}"#.into(),
            };
            return ok(body, "application/json");
        }
        let Some(endpoint) = endpoint else {
            return if xml {
                ok(
                    r#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"/>"#.into(),
                    "text/xml",
                )
            } else {
                ok(
                    r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.into(),
                    "application/json",
                )
            };
        };

        let number = |name: &str, fallback: i32| {
            query_value(request, name)
                .and_then(|v| v.parse().ok())
                .unwrap_or(fallback)
        };
        let offset = number("songOffset", 0);
        let count = number("songCount", 20);
        self.song_pages.lock().push((endpoint.to_string(), offset, count));
        if self.fail_later_song_pages.load(Ordering::SeqCst) && offset > 0 {
            return ResponseTemplate::new(503);
        }
        let ids: Vec<String> = library()
            .into_iter()
            .skip(offset.max(0) as usize)
            .take(count.max(0) as usize)
            .collect();
        let title = |id: &str| format!("Library Song {}", &id["lib-".len()..]);
        let envelope = if endpoint == "search2" {
            "searchResult2"
        } else {
            "searchResult3"
        };

        if xml {
            let ns = "http://subsonic.org/restapi";
            let mut result = XElement::ns(ns, envelope);
            for id in &ids {
                result.push(
                    XElement::ns(ns, "song")
                        .attr("id", id.as_str())
                        .attr("title", title(id))
                        .attr("artist", "Owned Artist"),
                );
            }
            let document = XElement::ns(ns, "subsonic-response")
                .attr("status", "ok")
                .attr("version", "1.16.1")
                .child(result);
            return ok(document.to_xml_string(), "text/xml");
        }
        let mut result = serde_json::Map::new();
        if !ids.is_empty() {
            result.insert(
                "song".into(),
                Value::Array(
                    ids.iter()
                        .map(|id| json!({"id": id, "title": title(id), "artist": "Owned Artist"}))
                        .collect(),
                ),
            );
        }
        ok(
            json!({"subsonic-response": {"status": "ok", "version": "1.16.1", envelope: result}}).to_string(),
            "application/json",
        )
    }
}

/// An Octo in front of a fake Navidrome and a fake Last.fm, for search paging.
struct Fixture {
    _server: MockServer,
    upstream: Upstream,
    metadata: Arc<TestMetadata>,
    state: AppState,
    app: App,
}

async fn fixture(discovery: bool) -> Fixture {
    let upstream = Upstream::default();
    let server = MockServer::builder().start().await;
    Mock::given(any())
        .respond_with(upstream.clone())
        .mount(&server)
        .await;
    let metadata = Arc::new(TestMetadata::answering(|artist, title, duration| {
        vec![Song {
            id: format!("ph-{title}"),
            artist: artist.into(),
            title: title.into(),
            album: String::new(),
            duration: Some(duration.unwrap_or(180)),
            is_local: false,
            external_provider: Some("outside".into()),
            ..Default::default()
        }]
    }));
    let state = AppState::for_tests_with(
        AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                auto_detect_download_path: false,
                enable_search_discovery: discovery,
                ..Default::default()
            },
            last_fm: LastFmSettings {
                api_key: "test-key".into(),
                ..Default::default()
            },
            ..Default::default()
        },
        TestServices {
            metadata: Some(metadata.clone()),
            last_fm_base_url: Some(format!("{}/2.0/", server.uri())),
            ..Default::default()
        },
    );
    Fixture {
        _server: server,
        upstream,
        metadata,
        app: app(state.clone()),
        state,
    }
}

async fn page(
    app: &App,
    offset: i32,
    count: i32,
    endpoint: &str,
    format: &str,
    user: &str,
    client: &str,
) -> Vec<String> {
    let body = get_string(
        app,
        &format!(
            "/rest/{endpoint}.view?query=paging&songCount={count}&songOffset={offset}&albumCount=0&artistCount=0\
             &u={user}&t=token&s=salt&v=1.16.1&c={client}&f={format}"
        ),
    )
    .await;
    song_ids(&body, endpoint, format)
}

fn song_ids(body: &str, endpoint: &str, format: &str) -> Vec<String> {
    if format == "xml" {
        let root = XElement::parse(body).expect("XML");
        let mut ids = Vec::new();
        fn walk(element: &XElement, ids: &mut Vec<String>) {
            for child in element.elements() {
                if child.name == "song" {
                    ids.push(child.attribute("id").unwrap().to_string());
                }
                walk(child, ids);
            }
        }
        walk(&root, &mut ids);
        return ids;
    }
    let document: Value = serde_json::from_str(body).expect("JSON");
    let envelope = if endpoint == "search2" {
        "searchResult2"
    } else {
        "searchResult3"
    };
    document["subsonic-response"][envelope]["song"]
        .as_array()
        .map(|songs| {
            songs
                .iter()
                .map(|song| song["id"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

async fn alice(app: &App, offset: i32, count: i32) -> Vec<String> {
    page(app, offset, count, "search3", "json", "alice", "Test").await
}

#[tokio::test]
async fn three_pages_show_the_whole_search_once_in_page_ones_order() {
    for (endpoint, format) in [
        ("search3", "json"),
        ("search3", "xml"),
        ("search2", "json"),
        ("search2", "xml"),
    ] {
        let fixture = fixture(true).await;
        let app = &fixture.app;
        let first = page(app, 0, 20, endpoint, format, "alice", "Test").await;
        let second = page(app, 20, 20, endpoint, format, "alice", "Test").await;
        let third = page(app, 40, 20, endpoint, format, "alice", "Test").await;
        let past = page(app, 60, 20, endpoint, format, "alice", "Test").await;

        // Page one is the library's own 20 best matches: no outside song takes a place while
        // the library has rows for it. The outside songs follow on the next pages, less the
        // one the library already has.
        let whole = whole_search();
        assert_eq!(first, whole[..20], "{endpoint} {format}");
        assert_eq!(
            first.len() + second.len() + third.len(),
            20 + 20 + 14,
            "{endpoint} {format}"
        );
        let all: Vec<String> = first.iter().chain(&second).chain(&third).cloned().collect();
        let distinct: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(distinct.len(), all.len(), "{endpoint} {format}");
        assert_eq!(all, whole, "{endpoint} {format}");
        assert!(past.is_empty(), "{endpoint} {format}: {past:?}");

        // The library rows after the prefix came from Navidrome at the right places.
        let asked = fixture.upstream.song_pages(endpoint);
        assert!(asked.contains(&(0, 20)), "{endpoint} {format}: {asked:?}");
        assert!(asked.contains(&(20, 16)), "{endpoint} {format}: {asked:?}");
    }
}

#[tokio::test]
async fn later_pages_use_page_ones_outside_songs_even_if_a_build_now_would_differ() {
    let fixture = fixture(true).await;
    let first = alice(&fixture.app, 0, 20).await;
    let calls = fixture.upstream.last_fm_calls.load(Ordering::SeqCst);
    fixture.upstream.reshuffle.store(true, Ordering::SeqCst);
    let second = alice(&fixture.app, 20, 20).await;

    assert_eq!(calls, fixture.upstream.last_fm_calls.load(Ordering::SeqCst));
    assert_eq!(second, whole_search()[20..40]);
    assert!(first.iter().all(|id| !second.contains(id)));
}

#[tokio::test]
async fn changing_the_page_size_still_shows_each_row_once() {
    let fixture = fixture(true).await;
    let mut all = alice(&fixture.app, 0, 20).await;
    let mut offset = 20;
    for size in [15, 30, 7, 50] {
        all.extend(alice(&fixture.app, offset, size).await);
        offset += size;
    }
    assert_eq!(all, whole_search());
}

/// Feishin's song search in Subsonic mode: the list asks for page one at its page size
/// while getSongListCount walks the same query 500 rows at a time from offset 0, advancing
/// by the rows it got. Both are page ones of one search; the list's next page must carry on
/// from the list's own page one, not the count's.
#[tokio::test]
async fn a_list_and_its_count_walk_keep_their_own_orders() {
    let fixture = fixture(true).await;
    // In the order that lost the list's page one: the count's page one lands after it.
    let first = alice(&fixture.app, 0, 50).await;
    let mut total = 0;
    loop {
        let rows = alice(&fixture.app, total, 500).await;
        if rows.is_empty() {
            break;
        }
        total += rows.len() as i32;
    }
    let second = alice(&fixture.app, 50, 50).await;
    let third = alice(&fixture.app, 100, 50).await;

    let all: Vec<String> = first.iter().chain(&second).chain(&third).cloned().collect();
    let distinct: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!(distinct.len(), all.len());
    let mut sorted = all.clone();
    sorted.sort();
    let mut expected = whole_search();
    expected.sort();
    assert_eq!(sorted, expected);
}

/// One person on two devices with different page sizes scrolls two lists.
#[tokio::test]
async fn two_clients_of_one_user_keep_their_own_orders() {
    let fixture = fixture(true).await;
    let app = &fixture.app;
    let phone = page(app, 0, 20, "search3", "json", "alice", "Phone").await;
    let desktop = page(app, 0, 15, "search3", "json", "alice", "Desktop").await;
    let phone_next = page(app, 20, 15, "search3", "json", "alice", "Phone").await;
    let desktop_next = page(app, 15, 15, "search3", "json", "alice", "Desktop").await;

    let whole = whole_search();
    assert_eq!(phone, whole[..20]);
    assert_eq!(phone_next, whole[20..35]);
    let desktop_all: Vec<&String> = desktop.iter().chain(&desktop_next).collect();
    let distinct: std::collections::HashSet<_> = desktop_all.iter().collect();
    assert_eq!(distinct.len(), desktop_all.len());
    assert_eq!(fixture.state.search_song_order_cache.count(), 2);
}

#[tokio::test]
async fn a_later_page_with_nothing_remembered_builds_the_same_order_again() {
    let fixture = fixture(true).await;
    let app = &fixture.app;
    // Bob never asked for page one (as after a restart): his page two is rebuilt from a
    // fresh build and the library's prefix, and lands where Alice's did.
    page(app, 0, 20, "search3", "json", "alice", "Test").await;
    let alice = page(app, 20, 20, "search3", "json", "alice", "Test").await;
    let bob = page(app, 20, 20, "search3", "json", "bob", "Test").await;
    assert_eq!(alice, bob);
}

#[tokio::test]
async fn a_type_ahead_sized_later_page_with_nothing_remembered_goes_to_navidrome_unchanged() {
    let fixture = fixture(true).await;
    let page = alice(&fixture.app, 10, 10).await;
    assert_eq!(page, library()[10..20]);
    assert_eq!(fixture.upstream.last_fm_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn with_discovery_off_later_pages_go_to_navidrome_unchanged() {
    let fixture = fixture(false).await;
    alice(&fixture.app, 0, 20).await;
    let second = alice(&fixture.app, 20, 20).await;
    assert_eq!(second, library()[20..30]);
}

/// Navidrome failing on a later page's library rows used to leave a page of outside songs
/// only, and the client never saw the rows it skipped. Now the page goes to Navidrome as it
/// used to, and the next try, once Navidrome answers, is the page it should be.
#[tokio::test]
async fn a_later_page_when_navidrome_fails_is_not_made_from_the_order_alone() {
    let fixture = fixture(true).await;
    alice(&fixture.app, 0, 20).await;
    fixture
        .upstream
        .fail_later_song_pages
        .store(true, Ordering::SeqCst);
    let failed = super::get(
        &fixture.app,
        "/rest/search3.view?query=paging&songCount=20&songOffset=40&albumCount=0&artistCount=0\
         &u=alice&t=token&s=salt&v=1.16.1&c=Test&f=json",
    )
    .await
    .body;
    fixture
        .upstream
        .fail_later_song_pages
        .store(false, Ordering::SeqCst);
    let second = alice(&fixture.app, 40, 20).await;

    assert!(!failed.contains("ph-"), "{failed}");
    assert_eq!(second, whole_search()[40..]);
}

/// A song the user owns is listed once, as the library's copy, before any outside song, and
/// the library is not cut short to make room for outside songs. A page with room to spare
/// fills the rest with outside songs, less the one already owned.
#[tokio::test]
async fn page_one_lists_the_library_first_and_an_owned_song_only_once() {
    for (count, locals, outsiders) in [(20, 20, 0), (40, 30, 9)] {
        let fixture = fixture(true).await;

        let first = alice(&fixture.app, 0, count).await;

        assert_eq!(first[..locals], library()[..locals], "songCount={count}");
        assert_eq!(first.len() - locals, outsiders, "songCount={count}");
        assert!(
            first[locals..].iter().all(|id| id.starts_with("ph-")),
            "songCount={count}: {first:?}"
        );
        assert!(
            !first.contains(&"ph-Library Song 3".to_string()),
            "songCount={count}"
        );
        let distinct: std::collections::HashSet<_> = first.iter().collect();
        assert_eq!(distinct.len(), first.len(), "songCount={count}");
    }
}

/// Octo's apps split library and outside songs into sections of their own, so page one keeps
/// the budget's share of outside songs for them, and their later pages carry on from it.
#[tokio::test]
async fn octo_apps_keep_outside_songs_on_page_one() {
    let fixture = fixture(true).await;
    let app = &fixture.app;
    let first = page(app, 0, 20, "search3", "json", "alice", "Octo").await;
    let second = page(app, 20, 20, "search3", "json", "alice", "Octo").await;

    let whole = octo_app_search();
    assert_eq!(first, whole[..19]);
    assert_eq!(second, whole[19..39]);
}

#[tokio::test]
async fn a_later_album_page_does_not_repeat_the_outside_albums() {
    let fixture = fixture(true).await;
    for offset in [0, 20] {
        get_string(
            &fixture.app,
            &format!(
                "/rest/search3.view?query=paging&songCount=0&albumCount=20&albumOffset={offset}\
                 &artistCount=0&u=alice&t=token&s=salt&v=1.16.1&c=Test&f=json"
            ),
        )
        .await;
    }
    assert_eq!(*fixture.metadata.album_searches.lock(), ["paging"]);
}

// ---------------------------------------------------------------------------------------
// The search order per caller (search3)
// ---------------------------------------------------------------------------------------

fn search(offset: i32, count: i32, auth: &str) -> String {
    format!(
        "/rest/search3.view?query=paging&songCount={count}&songOffset={offset}&albumCount=0&artistCount=0\
         {auth}&v=1.16.1&c=Test&f=json"
    )
}

fn key(user: &str) -> String {
    SearchSongOrderCache::key(user, "Test", "rest/search3", None, "paging")
}

#[tokio::test]
async fn two_api_key_users_never_share_a_search_order() {
    let fixture = fixture(true).await;
    let cache = &fixture.state.search_song_order_cache;

    get_string(&fixture.app, &search(0, 20, "&apiKey=alice-key")).await;
    get_string(&fixture.app, &search(0, 30, "&apiKey=bob-key")).await;
    get_string(&fixture.app, &search(20, 20, "&apiKey=alice-key")).await;

    assert_eq!(cache.get(&key("alice"), 20, 20).unwrap().page_one_count, 20);
    assert_eq!(cache.get(&key("bob"), 30, 30).unwrap().page_one_count, 30);
    assert!(cache.get(&key(""), 20, 20).is_none());
    assert_eq!(fixture.upstream.token_info_calls.load(Ordering::SeqCst), 2);
}

/// Nobody to file an order under: page one keeps nothing, and a later page reads nothing,
/// not even an order some other nameless request left, and is built again.
#[tokio::test]
async fn no_identity_neither_reads_nor_writes_the_search_order() {
    let fixture = fixture(true).await;
    let cache = &fixture.state.search_song_order_cache;

    get_string(&fixture.app, &search(0, 20, "&apiKey=unknown-key")).await;
    assert_eq!(cache.count(), 0);

    let planted = Song {
        id: "ph-planted".into(),
        artist: "Someone".into(),
        title: "Planted".into(),
        is_local: false,
        ..Default::default()
    };
    cache.set(
        &key(""),
        SearchSongOrder::from(Arc::new(vec![planted]), 20, 12, 8, &[]),
    );
    let anonymous = song_ids(
        &get_string(&fixture.app, &search(20, 20, "")).await,
        "search3",
        "json",
    );
    let named = song_ids(
        &get_string(&fixture.app, &search(20, 20, "&u=carol&t=token&s=salt")).await,
        "search3",
        "json",
    );

    assert!(!anonymous.contains(&"ph-planted".to_string()));
    assert_eq!(named, anonymous);
    assert!(cache.get(&key("carol"), 20, 20).is_some());
    assert_eq!(cache.count(), 2);
}
