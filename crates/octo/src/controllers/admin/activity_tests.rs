//! The dashboard's progress toasts read every job through one shape. Each job's running, just
//! finished and long finished states, and the gaps a page used to miss (a run accepted but not
//! yet Running). Then the views of Find songs, the Sharing
//! card, the imports and Library health, and the endpoint itself.

use axum::http::{Method, StatusCode};
use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use octo_core::lyrics::{LyricsLibraryMode, LyricsLibraryRun, LyricsLibraryStatus};
use octo_core::settings::AppSettings;

use super::*;
use crate::app::AppState;
use crate::controllers::admin::test_app::{app, send};
use crate::services::common::acquisition_tracker::{AcquisitionSnapshot, AcquisitionState};
use crate::services::cover_art::{CoverUpgradeMode, CoverUpgradeRun, CoverUpgradeStatus};
use crate::services::library::duplicate_scan_worker::DuplicateScanResult;
use crate::services::library::upgrade_queue::{UpgradeJob, UpgradeStates};
use crate::services::metadata::genre_backfill_state::{GenreBackfillRun, GenreBackfillStatus};

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 5, 12, 0, 0).unwrap()
}

#[test]
fn genre_accepted_but_not_running_is_running() {
    let old = GenreBackfillRun {
        run_id: "old".into(),
        status: GenreBackfillStatus::Completed,
        finished_utc: Some(now() - TimeDelta::hours(3)),
        ..Default::default()
    };
    let item = genre(&old, true, now()).expect("an item");
    assert_eq!(item.state, "running");
    assert_eq!(item.page, "tags");
    assert_eq!(item.detail.as_deref(), Some("Reading your library"));
}

#[test]
fn genre_running_counts_songs() {
    let run = GenreBackfillRun {
        run_id: "r1".into(),
        status: GenreBackfillStatus::Running,
        dry_run: false,
        processed: 120,
        total: 840,
        changed: 7,
        ..Default::default()
    };
    let item = genre(&run, false, now()).expect("an item");
    assert_eq!(
        (item.id.as_str(), item.state, item.done, item.total, item.unit),
        ("genre:r1", "running", Some(120), Some(840), Some("songs"))
    );
    assert_eq!(item.title, "Re-tagging genres");
    assert_eq!(item.detail.as_deref(), Some("7 changed so far"));
}

#[test]
fn genre_just_finished_says_how_it_ended_long_finished_is_gone() {
    let done = GenreBackfillRun {
        run_id: "r1".into(),
        status: GenreBackfillStatus::Completed,
        dry_run: true,
        changed: 42,
        finished_utc: Some(now() - TimeDelta::seconds(20)),
        ..Default::default()
    };
    let item = genre(&done, false, now()).expect("an item");
    assert_eq!(
        (item.state, item.detail.as_deref()),
        ("done", Some("42 songs would change"))
    );

    let failed = GenreBackfillRun {
        run_id: "r2".into(),
        status: GenreBackfillStatus::Failed,
        reason: Some("The music folder is read-only.".into()),
        finished_utc: Some(now() - TimeDelta::seconds(5)),
        ..Default::default()
    };
    let item = genre(&failed, false, now()).expect("an item");
    assert_eq!(
        (item.state, item.error.as_deref()),
        ("failed", Some("The music folder is read-only."))
    );

    let cancelled = GenreBackfillRun {
        run_id: "r3".into(),
        status: GenreBackfillStatus::Cancelled,
        finished_utc: Some(now() - TimeDelta::seconds(5)),
        ..Default::default()
    };
    assert_eq!(genre(&cancelled, false, now()).expect("an item").state, "stopped");

    let stale = GenreBackfillRun {
        run_id: "r4".into(),
        status: GenreBackfillStatus::Completed,
        finished_utc: Some(now() - RECENT_WINDOW - TimeDelta::seconds(1)),
        ..Default::default()
    };
    assert_eq!(genre(&stale, false, now()), None);
    assert_eq!(genre(&GenreBackfillRun::default(), false, now()), None);
}

