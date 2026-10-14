//! Hearts from family members: a listener saves, a member downloads into their own library,
//! the owner downloads as always, and unstar forgets what was saved.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Request, State};
use http_body_util::BodyExt;
use octo_core::family::abilities::{Abilities, Preset};
use octo_core::models::domain::{Album, Song};
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer};

use super::*;
use crate::app::{AppState, TestServices};
use crate::controllers::subsonic::test_app::{NAVIDROME_OK, navidrome_json, settings};
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::common::{AcquisitionRequest, TrackAcquisitionQueue};
use crate::services::family::FamilyCaller;
use crate::services::family::test_support::member;

struct Fixture {
    state: AppState,
    _navidrome: MockServer,
    _members: tempfile::TempDir,
    song: String,
    album: String,
}

impl Fixture {
    async fn new(family_on: bool) -> Fixture {
        let navidrome = MockServer::start().await;
        Mock::given(path("/rest/ping"))
            .respond_with(navidrome_json(NAVIDROME_OK))
            .mount(&navidrome)
            .await;
        let members = tempfile::tempdir().expect("a temp dir");
        let mut app = settings(&navidrome.uri());
        app.family.enabled = family_on;
        app.family.members_path = members.path().to_string_lossy().into_owned();
        // YouTube only, so the chain queues at once.
        app.subsonic.heart_download_sources = vec![octo_core::settings::HeartDownloadStep {
            source: octo_core::settings::HeartDownloadSource::YouTube,
            song_enabled: Some(true),
            album_enabled: Some(true),
            ..Default::default()
        }];
        let metadata = Arc::new(FakeMetadata::default());
        let state = AppState::for_tests_with(
            app,
            TestServices {
                metadata: Some(metadata.clone()),
                ..Default::default()
            },
        );
        let song = state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Song,
            artist: Some("Massive Attack".into()),
            title: Some("Teardrop".into()),
            album: Some("Mezzanine".into()),
            duration: Some(330),
            ..Default::default()
        });
        let album = state.external_id_registry.register(SoulseekRouting {
            kind: RoutingKind::Album,
            artist: Some("Massive Attack".into()),
            album: Some("Mezzanine".into()),
            ..Default::default()
        });
        metadata.songs.lock().insert(
            ("outside".into(), song.clone()),
            Song {
                title: "Teardrop".into(),
                artist: "Massive Attack".into(),
                album: "Mezzanine".into(),
                duration: Some(330),
                ..Default::default()
            },
        );
        metadata.albums.lock().insert(
            ("outside".into(), album.clone()),
            Album {
                title: "Mezzanine".into(),
                artist: "Massive Attack".into(),
                year: Some(1998),
                ..Default::default()
            },
        );
        state.family_store.set_owner("winters");
        state
            .family_store
            .upsert_member(member("alex", Preset::Member, 3));
        state
            .family_store
            .upsert_member(member("sam", Preset::Listener, 4));
        Fixture {
            state,
            _navidrome: navidrome,
            _members: members,
            song,
            album,
        }
    }

    /// Calls `handler` as `caller` would, the family gate having found them.
    async fn call(&self, query: &str, caller: Option<FamilyCaller>, unstarring: bool) -> serde_json::Value {
        let user = caller.as_ref().map_or("winters", |c| c.user.as_str()).to_string();
        let mut request = Request::builder()
            .uri(format!(
                "/rest/{}.view?{query}&u={user}&t=tok&s=salt&v=1.16.1&c=test&f=json",
                if unstarring { "unstar" } else { "star" }
            ))
            .body(Body::empty())
            .expect("a request");
        if let Some(caller) = caller {
            request.extensions_mut().insert(caller);
        }
        let response = if unstarring {
            unstar(State(self.state.clone()), request).await
        } else {
            star(State(self.state.clone()), request).await.expect("answered")
        };
        let body = response.into_body().collect().await.expect("read").to_bytes();
        serde_json::from_slice(&body).expect("JSON")
    }
}

fn caller(user: &str, preset: Preset) -> FamilyCaller {
    FamilyCaller::member(user, preset, Abilities::preset(preset))
}

