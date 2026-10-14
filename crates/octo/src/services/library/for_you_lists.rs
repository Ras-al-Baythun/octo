//! New Releases for one listener. The picking is `octo_core::library::for_you_lists`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Datelike, NaiveDate, TimeDelta, Utc};
use octo_core::common::SongIdentity;
use octo_core::json::dom::Node;
use octo_core::library::for_you_lists::{self, TopArtist};
use octo_core::library::generated_playlist_service::str_of;
use octo_core::metadata::deezer_metadata_service::{AlbumAnswer, AlbumHit, AlbumTrack};
use octo_core::models::domain::Song;
use octo_core::outside::routing::{OutsideRouting, RoutingKind};
use octo_core::outside::song_length::LengthSource;
use octo_core::settings::ExplicitFilter;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::services::metadata::DeezerMetadataService;
use crate::services::outside::ExternalIdRegistry;
use crate::services::outside::outside_metadata_service::OutsideMetadataService;

/// What an outside song of New Releases is made from, kept with the list so the song can be made
/// again, with the same id, whenever the list is served: Octo remembers outside songs only so
/// long (`ExternalIdRegistry` keeps the most recent 10,000), and the list outlives a busy day of
/// searches. Stored with PascalCase property names, nulls written.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CatalogSeed {
    pub title: String,
    pub artist: String,
    pub artist_id: Option<String>,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub cover_url: Option<String>,
    pub duration: Option<i32>,
    pub year: Option<i32>,
    pub track: Option<i32>,
    pub explicit_content_lyrics: Option<i32>,
}

/// One entry of New Releases: a library song as Navidrome sent it, or an outside song and its seed.
#[derive(Debug, Clone)]
pub struct Entry {
    pub library: Option<Node>,
    pub outside: Option<Song>,
    pub cover_url: Option<String>,
    pub seed: Option<CatalogSeed>,
}

/// What a build came to, and whether the catalog answered for every artist asked.
#[derive(Debug, Clone)]
pub struct BuildResult {
    pub entries: Vec<Entry>,
    pub whole: bool,
}

/// A catalog song made into an outside song: its artist, album and song registered so it
/// plays, opens its album and is added like any other.
///
/// Shared by New Releases and the charts (`services::common::top_songs_service::catalog_song`
/// adds the charts' own fields on top).
pub struct CatalogTrackSong<'a> {
    pub title: &'a str,
    pub artist: &'a str,
    pub artist_id: Option<&'a str>,
    pub album: Option<&'a str>,
    pub album_id: Option<&'a str>,
    pub cover_url: Option<&'a str>,
    pub duration: Option<i32>,
}

pub fn catalog_song(registry: &ExternalIdRegistry, track: &CatalogTrackSong) -> Song {
    let artist_id = registry.register(OutsideRouting {
        kind: RoutingKind::Artist,
        artist: Some(track.artist.to_string()),
        external_artist_id: track.artist_id.map(str::to_string),
        ..Default::default()
    });
    let album_id = track
        .album
        .filter(|album| !octo_core::common::dotnet::is_blank(album))
        .map(|album| {
            registry.register(OutsideRouting {
                kind: RoutingKind::Album,
                artist: Some(track.artist.to_string()),
                album: Some(album.to_string()),
                external_album_id: track.album_id.map(str::to_string),
                ..Default::default()
            })
        });
    // The album rides on the song's routing too, so a download tags it with this album rather
    // than one found again from the artist and title.
    let id = registry.register(OutsideRouting {
        kind: RoutingKind::Song,
        artist: Some(track.artist.to_string()),
        title: Some(track.title.to_string()),
        album: track.album.map(str::to_string),
        duration: track.duration,
        ..Default::default()
    });
    registry.remember_length(&id, track.duration, LengthSource::Deezer);
    Song {
        id: id.clone(),
        title: track.title.to_string(),
        artist: track.artist.to_string(),
        artist_id: Some(artist_id),
        album: track.album.unwrap_or("").to_string(),
        album_id,
        duration: track.duration,
        cover_art_url: track.cover_url.map(str::to_string),
        cover_art_url_large: track.cover_url.map(str::to_string),
        is_local: false,
        external_provider: Some(OutsideMetadataService::PROVIDER_NAME.to_string()),
        external_id: Some(id),
        ..Default::default()
    }
}

/// The outside song a seed makes, registered so it plays, opens its album and is added with "+".
pub fn mint(registry: &ExternalIdRegistry, seed: &CatalogSeed) -> Song {
    let mut song = catalog_song(
        registry,
        &CatalogTrackSong {
            title: &seed.title,
            artist: &seed.artist,
            artist_id: seed.artist_id.as_deref(),
            album: seed.album.as_deref(),
            album_id: seed.album_id.as_deref(),
            cover_url: seed.cover_url.as_deref(),
            duration: seed.duration,
        },
    );
    song.year = seed.year;
    song.track = seed.track;
    song.explicit_content_lyrics = seed.explicit_content_lyrics;
    song
}

