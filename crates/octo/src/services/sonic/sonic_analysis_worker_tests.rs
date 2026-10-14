//! Sounds alike's pass over the library: when it holds, what it sends to octo-sonic, what it
//! skips without touching the disk, how failures and crashes are remembered, and new arrivals
//! first.

use std::sync::atomic::{AtomicBool, AtomicUsize};

use chrono::TimeZone;
use octo_core::settings::{AppSettings, RadioSourceSettings};

use super::*;
use crate::services::sonic::sonic_client::{SonicDropped, SonicFeatures, SonicHealth};
use crate::services::sonic::transition_store::TransitionLookup;

#[derive(Default)]
struct Activity(AtomicBool);

impl IAcquisitionActivity for Activity {
    fn is_busy(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// octo-sonic: its health, and per file name an answer (features, an error, or a dropped
/// connection).
struct Sidecar {
    version: Mutex<Option<i32>>,
    /// The transition profile version it writes; `None` for one from before profiles.
    transition: Mutex<Option<i32>>,
    /// What each reading asked for.
    parts: Mutex<Vec<SonicParts>>,
    /// The published tempo each reading was handed.
    bpms: Mutex<Vec<Option<f64>>>,
    /// Its music folder is empty or unreadable: health answers 503.
    blind: AtomicBool,
    /// Sounds alike is off in .env, on an octo-sonic from before profiles: health answers 503
    /// with off.
    off: AtomicBool,
    /// Sounds alike is off in .env: it makes profiles only, and refuses to read the sound.
    profiles_only: AtomicBool,
    analysed: Mutex<Vec<String>>,
    errors: Mutex<HashMap<String, String>>,
    drops: Mutex<HashSet<String>>,
    /// Files it cannot find (404), as when it mounts another folder than Octo.
    missing: Mutex<HashSet<String>>,
    /// Files it answers with something that is not its JSON.
    garbled: Mutex<HashSet<String>>,
}

impl Default for Sidecar {
    fn default() -> Self {
        Sidecar {
            version: Mutex::new(Some(2)),
            transition: Mutex::new(None),
            parts: Mutex::default(),
            bpms: Mutex::default(),
            blind: AtomicBool::new(false),
            off: AtomicBool::new(false),
            profiles_only: AtomicBool::new(false),
            analysed: Mutex::default(),
            errors: Mutex::default(),
            drops: Mutex::default(),
            missing: Mutex::default(),
            garbled: Mutex::default(),
        }
    }
}

impl Sidecar {
    fn analysed(&self) -> Vec<String> {
        self.analysed.lock().clone()
    }
}

#[async_trait]
impl ISonicClient for Sidecar {
    async fn health(&self) -> Option<SonicHealth> {
        let version = (*self.version.lock())?;
        let transition_version = *self.transition.lock();
        if self.off.load(Ordering::SeqCst) {
            return Some(SonicHealth {
                version,
                transition_version,
                problem: None,
                off: true,
                profiles_only: false,
            });
        }
        Some(SonicHealth {
            version,
            transition_version,
            problem: self
                .blind
                .load(Ordering::SeqCst)
                .then(|| "cannot see the music folder".to_string()),
            off: false,
            profiles_only: self.profiles_only.load(Ordering::SeqCst),
        })
    }

    async fn analyze(
        &self,
        path: &str,
        parts: SonicParts,
        bpm: Option<f64>,
    ) -> Result<SonicAnswer, SonicDropped> {
        self.bpms.lock().push(bpm);
        self.parts.lock().push(parts);
        let name = std::path::Path::new(path)
            .file_name()
            .expect("a file name")
            .to_string_lossy()
            .into_owned();
        self.analysed.lock().push(name.clone());
        if self.drops.lock().contains(&name) {
            return Err(SonicDropped("connection reset".into()));
        }
        let answer = |error: &str, status: u16| SonicAnswer {
            error: Some(error.to_string()),
            status,
            ..Default::default()
        };
        if self.missing.lock().contains(&name) {
            return Ok(answer("no such file", 404));
        }
        if self.garbled.lock().contains(&name) {
            return Ok(answer(
                "octo-sonic answered something Octo could not read (HTTP 200)",
                200,
            ));
        }
        if let Some(error) = self.errors.lock().get(&name) {
            return Ok(answer(error, 422));
        }
        if parts.sound && self.profiles_only.load(Ordering::SeqCst) {
            panic!("asked octo-sonic for the sound of {name} with Sounds alike off in .env");
        }
        let transition = self.transition.lock().unwrap_or(1);
        Ok(SonicAnswer {
            features: parts.sound.then(|| SonicFeatures {
                values: (0..23).map(|i| i as f32 / 100.0).collect(),
                version: self.version.lock().unwrap_or(2),
            }),
            transition: parts
                .transition
                .then(|| serde_json::json!({ "version": transition, "durationMs": 200_000, "file": name })),
            error: None,
            status: 200,
        })
    }
}

struct Library {
    directory: std::path::PathBuf,
    rows: Mutex<Vec<LibrarySongRow>>,
    arrivals: Mutex<HashMap<String, LibrarySongRow>>,
    file_lookups: AtomicUsize,
    listable: AtomicBool,
    /// A page that fails, by its start.
    failing_page: Mutex<Option<i32>>,
    /// Songs Navidrome lists as missing: counted in the page, left out of its rows.
    missing_ids: Mutex<HashSet<String>>,
}

impl Library {
    fn row(
        &self,
        id: &str,
        path: std::path::PathBuf,
        size: i64,
        title: &str,
        seconds: Option<i32>,
    ) -> LibrarySongRow {
        LibrarySongRow {
            id: id.into(),
            path: path.to_string_lossy().into_owned(),
            size,
            suffix: "flac".into(),
            bit_rate: 900,
            title: title.into(),
            artist: "Artist".into(),
            duration: seconds,
            album: Some("Album".into()),
            ..Default::default()
        }
    }

    /// A row with no file behind it, for listing tests.
    fn add_listed(&self, id: &str) {
        let path = self.directory.join("none").join(format!("{id}.flac"));
        let row = self.row(id, path, 1000, id, Some(200));
        self.rows.lock().push(row);
    }

    /// A song with a file; listed unless `listed` is false.
    fn add(&self, id: &str, name: &str, seconds: Option<i32>, listed: bool) -> LibrarySongRow {
        let path = self.directory.join(name);
        std::fs::write(&path, vec![0u8; 1000 + id.len()]).expect("writes");
        let size = std::fs::metadata(&path).expect("there").len() as i64;
        let row = self.row(id, path, size, name, seconds);
        if listed {
            self.rows.lock().push(row.clone());
        }
        row
    }
}

#[async_trait]
impl ISonicLibrary for Library {
    /// As Navidrome's native API answers: the count is the page's rows before missing songs were
    /// left out, not the library's.
    async fn list(&self, start: i32, count: i32) -> Option<(Vec<LibrarySongRow>, usize)> {
        if !self.listable.load(Ordering::SeqCst) || *self.failing_page.lock() == Some(start) {
            return None;
        }
        let rows = self.rows.lock();
        let page: Vec<LibrarySongRow> = rows
            .iter()
            .skip(start as usize)
            .take(count as usize)
            .cloned()
            .collect();
        let missing = self.missing_ids.lock();
        let len = page.len();
        Some((
            page.into_iter()
                .filter(|row| !missing.contains(&row.id))
                .collect(),
            len,
        ))
    }

    async fn arrived(&self, id: &str) -> Option<LibrarySongRow> {
        self.arrivals.lock().get(id).cloned()
    }

    fn local_file(&self, row: &LibrarySongRow) -> Option<SonicFile> {
        self.file_lookups.fetch_add(1, Ordering::SeqCst);
        SonicFile::stat(&row.path)
    }
}

struct T {
    _dir: tempfile::TempDir,
    worker: Arc<SonicAnalysisWorker>,
    store: Arc<SonicStore>,
    transitions: Arc<TransitionStore>,
    sidecar: Arc<Sidecar>,
    library: Arc<Library>,
    activity: Arc<Activity>,
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl T {
    fn advance(&self, by: TimeDelta) {
        *self.now.lock() += by;
    }

    async fn tick(&self) {
        self.worker.tick().await;
    }

    fn state(&self) -> (String, Option<String>) {
        let status = self.worker.status();
        (status.state, status.reason)
    }

    async fn ticks_until_done(&self) -> usize {
        for tick in 1..=20 {
            self.tick().await;
            if self.worker.status().state == "Done" {
                return tick;
            }
        }
        panic!("the pass never finished");
    }
}

fn make(settings: Option<RadioSourceSettings>) -> T {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = Arc::new(SonicStore::new(dir.path().join("state.json")));
    let transitions = Arc::new(TransitionStore::new(dir.path().join("profiles")));
    let sidecar = Arc::new(Sidecar::default());
    let library = Arc::new(Library {
        directory: dir.path().to_path_buf(),
        rows: Mutex::default(),
        arrivals: Mutex::default(),
        file_lookups: AtomicUsize::new(0),
        listable: AtomicBool::new(true),
        failing_page: Mutex::default(),
        missing_ids: Mutex::default(),
    });
    let activity = Arc::new(Activity::default());
    let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()));
    let clock_now = now.clone();
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        radio_sources: settings.unwrap_or(RadioSourceSettings {
            sonic_url: "http://sonic.test".into(),
            sonic_pause_seconds: 0,
            ..Default::default()
        }),
        ..Default::default()
    }));
    let worker = SonicAnalysisWorker::new(
        store.clone(),
        transitions.clone(),
        sidecar.clone(),
        library.clone(),
        activity.clone(),
        settings,
        None,
        Clock::new(move || *clock_now.lock()),
    );
    T {
        _dir: dir,
        worker,
        store,
        transitions,
        sidecar,
        library,
        activity,
        now,
    }
}

