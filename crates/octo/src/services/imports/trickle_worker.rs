//! The trickle worker: fetches an import's missing songs a few an hour.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use futures::future::BoxFuture;
use octo_core::common::Clock;
use octo_core::common::acquisition_tracker::user_safe;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::outside::routing::{OutsideRouting, RoutingKind};
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::TrickleJob;
use super::import_models::import_track_states;
use super::trickle_queue::TrickleQueue;
use crate::services::common::acquisition_tracker::{
    AcquisitionEnd, AcquisitionSnapshot, AcquisitionState, AcquisitionTracker, key_of,
};
use crate::services::common::{HeartAcquisitionCoordinator, TrackAcquisitionQueue};
use crate::services::library::LibraryOwnership;
use crate::services::outside::{ExternalIdRegistry, OutsideMetadataService};

/// A song the trickle may not fetch for this person now (a family member whose library is full,
/// or who only saves songs), with the words they read. The job is skipped rather than failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TrickleHeld(pub String);

/// What the trickle is doing, in a word the dashboard and the apps know, and when it starts its
/// next song.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrickleStatus {
    pub state: &'static str,
    pub next_utc: Option<DateTime<Utc>>,
    pub per_hour: i32,
}

/// The trickle's words for [`TrickleStatus::state`]. A contract with the dashboard and the apps.
pub mod trickle_states {
    /// Nothing queued.
    pub const IDLE: &str = "idle";
    /// Working, or waiting for its next turn.
    pub const RUNNING: &str = "running";
    /// The asker paused theirs.
    pub const PAUSED: &str = "paused";
    /// Songs per hour is 0.
    pub const OFF: &str = "off";
    /// Someone's own downloads come first.
    pub const YIELDING: &str = "yielding";
}

/// A song the library already has: its id there, when Navidrome said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSong(pub Option<String>);

pub type OwnedSeam =
    Arc<dyn Fn(TrickleJob) -> BoxFuture<'static, anyhow::Result<Option<OwnedSong>>> + Send + Sync>;
pub type StartSeam = Arc<dyn Fn(TrickleJob) -> BoxFuture<'static, anyhow::Result<String>> + Send + Sync>;
pub type RowSeam = Arc<dyn Fn(&str) -> Option<AcquisitionSnapshot> + Send + Sync>;
pub type BusySeam = Arc<dyn Fn() -> bool + Send + Sync>;
pub type FinishedListener = Arc<dyn Fn(&TrickleJob) + Send + Sync>;

/// Seams, so the pace and the outcomes can be tested without downloading anything.
#[derive(Clone)]
pub struct TrickleSeams {
    pub owned: OwnedSeam,
    pub start: StartSeam,
    pub row: RowSeam,
    pub people_downloading: BusySeam,
    pub clock: Clock,
}

impl Default for TrickleSeams {
    /// A worker with no services: nothing owned, no downloads set up.
    fn default() -> Self {
        TrickleSeams {
            owned: Arc::new(|_| Box::pin(async { Ok(None) })),
            start: Arc::new(|_| {
                Box::pin(async { Err(anyhow::anyhow!("Downloads are not set up on this server.")) })
            }),
            row: Arc::new(|_| None),
            people_downloading: Arc::new(|| false),
            clock: Clock::system(),
        }
    }
}

/// Works through the missing songs a few an hour, through the same chain a heart takes: the
/// library check first (a song already there is done, never downloaded twice), then the heart
/// sources in their order, Lidarr, parallel downloads and all, so an import is fetched exactly
/// the way a heart would be. It never queues ahead of a person: while anyone's own download
/// runs, it waits.
///
/// One song at a time, started no closer together than the songs-per-hour setting allows. A song
/// handed to Lidarr is followed until Lidarr's import lands, without holding the next one back.
pub struct TrickleWorker {
    queue: Arc<TrickleQueue>,
    settings: Arc<SettingsStore>,
    seams: Mutex<TrickleSeams>,
    running: Mutex<Option<JoinHandle<()>>>,
    /// The song whose chain is running now, set before it starts and cleared once the chain
    /// returns.
    current: Mutex<Option<(String, String)>>,
    waiting_for: Mutex<&'static str>,
    finished: Mutex<Vec<FinishedListener>>,
}

