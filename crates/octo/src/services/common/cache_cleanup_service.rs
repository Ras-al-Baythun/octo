//! The cache cleanup, a worker of the supervisor.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::PathHelper;
use octo_core::common::dotnet::starts_with_ignore_case;
use octo_core::settings::{SettingsStore, StorageMode};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

/// Background service that periodically cleans up old cached files
/// Only runs when StorageMode is set to Cache
pub struct CacheCleanupService {
    // Read at use, never captured: the admin UI writes settings.json and the store reloads it,
    // but this is a singleton, so a captured copy would serve startup values until a restart
    // while the admin UI SHOWED the new value and nothing acted on it.
    settings: Arc<SettingsStore>,
    cleanup_interval: Duration,
    /// `PathHelper.GetCachePath()`; another folder in tests.
    cache_path: PathBuf,
}

impl CacheCleanupService {
    pub fn new(settings: Arc<SettingsStore>) -> Self {
        Self::with_cache_path(settings, PathBuf::from(PathHelper::get_cache_path()))
    }

    pub fn with_cache_path(settings: Arc<SettingsStore>, cache_path: PathBuf) -> Self {
        CacheCleanupService {
            settings,
            cleanup_interval: Duration::from_secs(60 * 60),
            cache_path,
        }
    }

    /// The worker's loop, until `stopping`.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        // Only run if storage mode is Cache
        if self.settings.current().subsonic.storage_mode != StorageMode::Cache {
            info!("CacheCleanupService disabled: StorageMode is not Cache");
            return Ok(());
        }

        info!(
            "CacheCleanupService started with cleanup interval of 01:00:00 and retention of {} hours",
            self.settings.current().subsonic.cache_duration_hours
        );

        while !stopping.is_cancelled() {
            let service = Arc::clone(&self);
            let token = stopping.clone();
            // Long file work goes off the runtime. Errors are caught per file inside; a panic
            // is the only way out, and the loop continues after it, as after a caught error.
            if let Err(e) =
                tokio::task::spawn_blocking(move || service.cleanup_old_cached_files(&token)).await
            {
                error!(error = %e, "Error during cache cleanup");
            }
            tokio::select! {
                () = stopping.cancelled() => break,
                () = tokio::time::sleep(self.cleanup_interval) => {}
            }
        }

        info!("CacheCleanupService stopped");
        Ok(())
    }

    /// One pass: every file not read for `CacheDurationHours` goes, then every empty folder.
    pub fn cleanup_old_cached_files(&self, cancellation: &CancellationToken) {
        let cache_path = &self.cache_path;
        if !cache_path.is_dir() {
            debug!("Cache directory does not exist: {}", cache_path.display());
            return;
        }

        let hours = self.settings.current().subsonic.cache_duration_hours;
        let cutoff_time = Utc::now() - TimeDelta::hours(i64::from(hours));
        let mut deleted_count = 0;
        let mut total_size = 0u64;

        info!("Starting cache cleanup: deleting files older than {cutoff_time}");

        let files = match all_files(cache_path) {
            Ok(files) => files,
            Err(e) => {
                error!(error = %e, "Error during cache cleanup");
                return;
            }
        };
        for file_path in files {
            if cancellation.is_cancelled() {
                break;
            }

            // Continuous Radio owns this sub-cache. Its prepared starter tracks
            // must outlive the generic one-hour download cache so every issued
            // (12-hour) station URL keeps its immediate-start guarantee.
            let relative = file_path
                .strip_prefix(cache_path)
                .map(|r| r.to_string_lossy().into_owned())
                .unwrap_or_default();
            if starts_with_ignore_case(&relative, "radio/") {
                continue;
            }

            let attempt = (|| -> std::io::Result<Option<(u64, DateTime<Utc>)>> {
                let metadata = std::fs::metadata(&file_path)?;
                // Use last access time to determine if file should be deleted
                // This gets updated when a cached file is streamed
                let accessed: DateTime<Utc> = metadata.accessed()?.into();
                if accessed < cutoff_time {
                    std::fs::remove_file(&file_path)?;
                    return Ok(Some((metadata.len(), accessed)));
                }
                Ok(None)
            })();
            match attempt {
                Ok(Some((size, accessed))) => {
                    deleted_count += 1;
                    total_size += size;
                    debug!(
                        "Deleted cached file: {} (last accessed: {accessed})",
                        file_path.display()
                    );
                }
                Ok(None) => {}
                Err(e) => warn!(error = %e, "Failed to delete cached file: {}", file_path.display()),
            }
        }

        // Clean up empty directories
        cleanup_empty_directories(cache_path, cancellation);

        if deleted_count > 0 {
            let size_mb = total_size as f64 / (1024.0 * 1024.0);
            info!("Cache cleanup completed: deleted {deleted_count} files, freed {size_mb:.2} MB");
        } else {
            debug!("Cache cleanup completed: no files to delete");
        }
    }
}