#[test]
fn covers_counts_albums_when_looking_up_folders_when_scanning() {
    let scan = CoverUpgradeRun {
        run_id: "c1".into(),
        status: CoverUpgradeStatus::Running,
        mode: CoverUpgradeMode::Scan,
        processed: 30,
        total: 400,
        ..Default::default()
    };
    let s = covers(&scan, true, now()).expect("an item");
    assert_eq!(
        (s.title.as_str(), s.done, s.total, s.unit),
        ("Looking for soft covers", Some(30), Some(400), Some("folders"))
    );

    let apply = CoverUpgradeRun {
        run_id: "c2".into(),
        status: CoverUpgradeStatus::Running,
        mode: CoverUpgradeMode::Apply,
        albums_done: 3,
        albums_total: 12,
        ..Default::default()
    };
    let a = covers(&apply, true, now()).expect("an item");
    assert_eq!(
        (a.title.as_str(), a.done, a.total, a.unit),
        ("Replacing covers", Some(3), Some(12), Some("albums"))
    );

    let finished = CoverUpgradeRun {
        run_id: "c3".into(),
        status: CoverUpgradeStatus::Completed,
        mode: CoverUpgradeMode::Scan,
        soft: 0,
        finished_utc: Some(now() - TimeDelta::seconds(3)),
        ..Default::default()
    };
    assert_eq!(
        covers(&finished, false, now())
            .expect("an item")
            .detail
            .as_deref(),
        Some("Every cover is sharp")
    );

    let starting = CoverUpgradeRun {
        status: CoverUpgradeStatus::Completed,
        finished_utc: Some(now() - TimeDelta::days(1)),
        ..Default::default()
    };
    let item = covers(&starting, true, now()).expect("an item");
    assert_eq!((item.id.as_str(), item.state), ("covers:pending", "running"));
}

#[test]
fn lyrics_running_and_finished() {
    let run = LyricsLibraryRun {
        run_id: "l1".into(),
        status: LyricsLibraryStatus::Running,
        mode: LyricsLibraryMode::Save,
        processed: 5,
        total: 50,
        written: 4,
        ..Default::default()
    };
    let item = lyrics(&run, true, now()).expect("an item");
    assert_eq!(
        (item.title.as_str(), item.state, item.detail.as_deref()),
        ("Saving lyrics", "running", Some("4 written so far"))
    );

    let done = LyricsLibraryRun {
        run_id: "l1".into(),
        status: LyricsLibraryStatus::Completed,
        mode: LyricsLibraryMode::Save,
        written: 41,
        finished_utc: Some(now() - TimeDelta::seconds(1)),
        ..Default::default()
    };
    assert_eq!(
        lyrics(&done, false, now()).expect("an item").detail.as_deref(),
        Some("41 songs got lyrics")
    );
}

#[test]
fn upgrades_one_item_for_the_queue() {
    let jobs = [
        UpgradeJob {
            title: Some("505".into()),
            artist: Some("Arctic Monkeys".into()),
            state: UpgradeStates::WORKING.into(),
            queued_utc: now() - TimeDelta::minutes(2),
            started_utc: Some(now() - TimeDelta::minutes(1)),
            updated_utc: now(),
            ..Default::default()
        },
        UpgradeJob {
            title: Some("Fluorescent Adolescent".into()),
            artist: Some("Arctic Monkeys".into()),
            state: UpgradeStates::QUEUED.into(),
            queued_utc: now() - TimeDelta::minutes(2),
            updated_utc: now(),
            ..Default::default()
        },
    ];
    let item = upgrades(&jobs, now()).expect("an item");
    assert_eq!((item.id.as_str(), item.state), ("upgrades", "running"));
    assert_eq!(
        item.detail.as_deref(),
        Some("Arctic Monkeys · 505, 1 more waiting")
    );
    // The earliest start, or queue time for a job not started yet.
    assert_eq!(item.started_utc, Some(now() - TimeDelta::minutes(2)));

    let ended = [
        UpgradeJob {
            state: UpgradeStates::UPGRADED.into(),
            updated_utc: now() - TimeDelta::seconds(10),
            ..Default::default()
        },
        UpgradeJob {
            state: UpgradeStates::FAILED.into(),
            updated_utc: now() - TimeDelta::seconds(5),
            ..Default::default()
        },
    ];
    let finished = upgrades(&ended, now()).expect("an item");
    assert_eq!(
        (finished.state, finished.detail.as_deref()),
        ("done", Some("1 upgraded, 1 failed"))
    );
    assert_eq!(upgrades(&[], now()), None);
}

fn a(
    title: &str,
    state: AcquisitionState,
    progress: Option<f64>,
    updated: Option<DateTime<Utc>>,
    error: Option<&str>,
) -> AcquisitionSnapshot {
    AcquisitionSnapshot {
        id: format!("id-{title}"),
        provider: "deezer".into(),
        external_id: title.into(),
        artist: Some("Frank Ocean".into()),
        title: Some(title.into()),
        album: Some("Blonde".into()),
        source: Some("Lidarr".into()),
        state,
        progress,
        started_at: now() - TimeDelta::minutes(1),
        updated_at: updated.unwrap_or_else(now),
        error: error.map(str::to_string),
        ..Default::default()
    }
}