/// The services the real trickle fetches with.
pub struct TrickleServices {
    pub owned: Arc<LibraryOwnership>,
    pub ids: Arc<ExternalIdRegistry>,
    pub hearts: Arc<HeartAcquisitionCoordinator>,
    pub downloads: Arc<TrackAcquisitionQueue>,
    pub tracker: Arc<AcquisitionTracker>,
}

impl TrickleWorker {
    pub const TICK_EVERY: Duration = Duration::from_secs(5);
    /// A hand-off nobody reports on is given up after this long.
    pub const GIVE_UP_AFTER: TimeDelta = TimeDelta::hours(12);
    pub const MAX_ATTEMPTS: i32 = 3;

    /// With the given seams, as the tests build it.
    pub fn new(queue: Arc<TrickleQueue>, settings: Arc<SettingsStore>, seams: TrickleSeams) -> Arc<Self> {
        Arc::new(TrickleWorker {
            queue,
            settings,
            seams: Mutex::new(seams),
            running: Mutex::new(None),
            current: Mutex::new(None),
            waiting_for: Mutex::new(trickle_states::IDLE),
            finished: Mutex::new(Vec::new()),
        })
    }

    /// Over the real heart chain, following the tracker's rows as they end.
    pub fn wired(
        queue: Arc<TrickleQueue>,
        settings: Arc<SettingsStore>,
        services: TrickleServices,
    ) -> Arc<Self> {
        let TrickleServices {
            owned,
            ids,
            hearts,
            downloads,
            tracker,
        } = services;
        let start_queue = queue.clone();
        let start_tracker = tracker.clone();
        let row_tracker = tracker.clone();
        let seams = TrickleSeams {
            owned: Arc::new(move |job| {
                let owned = owned.clone();
                Box::pin(async move {
                    Ok(owned
                        .find(
                            Some(&job.artist),
                            Some(&job.title),
                            job.seconds,
                            job.album.as_deref(),
                        )
                        .await
                        .map(|copy| OwnedSong(copy.navidrome_id)))
                })
            }),
            start: Arc::new(move |job| {
                let (ids, hearts, queue, tracker) = (
                    ids.clone(),
                    hearts.clone(),
                    start_queue.clone(),
                    start_tracker.clone(),
                );
                Box::pin(async move {
                    // A family member's songs go to their own library; one whose library is full
                    // (or who only saves songs) gets nothing until that changes.
                    let target = hearts.import_target(&job.owner).map_err(TrickleHeld)?;
                    let external_id = ids.register(OutsideRouting {
                        kind: RoutingKind::Song,
                        artist: Some(job.artist.clone()),
                        title: Some(job.title.clone()),
                        album: job.album.clone(),
                        duration: job.seconds,
                        isrc: job.isrc.clone(),
                        ..Default::default()
                    });
                    let provider = OutsideMetadataService::PROVIDER_NAME;
                    let key = key_of(provider, &external_id);
                    queue.update(&job.owner, &job.key, |j| j.acquisition_key = Some(key.clone()));
                    // The tracker follows only rows that were opened, so the download gets one,
                    // as a heart's does.
                    tracker.begin(
                        provider,
                        &external_id,
                        None,
                        Some(&job.owner),
                        Some(&job.artist),
                        Some(&job.title),
                        job.album.as_deref(),
                    );
                    hearts
                        .acquire_track_for(provider, &external_id, Some(&job.owner), &target)
                        .await?;
                    Ok(key)
                })
            }),
            row: Arc::new(move |key| {
                row_tracker
                    .all()
                    .into_iter()
                    .find(|row| key_of(&row.provider, &row.external_id) == key)
            }),
            people_downloading: Arc::new(move || !downloads.is_idle()),
            clock: Clock::system(),
        };
        let worker = Self::new(queue, settings, seams);
        let weak = Arc::downgrade(&worker);
        tracker.subscribe_ended(Arc::new(move |end: &AcquisitionEnd| {
            if let Some(worker) = weak.upgrade() {
                worker.on_ended(end);
            }
        }));
        worker
    }

    /// Changes the seams (tests use it).
    pub fn configure(&self, change: impl FnOnce(&mut TrickleSeams)) {
        change(&mut self.seams.lock());
    }

    fn seams(&self) -> TrickleSeams {
        self.seams.lock().clone()
    }

    fn now(&self) -> DateTime<Utc> {
        self.seams.lock().clock.now()
    }

    /// A song finished: fetched (with its library id when Navidrome showed it) or not.
    pub fn on_finished(&self, listener: FinishedListener) {
        self.finished.lock().push(listener);
    }

