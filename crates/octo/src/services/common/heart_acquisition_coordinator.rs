//! The heart acquisition coordinator: which source a heart or a play is handed to.

use std::collections::HashSet;
use std::sync::{Arc, OnceLock};

use octo_core::settings::{DownloadSource, HeartDownloadSource, SettingsStore};
use parking_lot::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::TrackAcquisitionQueue;
use super::{AcquisitionState, AcquisitionTarget, AcquisitionTracker, StarOnArrival};
use crate::services::family::{FamilyPool, LibraryScope, LibraryWriteGate, WriteDecision};
use crate::services::i_download_service::IDownloadService;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::{HeartOwnership, OwnedCopy, UpgradeSources};
use crate::services::lidarr::ILidarrHeartAcquisitionService;
use crate::services::soulseek::ExternalIdRegistry;

/// The coordinator's optional collaborators, each `None` by default.
#[derive(Default, Clone)]
pub struct HeartCoordinatorExtras {
    /// The chain is the only place that knows which failure is the last, so it is the one that
    /// reports it. Without one, nothing is reported.
    pub tracker: Option<Arc<AcquisitionTracker>>,
    pub id_registry: Option<Arc<ExternalIdRegistry>>,
    /// Without it every heart goes down the chain, and the download itself notices a song that
    /// is already there, as before.
    pub owned: Option<Arc<HeartOwnership>>,
    pub stars: Option<Arc<StarOnArrival>>,
}

/// How one song's heart chain ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackOutcome {
    /// A direct source got it; the file is here (a pool file for a member).
    Landed(String),
    /// The library (in the scope the chain looked in) already had it; it was favorited.
    AlreadyThere,
    /// Lidarr took it on, for the shared library.
    HandedToLidarr,
    /// Every source failed, with the last one's reason.
    Failed(String),
}

/// What the coordinator uses for family members: the write gate (where a member's play goes),
/// the family pool (members' album songs are starred where they land) and the catalog (an
/// album heart for a member is fetched song by song).
#[derive(Clone)]
pub struct FamilyHooks {
    pub gate: Arc<LibraryWriteGate>,
    pub pool: Arc<FamilyPool>,
    pub metadata: Arc<dyn IMusicMetadataService>,
}

/// Routes explicit heart gestures, and plays when DownloadOnPlay or LidarrAlbumOnPlay ask
/// for it.
pub struct HeartAcquisitionCoordinator {
    /// The chain is read at every heart.
    settings: Arc<SettingsStore>,
    direct_queue: Arc<TrackAcquisitionQueue>,
    direct_downloads: Arc<dyn IDownloadService>,
    lidarr: Arc<dyn ILidarrHeartAcquisitionService>,
    extras: HeartCoordinatorExtras,

    // Track ids handed to Lidarr on play, so a replay does not search every indexer again.
    // Cleared when full rather than aged: forgetting costs at most one more search.
    lidarr_plays: Mutex<HashSet<String>>,

    // Set once Family's services exist; without them every heart and play goes to the shared
    // library, as always.
    family: OnceLock<FamilyHooks>,
}

impl HeartAcquisitionCoordinator {
    const LIDARR_PLAY_MEMORY: usize = 10_000;

    pub fn new(
        settings: Arc<SettingsStore>,
        direct_queue: Arc<TrackAcquisitionQueue>,
        direct_downloads: Arc<dyn IDownloadService>,
        lidarr: Arc<dyn ILidarrHeartAcquisitionService>,
        extras: HeartCoordinatorExtras,
    ) -> Arc<Self> {
        Arc::new(HeartAcquisitionCoordinator {
            settings,
            direct_queue,
            direct_downloads,
            lidarr,
            extras,
            lidarr_plays: Mutex::new(HashSet::new()),
            family: OnceLock::new(),
        })
    }

    /// Gives the coordinator Family's services. Once only; a second call changes nothing.
    pub fn use_family(&self, hooks: FamilyHooks) {
        let _ = self.family.set(hooks);
    }