fn analyzed_song() -> SonicSong {
    SonicSong {
        stamp: "1:1".into(),
        version: 2,
        f: vec![0.0; 23],
        ..Default::default()
    }
}

#[tokio::test]
async fn it_holds_when_off_paused_down_or_busy() {
    let off = make(Some(RadioSourceSettings {
        sounds_alike: false,
        ..Default::default()
    }));
    off.tick().await;
    assert_eq!(off.worker.status().state, "Off");

    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.worker.set_paused(true);
    t.tick().await;
    assert_eq!(t.worker.status().state, "Paused");

    t.worker.set_paused(false);
    *t.sidecar.version.lock() = None;
    t.tick().await;
    assert_eq!(
        t.state(),
        (
            "Waiting".to_string(),
            Some("octo-sonic is not answering.".to_string())
        )
    );

    *t.sidecar.version.lock() = Some(2);
    t.activity.0.store(true, Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.state().1.as_deref(), Some("Waiting for a download to finish."));
    assert!(t.sidecar.analysed().is_empty());
}

#[tokio::test]
async fn without_a_navidrome_sign_in_it_says_so() {
    let t = make(None);
    t.library.listable.store(false, Ordering::SeqCst);
    t.tick().await;
    assert!(
        t.state()
            .1
            .unwrap()
            .contains("Octo needs its own Navidrome sign-in")
    );
}

