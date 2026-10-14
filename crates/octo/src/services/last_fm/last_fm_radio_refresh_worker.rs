//! Runs canonical recommendation refreshes inside the existing Octo host. A background worker
//! ("LastFmRadioRefreshWorker"), and the only settings-change subscriber: a settings change
//! queues the rebuilds it implies.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use indexmap::IndexMap;
use octo_core::common::{SingleFlight, dotnet};
use octo_core::last_fm::last_fm_radio_refresh_policy::{
    is_stale, should_schedule_periodic_refresh, startup_jitter,
};
use octo_core::models::radio::LastFmRadioStation;
use octo_core::settings::{AppSettings, LastFmSettings, SettingsStore};
use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::last_fm_radio_recommendation_service::LastFmRadioRecommendationService;
use super::last_fm_radio_refresh_queue::{LastFmRadioRefreshJob, LastFmRadioRefreshQueue};
use super::last_fm_radio_state_store::LastFmRadioStateStore;
use super::last_fm_radio_warmup_service::LastFmRadioWarmupService;

const STALE_SCAN_INTERVAL: Duration = Duration::from_secs(60);
const REFRESH_DEADLINE: Duration = Duration::from_secs(35);

/// What the settings looked like at the last change.
struct Watched {
    /// The enabled pinned definitions' fingerprints, by id ignoring case: the ignore-case key,
    /// then the id and the fingerprint.
    definitions: IndexMap<String, (String, String)>,
    radio_enabled: bool,
    personalized_shape: String,
}

impl Watched {
    fn of(settings: &LastFmSettings) -> Self {
        Watched {
            definitions: fingerprints(settings),
            radio_enabled: settings.enable_radio,
            personalized_shape: personalized_shape(settings),
        }
    }
}

pub struct LastFmRadioRefreshWorker {
    queue: Arc<LastFmRadioRefreshQueue>,
    recommendations: Arc<LastFmRadioRecommendationService>,
    state: Arc<LastFmRadioStateStore>,
    /// The Last.fm settings.
    settings: Arc<SettingsStore>,
    single_flight: SingleFlight<String, bool>,
    warmup: Option<Arc<LastFmRadioWarmupService>>,
    watched: Mutex<Watched>,
    /// Settings changes, subscribed at construction.
    changes: watch::Receiver<Arc<AppSettings>>,
}

impl LastFmRadioRefreshWorker {
    pub fn new(
        queue: Arc<LastFmRadioRefreshQueue>,
        recommendations: Arc<LastFmRadioRecommendationService>,
        state: Arc<LastFmRadioStateStore>,
        settings: Arc<SettingsStore>,
        warmup: Option<Arc<LastFmRadioWarmupService>>,
    ) -> Self {
        let changes = settings.subscribe();
        let watched = Watched::of(&settings.current().last_fm);
        LastFmRadioRefreshWorker {
            queue,
            recommendations,
            state,
            settings,
            single_flight: SingleFlight::new(),
            warmup,
            watched: Mutex::new(watched),
            changes,
        }
    }

    pub fn in_flight_count(&self) -> usize {
        self.single_flight.in_flight_count()
    }

    /// The settings-change handler: a removed pinned definition, radio switched on, or a
    /// different set of personalized station kinds rebuilds every known listener; an added or
    /// edited pinned definition refreshes only that station.
    pub fn on_settings_changed(&self, changed: &LastFmSettings) {
        let mut watched = self.watched.lock();
        let next = fingerprints(changed);
        let removed = watched.definitions.keys().any(|key| !next.contains_key(key));
        let rebuild_all = removed
            || (!watched.radio_enabled && changed.enable_radio)
            || watched.personalized_shape != personalized_shape(changed);
        for user in self.state.known_users() {
            if rebuild_all {
                self.queue.enqueue(&user, None);
            } else {
                for (key, (id, fingerprint)) in &next {
                    let unchanged = watched
                        .definitions
                        .get(key)
                        .is_some_and(|(_, old)| old == fingerprint);
                    if !unchanged {
                        self.queue.enqueue(&user, Some(id));
                    }
                }
            }
        }
        *watched = Watched {
            definitions: next,
            radio_enabled: changed.enable_radio,
            personalized_shape: personalized_shape(changed),
        };
    }

