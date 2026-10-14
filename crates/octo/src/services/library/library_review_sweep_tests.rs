//! The Review sweep asks about the library and never acts, and it gets out of a download's way
//! (#72). And AcoustID's background lane, which the sweep's lookups use.

use std::sync::Weak;
use std::sync::atomic::{AtomicI64, AtomicUsize};

use chrono::TimeZone;
use octo_core::fingerprint::acoust_id_client::AcoustIdRecording;
use octo_core::models::subsonic::ScanStatus;
use octo_core::settings::{AppSettings, DownloadSettings, NoticeKind, SubsonicSettings};

use super::*;
use crate::services::library::notice_playlist_worker::NoticePlaylistWorker;
use crate::services::library::notice_queue::{NoticeEntry, NoticeState};
use crate::services::local::{LocalSongMapping, ParsedExternalId, ParsedSongId};

type Answer = Box<dyn Fn() -> VerificationResult + Send + Sync>;

struct FakeVerifier {
    ready: std::sync::atomic::AtomicBool,
    asked: Mutex<Vec<String>>,
    answer: Mutex<Answer>,
}

impl FakeVerifier {
    fn answering(answer: impl Fn() -> VerificationResult + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(FakeVerifier {
            ready: std::sync::atomic::AtomicBool::new(true),
            asked: Mutex::new(Vec::new()),
            answer: Mutex::new(Box::new(answer)),
        })
    }

    fn confirming() -> Arc<Self> {
        Self::answering(|| confirmed(200, Some(200)))
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().clone()
    }
}

#[async_trait]
impl IReviewSweepVerifier for FakeVerifier {
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    async fn verify(&self, path: &str, _: Option<&str>, _: Option<&str>) -> VerificationResult {
        self.asked
            .lock()
            .push(path.rsplit('/').next().unwrap_or(path).to_string());
        (self.answer.lock())()
    }
}

#[derive(Default)]
struct FakeActivity {
    busy: std::sync::atomic::AtomicBool,
}

impl IAcquisitionActivity for FakeActivity {
    fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

/// A stand-in `ILocalLibraryService`: Octo's downloads, and a callback run while the library is
/// listed, given the worker.
#[derive(Default)]
struct FakeLibrary {
    octo_downloads: Vec<String>,
    while_listing: Option<Box<dyn Fn(&LibraryReviewSweepWorker) + Send + Sync>>,
    worker: OnceLock<Weak<LibraryReviewSweepWorker>>,
}

#[async_trait]
impl ILocalLibraryService for FakeLibrary {
    async fn get_local_path_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    async fn register_downloaded_song(&self, _: &Song, _: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_local_id_for_external_song(&self, _: &str, _: &str) -> Option<String> {
        None
    }

    fn parse_song_id(&self, _: &str) -> ParsedSongId {
        (false, None, None)
    }

    fn parse_external_id(&self, _: &str) -> ParsedExternalId {
        (false, None, None, None)
    }

    async fn get_mappings(&self) -> Vec<LocalSongMapping> {
        if let (Some(callback), Some(worker)) =
            (&self.while_listing, self.worker.get().and_then(Weak::upgrade))
        {
            callback(&worker);
        }
        self.octo_downloads
            .iter()
            .map(|path| LocalSongMapping {
                local_path: path.clone(),
                ..Default::default()
            })
            .collect()
    }

    async fn find_mapping_by_tags(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Option<LocalSongMapping> {
        None
    }

    async fn forget_mapping(&self, _: &str) -> anyhow::Result<Vec<crate::services::local::LocalSongMapping>> {
        Ok(Vec::new())
    }

    async fn trigger_library_scan(&self, _: bool) -> bool {
        true
    }

    async fn get_scan_status(&self) -> Option<ScanStatus> {
        None
    }
}

fn confirmed(file: i32, recording: Option<i32>) -> VerificationResult {
    let mut matched = AcoustIdRecording::new(
        "rec",
        "Teardrop",
        vec!["Massive Attack".to_string()],
        Some("Mezzanine"),
        Some(1998),
    );
    matched.duration_seconds = recording;
    VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        duration_seconds: file,
        recording_id: Some("rec".into()),
        r#match: Some(matched),
        ..Default::default()
    }
}

/// A fingerprint AcoustID has no entry for: the track stays unknown.
fn unknown() -> VerificationResult {
    VerificationResult {
        reason: InconclusiveReason::NoEntry,
        fingerprint: Some("AQADtEqk".into()),
        duration_seconds: 330,
        ..Default::default()
    }
}

fn settings_of(per_hour: i32) -> LibraryActionSettings {
    LibraryActionSettings {
        enabled: true,
        review_enabled: true,
        review_sweep_per_hour: per_hour,
        allowed_users: vec!["alice".into(), "bob".into()],
        ..Default::default()
    }
}

/// The test setup: a music root, the state file beside it, and every store built so far.
struct Sweep {
    root: tempfile::TempDir,
    stores: Mutex<Vec<Arc<ReviewSweepStore>>>,
}

#[derive(Default)]
struct Options {
    queue: Option<Arc<NoticeQueue>>,
    settings: Option<LibraryActionSettings>,
    activity: Option<Arc<FakeActivity>>,
    octo_downloads: Vec<String>,
    clock: Option<Clock>,
    while_listing: Option<Box<dyn Fn(&LibraryReviewSweepWorker) + Send + Sync>>,
}

impl Sweep {
    fn new() -> Self {
        Sweep {
            root: tempfile::tempdir().expect("a temp dir"),
            stores: Mutex::new(Vec::new()),
        }
    }

