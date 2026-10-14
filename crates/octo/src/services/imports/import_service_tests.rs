//! Importing end to end, with Spotify, its embed pages and Navidrome all stood in for: the
//! sign-in and its pasted address, reading liked songs and playlists (someone else's through its
//! public page), have and missing, the trickle queue, the playlist, and a fetched song finding
//! its place.

use std::sync::Arc;

use octo_core::settings::{AppSettings, ImportSettings};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request as Seen, Respond, ResponseTemplate};

use super::super::import_playlists::PlaylistNavidrome;
use super::super::spotify_link_reader::tests::embed_page;
use super::super::test_support::{FakeNavidrome, row};
use super::super::trickle_queue::tests::moving_clock;
use super::super::trickle_worker::{OwnedSong, TrickleSeams};
use super::*;
use crate::services::library::quality_upgrade_worker::LibrarySongRow;

const THEIRS: &str = "37i9dQZF1DXcBWIGoYBM5M";

fn json(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

/// The token endpoint: refused with the error set, else a token; counts the renewals.
struct Tokens {
    error: Arc<Mutex<String>>,
    refreshes: Arc<Mutex<i32>>,
}

impl Respond for Tokens {
    fn respond(&self, request: &Seen) -> ResponseTemplate {
        if String::from_utf8_lossy(&request.body).contains("grant_type=refresh_token") {
            *self.refreshes.lock() += 1;
        }
        let error = self.error.lock().clone();
        if !error.is_empty() {
            return json(400, &format!(r#"{{"error":"{error}"}}"#));
        }
        json(
            200,
            r#"{"access_token":"at","refresh_token":"rt2","expires_in":3600}"#,
        )
    }
}

/// The liked songs, or a fault inside Octo (an answer that is not JSON).
struct Liked {
    fault: Arc<Mutex<bool>>,
}

impl Respond for Liked {
    fn respond(&self, _: &Seen) -> ResponseTemplate {
        if *self.fault.lock() {
            return ResponseTemplate::new(200).set_body_string(
                "Could not find file '/data/config/imports.json'.\n   at Octo.Services.Imports.ImportStore.Save()",
            );
        }
        json(
            200,
            r#"{"total":2,"next":null,"items":[
              {"track":{"type":"track","id":"liked00000000000000001","name":"Teardrop","duration_ms":330000,"artists":[{"name":"Massive Attack"}],"album":{"name":"Mezzanine"}}},
              {"track":{"type":"track","id":"liked00000000000000002","name":"Angel","duration_ms":379000,"artists":[{"name":"Massive Attack"}],"album":{"name":"Mezzanine"}}}]}"#,
        )
    }
}

struct Rig {
    server: MockServer,
    now: Arc<Mutex<DateTime<Utc>>>,
    settings: Arc<SettingsStore>,
    store: Arc<ImportStore>,
    queue: Arc<TrickleQueue>,
    accounts: Arc<SpotifyAccountStore>,
    trickle: Arc<TrickleWorker>,
    service: Arc<ImportService>,
    library: Arc<Mutex<Vec<LibrarySongRow>>>,
    navidrome: Arc<FakeNavidrome>,
    token_error: Arc<Mutex<String>>,
    refreshes: Arc<Mutex<i32>>,
    fault: Arc<Mutex<bool>>,
}

fn settings(client_id: &str) -> AppSettings {
    AppSettings {
        imports: ImportSettings {
            spotify_client_id: client_id.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

async fn rig() -> Rig {
    let server = MockServer::builder().start().await;
    let token_error = Arc::new(Mutex::new(String::new()));
    let refreshes = Arc::new(Mutex::new(0));
    let fault = Arc::new(Mutex::new(false));
    Mock::given(method("POST"))
        .and(path("/api/token"))
        .respond_with(Tokens {
            error: token_error.clone(),
            refreshes: refreshes.clone(),
        })
        .mount(&server)
        .await;
    Mock::given(path("/v1/me"))
        .respond_with(json(200, r#"{"id":"me","display_name":"Alice on Spotify"}"#))
        .mount(&server)
        .await;
    Mock::given(path("/v1/me/tracks"))
        .respond_with(Liked { fault: fault.clone() })
        .mount(&server)
        .await;
    Mock::given(path("/v1/me/playlists"))
        .respond_with(json(
            200,
            &format!(
                r#"{{"next":null,"items":[
                  {{"id":"mine0000000000000000001","name":"Road trip","owner":{{"id":"me"}},"snapshot_id":"s1","items":{{"total":2}}}},
                  {{"id":"{THEIRS}","name":"Today's hits","owner":{{"id":"spotify","display_name":"Spotify"}},"snapshot_id":"s9","items":{{"total":3}}}}]}}"#
            ),
        ))
        .mount(&server)
        .await;
    Mock::given(path("/v1/playlists/mine0000000000000000001/items"))
        .respond_with(json(
            200,
            r#"{"total":2,"next":null,"items":[
              {"item":{"type":"track","id":"road00000000000000001","name":"Halo","duration_ms":261000,"artists":[{"name":"Beyoncé"}]}},
              {"item":{"type":"track","id":"liked00000000000000002","name":"Angel","duration_ms":379000,"artists":[{"name":"Massive Attack"}]}}]}"#,
        ))
        .mount(&server)
        .await;
    Mock::given(path(format!("/v1/playlists/{THEIRS}/items")))
        .respond_with(json(403, r#"{"error":{"status":403,"message":"Forbidden"}}"#))
        .mount(&server)
        .await;
    Mock::given(path(format!("/embed/playlist/{THEIRS}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(embed_page(3, "Today's hits"), "text/html"))
        .mount(&server)
        .await;

    let (now, clock) = moving_clock();
    let settings = Arc::new(SettingsStore::for_tests(settings("client-1")));
    let store = Arc::new(ImportStore::default());
    let queue = Arc::new(TrickleQueue::default());
    let accounts = Arc::new(SpotifyAccountStore::default());
    let trickle = TrickleWorker::new(queue.clone(), settings.clone(), TrickleSeams::default());
    let library = Arc::new(Mutex::new(vec![
        row(
            "nd-teardrop",
            "Massive Attack",
            "Teardrop",
            Some(331),
            Some("Mezzanine"),
            "flac",
            900,
        ),
        row("nd-halo", "Beyonce", "Halo", Some(261), None, "mp3", 320),
    ]));
    let pages = library.clone();
    let matcher = Arc::new(ImportMatcher::with_pages(Arc::new(move |start, count| {
        let page: Vec<LibrarySongRow> = pages
            .lock()
            .iter()
            .skip(start as usize)
            .take(count as usize)
            .cloned()
            .collect();
        let n = page.len();
        Box::pin(async move { Some((page, n)) })
    })));
    let navidrome = Arc::new(FakeNavidrome::default());
    *navidrome.admin.lock() = "alice".into();
    let spotify = SpotifyWebApi::with_urls(
        SpotifyWebApi::client(),
        &format!("{}/v1", server.uri()),
        &format!("{}/api/token", server.uri()),
    );
    spotify.set_wait(Arc::new(|_| Box::pin(async {})));
    spotify.set_clock(clock.clone());
    let service = ImportService::new(ImportParts {
        store: store.clone(),
        queue: queue.clone(),
        trickle: trickle.clone(),
        matcher,
        playlists: ImportPlaylists::new(navidrome.clone() as Arc<dyn PlaylistNavidrome>),
        auth: Arc::new(SpotifyAuth::default()),
        accounts: accounts.clone(),
        spotify: Arc::new(spotify),
        links: Arc::new(SpotifyLinkReader::with_base(
            SpotifyLinkReader::client(),
            &server.uri(),
        )),
        settings: settings.clone(),
        tracker: None,
    });
    service.set_clock(clock);
    Rig {
        server,
        now,
        settings,
        store,
        queue,
        accounts,
        trickle,
        service,
        library,
        navidrome,
        token_error,
        refreshes,
        fault,
    }
}

impl Rig {
    async fn connect_and_read(&self) -> ImportOverview {
        let begin = self.service.begin_spotify("alice", None);
        assert!(begin.ok, "{}", begin.message);
        let url = url::Url::parse(begin.url.as_deref().unwrap_or_default()).expect("a url");
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("a state");
        let finish = self
            .service
            .finish_spotify(
                Some(&format!("http://127.0.0.1/callback?code=the-code&state={state}")),
                None,
                None,
                Some("alice"),
            )
            .await
            .expect("finished");
        assert!(finish.ok, "{}", finish.message);
        self.wait_for_read().await
    }

    async fn wait_for_read(&self) -> ImportOverview {
        for _ in 0..1000 {
            if !self.service.overview("alice").reading.busy {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.service.overview("alice")
    }

    async fn calls(&self) -> Vec<String> {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|r| format!("{} {}", r.method, r.url.path()))
            .collect()
    }

    fn list<'a>(overview: &'a ImportOverview, name: &str) -> &'a ListSummary {
        overview.lists.iter().find(|l| l.name == name).expect("the list")
    }
}

#[tokio::test]
async fn without_a_client_id_the_sign_in_says_what_to_add() {
    let rig = rig().await;
    rig.settings.set(settings(""));
    let begin = rig.service.begin_spotify("alice", None);
    assert!(!begin.ok);
    assert!(begin.message.contains("Client ID"));
    assert!(!rig.service.overview("alice").spotify.configured);
}

#[tokio::test]
async fn an_app_signs_in_on_its_own_loopback_port() {
    let rig = rig().await;
    let begin = rig
        .service
        .begin_spotify("alice", Some("http://127.0.0.1:51234/callback"));
    assert!(begin.ok);
    assert!(
        begin
            .url
            .unwrap_or_default()
            .contains(&escape("http://127.0.0.1:51234/callback"))
    );
    assert!(
        !rig.service
            .begin_spotify("alice", Some("http://192.168.1.2:51234/callback"))
            .ok
    );
}

fn escape(text: &str) -> String {
    octo_core::common::dotnet::escape_data_string(text)
}

#[tokio::test]
async fn connecting_reads_liked_songs_and_playlists_and_says_what_the_library_has() {
    let rig = rig().await;
    let overview = rig.connect_and_read().await;
    assert_eq!(overview.reading.error, None);
    assert!(overview.spotify.connected);
    assert_eq!(overview.spotify.account.as_deref(), Some("Alice on Spotify"));
    assert_eq!(
        overview.spotify.ends_utc,
        Some(*rig.now.lock() + ImportService::SIGN_IN_LASTS)
    );

    let liked = overview
        .lists
        .iter()
        .find(|l| l.source == import_sources::SPOTIFY_LIKED)
        .expect("liked");
    assert_eq!((liked.total, liked.have, liked.missing), (2, 1, 1));
    let road = Rig::list(&overview, "Road trip");
    assert_eq!((road.have, road.missing), (1, 1));
    assert_eq!(road.partial, None);

    // Someone else's playlist: Spotify hides its songs, its public page lists them.
    let theirs = Rig::list(&overview, "Today's hits");
    assert_eq!(theirs.missing, 3);
    assert_eq!(theirs.by.as_deref(), Some("Spotify"));
    assert_eq!(theirs.partial, None);
    assert!(
        rig.calls()
            .await
            .iter()
            .any(|c| c.ends_with(&format!("/embed/playlist/{THEIRS}")))
    );

    let detail = rig.service.detail("alice", &liked.id).expect("detail");
    let teardrop = detail
        .tracks
        .iter()
        .find(|t| t.title == "Teardrop")
        .expect("teardrop");
    assert_eq!(teardrop.library_id.as_deref(), Some("nd-teardrop"));
    let angel = detail.tracks.iter().find(|t| t.title == "Angel").expect("angel");
    assert_eq!(angel.state, import_track_states::MISSING);
}

#[tokio::test]
async fn an_unchanged_playlist_is_not_read_again() {
    let rig = rig().await;
    rig.connect_and_read().await;
    let before = rig.calls().await.len();
    rig.service.read_spotify("alice").await;
    let calls = rig.calls().await;
    assert!(
        !calls[before..]
            .iter()
            .any(|c| c.contains("/playlists/mine0000000000000000001/items"))
    );
    let overview = rig.service.overview("alice");
    assert_eq!(Rig::list(&overview, "Road trip").have, 1);
}

#[tokio::test]
async fn a_sign_in_is_used_once_and_only_by_whoever_started_it() {
    let rig = rig().await;
    let begin = rig.service.begin_spotify("alice", None);
    let url = url::Url::parse(begin.url.as_deref().unwrap_or_default()).expect("a url");
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned());
    let bob = rig
        .service
        .finish_spotify(None, Some("c"), state.as_deref(), Some("bob"))
        .await
        .expect("answered");
    assert!(!bob.ok);
    // Refused for bob, and spent: a state answers once.
    let alice = rig
        .service
        .finish_spotify(None, Some("c"), state.as_deref(), Some("alice"))
        .await
        .expect("answered");
    assert!(!alice.ok);
    let denied = rig
        .service
        .finish_spotify(
            Some("http://127.0.0.1/callback?error=access_denied"),
            None,
            None,
            Some("alice"),
        )
        .await
        .expect("answered");
    assert!(denied.message.contains("not allowed"));
}

#[tokio::test]
async fn an_ended_sign_in_is_shown_and_nothing_is_read_with_it() {
    let rig = rig().await;
    rig.connect_and_read().await;
    *rig.now.lock() += TimeDelta::hours(2);
    *rig.token_error.lock() = "invalid_grant".into();
    rig.service.read_spotify("alice").await;
    let overview = rig.service.overview("alice");
    assert!(!overview.spotify.connected);
    assert!(
        overview
            .spotify
            .problem
            .unwrap_or_default()
            .contains("Connect again")
    );
    assert!(
        overview
            .reading
            .error
            .unwrap_or_default()
            .contains("Connect again")
    );
}

#[tokio::test]
async fn a_fault_while_reading_is_shown_with_no_server_path() {
    let rig = rig().await;
    rig.connect_and_read().await;
    *rig.fault.lock() = true;
    rig.service.read_spotify("alice").await;
    let error = rig.service.overview("alice").reading.error.unwrap_or_default();
    assert!(error.starts_with("Reading Spotify failed:"), "{error}");
    assert!(!error.contains("/data/config"), "{error}");
    assert!(!error.contains("ImportStore"), "{error}");
}

#[tokio::test]
async fn an_access_token_is_renewed_when_it_ends_keeping_the_new_refresh_token() {
    let rig = rig().await;
    rig.connect_and_read().await;
    *rig.now.lock() += TimeDelta::hours(2);
    rig.service.read_spotify("alice").await;
    assert_eq!(*rig.refreshes.lock(), 1);
    assert_eq!(rig.accounts.get("alice").expect("account").refresh_token, "rt2");
}

#[tokio::test]
async fn fetching_a_list_queues_its_missing_songs_and_stopping_takes_them_back() {
    let rig = rig().await;
    let overview = rig.connect_and_read().await;
    let liked = overview
        .lists
        .iter()
        .find(|l| l.source == import_sources::SPOTIFY_LIKED)
        .expect("liked")
        .id
        .clone();
    let road = Rig::list(&overview, "Road trip").id.clone();

    assert_eq!(rig.service.set_get_missing("alice", &liked, true).count, 1);
    // Angel is the only song Road trip misses, and Liked Songs already queued it.
    assert_eq!(rig.service.set_get_missing("alice", &road, true).count, 0);
    assert_eq!(rig.queue.snapshot(Some("alice")).len(), 1);
    let detail = rig.service.detail("alice", &road).expect("detail");
    let angel = detail.tracks.iter().find(|t| t.title == "Angel").expect("angel");
    assert_eq!(angel.state, import_track_states::QUEUED);

    // Angel is still wanted by Road trip, so stopping Liked Songs leaves it queued.
    assert_eq!(rig.service.set_get_missing("alice", &liked, false).count, 0);
    assert_eq!(rig.service.set_get_missing("alice", &road, false).count, 1);
    assert!(rig.queue.snapshot(Some("alice")).is_empty());
}

#[tokio::test]
async fn a_playlist_is_kept_and_a_fetched_song_takes_its_place() {
    let rig = rig().await;
    let overview = rig.connect_and_read().await;
    let road = Rig::list(&overview, "Road trip").id.clone();
    let made = rig.service.set_playlist("alice", &road, true).await;
    assert!(made.ok, "{}", made.message);
    assert_eq!(rig.navidrome.playlist("pl1"), ["nd-halo"]);
    let overview = rig.service.overview("alice");
    assert_eq!(
        overview
            .lists
            .iter()
            .find(|l| l.id == road)
            .and_then(|l| l.playlist_id.clone())
            .as_deref(),
        Some("pl1")
    );

    // The trickle brings Angel; the next pass matches it and puts it in its place.
    rig.service.set_get_missing("alice", &road, true);
    rig.library.lock().push(row(
        "nd-angel",
        "Massive Attack",
        "Angel",
        Some(379),
        Some("Mezzanine"),
        "flac",
        900,
    ));
    rig.trickle.configure(|seams| {
        seams.owned = Arc::new(|_| Box::pin(async { Ok(Some(OwnedSong(Some("nd-angel".into())))) }));
        seams.people_downloading = Arc::new(|| false);
    });
    rig.trickle.tick().await;
    rig.trickle.drain().await;
    rig.service.loop_once().await.expect("a pass");

    assert_eq!(rig.navidrome.playlist("pl1"), ["nd-halo", "nd-angel"]);
    let overview = rig.service.overview("alice");
    let list = overview.lists.iter().find(|l| l.id == road).expect("road");
    assert_eq!((list.have, list.missing), (2, 0));
}

#[tokio::test]
async fn a_playlist_gone_from_the_account_stays_while_it_is_kept() {
    let rig = rig().await;
    let overview = rig.connect_and_read().await;
    let road = Rig::list(&overview, "Road trip").id.clone();
    let hits = Rig::list(&overview, "Today's hits").id.clone();
    rig.service.set_get_missing("alice", &road, true);
    // Both disappear from the account.
    rig.store
        .update("alice", &road, |l| l.id = "spotify-renamed-away".into());
    rig.store
        .update("alice", &hits, |l| l.id = "spotify-also-away".into());
    rig.service.read_spotify("alice").await;
    let lists = rig.service.overview("alice").lists;
    assert!(
        lists
            .iter()
            .find(|l| l.id == "spotify-renamed-away")
            .expect("kept")
            .gone
    );
    assert!(!lists.iter().any(|l| l.id == "spotify-also-away"));
}

#[tokio::test]
async fn a_list_comes_from_a_link_or_a_file_with_no_sign_in() {
    let rig = rig().await;
    rig.settings.set(settings(""));
    let link = rig
        .service
        .add_link("bob", &format!("https://open.spotify.com/playlist/{THEIRS}?si=x"))
        .await
        .expect("answered");
    assert!(link.ok, "{}", link.message);
    assert_eq!(link.count, 3);

    let csv =
        "Track Name,Artist Name(s),Track Duration (ms)\nTeardrop,Massive Attack,330000\nNope,Nobody,1000\n";
    let file = rig.service.add_file("bob", "Gym.csv", csv.as_bytes()).await;
    assert!(file.ok, "{}", file.message);
    let overview = rig.service.overview("bob");
    let gym = Rig::list(&overview, "Gym");
    assert_eq!((gym.have, gym.missing), (1, 1));
    assert!(!gym.can_refresh);
    assert!(rig.service.overview("alice").lists.is_empty());

    let bad = rig.service.add_file("bob", "x.csv", b"a,b\n1,2\n").await;
    assert!(!bad.ok);
}

#[tokio::test]
async fn one_person_keeps_at_most_500_lists_and_a_file_of_the_same_name_still_replaces_its_list() {
    let rig = rig().await;
    rig.settings.set(settings(""));
    for i in 0..ImportService::MAX_LISTS {
        rig.store.save(ImportList {
            id: format!("file-{i}"),
            owner: "bob".into(),
            name: format!("List {i}"),
            source: import_sources::FILE.into(),
            ..Default::default()
        });
    }
    let csv = "Track Name,Artist Name(s)\nTeardrop,Massive Attack\n";
    let refused = rig.service.add_file("bob", "Gym.csv", csv.as_bytes()).await;
    assert!(!refused.ok);
    assert_eq!(
        refused.message,
        "That would make more than 500 lists, the most Octo keeps for one person. Remove some first."
    );
    assert!(rig.service.add_file("alice", "Gym.csv", csv.as_bytes()).await.ok);
}

#[test]
fn the_redirect_says_whether_octo_finishes_itself() {
    assert!(ImportService::octo_finishes(
        "https://octo.example.com/imports/spotify/callback"
    ));
    assert!(!ImportService::octo_finishes("http://127.0.0.1/callback"));
}

// ---- "Keep as playlist" after a failure -------------------------------------------------------

/// A playlist Navidrome would not take is tried again with a growing wait, not every pass, and
/// never leaves another empty playlist behind.
#[tokio::test]
async fn a_playlist_navidrome_would_not_take_waits_longer_each_time() {
    let rig = rig().await;
    let csv = "Track Name,Artist Name(s)\nTeardrop,Massive Attack\n";
    let added = rig.service.add_file("alice", "Gym.csv", csv.as_bytes()).await;
    let id = added.list_id.expect("an id");
    *rig.navidrome.fail_add.lock() = true;

    let refused = rig.service.set_playlist("alice", &id, true).await;
    assert!(!refused.ok);
    assert!(rig.navidrome.playlists.lock().is_empty(), "nothing left behind");
    assert_eq!(rig.store.get("alice", &id).and_then(|l| l.playlist_id), None);
    let creates = || {
        rig.navidrome
            .calls()
            .iter()
            .filter(|c| c.starts_with("create"))
            .count()
    };
    assert_eq!(creates(), 1);

    // Passes inside the first minute try nothing.
    for _ in 0..3 {
        *rig.now.lock() += TimeDelta::seconds(15);
        rig.service.loop_once().await.expect("a pass");
    }
    assert_eq!(creates(), 1);
    // After it, one try; then two minutes before the next.
    *rig.now.lock() += TimeDelta::seconds(30);
    rig.service.loop_once().await.expect("a pass");
    assert_eq!(creates(), 2);
    *rig.now.lock() += TimeDelta::seconds(90);
    rig.service.loop_once().await.expect("a pass");
    assert_eq!(creates(), 2);
    *rig.now.lock() += TimeDelta::seconds(31);
    rig.service.loop_once().await.expect("a pass");
    assert_eq!(creates(), 3);
    assert!(rig.navidrome.playlists.lock().is_empty());

    // Once Navidrome takes it, the playlist is made and the waits are over.
    *rig.navidrome.fail_add.lock() = false;
    let made = rig.service.set_playlist("alice", &id, true).await;
    assert!(made.ok, "{}", made.message);
    assert_eq!(rig.navidrome.playlists.lock().len(), 1);
}

#[tokio::test]
async fn a_listeners_list_is_one_request_until_an_approver_says_yes() {
    use crate::services::family::test_support::{World, member};
    use crate::services::imports::ImportFamily;
    use octo_core::family::abilities::Preset;
    use octo_core::family::request::{RequestKind, RequestState};

    let rig = rig().await;
    let overview = rig.connect_and_read().await;
    let road = Rig::list(&overview, "Road trip").id.clone();
    let world = World::new(None);
    world.family.upsert_member(member("alice", Preset::Listener, 6));
    let (requests, _fetcher) = world.list_requests();
    rig.service.use_family(ImportFamily {
        gate: world.gate(),
        requests: Arc::downgrade(&requests),
    });

    let asked = rig.service.set_get_missing("alice", &road, true);
    assert!(asked.ok, "{}", asked.message);
    assert!(asked.message.starts_with("Asked for a copy"), "{}", asked.message);
    assert!(
        rig.queue.snapshot(Some("alice")).is_empty(),
        "nothing fetched yet"
    );
    let made = requests.requests_of("alice");
    assert_eq!(made.len(), 1);
    assert_eq!(
        (made[0].kind, made[0].state),
        (RequestKind::Import, RequestState::Pending)
    );
    assert_eq!(made[0].title, "Road trip");
    // Choosing songs of the list asks the same request again.
    rig.service.get_songs("alice", &road, &["any".to_string()]);
    assert_eq!(requests.requests_of("alice").len(), 1);

    // Approved: the list fetches like anyone's.
    let approved = rig.service.approve_family_list("alice", &road);
    assert!(approved.ok, "{}", approved.message);
    assert_eq!(rig.queue.snapshot(Some("alice")).len(), 1);
}

#[tokio::test]
async fn an_account_that_only_saves_cannot_fetch_a_list() {
    use crate::services::family::test_support::{World, member};
    use crate::services::imports::ImportFamily;
    use octo_core::family::abilities::Preset;

    let rig = rig().await;
    let overview = rig.connect_and_read().await;
    let road = Rig::list(&overview, "Road trip").id.clone();
    let world = World::new(None);
    let mut alice = member("alice", Preset::Listener, 6);
    alice
        .edits
        .insert("AddToLibrary".into(), serde_json::json!("SaveOnly"));
    world.family.upsert_member(alice);
    let (requests, _fetcher) = world.list_requests();
    rig.service.use_family(ImportFamily {
        gate: world.gate(),
        requests: Arc::downgrade(&requests),
    });
    let refused = rig.service.set_get_missing("alice", &road, true);
    assert!(!refused.ok);
    assert_eq!(
        refused.message,
        crate::services::family::library_write_gate::SAVE_ONLY_MESSAGE
    );
    assert!(requests.requests_of("alice").is_empty());
    assert!(rig.queue.snapshot(Some("alice")).is_empty());
}

#[tokio::test]
async fn members_who_add_at_once_fetch_lists_as_always() {
    use crate::services::family::test_support::{World, member};
    use crate::services::imports::ImportFamily;
    use octo_core::family::abilities::Preset;

    let rig = rig().await;
    let overview = rig.connect_and_read().await;
    let road = Rig::list(&overview, "Road trip").id.clone();
    let world = World::new(None);
    world.family.upsert_member(member("alice", Preset::Member, 6));
    let (requests, _fetcher) = world.list_requests();
    rig.service.use_family(ImportFamily {
        gate: world.gate(),
        requests: Arc::downgrade(&requests),
    });
    assert_eq!(rig.service.set_get_missing("alice", &road, true).count, 1);
    assert!(requests.requests_of("alice").is_empty());
}

#[tokio::test]
async fn a_plain_list_is_read_whichever_way_round_names_the_library() {
    let rig = rig().await;
    // Written Title - Artist: read that way, both songs are in the library.
    let backward = "Teardrop - Massive Attack\nHalo - Beyonce\nNope - Nobody\n";
    let file = rig
        .service
        .add_file("bob", "Backward.txt", backward.as_bytes())
        .await;
    assert!(file.ok, "{}", file.message);
    let detail = rig
        .service
        .detail("bob", file.list_id.as_deref().unwrap_or_default())
        .expect("the list");
    assert_eq!(detail.list.name, "Backward");
    assert_eq!((detail.list.have, detail.list.missing), (2, 1));
    assert_eq!(detail.tracks[0].artist, "Massive Attack");
    assert_eq!(detail.tracks[0].title, "Teardrop");

    // Written Artist - Title: kept that way round.
    let forward = rig
        .service
        .add_text(
            "bob",
            Some("Forward"),
            "Massive Attack - Teardrop\nBeyonce - Halo\n",
        )
        .await;
    assert!(forward.ok, "{}", forward.message);
    let detail = rig
        .service
        .detail("bob", forward.list_id.as_deref().unwrap_or_default())
        .expect("the list");
    assert_eq!((detail.list.have, detail.list.missing), (2, 0));
    assert_eq!(detail.tracks[1].artist, "Beyonce");
}

#[tokio::test]
async fn pasted_text_is_a_list_of_its_own_name_and_says_what_it_needs() {
    let rig = rig().await;
    let pasted = rig.service.add_text("bob", None, "1. Beyonce - Halo\n").await;
    assert!(pasted.ok, "{}", pasted.message);
    assert_eq!(pasted.message, "Added Pasted list, 1 songs.");
    assert_eq!(Rig::list(&rig.service.overview("bob"), "Pasted list").have, 1);
    // The same name again replaces it.
    let again = rig
        .service
        .add_text("bob", Some("  "), "Massive Attack - Teardrop\nBeyonce - Halo\n")
        .await;
    assert!(again.ok, "{}", again.message);
    assert_eq!(rig.service.overview("bob").lists.len(), 1);

    let empty = rig
        .service
        .add_text("bob", Some("Nothing"), "\n# only a note\n")
        .await;
    assert!(!empty.ok);
    assert_eq!(empty.message, crate::services::imports::text_list::NO_SONGS);
}

#[tokio::test]
async fn the_overview_says_who_approves_only_when_a_member_waits_for_approval() {
    use crate::services::family::test_support::{World, member};
    use crate::services::imports::ImportFamily;
    use octo_core::family::abilities::Preset;

    let rig = rig().await;
    let world = World::new(None);
    world.family.upsert_member(member("alice", Preset::Listener, 6));
    world.family.upsert_member(member("bob", Preset::Member, 7));
    let mut quick = member("cara", Preset::Listener, 8);
    quick.edits.insert("AutoApprove".into(), serde_json::json!(true));
    world.family.upsert_member(quick);
    let (requests, _fetcher) = world.list_requests();
    rig.service.use_family(ImportFamily {
        gate: world.gate(),
        requests: Arc::downgrade(&requests),
    });

    let alice = serde_json::to_value(rig.service.overview("alice")).expect("json");
    assert_eq!(
        alice["approval"],
        serde_json::json!({"needed": true, "by": "winters"})
    );
    for (user, why) in [
        ("bob", "adds songs at once"),
        ("cara", "requests approved at once"),
        ("winters", "the owner"),
        ("stranger", "outside the family"),
    ] {
        let view = serde_json::to_value(rig.service.overview(user)).expect("json");
        assert!(view.get("approval").is_none(), "{user} ({why}): {view}");
    }

    // With Family off, nobody waits.
    let mut off = (*world.settings.current()).clone();
    off.family.enabled = false;
    world.settings.set(off);
    let alice = serde_json::to_value(rig.service.overview("alice")).expect("json");
    assert!(alice.get("approval").is_none(), "{alice}");
}