    /// Feeds every settings change to [`Self::on_settings_changed`] until `stopping`. (A watch
    /// channel keeps the latest snapshot, so changes that arrive faster than they are handled are
    /// seen as one.)
    pub async fn watch_settings(self: Arc<Self>, stopping: CancellationToken) {
        let mut changes = self.changes.clone();
        loop {
            tokio::select! {
                biased;
                () = stopping.cancelled() => return,
                changed = changes.changed() => if changed.is_err() { return },
            }
            let snapshot = changes.borrow_and_update().clone();
            self.on_settings_changed(&snapshot.last_fm);
        }
    }

    /// The worker's loop, until `stopping`.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let watcher = tokio::spawn(Arc::clone(&self).watch_settings(stopping.clone()));
        // Jitter startup work deterministically so restarts do not fan out all profiles at once.
        for username in self.state.known_users() {
            if stopping.is_cancelled() {
                break;
            }
            let user = self.state.get_user(&username);
            if is_stale(&user, &self.settings.current().last_fm, Utc::now()) {
                self.queue.enqueue(&username, None);
            }
            tokio::select! {
                biased;
                () = stopping.cancelled() => {
                    let _ = watcher.await;
                    return Ok(());
                }
                () = tokio::time::sleep(startup_jitter(&username)) => {}
            }
        }

        let stale_scan = tokio::spawn(Arc::clone(&self).scan_for_stale_users(stopping.clone()));
        while let Some(job) = self.queue.dequeue(&stopping).await {
            // `process` recorded the failure; continue draining.
            let _ = self.process(&job).await;
        }
        let _ = stale_scan.await;
        let _ = watcher.await;
        Ok(())
    }

    async fn scan_for_stale_users(self: Arc<Self>, stopping: CancellationToken) {
        let mut timer = tokio::time::interval_at(
            tokio::time::Instant::now() + STALE_SCAN_INTERVAL,
            STALE_SCAN_INTERVAL,
        );
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = stopping.cancelled() => return,
                _ = timer.tick() => {}
            }
            let settings = self.settings.current();
            if !settings.last_fm.enable_radio {
                continue;
            }
            for username in self.state.known_users() {
                if should_schedule_periodic_refresh(
                    &self.state.get_user(&username),
                    &settings.last_fm,
                    Utc::now(),
                ) {
                    self.queue.enqueue(&username, None);
                }
            }
        }
    }

    /// One refresh, shared with any refresh of the same listener already running. A failure is
    /// recorded on the listener (their snapshot is kept) and returned.
    pub async fn process(self: &Arc<Self>, job: &LastFmRadioRefreshJob) -> anyhow::Result<bool> {
        let worker = Arc::clone(self);
        let owned = job.clone();
        let result = self
            .single_flight
            .run(
                job.username.clone(),
                move |token| async move { worker.refresh(&owned, &token).await },
                REFRESH_DEADLINE,
            )
            .await;
        result.map_err(|error| {
            let message = error.to_string();
            self.state.mark_refresh_failed(&job.username, &message);
            warn!(
                "Last.fm radio refresh failed for {}; retaining snapshot: {error:#}",
                job.username
            );
            anyhow::anyhow!(message)
        })
    }

    async fn refresh(
        &self,
        job: &LastFmRadioRefreshJob,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<bool> {
        let snapshot = self.settings.current();
        let settings = &snapshot.last_fm;
        if !settings.enable_radio
            || (!settings.enable_personalized_stations && !settings.enable_discovery_stations)
        {
            return Ok(false);
        }
        self.state.mark_refreshing(&job.username);
        let built = self
            .recommendations
            .build(&job.username, cancellation_token)
            .await?;
        let mut stations = built.clone();
        if let Some(definition_id) = job.station_definition_id.as_deref().filter(|id| !id.is_empty()) {
            let pinned_key = format!("pinned-{definition_id}");
            let Some(replacement) = built.into_iter().find(|station| station.key == pinned_key) else {
                anyhow::bail!("Pinned station returned no usable tracks");
            };
            let mut merged: Vec<LastFmRadioStation> = self
                .state
                .get_user(&job.username)
                .stations
                .into_iter()
                .filter(|station| station.key != replacement.key)
                .collect();
            merged.push(replacement);
            stations = merged;
        }
        // An empty build normally means the provider failed, and replacing a user's stations with
        // nothing is worse than keeping a stale snapshot. Switching every station kind off is the
        // one empty build that is a choice, so it is the only one excused here.
        if stations.is_empty()
            && !self.state.get_user(&job.username).stations.is_empty()
            && !settings.stations_explicitly_empty()
        {
            anyhow::bail!("Provider returned no usable replacement stations");
        }
        self.state.replace_stations(&job.username, &stations);
        if let Some(warmup) = &self.warmup {
            warmup.queue_user(&job.username);
        }
        Ok(true)
    }
}

