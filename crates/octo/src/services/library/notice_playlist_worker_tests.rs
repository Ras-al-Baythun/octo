//! The reconcile decision for one person's notice playlist, driven directly. And what goes back
//! to AcoustID when a person keeps a track.

use std::sync::atomic::{AtomicI32, Ordering};

use chrono::{TimeDelta, Utc};

use super::*;

static ORDER: AtomicI32 = AtomicI32::new(0);

fn entry(id: &str, state: NoticeState, group: Option<&str>, minutes_ago: i64) -> NoticeEntry {
    NoticeEntry {
        key: format!("k-{id}"),
        username: "alice".into(),
        navidrome_id: Some(id.to_string()),
        state,
        group_key: group.map(str::to_string),
        order: ORDER.fetch_add(1, Ordering::SeqCst) + 1,
        created_utc: Utc::now() - TimeDelta::minutes(minutes_ago),
        ..Default::default()
    }
}

fn present(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|id| id.to_string()).collect()
}

fn added(plan: &NoticePlan) -> Vec<&str> {
    plan.add
        .iter()
        .filter_map(|e| e.navidrome_id.as_deref())
        .collect()
}

#[test]
fn plan_a_queued_track_gone_from_the_playlist_is_a_dismissal() {
    let plan = NoticeReconcile::plan(
        &[
            entry("a", NoticeState::Queued, None, 0),
            entry("b", NoticeState::Queued, None, 0),
        ],
        &present(&["b"]),
        10,
    );

    assert_eq!(plan.dismiss, ["k-a"]);
    assert!(plan.remove.is_empty());
    assert!(plan.add.is_empty());
}

/// A restart between adding a track and recording it must not add it twice.
#[test]
fn plan_a_waiting_track_already_in_the_playlist_is_adopted_not_added_again() {
    let plan = NoticeReconcile::plan(&[entry("a", NoticeState::Waiting, None, 0)], &present(&["a"]), 10);

    assert_eq!(plan.adopt, ["k-a"]);
    assert!(plan.add.is_empty());
}

#[test]
fn plan_a_settled_track_still_listed_is_taken_off() {
    for state in [
        NoticeState::Kept,
        NoticeState::Acted,
        NoticeState::Expired,
        NoticeState::Dismissed,
    ] {
        let plan = NoticeReconcile::plan(&[entry("a", state, None, 0)], &present(&["a"]), 10);

        assert_eq!(plan.remove, ["a"], "{state:?}");
        assert!(plan.dismiss.is_empty(), "{state:?}");
    }
}

#[test]
fn plan_fills_only_the_room_left_oldest_first() {
    let entries = [
        entry("asked", NoticeState::Queued, None, 0),
        entry("new", NoticeState::Waiting, None, 1),
        entry("old", NoticeState::Waiting, None, 30),
        entry("older", NoticeState::Waiting, None, 60),
    ];

    let plan = NoticeReconcile::plan(&entries, &present(&["asked"]), 3);

    assert_eq!(added(&plan), ["older", "old"]);
}

#[test]
fn plan_a_full_playlist_adds_nothing() {
    let plan = NoticeReconcile::plan(
        &[
            entry("asked", NoticeState::Queued, None, 0),
            entry("new", NoticeState::Waiting, None, 0),
        ],
        &present(&["asked"]),
        1,
    );

    assert!(plan.add.is_empty());
}

#[test]
fn plan_a_waiting_track_without_a_navidrome_id_waits() {
    let waiting = NoticeEntry {
        navidrome_id: None,
        ..entry("a", NoticeState::Waiting, None, 0)
    };

    assert!(
        NoticeReconcile::plan(&[waiting], &present(&[]), 10)
            .add
            .is_empty()
    );
}

/// A duplicate pair only makes sense together: it goes in whole or waits.
#[test]
fn plan_a_group_that_does_not_fit_waits_while_a_single_track_takes_the_room() {
    let entries = [
        entry("a1", NoticeState::Waiting, Some("g"), 60),
        entry("a2", NoticeState::Waiting, Some("g"), 60),
        entry("single", NoticeState::Waiting, None, 1),
    ];

    let plan = NoticeReconcile::plan(&entries, &present(&[]), 1);

    assert_eq!(added(&plan), ["single"]);
}

#[test]
fn plan_a_group_that_fits_goes_in_whole_in_its_order() {
    let entries = [
        entry("a1", NoticeState::Waiting, Some("g"), 0),
        entry("a2", NoticeState::Waiting, Some("g"), 0),
    ];

    let plan = NoticeReconcile::plan(&entries, &present(&[]), 2);

    assert_eq!(added(&plan), ["a1", "a2"]);
}

// ---- What goes back to AcoustID ---------------------------------------------------------

#[test]
fn may_submit_needs_consent_both_keys_and_a_real_run() {
    let cases = [
        (true, false, "app", "user", true),
        (false, false, "app", "user", false),
        (true, true, "app", "user", false),
        (true, false, "", "user", false),
        (true, false, "app", "", false),
    ];
    for (consent, dry_run, app_key, user_key, expected) in cases {
        let settings = LibraryActionSettings {
            dry_run,
            ..Default::default()
        };
        let downloads = DownloadSettings {
            submit_confirmed_fingerprints: consent,
            acoust_id_api_key: app_key.into(),
            acoust_id_user_api_key: user_key.into(),
            ..Default::default()
        };
        assert_eq!(
            NoticePlaylistWorker::may_submit(&settings, &downloads),
            expected,
            "{consent} {dry_run} {app_key:?} {user_key:?}"
        );
    }
}

/// A track AcoustID confidently named as something else was kept for some reason other than
/// "AcoustID is missing this", and a shortened fingerprint must never land beside the standard
/// ones.
#[test]
fn submittable_only_what_acoust_id_could_not_place_at_the_standard_length() {
    let cases = [
        (InconclusiveReason::NoEntry, 120, true),
        (InconclusiveReason::BelowThreshold, 120, true),
        (InconclusiveReason::SourceDisagreed, 120, false),
        (InconclusiveReason::NoEntry, 60, false),
    ];
    for (cause, seconds, expected) in cases {
        let entry = NoticeEntry {
            cause,
            fingerprint: Some("AQADtEqk".into()),
            duration_seconds: 330,
            ..Default::default()
        };
        let downloads = DownloadSettings {
            fingerprint_seconds: seconds,
            ..Default::default()
        };
        assert_eq!(
            NoticePlaylistWorker::submittable(&entry, &downloads),
            expected,
            "{cause:?} {seconds}"
        );
    }
}

#[test]
fn submittable_without_a_length_or_a_fingerprint_is_not() {
    let downloads = DownloadSettings {
        fingerprint_seconds: 120,
        ..Default::default()
    };

    assert!(!NoticePlaylistWorker::submittable(
        &NoticeEntry {
            cause: InconclusiveReason::NoEntry,
            fingerprint: Some("AQADtEqk".into()),
            ..Default::default()
        },
        &downloads
    ));
    assert!(!NoticePlaylistWorker::submittable(
        &NoticeEntry {
            cause: InconclusiveReason::NoEntry,
            duration_seconds: 330,
            ..Default::default()
        },
        &downloads
    ));
}
