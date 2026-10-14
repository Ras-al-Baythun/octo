//! Where a library song came from, and the outside routing that stands for it: the id a
//! download of it (or of the video someone picks for it) is asked for by. Shared by the
//! playlists, which keep a library song's outside id beside it, and octoPreview, which lets
//! a library song's versions be chosen like an outside song's.

use octo_core::common::dotnet::eq_ignore_case;
use octo_core::outside::{OutsideRouting, RoutingKind};

use super::navidrome_song_path_resolver::get_full_path;
use crate::app::AppState;
use crate::services::outside::{ExternalIdRegistry, OutsideMetadataService};

/// The provider and external id a library song was downloaded from: from Octo's record of
/// downloaded songs (by the song's id, else by the file's path), else from the download
/// history by the file's path. None for a song Octo did not download.
pub async fn origin_of(state: &AppState, id: &str, path: Option<&str>) -> Option<(String, String)> {
    let same_file =
        |known: &str| path.is_some_and(|path| known == path || get_full_path(known) == get_full_path(path));
    let mapped = state
        .local_library
        .get_mappings()
        .await
        .into_iter()
        .find(|mapping| mapping.local_subsonic_id.as_deref() == Some(id) || same_file(&mapping.local_path));
    if let Some(mapping) = mapped
        && !mapping.external_provider.is_empty()
        && !mapping.external_id.is_empty()
    {
        return Some((mapping.external_provider, mapping.external_id));
    }
    state
        .download_history
        .get_recent(i32::MAX)
        .into_iter()
        .find(|entry| same_file(&entry.path))
        .and_then(|entry| entry.key)
        .and_then(|key| {
            let (provider, external_id) = key.split_once(':')?;
            (!provider.is_empty() && !external_id.is_empty())
                .then(|| (provider.to_string(), external_id.to_string()))
        })
}

/// The outside id a library song was downloaded from, in the form it plays by: Octo's own
/// short id when the registry knows it, any other as `ext-<provider>-song-<id>`.
pub async fn outside_id_of(state: &AppState, id: &str, path: Option<&str>) -> Option<String> {
    origin_of(state, id, path)
        .await
        .map(|(provider, external_id)| outside_song_id(state, &provider, &external_id))
}

/// The song id an outside song plays by: one Octo knows by its short id, any other as
/// `ext-<provider>-song-<id>`.
pub fn outside_song_id(state: &AppState, provider: &str, external_id: &str) -> String {
    if eq_ignore_case(provider, OutsideMetadataService::PROVIDER_NAME)
        && state.external_id_registry.lookup(external_id).is_some()
    {
        return external_id.to_string();
    }
    format!("ext-{provider}-song-{external_id}")
}

/// The registry id that stands for the library song `navidrome_id`: the routing it was
/// downloaded from when the registry still has it, else a fresh one made from the file's own
/// facts (artist, title, album and length, as Navidrome reports them). None when the song's
/// file cannot be worked out, which is when nothing could replace it either.
pub async fn routing_for_library_song(state: &AppState, navidrome_id: &str) -> Option<String> {
    let file = state.navidrome_song_path_resolver.resolve(navidrome_id).await?;
    if let Some((provider, external_id)) = origin_of(state, navidrome_id, Some(&file.absolute_path)).await
        && eq_ignore_case(&provider, OutsideMetadataService::PROVIDER_NAME)
        && state.external_id_registry.lookup(&external_id).is_some()
    {
        return Some(external_id);
    }
    let routing = OutsideRouting {
        kind: RoutingKind::Song,
        artist: Some(file.artist),
        title: Some(file.title),
        album: Some(file.album).filter(|album| !album.trim().is_empty()),
        duration: file.duration_seconds,
        ..Default::default()
    };
    // Registered once: the registry's own copy carries the pick and the length a setPreview
    // gave it, which registering the file's facts again would write over.
    let id = ExternalIdRegistry::make_short_id(&routing);
    if state.external_id_registry.lookup(&id).is_some() {
        return Some(id);
    }
    Some(state.external_id_registry.register(routing))
}
