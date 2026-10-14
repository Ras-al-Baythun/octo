//! The star queue's turns per person, and whose library a request is for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use octo_core::settings::DownloadSource;
use tokio_util::sync::CancellationToken;

use super::*;

fn star(queue: &TrackAcquisitionQueue, id: &str, user: &str) -> Completion {
    queue.enqueue(
        "deezer",
        id,
        true,
        false,
        true,
        None,
        true,
        Some(user),
        false,
        None,
    )
}

async fn order(queue: &TrackAcquisitionQueue, count: usize) -> Vec<String> {
    let token = CancellationToken::new();
    let mut taken = Vec::new();
    for _ in 0..count {
        let request = queue.dequeue(&token).await.expect("a request");
        taken.push(request.external_id.clone());
    }
    taken
}

/// A queue whose stars take turns per person, as while Family is on.
fn fair_queue() -> TrackAcquisitionQueue {
    TrackAcquisitionQueue::new().with_fair_turns(Arc::new(|| true))
}

#[tokio::test]
async fn one_member_cannot_starve_another() {
    let queue = fair_queue();
    for n in 1..=5 {
        star(&queue, &format!("a{n}"), "alex");
    }
    star(&queue, "s1", "sam");
    star(&queue, "w1", "Winters");
    assert_eq!(order(&queue, 7).await, ["a1", "s1", "w1", "a2", "a3", "a4", "a5"]);
}

#[tokio::test]
async fn single_requester_order_unchanged() {
    let queue = TrackAcquisitionQueue::new();
    for n in 1..=5 {
        star(&queue, &format!("a{n}"), "alex");
    }
    assert_eq!(order(&queue, 5).await, ["a1", "a2", "a3", "a4", "a5"]);

    // Nobody named is one person too.
    let unnamed = TrackAcquisitionQueue::new();
    for n in 1..=3 {
        unnamed.enqueue(
            "deezer",
            &format!("n{n}"),
            true,
            false,
            true,
            None,
            true,
            None,
            false,
            None,
        );
    }
    assert_eq!(order(&unnamed, 3).await, ["n1", "n2", "n3"]);
}

#[tokio::test]
async fn stars_still_before_plays() {
    let queue = TrackAcquisitionQueue::new();
    queue
        .try_enqueue_play("deezer", "played", DownloadSource::YouTube, Some("alex"), None)
        .expect("queued");
    star(&queue, "s1", "sam");
    star(&queue, "a1", "alex");
    assert_eq!(order(&queue, 3).await, ["s1", "a1", "played"]);
}

#[tokio::test]
async fn a_star_that_arrives_while_the_worker_waits_is_taken() {
    let queue = Arc::new(TrackAcquisitionQueue::new());
    let waiting = {
        let queue = Arc::clone(&queue);
        tokio::spawn(async move { queue.dequeue(&CancellationToken::new()).await })
    };
    tokio::task::yield_now().await;
    star(&queue, "late", "alex");
    let taken = waiting.await.expect("joins").expect("a request");
    assert_eq!(taken.external_id, "late");
}

#[test]
fn members_share_one_download_in_flight() {
    let queue = TrackAcquisitionQueue::new();
    let alex = AcquisitionTarget::Member("alex".into());
    let sam = AcquisitionTarget::Member("sam".into());
    let first = queue.enqueue_for(
        &alex,
        false,
        "deezer",
        "1",
        true,
        false,
        true,
        None,
        true,
        Some("alex"),
        false,
        None,
    );
    let second = queue.enqueue_for(
        &sam,
        false,
        "deezer",
        "1",
        true,
        false,
        true,
        None,
        true,
        Some("sam"),
        false,
        None,
    );
    assert!(Arc::ptr_eq(&first.outcome, &second.outcome), "one job");
    let request = queue
        .in_flight
        .lock()
        .get("deezer:1")
        .cloned()
        .expect("in flight");
    assert_eq!(
        request.pool_destination(),
        Some(vec!["alex".to_string(), "sam".to_string()])
    );
    assert_eq!(request.first_requester().as_deref(), Some("alex"));
}