    /// What the trickle is doing for this person.
    pub fn status_for(&self, owner: &str) -> TrickleStatus {
        let per_hour = self.settings.current().imports.effective_songs_per_hour();
        let jobs = self.queue.snapshot(Some(owner));
        let open = jobs.iter().any(|job| import_track_states::open(&job.state));
        if !open {
            return TrickleStatus {
                state: trickle_states::IDLE,
                next_utc: None,
                per_hour,
            };
        }
        if self.queue.is_paused(owner) {
            return TrickleStatus {
                state: trickle_states::PAUSED,
                next_utc: None,
                per_hour,
            };
        }
        if per_hour == 0 {
            return TrickleStatus {
                state: trickle_states::OFF,
                next_utc: None,
                per_hour,
            };
        }
        if jobs.iter().any(|job| job.state == import_track_states::QUEUED) {
            let waiting = *self.waiting_for.lock();
            if waiting == trickle_states::YIELDING {
                return TrickleStatus {
                    state: waiting,
                    next_utc: None,
                    per_hour,
                };
            }
        }
        TrickleStatus {
            state: trickle_states::RUNNING,
            next_utc: self.next_start(per_hour),
            per_hour,
        }
    }

    fn next_start(&self, per_hour: i32) -> Option<DateTime<Utc>> {
        if per_hour <= 0 {
            return None;
        }
        let now = self.now();
        let next = match self.queue.last_start_utc() {
            None => now,
            Some(last) => last + TimeDelta::microseconds((3600.0 / f64::from(per_hour) * 1_000_000.0) as i64),
        };
        Some(if next < now { now } else { next })
    }

