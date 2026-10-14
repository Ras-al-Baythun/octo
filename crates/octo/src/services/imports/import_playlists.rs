//! "Keep as playlist": an imported list kept as a playlist Octo keeps itself, whole from the
//! start (each missing song plays from outside the library until its file arrives), or as a
//! Navidrome playlist of the songs the library has.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::soulseek::soulseek_metadata_service::{RoutingKind, SoulseekRouting};
use tracing::{info, warn};

use super::ImportList;
use super::import_models::import_sources;
use crate::services::family::{FamilyPlaylistEntry, FamilyPlaylistStore};
use crate::services::library::NavidromePlaylistApi;
use crate::services::soulseek::ExternalIdRegistry;
use crate::services::subsonic::NavidromeIdentityService;

/// Where a list goes when Octo keeps its playlist itself: the store of the playlists Octo
/// keeps, and the registry that mints each song's outside id, by which a missing song plays
/// from outside the library.
pub struct KeptPlaylists {
    pub store: Arc<FamilyPlaylistStore>,
    pub ids: Arc<ExternalIdRegistry>,
}

/// How keeping a list's playlist went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSyncResult {
    pub ok: bool,
    pub playlist_id: Option<String>,
    pub note: Option<String>,
    pub deleted: bool,
    pub songs: i32,
}

impl PlaylistSyncResult {
    fn failed(playlist_id: Option<String>, note: Option<String>, songs: i32) -> Self {
        PlaylistSyncResult {
            ok: false,
            playlist_id,
            note,
            deleted: false,
            songs,
        }
    }
}

/// What the playlist keeper asks Navidrome. A trait, so a test can build one with no Navidrome
/// at all.
#[async_trait]
pub trait PlaylistNavidrome: Send + Sync {
    /// A new private playlist, as the admin: its id, or None.
    async fn create(&self, name: &str, comment: Option<&str>) -> Option<String>;
    /// True when the playlist is the user's afterwards.
    async fn give_to(&self, id: &str, user: &str) -> bool;
    /// Whether the playlist is still there: None when Navidrome could not say.
    async fn exists(&self, id: &str) -> Option<bool>;
    /// Its songs in order, as (position, song id), or None when they could not be read.
    async fn read_tracks(&self, id: &str) -> Option<Vec<(String, String)>>;
    async fn remove(&self, id: &str, positions: &[String]) -> bool;
    async fn add(&self, id: &str, songs: &[String]) -> bool;
    /// Deletes a playlist Octo just made and could not fill.
    async fn delete(&self, id: &str) -> bool;
    /// The admin Octo signs in to Navidrome as.
    fn admin_name(&self) -> Option<String>;
}

/// The real one: Navidrome's native API as the admin.
pub struct NavidromePlaylists {
    pub api: Arc<NavidromePlaylistApi>,
    pub identity: NavidromeIdentityService,
}

#[async_trait]
impl PlaylistNavidrome for NavidromePlaylists {
    async fn create(&self, name: &str, comment: Option<&str>) -> Option<String> {
        self.api.create_playlist(name, comment).await.unwrap_or_else(|e| {
            warn!("Could not create playlist '{name}': {e}");
            None
        })
    }

    async fn give_to(&self, id: &str, user: &str) -> bool {
        self.api.give_to(id, user).await
    }

    async fn exists(&self, id: &str) -> Option<bool> {
        self.api.playlist_exists(id).await.unwrap_or(None)
    }

    async fn read_tracks(&self, id: &str) -> Option<Vec<(String, String)>> {
        self.api.read_all_tracks(id).await.ok().flatten().map(|rows| {
            rows.into_iter()
                .map(|row| (row.position, row.media_file_id))
                .collect()
        })
    }

    async fn remove(&self, id: &str, positions: &[String]) -> bool {
        self.api.remove_positions(id, positions).await.unwrap_or(false)
    }

    async fn add(&self, id: &str, songs: &[String]) -> bool {
        self.api.add_tracks(id, songs).await.unwrap_or(false)
    }

    async fn delete(&self, id: &str) -> bool {
        self.api.delete_playlist(id).await.unwrap_or(false)
    }

    fn admin_name(&self) -> Option<String> {
        self.identity.get_scan_auth().map(|(user, _, _)| user)
    }
}

