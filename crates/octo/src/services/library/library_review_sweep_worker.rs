//! The library Review sweep (#72), its state (`<config>/review-sweep.json`) and the fingerprint
//! check it asks.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use futures::FutureExt;
use octo_core::common::Clock;
use octo_core::common::dotnet;
use octo_core::fingerprint::verification::{InconclusiveReason, VerificationResult, VerificationVerdict};
use octo_core::json::datetime;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::models::domain::Song;
use octo_core::settings::{LibraryActionSettings, SettingsStore};
use octo_media::tags::TagFile;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::navidrome_song_path_resolver::get_full_path;
use super::notice_queue::{NoticeOrigin, NoticeQueue, set_aside};
use super::quality_upgrade_worker::compare_ordinal;
use crate::services::common::IAcquisitionActivity;
use crate::services::family::{LibraryScope, ScopeResolver, ScopeView};
use crate::services::fingerprint::AcoustIdRateLimiter;
use crate::services::framework::DotnetDictionary;
use crate::services::library::library_walk;
use crate::services::local::ILocalLibraryService;
use crate::services::soulseek::soulseek_download_service::excluded_folder_names;
use crate::services::state_file;

/// The one question the sweep asks of a file, behind a seam so tests need no fpcalc.
#[async_trait]
pub trait IReviewSweepVerifier: Send + Sync {
    fn is_ready(&self) -> bool;
    async fn verify(&self, path: &str, artist: Option<&str>, title: Option<&str>) -> VerificationResult;
}

/// What the sweep needs of `DownloadVerificationService`: whether a lookup can happen
/// (`is_fingerprinting_enabled`) and `verify(path, artist, title)` with no ISRC and `refuse_live`
/// off.
///
/// `DownloadVerificationService` implements it; `AppState::build` hands it to the
/// [`FingerprintSweepVerifier`] with `set_service`.
#[async_trait]
pub trait SweepVerification: Send + Sync {
    fn is_fingerprinting_enabled(&self) -> bool;
    async fn verify(&self, path: &str, artist: Option<&str>, title: Option<&str>) -> VerificationResult;
}

/// The real check, in AcoustID's background lane. Resolved late, because the workers are built
/// before the download service is first used: the service is set once it exists, and
/// until then the sweep is not ready.
#[derive(Default)]
pub struct FingerprintSweepVerifier {
    service: OnceLock<Arc<dyn SweepVerification>>,
}

impl FingerprintSweepVerifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// The verification service, once built. A second call is ignored.
    pub fn set_service(&self, service: Arc<dyn SweepVerification>) {
        let _ = self.service.set(service);
    }
}

#[async_trait]
impl IReviewSweepVerifier for FingerprintSweepVerifier {
    fn is_ready(&self) -> bool {
        self.service.get().is_some_and(|s| s.is_fingerprinting_enabled())
    }

    // No ISRC: the file's own tag held against itself proves nothing, and WithTaggedIsrc would
    // turn a lookup AcoustID never answered into a confirmation.
    async fn verify(&self, path: &str, artist: Option<&str>, title: Option<&str>) -> VerificationResult {
        match self.service.get() {
            Some(service) => AcoustIdRateLimiter::in_background(service.verify(path, artist, title)).await,
            None => VerificationResult::inconclusive(),
        }
    }
}

fn one() -> i32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ReviewSweepState {
    #[serde(default)]
    pub paused: bool,
    /// Library-relative path of the last file dealt with this pass; empty at its start.
    #[serde(default, deserialize_with = "state_file::null_as_default")]
    pub cursor: String,
    /// Library-relative path to the size and modified time it had when checked. Relative, so a
    /// mount moving does not forget everything; stamped, so a replaced file is checked again.
    #[serde(default, with = "super::state_dictionary")]
    pub checked: DotnetDictionary<String>,
    #[serde(default = "one")]
    pub pass: i32,
    #[serde(default)]
    pub total: i32,
    #[serde(default)]
    pub found: i32,
    #[serde(default)]
    pub fine: i32,
    #[serde(default)]
    pub undecodable: i32,
    #[serde(default, with = "datetime::utc_option")]
    pub last_checked_utc: Option<DateTime<Utc>>,
    #[serde(default, with = "datetime::utc_option")]
    pub pass_finished_utc: Option<DateTime<Utc>>,
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub next_pass_utc: DateTime<Utc>,
}

