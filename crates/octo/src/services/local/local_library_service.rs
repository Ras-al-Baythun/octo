//! The local library: `.mappings.json` in the download directory, and the Subsonic scan trigger.
//!
//! The file is written atomically (`.mappings.json.tmp`, then a rename), and a file that does not
//! parse is moved aside to `.mappings.json.corrupt-<UtcTicks>` and the service starts empty,
//! rather than failing every caller until the file is fixed by hand.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeDelta, Utc};
use indexmap::IndexMap;
use octo_core::common::dotnet::{self, eq_ignore_case, escape_data_string};
use octo_core::common::{PathHelper, SongIdentity};
use octo_core::json::{self, datetime};
use octo_core::models::domain::Song;
use octo_core::models::subsonic::ScanStatus;
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use tracing::{debug, error, info, warn};

use super::ILocalLibraryService;
use super::i_local_library_service::{ParsedExternalId, ParsedSongId};
use super::scan_wait::ScanHold;
use crate::services::family::NavidromeAdmin;
use crate::services::framework::DotnetDictionary;
use crate::services::outside::{ExternalIdRegistry, OutsideMetadataService};
use crate::services::state_file;
use crate::services::subsonic::NavidromeIdentityService;
use crate::services::subsonic::navidrome_version::ServerInfo;

/// Represents the mapping between an external song and its local file
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct LocalSongMapping {
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub external_provider: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub external_id: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub local_path: String,
    pub local_subsonic_id: Option<String>,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub title: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "state_file::null_as_default")]
    pub album: String,
    #[serde(with = "datetime::utc")]
    pub downloaded_at: DateTime<Utc>,

    /// Who delivered this file, when an earlier version fetched it from a peer. Optional, so mappings
    /// written before this existed still load.
    pub source_peer: Option<String>,
    pub source_file: Option<String>,

    /// The MusicBrainz recording a fingerprint confirmed this file is. Optional, like
    /// `source_peer`; it is what lets a later download of the same recording replace this file
    /// rather than sit beside it.
    pub music_brainz_recording_id: Option<String>,

    /// What the file was likely made from when its spectrum said it is a lossy file converted to
    /// lossless ("about 128 kbps MP3"). Optional, like `source_peer`; None for a genuine file and
    /// for every mapping written before the check existed.
    pub transcoded_from: Option<String>,
}

impl Default for LocalSongMapping {
    fn default() -> Self {
        LocalSongMapping {
            external_provider: String::new(),
            external_id: String::new(),
            local_path: String::new(),
            local_subsonic_id: None,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            // The earliest date, which the mappings file writes without a `Z`.
            downloaded_at: datetime::min_value(),
            source_peer: None,
            source_file: None,
            music_brainz_recording_id: None,
            transcoded_from: None,
        }
    }
}

/// The mappings, written in the dictionary's own order.
struct MappingsFile<'a>(&'a DotnetDictionary<LocalSongMapping>);

impl Serialize for MappingsFile<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0.iter() {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

/// Local library service implementation
/// Uses a simple JSON file to store mappings (can be replaced with a database)
pub struct LocalLibraryService {
    mapping_file_path: PathBuf,
    download_directory: String,
    http: reqwest::Client,
    // The live settings store, not a copy taken at startup: the admin UI writes settings.json
    // and the store reloads it, and this service lives as long as the app, so a captured copy
    // would serve startup values until a restart while the admin UI SHOWED the new value.
    settings: Arc<SettingsStore>,
    id_registry: Arc<ExternalIdRegistry>,
    nav_identity: NavidromeIdentityService,
    /// The mappings, loaded once and then cached. The file is small and its reads and writes
    /// are blocking `std::fs` held under this lock, so no two of them interleave.
    mappings: Mutex<Option<DotnetDictionary<LocalSongMapping>>>,
    /// Debounce to avoid triggering too many scans
    last_scan_trigger: Mutex<Option<DateTime<Utc>>>,
    /// Octo's own triggers, held back while the library tidy moves files.
    scan_hold: ScanHold,
    /// Scans of one library: Octo's admin calls, when each library was last scanned and
    /// whether one more scan is already waiting.
    admin: NavidromeAdmin,
    targeted: Arc<Mutex<HashMap<i64, TargetedScan>>>,
    scan_window: Duration,
    /// Told whenever Octo starts a scan, so the library index can be read again after it.
    scan_notice: Option<Arc<tokio::sync::Notify>>,
}

/// One library's scans.
#[derive(Debug, Default, Clone, Copy)]
struct TargetedScan {
    last: Option<tokio::time::Instant>,
    trailing: bool,
}

impl LocalLibraryService {
    const SCAN_DEBOUNCE_INTERVAL: TimeDelta = TimeDelta::seconds(30);