/// New Releases for one listener: their most-played artists found in the catalog (Deezer,
/// keyless, on its background lane so a refresh never slows search), each artist's releases of
/// the last weeks, and a few songs from each. A song the listener already has is their own copy;
/// the rest are outside songs, registered exactly as the artist "Top songs" ones are, so they
/// play, open their album and are added with "+".
pub struct NewReleasesBuilder {
    deezer: Arc<DeezerMetadataService>,
    registry: Arc<ExternalIdRegistry>,
    // One listener's catalog walk at a time. The catalog's background lane takes two calls a
    // second and every call, its wait included, gives up after 8 seconds: listeners refreshing
    // together would time each other out, and a timed-out artist looks like one with no releases.
    one_at_a_time: tokio::sync::Mutex<()>,
    // A catalog artist per library artist name, or none, for a while: names do not move.
    artists: Mutex<HashMap<String, (Option<String>, DateTime<Utc>)>>,
}

const FOUND: TimeDelta = TimeDelta::days(7);
const MISSED: TimeDelta = TimeDelta::days(1);
const ARTIST_CANDIDATES: i32 = 5;
const RELEASES_AT_ONCE: usize = 4;
const ARTIST_ALBUM_PAGES: i32 = 3;

impl NewReleasesBuilder {
    pub fn new(deezer: Arc<DeezerMetadataService>, registry: Arc<ExternalIdRegistry>) -> Self {
        Self {
            deezer,
            registry,
            one_at_a_time: tokio::sync::Mutex::new(()),
            artists: Mutex::new(HashMap::new()),
        }
    }

    pub fn registry(&self) -> &Arc<ExternalIdRegistry> {
        &self.registry
    }

    pub async fn build(
        &self,
        artists: &[TopArtist],
        library: &[Node],
        today: NaiveDate,
        weeks: i32,
        cancellation_token: &CancellationToken,
        explicit_filter: ExplicitFilter,
    ) -> anyhow::Result<BuildResult> {
        let _turn = tokio::select! {
            turn = self.one_at_a_time.lock() => turn,
            () = cancellation_token.cancelled() => return Err(crate::services::last_fm::OperationCanceled.into()),
        };
        self.build_one(
            artists,
            library,
            today,
            weeks,
            explicit_filter,
            cancellation_token,
        )
        .await
    }