/// Keeps an imported list as a Navidrome playlist of the songs the library has, in the source's
/// order, so it is there in every Subsonic app. Made through Navidrome's own API as the admin and
/// then handed to the person, as Navidrome's playlist editor lets an admin do, so a song that
/// arrives later can be put in its place without anyone's password.
///
/// Only what differs is written: the songs up to the first difference stay, everything after it
/// is replaced. Songs nobody has yet are left out until they arrive. A playlist the person
/// deleted stays deleted.
///
/// A playlist this call made and could not fill is deleted again, so a failure never leaves an
/// empty one behind, and the next try never makes a second one. When even the delete fails, its
/// id is handed back so the next try fills that one.
pub struct ImportPlaylists {
    navidrome: Arc<dyn PlaylistNavidrome>,
    kept: Option<KeptPlaylists>,
}

impl ImportPlaylists {
    const REMOVE_CHUNK: usize = 100;
    const ADD_CHUNK: usize = 500;

    pub fn new(navidrome: Arc<dyn PlaylistNavidrome>) -> Self {
        ImportPlaylists {
            navidrome,
            kept: None,
        }
    }

    /// Lets lists be kept as playlists Octo keeps itself.
    pub fn with_kept(mut self, kept: KeptPlaylists) -> Self {
        self.kept = Some(kept);
        self
    }

    /// Keeps the list's playlist in Navidrome.
    pub async fn sync(&self, list: &ImportList) -> PlaylistSyncResult {
        self.sync_as(list, true).await
    }

    /// Keeps the list's playlist the way it is kept: one Octo keeps itself (an `of` id, or a
    /// new one unless `in_navidrome`), else Navidrome's.
    pub async fn sync_as(&self, list: &ImportList, in_navidrome: bool) -> PlaylistSyncResult {
        let kept_id = list.playlist_id.as_deref().is_some_and(|id| id.starts_with("of"));
        if let Some(kept) = &self.kept
            && (kept_id || (list.playlist_id.is_none() && !in_navidrome))
        {
            return Self::sync_kept(kept, list);
        }
        self.sync_navidrome(list).await
    }

    /// A playlist Octo keeps: made for the list's owner, then made the list's songs in order
    /// whenever they differ. Every song has an outside id, so a missing one plays from outside
    /// the library; one the library has carries its library song too, which is served first.
    fn sync_kept(kept: &KeptPlaylists, list: &ImportList) -> PlaylistSyncResult {
        let now = Utc::now();
        let playlist = match list.playlist_id.as_deref() {
            Some(id) => match kept.store.get(id) {
                Some(playlist) => playlist,
                None => {
                    info!(
                        "Playlist for imported list '{}' ({}) was deleted; not making it again",
                        list.name, list.owner
                    );
                    return PlaylistSyncResult {
                        ok: true,
                        playlist_id: None,
                        note: None,
                        deleted: true,
                        songs: 0,
                    };
                }
            },
            None => {
                let made = kept.store.create(&list.name, &list.owner, now);
                info!(
                    "Made playlist '{}' ({}) for {}, kept by Octo",
                    list.name, made.id, list.owner
                );
                made
            }
        };
        let mut wanted: Vec<FamilyPlaylistEntry> = Vec::new();
        for track in &list.tracks {
            let outside = kept.ids.register(SoulseekRouting {
                kind: RoutingKind::Song,
                artist: Some(track.artist.clone()),
                title: Some(track.title.clone()),
                album: track.album.clone(),
                duration: track.seconds,
                isrc: track.isrc.clone(),
                ..Default::default()
            });
            if wanted.iter().any(|entry| {
                entry.library_id == track.library_id && entry.outside_id.as_deref() == Some(&outside)
            }) {
                continue;
            }
            wanted.push(FamilyPlaylistEntry {
                library_id: track.library_id.clone(),
                scope: String::new(),
                outside_id: Some(outside),
                added_by: list.owner.clone(),
                added: now,
                pending: false,
                duration: track.seconds.map(i64::from),
                explicit_status: None,
            });
        }
        let same =
            playlist.entries.len() == wanted.len()
                && playlist.entries.iter().zip(&wanted).all(|(have, want)| {
                    have.library_id == want.library_id && have.outside_id == want.outside_id
                });
        if !same {
            let all: Vec<usize> = (0..playlist.entries.len()).collect();
            if !all.is_empty() {
                kept.store.remove_indexes(&playlist.id, &all);
            }
            kept.store.add_entries(&playlist.id, wanted.clone());
        }
        PlaylistSyncResult {
            ok: true,
            playlist_id: Some(playlist.id),
            note: None,
            deleted: false,
            songs: wanted.len() as i32,
        }
    }