    fn root(&self) -> String {
        self.root.path().to_string_lossy().into_owned()
    }

    fn state_path(&self) -> PathBuf {
        self.root.path().join(".state").join("review-sweep.json")
    }

    fn song(&self, name: &str) -> String {
        let path = self.root.path().join(name);
        std::fs::write(&path, "x").expect("written");
        path.to_string_lossy().into_owned()
    }

    fn worker(&self, verifier: Arc<FakeVerifier>, options: Options) -> Arc<LibraryReviewSweepWorker> {
        // A worker built after another is a restart, and a stopping host flushes the store.
        for previous in self.stores.lock().iter() {
            previous.flush();
        }
        let store = Arc::new(ReviewSweepStore::new(Some(self.state_path())));
        self.stores.lock().push(store.clone());
        let library = Arc::new(FakeLibrary {
            octo_downloads: options.octo_downloads,
            while_listing: options.while_listing,
            worker: OnceLock::new(),
        });
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            library_actions: options.settings.unwrap_or_else(|| settings_of(60)),
            subsonic: SubsonicSettings {
                admin_username: Some("bob".into()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let root = self.root();
        let worker = Arc::new(LibraryReviewSweepWorker::new(
            LibraryReviewSweepParts {
                store,
                notices: options.queue.unwrap_or_else(|| Arc::new(NoticeQueue::new())),
                verifier,
                activity: options.activity.unwrap_or_default(),
                library: library.clone(),
                settings,
                music_root: Box::new(move || root.clone()),
            },
            options.clock.unwrap_or_else(Clock::system),
        ));
        let _ = library.worker.set(Arc::downgrade(&worker));
        worker
    }

    fn fine_on_disk(&self) -> i32 {
        let text = std::fs::read_to_string(self.state_path()).expect("the state file");
        serde_json::from_str::<ReviewSweepState>(&text)
            .expect("state")
            .fine
    }
}

/// A clock that moves only when told to, and by `step` each time it is read.
fn store_clock(step: TimeDelta) -> (Clock, Arc<AtomicI64>) {
    let start = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
    let offset = Arc::new(AtomicI64::new(0));
    let moved = offset.clone();
    let step = step.num_microseconds().expect("small");
    let clock = Clock::new(move || {
        let micros = moved.fetch_add(step, Ordering::SeqCst) + step;
        start + TimeDelta::microseconds(micros)
    });
    (clock, offset)
}

fn five_seconds() -> TimeDelta {
    TimeDelta::from_std(ReviewSweepStore::FLUSH_INTERVAL).expect("five seconds")
}

#[test]
fn the_store_writes_at_most_once_per_window_and_keeps_the_rest_for_the_next_flush() {
    let sweep = Sweep::new();
    let (clock, offset) = store_clock(TimeDelta::zero());
    let store = ReviewSweepStore::with_clock(Some(sweep.state_path()), clock);

    store.update(|s| s.fine = 1);
    assert_eq!(sweep.fine_on_disk(), 1);
    store.update(|s| s.fine = 2);
    store.update(|s| s.fine = 3);
    assert_eq!(sweep.fine_on_disk(), 1);

    // The timer's tick.
    store.flush();
    assert_eq!(sweep.fine_on_disk(), 3);
    store.update(|s| s.fine = 4);
    assert_eq!(sweep.fine_on_disk(), 3);

    offset.fetch_add(
        five_seconds().num_microseconds().expect("small"),
        Ordering::SeqCst,
    );
    store.update(|s| s.fine = 5);
    assert_eq!(sweep.fine_on_disk(), 5);

    store.update(|s| s.fine = 6);
    assert_eq!(sweep.fine_on_disk(), 5);
    store.flush();
    assert_eq!(
        ReviewSweepStore::new(Some(sweep.state_path())).read(|s| s.fine),
        6
    );
}

#[test]
fn a_pause_is_never_lost_to_an_older_write() {
    let sweep = Sweep::new();
    // Every change is old enough to be written at once.
    let (clock, _) = store_clock(five_seconds());
    let store = Arc::new(ReviewSweepStore::with_clock(Some(sweep.state_path()), clock));
    let (snapshot_taken, taken) = std::sync::mpsc::channel::<()>();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Mutex::new(released);
    let held = AtomicUsize::new(0);
    store.set_before_write(Some(Arc::new(move || {
        if held.fetch_add(1, Ordering::SeqCst) != 0 {
            return;
        }
        let _ = snapshot_taken.send(());
        let _ = released.lock().recv_timeout(std::time::Duration::from_secs(10));
    })));

    // A tick's write is held between its snapshot and the disk while a Pause comes in.
    let ticking = store.clone();
    let tick = std::thread::spawn(move || ticking.update(|s| s.fine += 1));
    taken
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the snapshot was taken");
    let pausing = store.clone();
    let pause = std::thread::spawn(move || {
        pausing.update(|s| s.paused = true);
        pausing.flush();
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    release.send(()).expect("released");
    tick.join().expect("the tick");
    pause.join().expect("the pause");

    let restarted = ReviewSweepStore::new(Some(sweep.state_path()));
    assert!(restarted.read(|s| s.paused));
    assert_eq!(restarted.read(|s| s.fine), 1);
}

#[tokio::test]
async fn starting_over_while_the_library_is_listed_lists_it_again() {
    let sweep = Sweep::new();
    sweep.song("a.flac");
    sweep.song("b.flac");
    let verifier = FakeVerifier::confirming();
    let resets = AtomicUsize::new(0);
    let worker = sweep.worker(
        verifier.clone(),
        Options {
            while_listing: Some(Box::new(move |w| {
                if resets.fetch_add(1, Ordering::SeqCst) == 0 {
                    w.reset();
                }
            })),
            ..Default::default()
        },
    );

    worker.tick().await;
    assert!(verifier.asked().is_empty());

    worker.tick().await;
    assert_eq!(verifier.asked(), ["a.flac"]);
    assert_eq!(worker.status().total, 2);
}

#[tokio::test]
async fn a_song_in_octos_staging_folder_is_never_checked() {
    let sweep = Sweep::new();
    std::fs::create_dir_all(sweep.root.path().join(".octo-incoming")).expect("created");
    std::fs::create_dir_all(sweep.root.path().join("Artist")).expect("created");
    sweep.song(".octo-incoming/half.flac");
    sweep.song("Artist/whole.flac");
    let verifier = FakeVerifier::confirming();
    let worker = sweep.worker(verifier.clone(), Options::default());
    for _ in 0..3 {
        worker.tick().await;
    }
    assert_eq!(verifier.asked(), ["whole.flac"]);
    assert_eq!(worker.status().total, 1);
}

#[tokio::test]
async fn zero_an_hour_is_off() {
    let sweep = Sweep::new();
    sweep.song("a.flac");
    let verifier = FakeVerifier::confirming();
    let worker = sweep.worker(
        verifier.clone(),
        Options {
            settings: Some(settings_of(0)),
            ..Default::default()
        },
    );
    worker.tick().await;
    assert!(verifier.asked().is_empty());
    assert_eq!(worker.status().state, "Off");
}

#[tokio::test]
async fn a_restart_carries_on_where_it_stopped() {
    let sweep = Sweep::new();
    sweep.song("a.flac");
    sweep.song("b.flac");
    sweep.song("c.flac");
    let first = FakeVerifier::confirming();
    let worker = sweep.worker(first.clone(), Options::default());
    worker.tick().await;
    worker.tick().await;
    assert_eq!(first.asked(), ["a.flac", "b.flac"]);

    let second = FakeVerifier::confirming();
    sweep.worker(second.clone(), Options::default()).tick().await;
    assert_eq!(second.asked(), ["c.flac"]);
}

#[tokio::test]
async fn a_changed_file_is_checked_again_and_an_unchanged_one_is_not() {
    let sweep = Sweep::new();
    let a = sweep.song("a.flac");
    sweep.song("b.flac");
    let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap()));
    let reading = now.clone();
    let verifier = FakeVerifier::confirming();
    let worker = sweep.worker(
        verifier.clone(),
        Options {
            clock: Some(Clock::new(move || *reading.lock())),
            ..Default::default()
        },
    );
    for _ in 0..3 {
        worker.tick().await;
    }
    assert_eq!(verifier.asked(), ["a.flac", "b.flac"]);

    std::fs::write(&a, "replaced with a longer file").expect("written");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(300);
    std::fs::File::options()
        .write(true)
        .open(&a)
        .and_then(|file| file.set_modified(later))
        .expect("touched");
    *now.lock() += LibraryReviewSweepWorker::PASS_INTERVAL + TimeDelta::minutes(1);
    worker.tick().await;
    worker.tick().await;
    assert_eq!(verifier.asked(), ["a.flac", "b.flac", "a.flac"]);
}

#[tokio::test]
async fn songs_octo_downloaded_are_skipped() {
    let sweep = Sweep::new();
    let mine = sweep.song("a.flac");
    sweep.song("b.flac");
    let verifier = FakeVerifier::confirming();
    sweep
        .worker(
            verifier.clone(),
            Options {
                octo_downloads: vec![mine],
                ..Default::default()
            },
        )
        .tick()
        .await;
    assert_eq!(verifier.asked(), ["b.flac"]);
}

#[tokio::test]
async fn only_the_keeper_is_asked() {
    let sweep = Sweep::new();
    sweep.song("a.flac");
    let queue = Arc::new(NoticeQueue::new());
    sweep
        .worker(
            FakeVerifier::answering(unknown),
            Options {
                queue: Some(queue.clone()),
                ..Default::default()
            },
        )
        .tick()
        .await;
    assert_eq!(queue.for_user("bob", NoticeKind::Review).len(), 1);
    assert!(queue.for_user("alice", NoticeKind::Review).is_empty());
}

#[test]
fn keeper_is_the_allowed_admin_else_the_first_allowed_user() {
    for (admin, expected) in [(Some("bob"), "bob"), (Some("root"), "alice"), (None, "alice")] {
        assert_eq!(
            LibraryReviewSweepWorker::keeper(&settings_of(60), admin).as_deref(),
            Some(expected),
            "{admin:?}"
        );
    }
}

#[tokio::test]
async fn a_confident_different_recording_is_asked_about_and_nothing_is_done() {
    let sweep = Sweep::new();
    let path = sweep.song("a.flac");
    let queue = Arc::new(NoticeQueue::new());
    let verifier = FakeVerifier::answering(|| VerificationResult {
        verdict: VerificationVerdict::Mismatch,
        score: 0.97,
        matched_artist: Some("Portishead".into()),
        matched_title: Some("Roads".into()),
        recording_id: Some("rec-roads".into()),
        fingerprint: Some("AQADtEqk".into()),
        duration_seconds: 305,
        ..Default::default()
    });
    sweep
        .worker(
            verifier,
            Options {
                queue: Some(queue.clone()),
                ..Default::default()
            },
        )
        .tick()
        .await;

    let entries = queue.for_user("bob", NoticeKind::Review);
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.cause, InconclusiveReason::SoundsLikeAnother);
    assert_eq!(entry.origin, NoticeOrigin::LibrarySweep);
    assert_eq!(
        entry.reason,
        "Sounds like 'Portishead - Roads', not what its tags say"
    );
    assert!(entry.fingerprint.is_none());
    assert!(Path::new(&path).exists());
}

#[tokio::test]
async fn a_song_much_shorter_than_its_recording_is_asked_about() {
    let sweep = Sweep::new();
    sweep.song("a.flac");
    let queue = Arc::new(NoticeQueue::new());
    sweep
        .worker(
            FakeVerifier::answering(|| confirmed(120, Some(300))),
            Options {
                queue: Some(queue.clone()),
                ..Default::default()
            },
        )
        .tick()
        .await;
    let entries = queue.for_user("bob", NoticeKind::Review);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].cause, InconclusiveReason::LengthOff);
}

#[test]
fn length_off_is_more_than_twenty_seconds_or_a_tenth() {
    for (file, recording, expected) in [
        (219, Some(200), false),
        (221, Some(200), true),
        (659, Some(600), false),
        (661, Some(600), true),
        (200, None, false),
    ] {
        assert_eq!(
            LibraryReviewSweepWorker::is_length_off(file, recording),
            expected,
            "{file} {recording:?}"
        );
    }
}

#[test]
fn keeping_a_library_question_sends_nothing_to_acoust_id() {
    let queue = NoticeQueue::new();
    queue.add_review_from(
        "bob",
        "/music/a.flac",
        &Song {
            artist: "Massive Attack".into(),
            title: "Teardrop".into(),
            ..Default::default()
        },
        &unknown(),
        NoticeOrigin::LibrarySweep,
    );
    let key = NoticeQueue::review_key("bob", "/music/a.flac");
    queue.set_navidrome_id(&key, "nd-1");
    queue.mark_queued(&[key]);

    assert!(queue.mark_kept("bob", "nd-1").is_some());
    assert!(queue.awaiting_submission().is_empty());
    assert!(!NoticePlaylistWorker::submittable(
        &NoticeEntry {
            cause: InconclusiveReason::NoEntry,
            fingerprint: Some("AQADtEqk".into()),
            duration_seconds: 330,
            origin: NoticeOrigin::LibrarySweep,
            ..Default::default()
        },
        &DownloadSettings {
            fingerprint_seconds: 120,
            ..Default::default()
        }
    ));
}

#[tokio::test]
async fn fifty_open_library_questions_pause_it_and_an_answer_lets_it_carry_on() {
    let sweep = Sweep::new();
    sweep.song("z.flac");
    let queue = Arc::new(NoticeQueue::new());
    for i in 0..LibraryReviewSweepWorker::MAX_OPEN_QUESTIONS {
        queue.add_review_from(
            "bob",
            &format!("/music/{i}.flac"),
            &Song {
                artist: "A".into(),
                title: format!("T{i}"),
                ..Default::default()
            },
            &unknown(),
            NoticeOrigin::LibrarySweep,
        );
    }
    let verifier = FakeVerifier::confirming();
    let worker = sweep.worker(
        verifier.clone(),
        Options {
            queue: Some(queue.clone()),
            ..Default::default()
        },
    );

    worker.tick().await;
    assert!(verifier.asked().is_empty());
    assert_eq!(worker.status().state, "Paused");

    queue.resolve(
        &NoticeQueue::review_key("bob", "/music/0.flac"),
        NoticeState::Kept,
    );
    worker.tick().await;
    assert_eq!(verifier.asked(), ["z.flac"]);
}

#[tokio::test]
async fn a_download_in_flight_makes_it_wait() {
    let sweep = Sweep::new();
    sweep.song("a.flac");
    let verifier = FakeVerifier::confirming();
    let activity = Arc::new(FakeActivity::default());
    activity.busy.store(true, Ordering::SeqCst);
    let worker = sweep.worker(
        verifier.clone(),
        Options {
            activity: Some(activity.clone()),
            ..Default::default()
        },
    );

    assert_eq!(worker.tick().await, LibraryReviewSweepWorker::BUSY_CHECK);
    assert!(verifier.asked().is_empty());
    activity.busy.store(false, Ordering::SeqCst);
    worker.tick().await;
    assert_eq!(verifier.asked(), ["a.flac"]);
}

#[tokio::test]
async fn an_unanswered_lookup_is_tried_again_not_recorded() {
    let sweep = Sweep::new();
    sweep.song("a.flac");
    sweep.song("b.flac");
    let verifier = FakeVerifier::answering(|| VerificationResult {
        reason: InconclusiveReason::LookupFailed,
        ..Default::default()
    });
    let worker = sweep.worker(verifier.clone(), Options::default());
    worker.tick().await;
    *verifier.answer.lock() = Box::new(|| confirmed(200, Some(200)));
    worker.tick().await;
    assert_eq!(verifier.asked(), ["a.flac", "a.flac"]);
}

// ---- AcoustID's background lane ----------------------------------------------------------
// That the lane is refused while a download waits, and never queues, is
// `the_background_lane_never_waits` in `acoust_id_rate_limiter.rs`.

#[tokio::test]
async fn the_background_lane_gets_a_permit_when_nobody_waits() {
    let limiter = AcoustIdRateLimiter::new();
    assert!(
        AcoustIdRateLimiter::in_background(limiter.acquire())
            .await
            .is_acquired()
    );
}

#[tokio::test]
async fn the_background_flag_does_not_leak_to_the_caller() {
    AcoustIdRateLimiter::in_background(async { 0 }).await;
    assert!(!AcoustIdRateLimiter::in_background_now());
}

// ---- The state file and the verdicts -----------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/rust-migration/fixtures/state")
        .join(name)
}

