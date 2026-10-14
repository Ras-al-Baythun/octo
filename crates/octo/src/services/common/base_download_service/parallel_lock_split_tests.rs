//! The parallel lock split: with job folders proven, the transfer runs outside DownloadLock and
//! everything after it runs inside. Twenty downloads with random transfer times: never more
//! transfers than the width, more than one at once (or the split proves nothing), and never two
//! placements at once.

use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, DownloadSettings, SettingsStore, SubsonicSettings};
use tokio_util::sync::CancellationToken;

use super::{BaseDownloadService, DownloadBackend, DownloadCore, DownloadServices, TrackDownload};
use crate::services::common::DownloadConcurrency;
use crate::services::common::test_fakes::FakeMetadata;
use crate::services::i_download_service::IDownloadService;
use crate::services::local::test_support::FakeLocalLibrary;
use crate::services::local::{DownloadHistoryService, ILocalLibraryService};
use crate::services::notifications::NotificationService;
use crate::services::subsonic::NavidromeIdentityService;

/// How many are inside at once, and the most there ever were.
#[derive(Default)]
struct Counter {
    now: AtomicI32,
    most: AtomicI32,
}

impl Counter {
    fn enter(&self) {
        let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
    }

    fn leave(&self) {
        self.now.fetch_sub(1, Ordering::SeqCst);
    }

    fn most(&self) -> i32 {
        self.most.load(Ordering::SeqCst)
    }
}

/// A transfer of a random few milliseconds that lands an MP3 in the staging folder.
struct SplitBackend {
    root: String,
    transfers: Arc<Counter>,
}

#[async_trait]
impl DownloadBackend for SplitBackend {
    fn provider_name(&self) -> &str {
        "test"
    }

    async fn is_available(&self, _: &BaseDownloadService) -> bool {
        true
    }

    async fn download_track(
        &self,
        _: &BaseDownloadService,
        download: &mut TrackDownload,
        _: &CancellationToken,
    ) -> anyhow::Result<String> {
        self.transfers.enter();
        let pause = 2 + u64::from(uuid::Uuid::new_v4().as_bytes()[0]) % 23;
        tokio::time::sleep(Duration::from_millis(pause)).await;
        self.transfers.leave();
        let landed = format!(
            "{}/.octo-incoming/{}-{}.mp3",
            self.root,
            download.track_id,
            uuid::Uuid::new_v4().simple()
        );
        std::fs::create_dir_all(format!("{}/.octo-incoming", self.root))?;
        std::fs::write(&landed, crate::services::test_support::mp3())?;
        Ok(landed)
    }

    fn extract_external_id_from_album_id(&self, _: &str) -> Option<String> {
        None
    }
}

/// A local library whose registration counts how many run at once.
struct CountingLibrary {
    inner: FakeLocalLibrary,
    placements: Arc<Counter>,
}

#[async_trait]
impl ILocalLibraryService for CountingLibrary {
    async fn get_local_path_for_external_song(&self, provider: &str, id: &str) -> Option<String> {
        self.inner.get_local_path_for_external_song(provider, id).await
    }

    async fn register_downloaded_song(&self, _: &Song, _: &str) -> anyhow::Result<()> {
        self.placements.enter();
        tokio::time::sleep(Duration::from_millis(3)).await;
        self.placements.leave();
        Ok(())
    }

    async fn get_local_id_for_external_song(&self, provider: &str, id: &str) -> Option<String> {
        self.inner.get_local_id_for_external_song(provider, id).await
    }

    fn parse_song_id(&self, song_id: &str) -> crate::services::local::ParsedSongId {
        self.inner.parse_song_id(song_id)
    }

    fn parse_external_id(&self, id: &str) -> crate::services::local::ParsedExternalId {
        self.inner.parse_external_id(id)
    }

    async fn get_mappings(&self) -> Vec<crate::services::local::LocalSongMapping> {
        self.inner.get_mappings().await
    }

    async fn find_mapping_by_tags(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) -> Option<crate::services::local::LocalSongMapping> {
        self.inner.find_mapping_by_tags(artist, title, album).await
    }

