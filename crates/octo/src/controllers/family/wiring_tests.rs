//! The family service over the services that do its work, through the whole pipeline: requests
//! go to the request service and are told once to the owner and the member whatever made them,
//! members' storage and own library are read from their personal library, a removed member's
//! files go through the family pool, the link probe reaches both pages, live streams come from
//! the stream tracker, the unmarked count from the library index, and family playlists follow
//! one set of rules. Navidrome is a fake that walks its libraries' folders on disk; pushes go
//! to a mock ntfy server.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, header};
use axum::response::Response;
use chrono::Utc;
use http_body_util::BodyExt;
use octo_core::family::abilities::Preset;
use octo_core::family::device::{Device, NetworkPlace};
use octo_core::family::request::{RequestKind, RequestState};
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, FamilySettings, NotificationSettings, SubsonicSettings};
use octo_subsonic::subsonic_request_parser::Parameters;
use serde_json::{Map, Value, json};
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::app::{AppState, TestServices};
use crate::controllers::subsonic::route_tests::TestMetadata;
use crate::http::pipeline::{App, app_routes, build_with};
use crate::http::static_files::{StaticAssets, StaticRoots};
use crate::services::admin::SessionKind;
use crate::services::family::audio_gate;
use crate::services::family::fake_navidrome::{FakeNavidrome, FakeNavidromeState};
use crate::services::family::hooks::caller_for;
use crate::services::family::stream_tracker::{StreamPurpose, StreamStart};
use crate::services::family::test_support::{eventually, write_file};
use crate::services::family::{Member, SharedLibraries};

const HOME: &str = "192.168.1.20";
const OUTSIDE: &str = "ext-deezer-song-77";
const OUTSIDE_TOO: &str = "ext-deezer-song-78";
/// The owner's Navidrome password.
const OWNER_PASSWORD: &str = "right-password";

/// Each member's one password, which their apps sign in with.
fn app_password(user: &str) -> String {
    format!("{}-APPP-ASSW-ORDX", user.to_uppercase())
}

struct Wired {
    app: App,
    state: AppState,
    nd: FakeNavidrome,
    ntfy: MockServer,
    dir: tempfile::TempDir,
    /// The metadata service outside songs are looked up in.
    metadata: Arc<TestMetadata>,
}

struct Reply {
    status: StatusCode,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    fn subsonic(&self) -> Value {
        self.json()["subsonic-response"].clone()
    }
}

fn member(state: &AppState, name: &str, preset: Preset, library: i64) -> Member {
    Member {
        username: name.into(),
        display_name: name[..1].to_uppercase() + &name[1..],
        preset,
        edits: Map::new(),
        navidrome_user_id: format!("u-{name}"),
        navidrome_password: state.family_vault.seal("hidden").expect("sealed"),
        personal_library_id: Some(library),
        shared_libraries: SharedLibraries::All,
        account_password: crate::services::family::passwords::seal(&state.family_vault, &app_password(name))
            .expect("sealed"),
        created: Utc::now(),
        suspended: false,
        ntfy_topic: format!("{name}-topic"),
    }
}

fn outside_song(id: &str, title: &str) -> Song {
    Song {
        id: format!("ext-deezer-song-{id}"),
        title: title.into(),
        artist: "Somebody".into(),
        album: "Elsewhere".into(),
        duration: Some(180),
        is_local: false,
        external_provider: Some("deezer".into()),
        external_id: Some(id.into()),
        explicit_content_lyrics: Some(0),
        ..Default::default()
    }
}