    /// `Library:DownloadPath` is read once, here. Creating the directory is
    /// [`LocalLibraryService::create_download_directory`], which the app state calls, so that
    /// building a service does no I/O.
    pub fn new(
        settings: Arc<SettingsStore>,
        http: reqwest::Client,
        id_registry: Arc<ExternalIdRegistry>,
        nav_identity: NavidromeIdentityService,
    ) -> Self {
        let download_directory = settings.raw("Library:DownloadPath").unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_default()
                .join("downloads")
                .to_string_lossy()
                .into_owned()
        });
        let mapping_file_path = Path::new(&download_directory).join(".mappings.json");
        let admin = NavidromeAdmin::new(nav_identity.clone(), http.clone(), Arc::clone(&settings));
        LocalLibraryService {
            mapping_file_path,
            download_directory,
            http,
            settings,
            id_registry,
            nav_identity,
            mappings: Mutex::new(None),
            last_scan_trigger: Mutex::new(None),
            scan_hold: ScanHold::new(),
            admin,
            targeted: Arc::new(Mutex::new(HashMap::new())),
            scan_window: Self::SCAN_DEBOUNCE_INTERVAL
                .to_std()
                .unwrap_or(Duration::from_secs(30)),
            scan_notice: None,
        }
    }

    /// Notifies `notice` whenever this service starts a scan.
    pub fn with_scan_notice(mut self, notice: Arc<tokio::sync::Notify>) -> Self {
        self.scan_notice = Some(notice);
        self
    }

    /// How long after a library's scan another call for it waits (tests shorten it).
    pub fn with_scan_window(mut self, window: Duration) -> Self {
        self.scan_window = window;
        self
    }

    /// Starts a scan of one library now, and tells the scan notice.
    async fn scan_one(admin: &NavidromeAdmin, notice: Option<&tokio::sync::Notify>, library_id: i64) -> bool {
        match admin.scan_library(library_id).await {
            Ok(()) => {
                info!("Started a scan of Navidrome library {library_id}");
                if let Some(notice) = notice {
                    notice.notify_one();
                }
                true
            }
            Err(e) => {
                warn!("Could not start a scan of Navidrome library {library_id}: {e}");
                false
            }
        }
    }

    /// Creates the download directory when it does not exist.
    pub fn create_download_directory(&self) {
        if let Err(e) = std::fs::create_dir_all(&self.download_directory) {
            warn!(
                "Could not create the download directory {}: {e}",
                self.download_directory
            );
        }
    }

    pub fn get_download_directory(&self) -> &str {
        &self.download_directory
    }

    /// The mappings, loaded on first use and cached from then on. Called with the lock held.
    /// `Err` when the file could not be read (it is tried again next time); a file that does
    /// not parse is moved aside and the service starts empty.
    fn loaded<'a>(
        &self,
        cached: &'a mut Option<DotnetDictionary<LocalSongMapping>>,
    ) -> anyhow::Result<&'a mut DotnetDictionary<LocalSongMapping>> {
        if cached.is_none() {
            *cached = Some(self.read_file()?);
        }
        Ok(cached.as_mut().expect("loaded just above"))
    }

    fn read_file(&self) -> anyhow::Result<DotnetDictionary<LocalSongMapping>> {
        let mut map = DotnetDictionary::new();
        let Some(text) = state_file::read_text(&self.mapping_file_path)? else {
            return Ok(map);
        };
        match serde_json::from_str::<Option<IndexMap<String, LocalSongMapping>>>(&text) {
            Ok(entries) => {
                for (key, mapping) in entries.unwrap_or_default() {
                    map.set(key, mapping);
                }
            }
            Err(e) => {
                let aside = corrupt_path(&self.mapping_file_path, Utc::now());
                match std::fs::rename(&self.mapping_file_path, &aside) {
                    Ok(()) => error!(
                        "{} is not valid JSON ({e}); moved it to {} and starting with no mappings",
                        self.mapping_file_path.display(),
                        aside.display()
                    ),
                    Err(move_error) => {
                        error!(
                            "{} is not valid JSON ({e}) and could not be moved aside: {move_error}",
                            self.mapping_file_path.display()
                        );
                        return Err(e.into());
                    }
                }
            }
        }
        Ok(map)
    }

    /// Writes the mappings, indented and atomically.
    fn save(&self, mappings: &DotnetDictionary<LocalSongMapping>) -> anyhow::Result<()> {
        let text = json::to_string_indented(&MappingsFile(mappings));
        state_file::save_atomic(&self.mapping_file_path, &text)?;
        Ok(())
    }

    /// A read that could not load the file answers as if there were no mappings, with the
    /// reason logged, rather than failing the caller.
    fn read<T>(&self, answer: impl FnOnce(&DotnetDictionary<LocalSongMapping>) -> T) -> Option<T> {
        let mut cached = self.mappings.lock();
        match self.loaded(&mut cached) {
            Ok(map) => Some(answer(map)),
            Err(e) => {
                error!("Could not read {}: {e}", self.mapping_file_path.display());
                None
            }
        }
    }
}