    async fn build_one(
        &self,
        artists: &[TopArtist],
        library: &[Node],
        today: NaiveDate,
        weeks: i32,
        explicit_filter: ExplicitFilter,
        cancellation_token: &CancellationToken,
    ) -> anyhow::Result<BuildResult> {
        let mut owned: HashMap<String, &Node> = HashMap::new();
        for song in library {
            owned
                .entry(SongIdentity::match_key(
                    str_of(song, "artist").unwrap_or(""),
                    str_of(song, "title").unwrap_or(""),
                ))
                .or_insert(song);
        }

        let mut whole = true;
        let mut releases: Vec<(AlbumHit, String, String)> = Vec::new();
        // The catalog answers a throttled or failed call with nothing, just as it answers a name
        // it does not know. Nothing at all, for every artist asked, is read as no answer, so a
        // bad moment never replaces yesterday's list with an empty one.
        let mut resolved = 0;
        let mut listed_any = false;
        for artist in artists {
            if cancellation_token.is_cancelled() {
                return Err(crate::services::last_fm::OperationCanceled.into());
            }
            let Some(id) = self.resolve(&artist.name).await else {
                continue;
            };
            resolved += 1;
            let albums = self
                .deezer
                .get_artist_albums_on(&id, &artist.name, true, ARTIST_ALBUM_PAGES)
                .await;
            if !albums.is_empty() {
                listed_any = true;
            }
            for release in for_you_lists::recent_releases(&albums, today, weeks) {
                if releases
                    .iter()
                    .all(|(known, _, _)| known.deezer_id != release.deezer_id)
                {
                    releases.push((release, artist.name.clone(), id.clone()));
                }
            }
        }
        if !artists.is_empty() && (resolved == 0 || !listed_any) {
            whole = false;
        }

        let gate = Arc::new(Semaphore::new(RELEASES_AT_ONCE));
        let lookups = releases.iter().map(|(release, _, _)| {
            let gate = Arc::clone(&gate);
            let deezer = Arc::clone(&self.deezer);
            let id = release.deezer_id.clone();
            async move {
                let _permit = gate.acquire_owned().await.ok();
                deezer.look_up_album_detail_on(&id, true).await
            }
        });
        let looked_up = futures::future::join_all(lookups).await;
        let mut picked: Vec<(&AlbumHit, Vec<AlbumTrack>, &str, &str)> = Vec::new();
        for ((release, artist_name, artist_id), lookup) in releases.iter().zip(looked_up) {
            if let Some(detail) = &lookup.detail {
                let record_type = detail.record_type.as_deref().or(release.record_type.as_deref());
                picked.push((
                    release,
                    for_you_lists::pick_tracks(record_type, &detail.tracks),
                    artist_name,
                    artist_id,
                ));
            } else if lookup.answer == AlbumAnswer::Unavailable {
                whole = false;
            }
        }
        // Newest first; releases of one date stay in the order they were found.
        picked.sort_by_key(|item| std::cmp::Reverse(for_you_lists::release_date(item.0)));

        let mut entries: Vec<Entry> = Vec::new();
        let mut listed: HashSet<String> = HashSet::new();
        for (release, tracks, artist_name, artist_id) in picked {
            for track in tracks {
                if entries.len() >= for_you_lists::TRACK_COUNT {
                    break;
                }
                let key = SongIdentity::match_key(&track.artist, &track.title);
                // The catalog may credit a shared song to the other artist (a release on Ella
                // Langley's page credited to Miranda Lambert), while the library files it under
                // the listener's artist who led here: the same title under that artist is theirs
                // too.
                let by_their_artist = SongIdentity::match_key(artist_name, &track.title);
                if listed.contains(&key) || listed.contains(&by_their_artist) {
                    continue;
                }
                let mine = owned.get(&key).or_else(|| owned.get(&by_their_artist)).copied();
                // The listener's own copy is theirs whatever the filter; an outside song obeys it.
                if mine.is_none() && !for_you_lists::explicit_allowed(track.explicit_content, explicit_filter)
                {
                    continue;
                }
                listed.insert(key);
                listed.insert(by_their_artist);
                if let Some(mine) = mine {
                    entries.push(Entry {
                        library: Some(mine.clone()),
                        outside: None,
                        cover_url: release.cover_url.clone(),
                        seed: None,
                    });
                    continue;
                }
                // The catalog artist found for the listener's artist is this song's when it is
                // credited to them, so its artist link opens the right page.
                let seed = CatalogSeed {
                    title: track.title.clone(),
                    artist: track.artist.clone(),
                    artist_id: (SongIdentity::key(&track.artist) == SongIdentity::key(artist_name))
                        .then(|| artist_id.to_string()),
                    album: Some(release.title.clone()),
                    album_id: Some(release.deezer_id.clone()),
                    cover_url: release.cover_url.clone(),
                    duration: track.duration,
                    year: for_you_lists::release_date(release).map(|date| date.year()),
                    track: track.track_position,
                    explicit_content_lyrics: track.explicit_content,
                };
                entries.push(Entry {
                    library: None,
                    outside: Some(mint(&self.registry, &seed)),
                    cover_url: release.cover_url.clone(),
                    seed: Some(seed),
                });
            }
        }
        info!(
            "New Releases: {} releases from {} artists, {} songs",
            releases.len(),
            artists.len(),
            entries.len()
        );
        Ok(BuildResult { entries, whole })
    }

    /// The catalog's artist for a library artist's name: the one whose name is the same name,
    /// most followed first, since two acts can share a name and the bigger one is the likelier.
    async fn resolve(&self, name: &str) -> Option<String> {
        let key = SongIdentity::key(name);
        if key.is_empty() {
            return None;
        }
        if let Some((id, until)) = self.artists.lock().get(&key)
            && *until > Utc::now()
        {
            return id.clone();
        }
        let hits = self.deezer.search_artists_on(name, ARTIST_CANDIDATES, true).await;
        // No candidates at all may be a throttled answer: not remembered, asked again next time.
        if hits.is_empty() {
            return None;
        }
        // Most followed first; of equals, the first listed (a stable order).
        let mut same: Vec<_> = hits
            .iter()
            .filter(|hit| SongIdentity::key(&hit.name) == key)
            .collect();
        same.sort_by_key(|hit| std::cmp::Reverse(hit.fans));
        let id = same.first().map(|hit| hit.deezer_id.clone());
        // Candidates but none of that very name: the catalog does not know them, for a day.
        let until = Utc::now() + if id.is_none() { MISSED } else { FOUND };
        self.artists.lock().insert(key, (id.clone(), until));
        id
    }
}

#[cfg(test)]
#[path = "for_you_lists_tests.rs"]
pub(crate) mod tests;