    /// Where an import list's song goes for `user`, or the words that say why it may not be
    /// fetched now. A member who must ask first has songs queued only for a list an approver
    /// said yes to, so theirs go to their own library too.
    pub fn import_target(&self, user: &str) -> Result<AcquisitionTarget, String> {
        let Some(family) = self.family.get() else {
            return Ok(AcquisitionTarget::Shared);
        };
        match family.gate.decide(Some(user)) {
            WriteDecision::Shared => Ok(AcquisitionTarget::Shared),
            WriteDecision::Member(user) => Ok(AcquisitionTarget::Member(user)),
            WriteDecision::Request { user, full: None } => Ok(AcquisitionTarget::Member(user)),
            WriteDecision::Request { full: Some(full), .. } => Err(full),
            WriteDecision::Refuse { message, .. } => Err(message),
        }
    }

    fn tracker(&self) -> Option<&Arc<AcquisitionTracker>> {
        self.extras.tracker.as_ref()
    }

    pub fn queue_track(self: &Arc<Self>, provider: &str, external_id: &str, requested_by: Option<&str>) {
        self.queue_track_for(provider, external_id, requested_by, AcquisitionTarget::Shared);
    }

    /// [`Self::queue_track`] into one library: the shared one, or a family member's own.
    pub fn queue_track_for(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        target: AcquisitionTarget,
    ) {
        let coordinator = Arc::clone(self);
        let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
        // Fire and forget: a chain that stops on an error is left as it stands.
        tokio::spawn(async move {
            let _ = coordinator
                .acquire_track_for(&provider, &external_id, requested_by.as_deref(), &target)
                .await;
        });
    }

    /// A client started an external track from its first byte.
    pub fn queue_play(
        self: &Arc<Self>,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
    ) {
        let settings = self.settings.current().subsonic.clone();
        // A family member's play keeps a copy only when they may add songs at once, and then in
        // their own library; `owner` is who signed in. Everyone else plays as always.
        let target = match self.family.get() {
            Some(family) => match family.gate.play_target(owner) {
                Some(target) => target,
                None => return,
            },
            None => AcquisitionTarget::Shared,
        };
        if settings.download_on_play
            && let Some(source) = self.play_source()
        {
            self.queue_play_download(
                provider,
                external_id,
                source,
                requested_by,
                client_id,
                owner,
                &target,
            );
        }
        // Lidarr adds whole albums to the shared library, never to a member's own.
        if !settings.lidarr_album_on_play || target != AcquisitionTarget::Shared {
            return;
        }
        let key = format!("{provider}:{external_id}");
        let added = {
            let mut plays = self.lidarr_plays.lock();
            if plays.len() >= Self::LIDARR_PLAY_MEMORY {
                plays.clear();
            }
            plays.insert(key.clone())
        };
        if added {
            let coordinator = Arc::clone(self);
            let (provider, external_id, requested_by) = owned3(provider, external_id, requested_by);
            tokio::spawn(async move {
                coordinator
                    .hand_play_to_lidarr(&key, &provider, &external_id, requested_by.as_deref())
                    .await
            });
        }
    }

    #[allow(clippy::too_many_arguments)] // the play's facts and whose library it is for
    fn queue_play_download(
        &self,
        provider: &str,
        external_id: &str,
        source: DownloadSource,
        requested_by: Option<&str>,
        client_id: Option<&str>,
        owner: Option<&str>,
        target: &AcquisitionTarget,
    ) {
        let on_queued = || {
            // Same row a heart gets, opened before the worker can report its first stage.
            let routing = self
                .extras
                .id_registry
                .as_ref()
                .and_then(|r| r.lookup(external_id))
                .map(|shared| shared.snapshot());
            if let Some(tracker) = self.tracker() {
                tracker.begin(
                    provider,
                    external_id,
                    client_id,
                    owner,
                    routing.as_ref().and_then(|r| r.artist.as_deref()),
                    routing.as_ref().and_then(|r| r.title.as_deref()),
                    routing.as_ref().and_then(|r| r.album.as_deref()),
                );
                tracker.stage(
                    provider,
                    external_id,
                    AcquisitionState::Queued,
                    Some(UpgradeSources::word(source)),
                    None,
                );
            }
        };
        let Some(request) = self.direct_queue.try_enqueue_play_for(
            target,
            provider,
            external_id,
            source,
            requested_by,
            Some(&on_queued),
        ) else {
            return;
        };
        let tracker = self.tracker().cloned();
        let (provider, external_id) = (provider.to_string(), external_id.to_string());
        tokio::spawn(async move {
            let Err(reason) = request.completion.wait().await else {
                return;
            };
            debug!("Play download failed for {provider}:{external_id}: {reason}");
            // A heart that joined owns the row, and may still be trying its next source.
            if !request.is_star
                && !request.heart_joined()
                && let Some(tracker) = tracker
            {
                tracker.fail(&provider, &external_id, Some(&reason.to_string()));
            }
        });
    }