#[tokio::test]
async fn it_analyzes_each_song_once_then_finishes_the_pass() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);

    t.tick().await;
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "b.flac"]);
    assert_eq!(t.store.read(|s| s.songs["a"].f.len()), 23);
    assert_eq!(t.store.read(|s| s.songs["a"].version), 2);

    let lookups = t.library.file_lookups.load(Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.worker.status().state, "Done");
    assert_eq!(t.store.read(|s| s.pass), 1);
    // Analyzed songs at the same size are passed over without asking for their files.
    assert_eq!(t.library.file_lookups.load(Ordering::SeqCst), lookups);
    assert_eq!(t.sidecar.analysed().len(), 2);
}

#[tokio::test]
async fn a_new_feature_version_is_analyzed_again() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    t.tick().await;
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));

    *t.sidecar.version.lock() = Some(3);
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "a.flac"]);
    assert_eq!(t.store.read(|s| s.songs["a"].version), 3);
}

#[tokio::test]
async fn an_unreadable_file_is_remembered_and_tried_again_a_week_later() {
    let t = make(None);
    t.library.add("bad", "bad.flac", Some(200), true);
    t.sidecar
        .errors
        .lock()
        .insert("bad.flac".into(), "empty or too short song".into());

    t.tick().await;
    assert_eq!(
        t.store.read(|s| s.failed["bad"].error.clone()),
        "empty or too short song"
    );
    let lookups = t.library.file_lookups.load(Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.sidecar.analysed().len(), 1);
    assert_eq!(t.library.file_lookups.load(Ordering::SeqCst), lookups);

    t.advance(RETRY_FAILED_AFTER + TimeDelta::hours(1));
    t.tick().await;
    t.tick().await;
    assert_eq!(t.sidecar.analysed().len(), 2);
}

