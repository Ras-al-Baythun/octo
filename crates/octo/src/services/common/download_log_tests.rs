//! The download log: the downloads drawer's server half. Each row's log (what was searched, what
//! was found, the copy chosen and why, the checks, the tags, the cover and the lyrics), clearing
//! finished rows, Find songs with its reasons, and a picked copy reaching the pipeline exactly
//! once.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use futures::FutureExt;
use octo_core::common::Clock;
use octo_core::lidarr::{LidarrRelease, LidarrReleasePick};
use octo_core::lyrics::{LyricsResult, SongLyricsPlace};
use octo_core::models::download::AcquisitionCandidate;
use octo_core::models::download::acquisition_event::quality_text;
use octo_core::settings::DownloadSource;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::acquisition_tracker::{
    AcquisitionState, AcquisitionTracker, acquisition_event_kinds, acquisition_kinds,
};
use super::download_picks::{DownloadPicks, PickedCopy};
use super::song_finder::{
    FindTarget, FoundCopy, PickOutcome, SearchAnswer, SongFinder, SongFinderServices, find_states,
};
use crate::services::library::LibraryActionState;
use crate::services::library::library_action_executor::{
    LibraryActionCodes, LibraryActionOutcome, LibraryActionRequest,
};
use crate::services::library::upgrade_queue::{UpgradeAsk, UpgradeQueue, UpgradeWorker, UpgradeWorkerSeams};
use crate::services::lyrics::lyrics_sidecar_writer::{
    LyricsJob, LyricsSidecarWriter, LyricsWrite, LyricsWriteOutcome,
};

/// A clock the test moves by hand.
#[derive(Clone)]
pub(crate) struct ManualClock(Arc<Mutex<DateTime<Utc>>>);

impl ManualClock {
    pub(crate) fn new() -> ManualClock {
        ManualClock(Arc::new(Mutex::new(
            Utc.with_ymd_and_hms(2026, 10, 4, 18, 0, 0).unwrap(),
        )))
    }

    pub(crate) fn advance(&self, by: chrono::TimeDelta) {
        *self.0.lock() += by;
    }

    pub(crate) fn clock(&self) -> Clock {
        let now = self.0.clone();
        Clock::new(move || *now.lock())
    }
}

fn new_tracker() -> Arc<AcquisitionTracker> {
    Arc::new(AcquisitionTracker::new(None, Clock::system()))
}

fn kinds(tracker: &AcquisitionTracker, user: &str) -> Vec<String> {
    log(tracker, user).iter().map(|line| line.kind.clone()).collect()
}

fn log(tracker: &AcquisitionTracker, user: &str) -> Vec<octo_core::models::download::AcquisitionEvent> {
    tracker
        .detail("outside:abc", Some(user))
        .expect("the row")
        .events
        .expect("its log")
}

fn begin(tracker: &AcquisitionTracker, id: &str, user: &str) {
    tracker.begin("outside", id, Some(id), Some(user), None, None, None);
}

