//! Library ownership on the download path (the service through its test backend): a heart, an
//! album walk or a play never adds a second copy of a song already in the library; a lossy copy
//! is upgraded in place instead. And the album walk's notices (its refusal and its summary).

use std::sync::Arc;

use futures::FutureExt;
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{
    AppSettings, DownloadSource, LibraryAction, LibraryActionDefinition, LibraryActionSettings,
    SettingsStore, SubsonicSettings,
};
use tokio_util::sync::CancellationToken;

use super::test_support::{Harness, build};
use super::*;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::library::library_ownership::{Candidate, ResolveFn, search_from};
use crate::services::local::test_support::FakeLocalLibrary;

fn c(id: &str, artist: &str, title: &str, seconds: Option<i32>, suffix: &str, bit_rate: i32) -> Candidate {
    Candidate {
        id: id.into(),
        artist: artist.into(),
        title: title.into(),
        album: None,
        duration: seconds,
        suffix: suffix.into(),
        bit_rate,
        isrcs: Vec::new(),
    }
}

fn flac(id: &str, artist: &str, title: &str, seconds: i32) -> Candidate {
    c(id, artist, title, Some(seconds), "flac", 900)
}

fn mp3(id: &str, artist: &str, title: &str, seconds: i32, bit_rate: i32) -> Candidate {
    c(id, artist, title, Some(seconds), "mp3", bit_rate)
}

struct Owned {
    harness: Harness,
    queue: Arc<UpgradeQueue>,
    root: tempfile::TempDir,
}

impl Owned {
    fn root_file(&self, name: &str) -> String {
        self.root.path().join(name).to_string_lossy().into_owned()
    }

    async fn get(&self, id: &str, upgrade_search: bool) -> String {
        self.harness
            .service
            .execute_acquisition(
                "test",
                id,
                false,
                true,
                None,
                &CancellationToken::new(),
                Some(vec!["alice".into()]),
                upgrade_search,
                None,
            )
            .await
            .expect("acquired")
    }

    async fn album(&self, id: &str) -> bool {
        self.harness
            .service
            .download_album_with_source(
                "test",
                id,
                DownloadSource::Lidarr,
                false,
                &CancellationToken::new(),
                Some(vec!["alice".into()]),
            )
            .await
            .expect("walked")
    }
}

/// Every library id resolves to `<root>/<id>.file`, written on the spot.
fn resolve_to_files(root: &std::path::Path) -> ResolveFn {
    let dir = root.to_path_buf();
    Arc::new(move |id| {
        let path = dir.join(format!("{id}.file"));
        async move {
            std::fs::write(&path, [1])?;
            Ok(Some(path.to_string_lossy().into_owned()))
        }
        .boxed()
    })
}