/// The state file reads and writes back byte for byte, its checked files in their order.
#[test]
fn the_review_sweep_fixture_round_trips_byte_for_byte() {
    let original = std::fs::read_to_string(fixture("review-sweep.json")).expect("the fixture");
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("review-sweep.json");
    std::fs::write(&path, &original).expect("copied");

    let store = ReviewSweepStore::new(Some(path.clone()));
    assert_eq!(store.read(|s| (s.pass, s.checked.len())), (2, 2));
    store.update(|_| {});
    assert_eq!(std::fs::read_to_string(&path).expect("written"), original);
}

/// The state is set aside, never overwritten, and the sweep starts over.
#[test]
fn a_corrupt_state_file_is_kept_aside_and_the_sweep_starts_over() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("review-sweep.json");
    std::fs::write(&path, "{\"Pass\":").expect("written");

    let store = ReviewSweepStore::new(Some(path.clone()));

    assert_eq!(store.read(|s| (s.pass, s.cursor.clone())), (1, String::new()));
    assert!(!path.exists());
    let aside = std::fs::read_dir(dir.path())
        .expect("listed")
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("review-sweep.json.corrupt-")
        })
        .count();
    assert_eq!(aside, 1);
}

/// A fresh state writes `NextPassUtc` as the minimum date, with no `Z`, and `Pass` 1.
#[test]
fn a_fresh_state_keeps_its_wire_shape() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("review-sweep.json");
    let store = ReviewSweepStore::new(Some(path.clone()));
    store.update(|s| s.paused = true);
    assert_eq!(
        std::fs::read_to_string(&path).expect("written"),
        r#"{"Paused":true,"Cursor":"","Checked":{},"Pass":1,"Total":0,"Found":0,"Fine":0,"Undecodable":0,"LastCheckedUtc":null,"PassFinishedUtc":null,"NextPassUtc":"0001-01-01T00:00:00"}"#
    );
}