#[tokio::test]
async fn a_file_that_drops_the_connection_twice_is_marked_unreadable_and_the_pass_moves_on() {
    let t = make(None);
    t.library.add("crash", "crash.flac", Some(200), true);
    t.library.add("fine", "fine.flac", Some(200), true);
    t.sidecar.drops.lock().insert("crash.flac".into());

    t.tick().await;
    assert_eq!(
        t.state().1.as_deref(),
        Some("octo-sonic stopped answering; trying again.")
    );
    assert!(t.store.read(|s| s.failed.is_empty()));

    t.tick().await;
    assert_eq!(
        t.store.read(|s| s.failed["crash"].error.clone()),
        "octo-sonic stopped while reading it"
    );

    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["crash.flac", "crash.flac", "fine.flac"]);
}

#[tokio::test]
async fn a_song_that_just_arrived_goes_first_even_if_the_pass_has_not_listed_it() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);
    t.tick().await;

    let arrival = t.library.add("new", "new.flac", Some(200), false);
    t.library.arrivals.lock().insert("new".into(), arrival);
    t.worker.arrived_first("new");
    t.tick().await;

    assert_eq!(t.sidecar.analysed(), ["a.flac", "new.flac"]);
}

#[tokio::test]
async fn a_long_mix_is_never_read() {
    let t = make(None);
    t.library.add("mix", "mix.flac", Some(2 * 3600), true);
    t.tick().await;
    assert!(t.sidecar.analysed().is_empty());
    assert_eq!(t.worker.status().state, "Done");
}

#[tokio::test]
async fn at_the_pass_end_gone_songs_are_forgotten_and_start_over_clears_everything() {
    let t = make(None);
    t.store.write(|s| {
        s.songs.insert("gone".into(), analyzed_song());
    });
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    t.tick().await;
    assert!(!t.store.read(|s| s.songs.contains_key("gone")));
    assert!(t.store.read(|s| s.songs.contains_key("a")));

    t.worker.reset();
    assert!(t.store.read(|s| s.songs.is_empty()));
    assert_eq!(t.store.read(|s| s.pass), 0);
}

#[tokio::test]
async fn a_page_that_fails_forgets_nothing() {
    let t = make(None);
    for i in 0..PAGE_SIZE + 5 {
        t.library.add_listed(&format!("s{i}"));
    }
    t.store.write(|s| {
        s.songs.insert(format!("s{}", PAGE_SIZE + 2), analyzed_song());
    });
    *t.library.failing_page.lock() = Some(PAGE_SIZE);

    t.tick().await;
    assert_eq!(t.worker.status().state, "Waiting");
    assert_eq!(t.store.read(|s| s.songs.len()), 1);
    assert_eq!(t.store.read(|s| s.pass), 0);
}

#[tokio::test]
async fn a_page_with_a_missing_song_is_not_the_end_of_the_list() {
    let t = make(None);
    for i in 0..PAGE_SIZE + 5 {
        t.library.add_listed(&format!("s{i}"));
    }
    t.library.missing_ids.lock().insert("s3".into());
    let later = format!("s{}", PAGE_SIZE + 2);
    t.store.write(|s| {
        s.songs.insert(later.clone(), analyzed_song());
    });

    t.ticks_until_done().await;
    assert!(
        t.store.read(|s| s.songs.contains_key(&later)),
        "a song on the second page is still known"
    );
    // Every listed row has no file: left out, and counted.
    assert_eq!(t.worker.status().skipped, PAGE_SIZE + 4);
}

#[tokio::test]
async fn a_song_that_arrived_during_a_pass_is_kept_at_its_end() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    let arrival = t.library.add("new", "new.flac", Some(200), false);
    t.library.arrivals.lock().insert("new".into(), arrival);
    t.worker.arrived_first("new");

    t.ticks_until_done().await;
    assert!(t.store.read(|s| s.songs.contains_key("new")));
}

#[tokio::test]
async fn a_song_that_arrives_between_passes_is_read_without_a_new_pass() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.ticks_until_done().await;
    let arrival = t.library.add("new", "new.flac", Some(200), false);
    t.library.arrivals.lock().insert("new".into(), arrival);
    t.worker.arrived_first("new");

    t.tick().await;
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "new.flac"]);
    assert_eq!(t.store.read(|s| s.pass), 1);
    assert_eq!(t.worker.status().state, "Done");
}