    async fn hand_play_to_lidarr(
        &self,
        key: &str,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
    ) {
        match self
            .lidarr
            .try_acquire_track(provider, external_id, false, requested_by)
            .await
        {
            Ok(true) => return,
            Ok(false) => debug!("Lidarr took no album for played track {provider}:{external_id}"),
            Err(e) => debug!("Lidarr hand-off failed for played track {provider}:{external_id}: {e}"),
        }
        // Not handed off, so the next play of this track tries again.
        self.lidarr_plays.lock().remove(key);
    }

    pub fn queue_album(
        self: &Arc<Self>,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
    ) {
        let coordinator = Arc::clone(self);
        let (provider, album_id, requested_by) = owned3(provider, album_external_id, requested_by);
        tokio::spawn(async move {
            let _ = coordinator
                .acquire_album(&provider, &album_id, requested_by.as_deref())
                .await;
        });
    }

    /// [`Self::queue_album`] into one library. A member's album comes song by song into the
    /// family pool, each song starred for them where it lands.
    pub fn queue_album_for(
        self: &Arc<Self>,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        target: AcquisitionTarget,
    ) {
        let AcquisitionTarget::Member(user) = target else {
            self.queue_album(provider, album_external_id, requested_by);
            return;
        };
        let coordinator = Arc::clone(self);
        let (provider, album_id, requested_by) = owned3(provider, album_external_id, requested_by);
        tokio::spawn(async move {
            coordinator
                .acquire_album_for_member(&provider, &album_id, requested_by.as_deref(), &user, true)
                .await;
        });
    }

    /// Every song of an outside album into `user`'s own library, one heart chain each (no
    /// Lidarr, which fills the shared library). With `star`, each song is starred for them once
    /// it shows in their library. Answers where each song that landed is.
    pub async fn acquire_album_for_member(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
        user: &str,
        star: bool,
    ) -> Vec<String> {
        let Some(family) = self.family.get().cloned() else {
            return Vec::new();
        };
        let Some(album) = family.metadata.get_album(provider, album_external_id).await else {
            if let Some(tracker) = self.tracker() {
                tracker.fail_album(
                    provider,
                    album_external_id,
                    Some("Octo could no longer look this album up, so nothing was downloaded."),
                );
            }
            return Vec::new();
        };
        let tracks: Vec<_> = album
            .songs
            .iter()
            .filter(|song| song.external_id.as_deref().is_some_and(|id| !id.is_empty()))
            .collect();
        if let Some(tracker) = self.tracker() {
            let listed: Vec<(String, Option<String>, Option<String>, Option<String>)> = tracks
                .iter()
                .map(|song| {
                    (
                        song.external_id.clone().unwrap_or_default(),
                        Some(song.artist.clone()),
                        Some(song.title.clone()),
                        Some(album.title.clone()),
                    )
                })
                .collect();
            tracker.announce(provider, Some(album_external_id), None, &listed);
        }
        info!(
            "Album '{}' for {user}: {} song(s) into their own library",
            album.title,
            tracks.len()
        );
        let target = AcquisitionTarget::Member(user.to_string());
        let mut landed = Vec::new();
        for song in tracks {
            let track_provider = song.external_provider.as_deref().unwrap_or(provider);
            let track_id = song.external_id.as_deref().unwrap_or_default();
            match self
                .acquire_track_outcome(track_provider, track_id, requested_by, &target)
                .await
            {
                Ok(TrackOutcome::Landed(path)) => {
                    if star {
                        let shown = family.pool.link_path(&path, user).unwrap_or_else(|| path.clone());
                        family.pool.star_when_indexed(user, &shown);
                    }
                    landed.push(path);
                }
                Ok(_) => {}
                Err(e) => warn!("Album song {track_provider}:{track_id} for {user} failed: {e}"),
            }
        }
        landed
    }