#[test]
fn classify_sorts_each_verdict_into_its_outcome() {
    let undecodable = VerificationResult {
        verdict: VerificationVerdict::Mismatch,
        ..Default::default()
    };
    assert_eq!(
        LibraryReviewSweepWorker::classify(undecodable).0,
        Outcome::Undecodable
    );
    assert_eq!(
        LibraryReviewSweepWorker::classify(confirmed(200, Some(200))).0,
        Outcome::Fine
    );
    let not_fingerprinted = VerificationResult {
        reason: InconclusiveReason::NotFingerprinted,
        ..Default::default()
    };
    assert_eq!(
        LibraryReviewSweepWorker::classify(not_fingerprinted).0,
        Outcome::NotFingerprinted
    );
    assert_eq!(LibraryReviewSweepWorker::classify(unknown()).0, Outcome::Ask);
}

#[tokio::test]
async fn without_a_verification_service_the_sweep_is_not_set_up() {
    let verifier = FingerprintSweepVerifier::new();
    assert!(!verifier.is_ready());
    assert_eq!(
        verifier.verify("/music/a.flac", None, None).await.reason,
        InconclusiveReason::Disabled
    );
}

#[tokio::test]
async fn reset_and_pause_reach_the_disk_at_once() {
    let sweep = Sweep::new();
    sweep.song("a.flac");
    let worker = sweep.worker(FakeVerifier::confirming(), Options::default());
    worker.tick().await;
    assert_eq!(worker.status().fine, 1);
    worker.set_paused(true);
    assert!(ReviewSweepStore::new(Some(sweep.state_path())).read(|s| s.paused));
    worker.reset();
    let restarted = ReviewSweepStore::new(Some(sweep.state_path()));
    assert_eq!(
        restarted.read(|s| (s.fine, s.checked.len(), s.paused)),
        (0, 0, true)
    );
    assert_eq!(worker.status().state, "Running");
}

