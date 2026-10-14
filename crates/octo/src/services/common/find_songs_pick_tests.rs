//! Find songs picks as they reach the queues: a second pick of a song waiting in the upgrade
//! queue, picks no download used, and how many looks one person may run at once.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use octo_core::models::download::AcquisitionCandidate;
use octo_core::settings::{
    AppSettings, LibraryAction, LibraryActionDefinition, LibraryActionSettings, LidarrSettings, SettingsStore,
};
use tokio_util::sync::CancellationToken;

use super::download_log_tests::upgrade_worker;
use super::download_picks::{AnywaySong, DownloadPicks, LIDARR_SOURCE, PickedCopy};
use super::song_finder::{
    FindSnapshot, FindTarget, FoundCopy, SearchAnswer, SongFinder, SongFinderServices, find_states,
    library_action_states,
};
use super::track_acquisition_queue::TrackAcquisitionQueue;
use crate::services::library::LibraryActionState;
use crate::services::library::UpgradeSources;
use crate::services::library::library_action_executor::LibraryActionOutcome;
use crate::services::library::upgrade_queue::{UpgradeAsk, UpgradeQueue};

fn owned() -> FindTarget {
    FindTarget {
        library_id: Some("nd-1".into()),
        owned_format: Some("mp3".into()),
        ..FindTarget::new("Air", "Sexy Boy", Some("Moon Safari"), None, "nd-1")
    }
}

struct Server {
    finder: Arc<SongFinder>,
    queue: Arc<UpgradeQueue>,
    picks: Arc<DownloadPicks>,
    settings: Arc<SettingsStore>,
}

/// A FLAC release of the album from indexer `indexer`, whose release id is the indexer's name.
fn flac(indexer: &str, rank: i32) -> FoundCopy {
    let release = format!("Air - Moon Safari (1998) [FLAC] ({indexer})");
    FoundCopy::new(
        AcquisitionCandidate {
            source: LIDARR_SOURCE.into(),
            file: Some(release.clone()),
            format: Some("FLAC".into()),
            size: Some(300_000_000),
            length: Some(298),
            rank: Some(rank),
            ..Default::default()
        },
        PickedCopy {
            source: LIDARR_SOURCE.into(),
            release_guid: Some(indexer.into()),
            indexer_id: Some(rank),
            release_title: Some(release),
            size: 300_000_000,
            ..Default::default()
        },
    )
}

fn three_flacs() -> anyhow::Result<SearchAnswer> {
    Ok((
        vec![flac("peer1", 1), flac("peer2", 2), flac("peer3", 3)],
        vec!["Air - Moon Safari (1998)".to_string()],
        Some("3 releases of Moon Safari".to_string()),
    ))
}

fn build(target: FindTarget, acquisitions: Option<Arc<TrackAcquisitionQueue>>) -> Server {
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run: false,
            allowed_users: vec!["alice".into(), "bob".into()],
            actions: vec![LibraryActionDefinition {
                action: LibraryAction::BetterQuality,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        },
        lidarr: LidarrSettings {
            base_url: Some("http://lidarr.test".into()),
            api_key: Some("k".into()),
            root_folder_path: Some("/music".into()),
            quality_profile_id: 1,
            metadata_profile_id: 1,
            ..Default::default()
        },
        ..Default::default()
    }));
    let queue = Arc::new(UpgradeQueue::new());
    let picks = Arc::new(DownloadPicks::default());
    let finder = Arc::new(SongFinder::new(SongFinderServices {
        settings: Some(settings.clone()),
        upgrade_queue: Some(queue.clone()),
        upgrade_sources: Some(Arc::new(UpgradeSources::new(settings.clone()))),
        executor: true,
        acquisitions,
        picks: Some(picks.clone()),
        ..Default::default()
    }));
    finder.seams(|seams| {
        seams.resolve = Arc::new(move |_, _| {
            let target = target.clone();
            async move { Some(target) }.boxed()
        });
        seams.sources_for = Arc::new(|_| vec![(LIDARR_SOURCE.to_string(), None)]);
        seams.search_lidarr = Arc::new(|_, _| async { three_flacs() }.boxed());
    });
    Server {
        finder,
        queue,
        picks,
        settings,
    }
}