/// The family `winters` owns: the listeners alex (library 3) and sam (library 4), and dee, a
/// member whose own library (5) is over her 1 GB limit. A shared library (1) holds one song.
async fn wired(family_on: bool) -> Wired {
    let dir = tempfile::tempdir().expect("a temp dir");
    let music = dir.path().join("music");
    let members = dir.path().join("members");
    for folder in ["alex", "sam", "dee"] {
        std::fs::create_dir_all(members.join(folder)).expect("member folder");
    }
    write_file(
        &music
            .join("Shared Artist/Shared Album/01 Track.flac")
            .to_string_lossy(),
        2000,
    );
    // Dee's library is full: one big song of her own.
    write_file(&members.join("dee/Big/Album/01 Big.flac").to_string_lossy(), 16);

    let nd = FakeNavidrome::start().await;
    for (id, name, admin) in [
        ("u-admin", "admin", true),
        ("u-winters", "winters", true),
        ("u-alex", "alex", false),
        ("u-sam", "sam", false),
        ("u-dee", "dee", false),
    ] {
        nd.add_user(id, name, admin);
    }
    // The owner signs in to Navidrome with their own password; a wrong one is refused there.
    nd.state
        .passwords
        .lock()
        .insert("winters".into(), OWNER_PASSWORD.into());
    nd.add_library(1, "Music", &music.to_string_lossy());
    for (id, name) in [(3, "alex"), (4, "sam"), (5, "dee")] {
        nd.add_library(
            id,
            &format!("{name}'s library"),
            &members.join(name).to_string_lossy(),
        );
    }
    nd.state
        .playlists
        .lock()
        .push(("pl-request".into(), "Request a copy".into(), "sam".into()));
    nd.state.rescan();
    // Dee's song is counted at the size Navidrome reports, past her limit.
    for row in nd.state.songs.lock().iter_mut() {
        if row["libraryId"] == 5 {
            row["size"] = json!(2_000_000_000u64);
        }
    }

    let ntfy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": "x"})))
        .mount(&ntfy)
        .await;

    let closed = format!("http://{}", crate::services::test_support::refused_address());
    let settings = AppSettings {
        radio_sources: octo_core::settings::RadioSourceSettings {
            sonic_url: format!("{closed}/sonic"),
            ..Default::default()
        },
        subsonic: SubsonicSettings {
            url: Some(nd.uri()),
            admin_username: Some("admin".into()),
            admin_password: Some("pw".into()),
            auto_detect_download_path: false,
            enable_search_discovery: false,
            ..Default::default()
        },
        family: FamilySettings {
            enabled: family_on,
            members_path: members.to_string_lossy().into_owned(),
            ..Default::default()
        },
        notifications: NotificationSettings {
            ntfy_url: format!("{}/owner-topic", ntfy.uri()),
            ..Default::default()
        },
        ..Default::default()
    };
    let metadata = Arc::new(TestMetadata::default());
    metadata
        .songs
        .lock()
        .insert("77".into(), outside_song("77", "Outside"));
    metadata
        .songs
        .lock()
        .insert("78".into(), outside_song("78", "Outside too"));
    let state = AppState::for_tests_with(
        settings,
        TestServices {
            config_dir: Some(dir.path().to_path_buf()),
            sign_in: Some("required".into()),
            metadata: Some(metadata.clone()),
            deezer_base_url: Some(closed.clone()),
            raw_settings: vec![
                ("YouTube:ShimUrl".into(), closed.clone()),
                (
                    "Library:DownloadPath".into(),
                    music.to_string_lossy().into_owned(),
                ),
            ],
            ..Default::default()
        },
    );
    assert!(state.family_vault.create_key_if_missing().expect("a key"));
    let store = &state.family_store;
    store.set_owner("winters");
    store.upsert_member(member(&state, "alex", Preset::Listener, 3));
    store.upsert_member(member(&state, "sam", Preset::Listener, 4));
    let mut dee = member(&state, "dee", Preset::Member, 5);
    dee.edits.insert("StorageLimitGb".into(), json!(1));
    store.upsert_member(dee);
    for user in ["alex", "sam", "dee"] {
        store.add_device(Device {
            id: format!("d_{user}"),
            username: user.into(),
            name: format!("{user}'s phone"),
            created: Utc::now(),
            last_seen: None,
            app: String::new(),
            place: NetworkPlace::Home,
            seen_apps: Vec::new(),
            navidrome_jwt: None,
            quality: Default::default(),
        });
    }
    state.library_index.refresh().await.expect("the index is read");
    let app = build_with(
        state.clone(),
        app_routes(&StaticAssets::load(&StaticRoots::from_env())),
    );
    Wired {
        app,
        state,
        nd,
        ntfy,
        dir,
        metadata,
    }
}

impl Wired {
    fn members(&self) -> String {
        self.dir.path().join("members").to_string_lossy().into_owned()
    }

    fn music(&self) -> String {
        self.dir.path().join("music").to_string_lossy().into_owned()
    }

    fn cookie(&self, user: &str) -> String {
        let kind = if user == "winters" {
            SessionKind::FamilyNavidrome
        } else {
            SessionKind::Member
        };
        format!(
            "octo_family={}",
            self.state.browse_sessions.create_family(user, kind)
        )
    }

