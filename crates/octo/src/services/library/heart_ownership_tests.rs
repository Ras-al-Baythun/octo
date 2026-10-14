//! A heart on a song Octo found outside the library downloads it, then favorites it for whoever
//! hearted it. A heart on a song that is already in the library is a favorite and nothing else:
//! it never downloads, never waits behind other downloads, and never sends
//! Lidarr for a whole album. An owned MP3 is also queued for Better quality.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use indexmap::IndexMap;
use octo_core::common::Clock;
use octo_core::models::domain::{Album, Song};
use octo_core::settings::{
    AppSettings, DownloadSource, HeartDownloadSource, HeartDownloadStep, LibraryAction,
    LibraryActionDefinition, LibraryActionSettings, SettingsStore, SubsonicSettings,
};
use octo_subsonic::SubsonicCredential;
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::services::common::test_fakes::{FakeDownloads, FakeLidarr, FakeMetadata, until};
use crate::services::common::{
    AcquisitionState, AcquisitionTracker, HeartAcquisitionCoordinator, HeartCoordinatorExtras, StarOnArrival,
    TrackAcquisitionQueue,
};
use crate::services::library::library_ownership::Candidate;
use crate::services::local::test_support::FakeLocalLibrary;

type Calls = Arc<Mutex<Vec<(String, IndexMap<String, String>)>>>;

struct Fixture {
    root: tempfile::TempDir,
    queue: Arc<TrackAcquisitionQueue>,
    downloads: Arc<FakeDownloads>,
    lidarr: Arc<FakeLidarr>,
    metadata: Arc<FakeMetadata>,
    tracker: Arc<AcquisitionTracker>,
    stars: Arc<StarOnArrival>,
    upgrades: Arc<UpgradeQueue>,
    calls: Calls,
    library: Arc<Mutex<Vec<Candidate>>>,
}

fn song(title: &str, id: &str, seconds: i32) -> Song {
    Song {
        title: title.into(),
        artist: "Massive Attack".into(),
        album: "Mezzanine".into(),
        duration: Some(seconds),
        external_provider: Some("deezer".into()),
        external_id: Some(id.into()),
        ..Default::default()
    }
}

fn owned(id: &str, title: &str, suffix: &str, bit_rate: i32) -> Candidate {
    Candidate {
        id: id.into(),
        artist: "Massive Attack".into(),
        title: title.into(),
        album: Some("Mezzanine".into()),
        duration: Some(if title == "Angel" { 380 } else { 330 }),
        suffix: suffix.into(),
        bit_rate,
        isrcs: Vec::new(),
    }
}

