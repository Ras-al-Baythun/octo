use std::sync::Arc;

use octo_core::models::domain::{Album, Song};
use octo_core::settings::SettingsStore;

use super::upgrade_queue::{UpgradeAsk, UpgradeQueue};
use super::upgrade_sources::UpgradeSources;
use super::{LibraryOwnership, OwnedCopy};
use crate::services::family::LibraryScope;
use crate::services::i_music_metadata_service::IMusicMetadataService;

/// Whether a hearted outside song, or every song of a hearted outside album, is already in the
/// library. Asked before a heart goes anywhere: a song you have is favorited, never downloaded
/// again, and never waits behind other downloads or a whole Lidarr album.
pub struct HeartOwnership {
    ownership: Arc<LibraryOwnership>,
    metadata: Arc<dyn IMusicMetadataService>,
    /// The Subsonic and library action settings, read at every use.
    settings: Arc<SettingsStore>,
    /// Optional: without them nothing is queued for Better quality.
    upgrades: Option<Arc<UpgradeQueue>>,
    sources: Option<Arc<UpgradeSources>>,
}

impl HeartOwnership {
    pub fn new(
        ownership: Arc<LibraryOwnership>,
        metadata: Arc<dyn IMusicMetadataService>,
        settings: Arc<SettingsStore>,
        upgrades: Option<Arc<UpgradeQueue>>,
        sources: Option<Arc<UpgradeSources>>,
    ) -> Self {
        HeartOwnership {
            ownership,
            metadata,
            settings,
            upgrades,
            sources,
        }
    }

    /// The library's copy of the hearted song, or `None` when it is not there, the check is
    /// off, or the song cannot be told (then the heart downloads, as before).
    pub async fn find_song(&self, provider: &str, external_id: &str) -> Option<(Song, OwnedCopy)> {
        self.find_song_in(&LibraryScope::Shared, provider, external_id)
            .await
    }

    /// [`Self::find_song`] in one scope: the shared library (the owner's hearts), or the shared
    /// library and one member's own (that member's hearts). Another member's own songs never
    /// count, so a heart is never skipped for a song only they have.
    pub async fn find_song_in(
        &self,
        scope: &LibraryScope,
        provider: &str,
        external_id: &str,
    ) -> Option<(Song, OwnedCopy)> {
        if !self.settings.current().subsonic.skip_owned_songs {
            return None;
        }
        let song = self.metadata.get_song(provider, external_id).await?;
        let owned = self.ownership.find_song_in(scope, &song).await?;
        Some((song, owned))
    }

    /// The catalog this asks for songs and albums.
    pub fn metadata(&self) -> &Arc<dyn IMusicMetadataService> {
        &self.metadata
    }

    /// The hearted album and the library's copy of each of its songs, but only when every
    /// song is there; `None` otherwise, and the album heart goes on to fetch what is missing.
    pub async fn find_whole_album(
        &self,
        provider: &str,
        album_id: &str,
    ) -> Option<(Album, Vec<(Song, OwnedCopy)>)> {
        if !self.settings.current().subsonic.skip_owned_songs {
            return None;
        }
        let album = self.metadata.get_album(provider, album_id).await?;
        if album.songs.is_empty() {
            return None;
        }
        let mut found = Vec::with_capacity(album.songs.len());
        for song in &album.songs {
            // `song.Album ?? album.Title`: the song's own album, which is never null here.
            let owned = self.ownership.find_song_in(&LibraryScope::Shared, song).await?;
            found.push((song.clone(), owned));
        }
        Some((album, found))
    }

    /// Queue an owned lossy copy for Better quality, when a lossless source is set up and every
    /// gate of the action is open for the person who hearted it. True when it was queued.
    pub fn queue_upgrade_if_wanted(
        &self,
        owned: &OwnedCopy,
        song: &Song,
        requested_by: Option<&str>,
    ) -> bool {
        let (Some(navidrome_id), Some(upgrades)) = (&owned.navidrome_id, &self.upgrades) else {
            return false;
        };
        if owned.lossless {
            return false;
        }
        if self.sources.as_ref().is_some_and(|sources| !sources.ready()) {
            return false;
        }
        if !LibraryOwnership::upgrade_allowed(&self.settings.current().library_actions, requested_by) {
            return false;
        }
        upgrades.add(
            vec![UpgradeAsk {
                navidrome_id: navidrome_id.clone(),
                title: Some(song.title.clone()),
                artist: Some(song.artist.clone()),
                album: Some(song.album.clone()),
                suffix: Some(owned.suffix.clone()),
                attempt_key: None,
                pick: None,
            }],
            requested_by.unwrap_or_default(),
            "heart",
        );
        true
    }
}

#[cfg(test)]
#[path = "heart_ownership_tests.rs"]
mod tests;