fn candidate(source: &str) -> AcquisitionCandidate {
    AcquisitionCandidate {
        source: source.to_string(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------
// The log
// ---------------------------------------------------------------------------------------

#[test]
fn a_download_logs_each_step_once_in_order() {
    let tracker = new_tracker();
    tracker.begin(
        "outside",
        "abc",
        Some("abc"),
        Some("alice"),
        Some("Daft Punk"),
        Some("Da Funk"),
        Some("Homework"),
    );
    tracker.stage(
        "outside",
        "abc",
        AcquisitionState::Searching,
        Some("Lidarr"),
        None,
    );
    tracker.stage(
        "outside",
        "abc",
        AcquisitionState::Searching,
        Some("Lidarr"),
        None,
    );
    tracker.found(
        "outside",
        "abc",
        "1 copy fits, best first",
        vec![AcquisitionCandidate {
            file: Some("Da Funk.flac".into()),
            format: Some("flac".into()),
            rank: Some(1),
            ..candidate("Lidarr")
        }],
        None,
    );
    tracker.trying(
        "outside",
        "abc",
        AcquisitionCandidate {
            file: Some("Da Funk.flac".into()),
            format: Some("flac".into()),
            bit_depth: Some(16),
            sample_rate: Some(44100),
            ..candidate("Lidarr")
        },
        "Trying the only copy that fits",
        Some("lossless"),
    );
    tracker.transfer("outside", "abc", Some(10), Some(2_000_000), None, Some("Lidarr"));
    tracker.transfer(
        "outside",
        "abc",
        Some(1_000_000),
        Some(2_000_000),
        None,
        Some("Lidarr"),
    );
    tracker.stage("outside", "abc", AcquisitionState::Verifying, None, None);
    tracker.stage("outside", "abc", AcquisitionState::Importing, None, None);
    tracker.imported(
        "outside",
        "abc",
        Some("Daft Punk"),
        Some("Da Funk"),
        Some("/music/Da Funk.flac"),
    );

    let lines = log(&tracker, "alice");
    assert_eq!(
        lines.iter().map(|l| l.kind.as_str()).collect::<Vec<_>>(),
        [
            "queued", "search", "found", "try", "transfer", "check", "tags", "library", "done"
        ]
    );
    assert_eq!(lines[4].text, "Downloading from Lidarr");
    assert_eq!(lines.last().expect("a line").text, "In your library");
    let row = tracker.detail("outside:abc", Some("alice")).expect("the row");
    assert_eq!(row.quality.as_deref(), Some("FLAC 16-bit 44.1 kHz"));
}

#[test]
fn a_note_is_logged_only_when_it_changes_and_a_failure_says_why() {
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");
    for _ in 0..2 {
        tracker.stage(
            "outside",
            "abc",
            AcquisitionState::Searching,
            Some("YouTube"),
            Some("Lidarr couldn't get it, trying YouTube"),
        );
    }
    tracker.fail(
        "outside",
        "abc",
        Some("No YouTube match for that song. More words for the log at /music/x."),
    );

    let lines = log(&tracker, "alice");
    assert_eq!(kinds(&tracker, "alice"), ["queued", "note", "search", "failed"]);
    assert_eq!(
        lines.last().expect("a line").detail.as_deref(),
        Some("No YouTube match for that song")
    );
}

#[test]
fn lines_may_follow_the_end_but_a_restart_starts_a_fresh_log() {
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");
    tracker.complete("outside", "abc", Some("nd-1"));
    tracker.log(
        "outside",
        "abc",
        acquisition_event_kinds::LYRICS,
        "Synced lyrics from LRCLIB",
        Some("Embedded in the song"),
    );
    assert_eq!(kinds(&tracker, "alice"), ["queued", "done", "lyrics"]);

    tracker.begin_kind(
        "outside",
        "abc",
        Some("abc"),
        Some("alice"),
        None,
        None,
        None,
        acquisition_kinds::PICK,
    );
    let again = tracker.detail("outside:abc", Some("alice")).expect("the row");
    assert_eq!(again.kind, acquisition_kinds::PICK);
    let events = again.events.expect("a log");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].text, "Asked for the copy you picked");
}

#[test]
fn a_heart_joining_a_pick_keeps_it_a_pick() {
    let tracker = new_tracker();
    tracker.begin_kind(
        "outside",
        "abc",
        Some("abc"),
        Some("alice"),
        None,
        None,
        None,
        acquisition_kinds::PICK,
    );
    begin(&tracker, "abc", "bob");
    assert_eq!(
        tracker.detail("outside:abc", None).expect("the row").kind,
        acquisition_kinds::PICK
    );
    assert_eq!(log(&tracker, "alice").len(), 1);
}

#[test]
fn a_long_log_keeps_its_first_line() {
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");
    for i in 0..AcquisitionTracker::MAX_EVENTS + 20 {
        tracker.log(
            "outside",
            "abc",
            acquisition_event_kinds::NOTE,
            &format!("line {i}"),
            None,
        );
    }

    let lines = log(&tracker, "alice");
    assert_eq!(lines.len(), AcquisitionTracker::MAX_EVENTS);
    assert_eq!(lines[0].kind, "queued");
    assert_eq!(
        lines.last().expect("a line").text,
        format!("line {}", AcquisitionTracker::MAX_EVENTS + 19)
    );
}

#[test]
fn the_log_is_only_for_the_rows_owners() {
    let tracker = new_tracker();
    begin(&tracker, "abc", "alice");
    assert!(tracker.detail("outside:abc", Some("ALICE")).is_some());
    assert!(tracker.detail("outside:abc", Some("bob")).is_none());
    assert!(tracker.detail("outside:nope", Some("alice")).is_none());
}

#[test]
fn a_shared_rows_log_names_nobody() {
    let tracker = new_tracker();
    for user in ["alice", "bob"] {
        tracker.begin(
            "outside",
            "abc",
            Some("abc"),
            Some(user),
            Some("Daft Punk"),
            Some("Da Funk"),
            Some("Homework"),
        );
    }

    let lines = log(&tracker, "bob");

    assert!(!lines.iter().any(|line| {
        format!("{}{}", line.text, line.detail.clone().unwrap_or_default())
            .to_lowercase()
            .contains("alice")
    }));
}

#[test]
fn clearing_takes_finished_rows_off_one_persons_list() {
    let tracker = new_tracker();
    begin(&tracker, "shared", "alice");
    begin(&tracker, "shared", "bob");
    tracker.complete("outside", "shared", None);
    begin(&tracker, "mine", "alice");
    tracker.fail("outside", "mine", Some("nope"));
    begin(&tracker, "running", "alice");

    let ids = |user: &str| -> Vec<String> {
        let mut ids: Vec<String> = tracker.for_user(user).into_iter().map(|r| r.id).collect();
        ids.sort();
        ids
    };
    assert_eq!(tracker.clear("alice", Some("outside:mine")), 1);
    assert_eq!(ids("alice"), ["running", "shared"]);
    assert_eq!(tracker.clear("alice", None), 1);
    assert_eq!(ids("alice"), ["running"]);
    // Still bob's.
    assert_eq!(ids("bob"), ["shared"]);
}

#[test]
fn quality_reads_like_a_person() {
    let cases: [(Option<&str>, Option<i32>, Option<i32>, Option<i32>, Option<&str>); 5] = [
        (
            Some("flac"),
            None,
            Some(16),
            Some(44100),
            Some("FLAC 16-bit 44.1 kHz"),
        ),
        (
            Some("flac"),
            None,
            Some(24),
            Some(96000),
            Some("FLAC 24-bit 96 kHz"),
        ),
        (Some(".mp3"), Some(320), None, None, Some("MP3 320 kbps")),
        (Some("ogg"), None, None, None, Some("OGG")),
        (None, Some(320), None, None, None),
    ];
    for (format, bit_rate, depth, rate, expected) in cases {
        assert_eq!(
            quality_text(format, bit_rate, depth, rate).as_deref(),
            expected,
            "{format:?}"
        );
    }
}

fn target(artist: &str, title: &str, album: Option<&str>, duration: Option<i32>) -> FindTarget {
    FindTarget {
        external_id: Some("id".into()),
        ..FindTarget::new(artist, title, album, duration, "id")
    }
}

// ---------------------------------------------------------------------------------------
// Find songs
// ---------------------------------------------------------------------------------------

pub(crate) fn new_finder(target: Option<FindTarget>) -> Arc<SongFinder> {
    let finder = Arc::new(SongFinder::new(SongFinderServices::default()));
    finder.seams(|seams| {
        seams.resolve = Arc::new(move |_, _| {
            let target = target.clone();
            async move { target }.boxed()
        })
    });
    finder
}

pub(crate) fn answer(copies: Vec<FoundCopy>) -> anyhow::Result<SearchAnswer> {
    let text = format!("{} found", copies.len());
    Ok((copies, vec!["query".to_string()], Some(text)))
}

pub(crate) fn copy(source: &str, rank: Option<i32>, file: &str) -> FoundCopy {
    FoundCopy::new(
        AcquisitionCandidate {
            file: Some(file.into()),
            rank,
            format: Some("flac".into()),
            ..candidate(source)
        },
        PickedCopy {
            source: source.into(),
            release_guid: Some(file.into()),
            indexer_id: Some(1),
            release_title: Some(file.into()),
            ..Default::default()
        },
    )
}

fn sources(names: &[&str]) -> super::song_finder::SourcesFn {
    let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    Arc::new(move |_| names.iter().map(|n| (n.clone(), None)).collect())
}

fn searching(
    copies: impl Fn() -> anyhow::Result<SearchAnswer> + Send + Sync + 'static,
) -> super::song_finder::SearchFn {
    let copies = Arc::new(copies);
    Arc::new(move |_, _| {
        let copies = copies.clone();
        async move { copies() }.boxed()
    })
}

fn files(found: &[FoundCopy]) -> Vec<String> {
    found
        .iter()
        .map(|c| c.shown.file.clone().unwrap_or_default())
        .collect()
}

const TEN: Duration = Duration::from_secs(10);

#[tokio::test]
async fn a_look_searches_the_source_that_is_on_and_orders_the_copies() {
    let finder = new_finder(Some(target("Air", "Sexy Boy", Some("Moon Safari"), Some(298))));
    finder.seams(|s| {
        s.sources_for = sources(&["Lidarr"]);
        s.search_lidarr = searching(|| {
            answer(vec![
                copy("Lidarr", None, "other.flac"),
                copy("Lidarr", Some(2), "best.flac"),
                copy("Lidarr", Some(1), "Lidarr's choice"),
            ])
        });
    });

    let started = finder
        .start("id", "alice", &CancellationToken::new())
        .await
        .expect("a look");
    let done = finder.wait(&started.id, TEN).await.expect("the look");

    assert_eq!(done.state, find_states::DONE);
    assert_eq!(
        files(&done.copies),
        ["Lidarr's choice", "best.flac", "other.flac"]
    );
    assert!(done.sources.iter().all(|s| s.state == find_states::DONE));
    assert!(finder.get(&started.id, "bob").is_none());
}

#[tokio::test]
async fn a_source_that_fails_says_so() {
    let finder = new_finder(Some(target("Air", "Sexy Boy", None, None)));
    finder.seams(|s| {
        s.sources_for = sources(&["Lidarr"]);
        s.search_lidarr = searching(|| {
            Err(anyhow::anyhow!(
                "Lidarr is not available on this server. Try later."
            ))
        });
    });

    let id = finder
        .start("id", "alice", &CancellationToken::new())
        .await
        .expect("a look")
        .id;
    let done = finder.wait(&id, TEN).await.expect("the look");

    assert_eq!(done.state, find_states::FAILED);
    let lidarr = done.sources.iter().find(|s| s.name == "Lidarr").expect("Lidarr");
    assert_eq!(lidarr.state, find_states::FAILED);
    assert_eq!(
        lidarr.text.as_deref(),
        Some("Lidarr is not available on this server")
    );
    assert_eq!(
        done.error.as_deref(),
        Some("Lidarr is not available on this server")
    );
}

#[tokio::test]
async fn with_every_source_off_a_look_fails_at_once_and_says_why() {
    let finder = new_finder(Some(target("Air", "Sexy Boy", None, None)));
    finder.seams(|s| {
        s.sources_for = Arc::new(|_| {
            vec![(
                "Lidarr".to_string(),
                Some("Lidarr is not set up on this server".to_string()),
            )]
        });
    });

    let look = finder
        .start("id", "alice", &CancellationToken::new())
        .await
        .expect("a look");

    assert_eq!(look.state, find_states::FAILED);
    assert!(
        look.error
            .as_deref()
            .is_some_and(|e| e.contains("None of your download sources can search"))
    );
}

fn fetching(fetched: Arc<Mutex<Option<FoundCopy>>>) -> super::song_finder::FetchFn {
    Arc::new(move |_, copy, _, _| {
        *fetched.lock() = Some(copy);
        async { Ok(PickOutcome::new("queued", "Getting it.")) }.boxed()
    })
}

#[tokio::test]
async fn a_pick_goes_to_fetch_with_the_copy_at_that_place() {
    let finder = new_finder(Some(target("Air", "Sexy Boy", None, None)));
    let fetched = Arc::new(Mutex::new(None));
    finder.seams(|s| {
        s.sources_for = sources(&["Lidarr"]);
        s.search_lidarr = searching(|| {
            answer(vec![
                copy("Lidarr", Some(1), "one.flac"),
                copy("Lidarr", Some(2), "two.flac"),
            ])
        });
        s.fetch = fetching(fetched.clone());
    });
    let id = finder
        .start("id", "alice", &CancellationToken::new())
        .await
        .expect("a look")
        .id;
    let look = finder.wait(&id, TEN).await.expect("the look");

    assert_eq!(finder.pick_at(&look.id, 5, "alice").await.state, "skipped");
    assert_eq!(finder.pick_at(&look.id, 1, "bob").await.state, "skipped");
    assert_eq!(finder.pick_at(&look.id, 1, "alice").await.state, "queued");
    assert_eq!(
        fetched
            .lock()
            .as_ref()
            .and_then(|c| c.shown.file.clone())
            .as_deref(),
        Some("two.flac")
    );
}

#[tokio::test]
async fn a_pick_waits_for_the_search_to_end_and_copies_keep_their_ids() {
    let finder = new_finder(Some(target("Air", "Sexy Boy", None, None)));
    let fetched = Arc::new(Mutex::new(None));
    let (lidarr_done, lidarr_wait) = tokio::sync::watch::channel(false);
    finder.seams(|s| {
        s.sources_for = sources(&["Lidarr"]);
        s.search_lidarr = Arc::new(move |_, _| {
            let mut wait = lidarr_wait.clone();
            async move {
                let _ = wait.wait_for(|done| *done).await;
                answer(vec![
                    copy("Lidarr", Some(2), "two.flac"),
                    copy("Lidarr", None, "other.flac"),
                    copy("Lidarr", Some(1), "Lidarr's choice"),
                ])
            }
            .boxed()
        });
        s.fetch = fetching(fetched.clone());
    });
    let id = finder
        .start("id", "alice", &CancellationToken::new())
        .await
        .expect("a look")
        .id;
    let early = finder.get(&id, "alice").expect("the look");

    // Lidarr has not answered: a pick now, by place or by id, waits.
    assert_eq!(early.state, find_states::SEARCHING);
    assert!(early.copies.is_empty());
    assert_eq!(
        finder.pick_at(&id, 0, "alice").await.detail,
        SongFinder::STILL_SEARCHING_TEXT
    );
    assert_eq!(
        finder.pick_by_id(&id, "c1", "alice").await.detail,
        SongFinder::STILL_SEARCHING_TEXT
    );
    assert!(fetched.lock().is_none());

    lidarr_done.send(true).expect("sent");
    let done = finder.wait(&id, TEN).await.expect("the look");

    // Lidarr's first choice leads; each copy has an id of its own.
    assert_eq!(files(&done.copies), ["Lidarr's choice", "two.flac", "other.flac"]);
    assert_eq!(
        done.copies.iter().map(|c| c.id.clone()).collect::<Vec<_>>(),
        ["c1", "c2", "c3"]
    );
    assert_eq!(
        finder.pick_by_id(&id, &done.copies[2].id, "alice").await.state,
        "queued"
    );
    assert_eq!(
        fetched
            .lock()
            .as_ref()
            .and_then(|c| c.shown.file.clone())
            .as_deref(),
        Some("other.flac")
    );
    assert_eq!(
        finder.pick_by_id(&id, "c99", "alice").await.detail,
        "That copy is not on the list."
    );
}

fn owned(format: &str) -> FindTarget {
    FindTarget {
        library_id: Some("nd-1".into()),
        owned_format: Some(format.into()),
        ..FindTarget::new("Air", "Sexy Boy", None, None, "nd-1")
    }
}

fn lidarr_copy(format: Option<&str>, release: bool) -> FoundCopy {
    FoundCopy::new(
        AcquisitionCandidate {
            format: format.map(str::to_string),
            ..candidate("Lidarr")
        },
        PickedCopy {
            source: "Lidarr".into(),
            release_guid: release.then(|| "g".to_string()),
            indexer_id: release.then_some(1),
            ..Default::default()
        },
    )
}

#[test]
fn only_a_lossless_copy_may_replace_a_lossy_library_song() {
    let mp3_owned = owned("mp3");
    let flac_owned = owned("flac");
    let flac = lidarr_copy(Some("FLAC"), true);
    let refusal = |target: &FindTarget, copy: &FoundCopy| SongFinder::replace_refusal(target, copy);

    assert_eq!(refusal(&mp3_owned, &flac), None);
    assert_eq!(refusal(&mp3_owned, &lidarr_copy(Some("FLAC 24bit"), true)), None);
    assert_eq!(refusal(&mp3_owned, &lidarr_copy(None, false)), None);
    assert!(refusal(&mp3_owned, &lidarr_copy(Some("MP3-320"), true)).is_some());
    assert_eq!(
        refusal(&flac_owned, &flac).as_deref(),
        Some("Your copy is already lossless, so nothing would be better.")
    );
}

#[test]
fn lidarr_releases_keep_lidarrs_order_and_say_why_one_is_rejected() {
    let target = target("Air", "Sexy Boy", Some("Moon Safari"), Some(298));
    let release =
        |guid: &str, indexer_id: i32, title: &str, indexer: &str, quality: &str, size: i64| LidarrRelease {
            guid: guid.into(),
            indexer_id,
            title: title.into(),
            indexer: Some(indexer.into()),
            quality: Some(quality.into()),
            size: Some(size),
            seeders: None,
            leechers: None,
            protocol: None,
            age_days: None,
            rejected: false,
            rejections: Vec::new(),
        };
    let copies = SongFinder::lidarr_copies(
        &[
            LidarrRelease {
                seeders: Some(12),
                leechers: Some(0),
                protocol: Some("torrent".into()),
                age_days: Some(3),
                ..release(
                    "a",
                    1,
                    "Air - Moon Safari [FLAC]",
                    "Redacted",
                    "FLAC",
                    300_000_000,
                )
            },
            LidarrRelease {
                protocol: Some("usenet".into()),
                age_days: Some(1),
                rejected: true,
                rejections: vec!["Not wanted in profile".into()],
                ..release("b", 2, "Air - Moon Safari MP3", "NZBgeek", "MP3-320", 90_000_000)
            },
        ],
        &target,
        "Moon Safari",
    );

    assert_eq!(copies[0].shown.rank, Some(2));
    assert_eq!(
        copies[0].shown.note.as_deref(),
        Some("torrent, 12 seeders, 3 days old")
    );
    assert_eq!(copies[1].shown.rank, None);
    assert_eq!(copies[1].shown.note.as_deref(), Some("Not wanted in profile"));
    assert_eq!(
        copies[0].pick.release(),
        Some(LidarrReleasePick {
            guid: "a".into(),
            indexer_id: 1,
            title: Some("Air - Moon Safari [FLAC]".into()),
            size: Some(300_000_000),
        })
    );
}

// ---------------------------------------------------------------------------------------
// Picks reach the pipeline once
// ---------------------------------------------------------------------------------------

#[test]
fn a_pick_is_taken_once_and_only_by_its_own_source() {
    let clock = ManualClock::new();
    let picks = DownloadPicks::new(clock.clock());
    picks.pin(
        "abc",
        Arc::new(PickedCopy {
            source: "Lidarr".into(),
            release_guid: Some("g".into()),
            indexer_id: Some(4),
            ..Default::default()
        }),
    );

    assert!(picks.take("abc", "YouTube").is_none());
    assert_eq!(
        picks
            .take("abc", "Lidarr")
            .and_then(|p| p.release_guid.clone())
            .as_deref(),
        Some("g")
    );
    assert!(picks.take("abc", "Lidarr").is_none());

    picks.pin(
        "old",
        Arc::new(PickedCopy {
            source: "Lidarr".into(),
            release_guid: Some("old".into()),
            indexer_id: Some(1),
            ..Default::default()
        }),
    );
    clock.advance(DownloadPicks::KEEP);
    assert!(picks.take("old", "Lidarr").is_none());
}

#[test]
fn a_pick_waiting_in_the_upgrade_queue_outlives_a_restart() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("upgrades.json");
    let pick = PickedCopy {
        source: "Lidarr".into(),
        release_guid: Some("g1".into()),
        indexer_id: Some(7),
        release_title: Some("Air - Moon Safari [FLAC]".into()),
        ..Default::default()
    };
    UpgradeQueue::with_path(Some(path.clone())).add(
        vec![UpgradeAsk {
            pick: Some(pick),
            ..UpgradeAsk::new("nd-1")
        }],
        "alice",
        "find",
    );
    // What is derived from the pick (the release) is never written.
    assert!(
        !std::fs::read_to_string(&path)
            .expect("written")
            .contains("\"Release\":")
    );

    let jobs = UpgradeQueue::with_path(Some(path)).snapshot();
    assert_eq!(jobs.len(), 1);
    let pick = jobs[0].pick.as_ref().expect("the pick");
    assert_eq!(
        pick.release(),
        Some(LidarrReleasePick {
            guid: "g1".into(),
            indexer_id: 7,
            title: Some("Air - Moon Safari [FLAC]".into()),
            size: None,
        })
    );
}