fn upgrades_on() -> LibraryActionSettings {
    LibraryActionSettings {
        enabled: true,
        dry_run: false,
        allowed_users: vec!["alice".into()],
        actions: vec![LibraryActionDefinition {
            action: LibraryAction::BetterQuality,
            enabled: true,
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn alice() -> SubsonicCredential {
    let pairs: Vec<(String, String)> = vec![
        ("u".into(), "alice".into()),
        ("t".into(), "token-alice".into()),
        ("s".into(), "salt-alice".into()),
        ("c".into(), "Symfonium".into()),
    ];
    SubsonicCredential::from(pairs.iter().map(|(k, v)| (k, v))).expect("a sign-in")
}

impl Fixture {
    fn new() -> Fixture {
        let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
        tracker.configure_watch(|watch| {
            watch.visibility_poll = Duration::from_millis(10);
            watch.library_lookup = Some(Arc::new(|_, title: String, _| {
                async move { Ok(Some(format!("nd-{}", title.to_lowercase()))) }.boxed()
            }));
        });
        let stars = StarOnArrival::new(
            tracker.clone(),
            None,
            Arc::new(SettingsStore::for_tests(AppSettings::default())),
            Clock::system(),
        );
        let calls: Calls = Arc::default();
        let recorded = calls.clone();
        stars.configure(|seams| {
            seams.call = Some(Arc::new(move |endpoint: String, parameters: IndexMap<String, String>| {
                recorded.lock().push((endpoint.clone(), parameters.clone()));
                let body = if endpoint == "rest/getSong" {
                    format!(
                        r#"{{"subsonic-response":{{"status":"ok","song":{{"id":"{}","albumId":"al-1"}}}}}}"#,
                        parameters["id"]
                    )
                } else {
                    r#"{"subsonic-response":{"status":"ok"}}"#.to_string()
                };
                async move { Ok(body.into_bytes()) }.boxed()
            }));
        });
        let metadata = Arc::new(FakeMetadata::default());
        metadata.songs.lock().insert(
            ("deezer".into(), "teardrop".into()),
            song("Teardrop", "teardrop", 330),
        );
        metadata.albums.lock().insert(
            ("deezer".into(), "mezzanine".into()),
            Album {
                title: "Mezzanine".into(),
                artist: "Massive Attack".into(),
                songs: vec![song("Angel", "angel", 380), song("Teardrop", "teardrop", 330)],
                ..Default::default()
            },
        );
        Fixture {
            root: tempfile::tempdir().expect("temp dir"),
            queue: Arc::new(TrackAcquisitionQueue::new()),
            downloads: Arc::new(FakeDownloads::albums(|_, _| Ok(true))),
            lidarr: Arc::new(FakeLidarr::default().tracks(|_, _, _| Ok(true))),
            metadata,
            tracker,
            stars,
            upgrades: Arc::new(UpgradeQueue::new()),
            calls,
            library: Arc::default(),
        }
    }

    fn own(&self, library: Vec<Candidate>) {
        *self.library.lock() = library;
    }

    fn coordinator(
        &self,
        chain: &[HeartDownloadSource],
        skip_owned: bool,
        actions: Option<LibraryActionSettings>,
    ) -> Arc<HeartAcquisitionCoordinator> {
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            subsonic: SubsonicSettings {
                skip_owned_songs: skip_owned,
                heart_download_sources: chain
                    .iter()
                    .map(|source| HeartDownloadStep {
                        source: *source,
                        song_enabled: Some(true),
                        album_enabled: Some(true),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            },
            lidarr: octo_core::settings::LidarrSettings {
                base_url: Some("http://lidarr:8686".into()),
                api_key: Some("k".into()),
                root_folder_path: Some("/music".into()),
                quality_profile_id: 1,
                metadata_profile_id: 1,
                ..Default::default()
            },
            library_actions: actions.unwrap_or_default(),
            ..Default::default()
        }));
        let ownership = Arc::new(LibraryOwnership::new(
            settings.clone(),
            None,
            Arc::new(FakeLocalLibrary::default()),
        ));
        let library = self.library.clone();
        ownership.set_search(Arc::new(move |_| {
            let candidates = library.lock().clone();
            async move { Ok(Some(candidates)) }.boxed()
        }));
        let dir = self.root.path().to_path_buf();
        ownership.set_resolve(Arc::new(move |id| {
            let path = dir.join(format!("{id}.file"));
            async move {
                std::fs::write(&path, [1])?;
                Ok(Some(path.to_string_lossy().into_owned()))
            }
            .boxed()
        }));
        let hearts = Arc::new(HeartOwnership::new(
            ownership,
            self.metadata.clone(),
            settings.clone(),
            Some(self.upgrades.clone()),
            Some(Arc::new(UpgradeSources::new(settings.clone()))),
        ));
        HeartAcquisitionCoordinator::new(
            settings,
            self.queue.clone(),
            self.downloads.clone(),
            self.lidarr.clone(),
            HeartCoordinatorExtras {
                tracker: Some(self.tracker.clone()),
                owned: Some(hearts),
                stars: Some(self.stars.clone()),
                ..Default::default()
            },
        )
    }

    fn youtube_only(&self) -> Arc<HeartAcquisitionCoordinator> {
        self.coordinator(&[HeartDownloadSource::YouTube], true, None)
    }

    /// What the star endpoint does before it hands a song heart to the coordinator.
    fn heart(&self, octo_app: bool) {
        self.tracker.begin(
            "deezer",
            "teardrop",
            Some("teardrop"),
            Some("alice"),
            Some("Massive Attack"),
            Some("Teardrop"),
            Some("Mezzanine"),
        );
        if !octo_app {
            self.stars
                .hold_song("deezer", "teardrop", &alice(), Some("alice"));
        }
    }

    fn stars_sent(&self) -> Vec<IndexMap<String, String>> {
        self.calls
            .lock()
            .iter()
            .filter(|(endpoint, _)| endpoint == "rest/star")
            .map(|(_, parameters)| parameters.clone())
            .collect()
    }

    fn row(&self) -> crate::services::common::AcquisitionSnapshot {
        self.tracker
            .all()
            .into_iter()
            .find(|row| row.external_id == "teardrop")
            .expect("the row")
    }

    async fn acquire_track(&self, coordinator: &Arc<HeartAcquisitionCoordinator>) {
        tokio::time::timeout(
            Duration::from_secs(5),
            coordinator.acquire_track("deezer", "teardrop", Some("alice")),
        )
        .await
        .expect("in time")
        .expect("ran");
    }
}

#[tokio::test]
async fn a_heart_on_a_song_you_have_favorites_your_copy_and_downloads_nothing() {
    let f = Fixture::new();
    f.own(vec![owned("nd-9", "Teardrop", "flac", 1000)]);
    f.heart(false);

    f.acquire_track(&f.youtube_only()).await;

    until(|| f.stars_sent().len() == 1).await;
    let star = &f.stars_sent()[0];
    assert_eq!(star["id"], "nd-9");
    assert_eq!(star["u"], "alice");
    assert!(f.queue.is_idle());
    assert!(f.upgrades.snapshot().is_empty());
}

#[tokio::test]
async fn a_heart_on_an_mp3_you_have_favorites_it_and_queues_better_quality() {
    let f = Fixture::new();
    f.own(vec![owned("nd-9", "Teardrop", "mp3", 320)]);
    f.heart(false);

    f.acquire_track(&f.coordinator(&[HeartDownloadSource::YouTube], true, Some(upgrades_on())))
        .await;

    until(|| f.stars_sent().len() == 1).await;
    assert_eq!(f.stars_sent()[0]["id"], "nd-9");
    assert!(f.queue.is_idle());
    let jobs = f.upgrades.snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].navidrome_id, "nd-9");
    assert_eq!(jobs[0].origin, "heart");
}

async fn take_download(f: &Fixture) -> Arc<crate::services::common::AcquisitionRequest> {
    tokio::time::timeout(Duration::from_secs(5), f.queue.dequeue(&CancellationToken::new()))
        .await
        .expect("in time")
        .expect("a download")
}

#[tokio::test]
async fn a_heart_on_a_song_you_do_not_have_downloads_it() {
    let f = Arc::new(Fixture::new());
    f.heart(false);

    let coordinator = f.youtube_only();
    let running = coordinator.clone();
    let chain = tokio::spawn(async move { running.acquire_track("deezer", "teardrop", Some("alice")).await });
    let download = take_download(&f).await;

    assert_eq!(download.external_id, "teardrop");
    assert!(download.is_star);
    assert!(f.stars_sent().is_empty());
    f.queue.release(&download);
    download
        .completion
        .try_set_result(f.root.path().join("teardrop.flac").to_string_lossy());
    tokio::time::timeout(Duration::from_secs(5), chain)
        .await
        .expect("in time")
        .expect("joined")
        .expect("ran");
}

/// The heart was the download: it is not a favorite when the song lands, unless
/// StarDownloadsForRequester asks for that (off by default).
#[tokio::test]
async fn a_heart_on_a_song_you_do_not_have_is_not_a_favorite_when_it_lands() {
    let f = Arc::new(Fixture::new());
    f.heart(false);

    let coordinator = f.youtube_only();
    let running = coordinator.clone();
    let chain = tokio::spawn(async move { running.acquire_track("deezer", "teardrop", Some("alice")).await });
    let download = take_download(&f).await;
    // What the download does once the file is placed.
    let placed = f.root.path().join("teardrop.flac").to_string_lossy().into_owned();
    f.tracker.imported(
        "deezer",
        "teardrop",
        Some("Massive Attack"),
        Some("Teardrop"),
        Some(&placed),
    );
    f.queue.release(&download);
    download.completion.try_set_result(placed);
    tokio::time::timeout(Duration::from_secs(5), chain)
        .await
        .expect("in time")
        .expect("joined")
        .expect("ran");

    until(|| f.row().state == AcquisitionState::Done).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.stars_sent().is_empty());
    assert_eq!(f.stars.held(), 0);
}

