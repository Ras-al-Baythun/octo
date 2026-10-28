//! The download service: an outside song's permanent copy from YouTube (the yt-dlp shim's MP3)
//! or Lidarr, and its instant preview stream from YouTube.
//!
//! [`OutsideDownloadService`] is the [`DownloadBackend`] of a [`BaseDownloadService`]: the
//! source-specific members, called back by the base with itself.
//! [`OutsideDownloadService::build`] puts the two together, and the base is what the app holds
//! as its `IDownloadService`.
//!
//! The optional services the base does not carry (Lidarr's import hand-off and track fetcher)
//! are the `Option` fields of [`OutsideDownloadParts`].

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures::TryStreamExt;
use octo_core::common::{dotnet, live_version};
use octo_core::fingerprint::{InconclusiveReason, VerificationVerdict};
use octo_core::models::domain::Song;
use octo_core::notifications::{NotificationEvent, NotificationEventType};
use octo_core::outside::{LengthSource, OutsideRouting, RoutingKind, StreamFacts};
use octo_core::settings::DownloadSource;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::external_id_registry::{ExternalIdRegistry, SharedRouting};
use super::outside_metadata_service::OutsideMetadataService;
use crate::services::common::acquisition_tracker::acquisition_event_kinds;
use crate::services::common::download_picks::LIDARR_SOURCE;
use crate::services::common::{
    AcquisitionState, BaseDownloadService, DownloadBackend, DownloadCore, DownloadServices,
    OperationCanceled, TrackDownload,
};
use crate::services::fingerprint::DownloadVerificationService;
use crate::services::i_download_service::DirectStreamInfo;
use crate::services::library::navidrome_song_path_resolver::get_full_path;
use crate::services::library::upgrade_queue::AudioSummary;
use crate::services::lidarr::{ILidarrTrackFetcher, LidarrImport, LidarrImportHandoff, LidarrTrackRequest};
use crate::services::you_tube::YouTubeResolver;
use octo_core::lidarr::lidarr_track_fetcher::LidarrStep;

/// Where the shim writes a download before Octo has decided its name. A dot folder, which
/// Navidrome's scanner skips (Scanner.IgnoreDotFolders, on by default), the same way it skips the
/// library-action quarantine.
pub const INCOMING_FOLDER_NAME: &str = ".octo-incoming";

/// A download that cannot be made as asked (no artist or title, no download path, no Lidarr,
/// audio YouTube could not decode). The library-action executor tries the next source on it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidOperationException(pub String);

/// No copy found, or the copy found is gone. Reported as an `io::Error` of kind `NotFound`, which
/// is how the library-action executor recognizes "no copy here".
fn file_not_found(message: String) -> anyhow::Error {
    std::io::Error::new(std::io::ErrorKind::NotFound, message).into()
}

/// The whole stream's size as a shim answer shows it: the body of a 200, or the total after the
/// slash of a partial answer's Content-Range (`bytes 0-1/12345`, or `bytes */12345` on a 416).
pub(crate) fn played_size(
    status: u16,
    content_length: Option<u64>,
    content_range: Option<&str>,
) -> Option<u64> {
    let total = content_range
        .and_then(|range| range.rsplit_once('/'))
        .and_then(|(_, total)| total.trim().parse::<u64>().ok());
    match status {
        200 => content_length.or(total),
        206 | 416 => total,
        _ => None,
    }
    .filter(|size| *size > 0)
}

/// The future, unless the caller gives up first, which is [`OperationCanceled`].
async fn or_canceled<T>(
    ct: &CancellationToken,
    work: impl Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        biased;
        _ = ct.cancelled() => Err(OperationCanceled.into()),
        result = work => result,
    }
}

/// What the service needs beyond the base's own services.
pub struct OutsideDownloadParts {
    pub verification: Arc<DownloadVerificationService>,
    pub youtube: Arc<YouTubeResolver>,
    pub id_registry: Arc<ExternalIdRegistry>,
    /// Lidarr's import hand-off, when there is one.
    pub lidarr_imports: Option<Arc<LidarrImportHandoff>>,
    /// Lidarr's track fetcher, when there is one.
    pub lidarr_fetcher: Option<Arc<dyn ILidarrTrackFetcher>>,
}