impl Default for ReviewSweepState {
    fn default() -> Self {
        ReviewSweepState {
            paused: false,
            cursor: String::new(),
            checked: DotnetDictionary::new(),
            pass: 1,
            total: 0,
            found: 0,
            fine: 0,
            undecodable: 0,
            last_checked_utc: None,
            pass_finished_utc: None,
            next_pass_utc: datetime::min_value(),
        }
    }
}

struct StoreInner {
    state: ReviewSweepState,
    dirty: bool,
}

/// A hook run between taking a snapshot and writing it (a test seam).
pub type BeforeWrite = Arc<dyn Fn() + Send + Sync>;

/// review-sweep.json. [`ReviewSweepState::checked`] holds a line per library file, so the file
/// is written at most every [`ReviewSweepStore::FLUSH_INTERVAL`], not on every change: at 50,000
/// files a write per song would rewrite hundreds of megabytes an hour.
///
/// [`ReviewSweepStore::run_flusher`] flushes every 5 s (a worker, which flushes once more on
/// shutdown), and `Drop` flushes what is left.
pub struct ReviewSweepStore {
    path: Option<PathBuf>,
    clock: Clock,
    inner: Mutex<StoreInner>,
    flush_lock: Mutex<()>,
    /// When the state last went to disk; None before the first write.
    last_write: Mutex<Option<DateTime<Utc>>>,
    before_write: Mutex<Option<BeforeWrite>>,
}

impl ReviewSweepStore {
    pub const FLUSH_INTERVAL: Duration = Duration::from_secs(5);

    /// A store kept in `path` (None keeps it in memory), on the system clock.
    pub fn new(path: Option<PathBuf>) -> Self {
        Self::with_clock(path, Clock::system())
    }

    /// A store kept in `path` on the given clock. A file that cannot be read is set aside as
    /// `<file>.corrupt-<ticks>` and the sweep starts over.
    pub fn with_clock(path: Option<PathBuf>, clock: Clock) -> Self {
        let path = path.filter(|p| !dotnet::is_blank(&p.to_string_lossy()));
        let mut state = ReviewSweepState::default();
        if let Some(path) = &path
            && path.exists()
        {
            match Self::load(path) {
                Ok(loaded) => state = loaded,
                Err(e) => {
                    // Kept aside: losing it only means checking the library again, but it
                    // should be seen.
                    warn!("review sweep state could not be read ({e}); starting over");
                    set_aside(path);
                }
            }
        }
        ReviewSweepStore {
            path,
            clock,
            inner: Mutex::new(StoreInner { state, dirty: false }),
            flush_lock: Mutex::new(()),
            last_write: Mutex::new(None),
            before_write: Mutex::new(None),
        }
    }