fn build_owned(library: Vec<Candidate>, source: DownloadSource, better_quality: bool) -> Owned {
    let root = tempfile::tempdir().expect("a temp dir");
    let songs: Vec<Song> = [
        ("t1", "Intro", 90),
        ("t2", "Hold On", 200),
        ("t3", "Started", 180),
        ("t4", "Too Much", 260),
    ]
    .iter()
    .enumerate()
    .map(|(i, (id, title, seconds))| Song {
        external_provider: Some("test".into()),
        external_id: Some(id.to_string()),
        title: title.to_string(),
        artist: "Drake".into(),
        album: "NWTS".into(),
        duration: Some(*seconds),
        track: Some(i as i32 + 1),
        ..Default::default()
    })
    .collect();
    let metadata = FakeMetadata::default();
    for song in &songs {
        metadata.songs.lock().insert(
            ("test".into(), song.external_id.clone().expect("an id")),
            song.clone(),
        );
    }
    metadata.albums.lock().insert(
        ("test".into(), "album-1".into()),
        Album {
            id: "album-1".into(),
            title: "NWTS".into(),
            artist: "Drake".into(),
            songs,
            ..Default::default()
        },
    );

    let settings = AppSettings {
        subsonic: SubsonicSettings {
            download_source: source,
            ..Default::default()
        },
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run: false,
            allowed_users: vec!["alice".into()],
            actions: vec![LibraryActionDefinition {
                action: LibraryAction::BetterQuality,
                enabled: better_quality,
                ..Default::default()
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let ownership = LibraryOwnership::new(
        Arc::new(SettingsStore::for_tests(AppSettings::default())),
        None,
        Arc::new(FakeLocalLibrary::default()),
    );
    ownership.set_search(search_from(Some(library)));
    ownership.set_resolve(resolve_to_files(root.path()));
    let queue = Arc::new(UpgradeQueue::new());
    let services = DownloadServices {
        ownership: Some(Arc::new(ownership)),
        upgrade_queue: Some(Arc::clone(&queue)),
        ..Default::default()
    };
    let harness = build(root.path(), settings, None, Some(Arc::new(metadata)), services);
    // The transfer lands an MP3 in the incoming folder.
    let incoming = root.path().join(".octo-incoming");
    harness.backend.land(move |_, track_id| {
        std::fs::create_dir_all(&incoming).expect("made");
        let landed = incoming.join(format!("{track_id}-{}.mp3", uuid::Uuid::new_v4().simple()));
        std::fs::write(&landed, crate::services::test_support::mp3()).expect("written");
        landed.to_string_lossy().into_owned()
    });
    Owned { harness, queue, root }
}

#[tokio::test]
async fn an_owned_flac_is_never_downloaded_again() {
    let owned = build_owned(
        vec![flac("nd-2", "Drake", "Hold On", 201)],
        DownloadSource::Lidarr,
        true,
    );

    let path = owned.get("t2", false).await;

    assert_eq!(path, owned.root_file("nd-2.file"));
    assert_eq!(owned.harness.backend.transfers(), 0);
    assert!(owned.queue.snapshot().is_empty());
}

#[tokio::test]
async fn an_owned_mp3_is_kept_and_queued_for_a_higher_quality_copy() {
    let owned = build_owned(
        vec![mp3("nd-2", "Drake", "Hold On", 200, 320)],
        DownloadSource::Lidarr,
        true,
    );

    let path = owned.get("t2", false).await;

    assert_eq!(path, owned.root_file("nd-2.file"));
    assert_eq!(owned.harness.backend.transfers(), 0);
    let jobs = owned.queue.snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (
            jobs[0].navidrome_id.as_str(),
            jobs[0].requested_by.as_str(),
            jobs[0].origin.as_str()
        ),
        ("nd-2", "alice", "heart")
    );
}

#[tokio::test]
async fn with_only_you_tube_an_owned_mp3_is_just_kept() {
    let owned = build_owned(
        vec![mp3("nd-2", "Drake", "Hold On", 200, 320)],
        DownloadSource::YouTube,
        true,
    );

    owned.get("t2", false).await;

    assert_eq!(owned.harness.backend.transfers(), 0);
    assert!(owned.queue.snapshot().is_empty());
}

#[tokio::test]
async fn with_better_quality_off_an_owned_mp3_is_just_kept() {
    let owned = build_owned(
        vec![mp3("nd-2", "Drake", "Hold On", 200, 320)],
        DownloadSource::Lidarr,
        false,
    );

    owned.get("t2", false).await;

    assert_eq!(owned.harness.backend.transfers(), 0);
    assert!(owned.queue.snapshot().is_empty());
}

#[tokio::test]
async fn a_better_quality_search_still_downloads() {
    let owned = build_owned(
        vec![mp3("nd-2", "Drake", "Hold On", 200, 320)],
        DownloadSource::Lidarr,
        true,
    );

    owned.get("t2", true).await;

    assert_eq!(owned.harness.backend.transfers(), 1);
}

#[tokio::test]
async fn a_song_you_do_not_have_downloads() {
    let owned = build_owned(
        vec![flac("nd-9", "Drake", "Something Else", 200)],
        DownloadSource::Lidarr,
        true,
    );

    owned.get("t2", false).await;

    assert_eq!(owned.harness.backend.transfers(), 1);
}

#[tokio::test]
async fn an_album_walk_fetches_only_what_is_missing() {
    let owned = build_owned(
        vec![
            flac("nd-1", "Drake", "Intro", 90),
            flac("nd-3", "Drake", "Started", 181),
            mp3("nd-4", "Drake", "Too Much", 260, 192),
        ],
        DownloadSource::Lidarr,
        true,
    );

    assert!(owned.album("album-1").await);
    // Only Hold On.
    assert_eq!(owned.harness.backend.transfers(), 1);
    let jobs = owned.queue.snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].navidrome_id, "nd-4");
}

