//! The trickle worker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use octo_core::settings::{AppSettings, ImportSettings};

use super::super::ImportTrack;
use super::super::trickle_queue::tests::{moving_clock, song};
use super::*;

struct Rig {
    now: Arc<Mutex<DateTime<Utc>>>,
    queue: Arc<TrickleQueue>,
    settings: Arc<SettingsStore>,
    worker: Arc<TrickleWorker>,
    rows: Arc<Mutex<HashMap<String, AcquisitionSnapshot>>>,
    started: Arc<Mutex<Vec<String>>>,
    finished: Arc<Mutex<Vec<TrickleJob>>>,
    busy: Arc<AtomicBool>,
}

type Owned = Arc<dyn Fn(&TrickleJob) -> Option<String> + Send + Sync>;
type Outcome = Arc<dyn Fn(&TrickleJob) -> Option<AcquisitionState> + Send + Sync>;

fn row(key: &str, state: AcquisitionState) -> AcquisitionSnapshot {
    AcquisitionSnapshot {
        id: key.into(),
        provider: "outside".into(),
        external_id: key.into(),
        artist: Some("Artist".into()),
        title: Some("Song".into()),
        requested_by: vec!["alice".into()],
        source: Some("YouTube".into()),
        state,
        started_at: Utc::now(),
        updated_at: Utc::now(),
        error: (state == AcquisitionState::Failed).then(|| "No YouTube match.".to_string()),
        library_id: (state == AcquisitionState::Done).then(|| format!("nd-{key}")),
        ..Default::default()
    }
}

fn settings(per_hour: i32) -> AppSettings {
    AppSettings {
        imports: ImportSettings {
            songs_per_hour: per_hour,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn rig_with(owned: Owned, outcome: Outcome) -> Rig {
    let (now, clock) = moving_clock();
    let queue = Arc::new(TrickleQueue::default());
    queue.set_clock(clock.clone());
    let settings = Arc::new(SettingsStore::for_tests(settings(12)));
    let rows: Arc<Mutex<HashMap<String, AcquisitionSnapshot>>> = Arc::default();
    let started: Arc<Mutex<Vec<String>>> = Arc::default();
    let busy = Arc::new(AtomicBool::new(false));
    let seams = TrickleSeams {
        owned: Arc::new(move |job| {
            let found = owned(&job).map(|id| OwnedSong(Some(id)));
            Box::pin(async move { Ok(found) })
        }),
        start: {
            let (queue, rows, started) = (queue.clone(), rows.clone(), started.clone());
            Arc::new(move |job| {
                let key = format!("outside:{}", job.key);
                queue.update(&job.owner, &job.key, |j| j.acquisition_key = Some(key.clone()));
                started.lock().push(job.key.clone());
                if let Some(state) = outcome(&job) {
                    rows.lock().insert(key.clone(), row(&key, state));
                }
                Box::pin(async move { Ok(key) })
            })
        },
        row: {
            let rows = rows.clone();
            Arc::new(move |key| rows.lock().get(key).cloned())
        },
        people_downloading: {
            let busy = busy.clone();
            Arc::new(move || busy.load(Ordering::SeqCst))
        },
        clock,
    };
    let worker = TrickleWorker::new(queue.clone(), settings.clone(), seams);
    let finished: Arc<Mutex<Vec<TrickleJob>>> = Arc::default();
    let seen = finished.clone();
    worker.on_finished(Arc::new(move |job| seen.lock().push(job.clone())));
    Rig {
        now,
        queue,
        settings,
        worker,
        rows,
        started,
        finished,
        busy,
    }
}

fn rig() -> Rig {
    rig_with(Arc::new(|_| None), Arc::new(|_| Some(AcquisitionState::Done)))
}

impl Rig {
    async fn tick(&self) -> bool {
        let started = self.worker.tick().await;
        self.worker.drain().await;
        started
    }

    fn add(&self, keys: &[&str]) {
        let songs: Vec<ImportTrack> = keys.iter().map(|k| song(k)).collect();
        self.queue.add("alice", &songs);
    }

    fn advance(&self, by: TimeDelta) {
        *self.now.lock() += by;
    }
}

#[tokio::test]
async fn songs_start_no_closer_than_the_setting_allows() {
    let rig = rig();
    rig.add(&["a", "b"]);
    assert!(rig.tick().await);
    assert!(!rig.tick().await);
    rig.advance(TimeDelta::minutes(4));
    assert!(!rig.tick().await);
    let status = rig.worker.status_for("alice");
    assert_eq!(status.state, trickle_states::RUNNING);
    assert_eq!(status.next_utc, Some(*rig.now.lock() + TimeDelta::minutes(1)));
    rig.advance(TimeDelta::minutes(1));
    assert!(rig.tick().await);
    assert_eq!(*rig.started.lock(), ["a", "b"]);
}

#[tokio::test]
async fn a_fetched_song_is_done_with_its_library_id() {
    let rig = rig();
    rig.add(&["a"]);
    rig.tick().await;
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::DONE);
    assert_eq!(job.library_id.as_deref(), Some("nd-outside:a"));
    assert_eq!(job.detail.as_deref(), Some("Fetched from YouTube"));
    assert_eq!(rig.finished.lock().len(), 1);
}

#[tokio::test]
async fn a_song_that_throws_says_why_in_plain_words_with_no_server_path() {
    let rig = rig();
    rig.worker.configure(|seams| {
        seams.start = Arc::new(|_| {
            Box::pin(async {
                Err(anyhow::anyhow!(
                    "Could not find file '/data/config/octo/history.json'. See the inner exception.\n   at Octo.Something()"
                ))
            })
        });
    });
    rig.add(&["a"]);
    rig.tick().await;
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::NOT_FOUND);
    let detail = job.detail.unwrap_or_default();
    assert!(!detail.contains("/data/config"), "{detail}");
    assert!(!detail.contains("Octo.Something"), "{detail}");
}

#[tokio::test]
async fn a_song_already_in_the_library_is_never_downloaded() {
    let rig = rig_with(
        Arc::new(|_| Some("nd-owned".to_string())),
        Arc::new(|_| Some(AcquisitionState::Done)),
    );
    rig.add(&["a"]);
    rig.tick().await;
    assert!(rig.started.lock().is_empty());
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::DONE);
    assert_eq!(job.library_id.as_deref(), Some("nd-owned"));
    assert_eq!(job.detail.as_deref(), Some("Already in your library"));
}