async fn next_queued(queue: &TrackAcquisitionQueue) -> Option<Arc<AcquisitionRequest>> {
    tokio::time::timeout(
        Duration::from_millis(500),
        queue.dequeue(&CancellationToken::new()),
    )
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn listener_heart_on_outside_song_saves() {
    let fixture = Fixture::new(true).await;
    let answer = fixture
        .call(
            &format!("id={}", fixture.song),
            Some(caller("sam", Preset::Listener)),
            false,
        )
        .await;
    assert_eq!(answer["subsonic-response"]["status"], "ok");
    let saved = fixture.state.family_saved.list("sam");
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].id, fixture.song);
    assert_eq!(
        (saved[0].title.as_str(), saved[0].artist.as_str()),
        ("Teardrop", "Massive Attack")
    );
    assert_eq!(saved[0].duration, 330);
    assert!(
        next_queued(&fixture.state.track_acquisition_queue)
            .await
            .is_none(),
        "no download"
    );
    assert!(fixture.state.acquisition_tracker.all().is_empty());
}

#[tokio::test]
async fn listener_heart_on_outside_album_saves_the_album() {
    let fixture = Fixture::new(true).await;
    let answer = fixture
        .call(
            &format!("albumId={}", fixture.album),
            Some(caller("sam", Preset::Listener)),
            false,
        )
        .await;
    assert_eq!(answer["subsonic-response"]["status"], "ok");
    let albums = fixture.state.family_saved.albums("sam");
    assert_eq!(albums.len(), 1);
    assert_eq!(
        (albums[0].name.as_str(), albums[0].year),
        ("Mezzanine", Some(1998))
    );
    assert!(
        next_queued(&fixture.state.track_acquisition_queue)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn member_heart_downloads_into_their_own_library() {
    let fixture = Fixture::new(true).await;
    fixture
        .call(
            &format!("id={}", fixture.song),
            Some(caller("alex", Preset::Member)),
            false,
        )
        .await;
    let request = next_queued(&fixture.state.track_acquisition_queue)
        .await
        .expect("queued");
    assert_eq!(request.external_id, fixture.song);
    assert_eq!(request.pool_destination(), Some(vec!["alex".to_string()]));
    assert!(fixture.state.family_saved.list("alex").is_empty());
}

#[tokio::test]
async fn owner_heart_unchanged() {
    let fixture = Fixture::new(true).await;
    fixture.call(&format!("id={}", fixture.song), None, false).await;
    let request = next_queued(&fixture.state.track_acquisition_queue)
        .await
        .expect("queued");
    assert_eq!(request.pool_destination(), None, "the shared library, as always");
    assert!(request.wants_shared());

    // With Family off a member's name means nothing either.
    let off = Fixture::new(false).await;
    off.call(&format!("id={}", off.song), None, false).await;
    let request = next_queued(&off.state.track_acquisition_queue)
        .await
        .expect("queued");
    assert_eq!(request.pool_destination(), None);
}

#[tokio::test]
async fn unstar_removes_saved() {
    let fixture = Fixture::new(true).await;
    let sam = || Some(caller("sam", Preset::Listener));
    fixture.call(&format!("id={}", fixture.song), sam(), false).await;
    fixture
        .call(&format!("albumId={}", fixture.album), sam(), false)
        .await;
    assert_eq!(fixture.state.family_saved.list("sam").len(), 1);

    let answer = fixture.call(&format!("id={}", fixture.song), sam(), true).await;
    assert_eq!(answer["subsonic-response"]["status"], "ok");
    assert!(fixture.state.family_saved.list("sam").is_empty());
    fixture
        .call(&format!("albumId={}", fixture.album), sam(), true)
        .await;
    assert!(fixture.state.family_saved.albums("sam").is_empty());
}

#[tokio::test]
async fn a_full_library_saves_and_asks_for_a_copy() {
    let fixture = Fixture::new(true).await;
    let mut full = member("alex", Preset::Member, 3);
    full.edits.insert("StorageLimitGb".into(), serde_json::json!(1));
    fixture.state.family_store.upsert_member(full);
    let members = crate::services::family::members_place::members_path(&fixture.state);
    fixture
        .state
        .library_index
        .note_placed(3, &format!("{members}/alex/A/B/c.flac"), 2_000_000_000);

    fixture
        .call(
            &format!("id={}", fixture.song),
            Some(caller("alex", Preset::Member)),
            false,
        )
        .await;
    assert_eq!(fixture.state.family_saved.list("alex").len(), 1, "saved");
    let requests = fixture.state.family_request_service.requests_of("alex");
    assert_eq!(requests.len(), 1, "and asked for");
    assert_eq!(requests[0].note, "Your library is full (2.0 of 1 GB)");
    assert!(
        next_queued(&fixture.state.track_acquisition_queue)
            .await
            .is_none()
    );
}
