//! The trickle queue and its file, `imports-trickle.json`: the missing songs the trickle is
//! working through, one job per song per person however many lists hold it, with who has paused
//! theirs, on disk so a restart carries on where it was. A job running when Octo stopped is
//! queued again; the library check it starts with catches one that arrived meanwhile.
//!
//! A song that could not be found is tried again after two weeks, since what the sources have
//! changes; one someone skipped stays skipped until they ask for it again.

use std::path::{Path, PathBuf};

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::Clock;
use octo_core::common::dotnet::{eq_ignore_case, ordinal_ignore_case_key, to_lower_invariant};
use octo_core::json::datetime;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::import_models::import_track_states;
use super::{ImportTrack, TrickleJob};
use crate::services::framework::DotnetDictionary;
use crate::services::state_file::null_as_default;

/// A song that could not be found is tried again after two weeks, since what the sources have
/// changes.
pub const RETRY_NOT_FOUND_AFTER: TimeDelta = TimeDelta::days(14);

/// A fetched job is kept a week, then pruned.
pub const KEEP_DONE: TimeDelta = TimeDelta::days(7);

/// `TrickleQueue.FileShape`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct TrickleFile {
    #[serde(deserialize_with = "null_as_default")]
    pub jobs: Vec<TrickleJob>,
    /// The people who paused their trickle.
    #[serde(deserialize_with = "null_as_default")]
    pub paused: Vec<String>,
    #[serde(with = "datetime::utc_option")]
    pub last_start_utc: Option<DateTime<Utc>>,
}

/// The same shape over the queue's own jobs, for writing without a copy.
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct FileRef<'a> {
    jobs: Vec<&'a TrickleJob>,
    paused: Vec<&'a String>,
    #[serde(with = "datetime::utc_option")]
    last_start_utc: Option<DateTime<Utc>>,
}

/// The file as the queue starts from it: a job running when Octo stopped is queued again (the
/// library check it starts with catches one that arrived meanwhile).
pub fn load(path: &Path) -> TrickleFile {
    let mut file: TrickleFile = super::load_or_default(path, "the trickle queue");
    for job in &mut file.jobs {
        if job.state == import_track_states::DOWNLOADING {
            job.state = import_track_states::QUEUED.to_string();
            job.detail = Some("Octo restarted while this was running; it goes again.".to_string());
        }
    }
    file
}

pub fn save(path: &Path, file: &TrickleFile) -> bool {
    super::save(path, file, "the trickle queue")
}

struct State {
    /// By `owner.ToLowerInvariant()` and key.
    jobs: DotnetDictionary<TrickleJob>,
    /// The people who paused theirs, by name ignoring case.
    paused: DotnetDictionary<String>,
    last_start: Option<DateTime<Utc>>,
}

/// The queue itself.
pub struct TrickleQueue {
    path: Option<PathBuf>,
    state: Mutex<State>,
    clock: Mutex<Clock>,
}

impl Default for TrickleQueue {
    fn default() -> Self {
        TrickleQueue::new(None)
    }
}

fn key_of(owner: &str, key: &str) -> String {
    format!("{}\n{key}", to_lower_invariant(owner))
}