/// Everything that decides WHICH personalized stations get built, as one value.
///
/// A change to any of it has to force a rebuild, because the station list is only recomputed on
/// a refresh. Without this, turning artist radios off leaves them in every client until the next
/// scheduled refresh hours later, and the setting reads as broken. One fingerprint rather than a
/// flag per setting, so a new station type cannot be added without this noticing.
fn personalized_shape(settings: &LastFmSettings) -> String {
    [
        dotnet_bool(settings.enable_personalized_stations).to_string(),
        dotnet_bool(settings.enable_your_mix).to_string(),
        dotnet_bool(settings.enable_discovery_mix).to_string(),
        settings.effective_artist_station_count().to_string(),
        settings.effective_genre_station_count().to_string(),
    ]
    .join("|")
}

fn fingerprints(settings: &LastFmSettings) -> IndexMap<String, (String, String)> {
    settings
        .effective_discovery_stations()
        .into_iter()
        .filter(|definition| definition.enabled)
        .map(|definition| {
            let fingerprint = format!(
                "{}|{}|{}",
                definition.name,
                dotnet_bool(definition.enabled),
                definition.tags.join("|")
            );
            (
                dotnet::ordinal_ignore_case_key(&definition.id),
                (definition.id, fingerprint),
            )
        })
        .collect()
}