#[tokio::test]
async fn a_song_no_source_had_is_not_found_with_the_reason() {
    let rig = rig_with(Arc::new(|_| None), Arc::new(|_| Some(AcquisitionState::Failed)));
    rig.add(&["a"]);
    rig.tick().await;
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::NOT_FOUND);
    assert_eq!(job.detail.as_deref(), Some("No YouTube match."));
}

#[tokio::test]
async fn a_hand_off_to_lidarr_is_followed_without_holding_the_next_song_back() {
    let rig = rig_with(
        Arc::new(|_| None),
        Arc::new(|job| {
            Some(if job.key == "a" {
                AcquisitionState::Downloading
            } else {
                AcquisitionState::Done
            })
        }),
    );
    rig.add(&["a", "b"]);
    rig.tick().await;
    assert_eq!(
        rig.queue.get("alice", "a").expect("a").state,
        import_track_states::DOWNLOADING
    );

    rig.advance(TimeDelta::minutes(5));
    assert!(rig.tick().await);
    assert_eq!(
        rig.queue.get("alice", "b").expect("b").state,
        import_track_states::DONE
    );
    assert_eq!(
        rig.queue.get("alice", "a").expect("a").state,
        import_track_states::DOWNLOADING
    );

    rig.rows
        .lock()
        .insert("outside:a".into(), row("outside:a", AcquisitionState::Done));
    rig.tick().await;
    assert_eq!(
        rig.queue.get("alice", "a").expect("a").state,
        import_track_states::DONE
    );
}

#[tokio::test]
async fn it_waits_while_peoples_own_downloads_run() {
    let rig = rig();
    rig.busy.store(true, Ordering::SeqCst);
    rig.add(&["a"]);
    assert!(!rig.tick().await);
    assert_eq!(rig.worker.status_for("alice").state, trickle_states::YIELDING);

    rig.busy.store(false, Ordering::SeqCst);
    assert!(rig.tick().await);
}

#[tokio::test]
async fn none_an_hour_or_a_pause_starts_nothing() {
    let rig = rig();
    rig.settings.set(settings(0));
    rig.add(&["a"]);
    assert!(!rig.tick().await);
    assert_eq!(rig.worker.status_for("alice").state, trickle_states::OFF);

    rig.settings.set(settings(12));
    rig.queue.set_paused("alice", true);
    assert!(!rig.tick().await);
    assert_eq!(rig.worker.status_for("alice").state, trickle_states::PAUSED);
    assert!(rig.started.lock().is_empty());
}

#[tokio::test]
async fn a_download_nobody_reports_on_is_checked_again() {
    let rig = rig_with(Arc::new(|_| None), Arc::new(|_| None));
    rig.add(&["a"]);
    rig.tick().await;
    let job = rig.queue.get("alice", "a").expect("a");
    assert_eq!(job.state, import_track_states::QUEUED);
    assert_eq!(job.detail.as_deref(), Some("Checking again"));
}