fn album(artist: &str, title: &str, songs: Vec<Song>) -> Album {
    Album {
        artist: artist.into(),
        title: title.into(),
        songs,
        ..Default::default()
    }
}

#[test]
fn the_album_summary_says_what_was_kept() {
    let nwts = album("Drake", "NWTS", vec![]);
    let summary = BaseDownloadService::build_album_summary(&nwts, 1, 1, 0, 2, 1).expect("a summary");
    assert_eq!(summary.kept_count, Some(2));
    assert_eq!(summary.upgrading_count, Some(1));
    assert!(BaseDownloadService::build_album_summary(&nwts, 0, 0, 0, 4, 0).is_some());
    assert!(BaseDownloadService::build_album_summary(&nwts, 0, 0, 0, 0, 0).is_none());
}

// ---- the album walk's notices -------------------------------------------------------------

/// A heart on an album whose track list never loaded used to do nothing and say nothing: no
/// download, no notification, and the source chain stopped there.
#[test]
fn album_walk_refusal_names_why_nothing_can_be_downloaded() {
    let empty = album("A", "B", vec![]);
    let unkeyed = album(
        "A",
        "B",
        vec![Song {
            title: "t".into(),
            ..Default::default()
        }],
    );
    let walkable = album(
        "A",
        "B",
        vec![Song {
            title: "t".into(),
            external_id: Some("x".into()),
            ..Default::default()
        }],
    );

    assert!(BaseDownloadService::album_walk_refusal(None).is_some());
    assert!(
        BaseDownloadService::album_walk_refusal(Some(&empty))
            .expect("refused")
            .contains("No track list")
    );
    assert!(BaseDownloadService::album_walk_refusal(Some(&unkeyed)).is_some());
    assert!(BaseDownloadService::album_walk_refusal(Some(&walkable)).is_none());
}

#[test]
fn album_summary_is_skipped_when_the_walk_did_nothing() {
    // A re-star whose tracks are all already present must not ping the phone.
    let album = album("A", "B", vec![]);

    assert!(BaseDownloadService::build_album_summary(&album, 0, 0, 0, 0, 0).is_none());
    assert!(BaseDownloadService::build_album_summary(&album, 1, 1, 0, 0, 0).is_some());
}

/// A walk with no track list refuses, and an album heart says so once.
#[tokio::test]
async fn a_walk_with_no_track_list_is_refused_and_downloads_nothing() {
    let root = tempfile::tempdir().expect("a temp dir");
    let harness = build(
        root.path(),
        AppSettings::default(),
        None,
        None,
        DownloadServices::default(),
    );

    let walked = harness
        .service
        .download_album_with_source(
            "test",
            "missing",
            DownloadSource::Lidarr,
            false,
            &CancellationToken::new(),
            None,
        )
        .await
        .expect("answered");

    assert!(!walked);
    assert_eq!(harness.backend.transfers(), 0);
    // Another provider's album is not this service's to walk.
    assert!(
        !harness
            .service
            .download_album_with_source(
                "other",
                "x",
                DownloadSource::Lidarr,
                false,
                &CancellationToken::new(),
                None
            )
            .await
            .expect("answered")
    );
}