#[test]
fn anyone_who_wants_it_shared_makes_it_shared() {
    let queue = TrackAcquisitionQueue::new();
    let alex = AcquisitionTarget::Member("alex".into());
    queue.enqueue_for(
        &alex,
        false,
        "deezer",
        "1",
        true,
        false,
        true,
        None,
        true,
        Some("alex"),
        false,
        None,
    );
    star(&queue, "1", "winters");
    let request = queue
        .in_flight
        .lock()
        .get("deezer:1")
        .cloned()
        .expect("in flight");
    assert!(request.wants_shared());
    assert_eq!(request.pool_destination(), None);
    assert_eq!(request.pool_members(), ["alex"]);
}

#[test]
fn every_request_is_shared_unless_a_member_asks() {
    let request = AcquisitionRequest::new("deezer", "1", true, false, true, None, true, None);
    assert_eq!(request.place(), AcquisitionPlace::default());
    let queue = TrackAcquisitionQueue::new();
    star(&queue, "2", "winters");
    let shared = queue
        .in_flight
        .lock()
        .get("deezer:2")
        .cloned()
        .expect("in flight");
    assert_eq!(shared.place().pool_members, None);
}

#[test]
fn a_real_flac_ask_is_its_own_job() {
    let queue = TrackAcquisitionQueue::new();
    let alex = AcquisitionTarget::Member("alex".into());
    let any = star(&queue, "1", "alex");
    let flac = queue.enqueue_for(
        &alex,
        true,
        "deezer",
        "1",
        true,
        false,
        true,
        Some(DownloadSource::YouTube),
        true,
        Some("alex"),
        false,
        None,
    );
    assert!(!Arc::ptr_eq(&any.outcome, &flac.outcome));
    let request = queue
        .in_flight
        .lock()
        .get("deezer:1:flac")
        .cloned()
        .expect("in flight");
    assert!(request.place().flac_only);
}

#[tokio::test]
async fn family_off_stars_keep_arrival_order() {
    let queue = TrackAcquisitionQueue::new();
    for n in 1..=3 {
        star(&queue, &format!("a{n}"), "alice");
    }
    star(&queue, "b1", "bob");
    assert_eq!(order(&queue, 4).await, ["a1", "a2", "a3", "b1"]);
}

#[tokio::test]
async fn turning_family_on_or_off_changes_the_order_of_stars_already_waiting() {
    let fair = Arc::new(AtomicBool::new(false));
    let switch = Arc::clone(&fair);
    let queue = TrackAcquisitionQueue::new().with_fair_turns(Arc::new(move || switch.load(Ordering::SeqCst)));
    for n in 1..=3 {
        star(&queue, &format!("a{n}"), "alice");
    }
    star(&queue, "b1", "bob");
    star(&queue, "a4", "alice");
    assert_eq!(order(&queue, 1).await, ["a1"]);
    fair.store(true, Ordering::SeqCst);
    assert_eq!(order(&queue, 2).await, ["a2", "b1"]);
    fair.store(false, Ordering::SeqCst);
    assert_eq!(order(&queue, 2).await, ["a3", "a4"]);
}

#[tokio::test]
async fn stars_lined_up_count_toward_the_bound() {
    let queue = TrackAcquisitionQueue::new();
    let token = CancellationToken::new();
    for n in 0..TrackAcquisitionQueue::STAR_CAPACITY {
        star(&queue, &format!("s{n}"), "alice");
    }
    // One taken: the rest move from the channel into the lines, where they still count.
    let first = queue.dequeue(&token).await.expect("a request");
    queue.release(&first);
    star(&queue, "room-for-one", "bob");
    let over = star(&queue, "one-too-many", "bob");
    assert!(over.is_completed(), "refused at once");
    assert!(over.wait().await.is_err());
    let mut taken = 0;
    while let Ok(Some(request)) =
        tokio::time::timeout(std::time::Duration::from_millis(50), queue.dequeue(&token)).await
    {
        assert_ne!(request.external_id, "one-too-many");
        taken += 1;
    }
    assert_eq!(taken, TrackAcquisitionQueue::STAR_CAPACITY);
}