impl LocalLibraryService {
    /// startScan, debounced unless forced. True only when Navidrome said the scan started: a
    /// 200 carrying a failed envelope (a sign-in Navidrome refused) is not a scan.
    async fn send_scan(&self, force: bool) -> bool {
        // Debounce: avoid triggering too many successive scans. A forced call skips it:
        // otherwise the last track of a batch can have its scan swallowed and stay
        // invisible until some unrelated trigger happens along.
        let now = Utc::now();
        {
            let mut last = self.last_scan_trigger.lock();
            if !force && let Some(previous) = *last {
                let elapsed = now - previous;
                if elapsed < Self::SCAN_DEBOUNCE_INTERVAL {
                    debug!(
                        "Scan debounced - last scan was {}s ago",
                        elapsed.num_milliseconds() as f64 / 1000.0
                    );
                    return true;
                }
            }
            *last = Some(now);
        }

        // Navidrome's startScan requires an admin identity. Octo, as a proxy,
        // gets one from the NavidromeIdentityService (captured from a client's
        // relayed login or configured admin creds). When available we send the
        // Subsonic u/t/s triplet; otherwise we fall back to the bare call, which
        // only works on servers that allow unauthenticated localhost scans.
        let auth = self.nav_identity.get_scan_auth();
        let base = self.settings.current().subsonic.url.clone().unwrap_or_default();
        let url = match &auth {
            Some((user, token, salt)) => format!(
                "{base}/rest/startScan?f=json&c=octo&v=1.16.1&u={}&t={token}&s={salt}",
                escape_data_string(user)
            ),
            None => format!("{base}/rest/startScan?f=json"),
        };

        info!(
            "Triggering Subsonic library scan ({})...",
            if auth.is_none() {
                "unauthenticated"
            } else {
                "authenticated"
            }
        );

        let result: anyhow::Result<bool> = async {
            let response = self.http.get(&url).send().await?;
            let status = response.status();
            if status.is_success() {
                let content = response.text().await?;
                if !scan_started(&content) {
                    warn!("Navidrome did not start a scan: {content}");
                    return Ok(false);
                }
                info!("Subsonic scan triggered successfully: {content}");
                if let Some(notice) = &self.scan_notice {
                    notice.notify_one();
                }
                Ok(true)
            } else {
                warn!("Failed to trigger Subsonic scan: {status} - Server may require authentication");
                Ok(false)
            }
        }
        .await;
        result.unwrap_or_else(|e| {
            error!(error = %e, "Error triggering Subsonic library scan");
            false
        })
    }
}

/// `<file>.corrupt-<ticks>`, ticks being 100 ns intervals since 0001-01-01 UTC: the name the
/// other stores move a bad file to.
fn corrupt_path(path: &Path, now: DateTime<Utc>) -> PathBuf {
    const UNIX_EPOCH_TICKS: i64 = 621_355_968_000_000_000;
    let ticks =
        UNIX_EPOCH_TICKS + now.timestamp() * 10_000_000 + i64::from(now.timestamp_subsec_nanos() / 100);
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".corrupt-{ticks}"));
    PathBuf::from(name)
}

/// The full path of `path`, or the path itself when it is empty.
fn full_path_or_self(path: &str) -> String {
    if path.is_empty() {
        return path.to_string();
    }
    crate::services::library::navidrome_song_path_resolver::get_full_path(path)
}

fn file_exists(path: &str) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