    fn load(path: &Path) -> anyhow::Result<ReviewSweepState> {
        let text = state_file::read_all_text(path)?;
        Ok(serde_json::from_str::<Option<ReviewSweepState>>(&text)?.unwrap_or_default())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Test seam: runs between taking a snapshot and writing it.
    pub fn set_before_write(&self, hook: Option<BeforeWrite>) {
        *self.before_write.lock() = hook;
    }

    pub fn read<T>(&self, read: impl FnOnce(&ReviewSweepState) -> T) -> T {
        read(&self.inner.lock().state)
    }

    /// Changes the state in memory. It reaches the disk now if nothing was written in the last
    /// few seconds, otherwise on the next timer tick or [`ReviewSweepStore::flush`].
    pub fn update(&self, change: impl FnOnce(&mut ReviewSweepState)) {
        {
            let mut inner = self.inner.lock();
            change(&mut inner.state);
            inner.dirty = true;
        }
        if self.path.is_none() {
            return;
        }
        let now = self.clock.now();
        let due = self.last_write.lock().is_none_or(|last| {
            now - last >= TimeDelta::from_std(Self::FLUSH_INTERVAL).expect("five seconds")
        });
        if due {
            self.flush();
        }
    }

    /// Writes the state if it changed. The snapshot is taken inside the same lock as the write,
    /// so an older snapshot can never land after a newer one (a Pause overwritten by the tick
    /// before it).
    pub fn flush(&self) -> bool {
        let Some(path) = self.path.as_deref() else {
            return true;
        };
        let _flushing = self.flush_lock.lock();
        let json = {
            let mut inner = self.inner.lock();
            if !inner.dirty {
                return true;
            }
            inner.dirty = false;
            octo_core::json::to_string(&inner.state)
        };
        let hook = self.before_write.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
        *self.last_write.lock() = Some(self.clock.now());
        match state_file::save_atomic(path, &json) {
            Ok(()) => true,
            Err(e) => {
                self.inner.lock().dirty = true;
                warn!("review sweep state could not be written: {e}");
                false
            }
        }
    }

    /// The 5-second flush timer, which picks up a change made just after a write that no later
    /// change may come to flush, and the last flush on shutdown, as a worker.
    pub async fn run_flusher(self: Arc<Self>, token: CancellationToken) -> anyhow::Result<()> {
        let store = self.clone();
        state_file::flush_every(
            Self::FLUSH_INTERVAL,
            token,
            Arc::new(move || {
                store.flush();
            }),
        )
        .await
    }
}

impl Drop for ReviewSweepStore {
    /// The last flush.
    fn drop(&mut self) {
        self.flush();
    }
}

/// What the dashboard shows of the sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewSweepStatus {
    pub state: String,
    pub reason: Option<String>,
    pub paused: bool,
    pub position: usize,
    pub total: i32,
    pub pass: i32,
    pub found: i32,
    pub open: usize,
    pub fine: i32,
    pub undecodable: i32,
    pub keeper: Option<String>,
    pub per_hour: i32,
    pub last_checked_utc: Option<DateTime<Utc>>,
    pub pass_finished_utc: Option<DateTime<Utc>>,
}

/// How one file's check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Fine,
    Ask,
    Undecodable,
    NotFingerprinted,
    LookupFailed,
}

/// The audio files the sweep walks: the same extensions the cover upgrade walks.
const AUDIO_EXTENSIONS: [&str; 14] = crate::services::cover_art::CoverUpgradeWorker::AUDIO_EXTENSIONS;

/// What the sweep is built from, apart from the clock.
pub struct LibraryReviewSweepParts {
    pub store: Arc<ReviewSweepStore>,
    pub notices: Arc<NoticeQueue>,
    pub verifier: Arc<dyn IReviewSweepVerifier>,
    pub activity: Arc<dyn IAcquisitionActivity>,
    pub library: Arc<dyn ILocalLibraryService>,
    /// The library action and Subsonic settings, read at use.
    pub settings: Arc<SettingsStore>,
    /// The resolver's root, the same one review actions resolve inside.
    pub music_root: Box<dyn Fn() -> String + Send + Sync>,
}

/// Asks about music that was already in the library (#72), the way Review asks about a download
/// (#47): a few files an hour, only while nothing is downloading, only of whoever keeps the
/// library, and never more than [`LibraryReviewSweepWorker::MAX_OPEN_QUESTIONS`] open at once.
/// It never acts on a file. A confident "this is something else" is a question here, not a
/// deletion.
pub struct LibraryReviewSweepWorker {
    store: Arc<ReviewSweepStore>,
    notices: Arc<NoticeQueue>,
    verifier: Arc<dyn IReviewSweepVerifier>,
    activity: Arc<dyn IAcquisitionActivity>,
    library: Arc<dyn ILocalLibraryService>,
    settings: Arc<SettingsStore>,
    music_root: Box<dyn Fn() -> String + Send + Sync>,
    clock: Clock,

