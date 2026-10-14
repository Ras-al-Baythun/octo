//! The heart acquisition coordinator. The settings-only tests (the legacy fallback's order, a
//! configured order with missing sources appended disabled) live with the settings, in
//! `octo_core::settings::subsonic`.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::Clock;
use octo_core::settings::{
    AppSettings, DownloadSource, HeartDownloadSource, HeartDownloadStep, SubsonicSettings,
};

use super::*;
use crate::services::common::AcquisitionRequest;
use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr, until};

fn store(subsonic: SubsonicSettings) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        subsonic,
        ..Default::default()
    }))
}

fn legacy(source: DownloadSource) -> SubsonicSettings {
    SubsonicSettings {
        download_source: source,
        ..Default::default()
    }
}

fn steps(list: &[(HeartDownloadSource, bool)]) -> SubsonicSettings {
    SubsonicSettings {
        heart_download_sources: list
            .iter()
            .map(|(source, enabled)| HeartDownloadStep {
                source: *source,
                enabled: Some(*enabled),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn step(source: HeartDownloadSource, song: bool, album: bool) -> HeartDownloadStep {
    HeartDownloadStep {
        source,
        song_enabled: Some(song),
        album_enabled: Some(album),
        ..Default::default()
    }
}

fn coordinator(
    settings: Arc<SettingsStore>,
    queue: Arc<TrackAcquisitionQueue>,
    direct: Arc<FakeDownloads>,
    lidarr: Arc<FakeLidarr>,
) -> Arc<HeartAcquisitionCoordinator> {
    HeartAcquisitionCoordinator::new(settings, queue, direct, lidarr, HeartCoordinatorExtras::default())
}

fn new_queue() -> Arc<TrackAcquisitionQueue> {
    Arc::new(TrackAcquisitionQueue::new())
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
async fn lidarr_source_routes_track_and_album_without_calling_direct_downloader() {
    let lidarr = Arc::new(
        FakeLidarr::default()
            .tracks(|p, id, notify| Ok(p == "outside" && id == "track-id" && notify))
            .albums(|p, id, notify| Ok(p == "outside" && id == "album-id" && notify)),
    );
    let direct = Arc::new(FakeDownloads::default());
    let coordinator = coordinator(
        store(legacy(DownloadSource::Lidarr)),
        new_queue(),
        direct.clone(),
        lidarr.clone(),
    );

    coordinator
        .acquire_track("outside", "track-id", None)
        .await
        .expect("ran");
    coordinator
        .acquire_album("outside", "album-id", None)
        .await
        .expect("ran");

    assert_eq!(lidarr.track_calls_for("outside", "track-id", true), 1);
    assert_eq!(lidarr.album_calls_for("outside", "album-id", true), 1);
    assert_eq!(*direct.remaining_calls.lock(), 0);
}

#[tokio::test]
async fn youtube_source_keeps_album_on_existing_direct_path() {
    let lidarr = Arc::new(FakeLidarr::default());
    let direct = Arc::new(FakeDownloads::albums(|source, suppress| {
        Ok(source == DownloadSource::YouTube && !suppress)
    }));
    let coordinator = coordinator(
        store(legacy(DownloadSource::YouTube)),
        new_queue(),
        direct.clone(),
        lidarr.clone(),
    );

    coordinator
        .acquire_album("outside", "album-id", None)
        .await
        .expect("ran");

    let calls = direct.album_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str(), calls[0].2, calls[0].3),
        ("outside", "album-id", DownloadSource::YouTube, false)
    );
    assert_eq!(lidarr.calls(), 0);
}

#[tokio::test]
async fn download_source_change_takes_effect_without_rebuilding_coordinator() {
    let lidarr = Arc::new(FakeLidarr::default().albums(|_, id, notify| Ok(id == "second-album" && notify)));
    let direct = Arc::new(FakeDownloads::albums(|source, suppress| {
        Ok(source == DownloadSource::YouTube && !suppress)
    }));
    let settings = store(legacy(DownloadSource::YouTube));
    let coordinator = coordinator(settings.clone(), new_queue(), direct.clone(), lidarr.clone());

    coordinator
        .acquire_album("outside", "first-album", None)
        .await
        .expect("ran");
    settings.set(AppSettings {
        subsonic: legacy(DownloadSource::Lidarr),
        ..Default::default()
    });
    coordinator
        .acquire_album("outside", "second-album", None)
        .await
        .expect("ran");

    let calls = direct.album_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "first-album");
    assert_eq!(lidarr.album_calls_for("outside", "second-album", true), 1);
}

#[tokio::test]
async fn album_priority_stops_at_first_successful_source() {
    let lidarr = Arc::new(FakeLidarr::default());
    let direct = Arc::new(FakeDownloads::albums(|source, suppress| {
        Ok(source == DownloadSource::YouTube && suppress)
    }));
    let settings = store(steps(&[
        (HeartDownloadSource::YouTube, true),
        (HeartDownloadSource::Lidarr, true),
    ]));
    let coordinator = coordinator(settings, new_queue(), direct, lidarr.clone());

    coordinator
        .acquire_album("outside", "album-id", None)
        .await
        .expect("ran");

    assert_eq!(lidarr.calls(), 0);
}

#[tokio::test]
async fn album_priority_falls_through_to_lidarr_after_direct_failure() {
    let lidarr = Arc::new(
        FakeLidarr::default().albums(|p, id, notify| Ok(p == "outside" && id == "album-id" && notify)),
    );
    let direct = Arc::new(FakeDownloads::albums(|_, _| Ok(false)));
    let settings = store(steps(&[
        (HeartDownloadSource::YouTube, true),
        (HeartDownloadSource::Lidarr, true),
    ]));
    let coordinator = coordinator(settings, new_queue(), direct.clone(), lidarr.clone());

    coordinator
        .acquire_album("outside", "album-id", None)
        .await
        .expect("ran");

    assert_eq!(lidarr.album_calls_for("outside", "album-id", true), 1);
    assert_eq!(direct.album_sources(), [DownloadSource::YouTube]);
}

#[tokio::test]
async fn track_priority_falls_through_from_youtube_to_lidarr() {
    let lidarr = Arc::new(
        FakeLidarr::default().tracks(|p, id, notify| Ok(p == "outside" && id == "track-id" && notify)),
    );
    let queue = new_queue();
    let settings = store(steps(&[
        (HeartDownloadSource::YouTube, true),
        (HeartDownloadSource::Lidarr, true),
    ]));
    let coordinator = coordinator(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
    );

    let running = coordinator.clone();
    let acquisition = tokio::spawn(async move { running.acquire_track("outside", "track-id", None).await });
    let request = queue.dequeue(&CancellationToken::new()).await.expect("a request");
    assert_eq!(request.source_override, Some(DownloadSource::YouTube));
    assert!(!request.notify_on_failure);
    queue.release(&request);
    request.completion.try_set_error(anyhow::anyhow!("no peer"));
    acquisition.await.expect("joined").expect("ran");

    assert_eq!(lidarr.track_calls_for("outside", "track-id", true), 1);
}

#[tokio::test]
async fn song_and_album_hearts_use_their_own_per_source_switches() {
    let lidarr = Arc::new(
        FakeLidarr::default().albums(|p, id, notify| Ok(p == "outside" && id == "album-id" && notify)),
    );
    let queue = new_queue();
    let settings = store(SubsonicSettings {
        heart_download_sources: vec![
            step(HeartDownloadSource::YouTube, true, false),
            step(HeartDownloadSource::Lidarr, false, true),
        ],
        ..Default::default()
    });
    let coordinator = coordinator(
        settings,
        queue.clone(),
        Arc::new(FakeDownloads::default()),
        lidarr.clone(),
    );

    let running = coordinator.clone();
    let track = tokio::spawn(async move { running.acquire_track("outside", "track-id", None).await });
    let request = queue.dequeue(&CancellationToken::new()).await.expect("a request");
    assert_eq!(request.source_override, Some(DownloadSource::YouTube));
    request.completion.try_set_result("/music/track.flac");
    queue.release(&request);
    track.await.expect("joined").expect("ran");
    coordinator
        .acquire_album("outside", "album-id", None)
        .await
        .expect("ran");

    assert!(lidarr.track_calls.lock().is_empty());
    assert_eq!(lidarr.album_calls_for("outside", "album-id", true), 1);
}

fn play_settings(
    download_on_play: bool,
    lidarr_album_on_play: bool,
    list: Vec<HeartDownloadStep>,
) -> SubsonicSettings {
    SubsonicSettings {
        download_on_play,
        lidarr_album_on_play,
        heart_download_sources: if list.is_empty() {
            vec![
                step(HeartDownloadSource::Lidarr, true, true),
                step(HeartDownloadSource::YouTube, true, true),
            ]
        } else {
            list
        },
        ..Default::default()
    }
}

fn with_tracker(
    settings: SubsonicSettings,
    queue: Arc<TrackAcquisitionQueue>,
    lidarr: Arc<FakeLidarr>,
    tracker: Option<Arc<AcquisitionTracker>>,
) -> Arc<HeartAcquisitionCoordinator> {
    HeartAcquisitionCoordinator::new(
        store(settings),
        queue,
        Arc::new(FakeDownloads::default()),
        lidarr,
        HeartCoordinatorExtras {
            tracker,
            ..Default::default()
        },
    )
}

#[tokio::test]
async fn play_starts_nothing_by_default() {
    let lidarr = Arc::new(FakeLidarr::default());
    let queue = new_queue();
    let coordinator = with_tracker(
        play_settings(false, false, Vec::new()),
        queue.clone(),
        lidarr.clone(),
        None,
    );

    coordinator.queue_play("outside", "track-id", None, None, None);

    assert!(next_queued(&queue).await.is_none());
    assert_eq!(lidarr.calls(), 0);
}

#[tokio::test]
async fn download_on_play_queues_the_first_direct_song_source_and_skips_lidarr() {
    let lidarr = Arc::new(FakeLidarr::default());
    let queue = new_queue();
    let coordinator = with_tracker(
        play_settings(true, false, Vec::new()),
        queue.clone(),
        lidarr.clone(),
        None,
    );

    coordinator.queue_play("outside", "track-id", Some("felix"), None, None);

    let request = next_queued(&queue).await.expect("queued");
    assert_eq!(request.external_id, "track-id");
    assert!(!request.is_star);
    assert!(request.force_permanent);
    assert_eq!(request.source_override, Some(DownloadSource::YouTube));
    assert_eq!(request.requested_by(), ["felix"]);
    assert_eq!(lidarr.calls(), 0);
}

#[tokio::test]
async fn lidarr_album_on_play_hands_each_track_to_lidarr_once() {
    let lidarr = Arc::new(
        FakeLidarr::default().tracks(|p, id, notify| Ok(p == "outside" && id == "track-id" && !notify)),
    );
    let queue = new_queue();
    let coordinator = with_tracker(
        play_settings(false, true, Vec::new()),
        queue.clone(),
        lidarr.clone(),
        None,
    );

    coordinator.queue_play("outside", "track-id", None, None, None);
    coordinator.queue_play("outside", "track-id", None, None, None);

    assert!(next_queued(&queue).await.is_none());
    assert_eq!(lidarr.track_calls_for("outside", "track-id", false), 1);
}

/// The hand-off is a task, so each play waits for it to settle before the next.
#[tokio::test]
async fn lidarr_album_on_play_tries_again_after_a_hand_off_that_did_not_take() {
    for throws in [false, true] {
        let mut answers = if throws {
            vec![Err(anyhow::anyhow!("down")), Ok(true)]
        } else {
            vec![Ok(false), Ok(true)]
        }
        .into_iter();
        let lidarr =
            Arc::new(FakeLidarr::default().tracks(move |_, _, _| answers.next().unwrap_or(Ok(false))));
        let coordinator = with_tracker(
            play_settings(false, true, Vec::new()),
            new_queue(),
            lidarr.clone(),
            None,
        );

        for i in 0..3 {
            coordinator.queue_play("outside", "track-id", None, None, None);
            let settled = coordinator.clone();
            let lidarr = lidarr.clone();
            until(move || {
                let calls = lidarr.track_calls_for("outside", "track-id", false);
                // The first hand-off did not take and lets go of the track; the second took.
                match i {
                    0 => calls == 1 && settled.lidarr_plays.lock().is_empty(),
                    _ => calls == 2,
                }
            })
            .await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            lidarr.track_calls_for("outside", "track-id", false),
            2,
            "throws {throws}"
        );
    }
}

#[tokio::test]
async fn only_one_played_song_waits_and_a_skipped_one_can_be_asked_for_again() {
    let queue = new_queue();
    let coordinator = with_tracker(
        play_settings(true, false, Vec::new()),
        queue.clone(),
        Arc::new(FakeLidarr::default()),
        None,
    );
    coordinator.queue_play("outside", "first", None, None, None);
    coordinator.queue_play("outside", "second", None, None, None);
    assert_eq!(queue.waiting_plays(), 1);
    assert_eq!(next_queued(&queue).await.expect("queued").external_id, "first");

    coordinator.queue_play("outside", "second", None, None, None);
    assert_eq!(next_queued(&queue).await.expect("queued").external_id, "second");
}

#[tokio::test]
async fn a_heart_goes_before_a_waiting_play_and_upgrades_one_it_joins() {
    let queue = new_queue();
    with_tracker(
        play_settings(true, false, Vec::new()),
        queue.clone(),
        Arc::new(FakeLidarr::default()),
        None,
    )
    .queue_play("outside", "played", None, None, None);
    let _ = queue.enqueue(
        "outside", "hearted", true, false, true, None, true, None, false, None,
    );
    assert_eq!(next_queued(&queue).await.expect("queued").external_id, "hearted");

    let _ = queue.enqueue(
        "outside",
        "played",
        true,
        false,
        true,
        None,
        true,
        Some("felix"),
        false,
        None,
    );
    let joined = next_queued(&queue).await.expect("queued");
    assert!(joined.heart_joined());
    assert!(joined.notifies_on_failure());
    assert!(joined.requested_by().contains(&"felix".to_string()));
}

#[tokio::test]
async fn a_played_download_has_a_row_that_fails_with_it() {
    let queue = new_queue();
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    let coordinator = with_tracker(
        play_settings(true, false, Vec::new()),
        queue.clone(),
        Arc::new(FakeLidarr::default()),
        Some(tracker.clone()),
    );
    coordinator.queue_play("outside", "track-id", None, Some("ext-1"), Some("felix"));
    coordinator.queue_play("outside", "skipped", None, Some("ext-2"), Some("felix"));
    let rows = tracker.for_user("felix");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, AcquisitionState::Queued);

    let request = next_queued(&queue).await.expect("queued");
    queue.release(&request);
    request.completion.try_set_error(anyhow::anyhow!("no peers"));
    let watched = tracker.clone();
    until(move || watched.for_user("felix")[0].state == AcquisitionState::Failed).await;
}

#[tokio::test]
async fn a_dropped_play_never_strands_a_heart() {
    // The heart for B arrives at the worst moment: while the play for B is being dropped.
    let queue = new_queue();
    let heart: Arc<parking_lot::Mutex<Option<crate::services::common::Completion>>> = Arc::default();
    let (slot, hearted) = (heart.clone(), Arc::downgrade(&queue));
    queue.set_on_skipped_play(Arc::new(move || {
        if let Some(queue) = hearted.upgrade() {
            *slot.lock() =
                Some(queue.enqueue("outside", "B", true, false, true, None, true, None, false, None));
        }
    }));
    assert!(
        queue
            .try_enqueue_play("outside", "A", DownloadSource::YouTube, None, None)
            .is_some()
    );

    assert!(
        queue
            .try_enqueue_play("outside", "B", DownloadSource::YouTube, None, None)
            .is_none()
    );
    let heart = heart.lock().clone().expect("the heart was queued");

    // The heart has its own request, queued and taken first, not a released play's.
    let next = next_queued(&queue).await.expect("queued");
    if next.external_id == "B" {
        queue.release(&next);
        next.completion.try_set_result("/music/b.flac");
    }
    let path = tokio::time::timeout(Duration::from_secs(2), heart.wait())
        .await
        .expect("in time")
        .expect("downloaded");
    assert_eq!(path, "/music/b.flac");
    assert!(next.is_star);
}