#[async_trait]
impl ILocalLibraryService for LocalLibraryService {
    async fn get_local_path_for_external_song(
        &self,
        external_provider: &str,
        external_id: &str,
    ) -> Option<String> {
        let key = format!("{external_provider}:{external_id}");
        let path = self.read(|map| map.get(&key).map(|m| m.local_path.clone()))??;
        file_exists(&path).then_some(path)
    }

    async fn register_downloaded_song(&self, song: &Song, local_path: &str) -> anyhow::Result<()> {
        let (Some(provider), Some(external_id)) = (&song.external_provider, &song.external_id) else {
            return Ok(());
        };
        let mut cached = self.mappings.lock();
        let mappings = self.loaded(&mut cached)?;
        mappings.set(
            format!("{provider}:{external_id}"),
            LocalSongMapping {
                external_provider: provider.clone(),
                external_id: external_id.clone(),
                local_path: local_path.to_string(),
                title: song.title.clone(),
                artist: song.artist.clone(),
                album: song.album.clone(),
                downloaded_at: Utc::now(),
                source_peer: song.source_peer.clone(),
                source_file: song.source_file.clone(),
                transcoded_from: song.transcoded_from.clone(),
                music_brainz_recording_id: song.music_brainz_recording_id.clone(),
                local_subsonic_id: None,
            },
        );
        self.save(mappings)
    }

    async fn get_local_id_for_external_song(
        &self,
        _external_provider: &str,
        _external_id: &str,
    ) -> Option<String> {
        // For now, return None as we don't yet have integration with the Subsonic server to
        // retrieve local ID after scan
        None
    }

    fn parse_song_id(&self, song_id: &str) -> ParsedSongId {
        let (is_external, provider, _, external_id) = self.parse_external_id(song_id);
        (is_external, provider, external_id)
    }

    fn parse_external_id(&self, id: &str) -> ParsedExternalId {
        // First check the registry: IDs we generated for outside songs are pure base62 (no
        // prefix) so they look identical to local Navidrome IDs to clients but we still know
        // they're ours.
        if self.id_registry.lookup(id).is_some() {
            return (
                true,
                Some(OutsideMetadataService::PROVIDER_NAME.into()),
                Some("song".into()),
                Some(id.to_string()),
            );
        }

        if !id.starts_with("ext-") {
            return (false, None, None, None);
        }

        let parts: Vec<&str> = id.split('-').collect();

        // Known types for the new format
        const KNOWN_TYPES: [&str; 3] = ["song", "album", "artist"];

        // New format: ext-{provider}-{type}-{id} (e.g., ext-deezer-artist-259)
        // Only use new format if parts[2] is a known type
        if parts.len() >= 4 && KNOWN_TYPES.contains(&parts[2]) {
            // Handle IDs with dashes
            return (
                true,
                Some(parts[1].to_string()),
                Some(parts[2].to_string()),
                Some(parts[3..].join("-")),
            );
        }

        // Legacy format: ext-{provider}-{id} (assumes "song" type for backward compatibility)
        // This handles both 3-part IDs and 4+ part IDs where parts[2] is NOT a known type
        if parts.len() >= 3 {
            // Everything after provider is the ID
            return (
                true,
                Some(parts[1].to_string()),
                Some("song".into()),
                Some(parts[2..].join("-")),
            );
        }

        (false, None, None, None)
    }

    async fn get_mappings(&self) -> Vec<LocalSongMapping> {
        self.read(|map| map.values().cloned().collect())
            .unwrap_or_default()
    }

