//! Hearts and plays for family members: whose library a song counts as already in, where a
//! member's chain sends the file, and which plays keep a copy.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use octo_core::family::abilities::Preset;
use octo_core::models::domain::Song;
use octo_core::settings::{HeartDownloadSource, HeartDownloadStep};
use serde_json::json;

use super::*;
use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr, FakeMetadata};
use crate::services::common::{AcquisitionPlace, AcquisitionRequest};
use crate::services::family::library_index::ScopeResolver;
use crate::services::family::test_support::{World, member, write_file};
use crate::services::family::{LibraryWriteGate, PersonalLibrary};
use crate::services::library::LibraryOwnership;
use crate::services::library::library_ownership::{Candidate, search_from};

fn step(source: HeartDownloadSource) -> HeartDownloadStep {
    HeartDownloadStep {
        source,
        song_enabled: Some(true),
        album_enabled: Some(true),
        ..Default::default()
    }
}

/// The world's settings with these heart sources, download on play, and the owned check on.
fn configure(world: &World, sources: &[HeartDownloadSource], download_on_play: bool) {
    let mut settings = world.settings.current().as_ref().clone();
    settings.subsonic.heart_download_sources = sources.iter().map(|s| step(*s)).collect();
    settings.subsonic.download_on_play = download_on_play;
    settings.subsonic.skip_owned_songs = true;
    world.settings.set(settings);
}

struct Chain {
    world: World,
    queue: Arc<TrackAcquisitionQueue>,
    lidarr: Arc<FakeLidarr>,
    coordinator: Arc<HeartAcquisitionCoordinator>,
}

/// A coordinator over the world. The library holds "Song 1" by Artist only in Alex's own
/// library (the file `<members>/alex/Artist/Album/1.flac`).
fn chain(world: World) -> Chain {
    let metadata = Arc::new(FakeMetadata::default());
    metadata.songs.lock().insert(
        ("test".into(), "1".into()),
        Song {
            title: "Song 1".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            duration: Some(200),
            ..Default::default()
        },
    );
    let alex_copy = format!("{}/alex/Artist/Album/1.flac", world.members());
    write_file(&alex_copy, 10);
    let ownership = LibraryOwnership::new(world.settings.clone(), None, world.library.clone());
    ownership.set_search(search_from(Some(vec![Candidate {
        id: "alex-1".into(),
        artist: "Artist".into(),
        title: "Song 1".into(),
        album: Some("Album".into()),
        duration: Some(200),
        suffix: "flac".into(),
        bit_rate: 900,
        isrcs: Vec::new(),
    }])));
    ownership.set_resolve(Arc::new(move |_| {
        let path = alex_copy.clone();
        async move { Ok(Some(path)) }.boxed()
    }));
    ownership.set_scope(ScopeResolver::new(
        world.settings.clone(),
        world.family.clone(),
        world.index.clone(),
    ));
    let owned = Arc::new(HeartOwnership::new(
        Arc::new(ownership),
        metadata.clone(),
        world.settings.clone(),
        None,
        None,
    ));
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let lidarr = Arc::new(FakeLidarr::default().tracks(|_, _, _| Ok(true)));
    let coordinator = HeartAcquisitionCoordinator::new(
        world.settings.clone(),
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
        HeartCoordinatorExtras {
            owned: Some(owned),
            ..Default::default()
        },
    );
    let personal = Arc::new(PersonalLibrary::new(
        world.settings.clone(),
        world.family.clone(),
        world.index.clone(),
        world.pool.clone(),
    ));
    coordinator.use_family(FamilyHooks {
        gate: Arc::new(LibraryWriteGate::new(
            world.settings.clone(),
            world.family.clone(),
            personal,
        )),
        pool: world.pool.clone(),
        metadata,
    });
    Chain {
        world,
        queue,
        lidarr,
        coordinator,
    }
}

/// The next queued request, or `None` within 300 ms.
async fn next_queued(queue: &TrackAcquisitionQueue) -> Option<Arc<AcquisitionRequest>> {
    tokio::time::timeout(
        Duration::from_millis(300),
        queue.dequeue(&CancellationToken::new()),
    )
    .await
    .ok()
    .flatten()
}

#[tokio::test]
async fn owner_heart_downloads_even_if_a_member_owns_it() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], false);
    let chain = chain(world);
    let coordinator = chain.coordinator.clone();
    let running = tokio::spawn(async move {
        coordinator
            .acquire_track_outcome("test", "1", Some("winters"), &AcquisitionTarget::Shared)
            .await
    });
    let request = next_queued(&chain.queue)
        .await
        .expect("queued: Alex's copy is not the library's");
    assert_eq!(
        request.place(),
        AcquisitionPlace::default(),
        "into the shared library"
    );
    request.completion.try_set_result("/music/Artist/Album/1.flac");
    let outcome = running.await.expect("joins").expect("ran");
    assert_eq!(outcome, TrackOutcome::Landed("/music/Artist/Album/1.flac".into()));
}