#[tokio::test]
async fn a_heart_on_a_song_you_have_never_sends_lidarr_for_the_album() {
    let f = Fixture::new();
    f.own(vec![owned("nd-9", "Teardrop", "flac", 1000)]);
    f.heart(false);

    f.acquire_track(&f.coordinator(
        &[HeartDownloadSource::Lidarr, HeartDownloadSource::YouTube],
        true,
        None,
    ))
    .await;

    until(|| f.stars_sent().len() == 1).await;
    assert!(f.lidarr.track_calls.lock().is_empty());
}

#[tokio::test]
async fn from_octos_own_apps_a_song_you_have_is_left_as_it_is() {
    // Their heart means Add, and the song is already added: no favorite, no download.
    let f = Fixture::new();
    f.own(vec![owned("nd-9", "Teardrop", "flac", 1000)]);
    f.heart(true);

    f.acquire_track(&f.youtube_only()).await;

    assert!(f.queue.is_idle());
    assert!(f.stars_sent().is_empty());
    until(|| f.row().state == AcquisitionState::Done).await;
    assert_eq!(f.row().library_id.as_deref(), Some("nd-9"));
}

#[tokio::test]
async fn with_the_check_off_every_heart_downloads() {
    let f = Arc::new(Fixture::new());
    f.own(vec![owned("nd-9", "Teardrop", "flac", 1000)]);
    f.heart(false);

    let coordinator = f.coordinator(&[HeartDownloadSource::YouTube], false, None);
    tokio::spawn(async move {
        let _ = coordinator
            .acquire_track("deezer", "teardrop", Some("alice"))
            .await;
    });
    let download = take_download(&f).await;

    assert_eq!(download.external_id, "teardrop");
    f.queue.release(&download);
    download
        .completion
        .try_set_result(f.root.path().join("teardrop.flac").to_string_lossy());
}