async fn look(finder: &Arc<SongFinder>, user: &str) -> FindSnapshot {
    let id = finder
        .start("nd-1", user, &CancellationToken::new())
        .await
        .expect("a look")
        .id;
    finder.wait(&id, Duration::from_secs(10)).await.expect("the look")
}

fn queued_peer(queue: &UpgradeQueue) -> Option<String> {
    let jobs = queue.snapshot();
    assert_eq!(jobs.len(), 1);
    jobs[0].pick.as_ref().and_then(|p| p.release_guid.clone())
}

// ---- A second pick ----------------------------------------------------------------------

#[tokio::test]
async fn a_second_pick_before_the_upgrade_starts_takes_the_place_of_the_first() {
    let server = build(owned(), None);
    let look = look(&server.finder, "alice").await;

    assert_eq!(
        server
            .finder
            .pick_by_id(&look.id, &look.copies[0].id, "alice")
            .await
            .state,
        library_action_states::QUEUED
    );
    assert_eq!(queued_peer(&server.queue).as_deref(), Some("peer1"));

    let second = server
        .finder
        .pick_by_id(&look.id, &look.copies[1].id, "alice")
        .await;

    assert_eq!(second.state, library_action_states::QUEUED);
    assert!(
        second.detail.starts_with("Changed to the copy you picked"),
        "{}",
        second.detail
    );
    assert_eq!(queued_peer(&server.queue).as_deref(), Some("peer2"));
}

#[tokio::test]
async fn a_pick_for_an_upgrade_already_running_or_someone_elses_says_another_copy_is_on_its_way() {
    let server = build(owned(), None);
    let alice = look(&server.finder, "alice").await;
    server
        .finder
        .pick_by_id(&alice.id, &alice.copies[0].id, "alice")
        .await;

    let bobs = look(&server.finder, "bob").await;
    let bob = server
        .finder
        .pick_by_id(&bobs.id, &bobs.copies[2].id, "bob")
        .await;
    assert_eq!(bob.state, library_action_states::SKIPPED);
    assert_eq!(
        bob.detail,
        "Another copy of this song is already on its way. Pick again once it ends."
    );

    assert!(server.queue.take_next().is_some());
    let late = server
        .finder
        .pick_by_id(&alice.id, &alice.copies[1].id, "alice")
        .await;
    assert_eq!(late.state, library_action_states::SKIPPED);
    assert_eq!(queued_peer(&server.queue).as_deref(), Some("peer1"));

    // The same copy again is the pick already waiting, not another one.
    assert_eq!(
        server
            .finder
            .pick_by_id(&alice.id, &alice.copies[0].id, "alice")
            .await
            .state,
        library_action_states::QUEUED
    );
}

// ---- Picks no download used ---------------------------------------------------------------