    files: Mutex<Option<Arc<Vec<String>>>>,
    octo_files: Mutex<Arc<HashSet<String>>>,
    generation: AtomicI32,
    lookup_failures: AtomicI32,
    unfingerprinted: AtomicI32,
    hold: Mutex<(String, Option<String>)>,
    /// With Family on, the sweep keeps to the shared library: a member's own songs are never
    /// asked about.
    scope: Option<ScopeResolver>,
}

impl LibraryReviewSweepWorker {
    pub const MAX_OPEN_QUESTIONS: usize = 50;
    pub const MAX_LOOKUP_FAILURES_PER_FILE: i32 = 3;
    pub const MAX_UNFINGERPRINTED_IN_A_ROW: i32 = 5;
    pub const IDLE_CHECK: Duration = Duration::from_secs(60);
    pub const BUSY_CHECK: Duration = Duration::from_secs(30);
    /// After a full pass, how long before the library is walked again for new or changed files.
    pub const PASS_INTERVAL: TimeDelta = TimeDelta::hours(6);

    const DONE: &'static str = "Every song has been checked. New or changed ones are looked for later.";

    pub fn new(parts: LibraryReviewSweepParts, clock: Clock) -> Self {
        LibraryReviewSweepWorker {
            store: parts.store,
            notices: parts.notices,
            verifier: parts.verifier,
            activity: parts.activity,
            library: parts.library,
            settings: parts.settings,
            music_root: parts.music_root,
            clock,
            files: Mutex::new(None),
            octo_files: Mutex::new(Arc::new(HashSet::new())),
            generation: AtomicI32::new(0),
            lookup_failures: AtomicI32::new(0),
            unfingerprinted: AtomicI32::new(0),
            hold: Mutex::new(("Off".to_string(), None)),
            scope: None,
        }
    }

    /// Keeps the sweep to the shared library while Family is on.
    pub fn with_library_scope(mut self, scope: ScopeResolver) -> Self {
        self.scope = Some(scope);
        self
    }

    /// The scopes one pass of the sweep asks: None when every file may be asked about, else
    /// the shared library's while members have libraries of their own (Family on, or turned
    /// off with their libraries still there).
    fn shared_view(&self) -> Option<ScopeView> {
        self.scope
            .as_ref()
            .filter(|scope| scope.keeps_shared())
            .map(ScopeResolver::shared_view)
    }

    /// Whether the sweep may ask about the file at `full`, by the pass's `view`.
    fn in_shared(view: Option<&ScopeView>, full: &str) -> bool {
        view.is_none_or(|view| view.allows(&LibraryScope::Shared, None, Some(full)))
    }

    pub fn store(&self) -> &Arc<ReviewSweepStore> {
        &self.store
    }