#[tokio::test]
async fn turned_off_in_env_it_is_off_not_waiting_or_failing() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.sidecar.off.store(true, Ordering::SeqCst);
    t.tick().await;
    assert_eq!(
        t.state(),
        ("Off".to_string(), Some(TURNED_OFF_IN_ENV.to_string()))
    );
    assert!(t.sidecar.analysed().is_empty());
    assert!(t.store.read(|s| s.failed.is_empty()));
}

#[tokio::test]
async fn when_octo_sonic_cannot_see_the_music_it_waits_and_blames_no_song() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.sidecar.blind.store(true, Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.state().1.as_deref(), Some(CANNOT_SEE_MUSIC));
    assert!(t.sidecar.analysed().is_empty());

    // It sees a folder, but not Octo's: every file is missing to it.
    t.sidecar.blind.store(false, Ordering::SeqCst);
    t.sidecar.missing.lock().insert("a.flac".into());
    t.tick().await;
    assert_eq!(t.state().1.as_deref(), Some(CANNOT_SEE_MUSIC));
    assert!(t.store.read(|s| s.failed.is_empty()));

    t.sidecar.missing.lock().clear();
    t.tick().await;
    assert!(t.store.read(|s| s.songs.contains_key("a")));
}

#[tokio::test]
async fn an_answer_octo_cannot_read_is_that_songs_failure() {
    let t = make(None);
    t.library.add("odd", "odd.flac", Some(200), true);
    t.library.add("fine", "fine.flac", Some(200), true);
    t.sidecar.garbled.lock().insert("odd.flac".into());
    t.ticks_until_done().await;
    assert!(t.store.read(|s| s.failed["odd"].error.contains("could not read")));
    assert!(t.store.read(|s| s.songs.contains_key("fine")));
}

#[tokio::test]
async fn a_song_with_no_length_is_left_out_and_counted() {
    let t = make(None);
    t.library.add("zero", "zero.flac", Some(0), true);
    t.library.add("none", "none.flac", None, true);
    t.library.add("fine", "fine.flac", Some(200), true);
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.analysed(), ["fine.flac"]);
}

#[tokio::test]
async fn pause_and_start_over_are_saved_at_once() {
    let t = make(None);
    t.worker.set_paused(true);
    let path = t.store.path().to_path_buf();
    assert!(
        std::fs::read_to_string(&path)
            .expect("written")
            .contains("\"Paused\":true")
    );
    t.worker.reset();
    assert!(
        std::fs::read_to_string(&path)
            .expect("written")
            .contains("\"Pass\":0")
    );
}

/// Before its first look the analysis says it is about to start, and the status names what the
/// pass is reading.
#[tokio::test]
async fn before_its_first_look_it_says_it_is_about_to_start() {
    let t = make(None);
    assert_eq!(
        t.state(),
        (
            "Waiting".to_string(),
            Some("Starts a couple of minutes after Octo does.".to_string())
        )
    );
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);
    t.tick().await;
    let status = t.worker.status();
    assert_eq!(
        (status.state.as_str(), status.analysed, status.total),
        ("Running", 1, 2)
    );
    assert_eq!(
        serde_json::to_value(&status).unwrap()["analysed"],
        serde_json::json!(1)
    );
}

// ---- transition profiles ----------------------------------------------------------------

fn sound_and_profile() -> SonicParts {
    SonicParts {
        sound: true,
        transition: true,
    }
}

fn profile_only() -> SonicParts {
    SonicParts {
        sound: false,
        transition: true,
    }
}

fn profile_of(t: &T, key: &str) -> Option<serde_json::Value> {
    match t.transitions.lookup(key) {
        TransitionLookup::Ready(profile) => Some(profile),
        TransitionLookup::NotReady => None,
    }
}

#[tokio::test]
async fn one_reading_keeps_a_songs_sound_and_its_profile() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    assert_eq!(*t.sidecar.parts.lock(), [sound_and_profile()]);
    assert_eq!(t.store.read(|s| s.songs["a"].f.len()), 23);
    assert_eq!(
        profile_of(&t, "a").map(|p| p["file"].clone()),
        Some("a.flac".into())
    );
    assert_eq!(t.transitions.current_version(), Some(1));

    let lookups = t.library.file_lookups.load(Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.worker.status().state, "Done");
    // Both kept at this size: passed over without asking for the file.
    assert_eq!(t.library.file_lookups.load(Ordering::SeqCst), lookups);
    assert_eq!(t.sidecar.analysed().len(), 1);
}