impl TrickleQueue {
    /// No path keeps the queue in memory only, as the tests build it.
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut state = State {
            jobs: DotnetDictionary::new(),
            paused: DotnetDictionary::new(),
            last_start: None,
        };
        if let Some(path) = path.as_deref().filter(|p| p.exists()) {
            let file = load(path);
            for job in file.jobs {
                state.jobs.set(key_of(&job.owner, &job.key), job);
            }
            for name in file.paused {
                let key = ordinal_ignore_case_key(&name);
                if !state.paused.contains_key(&key) {
                    state.paused.set(key, name);
                }
            }
            state.last_start = file.last_start_utc;
        }
        TrickleQueue {
            path,
            state: Mutex::new(state),
            clock: Mutex::new(Clock::system()),
        }
    }

    /// The `Clock` seam the tests set.
    pub fn set_clock(&self, clock: Clock) {
        *self.clock.lock() = clock;
    }

    fn now(&self) -> DateTime<Utc> {
        self.clock.lock().now()
    }

    /// Queues the songs not already queued, running, fetched or skipped. A song not found two
    /// weeks ago or more is queued again. Answers how many were queued.
    pub fn add<'a>(&self, owner: &str, tracks: impl IntoIterator<Item = &'a ImportTrack>) -> i32 {
        let now = self.now();
        let mut state = self.state.lock();
        let mut added = 0;
        let mut seen: Vec<&str> = Vec::new();
        for track in tracks {
            if seen.contains(&track.key.as_str()) {
                continue;
            }
            seen.push(&track.key);
            if track.key.trim().is_empty() {
                continue;
            }
            let key = key_of(owner, &track.key);
            if let Some(existing) = state.jobs.get(&key) {
                let stale = existing.state == import_track_states::NOT_FOUND
                    && now - existing.updated_utc >= RETRY_NOT_FOUND_AFTER;
                if !stale {
                    continue;
                }
            }
            state.jobs.set(
                key,
                TrickleJob {
                    owner: owner.to_string(),
                    key: track.key.clone(),
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    album: track.album.clone(),
                    seconds: track.seconds,
                    isrc: track.isrc.clone(),
                    state: import_track_states::QUEUED.to_string(),
                    queued_utc: now,
                    updated_utc: now,
                    ..Default::default()
                },
            );
            added += 1;
        }
        if added > 0 {
            self.write(&state);
        }
        added
    }

    /// Every job, or one person's, oldest first, as copies.
    pub fn snapshot(&self, owner: Option<&str>) -> Vec<TrickleJob> {
        let now = self.now();
        let mut state = self.state.lock();
        prune(&mut state, now);
        let mut jobs: Vec<TrickleJob> = state
            .jobs
            .values()
            .filter(|job| owner.is_none_or(|o| eq_ignore_case(&job.owner, o)))
            .cloned()
            .collect();
        jobs.sort_by_key(|job| job.queued_utc);
        jobs
    }

    pub fn get(&self, owner: &str, key: &str) -> Option<TrickleJob> {
        self.state.lock().jobs.get(&key_of(owner, key)).cloned()
    }

    pub fn is_paused(&self, owner: &str) -> bool {
        self.state
            .lock()
            .paused
            .contains_key(&ordinal_ignore_case_key(owner))
    }

    pub fn set_paused(&self, owner: &str, paused: bool) {
        let mut state = self.state.lock();
        let key = ordinal_ignore_case_key(owner);
        let changed = if paused {
            !state.paused.contains_key(&key) && {
                state.paused.set(key, owner.to_string());
                true
            }
        } else {
            state.paused.remove(&key).is_some()
        };
        if changed {
            self.write(&state);
        }
    }

    /// When the trickle last started a song, for its pace.
    pub fn last_start_utc(&self) -> Option<DateTime<Utc>> {
        self.state.lock().last_start
    }

    /// The next song, now running, or None. People take turns: the one whose last song started
    /// longest ago goes next, so one long list cannot keep everyone else waiting. Paused people
    /// are passed over.
    pub fn take_next(&self) -> Option<TrickleJob> {
        let now = self.now();
        let mut state = self.state.lock();
        let last_by_owner = |state: &State, owner: &str| -> DateTime<Utc> {
            state
                .jobs
                .values()
                .filter(|job| eq_ignore_case(&job.owner, owner))
                .filter_map(|job| job.started_utc)
                .max()
                .unwrap_or_else(datetime::min_value)
        };
        let mut queued: Vec<(DateTime<Utc>, DateTime<Utc>, String)> = state
            .jobs
            .iter()
            .filter(|(_, job)| {
                job.state == import_track_states::QUEUED
                    && !state.paused.contains_key(&ordinal_ignore_case_key(&job.owner))
            })
            .map(|(key, job)| (last_by_owner(&state, &job.owner), job.queued_utc, key.clone()))
            .collect();
        // Stable, as OrderBy and ThenBy are.
        queued.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let (_, _, key) = queued.into_iter().next()?;
        let job = state.jobs.get_mut(&key)?;
        job.state = import_track_states::DOWNLOADING.to_string();
        job.detail = None;
        job.started_utc = Some(now);
        job.updated_utc = now;
        job.attempts += 1;
        let next = job.clone();
        state.last_start = Some(now);
        self.write(&state);
        Some(next)
    }

    pub fn update(&self, owner: &str, key: &str, change: impl FnOnce(&mut TrickleJob)) {
        let now = self.now();
        let mut state = self.state.lock();
        let Some(job) = state.jobs.get_mut(&key_of(owner, key)) else {
            return;
        };
        change(job);
        job.updated_utc = now;
        self.write(&state);
    }

    /// The running job with this download, or None.
    pub fn by_acquisition(&self, acquisition_key: &str) -> Option<TrickleJob> {
        self.state
            .lock()
            .jobs
            .values()
            .find(|job| {
                job.state == import_track_states::DOWNLOADING
                    && job
                        .acquisition_key
                        .as_deref()
                        .is_some_and(|k| eq_ignore_case(k, acquisition_key))
            })
            .cloned()
    }

    /// Songs not found or skipped, queued again. Answers how many.
    pub fn retry<S: AsRef<str>>(&self, owner: &str, keys: &[S]) -> i32 {
        let now = self.now();
        self.change(
            owner,
            keys,
            |job| job.state == import_track_states::NOT_FOUND || job.state == import_track_states::SKIPPED,
            |job| {
                job.state = import_track_states::QUEUED.to_string();
                job.detail = None;
                job.attempts = 0;
                job.queued_utc = now;
            },
        )
    }

    /// Queued songs set aside. A running one runs to its end. Answers how many.
    pub fn skip<S: AsRef<str>>(&self, owner: &str, keys: &[S]) -> i32 {
        self.change(
            owner,
            keys,
            |job| job.state == import_track_states::QUEUED,
            |job| {
                job.state = import_track_states::SKIPPED.to_string();
                job.detail = Some("Skipped".to_string());
            },
        )
    }

    fn change<S: AsRef<str>>(
        &self,
        owner: &str,
        keys: &[S],
        when: impl Fn(&TrickleJob) -> bool,
        change: impl Fn(&mut TrickleJob),
    ) -> i32 {
        let now = self.now();
        let mut state = self.state.lock();
        let mut changed = 0;
        let mut seen: Vec<&str> = Vec::new();
        for key in keys {
            let key = key.as_ref();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            let Some(job) = state.jobs.get_mut(&key_of(owner, key)) else {
                continue;
            };
            if !when(job) {
                continue;
            }
            change(job);
            job.updated_utc = now;
            changed += 1;
        }
        if changed > 0 {
            self.write(&state);
        }
        changed
    }

    /// Forgets one person's finished songs: fetched, not found and skipped. Answers how many.
    pub fn clear_finished(&self, owner: &str) -> i32 {
        let mut state = self.state.lock();
        let finished: Vec<String> = state
            .jobs
            .iter()
            .filter(|(_, job)| eq_ignore_case(&job.owner, owner) && !import_track_states::open(&job.state))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &finished {
            state.jobs.remove(key);
        }
        if !finished.is_empty() {
            self.write(&state);
        }
        finished.len() as i32
    }

    /// Takes back one person's queued songs, leaving what already ran. Answers how many.
    pub fn cancel_queued(&self, owner: &str, keys: Option<&[String]>) -> i32 {
        let mut state = self.state.lock();
        let queued: Vec<String> = state
            .jobs
            .iter()
            .filter(|(_, job)| {
                eq_ignore_case(&job.owner, owner)
                    && job.state == import_track_states::QUEUED
                    && keys.is_none_or(|only| only.contains(&job.key))
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &queued {
            state.jobs.remove(key);
        }
        if !queued.is_empty() {
            self.write(&state);
        }
        queued.len() as i32
    }

    // Called with the lock held.
    fn write(&self, state: &State) {
        if let Some(path) = &self.path {
            let file = FileRef {
                jobs: state.jobs.values().collect(),
                paused: state.paused.values().collect(),
                last_start_utc: state.last_start,
            };
            super::save(path, &file, "the trickle queue");
        }
    }
}

// Called with the lock held. Fetched songs go after a week; the list itself says they are there.
fn prune(state: &mut State, now: DateTime<Utc>) {
    let cutoff = now - KEEP_DONE;
    let old: Vec<String> = state
        .jobs
        .iter()
        .filter(|(_, job)| job.state == import_track_states::DONE && job.updated_utc < cutoff)
        .map(|(key, _)| key.clone())
        .collect();
    for key in old {
        state.jobs.remove(&key);
    }
}

#[cfg(test)]
#[path = "trickle_queue_tests.rs"]
pub(crate) mod tests;