    /// One pass: settle hand-offs, then start the next song when its turn has come. True when one
    /// started.
    pub async fn tick(self: &Arc<Self>) -> bool {
        self.settle_hand_offs();
        if self
            .running
            .lock()
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            return false;
        }
        let per_hour = self.settings.current().imports.effective_songs_per_hour();
        if per_hour == 0 {
            return false;
        }
        if self.next_start(per_hour).is_some_and(|next| next > self.now()) {
            return false;
        }
        let seams = self.seams();
        if (seams.people_downloading)() {
            *self.waiting_for.lock() = trickle_states::YIELDING;
            return false;
        }
        *self.waiting_for.lock() = trickle_states::RUNNING;
        let Some(job) = self.queue.take_next() else {
            *self.waiting_for.lock() = trickle_states::IDLE;
            return false;
        };
        *self.current.lock() = Some((job.owner.clone(), job.key.clone()));
        let worker = self.clone();
        *self.running.lock() = Some(tokio::spawn(async move { worker.run(job).await }));
        true
    }

    /// Waits for the song started last. Only tests need it.
    pub async fn drain(&self) {
        let running = self.running.lock().take();
        if let Some(task) = running {
            let _ = task.await;
        }
    }

    async fn run(&self, job: TrickleJob) {
        let seams = self.seams();
        let outcome: anyhow::Result<()> = async {
            if let Some(owned) = (seams.owned)(job.clone()).await? {
                self.finish(
                    &job,
                    import_track_states::DONE,
                    Some("Already in your library"),
                    owned.0,
                );
                return Ok(());
            }
            let key = (seams.start)(job.clone()).await?;
            // The chain is over: a song in the library, a failure, or a hand-off to Lidarr still
            // running. The tracker's row says which; a hand-off is settled when it ends.
            *self.current.lock() = None;
            self.settle(&job.owner, &job.key, (seams.row)(&key));
            Ok(())
        }
        .await;
        if let Err(e) = outcome {
            if let Some(held) = e.downcast_ref::<TrickleHeld>() {
                info!(
                    "Trickle for '{} - {}' ({}) skipped: {held}",
                    job.artist, job.title, job.owner
                );
                self.finish(&job, import_track_states::SKIPPED, Some(&held.0), None);
                *self.current.lock() = None;
                return;
            }
            warn!(
                "Trickle for '{} - {}' ({}) failed: {e}",
                job.artist, job.title, job.owner
            );
            let detail = user_safe(Some(&e.to_string())).unwrap_or_else(|| "Could not get it.".to_string());
            self.finish(&job, import_track_states::NOT_FOUND, Some(&detail), None);
        }
        *self.current.lock() = None;
    }

    fn is_current(&self, job: &TrickleJob) -> bool {
        self.current
            .lock()
            .as_ref()
            .is_some_and(|(owner, key)| *key == job.key && eq_ignore_case(owner, &job.owner))
    }

    fn on_ended(&self, end: &AcquisitionEnd) {
        // The song whose chain is still running is settled when the chain returns: a source in
        // the middle of the chain failing is not the end of it.
        let Some(job) = self.queue.by_acquisition(&end.key) else {
            return;
        };
        if self.is_current(&job) {
            return;
        }
        let row = (self.seams().row)(&end.key);
        self.settle(&job.owner, &job.key, row);
        debug!("Trickle settled {} on its download's end", job.key);
    }

    /// Songs whose chain returned with a download still running elsewhere (Lidarr), and ones
    /// nothing reports on.
    fn settle_hand_offs(&self) {
        let row = self.seams().row;
        for job in self
            .queue
            .snapshot(None)
            .into_iter()
            .filter(|job| job.state == import_track_states::DOWNLOADING && !self.is_current(job))
        {
            let found = job.acquisition_key.as_deref().and_then(|key| row(key));
            self.settle(&job.owner, &job.key, found);
        }
    }

    fn settle(&self, owner: &str, track_key: &str, row: Option<AcquisitionSnapshot>) {
        let Some(job) = self
            .queue
            .get(owner, track_key)
            .filter(|job| job.state == import_track_states::DOWNLOADING)
        else {
            return;
        };
        let age = job
            .started_utc
            .map_or(TimeDelta::zero(), |started| self.now() - started);
        match row.as_ref().map(|r| r.state) {
            Some(AcquisitionState::Done) => {
                let row = row.as_ref().expect("a row");
                let detail = match &row.source {
                    Some(source) => format!("Fetched from {source}"),
                    None => "Fetched".to_string(),
                };
                self.finish(
                    &job,
                    import_track_states::DONE,
                    Some(&detail),
                    row.library_id.clone(),
                );
            }
            Some(AcquisitionState::Failed) => {
                let row = row.as_ref().expect("a row");
                let detail = row
                    .error
                    .clone()
                    .unwrap_or_else(|| "No download source could get this song.".to_string());
                self.finish(&job, import_track_states::NOT_FOUND, Some(&detail), None);
            }
            None => {
                // No row: it aged out of the tracker, or never opened. The library check at the
                // start of its next turn says whether it arrived; three times without one is
                // enough.
                if job.attempts >= Self::MAX_ATTEMPTS {
                    self.finish(
                        &job,
                        import_track_states::NOT_FOUND,
                        Some("Octo lost track of this download three times."),
                        None,
                    );
                } else {
                    self.queue.update(owner, track_key, |j| {
                        j.state = import_track_states::QUEUED.to_string();
                        j.detail = Some("Checking again".to_string());
                    });
                }
            }
            Some(_) => {
                if age > Self::GIVE_UP_AFTER {
                    self.finish(
                        &job,
                        import_track_states::NOT_FOUND,
                        Some("Still not in the library after 12 hours."),
                        None,
                    );
                }
            }
        }
    }

    fn finish(&self, job: &TrickleJob, state: &str, detail: Option<&str>, library_id: Option<String>) {
        self.queue.update(&job.owner, &job.key, |j| {
            j.state = state.to_string();
            j.detail = detail.map(str::to_string);
            j.library_id = library_id;
        });
        info!(
            "Trickle: '{} - {}' for {}: {state} ({})",
            job.artist,
            job.title,
            job.owner,
            detail.unwrap_or("")
        );
        if let Some(finished) = self.queue.get(&job.owner, &job.key) {
            let listeners = self.finished.lock().clone();
            for listener in listeners {
                let told = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(&finished)));
                if told.is_err() {
                    warn!("A trickle listener failed");
                }
            }
        }
    }

    /// The hosted loop: a tick every five seconds until Octo stops.
    pub async fn run_loop(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        while !stopping.is_cancelled() {
            // Per-tick catch: a panic in one tick must not stop the trickle.
            let worker = self.clone();
            let tick = tokio::spawn(async move { worker.tick().await });
            if let Err(e) = tick.await {
                error!("Trickle tick failed: {e}");
            }
            tokio::select! {
                () = stopping.cancelled() => break,
                () = tokio::time::sleep(Self::TICK_EVERY) => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "trickle_worker_tests.rs"]
mod tests;