#[tokio::test]
async fn an_octo_sonic_from_before_profiles_is_asked_for_the_sound_alone() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    assert_eq!(*t.sidecar.parts.lock(), [SonicParts::SOUND]);
    assert_eq!(profile_of(&t, "a"), None);
    assert_eq!(t.transitions.current_version(), None);
}

#[tokio::test]
async fn a_song_read_before_profiles_is_read_again_for_its_profile_alone() {
    let t = make(None);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    t.tick().await;
    assert_eq!(t.worker.status().state, "Done");
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));

    // octo-sonic is updated and now makes profiles.
    *t.sidecar.transition.lock() = Some(1);
    t.tick().await;
    assert_eq!(*t.sidecar.parts.lock(), [SonicParts::SOUND, profile_only()]);
    assert!(profile_of(&t, "a").is_some());
    // The features it had are kept as they were.
    assert_eq!(t.store.read(|s| s.songs["a"].f.len()), 23);
    assert!(t.store.read(|s| s.failed.is_empty()));
}

#[tokio::test]
async fn a_new_profile_version_is_read_again_and_the_old_one_is_not_served() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    t.tick().await;
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));

    *t.sidecar.transition.lock() = Some(2);
    t.library.listable.store(false, Ordering::SeqCst);
    t.tick().await;
    // Until it is read again, the old profile is not handed out.
    assert_eq!(profile_of(&t, "a"), None);
    t.library.listable.store(true, Ordering::SeqCst);
    t.tick().await;
    assert_eq!(t.sidecar.parts.lock().last(), Some(&profile_only()));
    assert_eq!(profile_of(&t, "a").map(|p| p["version"].clone()), Some(2.into()));
}

#[tokio::test]
async fn an_outside_song_that_lands_gets_its_profile_under_its_key_first() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    let landed = t.library.directory.join("landed.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    let landed = landed.to_string_lossy().into_owned();
    let arrivals = DownloadArrivals::default();
    t.worker.listen_to(&arrivals);
    arrivals.arrived("youtube:abc", &landed);

    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["landed.mp3"]);
    assert_eq!(*t.sidecar.parts.lock(), [profile_only()]);
    assert!(profile_of(&t, "youtube:abc").is_some());
    assert_eq!(
        t.transitions.entry("youtube:abc").and_then(|e| e.path),
        Some(landed.clone())
    );
    // Nothing of it goes into the library's sounds.
    assert!(t.store.read(|s| !s.songs.contains_key("youtube:abc")));

    // The same file landing again is not read again.
    arrivals.arrived("youtube:abc", &landed);
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["landed.mp3", "a.flac"]);
}

#[tokio::test]
async fn an_outside_song_between_passes_is_read_without_a_new_pass() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.ticks_until_done().await;
    let landed = t.library.directory.join("landed.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    t.worker.arrived_outside("outside:xyz", &landed.to_string_lossy());
    t.tick().await;
    assert!(profile_of(&t, "outside:xyz").is_some());
    assert_eq!(t.store.read(|s| s.pass), 1);
}

#[tokio::test]
async fn an_outside_song_octo_sonic_cannot_see_blames_nothing_and_moves_on() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let landed = t.library.directory.join("cached.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    t.sidecar.missing.lock().insert("cached.mp3".into());
    t.library.add("a", "a.flac", Some(200), true);
    t.worker.arrived_outside("youtube:abc", &landed.to_string_lossy());
    t.tick().await;
    assert_eq!(t.worker.status().state, "Running");
    t.tick().await;
    assert_eq!(t.sidecar.analysed(), ["cached.mp3", "a.flac"]);
    assert!(t.store.read(|s| s.failed.is_empty()));
}

#[tokio::test]
async fn an_outside_song_waits_while_octo_sonic_makes_no_profiles() {
    let t = make(None);
    let landed = t.library.directory.join("landed.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    t.worker.arrived_outside("youtube:abc", &landed.to_string_lossy());
    t.ticks_until_done().await;
    assert!(t.sidecar.analysed().is_empty());
}

#[tokio::test]
async fn at_the_pass_end_gone_songs_lose_their_profiles() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.transitions
        .put("gone", "1:1", None, &serde_json::json!({ "version": 1 }));
    t.library.add("a", "a.flac", Some(200), true);
    t.ticks_until_done().await;
    assert!(t.transitions.entry("gone").is_none());
    assert!(profile_of(&t, "a").is_some());
}

// ---- profiles with Sounds alike off -----------------------------------------------------