/// Hybrid download service:
///   - `get_direct_stream`  -> instant lossy preview via YouTube (yt-dlp)
///   - `download_track`     -> permanent copy: Lidarr when a caller asks for it by name, else
///     the YouTube MP3. Runs when the user stars a track, and in Permanent mode when one is
///     played.
pub struct OutsideDownloadService {
    verification: Arc<DownloadVerificationService>,
    youtube: Arc<YouTubeResolver>,
    id_registry: Arc<ExternalIdRegistry>,
    lidarr_imports: Option<Arc<LidarrImportHandoff>>,
    lidarr_fetcher: Option<Arc<dyn ILidarrTrackFetcher>>,
}

impl OutsideDownloadService {
    pub fn new(parts: OutsideDownloadParts) -> Self {
        OutsideDownloadService {
            verification: parts.verification,
            youtube: parts.youtube,
            id_registry: parts.id_registry,
            lidarr_imports: parts.lidarr_imports,
            lidarr_fetcher: parts.lidarr_fetcher,
        }
    }

    /// The download service as the app registers it (`AddSingleton<IDownloadService,
    /// OutsideDownloadService>()`): the base over this backend.
    pub fn build(
        core: DownloadCore,
        services: DownloadServices,
        parts: OutsideDownloadParts,
    ) -> (Arc<BaseDownloadService>, Arc<OutsideDownloadService>) {
        let backend = Arc::new(Self::new(parts));
        let base = BaseDownloadService::new(core, services, Arc::clone(&backend) as Arc<dyn DownloadBackend>);
        (base, backend)
    }

    fn routing_for(&self, external_id: &str) -> Option<SharedRouting> {
        self.id_registry.lookup(external_id).or_else(|| {
            OutsideMetadataService::try_decode_external_id(Some(external_id)).map(SharedRouting::new)
        })
    }

    // =========================================================================
    // Lidarr
    // =========================================================================

    /// A file a Lidarr heart brought in, taken into a job folder and held to the checks every
    /// download meets. AcoustID naming another recording, or a live take nobody asked for, throws
    /// the file away: Lidarr has no second copy, so the heart's next source tries instead. A
    /// FLAC made from an MP3 is kept and marked: it is still the right song.
    async fn adopt_lidarr_import(
        &self,
        base: &BaseDownloadService,
        routing: &OutsideRouting,
        download: &mut TrackDownload,
        import: LidarrImport,
    ) -> anyhow::Result<String> {
        let (artist, title) = names(routing);
        download.fetched_from = Some("Lidarr".to_string());
        if import.quiet {
            download.muted = true;
        }
        if !file_exists(&import.path) {
            return Err(file_not_found(format!(
                "Lidarr's file for '{artist} - {title}' is gone: {}",
                import.path
            )));
        }

        let job_dir = combine(
            &combine(&combine(&base.download_path(), INCOMING_FOLDER_NAME), "lidarr"),
            &new_guid(),
        );
        std::fs::create_dir_all(&job_dir)?;
        let local = combine(&job_dir, file_name(&import.path));
        if std::fs::rename(&import.path, &local).is_err() {
            // Another volume: copied, then the import removed.
            std::fs::copy(&import.path, &local)?;
            std::fs::remove_file(&import.path)?;
        }
        BaseDownloadService::try_remove_empty_parents(
            directory_name(&import.path).as_deref(),
            &base.download_path(),
        );

        let song = &mut download.song;
        if let (Some(provider), Some(id)) = (
            song.external_provider.clone().filter(|p| !p.is_empty()),
            song.external_id.clone().filter(|i| !i.is_empty()),
        ) {
            base.track(|t| t.stage(&provider, &id, AcquisitionState::Verifying, Some("Lidarr"), None));
        }
        let verdict = self
            .verification
            .verify(
                &local,
                routing.artist.as_deref(),
                routing.title.as_deref(),
                song.isrc.as_deref().or(routing.isrc.as_deref()),
                !live_version::requested(routing.title.as_deref(), routing.album.as_deref()),
            )
            .await;
        if verdict.verdict == VerificationVerdict::Mismatch {
            warn!(
                "Lidarr brought {} for '{artist} - {title}' (AcoustID score {}); discarding it",
                verdict.describe(),
                percent(verdict.score)
            );
            // Swept with the job folders when it cannot be removed now.
            let _ = std::fs::remove_file(&local);
            return Err(file_not_found(format!(
                "AcoustID identified Lidarr's file as {}",
                verdict.describe()
            )));
        }
        let spectrum = self
            .verification
            .check_lossless(&local, routing.artist.as_deref(), routing.title.as_deref())
            .await;
        if spectrum.is_likely_lossy() {
            warn!(
                "Lidarr's copy of '{artist} - {title}' is {}; keeping it, marked as a transcode",
                spectrum.describe()
            );
            song.transcoded_from = spectrum.estimate.clone();
        }
        info!("Took Lidarr's import of '{artist} - {title}' into the pipeline: {local}");
        Ok(local)
    }