    /// The worker loop until the host stops, then a last flush.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        // Never alongside startup, which already works the disk and Navidrome.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(120)) => {}
            _ = stopping.cancelled() => {
                self.stop();
                return Ok(());
            }
        }
        while !stopping.is_cancelled() {
            // Per-tick catch is mandatory: a tick that panics must never stop the worker.
            let tick = tokio::select! {
                tick = std::panic::AssertUnwindSafe(self.tick()).catch_unwind() => tick,
                _ = stopping.cancelled() => break,
            };
            let wait = tick.unwrap_or_else(|_| {
                error!("Library review sweep failed");
                Self::IDLE_CHECK
            });
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = stopping.cancelled() => break,
            }
        }
        self.stop();
        Ok(())
    }

    /// The store writes every few seconds at most; whatever changed since goes down now.
    fn stop(&self) {
        self.store.flush();
    }

    pub async fn tick(&self) -> Duration {
        let settings = self.settings.current().library_actions.clone();
        let per_hour = settings.effective_review_sweep_per_hour();
        if !settings.enabled || !settings.review_enabled || per_hour == 0 {
            return self.hold(
                "Off",
                Some("Set how many songs an hour to check, with library actions and Review on."),
                Self::IDLE_CHECK,
            );
        }
        if self.store.read(|s| s.paused) {
            return self.hold("Paused", Some("Paused from the dashboard."), Self::IDLE_CHECK);
        }
        if !self.verifier.is_ready() {
            return self.hold(
                "Paused",
                Some("AcoustID is not set up: turn on download verification and add a key."),
                Self::IDLE_CHECK,
            );
        }
        let admin = self.settings.current().subsonic.admin_username.clone();
        let Some(keeper) = Self::keeper(&settings, admin.as_deref()) else {
            return self.hold(
                "Paused",
                Some("Nobody is on the library actions allowlist to ask."),
                Self::IDLE_CHECK,
            );
        };
        let open = self.notices.open_count(NoticeOrigin::LibrarySweep);
        if open >= Self::MAX_OPEN_QUESTIONS {
            let reason =
                format!("{open} library questions are waiting in Review. Answer some and it carries on.");
            return self.hold("Paused", Some(&reason), Self::IDLE_CHECK);
        }
        if self.activity.is_busy() {
            return self.hold(
                "Waiting",
                Some("Waiting for a download to finish."),
                Self::BUSY_CHECK,
            );
        }

        let now = self.clock.now();
        if self.files.lock().is_none() && self.store.read(|s| s.cursor.is_empty() && s.next_pass_utc > now) {
            return self.hold("Done", Some(Self::DONE), Self::IDLE_CHECK);
        }

        let generation = self.generation.load(Ordering::SeqCst);
        let root = (self.music_root)();
        if self.files.lock().is_none() {
            if !Path::new(&root).is_dir() {
                return self.hold(
                    "Paused",
                    Some("The music folder cannot be read."),
                    Self::IDLE_CHECK,
                );
            }
            let trash = library_walk::trash_folder(Path::new(&root), &settings);
            let listed = Arc::new(Self::enumerate(&root, Some(&trash)));
            *self.files.lock() = Some(listed.clone());
            let octo_files: HashSet<String> = if settings.review_sweep_octo_downloads {
                HashSet::new()
            } else {
                self.library
                    .get_mappings()
                    .await
                    .into_iter()
                    .map(|mapping| mapping.local_path)
                    .filter(|path| !path.is_empty())
                    .map(|path| get_full_path(&path))
                    .collect()
            };
            *self.octo_files.lock() = Arc::new(octo_files);
            // Start over was pressed while the library was being listed. Keeping this listing
            // would leave the pass with a total of 0, so the next tick lists it again.
            if generation != self.generation.load(Ordering::SeqCst) {
                *self.files.lock() = None;
                return Duration::ZERO;
            }
            let total = i32::try_from(listed.len()).unwrap_or(i32::MAX);
            self.record(generation, |s| s.total = total);
        }

        let Some(files) = self.files.lock().clone() else {
            // Start over was pressed since: list again next tick.
            return Duration::ZERO;
        };
        let octo_files = self.octo_files.lock().clone();
        let scope_view = self.shared_view();
        let reviewed = self.notices.reviewed_paths();
        let cursor = self.store.read(|s| s.cursor.clone());
        let (mut target, mut stamp, mut passed): (Option<String>, Option<String>, String) =
            (None, None, cursor.clone());
        let mut i = first_after(&files, &cursor);
        while i < files.len() && target.is_none() {
            let full = full_path(&root, &files[i]);
            let file_stamp = std::fs::metadata(&full)
                .ok()
                .filter(|m| m.is_file())
                .map(|m| format!("{}:{}", m.len(), modified_ticks(&m)));
            // One entry looked up at a time: copying the whole map each tick costs a line per
            // library file.
            let relative = &files[i];
            let skip = match &file_stamp {
                None => true,
                Some(file_stamp) => {
                    !Self::in_shared(scope_view.as_ref(), &full)
                        || octo_files.contains(&full)
                        || reviewed.contains(&full)
                        || self
                            .store
                            .read(|s| s.checked.get(relative).is_some_and(|seen| seen == file_stamp))
                }
            };
            if skip {
                passed = files[i].clone();
            } else {
                target = Some(files[i].clone());
                stamp = file_stamp;
            }
            i += 1;
        }

        let Some(target) = target else {
            let present: HashSet<&str> = files.iter().map(String::as_str).collect();
            self.record(generation, |s| {
                let gone: Vec<String> = s
                    .checked
                    .keys()
                    .filter(|key| !present.contains(key.as_str()))
                    .cloned()
                    .collect();
                for key in gone {
                    s.checked.remove(&key);
                }
                s.cursor = String::new();
                s.pass += 1;
                s.pass_finished_utc = Some(now);
                s.next_pass_utc = now + Self::PASS_INTERVAL;
            });
            *self.files.lock() = None;
            info!(
                "Library review sweep finished a pass over {} file(s)",
                files.len()
            );
            return self.hold("Done", Some(Self::DONE), Self::IDLE_CHECK);
        };

        let path = full_path(&root, &target);
        let song = Self::read_tags(&path);
        let verdict = self
            .verifier
            .verify(&path, Some(&song.artist), Some(&song.title))
            .await;
        let interval = Duration::from_secs_f64(3600.0 / f64::from(per_hour));
        if generation != self.generation.load(Ordering::SeqCst) {
            return interval;
        }

        let (outcome, question) = Self::classify(verdict);
        match outcome {
            Outcome::LookupFailed
                if self.lookup_failures.fetch_add(1, Ordering::SeqCst) + 1
                    < Self::MAX_LOOKUP_FAILURES_PER_FILE =>
            {
                // The songs skipped on the way here are kept; every other path moves the cursor
                // to the target itself, which is past them, so a tick writes once.
                if passed != cursor {
                    self.record(generation, |s| s.cursor = passed.clone());
                }
                return self.hold(
                    "Waiting",
                    Some("AcoustID did not answer; trying that song again."),
                    interval,
                );
            }
            Outcome::LookupFailed => {
                self.lookup_failures.store(0, Ordering::SeqCst);
                self.record(generation, |s| s.cursor = target.clone());
                return self.hold("Running", None, interval);
            }
            Outcome::NotFingerprinted => {
                self.record(generation, |s| s.cursor = target.clone());
                if self.unfingerprinted.fetch_add(1, Ordering::SeqCst) + 1
                    < Self::MAX_UNFINGERPRINTED_IN_A_ROW
                {
                    return self.hold("Running", None, interval);
                }
                self.unfingerprinted.store(0, Ordering::SeqCst);
                return self.hold(
                    "Paused",
                    Some("fpcalc could not read five songs in a row. Is the music folder readable?"),
                    Duration::from_secs(3600),
                );
            }
            _ => {}
        }

        self.lookup_failures.store(0, Ordering::SeqCst);
        self.unfingerprinted.store(0, Ordering::SeqCst);
        let asked = outcome == Outcome::Ask
            && self
                .notices
                .add_review_from(&keeper, &path, &song, &question, NoticeOrigin::LibrarySweep);
        if asked {
            info!(
                "Asking {keeper} about '{} - {}' from the library: {}",
                song.artist,
                song.title,
                question.reason.name()
            );
        }
        let stamp = stamp.unwrap_or_default();
        self.record(generation, |s| {
            s.cursor = target.clone();
            s.checked.set(target.clone(), stamp);
            s.last_checked_utc = Some(now);
            if asked {
                s.found += 1;
            } else if outcome == Outcome::Undecodable {
                s.undecodable += 1;
            } else {
                s.fine += 1;
            }
        });
        self.hold("Running", None, interval)
    }

    pub fn classify(verdict: VerificationResult) -> (Outcome, VerificationResult) {
        match verdict.verdict {
            VerificationVerdict::Confirmed
                if Self::is_length_off(
                    verdict.duration_seconds,
                    verdict.r#match.as_ref().and_then(|m| m.duration_seconds),
                ) =>
            {
                (
                    Outcome::Ask,
                    VerificationResult {
                        reason: InconclusiveReason::LengthOff,
                        ..verdict
                    },
                )
            }
            VerificationVerdict::Confirmed => (Outcome::Fine, verdict),
            // No recording id is "no decodable audio". On a network mount that can be a read
            // that failed this once, so it is counted and never asked about.
            VerificationVerdict::Mismatch if verdict.recording_id.as_deref().is_none_or(str::is_empty) => {
                (Outcome::Undecodable, verdict)
            }
            VerificationVerdict::Mismatch => (
                Outcome::Ask,
                VerificationResult {
                    reason: InconclusiveReason::SoundsLikeAnother,
                    ..verdict
                },
            ),
            _ if verdict.needs_review() => (Outcome::Ask, verdict),
            _ if verdict.reason == InconclusiveReason::NotFingerprinted => {
                (Outcome::NotFingerprinted, verdict)
            }
            _ => (Outcome::LookupFailed, verdict),
        }
    }

    /// Off by more than 20 seconds or a tenth of the recording, whichever is more. 20 seconds
    /// absorbs fades, pregaps and trailing silence; a tenth still catches a radio edit standing
    /// in for the album cut on a six-minute track, which a wider margin would let through.
    pub fn is_length_off(file_seconds: i32, recording_seconds: Option<i32>) -> bool {
        match recording_seconds {
            Some(expected) if file_seconds > 0 && expected > 0 => {
                f64::from((file_seconds - expected).abs()) > f64::max(20.0, f64::from(expected) / 10.0)
            }
            _ => false,
        }
    }

    /// The Navidrome admin when they may answer, otherwise the first allowed user. One person,
    /// because a library file has no requester to ask instead.
    pub fn keeper(settings: &LibraryActionSettings, admin_username: Option<&str>) -> Option<String> {
        if settings.is_allowed(admin_username) {
            return admin_username.map(|admin| admin.trim().to_string());
        }
        settings
            .allowed_users
            .iter()
            .map(|user| user.trim())
            .find(|user| !user.is_empty())
            .map(str::to_string)
    }

    /// Header only: the fingerprint reads the audio anyway, and a mount is slow.
    pub fn read_tags(path: &str) -> Song {
        let fallback = file_name_without_extension(path);
        match TagFile::open(path) {
            Ok(file) => {
                let performers = file.performers();
                let title = file.title().filter(|title| !dotnet::is_blank(title));
                Song {
                    artist: performers.join("; "),
                    title: title.map_or(fallback, |title| title.trim().to_string()),
                    album: file.album().unwrap_or_default(),
                    ..Default::default()
                }
            }
            Err(_) => Song {
                artist: String::new(),
                title: fallback,
                album: String::new(),
                ..Default::default()
            },
        }
    }

    /// Every audio file under `root`, library-relative with `/`, in ordinal order. A leading dot
    /// is the quarantine and anything else kept out of Navidrome's scan; Octo's staging
    /// folder holds half-written downloads when it sits in the library. A folder that cannot be
    /// read is skipped, and hidden entries, whose names start with a dot, are not walked. The
    /// trash folder is left out by its configured name too, which may have no dot.
    pub fn enumerate(root: &str, trash: Option<&Path>) -> Vec<String> {
        let root = Path::new(root);
        let mut found: Vec<String> = Vec::new();
        let mut folders = vec![root.to_path_buf()];
        while let Some(folder) = folders.pop() {
            let Ok(entries) = std::fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                let is_dir = match entry.file_type() {
                    Ok(kind) if kind.is_symlink() => {
                        std::fs::metadata(&path).is_ok_and(|target| target.is_dir())
                    }
                    Ok(kind) => kind.is_dir(),
                    Err(_) => false,
                };
                if is_dir {
                    if !library_walk::is_trash(&path, trash) {
                        folders.push(path);
                    }
                    continue;
                }
                let extension = extension_of(&name);
                if !AUDIO_EXTENSIONS
                    .iter()
                    .any(|audio| dotnet::eq_ignore_case(audio, &extension))
                {
                    continue;
                }
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                let relative = relative.to_string_lossy().replace('\\', "/");
                // Octo's own staging.
                let incomplete_folders = excluded_folder_names();
                let kept_out = relative.split('/').any(|segment| {
                    segment.starts_with('.')
                        || incomplete_folders
                            .iter()
                            .any(|folder| dotnet::eq_ignore_case(folder, segment))
                });
                if !kept_out {
                    found.push(relative);
                }
            }
        }
        found.sort_by(|a, b| compare_ordinal(a, b));
        found
    }

    fn record(&self, generation: i32, change: impl FnOnce(&mut ReviewSweepState)) {
        self.store.update(|s| {
            if generation == self.generation.load(Ordering::SeqCst) {
                change(s);
            }
        });
    }

    fn hold(&self, state: &str, reason: Option<&str>, wait: Duration) -> Duration {
        *self.hold.lock() = (state.to_string(), reason.map(str::to_string));
        wait
    }

    pub fn set_paused(&self, paused: bool) {
        self.store.update(|s| s.paused = paused);
        self.store.flush();
    }

    pub fn reset(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.files.lock() = None;
        self.store.update(|s| {
            s.cursor = String::new();
            s.checked = DotnetDictionary::new();
            s.pass = 1;
            s.total = 0;
            s.found = 0;
            s.fine = 0;
            s.undecodable = 0;
            s.last_checked_utc = None;
            s.pass_finished_utc = None;
            s.next_pass_utc = datetime::min_value();
        });
        self.store.flush();
    }

    pub fn status(&self) -> ReviewSweepStatus {
        let settings = self.settings.current();
        let files = self.files.lock().clone();
        let (state, reason) = self.hold.lock().clone();
        let keeper = Self::keeper(
            &settings.library_actions,
            settings.subsonic.admin_username.as_deref(),
        );
        self.store.read(|s| ReviewSweepStatus {
            state,
            reason,
            paused: s.paused,
            position: match &files {
                None if s.cursor.is_empty() => 0,
                None => s.checked.len(),
                Some(files) => first_after(files, &s.cursor),
            },
            total: s.total,
            pass: s.pass,
            found: s.found,
            open: self.notices.open_count(NoticeOrigin::LibrarySweep),
            fine: s.fine,
            undecodable: s.undecodable,
            keeper,
            per_hour: settings.library_actions.effective_review_sweep_per_hour(),
            last_checked_utc: s.last_checked_utc,
            pass_finished_utc: s.pass_finished_utc,
        })
    }
}

/// `relative` under `root`, made a full path.
fn full_path(root: &str, relative: &str) -> String {
    get_full_path(&format!("{root}/{relative}"))
}

/// A binary search for the cursor in ordinal order: the index after the cursor, or where it
/// would go.
fn first_after(files: &[String], cursor: &str) -> usize {
    match files.binary_search_by(|file| compare_ordinal(file, cursor)) {
        Ok(index) => index + 1,
        Err(index) => index,
    }
}

/// The file's last write time in 100 ns ticks since 0001-01-01.
fn modified_ticks(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .map(|modified| dotnet_ticks(DateTime::<Utc>::from(modified)))
        .unwrap_or(0)
}

/// The extension of a file name: from its last dot, or nothing.
fn extension_of(name: &str) -> String {
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => name[dot..].to_string(),
        _ => String::new(),
    }
}

/// The file name up to its last dot.
fn file_name_without_extension(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) => name[..dot].to_string(),
        None => name.to_string(),
    }
}

#[cfg(test)]
#[path = "library_review_sweep_tests.rs"]
mod tests;
