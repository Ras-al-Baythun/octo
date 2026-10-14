//! Download attribution has to survive the one thing that makes it interesting, two people
//! wanting the same track. The queue deduplicates by track, so the second star joins a transfer
//! that is already running rather than starting another, and anything that reads the requester
//! at enqueue time would credit whoever happened to win that race and drop the rest.

use std::sync::Arc;

use futures::FutureExt;
use octo_core::models::download::DownloadHistoryEntry;
use tokio_util::sync::CancellationToken;

use super::track_acquisition_queue::{AcquisitionRequest, Completion, TrackAcquisitionQueue};

fn enqueue(queue: &TrackAcquisitionQueue, id: &str, is_star: bool, user: Option<&str>) -> Completion {
    queue.enqueue("deezer", id, is_star, false, true, None, false, user, false, None)
}

/// Takes the request the worker would take next. Never waits: every test here queues before it
/// reads.
fn dequeue(queue: &TrackAcquisitionQueue) -> Arc<AcquisitionRequest> {
    queue
        .dequeue(&CancellationToken::new())
        .now_or_never()
        .expect("nothing was queued")
        .expect("a request")
}

#[test]
fn enqueue_records_the_user_who_asked() {
    let queue = TrackAcquisitionQueue::new();
    enqueue(&queue, "1", true, Some("alice"));

    assert_eq!(dequeue(&queue).requested_by(), ["alice"]);
}

/// The case the original proposal did not cover. Both names, one transfer.
#[test]
fn enqueue_second_user_joining_an_in_flight_request_is_also_recorded() {
    let queue = TrackAcquisitionQueue::new();
    let first = enqueue(&queue, "1", true, Some("alice"));
    let second = enqueue(&queue, "1", true, Some("bob"));

    // Joined, not queued twice: the two completions are one.
    first.try_set_result("/music/a.flac");
    assert!(second.is_completed());

    assert_eq!(dequeue(&queue).requested_by(), ["alice", "bob"]);
}

/// A user can join after the worker has taken the request off the queue but before the transfer
/// finishes, which is most of the window. Reading the set when the file is recorded rather than
/// when it was queued is what makes that work.
#[test]
fn enqueue_a_join_after_dequeue_still_lands_on_the_same_request() {
    let queue = TrackAcquisitionQueue::new();
    enqueue(&queue, "1", true, Some("alice"));
    let request = dequeue(&queue);

    enqueue(&queue, "1", true, Some("bob"));

    assert_eq!(request.requested_by(), ["alice", "bob"]);
}

#[test]
fn enqueue_the_same_user_twice_is_recorded_once() {
    let queue = TrackAcquisitionQueue::new();
    enqueue(&queue, "1", true, Some("Alice"));
    enqueue(&queue, "1", true, Some("alice"));

    assert_eq!(dequeue(&queue).requested_by(), ["Alice"]);
}

/// Octo starts acquisitions of its own, and the setting can be off. Either way nothing is
/// captured, and the history entry has to leave the field out rather than write an empty list
/// that reads as "requested by nobody in particular".
#[test]
fn enqueue_with_no_user_records_nothing() {
    for username in [None, Some(""), Some("   ")] {
        let queue = TrackAcquisitionQueue::new();
        enqueue(&queue, "1", false, username);

        assert!(dequeue(&queue).requested_by().is_empty(), "{username:?}");
    }
}

/// A separate track keeps its own requesters; the dedup key is the track, not the user.
#[test]
fn enqueue_different_tracks_do_not_share_requesters() {
    let queue = TrackAcquisitionQueue::new();
    enqueue(&queue, "1", true, Some("alice"));
    enqueue(&queue, "2", true, Some("bob"));

    let requests = [dequeue(&queue), dequeue(&queue)];
    let by_id = |id: &str| {
        requests
            .iter()
            .find(|r| r.external_id == id)
            .map(|r| r.requested_by())
            .expect("dequeued")
    };

    assert_eq!(by_id("1"), ["alice"]);
    assert_eq!(by_id("2"), ["bob"]);
}

/// Every entry written before this field existed has to keep loading, which is the whole reason
/// it is nullable rather than an empty list.
#[test]
fn history_entry_written_before_attribution_existed_still_loads() {
    let entry: DownloadHistoryEntry = serde_json::from_str(
        r#"{"Artist":"Portishead","Title":"Glory Box","Path":"/music/a.flac",
            "Format":"FLAC","Source":"Lidarr","SizeBytes":1,"DownloadedAt":"2026-09-01T00:00:00Z"}"#,
    )
    .expect("loads");

    assert_eq!(entry.artist, "Portishead");
    assert_eq!(entry.requested_by, None);
}

#[test]
fn history_entry_round_trips_its_requesters() {
    let json = octo_core::json::to_string(&DownloadHistoryEntry {
        artist: "Portishead".into(),
        title: "Glory Box".into(),
        requested_by: Some(vec!["alice".into(), "bob".into()]),
        ..Default::default()
    });

    let back: DownloadHistoryEntry = serde_json::from_str(&json).expect("reads back");
    assert_eq!(
        back.requested_by,
        Some(vec!["alice".to_string(), "bob".to_string()])
    );
}