    async fn send(&self, method: Method, uri: &str, cookie: Option<&str>, body: Option<Value>) -> Reply {
        let mut req = Request::builder().method(method.clone()).uri(uri);
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        if !matches!(method, Method::GET | Method::HEAD) {
            req = req.header("x-octo-family", "1").header("x-octo-admin", "1");
        }
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        if !body.is_empty() {
            req = req.header("content-type", "application/json");
        }
        let mut req = req.body(Body::from(body)).expect("a request");
        let address: SocketAddr = format!("{HOME}:50000").parse().expect("an address");
        req.extensions_mut().insert(ConnectInfo(address));
        let res = self.app.clone().oneshot(req).await.expect("infallible");
        let status = res.status();
        let bytes = res.into_body().collect().await.expect("a body").to_bytes();
        Reply {
            status,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn page(&self, method: Method, path: &str, user: &str, body: Option<Value>) -> Reply {
        let cookie = self.cookie(user);
        self.send(method, path, Some(&cookie), body).await
    }

    /// A Subsonic call from a member's app.
    async fn rest(&self, user: &str, endpoint: &str, query: &str) -> Reply {
        let uri = format!(
            "/rest/{endpoint}?u={user}&p={}&v=1.16.1&c=test&f=json&{query}",
            app_password(user)
        );
        self.send(Method::GET, &uri, None, None).await
    }

    /// The pushes the mock ntfy server took for one topic: `(title, message)`.
    async fn pushes(&self, topic: &str) -> Vec<(String, String)> {
        self.ntfy
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|r| serde_json::from_slice::<Value>(&r.body).ok())
            .filter(|push| push["topic"] == topic)
            .map(|push| {
                (
                    push["title"].as_str().unwrap_or("").to_string(),
                    push["message"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect()
    }

    /// The owner's pushes about new requests.
    async fn asked(&self) -> Vec<String> {
        self.pushes("owner-topic")
            .await
            .into_iter()
            .map(|(title, _)| title)
            .filter(|title| title.contains(" asked for "))
            .collect()
    }

    /// Starts Family's background work, which tells people about requests, and waits until it
    /// listens.
    async fn start_family_work(&self) {
        self.state.start_family();
        self.state.workers.start();
        let events = Arc::clone(&self.state.family_events);
        assert!(
            eventually(|| events.listeners() > 0).await,
            "Family's work never listened"
        );
    }

    /// A file in the family pool at `relative`, linked into each of `users`' libraries, with
    /// the index read again.
    async fn pooled(&self, relative: &str, bytes: usize, users: &[&str]) -> String {
        let pool = &self.state.family_pool_service;
        let path = format!("{}/{relative}", pool.pool_root_path());
        write_file(&path, bytes);
        for user in users {
            pool.link(&path, user).await.expect("linked");
        }
        self.nd.state.rescan();
        self.state.library_index.refresh().await.expect("read");
        path
    }

    async fn until_quiet(&self) {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

// ------------------------------------------------------------------------------------------
// Requests and their notices
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn web_requests_go_through_the_request_service() {
    let w = wired(true).await;
    let made = w
        .page(
            Method::POST,
            "/api/family/me/requests",
            "alex",
            Some(json!({ "id": OUTSIDE, "quality": "Best" })),
        )
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let made = made.json();
    assert_eq!(made["state"], "Pending");
    // The catalog named it: the request service looked the song up.
    assert_eq!(made["title"], "Outside");
    assert_eq!(made["artist"], "Somebody");
    let id = made["id"].as_str().expect("an id").to_string();
    let stored = w.state.family_requests.get(&id).expect("kept");
    assert_eq!(stored.kind, RequestKind::Song);

    let canceled = w
        .page(
            Method::DELETE,
            &format!("/api/family/me/requests/{id}"),
            "alex",
            None,
        )
        .await;
    assert_eq!(canceled.status, StatusCode::OK, "{}", canceled.body);
    assert_eq!(canceled.json()["state"], "Cancelled");

    // A request the service refuses says why: a listener cannot ask for a song by an unknown id.
    let unknown = w
        .page(
            Method::POST,
            "/api/family/me/requests",
            "alex",
            Some(json!({ "id": "nothing-like-this" })),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND, "{}", unknown.body);

    // The owner decides through the same service.
    let again = w
        .page(
            Method::POST,
            "/api/family/me/requests",
            "alex",
            Some(json!({ "id": OUTSIDE_TOO })),
        )
        .await
        .json();
    let declined = w
        .page(
            Method::POST,
            &format!(
                "/api/family/requests/{}/decline",
                again["id"].as_str().expect("id")
            ),
            "winters",
            Some(json!({ "note": "Not now" })),
        )
        .await;
    assert_eq!(declined.status, StatusCode::OK, "{}", declined.body);
    assert_eq!(declined.json()["state"], "Declined");
    assert_eq!(declined.json()["note"], "Not now");
}

#[tokio::test]
async fn the_owner_hears_each_request_once_whatever_made_it() {
    let w = wired(true).await;
    w.start_family_work().await;

    // From the Family page.
    let web = w
        .page(
            Method::POST,
            "/api/family/me/requests",
            "alex",
            Some(json!({ "id": OUTSIDE })),
        )
        .await;
    assert_eq!(web.json()["state"], "Pending", "{}", web.body);

    // From a heart: Dee adds directly, but her library is full, so her heart asks.
    let heart = w.rest("dee", "star", &format!("id={OUTSIDE_TOO}")).await;
    assert_eq!(heart.subsonic()["status"], "ok", "{}", heart.body);
    assert!(w.state.family_saved.is_saved("dee", OUTSIDE_TOO));
    let dees = w.state.family_requests.for_user("dee");
    assert_eq!(dees.len(), 1, "{dees:?}");
    assert_eq!(dees[0].state, RequestState::Pending);

    // From an action playlist.
    let playlist = w
        .rest(
            "sam",
            "updatePlaylist",
            &format!("playlistId=pl-request&songIdToAdd={OUTSIDE}"),
        )
        .await;
    assert_eq!(playlist.subsonic()["status"], "ok", "{}", playlist.body);
    assert_eq!(w.state.family_requests.for_user("sam").len(), 1);

    let mut heard = Vec::new();
    for _ in 0..100 {
        heard = w.asked().await;
        if heard.len() >= 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.until_quiet().await;
    let mut heard_now = w.asked().await;
    heard_now.sort();
    assert_eq!(heard_now.len(), 3, "{heard_now:?} (first {heard:?})");
    assert_eq!(
        heard_now,
        [
            "Alex asked for Outside by Somebody (best quality)",
            "Dee asked for Outside too by Somebody (best quality)",
            "Sam asked for Outside by Somebody (best quality)",
        ]
    );

    // The member hears a decision once, on their own topic.
    let alex = w.state.family_requests.for_user("alex").pop().expect("alex's");
    let declined = w
        .page(
            Method::POST,
            &format!("/api/family/requests/{}/decline", alex.id),
            "winters",
            None,
        )
        .await;
    assert_eq!(declined.status, StatusCode::OK, "{}", declined.body);
    for _ in 0..100 {
        if !w.pushes("alex-topic").await.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.until_quiet().await;
    let told = w.pushes("alex-topic").await;
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].0.starts_with("Declined: Outside"), "{told:?}");
    // Nothing new reached the owner for the decision.
    assert_eq!(w.asked().await.len(), 3);
}

// ------------------------------------------------------------------------------------------
// Members' own libraries, folders and files
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn storage_and_my_library_come_from_the_personal_library() {
    let w = wired(true).await;
    let pooled = w.pooled("Artist/Album/01 Song.flac", 3000, &["alex"]).await;
    let me = w.page(Method::GET, "/api/family/me", "alex", None).await.json();
    assert_eq!(me["storageUsedBytes"], 3000, "{me}");

    let library = w
        .page(Method::GET, "/api/family/me/library", "alex", None)
        .await
        .json();
    assert_eq!(library["total"], 1, "{library}");
    let song = &library["songs"][0];
    assert_eq!(song["title"], "Song");
    assert_eq!(song["artist"], "Artist");
    assert_eq!(song["album"], "Album");
    assert_eq!(song["size"], 3000);
    assert!(song["added"].is_string(), "{song}");
    assert_eq!(library["usedBytes"], 3000);

    // A shared song is not theirs to remove.
    let shared = FakeNavidromeState::song_id(1, "Shared Artist/Shared Album/01 Track.flac");
    let refused = w
        .rest("alex", "removeFromMyLibrary", &format!("id={shared}"))
        .await;
    assert_eq!(refused.subsonic()["error"]["code"], 50, "{}", refused.body);

    let id = song["id"].as_str().expect("an id").to_string();
    let removed = w
        .page(
            Method::DELETE,
            &format!("/api/family/me/library/{id}"),
            "alex",
            None,
        )
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.body);
    let link = format!("{}/alex/Artist/Album/01 Song.flac", w.members());
    assert!(std::fs::symlink_metadata(&link).is_err(), "the link went");
    assert!(
        !Path::new(&pooled).exists(),
        "nobody has it, so it went to the trash"
    );
}

#[tokio::test]
async fn adding_a_member_makes_their_folder_and_library() {
    let w = wired(true).await;
    let added = w
        .page(
            Method::POST,
            "/api/family/members",
            "winters",
            Some(json!({ "username": "rae", "displayName": "Rae", "preset": "Listener" })),
        )
        .await;
    assert_eq!(added.status, StatusCode::OK, "{}", added.body);
    let folder = format!("{}/rae", w.members());
    assert!(Path::new(&folder).is_dir());
    assert!(
        w.nd.state
            .libraries
            .lock()
            .iter()
            .any(|library| library.path == folder),
        "Navidrome got a library over the folder"
    );
}

async fn remove_alex(choice: &str) -> (Wired, String, String) {
    let w = wired(true).await;
    let only = w.pooled("Artist/Album/01 Only.flac", 100, &["alex"]).await;
    let both = w.pooled("Artist/Album/02 Both.flac", 200, &["alex", "sam"]).await;
    let removed = w
        .page(
            Method::DELETE,
            "/api/family/members/alex",
            "winters",
            Some(json!({ "confirm": "alex", "files": choice })),
        )
        .await;
    assert_eq!(
        removed.status,
        StatusCode::NO_CONTENT,
        "{choice}: {}",
        removed.body
    );
    assert!(w.state.family_store.member("alex").is_none());
    // Sam still has the song they shared, as it was.
    assert!(Path::new(&both).is_file(), "{choice}");
    let sams = format!("{}/sam/Artist/Album/02 Both.flac", w.members());
    assert!(Path::new(&sams).is_file(), "{choice}");
    let refs = w.state.family_pool.get(&both).expect("still pooled").refs;
    assert_eq!(refs, ["sam"], "{choice}");
    assert!(!Path::new(&format!("{}/alex", w.members())).exists(), "{choice}");
    (w, only, both)
}

#[tokio::test]
async fn removing_a_member_can_keep_their_files_aside() {
    let (w, only, _) = remove_alex("keep").await;
    assert!(!Path::new(&only).exists());
    assert!(w.state.family_pool.get(&only).is_none());
    let kept = format!("{}/.kept/alex/Artist/Album/01 Only.flac", w.members());
    assert!(Path::new(&kept).is_file(), "kept aside at {kept}");
}

#[tokio::test]
async fn removing_a_member_can_move_their_files_into_the_shared_library() {
    let (w, only, _) = remove_alex("promote").await;
    assert!(!Path::new(&only).exists());
    let shared = format!("{}/Artist/Album/01 Only.flac", w.music());
    assert!(Path::new(&shared).is_file(), "moved to {shared}");
}

#[tokio::test]
async fn removing_a_member_can_send_their_files_to_the_trash() {
    let (w, only, _) = remove_alex("delete").await;
    assert!(!Path::new(&only).exists());
    assert!(w.state.family_pool.get(&only).is_none());
    assert!(!Path::new(&format!("{}/.kept", w.members())).exists());
    assert!(!Path::new(&format!("{}/Artist/Album/01 Only.flac", w.music())).exists());
}

#[tokio::test]
async fn a_removed_members_waiting_requests_are_canceled_and_cannot_be_approved() {
    let w = wired(true).await;
    let waiting = w.state.family_requests.create(
        octo_core::family::request::FamilyRequest {
            id: String::new(),
            username: "alex".into(),
            kind: RequestKind::Song,
            target: OUTSIDE.into(),
            title: "Outside".into(),
            artist: "Somebody".into(),
            album: "Elsewhere".into(),
            cover_art: OUTSIDE.into(),
            quality: octo_core::family::abilities::RequestQuality::Best,
            state: RequestState::Pending,
            created: Utc::now(),
            decided_by: None,
            decided: None,
            note: String::new(),
            library_song_id: None,
            failure: None,
            outcome: None,
        },
        Utc::now(),
    );
    let removed = w
        .page(
            Method::DELETE,
            "/api/family/members/alex",
            "winters",
            Some(json!({ "confirm": "alex", "files": "delete" })),
        )
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT, "{}", removed.body);
    assert_eq!(
        w.state.family_requests.get(&waiting.id).expect("kept").state,
        RequestState::Cancelled
    );
    assert!(
        w.state
            .family_request_service
            .approve(&waiting.id, "winters", None)
            .is_err()
    );
}

#[tokio::test]
async fn turning_family_on_over_pool_records_whose_disk_is_missing_marks_nothing() {
    let w = wired(false).await;
    let admin = format!("octo_browse={}", w.state.browse_sessions.create("winters"));
    let pool = &w.state.family_pool_service;
    // Family was on before and recorded a member's download; its disk is not mounted now.
    pool.record(crate::services::family::PoolFile {
        path: format!("{}/Artist/Album/01 Song.flac", pool.pool_root_path()),
        size: 10,
        format: "flac".into(),
        outside_ids: Vec::new(),
        isrc: None,
        title: "Song".into(),
        artist: "Artist".into(),
        album: "Album".into(),
        refs: vec!["alex".into()],
        transcoded_from: None,
    });
    let on = w
        .send(
            Method::POST,
            "/api/admin/family/enable",
            Some(&admin),
            Some(json!({ "enabled": true })),
        )
        .await;
    assert_eq!(on.status, StatusCode::CONFLICT, "{}", on.body);
    assert!(on.body.contains("mount"), "{}", on.body);
    let marker = format!(
        "{}/{}",
        pool.pool_root_path(),
        crate::services::family::pool::POOL_MARKER
    );
    assert!(!Path::new(&marker).exists(), "no marker on an empty mount point");
    assert!(!w.state.settings.current().family.enabled, "Family stays off");
    assert_eq!(pool.store().list().len(), 1, "the record is kept");
}

#[tokio::test]
async fn turning_family_on_probes_links_and_both_pages_show_them() {
    let w = wired(false).await;
    let admin = format!("octo_browse={}", w.state.browse_sessions.create("winters"));
    let before = w.send(Method::GET, "/api/admin/family", Some(&admin), None).await;
    assert_eq!(before.json()["links"], Value::Null, "{}", before.body);
    let on = w
        .send(
            Method::POST,
            "/api/admin/family/enable",
            Some(&admin),
            Some(json!({ "enabled": true })),
        )
        .await;
    assert_eq!(on.status, StatusCode::OK, "{}", on.body);
    assert_eq!(on.json()["links"], "Symlink");
    let marker = format!(
        "{}/{}",
        w.state.family_pool_service.pool_root_path(),
        crate::services::family::pool::POOL_MARKER
    );
    assert!(
        Path::new(&marker).is_file(),
        "turning Family on marks the pool folder"
    );
    assert!(w.state.family_pool_service.pool_ready());
    let session = w
        .page(Method::GET, "/api/family/session", "winters", None)
        .await
        .json();
    assert_eq!(session["links"], "Symlink", "{session}");
    assert_eq!(session["linksMessage"], Value::Null);
    let summary = w
        .send(Method::GET, "/api/admin/family", Some(&admin), None)
        .await
        .json();
    assert_eq!(summary["links"], "Symlink", "{summary}");

    // Navidrome never showing a linked song switches the pool to copies; both pages say so.
    w.state.family_pool_service.fall_back_to_copies().await;
    let session = w
        .page(Method::GET, "/api/family/session", "winters", None)
        .await
        .json();
    assert_eq!(session["links"], "Copy", "{session}");
    let message = session["linksMessage"].as_str().expect("a message");
    // The words name the storage the members folder is on (here a local disk).
    let members = w.state.family_pool_service.members_root();
    assert_eq!(
        message,
        crate::services::family::members_place::link_hint(&members).message
    );
    assert!(message.ends_with("Family works either way."), "{message}");
    assert_eq!(
        session["linksFix"],
        crate::services::family::members_place::link_hint(&members).fix
    );
    let summary = w
        .send(Method::GET, "/api/admin/family", Some(&admin), None)
        .await
        .json();
    assert_eq!(summary["links"], "Copy");
    assert_eq!(summary["linksMessage"], session["linksMessage"]);
}

// ------------------------------------------------------------------------------------------
// Live streams, the unmarked count and family playlists
// ------------------------------------------------------------------------------------------

#[tokio::test]
async fn live_streams_come_from_the_stream_tracker() {
    let w = wired(true).await;
    let song = FakeNavidromeState::song_id(1, "Shared Artist/Shared Album/01 Track.flac");
    let body = w.state.family_audio.tracker().track(
        StreamStart {
            user: "alex".into(),
            device_id: Some("d_alex".into()),
            device_name: "alex's phone".into(),
            song_id: song.clone(),
            place: NetworkPlace::Away,
            purpose: StreamPurpose::Play,
            devices_at_once: 0,
            pace_bytes_per_second: None,
        },
        Body::from(vec![1u8; 2048]),
    );
    let mut body = body;
    // The stream counts from its first read.
    let first = body.frame().await.expect("a frame").expect("bytes");
    assert!(first.is_data());

    let load = w
        .page(Method::GET, "/api/family/load", "winters", None)
        .await
        .json();
    assert_eq!(load["streams"][0]["username"], "alex", "{load}");
    assert_eq!(load["streams"][0]["device"], "alex's phone");
    assert_eq!(load["streams"][0]["songTitle"], "Track");
    assert_eq!(load["streams"][0]["place"], "Away");
    let members = w
        .page(Method::GET, "/api/family/members", "winters", None)
        .await
        .json();
    let alex = members
        .as_array()
        .and_then(|list| list.iter().find(|m| m["username"] == "alex"))
        .expect("alex")
        .clone();
    assert_eq!(alex["playingNow"], true, "{alex}");
    let devices = w.rest("alex", "getFamilyDevices", "").await.subsonic();
    let phone = devices["familyDevices"]["device"]
        .as_array()
        .and_then(|list| list.iter().find(|d| d["id"] == "d_alex"))
        .expect("the phone")
        .clone();
    assert_eq!(phone["playing"]["songId"], json!(song), "{phone}");
    assert_eq!(phone["playing"]["title"], "Track");
    assert_eq!(phone["playing"]["artist"], "Shared Artist");

    drop(body);
    let load = w
        .page(Method::GET, "/api/family/load", "winters", None)
        .await
        .json();
    assert_eq!(load["streams"], json!([]), "{load}");
}

#[tokio::test]
async fn an_outside_stream_is_named_from_the_metadata_service_and_never_waits_for_it() {
    let w = wired(true).await;
    // The metadata service holds every lookup until the test lets it answer.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *w.metadata.song_gate.lock() = Some(gate.clone());
    let mut caller = caller_for(&w.state.family_store, "alex");
    caller.device_id = Some("d_alex".into());
    let mut parameters = Parameters::new();
    parameters.insert("id".into(), OUTSIDE.into());
    let admitted = audio_gate::admit(
        &w.state,
        &caller,
        &Method::GET,
        "stream",
        &parameters,
        &HeaderMap::new(),
        &audio_gate::Sent {
            query: Some(&format!("id={OUTSIDE}")),
            ..Default::default()
        },
    )
    .await
    .expect("alex may play it");
    let mut audio = Response::new(Body::from(vec![1u8; 2048]));
    audio
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("audio/mpeg"));
    let mut body = admitted.track(&w.state, audio).into_body();

    // The audio flows while the lookup is still held.
    let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("the stream does not wait for the lookup")
        .expect("a frame")
        .expect("bytes");
    assert!(first.is_data());
    let tracker = w.state.family_audio.tracker().clone();
    assert!(!tracker.start_naming(OUTSIDE), "a lookup is under way");
    let load = w
        .page(Method::GET, "/api/family/load", "winters", None)
        .await
        .json();
    assert_eq!(load["streams"][0]["songTitle"], OUTSIDE, "{load}");

    // Once the metadata service answers, the stream carries the title and artist.
    gate.add_permits(1);
    assert!(eventually(|| tracker.live().first().is_some_and(|stream| stream.name.is_some())).await);
    let load = w
        .page(Method::GET, "/api/family/load", "winters", None)
        .await
        .json();
    assert_eq!(load["streams"][0]["songTitle"], "Outside", "{load}");
    let devices = w.rest("alex", "getFamilyDevices", "").await.subsonic();
    let phone = devices["familyDevices"]["device"]
        .as_array()
        .and_then(|list| list.iter().find(|d| d["id"] == "d_alex"))
        .expect("the phone")
        .clone();
    assert_eq!(phone["playing"]["songId"], OUTSIDE, "{phone}");
    assert_eq!(phone["playing"]["title"], "Outside");
    assert_eq!(phone["playing"]["artist"], "Somebody");
    drop(body);
}

#[tokio::test]
async fn the_unmarked_count_reads_the_library_index() {
    let w = wired(true).await;
    w.pooled("Artist/Album/01 Own.flac", 10, &["alex"]).await;
    let unrated = w
        .page(Method::GET, "/api/family/unrated", "winters", None)
        .await
        .json();
    // The shared song has no mark; members' own songs are not counted.
    assert_eq!(unrated["count"], 1, "{unrated}");
}

#[tokio::test]
async fn family_playlists_follow_one_set_of_rules_everywhere() {
    let w = wired(true).await;
    let refused = w
        .page(
            Method::POST,
            "/api/family/playlists",
            "dee",
            Some(json!({ "name": "Dee's mix", "approveAdditions": true })),
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    let refused = w
        .rest("dee", "createFamilyPlaylist", "name=Mix&approveAdditions=true")
        .await;
    assert_eq!(refused.subsonic()["error"]["code"], 50, "{}", refused.body);

    let made = w
        .page(
            Method::POST,
            "/api/family/playlists",
            "dee",
            Some(json!({ "name": "Dee's mix" })),
        )
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let id = made.json()["id"].as_str().expect("an id").to_string();
    let path = format!("/api/family/playlists/{id}");
    let not_theirs = w
        .page(Method::PUT, &path, "sam", Some(json!({ "name": "Mine" })))
        .await;
    assert_eq!(not_theirs.status, StatusCode::FORBIDDEN, "{}", not_theirs.body);
    let no_approval = w
        .page(
            Method::PUT,
            &path,
            "dee",
            Some(json!({ "approveAdditions": true })),
        )
        .await;
    assert_eq!(no_approval.status, StatusCode::FORBIDDEN, "{}", no_approval.body);
    let renamed = w
        .page(Method::PUT, &path, "dee", Some(json!({ "name": "Dee's songs" })))
        .await;
    assert_eq!(renamed.json()["name"], "Dee's songs", "{}", renamed.body);
    let approval = w
        .page(
            Method::PUT,
            &path,
            "winters",
            Some(json!({ "approveAdditions": true })),
        )
        .await;
    assert_eq!(approval.json()["approveAdditions"], true, "{}", approval.body);
    let listed = w
        .page(Method::GET, "/api/family/playlists", "sam", None)
        .await
        .json();
    assert_eq!(listed[0]["name"], "Dee's songs", "{listed}");
    assert_eq!(listed[0]["songCount"], 0);
    let deleted = w.page(Method::DELETE, &path, "winters", None).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT, "{}", deleted.body);
}

// ------------------------------------------------------------------------------------------
// Sign-ins that are not what they claim
// ------------------------------------------------------------------------------------------

/// A token shaped like Navidrome's for `sub`, with a signature nobody made.
fn forged_navidrome_token(sub: &str) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(json!({ "sub": sub, "exp": 9999999999i64 }).to_string()),
        URL_SAFE_NO_PAD.encode("not-a-signature")
    )
}

/// A token shaped like Octo's own for `sub` and one of their devices, signed with a key that is
/// not this server's.
fn forged_octo_token(sub: &str, device: &str) -> String {
    use crate::services::family::web_session::{WebClaims, mint};
    let now = Utc::now().timestamp();
    mint(
        &[7u8; 32],
        &WebClaims {
            iss: "ND".into(),
            sub: sub.into(),
            uid: format!("u-{sub}"),
            adm: false,
            iat: now,
            exp: now + 3600,
            did: device.into(),
        },
    )
}

/// A request with extra headers and no cookie, from home.
async fn send_with_headers(w: &Wired, method: Method, uri: &str, headers: &[(&str, String)]) -> Reply {
    let mut req = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        req = req.header(*name, value.as_str());
    }
    let mut req = req.body(Body::empty()).expect("a request");
    let address: SocketAddr = format!("{HOME}:50000").parse().expect("an address");
    req.extensions_mut().insert(ConnectInfo(address));
    let res = w.app.clone().oneshot(req).await.expect("infallible");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("a body").to_bytes();
    Reply {
        status,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

#[tokio::test]
async fn a_forged_token_cannot_add_a_device_for_a_member() {
    let w = wired(true).await;
    let devices_before = w.state.family_store.devices_of("alex").len();
    let uri = format!(
        "/rest/addFamilyDevice?jwt={}&v=1.16.1&c=evil&f=json&name=evil&kind=SubsonicApp",
        forged_navidrome_token("alex")
    );
    let r = w.send(Method::GET, &uri, None, None).await;
    assert_eq!(r.subsonic()["error"]["code"], 40, "{}", r.body);
    assert_eq!(
        w.state.family_store.devices_of("alex").len(),
        devices_before,
        "a forged token added a device"
    );
}

#[tokio::test]
async fn a_forged_token_cannot_read_the_family() {
    let w = wired(true).await;
    let uri = format!(
        "/rest/getFamily?jwt={}&v=1.16.1&c=evil&f=json",
        forged_navidrome_token("sam")
    );
    let r = w.send(Method::GET, &uri, None, None).await;
    assert_ne!(r.subsonic()["status"], "ok", "a forged token read getFamily");
    assert_eq!(r.subsonic()["error"]["code"], 40, "{}", r.body);
}

#[tokio::test]
async fn no_unchecked_sign_in_names_a_member_or_a_manager_on_rest() {
    let w = wired(true).await;
    let devices = w.state.family_store.devices().len();
    let tail = "v=1.16.1&c=evil&f=json";
    let mut tries: Vec<(String, Vec<(&str, String)>)> = Vec::new();
    for endpoint in ["getFamily", "forgetFamilyDevice?id=d_alex"] {
        let join = if endpoint.contains('?') { '&' } else { '?' };
        let base = format!("/rest/{endpoint}{join}{tail}");
        for token in [
            forged_navidrome_token("alex"),
            forged_navidrome_token("winters"),
            forged_octo_token("alex", "d_alex"),
            forged_octo_token("winters", "x_owner"),
        ] {
            // A token alone, and a token next to the name it claims.
            tries.push((format!("{base}&jwt={token}"), Vec::new()));
            tries.push((format!("{base}&u=alex&jwt={token}"), Vec::new()));
            tries.push((format!("{base}&u=winters&jwt={token}"), Vec::new()));
            tries.push((base.clone(), vec![("authorization", format!("Bearer {token}"))]));
            tries.push((
                base.clone(),
                vec![("x-nd-authorization", format!("Bearer {token}"))],
            ));
        }
        tries.push((format!("{base}&apiKey=not-a-key"), Vec::new()));
        tries.push((format!("{base}&u=alex&p=wrong-password"), Vec::new()));
    }
    for (uri, headers) in tries {
        let r = send_with_headers(&w, Method::GET, &uri, &headers).await;
        let answer = r.subsonic();
        assert_ne!(answer["status"], "ok", "{uri} {headers:?}: {}", r.body);
        assert!(answer.get("family").is_none(), "{uri}: {}", r.body);
        assert!(answer.get("familyLogin").is_none(), "{uri}: {}", r.body);
    }
    assert_eq!(
        w.state.family_store.devices().len(),
        devices,
        "no device was added or removed"
    );
    // The real sign-ins still work.
    let alex = w.rest("alex", "getFamily", "").await;
    assert_eq!(alex.subsonic()["status"], "ok", "{}", alex.body);
    assert!(alex.subsonic()["family"].get("manager").is_none());
}

#[tokio::test]
async fn no_unchecked_sign_in_reaches_the_family_api() {
    let w = wired(true).await;
    let tail = "v=1.16.1&c=evil&f=json";
    let mut tries: Vec<(String, Vec<(&str, String)>)> = Vec::new();
    for path in ["/api/family/me", "/api/family/members", "/api/family/devices"] {
        for token in [
            forged_navidrome_token("alex"),
            forged_navidrome_token("winters"),
            forged_octo_token("alex", "d_alex"),
            forged_octo_token("winters", "x_owner"),
        ] {
            tries.push((format!("{path}?jwt={token}&{tail}"), Vec::new()));
            tries.push((format!("{path}?u=alex&jwt={token}&{tail}"), Vec::new()));
            tries.push((format!("{path}?u=winters&jwt={token}&{tail}"), Vec::new()));
            tries.push((
                path.to_string(),
                vec![("authorization", format!("Bearer {token}"))],
            ));
            tries.push((
                path.to_string(),
                vec![("x-nd-authorization", format!("Bearer {token}"))],
            ));
        }
        tries.push((format!("{path}?apiKey=not-a-key&{tail}"), Vec::new()));
        tries.push((format!("{path}?u=alex&p=wrong-password&{tail}"), Vec::new()));
    }
    for (uri, headers) in tries {
        let r = send_with_headers(&w, Method::GET, &uri, &headers).await;
        assert_eq!(
            r.status,
            StatusCode::UNAUTHORIZED,
            "{uri} {headers:?}: {}",
            r.body
        );
    }
    // Resetting a password through the API needs a real sign-in too.
    let before = w
        .state
        .family_store
        .member("alex")
        .expect("member")
        .account_password;
    let forged = forged_navidrome_token("winters");
    let r = send_with_headers(
        &w,
        Method::POST,
        &format!("/api/family/members/alex/reset?u=winters&jwt={forged}&{tail}"),
        &[("x-octo-family", "1".to_string())],
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{}", r.body);
    assert_eq!(
        w.state
            .family_store
            .member("alex")
            .expect("member")
            .account_password,
        before
    );
    // A signed call with the member's own app password works.
    let signed = send_with_headers(
        &w,
        Method::GET,
        &format!("/api/family/me?u=alex&p={}&{tail}", app_password("alex")),
        &[],
    )
    .await;
    assert_eq!(signed.status, StatusCode::OK, "{}", signed.body);
}

#[tokio::test]
async fn signing_in_to_one_account_never_clears_wrong_tries_at_another() {
    let w = wired(true).await;
    for (name, pw) in [("alex", "alex-own-password"), ("sam", "sams-secret-password")] {
        let mut m = w.state.family_store.member(name).expect("member");
        m.account_password =
            crate::services::family::passwords::seal(&w.state.family_vault, pw).expect("sealed");
        w.state.family_store.upsert_member(m);
    }
    let mut throttled = 0;
    let mut wrong = 0;
    for round in 0..3 {
        for guess in 0..9 {
            let r = w
                .send(
                    Method::POST,
                    "/api/family/auth",
                    None,
                    Some(json!({ "username": "sam", "password": format!("guess-{round}-{guess}") })),
                )
                .await;
            if r.status == StatusCode::TOO_MANY_REQUESTS {
                throttled += 1;
            } else if r.status == StatusCode::UNAUTHORIZED {
                wrong += 1;
            }
        }
        let own = w
            .send(
                Method::POST,
                "/api/family/auth",
                None,
                Some(json!({ "username": "alex", "password": "alex-own-password" })),
            )
            .await;
        assert_eq!(own.status, StatusCode::OK, "{}", own.body);
    }
    assert_eq!(wrong, 10, "ten wrong guesses for sam are checked");
    assert_eq!(throttled, 17, "the rest wait, whatever alex does meanwhile");
    // Sam's own right password waits too; another account's does not.
    let sam = w
        .send(
            Method::POST,
            "/api/family/auth",
            None,
            Some(json!({ "username": "sam", "password": "sams-secret-password" })),
        )
        .await;
    assert_eq!(sam.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn wrong_sign_ins_make_no_devices_or_notices() {
    let w = wired(true).await;
    w.start_family_work().await;
    let before = w.state.family_store.detected().len();
    let ping = |password: &str, device: String| {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(format!("/rest/ping?u=winters&p={password}&v=1.16.1&c=x&f=json"))
            .header("x-octo-device-id", device)
            .body(Body::empty())
            .expect("a request");
        let address: SocketAddr = "203.0.113.50:50000".parse().expect("an address");
        req.extensions_mut().insert(ConnectInfo(address));
        w.app.clone().oneshot(req)
    };
    for n in 0..5 {
        let res = ping("wrong-password", format!("forged-device-{n}"))
            .await
            .expect("infallible");
        let body = res.into_body().collect().await.expect("a body").to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("\"code\":40"));
    }
    w.until_quiet().await;
    assert_eq!(
        w.state.family_store.detected().len(),
        before,
        "wrong sign-ins recorded devices"
    );
    let notices = w.pushes("owner-topic").await;
    assert!(
        !notices
            .iter()
            .any(|(title, message)| title.contains("device") || message.contains("device")),
        "{notices:?}"
    );
    // The owner's right sign-in from a new app is noted, once Navidrome took it.
    let res = ping(OWNER_PASSWORD, "real-device".into())
        .await
        .expect("infallible");
    let _ = res.into_body().collect().await;
    assert_eq!(w.state.family_store.detected().len(), before + 1);
}

#[tokio::test]
async fn sign_ins_sent_all_at_once_count_before_they_are_checked() {
    let w = wired(true).await;
    let mut sam = w.state.family_store.member("sam").expect("member");
    sam.account_password =
        crate::services::family::passwords::seal(&w.state.family_vault, "sams-secret-password")
            .expect("sealed");
    w.state.family_store.upsert_member(sam);
    let tries = (0..15).map(|n| {
        w.send(
            Method::POST,
            "/api/family/auth",
            None,
            Some(json!({ "username": "sam", "password": format!("guess-{n}") })),
        )
    });
    let replies = futures::future::join_all(tries).await;
    let checked = replies
        .iter()
        .filter(|r| r.status == StatusCode::UNAUTHORIZED)
        .count();
    let waited = replies
        .iter()
        .filter(|r| r.status == StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert_eq!((checked, waited), (10, 5));
}

#[tokio::test]
async fn a_wrong_owner_password_cannot_change_a_family_playlist() {
    let w = wired(true).await;
    let made = w
        .page(
            Method::POST,
            "/api/family/playlists",
            "dee",
            Some(json!({ "name": "Dee's mix" })),
        )
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.body);
    let id = made.json()["id"].as_str().expect("an id").to_string();
    let as_owner =
        |password: &str, call: String| format!("/rest/{call}&u=winters&p={password}&v=1.16.1&c=x&f=json");
    for call in [
        format!("deletePlaylist?id={id}"),
        format!("updatePlaylist?playlistId={id}&name=Taken"),
    ] {
        let r = w
            .send(Method::GET, &as_owner("wrong-password", call.clone()), None, None)
            .await;
        assert_eq!(r.subsonic()["error"]["code"], 40, "{call}: {}", r.body);
    }
    let listed = w
        .page(Method::GET, "/api/family/playlists", "winters", None)
        .await;
    assert!(listed.body.contains("Dee's mix"), "{}", listed.body);
    let r = w
        .send(
            Method::GET,
            &as_owner(OWNER_PASSWORD, format!("deletePlaylist?id={id}")),
            None,
            None,
        )
        .await;
    assert_eq!(r.subsonic()["status"], "ok", "{}", r.body);
    let listed = w
        .page(Method::GET, "/api/family/playlists", "winters", None)
        .await;
    assert!(!listed.body.contains("Dee's mix"), "{}", listed.body);
}