fn scoped_sweep(family_on: bool) -> LibraryReviewSweepWorker {
    LibraryReviewSweepWorker::new(
        LibraryReviewSweepParts {
            store: Arc::new(ReviewSweepStore::new(None)),
            notices: Arc::new(NoticeQueue::new()),
            verifier: FakeVerifier::confirming(),
            activity: Arc::new(FakeActivity::default()),
            library: Arc::new(FakeLibrary::default()),
            settings: Arc::new(SettingsStore::for_tests(AppSettings::default())),
            music_root: Box::new(|| "/music".to_string()),
        },
        Clock::system(),
    )
    .with_library_scope(crate::services::family::library_index::scope_for_tests(
        family_on,
        &[("alex", 3)],
    ))
}

/// While members have libraries of their own the sweep keeps to the shared library: a member's
/// own songs and the family pool are never asked about, with Family on or turned off since.
#[test]
fn the_sweep_never_asks_about_a_family_members_own_songs() {
    for family_on in [true, false] {
        let sweep = scoped_sweep(family_on);
        let view = sweep.shared_view();
        let asks = |full: &str| LibraryReviewSweepWorker::in_shared(view.as_ref(), full);
        assert!(asks("/music/Massive Attack/Mezzanine/01 Angel.flac"));
        assert!(!asks("/members/alex/Massive Attack/Mezzanine/01 Angel.flac"));
        assert!(!asks("/members/.pool/Massive Attack/Mezzanine/01 Angel.flac"));
    }
    let never_family = scoped_sweep(false).with_library_scope(
        crate::services::family::library_index::scope_for_tests(false, &[]),
    );
    let view = never_family.shared_view();
    assert!(view.is_none());
    assert!(LibraryReviewSweepWorker::in_shared(
        view.as_ref(),
        "/members/alex/Massive Attack/Mezzanine/01 Angel.flac"
    ));
}