    async fn find_mapping_by_tags(
        &self,
        artist: Option<&str>,
        title: Option<&str>,
        album: Option<&str>,
    ) -> Option<LocalSongMapping> {
        let artist = artist.filter(|a| !dotnet::is_blank(a))?;
        let title = title.filter(|t| !dotnet::is_blank(t))?;
        let album = album.filter(|a| !dotnet::is_blank(a));

        // One song in one version, however its tags write the artist and title.
        let wanted = SongIdentity::match_key(artist, title);
        let album_key = album.map(SongIdentity::key);
        let candidates: Vec<LocalSongMapping> = self.read(|map| {
            map.values()
                .filter(|mapping| {
                    SongIdentity::match_key(&mapping.artist, &mapping.title) == wanted
                        // Album only narrows when both sides have one; a mapping written before
                        // album enrichment should not be excluded for lacking it.
                        && (album_key.is_none()
                            || dotnet::is_blank(&mapping.album)
                            || album_key.as_deref() == Some(SongIdentity::key(&mapping.album).as_str()))
                        && !mapping.local_path.is_empty()
                })
                .cloned()
                .collect()
        })?;
        // The file checks run outside the lock, in the same order, stopping at the second hit.
        let matches: Vec<LocalSongMapping> = candidates
            .into_iter()
            .filter(|mapping| file_exists(&mapping.local_path))
            .take(2)
            .collect();
        if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        }
    }

    /// Drop the mapping for a path Octo no longer owns.
    ///
    /// Without this, the download's existing-file short-circuit keeps pointing a re-acquire at
    /// the file that was just quarantined, and the replacement never happens. Answers the
    /// mappings it dropped, so a song put back can have them again.
    async fn forget_mapping(&self, local_path: &str) -> anyhow::Result<Vec<LocalSongMapping>> {
        if dotnet::is_blank(local_path) {
            return Ok(Vec::new());
        }
        let mut cached = self.mappings.lock();
        let mappings = self.loaded(&mut cached)?;
        // By the whole path, so "Artist/Album/Song.flac" and "Artist/./Album/Song.flac" are one
        // file.
        let wanted = full_path_or_self(local_path);
        let stale: Vec<(String, LocalSongMapping)> = mappings
            .iter()
            .filter(|(_, mapping)| eq_ignore_case(&full_path_or_self(&mapping.local_path), &wanted))
            .map(|(key, mapping)| (key.clone(), mapping.clone()))
            .collect();
        if stale.is_empty() {
            return Ok(Vec::new());
        }
        for (key, _) in &stale {
            mappings.remove(key);
        }
        self.save(mappings)?;
        Ok(stale.into_iter().map(|(_, mapping)| mapping).collect())
    }

    async fn restore_mappings(&self, mappings: &[LocalSongMapping], local_path: &str) -> anyhow::Result<()> {
        if mappings.is_empty() || dotnet::is_blank(local_path) {
            return Ok(());
        }
        let mut cached = self.mappings.lock();
        let current = self.loaded(&mut cached)?;
        let mut added = 0;
        for mapping in mappings {
            if mapping.external_provider.is_empty() || mapping.external_id.is_empty() {
                continue;
            }
            let key = format!("{}:{}", mapping.external_provider, mapping.external_id);
            // Downloaded again while this one was in the trash: that copy is the song's now.
            if current
                .get(&key)
                .is_some_and(|newer| file_exists(&newer.local_path))
            {
                continue;
            }
            let mut mapping = mapping.clone();
            mapping.local_path = local_path.to_string();
            current.set(key, mapping);
            added += 1;
        }
        if added > 0 {
            self.save(current)?;
        }
        Ok(())
    }

    async fn trigger_library_scan(&self, force: bool) -> bool {
        if self.scan_hold.defer() {
            debug!("Scan held back while the library tidy moves files; one is sent when it is done");
            return true;
        }
        self.send_scan(force).await
    }

    async fn update_local_path(&self, old_path: &str, new_path: &str) -> anyhow::Result<usize> {
        if dotnet::is_blank(old_path) || dotnet::is_blank(new_path) {
            return Ok(0);
        }
        let mut cached = self.mappings.lock();
        let mappings = self.loaded(&mut cached)?;
        let mut moved = 0;
        for mapping in mappings.values_mut() {
            if let Some(now) = PathHelper::moved_path(&mapping.local_path, old_path, new_path) {
                mapping.local_path = now;
                moved += 1;
            }
        }
        if moved > 0 {
            self.save(mappings)?;
        }
        Ok(moved)
    }

    async fn start_scan(&self) -> bool {
        self.send_scan(true).await
    }

    fn scan_hold(&self) -> Option<ScanHold> {
        Some(self.scan_hold.clone())
    }

    async fn trigger_library_scan_of(&self, library_id: i64) -> bool {
        // A held trigger is owed as one full scan when the hold ends, which covers this library.
        if self.scan_hold.defer() {
            debug!("Scan of library {library_id} held back while the library tidy moves files");
            return true;
        }
        let now = tokio::time::Instant::now();
        let wait = {
            let mut scans = self.targeted.lock();
            let scan = scans.entry(library_id).or_default();
            match scan.last {
                Some(last) if now.duration_since(last) < self.scan_window => {
                    if scan.trailing {
                        // One more scan is already waiting; it starts after this call.
                        return true;
                    }
                    scan.trailing = true;
                    Some(self.scan_window - now.duration_since(last))
                }
                _ => {
                    scan.last = Some(now);
                    None
                }
            }
        };
        let Some(wait) = wait else {
            return Self::scan_one(&self.admin, self.scan_notice.as_deref(), library_id).await;
        };
        let admin = self.admin.clone();
        let targeted = Arc::clone(&self.targeted);
        let notice = self.scan_notice.clone();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            {
                let mut scans = targeted.lock();
                let scan = scans.entry(library_id).or_default();
                scan.trailing = false;
                scan.last = Some(tokio::time::Instant::now());
            }
            Self::scan_one(&admin, notice.as_deref(), library_id).await;
        });
        true
    }

    async fn get_scan_status(&self) -> Option<ScanStatus> {
        let result: anyhow::Result<Option<ScanStatus>> = async {
            // Navidrome answers getScanStatus only with a sign-in, so it gets the same admin
            // identity startScan uses.
            let base = self.settings.current().subsonic.url.clone().unwrap_or_default();
            let url = match self.nav_identity.get_scan_auth() {
                Some((user, token, salt)) => format!(
                    "{base}/rest/getScanStatus?f=json&c=octo&v=1.16.1&u={}&t={token}&s={salt}",
                    escape_data_string(&user)
                ),
                None => format!("{base}/rest/getScanStatus?f=json"),
            };
            let response = self.http.get(&url).send().await?;
            if !response.status().is_success() {
                return Ok(None);
            }
            let content = response.bytes().await?;
            let doc: Value = serde_json::from_slice(&content)?;
            read_scan_status(&doc)
        }
        .await;
        result.unwrap_or_else(|e| {
            debug!(error = %e, "Error getting Subsonic scan status");
            None
        })
    }

    async fn get_server_info(&self) -> Option<ServerInfo> {
        let result: anyhow::Result<Option<ServerInfo>> = async {
            let base = self.settings.current().subsonic.url.clone().unwrap_or_default();
            let url = match self.nav_identity.get_scan_auth() {
                Some((user, token, salt)) => format!(
                    "{base}/rest/ping?f=json&c=octo&v=1.16.1&u={}&t={token}&s={salt}",
                    escape_data_string(&user)
                ),
                None => format!("{base}/rest/ping?f=json&c=octo&v=1.16.1"),
            };
            let content = self.http.get(&url).send().await?.bytes().await?;
            let doc: Value = serde_json::from_slice(&content)?;
            Ok(ServerInfo::read(&doc))
        }
        .await;
        result.unwrap_or_else(|e| {
            debug!(error = %e, "Could not ask the Subsonic server who it is");
            None
        })
    }
}