    async fn sync_navidrome(&self, list: &ImportList) -> PlaylistSyncResult {
        let mut playlist_id = list.playlist_id.clone();
        let mut note = list.playlist_note.clone();
        let mut made = false;
        if let Some(id) = &playlist_id {
            match self.navidrome.exists(id).await {
                None => return PlaylistSyncResult::failed(playlist_id, note, 0),
                Some(false) => {
                    info!(
                        "Playlist for imported list '{}' ({}) was deleted in Navidrome; not making it again",
                        list.name, list.owner
                    );
                    return PlaylistSyncResult {
                        ok: true,
                        playlist_id: None,
                        note: None,
                        deleted: true,
                        songs: 0,
                    };
                }
                Some(true) => {}
            }
        } else {
            let Some(id) = self.navidrome.create(&list.name, Some(comment(list))).await else {
                return PlaylistSyncResult::failed(None, note, 0);
            };
            made = true;
            let admin = self.navidrome.admin_name();
            if !admin.as_deref().is_some_and(|a| eq_ignore_case(a, &list.owner))
                && !self.navidrome.give_to(&id, &list.owner).await
            {
                note = Some(format!(
                    "Navidrome lists it under {}: it could not be given to {}.",
                    admin.as_deref().unwrap_or("Octo's admin account"),
                    list.owner
                ));
                warn!(
                    "Playlist {id} for {} stays the admin's: Navidrome would not change its owner",
                    list.owner
                );
            } else {
                note = None;
            }
            info!("Made playlist '{}' ({id}) for {}", list.name, list.owner);
            playlist_id = Some(id);
        }
        let playlist_id = playlist_id.unwrap_or_default();

        let mut wanted: Vec<String> = Vec::new();
        for id in list.tracks.iter().filter_map(|t| t.library_id.as_ref()) {
            if !wanted.contains(id) {
                wanted.push(id.clone());
            }
        }
        let Some(current) = self.navidrome.read_tracks(&playlist_id).await else {
            return self.give_up(playlist_id, note, made, 0).await;
        };

        let mut same = 0;
        while same < current.len() && same < wanted.len() && current[same].1 == wanted[same] {
            same += 1;
        }
        if same == current.len() && same == wanted.len() {
            return PlaylistSyncResult {
                ok: true,
                playlist_id: Some(playlist_id),
                note,
                deleted: false,
                songs: wanted.len() as i32,
            };
        }

        // From the end backwards: removing the last positions leaves the earlier ones where they are.
        let stale: Vec<String> = current[same..]
            .iter()
            .map(|(position, _)| position.clone())
            .collect();
        let mut end = stale.len();
        while end > 0 {
            let start = end.saturating_sub(Self::REMOVE_CHUNK);
            if !self.navidrome.remove(&playlist_id, &stale[start..end]).await {
                return self.give_up(playlist_id, note, made, same as i32).await;
            }
            end = start;
        }
        for chunk in wanted[same..].chunks(Self::ADD_CHUNK) {
            if !self.navidrome.add(&playlist_id, chunk).await {
                return self.give_up(playlist_id, note, made, same as i32).await;
            }
        }
        info!(
            "Playlist '{}' ({playlist_id}) now holds {} songs ({same} kept in place)",
            list.name,
            wanted.len()
        );
        PlaylistSyncResult {
            ok: true,
            playlist_id: Some(playlist_id),
            note,
            deleted: false,
            songs: wanted.len() as i32,
        }
    }

    /// A failure after the playlist was there. One this call made goes again, so nothing empty
    /// is left behind; when Navidrome will not delete it either, its id goes back so the next try
    /// fills it rather than making another.
    async fn give_up(
        &self,
        playlist_id: String,
        note: Option<String>,
        made: bool,
        songs: i32,
    ) -> PlaylistSyncResult {
        if made && self.navidrome.delete(&playlist_id).await {
            info!("Deleted playlist {playlist_id} again: it could not be filled");
            return PlaylistSyncResult::failed(None, None, songs);
        }
        PlaylistSyncResult::failed(Some(playlist_id), note, songs)
    }
}

fn comment(list: &ImportList) -> &'static str {
    match list.source.as_str() {
        import_sources::SPOTIFY_LIKED => {
            "Your liked songs on Spotify, from the ones in your library. Octo keeps it up to date."
        }
        import_sources::FILE => "Imported by Octo from a file.",
        _ => "Imported from Spotify by Octo, from the songs in your library. Octo keeps it up to date.",
    }
}

#[cfg(test)]
#[path = "import_playlists_tests.rs"]
mod tests;