    /// One song through Lidarr, copied into a job folder under the incoming dot folder, which no
    /// scan looks at. From there it is identified, checked and swapped in like any download.
    async fn download_via_lidarr(
        &self,
        base: &BaseDownloadService,
        routing: &OutsideRouting,
        download: &mut TrackDownload,
        lossless_only: bool,
        ct: &CancellationToken,
    ) -> anyhow::Result<String> {
        // A heart's album that Lidarr already brought in: the file is here, and only needs taking.
        if let Some(external_id) = download.song.external_id.clone().filter(|id| !id.is_empty())
            && let Some(import) = self.lidarr_imports.as_ref().and_then(|h| h.take(&external_id))
        {
            return self.adopt_lidarr_import(base, routing, download, import).await;
        }

        let Some(fetcher) = &self.lidarr_fetcher else {
            return Err(InvalidOperationException("Lidarr is not available on this server.".into()).into());
        };
        let song = &download.song;
        if let (Some(provider), Some(id)) = (
            song.external_provider.as_deref().filter(|p| !p.is_empty()),
            song.external_id.as_deref().filter(|i| !i.is_empty()),
        ) {
            base.track(|t| {
                t.stage(
                    provider,
                    id,
                    AcquisitionState::Searching,
                    Some("Lidarr"),
                    Some("Lidarr is searching the album"),
                )
            });
        }
        let job_dir = combine(
            &combine(&combine(&base.download_path(), INCOMING_FOLDER_NAME), "lidarr"),
            &new_guid(),
        );
        // A release picked in Find songs is grabbed as it is, instead of Lidarr's own search. One
        // the person said to take whatever its quality is taken in any quality.
        let external_id = song.external_id.clone().unwrap_or_default();
        let taken = song
            .external_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .and_then(|id| base.services().picks.as_ref()?.take(id, LIDARR_SOURCE));
        let lossless_only = lossless_only && taken.as_ref().is_none_or(|pick| pick.anyway.is_none());
        let picked = taken.and_then(|pick| pick.release());
        let base_arc = base.arc();
        let step_id = external_id.clone();
        let request = LidarrTrackRequest {
            artist: routing.artist.clone().unwrap_or_default(),
            title: routing.title.clone().unwrap_or_default(),
            album: routing.album.clone(),
            duration_seconds: routing.duration.or(song.duration),
            lossless_only,
            original_path: download.replacing_path.clone(),
            release: picked,
            step: Some(LidarrStep::new(move |text, detail| {
                base_arc.log_step(&step_id, acquisition_event_kinds::TRY, text, detail)
            })),
        };
        download.fetched_from = Some("Lidarr".to_string());
        let path = fetcher.fetch(&request, &job_dir, ct).await?;
        let (quality, _) = AudioSummary::describe(Some(&path));
        base.log_step(
            &external_id,
            acquisition_event_kinds::FOUND,
            "Lidarr brought a copy",
            quality.as_deref(),
        );
        Ok(path)
    }

    // =========================================================================
    // YouTube
    // =========================================================================