#[test]
fn downloads_one_item_while_moving_then_what_landed() {
    let moving = downloads(
        &[
            a("Nights", AcquisitionState::Queued, None, None, None),
            a(
                "Pink + White",
                AcquisitionState::Downloading,
                Some(0.64),
                None,
                None,
            ),
        ],
        now(),
    )
    .expect("an item");
    assert_eq!(
        (moving.id.as_str(), moving.title.as_str(), moving.fraction),
        ("downloads", "Fetching 2 songs", Some(0.64))
    );
    assert_eq!(
        moving.detail.as_deref(),
        Some("Frank Ocean · Pink + White · downloading")
    );

    let one = downloads(
        &[a(
            "Pink + White",
            AcquisitionState::Done,
            None,
            Some(now() - TimeDelta::seconds(4)),
            None,
        )],
        now(),
    )
    .expect("an item");
    assert_eq!(
        (one.state, one.title.as_str(), one.detail.as_deref()),
        ("done", "Added Pink + White", Some("Frank Ocean"))
    );

    let failed = downloads(
        &[a(
            "Nights",
            AcquisitionState::Failed,
            None,
            Some(now() - TimeDelta::seconds(4)),
            Some("No copy that fits was found."),
        )],
        now(),
    )
    .expect("an item");
    assert_eq!(
        (failed.state, failed.error.as_deref()),
        ("failed", Some("No copy that fits was found."))
    );

    assert_eq!(
        downloads(
            &[a(
                "Old",
                AcquisitionState::Done,
                None,
                Some(now() - TimeDelta::hours(1)),
                None
            )],
            now()
        ),
        None
    );
}

// ---- The job views and the endpoint ------------------------------------------------------

fn look(state: &str, copies: usize, sources: &[(&str, &str)], updated: DateTime<Utc>) -> FindLook {
    FindLook {
        id: "f1".into(),
        state: state.into(),
        target_title: "Nights".into(),
        copies,
        sources: sources
            .iter()
            .map(|(name, state)| (name.to_string(), state.to_string()))
            .collect(),
        started_at: now() - TimeDelta::seconds(30),
        updated_at: updated,
        error: (state == "failed").then(|| "Lidarr is not answering.".to_string()),
    }
}

#[test]
fn find_running_then_found_then_gone() {
    let asking = find(
        &[look(
            "searching",
            0,
            &[
                ("YouTube", "searching"),
                ("Lidarr", "searching"),
                ("Deezer", "off"),
            ],
            now(),
        )],
        now(),
    )
    .expect("an item");
    assert_eq!(
        (asking.id.as_str(), asking.title.as_str(), asking.state),
        ("find:f1", "Finding copies of Nights", "running")
    );
    assert_eq!(asking.detail.as_deref(), Some("Asking YouTube and Lidarr"));

    let some = find(&[look("searching", 1, &[("Lidarr", "searching")], now())], now()).expect("an item");
    assert_eq!(some.detail.as_deref(), Some("1 copy so far"));

    let done = find(&[look("done", 3, &[], now() - TimeDelta::seconds(5))], now()).expect("an item");
    assert_eq!(
        (done.title.as_str(), done.state, done.detail.as_deref()),
        (
            "Looked for copies of Nights",
            "done",
            Some("3 copies found. Pick one on Fetched songs")
        )
    );
    let none = find(&[look("done", 0, &[], now())], now()).expect("an item");
    assert_eq!(none.detail.as_deref(), Some("No copies were found"));

    let failed = find(&[look("failed", 0, &[], now())], now()).expect("an item");
    assert_eq!(
        (failed.state, failed.error.as_deref()),
        ("failed", Some("Lidarr is not answering."))
    );

    assert_eq!(
        find(&[look("done", 3, &[], now() - TimeDelta::minutes(3))], now()),
        None
    );
}

#[test]
fn imports_reading_and_trickle() {
    let reading = imports(
        &ImportsActivity {
            reading_busy: true,
            reading_step: Some("Reading Liked Songs".into()),
            trickle_downloading: 2,
            trickle_done: 1200,
            trickle_queued: 30,
            ..Default::default()
        },
        now(),
    );
    assert_eq!(reading.len(), 2);
    assert_eq!(
        (reading[0].id.as_str(), reading[0].detail.as_deref()),
        ("imports:read", Some("Reading Liked Songs"))
    );
    assert_eq!(reading[1].detail.as_deref(), Some("1,200 done, 30 to go"));

    let at = now() - TimeDelta::seconds(10);
    let failed = imports(
        &ImportsActivity {
            reading_error: Some("Spotify said no.".into()),
            reading_finished_utc: Some(at),
            ..Default::default()
        },
        now(),
    );
    assert_eq!(failed[0].state, "failed");
    assert_eq!(failed[0].id, format!("imports:read:{}", dotnet_ticks(at)));

    let read = imports(
        &ImportsActivity {
            reading_finished_utc: Some(at),
            ..Default::default()
        },
        now(),
    );
    assert_eq!(
        (read[0].title.as_str(), read[0].state),
        ("Read your Spotify lists", "done")
    );
    assert!(imports(&ImportsActivity::default(), now()).is_empty());
}