#[tokio::test]
async fn an_album_you_have_whole_is_favorited_and_nothing_downloads() {
    let f = Fixture::new();
    f.own(vec![
        owned("nd-1", "Angel", "flac", 1000),
        owned("nd-3", "Teardrop", "flac", 1000),
    ]);
    f.tracker.begin_album("deezer", "mezzanine", Some("alice"));
    f.stars.hold_album("deezer", "mezzanine", &alice(), Some("alice"));

    tokio::time::timeout(
        Duration::from_secs(5),
        f.youtube_only()
            .acquire_album("deezer", "mezzanine", Some("alice")),
    )
    .await
    .expect("in time")
    .expect("ran");

    until(|| f.stars_sent().iter().any(|call| call.contains_key("albumId"))).await;
    let album_star = f
        .stars_sent()
        .into_iter()
        .find(|call| call.contains_key("albumId"))
        .expect("an album star");
    assert_eq!(album_star["albumId"], "al-1");
    assert!(f.downloads.album_calls.lock().is_empty());
}

#[tokio::test]
async fn an_album_you_have_partly_goes_to_fetch_the_rest() {
    let f = Fixture::new();
    f.own(vec![owned("nd-3", "Teardrop", "flac", 1000)]);

    tokio::time::timeout(
        Duration::from_secs(5),
        f.youtube_only()
            .acquire_album("deezer", "mezzanine", Some("alice")),
    )
    .await
    .expect("in time")
    .expect("ran");

    let calls = f.downloads.album_calls.lock().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].0.as_str(), calls[0].1.as_str()),
        ("deezer", "mezzanine")
    );
    assert_eq!(calls[0].2, DownloadSource::YouTube);
}