    // Lossy MP3 via the yt-dlp shim's /download. The shim writes <dest>.mp3 into the staging
    // folder with clean tags and a cover; the base's placement moves it once the tags are settled.
    async fn download_via_you_tube(
        &self,
        base: &BaseDownloadService,
        routing: &SharedRouting,
        download: &mut TrackDownload,
        announce_start: bool,
        ct: &CancellationToken,
    ) -> anyhow::Result<String> {
        let download_path = base.download_path();
        if download_path.is_empty() {
            return Err(InvalidOperationException("DownloadPath is not configured".into()).into());
        }

        let provider = base.provider_name().to_string();
        let track_key = download.song.external_id.clone().unwrap_or_default();
        base.track(|t| {
            t.stage(
                &provider,
                &track_key,
                AcquisitionState::Searching,
                Some("YouTube"),
                None,
            )
        });

        let r = routing.snapshot();
        let (artist, title) = names(&r);
        let mut video_id = r.you_tube_id.clone().filter(|v| !v.is_empty());
        if video_id.is_none() {
            let hit = self
                .youtube
                .search(&format!("{artist} {title}"), r.duration, false)
                .await
                .filter(|h| !h.video_id.is_empty());
            if let Some(hit) = &hit {
                self.id_registry.set_you_tube_id(&track_key, &hit.video_id);
                self.id_registry
                    .remember_stream(&track_key, &hit.video_id, &hit.stream);
            }
            video_id = hit.map(|h| h.video_id);
        }
        let Some(video_id) = video_id else {
            return Err(file_not_found(format!(
                "No YouTube match for '{artist} - {title}'"
            )));
        };
        base.log_step(
            &track_key,
            acquisition_event_kinds::FOUND,
            "Found it on YouTube",
            Some(&format!("Video {video_id}, saved as an MP3")),
        );

        if !download.suppress_notify && announce_start {
            base.notifications().notify(NotificationEvent {
                artist: r.artist.clone(),
                title: r.title.clone(),
                album: r.album.clone(),
                source: Some("YouTube".into()),
                format: Some("MP3".into()),
                duration_seconds: r.duration,
                ..NotificationEvent::new(NotificationEventType::DownloadStarted)
            });
        }

        // Staged, not written into the library. Writing straight to the layout path let the shim
        // overwrite a different file that happened to share the name before anything could
        // protect it. Extension left empty: the shim appends .mp3 itself.
        let incoming = combine(&download_path, INCOMING_FOLDER_NAME);
        sweep_incoming(&incoming);
        let dest_without_ext = combine(&incoming, &format!("{video_id}-{}", new_guid()));

        // The shim answers only once the file is written, so there is nothing to count here.
        base.track(|t| t.transfer(&provider, &track_key, None, None, None, Some("YouTube")));
        let path = or_canceled(ct, async {
            Ok(self
                .youtube
                .download(
                    &video_id,
                    &dest_without_ext,
                    r.artist.as_deref(),
                    r.title.as_deref(),
                    r.shown_duration.or(r.duration),
                )
                .await)
        })
        .await?;
        let Some(path) = path.filter(|p| !p.is_empty() && file_exists(p)) else {
            return Err(file_not_found(format!(
                "YouTube MP3 download failed for '{artist} - {title}'"
            )));
        };

        info!("YouTube MP3 download complete: {path}");

        // Identification only. YouTube has no second candidate to fall back to, so a
        // disagreement is something to ask a person about (the Review playlist), never a reason
        // to throw the song away.
        base.track(|t| t.stage(&provider, &track_key, AcquisitionState::Verifying, None, None));
        let song = &mut download.song;
        let mut verdict = self
            .verification
            .verify(
                &path,
                r.artist.as_deref(),
                r.title.as_deref(),
                song.isrc.as_deref().or(r.isrc.as_deref()),
                false,
            )
            .await;
        if verdict.verdict == VerificationVerdict::Mismatch {
            if verdict.r#match.is_none() && verdict.matched_title.as_deref().is_none_or(str::is_empty) {
                // No decodable audio at all: a broken file, not a question.
                let _ = std::fs::remove_file(&path);
                return Err(InvalidOperationException(format!(
                    "YouTube delivered no decodable audio for '{artist} - {title}'"
                ))
                .into());
            }
            warn!(
                "AcoustID says the YouTube file for '{artist} - {title}' is {}; keeping it and asking about it",
                verdict.describe()
            );
            base.log_step(
                &track_key,
                acquisition_event_kinds::CHECK,
                &format!("AcoustID says this is {}", verdict.describe()),
                Some("YouTube has no other copy, so it is kept and goes to your Review playlist"),
            );
            verdict.verdict = VerificationVerdict::Inconclusive;
            verdict.reason = InconclusiveReason::SourceDisagreed;
        } else {
            verdict.apply_tags_to(song);
        }
        song.verification = Some(Box::new(verdict));
        Ok(path)
    }
}

#[async_trait]
impl DownloadBackend for OutsideDownloadService {
    fn provider_name(&self) -> &str {
        OutsideMetadataService::PROVIDER_NAME
    }

    /// The service needs nothing beyond the shim it is built with, so it is always available; a
    /// shim that is down fails the download that asks it.
    async fn is_available(&self, _base: &BaseDownloadService) -> bool {
        true
    }

    // Octo's album ids ARE the external id, so this is identity plus a kind check that
    // stops a song or artist id being walked as if it were an album.
    fn extract_external_id_from_album_id(&self, album_id: &str) -> Option<String> {
        self.id_registry
            .lookup(album_id)
            .filter(|routing| routing.lock().kind == RoutingKind::Album)
            .map(|_| album_id.to_string())
    }