    async fn forget_mapping(
        &self,
        path: &str,
    ) -> anyhow::Result<Vec<crate::services::local::LocalSongMapping>> {
        self.inner.forget_mapping(path).await
    }

    async fn restore_mappings(
        &self,
        mappings: &[crate::services::local::LocalSongMapping],
        path: &str,
    ) -> anyhow::Result<()> {
        self.inner.restore_mappings(mappings, path).await
    }

    async fn trigger_library_scan(&self, force: bool) -> bool {
        self.inner.trigger_library_scan(force).await
    }

    async fn get_scan_status(&self) -> Option<octo_core::models::subsonic::ScanStatus> {
        self.inner.get_scan_status().await
    }
}

/// A catalog that answers every id with a song of that id.
fn catalog() -> Arc<FakeMetadata> {
    let metadata = FakeMetadata::default();
    for round in 0..3 {
        for i in 0..20 {
            let id = format!("r{round}-{i}");
            metadata.songs.lock().insert(
                ("test".to_string(), id.clone()),
                Song {
                    title: format!("Song {id}"),
                    artist: "Artist".into(),
                    album: "Album".into(),
                    external_provider: Some("test".into()),
                    external_id: Some(id),
                    ..Default::default()
                },
            );
        }
    }
    Arc::new(metadata)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transfers_overlap_up_to_the_width_and_placements_never() {
    {
        let width = 3;
        let root = tempfile::Builder::new()
            .prefix("octo-split-")
            .tempdir()
            .expect("a temp folder");
        let root_text = root.path().to_string_lossy().into_owned();
        let transfers = Arc::new(Counter::default());
        let placements = Arc::new(Counter::default());

        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            subsonic: SubsonicSettings {
                auto_detect_download_path: false,
                ..Default::default()
            },
            downloads: DownloadSettings {
                parallel_downloads: width,
                ..Default::default()
            },
            ..Default::default()
        }));
        settings.set_raw("Library:DownloadPath", Some(&root_text));
        let concurrency = Arc::new(DownloadConcurrency::new(Arc::clone(&settings)));
        let core = DownloadCore {
            settings: Arc::clone(&settings),
            local_library: Arc::new(CountingLibrary {
                inner: FakeLocalLibrary::default(),
                placements: Arc::clone(&placements),
            }),
            metadata: catalog(),
            navidrome_identity: NavidromeIdentityService::new(Arc::clone(&settings), reqwest::Client::new()),
            history: Arc::new(DownloadHistoryService::new(root.path().join("history.json"))),
            notifications: Arc::new(NotificationService::new(Vec::new(), Arc::clone(&settings), None)),
        };
        let service = BaseDownloadService::new(
            core,
            DownloadServices {
                concurrency: Some(Arc::clone(&concurrency)),
                ..Default::default()
            },
            Arc::new(SplitBackend {
                root: root_text.clone(),
                transfers: Arc::clone(&transfers),
            }),
        );

        for round in 0..3 {
            let downloads = (0..20).map(|i| {
                let service = Arc::clone(&service);
                tokio::spawn(async move {
                    service
                        .execute_acquisition(
                            "test",
                            &format!("r{round}-{i}"),
                            false,
                            true,
                            None,
                            &CancellationToken::new(),
                            None,
                            false,
                            None,
                        )
                        .await
                })
            });
            let all = futures::future::join_all(downloads);
            for outcome in tokio::time::timeout(Duration::from_secs(60), all)
                .await
                .expect("within a minute")
            {
                outcome.expect("the task ran").expect("the download landed");
            }
        }

        let case = format!("width {width}");
        assert_eq!(placements.most(), 1, "{case}");
        assert!(
            transfers.most() > 1,
            "{case}: the transfers never overlapped, so the split proves nothing"
        );
        assert!(
            transfers.most() <= width,
            "{case}: {} transfers at once against a width of {width}",
            transfers.most()
        );
        assert_eq!(concurrency.transfers().in_use(), 0, "{case}");
    }
}