#[tokio::test]
async fn a_member_heart_counts_their_own_copy_and_never_anothers() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], false);
    let chain = chain(world);
    let alex = chain
        .coordinator
        .acquire_track_outcome(
            "test",
            "1",
            Some("alex"),
            &AcquisitionTarget::Member("alex".into()),
        )
        .await
        .expect("ran");
    assert_eq!(alex, TrackOutcome::AlreadyThere);
    assert!(next_queued(&chain.queue).await.is_none());

    // Sam cannot see Alex's library: their heart downloads, into the pool, for them.
    let coordinator = chain.coordinator.clone();
    tokio::spawn(async move {
        coordinator
            .acquire_track_outcome("test", "1", Some("sam"), &AcquisitionTarget::Member("sam".into()))
            .await
    });
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(request.pool_destination(), Some(vec!["sam".to_string()]));
}

#[tokio::test]
async fn member_chain_skips_lidarr() {
    let world = World::new(None);
    configure(
        &world,
        &[HeartDownloadSource::Lidarr, HeartDownloadSource::YouTube],
        false,
    );
    let chain = chain(world);
    let coordinator = chain.coordinator.clone();
    tokio::spawn(async move {
        coordinator
            .acquire_track_outcome("test", "2", Some("sam"), &AcquisitionTarget::Member("sam".into()))
            .await
    });
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(request.source_override, Some(DownloadSource::YouTube));
    assert_eq!(chain.lidarr.calls(), 0, "Lidarr fills the shared library only");

    // The owner's chain still starts with Lidarr.
    chain
        .coordinator
        .acquire_track_outcome("test", "3", Some("winters"), &AcquisitionTarget::Shared)
        .await
        .expect("ran");
    assert_eq!(chain.lidarr.calls(), 1);
}

#[tokio::test]
async fn plays_keep_a_copy_only_for_those_who_add_at_once() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], true);
    let chain = chain(world);

    chain
        .coordinator
        .queue_play("test", "9", Some("sam"), None, Some("sam"));
    assert!(
        next_queued(&chain.queue).await.is_none(),
        "a listener's play keeps no copy"
    );

    chain
        .coordinator
        .queue_play("test", "9", Some("alex"), None, Some("alex"));
    let alex = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(alex.pool_destination(), Some(vec!["alex".to_string()]));

    chain
        .coordinator
        .queue_play("test", "8", Some("winters"), None, Some("winters"));
    let owner = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(owner.place(), AcquisitionPlace::default());
}

#[tokio::test]
async fn import_songs_go_where_the_member_may_add_them() {
    let world = World::new(None);
    configure(&world, &[HeartDownloadSource::YouTube], false);
    let mut pat = member("pat", Preset::Listener, 6);
    pat.edits.insert("AddToLibrary".into(), json!("SaveOnly"));
    world.family.upsert_member(pat);
    let mut full = member("max", Preset::Member, 7);
    full.edits.insert("StorageLimitGb".into(), json!(1));
    world.family.upsert_member(full);
    world
        .index
        .note_placed(7, &format!("{}/max/A/B/c.flac", world.members()), 2_000_000_000);
    let chain = chain(world);
    let coordinator = &chain.coordinator;
    assert_eq!(
        coordinator.import_target("winters"),
        Ok(AcquisitionTarget::Shared)
    );
    assert_eq!(
        coordinator.import_target("alex"),
        Ok(AcquisitionTarget::Member("alex".into()))
    );
    assert_eq!(
        coordinator.import_target("sam"),
        Ok(AcquisitionTarget::Member("sam".into()))
    );
    assert_eq!(
        coordinator.import_target("pat"),
        Err(crate::services::family::library_write_gate::SAVE_ONLY_MESSAGE.to_string())
    );
    assert_eq!(
        coordinator.import_target("max"),
        Err("Your library is full (2.0 of 1 GB)".to_string())
    );
    drop(chain.world);
}

#[tokio::test]
async fn family_off_plays_and_hearts_as_always() {
    let world = World::with_family(None, false);
    // Nobody has a library of their own: Family was never used here.
    world.family.remove_member("alex");
    world.family.remove_member("sam");
    configure(&world, &[HeartDownloadSource::YouTube], true);
    let chain = chain(world);
    chain
        .coordinator
        .queue_play("test", "9", Some("sam"), None, Some("sam"));
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(request.place(), AcquisitionPlace::default());
    // The file under the members folder is just the library's.
    let owner = chain
        .coordinator
        .acquire_track_outcome("test", "1", Some("winters"), &AcquisitionTarget::Shared)
        .await
        .expect("ran");
    assert_eq!(owner, TrackOutcome::AlreadyThere);
}

#[tokio::test]
async fn family_turned_off_never_counts_a_members_own_copy_as_the_librarys() {
    let world = World::with_family(None, false);
    configure(&world, &[HeartDownloadSource::YouTube], true);
    let chain = chain(world);
    let coordinator = chain.coordinator.clone();
    let owner = tokio::spawn(async move {
        coordinator
            .acquire_track_outcome("test", "1", Some("winters"), &AcquisitionTarget::Shared)
            .await
    });
    // Alex's library is still there, so the shared library does not have the song: it is fetched.
    let request = next_queued(&chain.queue).await.expect("queued");
    assert_eq!(request.external_id, "1");
    assert_eq!(request.place(), AcquisitionPlace::default());
    owner.abort();
}