    /// Restore an album track's routing if the registry evicted it mid-download.
    /// The fields here MUST match what OutsideMetadataService's get_album registered
    /// (YouTubeId left None, the Song's own Duration) or this hashes to a different id and
    /// fails to restore anything. Routings are mutated in place elsewhere, so rebuild from
    /// the Song, which still carries the values used at registration time.
    fn ensure_routing_registered(&self, track: &Song) {
        let Some(external_id) = track.external_id.as_deref().filter(|id| !id.is_empty()) else {
            return;
        };
        if self.id_registry.lookup(external_id).is_some() {
            return;
        }

        self.id_registry.register(OutsideRouting {
            kind: RoutingKind::Song,
            artist: Some(track.artist.clone()),
            title: Some(track.title.clone()),
            album: Some(track.album.clone()),
            duration: track.duration,
            track: track.track,
            disc_number: track.disc_number,
            total_tracks: track.total_tracks,
            isrc: track.isrc.clone(),
            ..Default::default()
        });
    }

    // =========================================================================
    // Streaming path (every play of an unowned radio track)
    // =========================================================================
    async fn get_direct_stream(
        &self,
        _base: &BaseDownloadService,
        external_provider: &str,
        external_id: &str,
        range_header: Option<&str>,
        _cancellation_token: &CancellationToken,
    ) -> anyhow::Result<Option<DirectStreamInfo>> {
        if !dotnet::eq_ignore_case(external_provider, OutsideMetadataService::PROVIDER_NAME) {
            return Ok(None);
        }

        let Some(routing) = self.routing_for(external_id) else {
            return Ok(None);
        };

        let r = routing.snapshot();
        let mut video_id = r.you_tube_id.clone().filter(|v| !v.is_empty());
        if video_id.is_none() && r.has_artist_title() {
            let (artist, title) = names(&r);
            // YouTube Music's row for the song first: one quick search, and the stream is
            // resolved as it opens. Else the shim's yt-dlp search, which resolves both at once.
            let hit = match self.youtube.music_song(&artist, &title, r.duration).await {
                Some(hit) => {
                    debug!(
                        "play of '{artist} - {title}': video {} from YouTube Music",
                        hit.video_id
                    );
                    Some(hit)
                }
                None => {
                    let hit = self
                        .youtube
                        .search(&format!("{artist} {title}"), r.duration, false)
                        .await
                        .filter(|h| !h.video_id.is_empty());
                    if let Some(hit) = &hit {
                        debug!("play of '{artist} - {title}': video {} from yt-dlp", hit.video_id);
                    }
                    hit
                }
            };
            // Pin it on the song so a second click on the same placeholder skips the
            // yt-dlp ytsearch1: round trip: that 3-8s saving is the difference between
            // Arpeggi (~10s HTTP timeout) playing the song or canceling and falling back
            // to a local one. Every later stream request for this id reads the same pin.
            if let Some(hit) = &hit {
                self.id_registry.set_you_tube_id(external_id, &hit.video_id);
                self.id_registry
                    .remember_stream(external_id, &hit.video_id, &hit.stream);
                // The video that plays says how long the song runs, for every later row.
                if !self
                    .id_registry
                    .remember_length(external_id, hit.duration, LengthSource::Video)
                {
                    routing.remember_length(hit.duration, LengthSource::Video);
                }
            }
            video_id = hit.map(|h| h.video_id);
        }
        let Some(video_id) = video_id else {
            return Ok(None);
        };

        let Some(opened) = self.youtube.open_stream(&video_id, range_header).await else {
            warn!("yt-dlp shim failed to open stream for vid={video_id}");
            return Ok(None);
        };

        // The bytes on their way are the last word on the size.
        if let Some(size) = played_size(
            opened.status_code,
            opened.content_length,
            opened.content_range.as_deref(),
        ) {
            self.id_registry.remember_stream(
                external_id,
                &video_id,
                &StreamFacts {
                    size: Some(size),
                    size_confirmed: true,
                    ..Default::default()
                },
            );
        }

        info!(
            "YouTube preview '{} - {}' (vid={video_id}, status={}, {} bytes{})",
            r.artist.as_deref().unwrap_or(""),
            r.title.as_deref().unwrap_or(""),
            opened.status_code,
            opened.content_length.map(|l| l.to_string()).unwrap_or_default(),
            opened
                .content_range
                .as_deref()
                .map(|range| format!(", range={range}"))
                .unwrap_or_default()
        );

        // The response owns the shim's connection: dropping the stream closes it.
        Ok(Some(DirectStreamInfo {
            audio_stream: Box::pin(opened.response.bytes_stream().map_err(std::io::Error::other)),
            content_type: opened.content_type,
            content_length: opened.content_length,
            quality: Some("youtube-m4a".to_string()),
            status_code: opened.status_code,
            content_range: opened.content_range,
        }))
    }