/// `Directory.GetFiles(path, "*.*", SearchOption.AllDirectories)`: every file below `root`.
fn all_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

fn all_directories(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                directories.push(entry.path());
                pending.push(entry.path());
            }
        }
    }
    Ok(directories)
}

fn cleanup_empty_directories(root: &Path, cancellation: &CancellationToken) {
    let mut directories = match all_directories(root) {
        Ok(directories) => directories,
        Err(e) => {
            warn!(error = %e, "Error cleaning up empty directories");
            return;
        }
    };
    // Process deepest directories first
    directories.sort_by_key(|d| std::cmp::Reverse(d.as_os_str().len()));
    for directory in directories {
        if cancellation.is_cancelled() {
            break;
        }
        let attempt = (|| -> std::io::Result<bool> {
            if std::fs::read_dir(&directory)?.next().is_none() {
                std::fs::remove_dir(&directory)?;
                return Ok(true);
            }
            Ok(false)
        })();
        match attempt {
            Ok(true) => debug!("Deleted empty directory: {}", directory.display()),
            Ok(false) => {}
            Err(e) => warn!(error = %e, "Failed to delete empty directory: {}", directory.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, FileTimes};
    use std::time::SystemTime;

    use octo_core::settings::{AppSettings, SubsonicSettings};

    use super::*;

    fn service(dir: &Path, mode: StorageMode) -> CacheCleanupService {
        CacheCleanupService::with_cache_path(
            Arc::new(SettingsStore::for_tests(AppSettings {
                subsonic: SubsonicSettings {
                    storage_mode: mode,
                    cache_duration_hours: 1,
                    ..Default::default()
                },
                ..Default::default()
            })),
            dir.to_path_buf(),
        )
    }

    fn write(path: &Path, accessed_hours_ago: u64) {
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
        std::fs::write(path, [1, 2, 3]).expect("write");
        let when = SystemTime::now() - Duration::from_secs(accessed_hours_ago * 3600);
        File::options()
            .write(true)
            .open(path)
            .expect("open")
            .set_times(FileTimes::new().set_accessed(when))
            .expect("set the access time");
    }

    /// A file not read for the retention goes, a fresh one and the radio sub-cache stay, and
    /// folders left empty go.
    #[test]
    fn old_files_go_and_radio_stays() {
        let dir = tempfile::tempdir().expect("temp dir");
        let old = dir.path().join("outside/a/old.flac");
        let fresh = dir.path().join("youtube/fresh.mp3");
        let radio = dir.path().join("radio/key.mp3");
        write(&old, 3);
        write(&fresh, 0);
        write(&radio, 30);

        service(dir.path(), StorageMode::Cache).cleanup_old_cached_files(&CancellationToken::new());

        assert!(!old.exists());
        assert!(!dir.path().join("outside").exists(), "the emptied folders go");
        assert!(fresh.exists());
        assert!(radio.exists());
    }

    /// Outside Cache mode the worker ends at once.
    #[tokio::test]
    async fn the_worker_does_nothing_outside_cache_mode() {
        let dir = tempfile::tempdir().expect("temp dir");
        let old = dir.path().join("old.flac");
        write(&old, 3);
        Arc::new(service(dir.path(), StorageMode::Permanent))
            .run(CancellationToken::new())
            .await
            .expect("finished");
        assert!(old.exists());
    }

    #[tokio::test]
    async fn the_worker_cleans_then_stops_when_asked() {
        let dir = tempfile::tempdir().expect("temp dir");
        let old = dir.path().join("old.flac");
        write(&old, 3);
        let stopping = CancellationToken::new();
        let worker = tokio::spawn(Arc::new(service(dir.path(), StorageMode::Cache)).run(stopping.clone()));
        for _ in 0..200 {
            if !old.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!old.exists());
        stopping.cancel();
        worker.await.expect("joined").expect("finished");
    }
}