#[test]
fn health_check_and_runs() {
    let run = HealthActivity {
        id: "h1".into(),
        kind: "lookup".into(),
        label: "Looking up tags".into(),
        total: 10,
        done: 8,
        state: "done".into(),
        started_utc: now() - TimeDelta::minutes(1),
        finished_utc: Some(now() - TimeDelta::seconds(5)),
        current: None,
        found: 6,
        outcome_summary: None,
        error: None,
    };
    let items = health(Some(&run), Some(now() - TimeDelta::seconds(2)), now());
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].id, "health:check");
    assert_eq!(
        (items[1].unit, items[1].detail.as_deref()),
        (Some("songs"), Some("Found tags for 6 of 8 songs"))
    );

    let fix = HealthActivity {
        kind: "fix".into(),
        state: "running".into(),
        current: Some("Fixing a title".into()),
        finished_utc: None,
        ..run.clone()
    };
    let items = health(Some(&fix), None, now());
    assert_eq!(
        (items[0].state, items[0].unit, items[0].detail.as_deref()),
        ("running", Some("changes"), Some("Fixing a title"))
    );

    let old = HealthActivity {
        finished_utc: Some(now() - TimeDelta::hours(1)),
        ..run
    };
    assert!(health(Some(&old), None, now()).is_empty());
}

#[test]
fn duplicates_running_then_its_result() {
    let starting = duplicates(
        &DuplicateScan {
            requested: true,
            ..Default::default()
        },
        now(),
    )
    .expect("an item");
    assert_eq!(
        (starting.state, starting.detail.as_deref()),
        ("running", Some("Starting"))
    );

    let at = now() - TimeDelta::seconds(30);
    let part = duplicates(
        &DuplicateScan {
            last: Some(DuplicateScanResult {
                at_utc: at,
                tracks: 5000,
                groups: 1200,
                added: 3,
                complete: false,
            }),
            ..Default::default()
        },
        now(),
    )
    .expect("an item");
    assert_eq!(part.id, format!("duplicates:{}", dotnet_ticks(at)));
    assert_eq!(
        part.detail.as_deref(),
        Some("1,200 sets of duplicates, 3 new (part of the library)")
    );
    assert_eq!(duplicates(&DuplicateScan::default(), now()), None);
}

/// The answer's members, nulls written, in a fixed order.
#[test]
fn an_item_keeps_its_wire_shape() {
    let item = ActivityItem::new("covers:c1", "covers", "covers", "Looking for soft covers", "done")
        .counted(3, 3, "folders")
        .detail(Some("3 albums have soft covers".into()))
        .started(Some(now()))
        .finished(Some(now()));
    assert_eq!(
        octo_core::json::to_string(&item),
        r#"{"id":"covers:c1","kind":"covers","page":"covers","title":"Looking for soft covers","state":"done","done":3,"total":3,"unit":"folders","fraction":null,"detail":"3 albums have soft covers","startedUtc":"2026-10-05T12:00:00Z","finishedUtc":"2026-10-05T12:00:00Z","error":null}"#
    );
}

/// With the sign-in off (the test default) a browser with no session is answered, as not signed
/// in; with a session, or the recovery code, the jobs are listed and `signedIn` says so.
#[tokio::test]
async fn the_endpoint_lists_jobs_for_a_signed_in_browser() {
    let state = AppState::for_tests(AppSettings::default());
    let app = app(state.clone());

    let out = send(&app, Method::GET, "/api/admin/activity", &[], None).await;
    assert_eq!(out.status, StatusCode::OK);
    assert_eq!(out.text(), r#"{"items":[],"signedIn":false}"#);

    let token = state.browse_sessions.create("admin");
    let signed = send(
        &app,
        Method::GET,
        "/api/admin/activity",
        &[("X-Octo-Browse-Token", &token)],
        None,
    )
    .await;
    assert_eq!(signed.status, StatusCode::OK, "{}", signed.text());
    assert_eq!(signed.json(), serde_json::json!({"items": [], "signedIn": true}));

    let recovery = state.browse_sessions.create_recovery();
    let signed = send(
        &app,
        Method::GET,
        "/api/admin/activity",
        &[("X-Octo-Browse-Token", &recovery)],
        None,
    )
    .await;
    assert_eq!(signed.json()["signedIn"], true);
}