    /// The song chain. `Err` for a Lidarr step that failed.
    #[cfg(test)]
    pub(crate) async fn acquire_track(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
    ) -> anyhow::Result<()> {
        self.acquire_track_for(provider, external_id, requested_by, &AcquisitionTarget::Shared)
            .await
    }

    /// [`Self::acquire_track`] into one library.
    pub(crate) async fn acquire_track_for(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        target: &AcquisitionTarget,
    ) -> anyhow::Result<()> {
        self.acquire_track_outcome(provider, external_id, requested_by, target)
            .await
            .map(|_| ())
    }

    /// The song chain into one library, answering how it ended. A member's chain skips Lidarr,
    /// which fills the shared library.
    pub async fn acquire_track_outcome(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        target: &AcquisitionTarget,
    ) -> anyhow::Result<TrackOutcome> {
        let scope = match target {
            AcquisitionTarget::Shared => LibraryScope::Shared,
            AcquisitionTarget::Member(user) => LibraryScope::Viewer(user.clone()),
        };
        if self
            .already_yours(provider, external_id, requested_by, &scope)
            .await
        {
            return Ok(TrackOutcome::AlreadyThere);
        }
        let steps: Vec<HeartDownloadSource> = self
            .enabled_steps(false)
            .into_iter()
            .filter(|step| *target == AcquisitionTarget::Shared || *step != HeartDownloadSource::Lidarr)
            .collect();
        for (index, &step) in steps.iter().enumerate() {
            let is_last = index == steps.len() - 1;
            // One entry for the whole chain. The first direct source is still waiting for the
            // worker; every later source, and Lidarr, starts over by looking.
            if let Some(tracker) = self.tracker() {
                let note = (index > 0).then(|| {
                    format!(
                        "{} couldn't get it, trying {}",
                        source_name(steps[index - 1]),
                        source_name(step)
                    )
                });
                tracker.stage(
                    provider,
                    external_id,
                    if index == 0 && step != HeartDownloadSource::Lidarr {
                        AcquisitionState::Queued
                    } else {
                        AcquisitionState::Searching
                    },
                    Some(source_name(step)),
                    note.as_deref(),
                );
            }
            if step == HeartDownloadSource::Lidarr {
                // A Lidarr that fails ends the chain with its error, as its exception did.
                if self
                    .lidarr
                    .try_acquire_track(provider, external_id, is_last, requested_by)
                    .await?
                {
                    return Ok(TrackOutcome::HandedToLidarr);
                }
                continue;
            }

            let completion = self.direct_queue.enqueue_for(
                target,
                false,
                provider,
                external_id,
                true,
                false,
                true,
                Some(to_direct_source(step)),
                is_last,
                // Passed on every step, not only the first. A track that fails its way
                // down the source chain is still the same person's star.
                requested_by,
                false,
                None,
            );
            let failure = match completion.wait().await {
                Ok(path) => return Ok(TrackOutcome::Landed(path)),
                Err(failure) => failure,
            };

            // A muted mid-chain failure must still leave a trace, or a track that
            // silently fell through every source is undiagnosable from the logs.
            warn!(
                "Heart source {} failed for track {provider}:{external_id}: {failure}",
                source_enum_name(step)
            );
            if is_last {
                if let Some(tracker) = self.tracker() {
                    tracker.fail(provider, external_id, Some(&failure.to_string()));
                }
                return Ok(TrackOutcome::Failed(failure.to_string()));
            }
            // The next enabled source owns the fallback.
        }
        // Only reached when the last source was Lidarr and it said no. It records its own
        // reason first; this is the fallback when it could not.
        const NO_SOURCE: &str = "No download source could get this song.";
        if let Some(tracker) = self.tracker() {
            tracker.fail(provider, external_id, Some(NO_SOURCE));
        }
        Ok(TrackOutcome::Failed(NO_SOURCE.to_string()))
    }