    // =========================================================================
    // Permanent download path (a star, or a play while in Permanent mode)
    // =========================================================================
    async fn download_track(
        &self,
        base: &BaseDownloadService,
        download: &mut TrackDownload,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<String> {
        let external_id = download.song.external_id.clone().unwrap_or_default();
        let routing = self
            .routing_for(&external_id)
            .filter(|routing| routing.lock().has_artist_title());
        let Some(routing) = routing else {
            return Err(InvalidOperationException(format!(
                "Cannot download '{} - {}': missing artist/title in external id",
                download.song.artist, download.song.title
            ))
            .into());
        };

        // Only ever asked for by name: a library action's replacement, or a heart whose chain
        // says so. DownloadSource set to Lidarr means hearts go to Lidarr, and every other
        // download still uses YouTube below.
        if download.source_override == Some(DownloadSource::Lidarr) {
            let snapshot = routing.snapshot();
            let lossless_only = download.upgrade_search;
            return self
                .download_via_lidarr(base, &snapshot, download, lossless_only, cancellation_token)
                .await;
        }

        // DownloadOnStar decides WHETHER to download; the copy itself is YouTube's MP3.
        self.download_via_you_tube(base, &routing, download, true, cancellation_token)
            .await
    }
}

/// The folder names a finished download is never taken from: Octo's own staging, where a file
/// belongs to one download and a search by name must never hand it to another.
pub fn excluded_folder_names() -> Vec<String> {
    vec![INCOMING_FOLDER_NAME.into()]
}

/// Clear what a crash or a failed download left in the staging folder. A day old is long past
/// any download still in flight, and nothing outside this one folder is touched.
fn sweep_incoming(incoming: &str) {
    let sweep = || -> std::io::Result<()> {
        if !Path::new(incoming).is_dir() {
            return Ok(());
        }
        let root = format!("{}/", get_full_path(incoming).trim_end_matches('/'));
        for file in files_in(incoming)? {
            if !get_full_path(&file).starts_with(&root) {
                continue;
            }
            if age_of(&std::fs::metadata(&file)?.modified()?) < Duration::from_secs(24 * 3600) {
                continue;
            }
            std::fs::remove_file(&file)?;
            info!("Removed a stale staged download {file}");
        }
        Ok(())
    };
    if let Err(e) = sweep() {
        debug!("Could not sweep {incoming}: {e}");
    }
}

// ---- small helpers -------------------------------------------------------------------------

// ---- small helpers -------------------------------------------------------------------------

/// The routing's artist and title, for messages.
fn names(routing: &OutsideRouting) -> (String, String) {
    (
        routing.artist.clone().unwrap_or_default(),
        routing.title.clone().unwrap_or_default(),
    )
}

/// A whole percentage, for the log.
fn percent(fraction: f64) -> String {
    format!("{}%", dotnet::round(fraction * 100.0, 0))
}

/// A new random UUID as 32 hex digits, no dashes.
fn new_guid() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// `b` under `a`, or `b` alone when it is absolute or `a` is empty.
fn combine(a: &str, b: &str) -> String {
    if b.starts_with('/') || a.is_empty() {
        b.to_string()
    } else if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// The last segment of a path.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The folder a path is in; None when it has no slash.
fn directory_name(path: &str) -> Option<String> {
    let slash = path.rfind('/')?;
    Some(if slash == 0 {
        "/".to_string()
    } else {
        path[..slash].to_string()
    })
}

fn file_exists(path: &str) -> bool {
    !path.is_empty() && Path::new(path).is_file()
}

fn age_of(time: &SystemTime) -> Duration {
    SystemTime::now().duration_since(*time).unwrap_or(Duration::ZERO)
}

/// The files directly in a folder.
fn files_in(folder: &str) -> std::io::Result<Vec<String>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(folder)? {
        let path = entry?.path();
        if path.is_file() {
            files.push(path.to_string_lossy().into_owned());
        }
    }
    Ok(files)
}

#[cfg(test)]
#[path = "outside_download_service_tests.rs"]
mod tests;