/// `subsonic-response.scanStatus`, read strictly: a property of the wrong kind (`scanning` not
/// a bool, `count` not a number, a node that is not an object) is an error; a `count` that is a
/// number but not an `i32` is None.
fn read_scan_status(doc: &Value) -> anyhow::Result<Option<ScanStatus>> {
    fn property<'a>(element: &'a Value, name: &str) -> anyhow::Result<Option<&'a Value>> {
        match element {
            Value::Object(map) => Ok(map.get(name)),
            _ => anyhow::bail!("the element is not an object"),
        }
    }
    let Some(response) = property(doc, "subsonic-response")? else {
        return Ok(None);
    };
    let Some(scan_status) = property(response, "scanStatus")? else {
        return Ok(None);
    };
    let scanning = match property(scan_status, "scanning")? {
        Some(Value::Bool(b)) => *b,
        Some(_) => anyhow::bail!("scanning is not a boolean"),
        None => false,
    };
    // A number that is not an `i32` reads as None; a count that is not a number at all is
    // still an error.
    let count = match property(scan_status, "count")? {
        Some(Value::Number(n)) => n.as_i64().and_then(|n| i32::try_from(n).ok()),
        Some(_) => anyhow::bail!("count is not a number"),
        None => None,
    };
    let text = |name: &str| scan_status.get(name).and_then(Value::as_str).map(str::to_string);
    let number = |name: &str| scan_status.get(name).and_then(Value::as_i64);
    Ok(Some(ScanStatus {
        scanning,
        count,
        last_scan: text("lastScan"),
        elapsed_time: number("elapsedTime"),
        folder_count: number("folderCount"),
    }))
}

/// Whether a startScan answer is an "ok" envelope.
fn scan_started(content: &str) -> bool {
    serde_json::from_str::<Value>(content).is_ok_and(|doc| {
        doc.get("subsonic-response")
            .and_then(|envelope| envelope.get("status"))
            .and_then(Value::as_str)
            == Some("ok")
    })
}

#[cfg(test)]
#[path = "local_library_service_tests.rs"]
mod tests;