    /// The album chain. `Err` for a Lidarr step that failed.
    pub(crate) async fn acquire_album(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
    ) -> anyhow::Result<()> {
        if self
            .album_already_yours(provider, album_external_id, requested_by)
            .await
        {
            return Ok(());
        }
        let steps = self.enabled_steps(true);
        for (index, &step) in steps.iter().enumerate() {
            let is_last = index == steps.len() - 1;
            if step == HeartDownloadSource::Lidarr {
                if self
                    .lidarr
                    .try_acquire_album(provider, album_external_id, is_last, requested_by)
                    .await?
                {
                    return Ok(());
                }
                continue;
            }

            let failure = match self
                .direct_downloads
                .download_album_with_source(
                    provider,
                    album_external_id,
                    to_direct_source(step),
                    !is_last,
                    &CancellationToken::new(),
                    requested_by.map(|r| vec![r.to_string()]),
                )
                .await
            {
                Ok(true) => return Ok(()),
                Ok(false) => None,
                Err(e) => Some(e),
            };

            if let Some(failure) = failure {
                warn!(
                    "Heart source {} failed for album {provider}:{album_external_id}: {failure}",
                    source_enum_name(step)
                );
                if is_last {
                    if let Some(tracker) = self.tracker() {
                        tracker.fail_album(provider, album_external_id, Some(&failure.to_string()));
                    }
                    return Ok(());
                }
            }
            // Continue down the configured priority list.
        }
        // Every source has had its go. Tracks the last walk already settled keep what it said;
        // this only closes the ones nothing finished.
        if let Some(tracker) = self.tracker() {
            tracker.fail_album(
                provider,
                album_external_id,
                Some("No download source could get this track."),
            );
        }
        Ok(())
    }

    fn enabled_steps(&self, album_heart: bool) -> Vec<HeartDownloadSource> {
        self.settings
            .current()
            .subsonic
            .effective_heart_download_sources()
            .into_iter()
            .filter(|step| {
                if album_heart {
                    step.album_enabled == Some(true)
                } else {
                    step.song_enabled == Some(true)
                }
            })
            .map(|step| step.source)
            .collect()
    }

    /// The source a play's copy comes from: the first direct source with song hearts enabled,
    /// which is YouTube; None when no direct source is.
    fn play_source(&self) -> Option<DownloadSource> {
        self.enabled_steps(false)
            .into_iter()
            .find(|s| *s != HeartDownloadSource::Lidarr)
            .map(to_direct_source)
    }

    /// A heart on a song already in the library is a favorite, not a download. Asked first, so
    /// it never waits behind other downloads, and never sends Lidarr for a whole album. The row closes on the library's own song, which favorites it for whoever
    /// hearted it (StarOnArrival); an owned lossy copy is also queued for Better quality.
    ///
    /// `scope` is whose library counts: the shared one for the owner, and the shared one plus
    /// their own for a member. Another member's own songs never count.
    async fn already_yours(
        &self,
        provider: &str,
        external_id: &str,
        requested_by: Option<&str>,
        scope: &LibraryScope,
    ) -> bool {
        let Some(owned) = &self.extras.owned else {
            return false;
        };
        let Some((song, copy)) = owned.find_song_in(scope, provider, external_id).await else {
            return false;
        };
        let upgrading = owned.queue_upgrade_if_wanted(&copy, &song, requested_by);
        // Before the row closes, so the close never reads as a download that landed.
        if let Some(stars) = &self.extras.stars {
            stars.favorite_owned(
                provider,
                external_id,
                copy.navidrome_id.as_deref(),
                &song.artist,
                &song.title,
                &copy.absolute_path,
            );
        }
        info!(
            "Hearted '{} - {}' is already in the library ({}); favoriting it instead of downloading{}",
            song.artist,
            song.title,
            copy.suffix,
            if upgrading {
                ", and looking for a higher quality copy"
            } else {
                ""
            }
        );
        self.settle(provider, external_id, &copy);
        true
    }