/// A worker whose Better quality is `apply`, with the tracker and the picks given.
pub(crate) fn upgrade_worker(
    queue: &Arc<UpgradeQueue>,
    tracker: Option<Arc<AcquisitionTracker>>,
    picks: Arc<DownloadPicks>,
    apply: impl Fn(LibraryActionRequest) -> LibraryActionOutcome + Send + Sync + 'static,
) -> Arc<UpgradeWorker> {
    let apply = Arc::new(apply);
    let seams = UpgradeWorkerSeams {
        apply: Arc::new(move |request| {
            let outcome = apply(request);
            async move { Ok(outcome) }.boxed()
        }),
        describe: Arc::new(|_| async { None }.boxed()),
        width: Arc::new(|| 1),
        source_name: Arc::new(|| "Lidarr".to_string()),
    };
    Arc::new(UpgradeWorker::with_seams(queue.clone(), seams, tracker, None, None).with_picks(Some(picks)))
}

#[tokio::test]
async fn an_upgrade_with_a_pick_fetches_only_that_copy_and_logs_the_verdict() {
    let queue = Arc::new(UpgradeQueue::new());
    let picks = Arc::new(DownloadPicks::default());
    let tracker = new_tracker();
    let pick = PickedCopy {
        source: "Lidarr".into(),
        release_guid: Some("g9".into()),
        indexer_id: Some(9),
        release_title: Some("Air - Moon Safari [FLAC]".into()),
        ..Default::default()
    };
    queue.add(
        vec![UpgradeAsk {
            title: Some("Sexy Boy".into()),
            artist: Some("Air".into()),
            album: Some("Moon Safari".into()),
            suffix: Some("mp3".into()),
            pick: Some(pick),
            ..UpgradeAsk::new("nd-1")
        }],
        "alice",
        "find",
    );
    let asked: Arc<Mutex<Option<Option<DownloadSource>>>> = Arc::default();
    let taken: Arc<Mutex<Option<Arc<PickedCopy>>>> = Arc::default();
    let worker = {
        let (asked, taken, picks, tracker) = (asked.clone(), taken.clone(), picks.clone(), tracker.clone());
        upgrade_worker(&queue, Some(tracker.clone()), picks.clone(), move |request| {
            *asked.lock() = Some(request.only_source);
            (request.on_replacement_queued.as_ref().expect("a follower"))("outside", "ext1");
            *taken.lock() = picks.take("ext1", "Lidarr");
            tracker.complete("outside", "ext1", None);
            LibraryActionOutcome {
                code: Some(LibraryActionCodes::NO_REPLACEMENT.to_string()),
                ..LibraryActionOutcome::new(LibraryActionState::Failed, Some("nothing".into()))
            }
        })
    };

    worker.tick(&CancellationToken::new()).await;
    worker.drain().await;

    assert_eq!(*asked.lock(), Some(Some(DownloadSource::Lidarr)));
    assert_eq!(
        taken
            .lock()
            .as_ref()
            .and_then(|p| p.release_guid.clone())
            .as_deref(),
        Some("g9")
    );
    let row = tracker.detail("outside:ext1", Some("alice")).expect("the row");
    assert_eq!(row.kind, acquisition_kinds::UPGRADE);
    assert_eq!(row.id, "nd-1");
    let events = row.events.expect("a log");
    assert!(
        events
            .iter()
            .any(|line| line.text == "Getting Lidarr release \"Air - Moon Safari [FLAC]\"")
    );
    assert_eq!(
        events.last().expect("a line").text,
        "The copy you picked did not go in; your copy was kept"
    );
    let job = &queue.snapshot()[0];
    assert_eq!(job.state, "notFound");
    assert_eq!(
        job.detail.as_deref(),
        Some("The copy you picked did not replace yours: nothing. Your copy was kept.")
    );
    assert_eq!(
        UpgradeWorker::pick_not_taken(Some(
            "The replacement was the wrong recording, so nothing changed."
        )),
        "The copy you picked did not replace yours: it was the wrong recording. Your copy was kept."
    );
}