#[tokio::test]
async fn a_pick_is_let_go_when_its_download_ends_without_using_it() {
    let target = FindTarget {
        external_id: Some("ext1".into()),
        ..FindTarget::new("Air", "Sexy Boy", Some("Moon Safari"), Some(298), "ext1")
    };
    let acquisitions = Arc::new(TrackAcquisitionQueue::new());
    let server = build(target, Some(acquisitions.clone()));
    let look = look(&server.finder, "alice").await;

    assert_eq!(
        server
            .finder
            .pick_by_id(&look.id, &look.copies[0].id, "alice")
            .await
            .state,
        library_action_states::QUEUED
    );
    assert!(server.picks.has("ext1"));

    // The download ends at once, "already in your library", and never takes the pick.
    let request = acquisitions
        .dequeue(&CancellationToken::new())
        .await
        .expect("the request");
    acquisitions.release(&request);
    request
        .completion
        .try_set_result("/music/Air/Moon Safari/02 - Sexy Boy.flac");
    for _ in 0..200 {
        if !server.picks.has("ext1") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(!server.picks.has("ext1"));
}

#[test]
fn forgetting_a_pick_leaves_a_newer_one_for_the_same_song() {
    let picks = DownloadPicks::default();
    let copy = |release: &str| {
        Arc::new(PickedCopy {
            source: LIDARR_SOURCE.into(),
            release_guid: Some(release.into()),
            indexer_id: Some(1),
            ..Default::default()
        })
    };
    let (first, second) = (copy("p1"), copy("p2"));
    picks.pin("ext1", first.clone());
    picks.pin("ext1", second);

    assert!(!picks.forget("ext1", &first));
    assert_eq!(
        picks
            .take("ext1", "Lidarr")
            .and_then(|p| p.release_guid.clone())
            .as_deref(),
        Some("p2")
    );
}

#[tokio::test]
async fn an_upgrade_lets_go_of_a_pick_it_never_used() {
    let queue = Arc::new(UpgradeQueue::new());
    let picks = Arc::new(DownloadPicks::default());
    queue.add(
        vec![UpgradeAsk {
            title: Some("Sexy Boy".into()),
            artist: Some("Air".into()),
            album: Some("Moon Safari".into()),
            suffix: Some("mp3".into()),
            pick: Some((*flac("peer1", 1).pick).clone()),
            ..UpgradeAsk::new("nd-1")
        }],
        "alice",
        "find",
    );
    let worker = upgrade_worker(&queue, None, picks.clone(), |request| {
        (request.on_replacement_queued.as_ref().expect("a follower"))("outside", "ext1");
        // Joined a download already running, so this pick was never taken.
        LibraryActionOutcome::new(LibraryActionState::Failed, Some("joined".into()))
    });

    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;

    assert!(!picks.has("ext1"));
}

// ---- Replacing anyway ------------------------------------------------------------------------

fn owned_flac() -> FindTarget {
    FindTarget {
        library_id: Some("nd-1".into()),
        owned_format: Some("flac".into()),
        owned_quality: Some("FLAC 16-bit 44.1 kHz".into()),
        owned_path: Some("/music/Air/Moon Safari/02 - Sexy Boy.flac".into()),
        ..FindTarget::new("Air", "Sexy Boy", Some("Moon Safari"), Some(298), "nd-1")
    }
}

const SAME_QUALITY: &str = "This copy is FLAC like yours, so it is not better. Replace yours anyway?";

#[tokio::test]
async fn an_app_that_cannot_ask_hears_the_old_refusal_and_one_that_can_is_asked_first() {
    let server = build(owned_flac(), None);
    let look = look(&server.finder, "alice").await;
    let copy = &look.copies[0].id;

    let old = server.finder.pick_by_id(&look.id, copy, "alice").await;
    assert_eq!(old.state, library_action_states::SKIPPED);
    assert_eq!(
        old.detail,
        "Your copy is already lossless, so nothing would be better."
    );

    let asked = server
        .finder
        .pick_copy(&look.id, copy, "alice", Some(false))
        .await;
    assert_eq!(asked.state, library_action_states::CONFIRM);
    assert_eq!(asked.detail, SAME_QUALITY);
    assert!(
        server.queue.snapshot().is_empty(),
        "nothing is queued before the person says yes"
    );

    // Picking by place asks the same way.
    let by_place = server.finder.pick_place(&look.id, 0, "alice", Some(false)).await;
    assert_eq!(by_place.state, library_action_states::CONFIRM);
}

#[tokio::test]
async fn replace_anyway_queues_the_copy_held_to_the_library_song_with_no_quality_check() {
    let server = build(owned_flac(), None);
    let look = look(&server.finder, "alice").await;

    let picked = server
        .finder
        .pick_copy(&look.id, &look.copies[1].id, "alice", Some(true))
        .await;

    assert_eq!(picked.state, library_action_states::QUEUED, "{}", picked.detail);
    let jobs = server.queue.snapshot();
    assert_eq!(jobs.len(), 1);
    let pick = jobs[0].pick.clone().expect("the pick");
    assert_eq!(pick.release_guid.as_deref(), Some("peer2"));
    assert_eq!(
        pick.anyway,
        Some(AnywaySong {
            title: "Sexy Boy".into(),
            album: Some("Moon Safari".into()),
            length: Some(298),
            retitle: false,
        })
    );
}

#[tokio::test]
async fn a_better_copy_asked_about_is_ready_and_then_an_ordinary_pick() {
    let server = build(owned(), None);
    let look = look(&server.finder, "alice").await;

    let asked = server
        .finder
        .pick_copy(&look.id, &look.copies[0].id, "alice", Some(false))
        .await;
    assert_eq!(asked.state, library_action_states::READY);
    assert!(server.queue.snapshot().is_empty(), "asking fetches nothing");
    let compare = asked.compare.expect("the two copies");
    assert_eq!(compare.yours.quality.as_deref(), Some("MP3"));
    assert_eq!(compare.copy.quality.as_deref(), Some("FLAC"));
    assert_eq!(compare.copy.length, Some(298));
    assert_eq!(compare.copy.take, None);

    let picked = server
        .finder
        .pick_copy(&look.id, &look.copies[0].id, "alice", Some(true))
        .await;
    assert_eq!(picked.state, library_action_states::QUEUED);
    let jobs = server.queue.snapshot();
    assert_eq!(jobs[0].pick.as_ref().and_then(|p| p.anyway.clone()), None);
}

#[tokio::test]
async fn a_live_take_in_the_library_is_named_on_the_look_and_in_the_question() {
    let target = FindTarget {
        library_id: Some("nd-1".into()),
        owned_format: Some("flac".into()),
        owned_quality: Some("FLAC 16-bit 44.1 kHz".into()),
        owned_path: Some("/music/Air/Live at the Olympia/05 - Sexy Boy (Live).flac".into()),
        ..FindTarget::new(
            "Air",
            "Sexy Boy (Live)",
            Some("Live at the Olympia"),
            Some(402),
            "nd-1",
        )
    };
    let server = build(target, None);
    let look = look(&server.finder, "alice").await;
    assert_eq!(look.take, Some(super::replace_anyway::OtherTake::Live));

    let asked = server
        .finder
        .pick_copy(&look.id, &look.copies[0].id, "alice", Some(false))
        .await;
    assert_eq!(asked.state, library_action_states::CONFIRM);
    let compare = asked.compare.expect("the two copies");
    assert_eq!(compare.yours.take.as_deref(), Some("live recording"));
    assert_eq!(compare.yours.length, Some(402));
    assert_eq!(compare.copy.take.as_deref(), Some("the plain song"));

    let replaced = server
        .finder
        .pick_copy(&look.id, &look.copies[0].id, "alice", Some(true))
        .await;
    assert_eq!(replaced.state, library_action_states::QUEUED);
    let song = server.queue.snapshot()[0]
        .pick
        .as_ref()
        .and_then(|p| p.anyway.clone())
        .expect("forced");
    assert!(song.retitle);
    assert_eq!(song.title, "Sexy Boy");
}

#[tokio::test]
async fn someone_who_may_no_longer_replace_songs_is_told_so_and_never_asked() {
    let server = build(owned_flac(), None);
    let look = look(&server.finder, "alice").await;
    let mut changed = (*server.settings.current()).clone();
    changed.library_actions.allowed_users = vec!["bob".into()];
    server.settings.set(changed);

    for anyway in [Some(false), Some(true)] {
        let picked = server
            .finder
            .pick_copy(&look.id, &look.copies[0].id, "alice", anyway)
            .await;
        assert_eq!(picked.state, library_action_states::SKIPPED);
        assert_eq!(picked.detail, "alice is not on the library actions allowed list.");
    }
    assert!(server.queue.snapshot().is_empty());
}

#[tokio::test]
async fn the_upgrade_worker_hands_the_anyway_song_to_the_replacement() {
    let queue = Arc::new(UpgradeQueue::new());
    let picks = Arc::new(DownloadPicks::default());
    let song = AnywaySong {
        title: "Sexy Boy".into(),
        album: None,
        length: Some(298),
        retitle: true,
    };
    queue.add(
        vec![UpgradeAsk {
            title: Some("Sexy Boy (Live)".into()),
            artist: Some("Air".into()),
            suffix: Some("flac".into()),
            pick: Some(PickedCopy {
                anyway: Some(song.clone()),
                ..(*flac("peer1", 1).pick).clone()
            }),
            ..UpgradeAsk::new("nd-1")
        }],
        "alice",
        "find",
    );
    let heard = Arc::new(parking_lot::Mutex::new(None));
    let seen = heard.clone();
    let worker = upgrade_worker(&queue, None, picks, move |request| {
        *seen.lock() = Some((request.anyway.clone(), request.only_source));
        LibraryActionOutcome::new(LibraryActionState::Failed, Some("not this time".into()))
    });

    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;

    assert_eq!(
        heard.lock().clone(),
        Some((Some(song), Some(octo_core::settings::DownloadSource::Lidarr)))
    );
}

#[test]
fn a_pick_with_no_anyway_song_is_written_as_before() {
    let plain = serde_json::to_string(&*flac("peer1", 1).pick).expect("json");
    assert!(!plain.contains("Anyway"), "{plain}");
    let forced = PickedCopy {
        anyway: Some(AnywaySong {
            title: "Sexy Boy".into(),
            album: None,
            length: Some(298),
            retitle: false,
        }),
        ..(*flac("peer1", 1).pick).clone()
    };
    let text = serde_json::to_string(&forced).expect("json");
    assert!(
        text.contains(r#""Anyway":{"Title":"Sexy Boy","Album":null,"Length":298}"#),
        "{text}"
    );
    let back: PickedCopy = serde_json::from_str(&text).expect("read back");
    assert_eq!(back, forced);
}

// ---- How many looks run at once --------------------------------------------------------------

#[tokio::test]
async fn one_person_runs_two_looks_at_once_and_another_persons_look_is_never_dropped_for_room() {
    let slow_song = FindTarget {
        external_id: Some("ext1".into()),
        ..FindTarget::new("Air", "Sexy Boy", Some("Moon Safari"), Some(298), "ext1")
    };
    let quick_song = FindTarget {
        title: "Kelly Watch the Stars".into(),
        external_id: Some("ext2".into()),
        ..slow_song.clone()
    };
    let server = build(slow_song.clone(), None);
    let (release, released) = tokio::sync::watch::channel(false);
    let slow_title = slow_song.title.clone();
    server.finder.seams(|seams| {
        // Looks for "slow" wait; every other one ends at once.
        seams.resolve = Arc::new(move |id, _| {
            let found = if id == "slow" {
                slow_song.clone()
            } else {
                quick_song.clone()
            };
            async move { Some(found) }.boxed()
        });
        seams.search_lidarr = Arc::new(move |song, _| {
            let slow = song.title == slow_title;
            let mut released = released.clone();
            async move {
                if slow {
                    let _ = released.wait_for(|done| *done).await;
                }
                Ok((Vec::new(), Vec::new(), Some("Nothing found".to_string())))
            }
            .boxed()
        });
    });
    let ct = CancellationToken::new();

    let bobs = server.finder.start("slow", "bob", &ct).await.expect("bob's look");
    for _ in 0..2 {
        assert_eq!(
            server
                .finder
                .start("slow", "alice", &ct)
                .await
                .expect("a look")
                .state,
            find_states::SEARCHING
        );
    }
    let third = server.finder.start("slow", "alice", &ct).await.expect("a look");

    assert_eq!(third.state, find_states::FAILED);
    assert_eq!(
        third.error.as_deref(),
        Some("You already have 2 searches running. Search again once one of them ends.")
    );

    // Many finished looks later, bob's look, still running, is still there to follow.
    for i in 0..SongFinder::CAPACITY + 5 {
        let id = server
            .finder
            .start("quick", &format!("user{i}"), &ct)
            .await
            .expect("a look")
            .id;
        server.finder.wait(&id, Duration::from_secs(10)).await;
    }

    assert_eq!(
        server.finder.get(&bobs.id, "bob").expect("bob's look").state,
        find_states::SEARCHING
    );
    release.send(true).expect("released");
}

#[tokio::test]
async fn a_library_songs_look_is_only_for_the_people_who_may_replace_it() {
    let server = build(owned(), None);
    let ct = CancellationToken::new();

    let carol = server.finder.start("nd-1", "carol", &ct).await.expect("a look");

    assert_eq!(carol.state, find_states::FAILED);
    assert_eq!(
        carol.error.as_deref(),
        Some("carol is not on the library actions allowed list.")
    );
    assert_eq!(
        server
            .finder
            .start("nd-1", "alice", &ct)
            .await
            .expect("a look")
            .state,
        find_states::SEARCHING
    );
}