fn sounds_alike_off() -> RadioSourceSettings {
    RadioSourceSettings {
        sounds_alike: false,
        sonic_url: "http://sonic.test".into(),
        sonic_pause_seconds: 0,
        ..Default::default()
    }
}

#[tokio::test]
async fn with_sounds_alike_off_on_the_radio_page_every_song_still_gets_its_profile() {
    let t = make(Some(sounds_alike_off()));
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.library.add("b", "b.flac", Some(200), true);
    t.tick().await;
    assert_eq!(
        t.state(),
        ("Running".to_string(), Some(PROFILES_ONLY_RADIO_PAGE.to_string()))
    );
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.analysed(), ["a.flac", "b.flac"]);
    assert!(t.sidecar.parts.lock().iter().all(|&p| p == profile_only()));
    assert!(profile_of(&t, "a").is_some() && profile_of(&t, "b").is_some());
    // No bliss work: not a song's sound is kept, and none is counted as failed.
    assert!(t.store.read(|s| s.songs.is_empty() && s.failed.is_empty()));
    let status = t.worker.status();
    assert_eq!(
        (status.profiled, status.analysed, status.reads_sound),
        (2, 0, false)
    );
    assert_eq!(status.reason.as_deref(), Some(DONE_PROFILES));
    assert_eq!(t.transitions.current_version(), Some(1));
}

#[tokio::test]
async fn with_sounds_alike_off_in_env_only_profiles_are_asked_for() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    t.sidecar.profiles_only.store(true, Ordering::SeqCst);
    t.library.add("a", "a.flac", Some(200), true);
    t.tick().await;
    assert_eq!(
        t.state(),
        ("Running".to_string(), Some(PROFILES_ONLY_IN_ENV.to_string()))
    );
    t.ticks_until_done().await;
    assert_eq!(*t.sidecar.parts.lock(), [profile_only()]);
    assert!(profile_of(&t, "a").is_some());
    assert!(t.store.read(|s| s.songs.is_empty() && s.failed.is_empty()));
}

#[tokio::test]
async fn with_sounds_alike_off_an_outside_song_that_lands_still_gets_its_profile() {
    let t = make(Some(sounds_alike_off()));
    *t.sidecar.transition.lock() = Some(1);
    let landed = t.library.directory.join("landed.mp3");
    std::fs::write(&landed, b"some sound").expect("writes");
    t.worker.arrived_outside("youtube:abc", &landed.to_string_lossy());
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.analysed(), ["landed.mp3"]);
    assert!(profile_of(&t, "youtube:abc").is_some());
}

#[tokio::test]
async fn turning_sounds_alike_on_reads_the_sound_of_songs_that_have_only_a_profile() {
    let t = make(Some(sounds_alike_off()));
    *t.sidecar.transition.lock() = Some(1);
    t.library.add("a", "a.flac", Some(200), true);
    t.ticks_until_done().await;
    assert!(t.store.read(|s| s.songs.is_empty()));

    let settings = t.worker.settings.clone();
    let mut on = (*settings.current()).clone();
    on.radio_sources.sounds_alike = true;
    settings.set(on);
    // Without waiting out the pass interval.
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.parts.lock().last(), Some(&SonicParts::SOUND));
    assert!(t.store.read(|s| s.songs.contains_key("a")));
}

// ---- published tempo ---------------------------------------------------------------------

/// A catalog's tempos by ISRC, counting what it is asked.
#[derive(Default)]
struct Catalog {
    tempos: Mutex<HashMap<String, f64>>,
    asked: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl IPublishedTempo for Catalog {
    async fn bpm(&self, isrcs: &[String]) -> Option<f64> {
        self.asked.lock().push(isrcs.to_vec());
        let tempos = self.tempos.lock();
        isrcs
            .iter()
            .filter_map(|isrc| octo_core::common::SongIdentity::normalize_isrc(isrc))
            .find_map(|isrc| tempos.get(&isrc).copied())
    }
}

fn with_catalog(t: &T) -> Arc<Catalog> {
    let catalog = Arc::new(Catalog::default());
    catalog.tempos.lock().insert("GBAAA9800001".into(), 77.1);
    t.worker.use_published_tempo(catalog.clone());
    catalog
}

fn listed_with(t: &T, id: &str, bpm: Option<i32>, isrcs: &[&str]) {
    let mut row = t.library.add(id, &format!("{id}.flac"), Some(200), false);
    row.bpm = bpm;
    row.isrcs = isrcs.iter().map(|isrc| isrc.to_string()).collect();
    t.library.rows.lock().push(row);
}

#[tokio::test]
async fn a_profile_is_handed_the_files_bpm_tag_without_asking_the_catalog() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", Some(128), &["GBAAA9800001"]);
    t.tick().await;
    assert_eq!(*t.sidecar.bpms.lock(), [Some(128.0)]);
    assert!(catalog.asked.lock().is_empty());
}