// ---------------------------------------------------------------------------------------
// Lyrics in the log
// ---------------------------------------------------------------------------------------

#[test]
fn lyrics_say_what_was_found_from_where_and_where_they_went() {
    let job = LyricsJob::new("/music/a.flac", "Air", "Sexy Boy", None, Some(298))
        .with_acquisition(Some("outside:abc".into()));
    let found = LyricsResult::new("LRCLIB", Some("[00:01.00]Hi".into()), None, false);
    let text = |outcome: LyricsWriteOutcome, result: Option<LyricsResult>, place: SongLyricsPlace| {
        let (text, detail) =
            LyricsSidecarWriter::report_text(&job, &LyricsWrite::new(outcome, result), place);
        (text, detail)
    };
    let said = |text: &str, detail: &str| (Some(text.to_string()), Some(detail.to_string()));

    assert_eq!(
        text(
            LyricsWriteOutcome::Written,
            Some(found.clone()),
            SongLyricsPlace::Inside
        ),
        said("Synced lyrics from LRCLIB", "Embedded in the song")
    );
    assert_eq!(
        text(LyricsWriteOutcome::Written, Some(found), SongLyricsPlace::Beside),
        said(
            "Synced lyrics from LRCLIB",
            "Saved beside the song as a .lrc file"
        )
    );
    assert_eq!(
        text(LyricsWriteOutcome::NotFound, None, SongLyricsPlace::None),
        said("No lyrics found", "None of the lyrics sources has this song")
    );
    assert_eq!(
        text(LyricsWriteOutcome::Retrying, None, SongLyricsPlace::None)
            .0
            .as_deref(),
        Some("No lyrics service answered")
    );
    assert_eq!(
        text(LyricsWriteOutcome::Gone, None, SongLyricsPlace::None).0,
        None
    );
}