/// A bool written "True" or "False".
fn dotnet_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use octo_core::models::radio::{LastFmRadioPlay, LastFmRadioTrack};
    use octo_core::settings::DiscoveryStationSettings;
    use tempfile::TempDir;
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::last_fm::LastFmService;
    use crate::services::last_fm::last_fm_radio_state_store::station_id;
    use crate::services::outside::ExternalIdRegistry;

    struct WorkerFixture {
        _dir: TempDir,
        _server: MockServer,
        settings: Arc<SettingsStore>,
        state: Arc<LastFmRadioStateStore>,
        queue: Arc<LastFmRadioRefreshQueue>,
        worker: Arc<LastFmRadioRefreshWorker>,
    }

    async fn fixture(last_fm: LastFmSettings, delay: Option<Duration>) -> WorkerFixture {
        let dir = TempDir::new().unwrap();
        let server = MockServer::start().await;
        let mut answer = ResponseTemplate::new(200).set_body_string("{}");
        if let Some(delay) = delay {
            answer = answer.set_delay(delay);
        }
        Mock::given(any()).respond_with(answer).mount(&server).await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            last_fm,
            ..Default::default()
        }));
        let state = Arc::new(LastFmRadioStateStore::new(
            dir.path().join("state.json"),
            settings.clone(),
            Arc::new(ExternalIdRegistry::in_memory()),
        ));
        let last_fm = Arc::new(LastFmService::with_base_url(
            settings.clone(),
            &format!("{}/", server.uri()),
        ));
        let recommendations = Arc::new(LastFmRadioRecommendationService::new(
            last_fm,
            state.clone(),
            settings.clone(),
            None,
        ));
        let queue = Arc::new(LastFmRadioRefreshQueue::new());
        let worker = Arc::new(LastFmRadioRefreshWorker::new(
            queue.clone(),
            recommendations,
            state.clone(),
            settings.clone(),
            None,
        ));
        WorkerFixture {
            _dir: dir,
            _server: server,
            settings,
            state,
            queue,
            worker,
        }
    }

    fn pinned(id: &str, name: &str, tag: &str) -> DiscoveryStationSettings {
        DiscoveryStationSettings {
            id: id.into(),
            name: name.into(),
            tags: vec![tag.into()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn worker_retains_stale_snapshot_and_records_failed_replacement() {
        let fixture = fixture(
            LastFmSettings {
                enable_radio: true,
                enable_personalized_stations: false,
                enable_discovery_stations: true,
                ..Default::default()
            },
            None,
        )
        .await;
        let prior = LastFmRadioStation {
            id: station_id("alice", "old"),
            key: "old".into(),
            name: "Last Good".into(),
            owner: "alice".into(),
            tracks: vec![LastFmRadioTrack {
                artist: "A".into(),
                title: "T".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        fixture.state.replace_stations("alice", &[prior]);
        let error = fixture
            .worker
            .process(&LastFmRadioRefreshJob::new("alice"))
            .await
            .expect_err("an empty build is refused");
        assert_eq!(
            error.to_string(),
            "Provider returned no usable replacement stations"
        );
        let user = fixture.state.get_user("alice");
        assert_eq!(user.stations.len(), 1);
        assert_eq!(user.stations[0].name, "Last Good");
        assert!(user.last_refresh_error.is_some());
        assert!(!user.refreshing);
    }

    #[tokio::test]
    async fn definition_change_queues_only_the_changed_pinned_definition() {
        let fixture = fixture(
            LastFmSettings {
                discovery_stations: vec![pinned("rock", "Rock", "rock")],
                ..Default::default()
            },
            None,
        )
        .await;
        let stopping = CancellationToken::new();
        tokio::spawn(fixture.worker.clone().watch_settings(stopping.clone()));
        fixture.state.record_play(
            "alice",
            LastFmRadioPlay {
                artist: "A".into(),
                title: "T".into(),
                ..Default::default()
            },
        );
        fixture.settings.set(AppSettings {
            last_fm: LastFmSettings {
                discovery_stations: vec![pinned("rock", "Rock", "rock"), pinned("jazz", "Jazz", "jazz")],
                ..Default::default()
            },
            ..Default::default()
        });
        let timeout = CancellationToken::new();
        let job = tokio::time::timeout(Duration::from_secs(1), fixture.queue.dequeue(&timeout))
            .await
            .expect("a job within a second")
            .unwrap();
        stopping.cancel();
        assert_eq!(job.username, "alice");
        assert_eq!(job.station_definition_id.as_deref(), Some("jazz"));
    }

    #[tokio::test]
    async fn worker_collapses_concurrent_refreshes_for_the_same_user() {
        let fixture = fixture(
            LastFmSettings {
                api_key: "key".into(),
                minimum_plays: 3,
                ..Default::default()
            },
            Some(Duration::from_millis(80)),
        )
        .await;
        for index in 0..3 {
            fixture.state.record_play(
                "alice",
                LastFmRadioPlay {
                    artist: format!("A{index}"),
                    title: format!("T{index}"),
                    ..Default::default()
                },
            );
        }
        let worker = fixture.worker.clone();
        let first = tokio::spawn(async move { worker.process(&LastFmRadioRefreshJob::new("alice")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let worker = fixture.worker.clone();
        let second = tokio::spawn(async move { worker.process(&LastFmRadioRefreshJob::new("alice")).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(fixture.worker.in_flight_count(), 1);
        assert!(first.await.unwrap().unwrap());
        assert!(second.await.unwrap().unwrap());
        assert_eq!(fixture.worker.in_flight_count(), 0);
    }

    /// Switching radio on, removing a definition, or changing the personalized kinds
    /// rebuilds everyone; an unchanged change queues nothing.
    #[tokio::test]
    async fn the_rebuilds_a_settings_change_implies() {
        let fixture = fixture(
            LastFmSettings {
                enable_radio: false,
                discovery_stations: vec![pinned("rock", "Rock", "rock")],
                ..Default::default()
            },
            None,
        )
        .await;
        fixture.state.mark_refreshing("alice");
        let none = CancellationToken::new();

        let same = fixture.settings.current().last_fm.clone();
        fixture.worker.on_settings_changed(&same);
        // Nothing was queued ahead of the probe.
        assert!(fixture.queue.enqueue("probe", None));
        assert_eq!(fixture.queue.dequeue(&none).await.unwrap().username, "probe");

        let on = LastFmSettings {
            enable_radio: true,
            ..same.clone()
        };
        fixture.worker.on_settings_changed(&on);
        let job = fixture.queue.dequeue(&none).await.unwrap();
        assert_eq!(
            (job.username.as_str(), job.station_definition_id),
            ("alice", None)
        );

        let edited = LastFmSettings {
            discovery_stations: vec![pinned("ROCK", "Rock & Roll", "rock")],
            ..on.clone()
        };
        fixture.worker.on_settings_changed(&edited);
        let job = fixture.queue.dequeue(&none).await.unwrap();
        assert_eq!(job.station_definition_id.as_deref(), Some("rock"));

        let fewer_kinds = LastFmSettings {
            enable_your_mix: false,
            ..edited
        };
        fixture.worker.on_settings_changed(&fewer_kinds);
        let job = fixture.queue.dequeue(&none).await.unwrap();
        assert_eq!(job.station_definition_id, None);

        let removed = LastFmSettings {
            discovery_stations: Vec::new(),
            ..fewer_kinds
        };
        fixture.worker.on_settings_changed(&removed);
        let job = fixture.queue.dequeue(&none).await.unwrap();
        assert_eq!(job.station_definition_id, None);
        assert_eq!(
            personalized_shape(&LastFmSettings::default()),
            "True|True|True|2|3"
        );
    }

    /// A refresh with radio off does nothing, and a pinned refresh keeps the other stations and
    /// replaces only its own.
    #[tokio::test]
    async fn a_pinned_refresh_replaces_only_its_station() {
        let fixture = fixture(
            LastFmSettings {
                enable_radio: false,
                ..Default::default()
            },
            None,
        )
        .await;
        assert!(
            !fixture
                .worker
                .process(&LastFmRadioRefreshJob::new("alice"))
                .await
                .unwrap()
        );

        fixture.settings.set(AppSettings {
            last_fm: LastFmSettings {
                api_key: String::new(),
                enable_personalized_stations: false,
                discovery_stations: vec![pinned("rock", "Rock", "rock")],
                ..Default::default()
            },
            ..Default::default()
        });
        fixture.state.record_play(
            "alice",
            LastFmRadioPlay {
                artist: "A".into(),
                title: "T".into(),
                genre: Some("Rock".into()),
                ..Default::default()
            },
        );
        let other = LastFmRadioStation {
            id: "other".into(),
            key: "your-mix".into(),
            tracks: vec![LastFmRadioTrack {
                artist: "B".into(),
                title: "U".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        fixture.state.replace_stations("alice", &[other]);
        let job = LastFmRadioRefreshJob {
            username: "alice".into(),
            station_definition_id: Some("rock".into()),
        };
        assert!(fixture.worker.process(&job).await.unwrap());
        let keys: Vec<String> = fixture
            .state
            .get_user("alice")
            .stations
            .into_iter()
            .map(|station| station.key)
            .collect();
        assert_eq!(keys, ["your-mix", "pinned-rock"]);

        let missing = LastFmRadioRefreshJob {
            username: "alice".into(),
            station_definition_id: Some("jazz".into()),
        };
        let error = fixture.worker.process(&missing).await.unwrap_err();
        assert_eq!(error.to_string(), "Pinned station returned no usable tracks");
        assert_eq!(
            fixture.state.get_user("alice").last_refresh_error.as_deref(),
            Some("Pinned station returned no usable tracks")
        );
    }
}