#[tokio::test]
async fn without_a_bpm_tag_the_catalogs_tempo_for_the_isrc_goes_with_the_profile() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", None, &["GBAAA9800001"]);
    listed_with(&t, "b", None, &["USAAA0000009"]);
    t.ticks_until_done().await;
    assert_eq!(*t.sidecar.bpms.lock(), [Some(77.1), None]);
    assert_eq!(catalog.asked.lock().len(), 2);
}

#[tokio::test]
async fn a_song_with_neither_a_bpm_tag_nor_an_isrc_asks_nothing() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", None, &[]);
    t.tick().await;
    assert_eq!(*t.sidecar.bpms.lock(), [None]);
    assert!(catalog.asked.lock().is_empty());
}

#[tokio::test]
async fn a_reading_of_the_sound_alone_asks_for_no_tempo() {
    // An octo-sonic from before profiles.
    let t = make(None);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", Some(128), &["GBAAA9800001"]);
    listed_with(&t, "b", None, &["GBAAA9800001"]);
    t.ticks_until_done().await;
    assert_eq!(*t.sidecar.bpms.lock(), [None, None]);
    assert!(catalog.asked.lock().is_empty());
}

#[tokio::test]
async fn a_song_already_profiled_asks_for_no_tempo() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    listed_with(&t, "a", None, &["GBAAA9800001"]);
    t.ticks_until_done().await;
    t.advance(PASS_INTERVAL + TimeDelta::minutes(1));
    t.ticks_until_done().await;
    assert_eq!(t.sidecar.analysed().len(), 1);
    assert_eq!(catalog.asked.lock().len(), 1);
}

#[tokio::test]
async fn an_outside_song_takes_its_tempo_and_isrc_from_its_own_tags() {
    let t = make(None);
    *t.sidecar.transition.lock() = Some(1);
    let catalog = with_catalog(&t);
    let tagged = |name: &str, bpm: Option<u32>, isrc: Option<&str>| {
        let path = t.library.directory.join(name);
        std::fs::write(&path, crate::services::test_support::flac()).expect("written");
        let mut file = octo_media::tags::TagFile::open(&path).expect("opens");
        if let Some(bpm) = bpm {
            file.set_bpm(bpm);
        }
        octo_media::tags::tag_writer_extras::set_text(&mut file, octo_media::tags::TagFields::ISRC, isrc);
        file.save().expect("saved");
        path.to_string_lossy().into_owned()
    };
    let with_tag = tagged("tagged.flac", Some(96), Some("GBAAA9800001"));
    let with_isrc = tagged("isrc.flac", None, Some("GB-AAA-98-00001"));
    let bare = tagged("bare.flac", None, None);
    t.worker.arrived_outside("outside:1", &with_tag);
    t.worker.arrived_outside("outside:2", &with_isrc);
    t.worker.arrived_outside("outside:3", &bare);
    for _ in 0..3 {
        t.tick().await;
    }
    assert_eq!(*t.sidecar.bpms.lock(), [Some(96.0), Some(77.1), None]);
    assert_eq!(*catalog.asked.lock(), [vec!["GB-AAA-98-00001".to_string()]]);
    assert_eq!(
        file_tempo_tags(&with_tag),
        (Some(96), vec!["GBAAA9800001".to_string()])
    );
}

#[test]
fn navidromes_bpm_of_0_is_no_bpm_tag() {
    let root = serde_json::json!([
        { "id": "a", "path": "A/a.flac", "bpm": 124 },
        { "id": "b", "path": "B/b.flac", "bpm": 0 },
        { "id": "c", "path": "C/c.flac" },
    ]);
    let (rows, _) = crate::services::library::quality_upgrade_worker::parse_songs(&root)
        .expect("read")
        .expect("rows");
    let bpms: Vec<Option<i32>> = rows.iter().map(|row| row.bpm).collect();
    assert_eq!(bpms, [Some(124), None, None]);
}