    /// An album heart where every song is already in the library: each is favorited
    /// through its row, and so the album too. One missing song and the album goes down the chain,
    /// where the songs already there are skipped as before.
    async fn album_already_yours(
        &self,
        provider: &str,
        album_external_id: &str,
        requested_by: Option<&str>,
    ) -> bool {
        let Some(owned) = &self.extras.owned else {
            return false;
        };
        let Some((album, songs)) = owned.find_whole_album(provider, album_external_id).await else {
            return false;
        };
        let tracked: Vec<&(octo_core::models::domain::Song, OwnedCopy)> = songs
            .iter()
            .filter(|(song, _)| song.external_id.as_deref().is_some_and(|id| !id.is_empty()))
            .collect();
        if let Some(any_song) = songs.iter().find_map(|(_, copy)| copy.navidrome_id.as_deref())
            && let Some(stars) = &self.extras.stars
        {
            stars.favorite_owned_album(provider, album_external_id, any_song);
        }
        if let Some(tracker) = self.tracker() {
            let listed: Vec<(String, Option<String>, Option<String>, Option<String>)> = tracked
                .iter()
                .map(|(song, _)| {
                    (
                        song.external_id.clone().unwrap_or_default(),
                        Some(song.artist.clone()),
                        Some(song.title.clone()),
                        Some(album.title.clone()),
                    )
                })
                .collect();
            tracker.announce(provider, Some(album_external_id), None, &listed);
        }
        let mut upgrading = 0;
        for (song, copy) in &tracked {
            if owned.queue_upgrade_if_wanted(copy, song, requested_by) {
                upgrading += 1;
            }
            self.settle(
                song.external_provider.as_deref().unwrap_or(provider),
                song.external_id.as_deref().unwrap_or_default(),
                copy,
            );
        }
        info!(
            "Hearted album '{} - {}' is already in the library ({} songs); favoriting it instead of downloading{}",
            album.artist,
            album.title,
            songs.len(),
            if upgrading > 0 {
                format!(", {upgrading} queued for a higher quality copy")
            } else {
                String::new()
            }
        );
        true
    }

    /// Close a row on the library's copy: by its Navidrome id when known, which is
    /// immediate, or by its path, which the tracker watches until Navidrome shows it.
    fn settle(&self, provider: &str, external_id: &str, copy: &OwnedCopy) {
        let Some(tracker) = self.tracker() else { return };
        match &copy.navidrome_id {
            Some(library_id) => tracker.complete(provider, external_id, Some(library_id)),
            None => tracker.imported(provider, external_id, None, None, Some(&copy.absolute_path)),
        }
    }
}

fn owned3(a: &str, b: &str, c: Option<&str>) -> (String, String, Option<String>) {
    (a.to_string(), b.to_string(), c.map(str::to_string))
}

fn source_name(source: HeartDownloadSource) -> &'static str {
    match source {
        HeartDownloadSource::Lidarr => "Lidarr",
        HeartDownloadSource::YouTube => "YouTube",
    }
}

/// The enum member's name, as `{Source}` interpolated it.
fn source_enum_name(source: HeartDownloadSource) -> &'static str {
    source_name(source)
}

fn to_direct_source(source: HeartDownloadSource) -> DownloadSource {
    match source {
        HeartDownloadSource::YouTube => DownloadSource::YouTube,
        HeartDownloadSource::Lidarr => DownloadSource::Lidarr,
    }
}

#[cfg(test)]
#[path = "heart_acquisition_coordinator_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "heart_acquisition_family_tests.rs"]
mod family_tests;
