//! How much of an outside album the library already holds, from its songs, and the library index
//! it is judged against.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use indexmap::IndexMap;
use octo_core::common::dotnet::is_null_or_white_space;
use octo_core::common::song_identity::{ArtistAgreement, SongIdentity, SongMatchOptions, SongRef};
use octo_core::models::domain::Album;
use regex::Regex;
use tracing::debug;

use octo_core::metadata::deezer_metadata_service::AlbumDetail;

use super::duplicate_scan_worker::is_lossless_file;
use super::library_ownership::LibraryOwnership;
use super::library_snapshot::LibrarySnapshot;
use super::quality_upgrade_worker::LibrarySongRow;
use crate::services::family::LibraryScope;
use crate::services::metadata::DeezerMetadataService;
use crate::services::outside::ExternalIdRegistry;

/// One library album, with its songs as the index read them.
#[derive(Debug, Clone, PartialEq)]
pub struct LibraryAlbum {
    pub id: String,
    pub name: String,
    pub album_artist: Option<String>,
    pub songs: Vec<Arc<LibrarySongRow>>,
}

/// The whole library as album ownership reads it: every song by its MatchKey, its title and its
/// ISRC, and every album by its name. Built once per read of the library, so a search weighs a
/// page of outside albums against it without asking Navidrome anything.
#[derive(Debug, Default)]
pub struct LibraryAlbumIndex {
    by_key: HashMap<String, Vec<Arc<LibrarySongRow>>>,
    by_title: HashMap<String, Vec<Arc<LibrarySongRow>>>,
    by_isrc: HashMap<String, Vec<Arc<LibrarySongRow>>>,
    albums: HashMap<String, Arc<LibraryAlbum>>,
    albums_by_name: HashMap<String, Vec<Arc<LibraryAlbum>>>,
    albums_by_base: HashMap<String, Vec<Arc<LibraryAlbum>>>,
    artists: HashSet<String>,
    song_count: usize,
    rows: Vec<Arc<LibrarySongRow>>,
}

/// A length within the ownership tolerance, and the lookup reading: a credit written another way
/// or a title in another script is the same song, a typo is not.
static LENIENT: LazyLock<SongMatchOptions> =
    LazyLock::new(|| SongIdentity::owned(LibraryOwnership::DURATION_TOLERANCE_SECONDS));

impl LibraryAlbumIndex {
    pub fn new(rows: impl IntoIterator<Item = LibrarySongRow>) -> Self {
        let mut index = LibraryAlbumIndex::default();
        // In the order each album was first seen.
        let mut by_album: IndexMap<String, Vec<Arc<LibrarySongRow>>> = IndexMap::new();
        for row in rows {
            let row = Arc::new(row);
            index.rows.push(Arc::clone(&row));
            if is_null_or_white_space(Some(&row.title)) {
                continue;
            }
            add(
                &mut index.by_key,
                SongIdentity::match_key(&row.artist, &row.title),
                &row,
            );
            add(
                &mut index.by_title,
                SongIdentity::parse_title(&row.title, Some(&row.artist)).key,
                &row,
            );
            for isrc in SongIdentity::isrcs(&row.isrcs) {
                add(&mut index.by_isrc, isrc, &row);
            }
            for key in artist_keys(Some(&row.artist))
                .into_iter()
                .chain(artist_keys(row.album_artist.as_deref()))
            {
                index.artists.insert(key);
            }
            if let Some(album_id) = row.album_id.as_deref().filter(|id| !id.is_empty()) {
                by_album
                    .entry(album_id.to_string())
                    .or_default()
                    .push(Arc::clone(&row));
            }
            index.song_count += 1;
        }
        for (id, songs) in by_album {
            let album = Arc::new(LibraryAlbum {
                name: songs[0].album.clone().unwrap_or_default(),
                album_artist: songs
                    .iter()
                    .filter_map(|song| song.album_artist.clone())
                    .find(|artist| !is_null_or_white_space(Some(artist))),
                songs,
                id: id.clone(),
            });
            index.albums.insert(id, Arc::clone(&album));
            add(&mut index.albums_by_name, SongIdentity::key(&album.name), &album);
            add(
                &mut index.albums_by_base,
                AlbumOwnership::base_key(Some(&album.name)),
                &album,
            );
        }
        index
    }

    /// How many songs the library has.
    pub fn song_count(&self) -> usize {
        self.song_count
    }

    /// Every row as it was read, for the checks that look at the whole library (Library health).
    pub fn rows(&self) -> &[Arc<LibrarySongRow>] {
        &self.rows
    }

    pub fn album(&self, id: &str) -> Option<&Arc<LibraryAlbum>> {
        self.albums.get(id)
    }

    /// The library's copy of a catalog album's song, or None. The same ISRC; the same MatchKey at
    /// a length within 8 seconds; the same title with the credit written another way; or, last,
    /// the same title on an album of the same name at that length, which is how a song a library
    /// credits to the guest ("Drake") is still found for the catalog's "PARTYNEXTDOOR". A version
    /// ("Live", "Remix", "Sped Up") is always its own song.
    pub fn find(
        &self,
        title: &str,
        artist: Option<&str>,
        seconds: Option<i32>,
        isrcs: &[Option<&str>],
        album_title: Option<&str>,
    ) -> Option<Arc<LibrarySongRow>> {
        for isrc in SongIdentity::isrcs(isrcs.iter().flatten()) {
            if let Some(coded) = self.by_isrc.get(&isrc)
                && let Some(by_isrc) = closest(coded.iter(), seconds)
            {
                return Some(by_isrc);
            }
        }

        if let Some(same) = self
            .by_key
            .get(&SongIdentity::match_key(artist.unwrap_or(""), title))
            && let Some(exact) = closest(
                same.iter().filter(|row| length_fits(seconds, row.duration)),
                seconds,
            )
        {
            return Some(exact);
        }

        let title_key = SongIdentity::parse_title(title, artist).key;
        if title_key.is_empty() {
            return None;
        }
        let titled = self.by_title.get(&title_key)?;
        let wanted = SongRef::new(title, artist.unwrap_or("")).with_seconds(seconds.map(f64::from));
        if let Some(credited) = closest(
            titled.iter().filter(|row| {
                let theirs = SongRef::new(row.title.as_str(), row.artist.as_str())
                    .with_seconds(row.duration.map(f64::from));
                SongIdentity::same(&wanted, &theirs, Some(&LENIENT)).is_same()
            }),
            seconds,
        ) {
            return Some(credited);
        }

        let album = AlbumOwnership::base_key(album_title);
        if album.is_empty() {
            return None;
        }
        let versioned = SongIdentity::title_key(title);
        closest(
            titled.iter().filter(|row| {
                SongIdentity::title_key(&row.title) == versioned
                    && AlbumOwnership::base_key(row.album.as_deref()) == album
                    && length_fits(seconds, row.duration)
            }),
            seconds,
        )
    }

    /// Library albums with this very name (case, accents and punctuation aside) that share an
    /// artist with `artist`.
    pub fn same_album(&self, name: Option<&str>, artist: Option<&str>) -> Vec<Arc<LibraryAlbum>> {
        let key = SongIdentity::key(name.unwrap_or(""));
        if key.is_empty() {
            return Vec::new();
        }
        self.albums_by_name
            .get(&key)
            .map(|named| {
                named
                    .iter()
                    .filter(|album| shares_artist(album, artist))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether the library may well hold songs of this catalog album, judged on names alone: an
    /// album of the same name once edition tags are set aside ("HABIBTI" and "HABIBTI (FOMO)"),
    /// or a song named as the album (a single), by an artist the two share. Only ever decides
    /// whether the album's songs are looked at now; never who owns what.
    pub fn likely(&self, album_title: Option<&str>, artist: Option<&str>) -> bool {
        let key = AlbumOwnership::base_key(album_title);
        if key.is_empty() {
            return false;
        }
        if self
            .albums_by_base
            .get(&key)
            .is_some_and(|albums| albums.iter().any(|album| shares_artist(album, artist)))
        {
            return true;
        }
        let song = SongIdentity::parse_title(album_title.unwrap_or(""), artist).key;
        let wanted = if song.is_empty() { key } else { song };
        self.by_title
            .get(&wanted)
            .is_some_and(|songs| songs.iter().any(|row| share_an_artist(Some(&row.artist), artist)))
    }

    /// Whether any library song or album credits one of this credit's artists.
    pub fn knows_artist(&self, artist: Option<&str>) -> bool {
        artist_keys(artist).iter().any(|key| self.artists.contains(key))
    }
}

fn shares_artist(album: &LibraryAlbum, artist: Option<&str>) -> bool {
    share_an_artist(album.album_artist.as_deref(), artist)
        || album
            .songs
            .iter()
            .any(|song| share_an_artist(Some(&song.artist), artist))
}

/// The two credits name one artist in common, whatever else each names.
pub(crate) fn share_an_artist(a: Option<&str>, b: Option<&str>) -> bool {
    matches!(
        SongIdentity::compare_artists(a.unwrap_or(""), b.unwrap_or("")),
        ArtistAgreement::Agree | ArtistAgreement::Loose | ArtistAgreement::Conflict
    )
}

fn artist_keys(credit: Option<&str>) -> Vec<String> {
    let Some(credit) = credit.filter(|credit| !is_null_or_white_space(Some(credit))) else {
        return Vec::new();
    };
    let parsed = SongIdentity::parse_artists(credit);
    parsed
        .names
        .iter()
        .chain(std::iter::once(&parsed.display))
        .map(|name| SongIdentity::key(name))
        .filter(|key| !key.is_empty())
        .collect()
}

fn length_fits(wanted: Option<i32>, found: Option<i32>) -> bool {
    match (wanted.filter(|w| *w > 0), found.filter(|f| *f > 0)) {
        (Some(wanted), Some(found)) => (wanted - found).abs() <= LibraryOwnership::DURATION_TOLERANCE_SECONDS,
        _ => true,
    }
}

/// The nearest length first, a lossless file before a lossy one; the first of equals.
fn closest<'a>(
    rows: impl Iterator<Item = &'a Arc<LibrarySongRow>>,
    seconds: Option<i32>,
) -> Option<Arc<LibrarySongRow>> {
    rows.min_by_key(|row| {
        let distance = match (seconds.filter(|s| *s > 0), row.duration.filter(|d| *d > 0)) {
            (Some(seconds), Some(duration)) => (seconds - duration).abs(),
            _ => 0,
        };
        (distance, !is_lossless_file(&row.suffix, row.bit_rate))
    })
    .cloned()
}

/// A catalog album's tracklist as album ownership reads it.
fn tracks(detail: Option<&AlbumDetail>) -> Option<Vec<CatalogTrack>> {
    detail.map(|detail| {
        detail
            .tracks
            .iter()
            .map(|t| CatalogTrack::new(t.title.as_str(), Some(&t.artist), t.duration, t.isrc.as_deref()))
            .collect()
    })
}

fn add<T>(map: &mut HashMap<String, Vec<Arc<T>>>, key: String, value: &Arc<T>) {
    if key.is_empty() {
        return;
    }
    map.entry(key).or_default().push(Arc::clone(value));
}

/// One catalog song, as the tracklist names it.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogTrack {
    pub title: String,
    pub artist: Option<String>,
    pub duration: Option<i32>,
    pub isrc: Option<String>,
}

impl CatalogTrack {
    pub fn new(
        title: impl Into<String>,
        artist: Option<&str>,
        duration: Option<i32>,
        isrc: Option<&str>,
    ) -> Self {
        Self {
            title: title.into(),
            artist: artist.map(str::to_string),
            duration,
            isrc: isrc.map(str::to_string),
        }
    }
}

/// What the library holds of one outside album. `owned` is None when its songs were not looked
/// at. `holders` are the library albums holding the owned songs, most first; `same_name` the
/// library albums with its very name and artist. `index` is where the album stood in the list
/// that was judged.
#[derive(Debug, Clone)]
pub struct Standing {
    pub index: usize,
    pub album: Album,
    pub owned: Option<i32>,
    pub total: i32,
    pub holders: Vec<Arc<LibraryAlbum>>,
    pub same_name: Vec<Arc<LibraryAlbum>>,
}

impl Standing {
    pub fn fully_owned(&self) -> bool {
        self.owned
            .is_some_and(|owned| self.total > 0 && owned >= self.total)
    }

    /// The library albums that stand for this one when it is left out: its namesake, or the
    /// albums that hold all of its songs.
    pub fn stand_ins(&self) -> &[Arc<LibraryAlbum>] {
        if !self.same_name.is_empty() {
            &self.same_name
        } else if self.fully_owned() {
            &self.holders
        } else {
            &[]
        }
    }
}

/// A tracklist read on its own task, which runs on when the request stops waiting.
type Running = Shared<BoxFuture<'static, Option<Vec<CatalogTrack>>>>;

/// `Library`: the library as it stands for a scope (the shared library, or a member's view with
/// their own songs), or None when it cannot be read.
pub type LibrarySource =
    Arc<dyn Fn(LibraryScope) -> BoxFuture<'static, Option<Arc<LibraryAlbumIndex>>> + Send + Sync>;
/// `Tracklist`: an outside album's tracklist, asked for now or in the background.
pub type TracklistSource =
    Arc<dyn Fn(Album, bool) -> BoxFuture<'static, Option<Vec<CatalogTrack>>> + Send + Sync>;
/// `Cached`: an outside album's tracklist when it is already known, without asking anyone.
pub type CachedTracklist = Arc<dyn Fn(&Album) -> Option<Vec<CatalogTrack>> + Send + Sync>;

static TRAILING_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[\(\[][^\(\)\[\]]*[\)\]]\s*$").expect("a valid pattern"));
static TRAILING_EDITION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\s+-\s+(?:single|ep|(?:(?:super\s+)?deluxe|expanded|explicit|clean|remaster(?:ed)?|special|anniversary)(?:\s+(?:edition|version))?)\s*$",
    )
    .expect("a valid pattern")
});

/// How much of an outside album the library already holds, from its songs. A name is not an
/// identity: "HABIBTI" is held whole inside the library's "HABIBTI (FOMO)", while a deluxe
/// edition is never held just because the standard one is. So the catalog album's tracklist is
/// read (cached, as album fill-in reads it) and each song looked for in the whole library,
/// whatever album, edition or album artist the library filed it under.
///
/// Search runs as people type, so not every album earns a tracklist then: an album whose name
/// the library has, or that is named as a library song (a single), is read at once; one by an
/// artist the library knows is read in the background, ready for the next search; the rest are
/// left alone. Whatever is not known in time goes out as before, without a count.
pub struct AlbumOwnership {
    // Seams: tests answer from lists instead of Navidrome and Deezer.
    pub(crate) library: LibrarySource,
    pub(crate) tracklist: TracklistSource,
    pub(crate) cached: CachedTracklist,
}

impl AlbumOwnership {
    /// Tracklists one request may ask Deezer for, at once and in the background. Deezer's budget
    /// is shared with everything else a search does (40 requests in 5 seconds, two per album),
    /// and an artist page can name fifty albums. Albums already cached cost nothing and are
    /// always counted, so each visit counts more of a long list than the one before.
    pub const MAX_READS: usize = 8;
    pub const MAX_BACKGROUND_READS: usize = 4;

    /// How long a request with only background reads running waits for them.
    pub const WARMING_GRACE: Duration = Duration::from_millis(100);

    pub fn new(library: LibrarySource, tracklist: TracklistSource, cached: CachedTracklist) -> Self {
        Self {
            library,
            tracklist,
            cached,
        }
    }

    /// The production sources: the library snapshot, and the catalog's tracklist of the album
    /// an outside album id was registered for, from its cache or one shared request.
    pub fn over(
        snapshot: Arc<LibrarySnapshot>,
        deezer: Arc<DeezerMetadataService>,
        ids: Arc<ExternalIdRegistry>,
    ) -> Self {
        let deezer_id = {
            let ids = Arc::clone(&ids);
            move |album: &Album| {
                ids.lookup(&album.id)
                    .and_then(|routing| routing.lock().external_album_id.clone())
                    .filter(|id| !id.is_empty())
            }
        };
        let tracklist_id = deezer_id.clone();
        let tracklist_deezer = Arc::clone(&deezer);
        Self::new(
            Arc::new(move |scope: LibraryScope| {
                let snapshot = Arc::clone(&snapshot);
                Box::pin(async move { snapshot.current_for(&scope).await })
            }),
            Arc::new(move |album: Album, background| {
                let deezer = Arc::clone(&tracklist_deezer);
                let id = tracklist_id(&album);
                Box::pin(async move {
                    let id = id?;
                    tracks(deezer.shared_album_detail(&id, background).await.detail.as_ref())
                })
            }),
            Arc::new(move |album: &Album| {
                let id = deezer_id(album)?;
                tracks(deezer.cached_album_detail(&id)?.detail.as_ref())
            }),
        )
    }

    /// For tests: no library, no tracklists.
    pub fn empty() -> Self {
        Self::new(
            Arc::new(|_| Box::pin(async { None })),
            Arc::new(|_, _| Box::pin(async { None })),
            Arc::new(|_| None),
        )
    }

    /// An album title without its edition: every trailing bracket and a trailing " - Single",
    /// " - EP" or " - Deluxe Edition" go, so "HABIBTI (FOMO)", "What A Time To Be Alive (Explicit
    /// Version)" and "Take Care (Deluxe)" read as their albums. Only ever used to pair names,
    /// never to decide that two editions hold the same songs.
    pub fn base_key(title: Option<&str>) -> String {
        let mut text = SongIdentity::fold(title.unwrap_or(""));
        loop {
            let without_bracket = TRAILING_BRACKET.replace_all(&text, "");
            let trimmed = TRAILING_EDITION.replace_all(&without_bracket, "").into_owned();
            if trimmed == text || SongIdentity::key(&trimmed).is_empty() {
                break;
            }
            text = trimmed;
        }
        SongIdentity::key(&text)
    }

    /// [`Self::judge_in`] the shared library.
    pub async fn judge(&self, albums: &[Album], wait: Duration) -> Vec<Standing> {
        self.judge_in(albums, wait, LibraryScope::Shared).await
    }

    /// What the library `scope` sees holds of each outside album, in the order given, waiting
    /// at most `wait` for the library and the tracklists. A member's scope counts their own
    /// songs too, so an album in their own library stands in for its outside version. Library
    /// albums pass through unjudged. Empty when the library cannot be read in time.
    pub async fn judge_in(&self, albums: &[Album], wait: Duration, scope: LibraryScope) -> Vec<Standing> {
        let outside: Vec<(usize, &Album)> = albums
            .iter()
            .enumerate()
            .filter(|(_, album)| !album.is_local)
            .collect();
        if outside.is_empty() {
            return Vec::new();
        }
        let clock = Instant::now();

        let index = match tokio::time::timeout(wait, (self.library)(scope)).await {
            Ok(Some(index)) => index,
            Ok(None) => return Vec::new(),
            Err(_) => {
                debug!(
                    "album ownership: the library was not read within {} ms; albums go out uncounted",
                    wait.as_millis()
                );
                return Vec::new();
            }
        };

        enum Read {
            Cached(Vec<CatalogTrack>),
            Running(Running),
            /// Read for the next request: counted only when it is back by the time the reads
            /// this request waits for are.
            Warming(Running),
        }
        let mut reads: HashMap<usize, Read> = HashMap::new();
        let (mut asked, mut warming) = (0, 0);
        for (at, album) in &outside {
            let (title, artist) = (Some(album.title.as_str()), Some(album.artist.as_str()));
            if !index.same_album(title, artist).is_empty() {
                continue;
            }
            let likely = index.likely(title, artist);
            if !likely && !index.knows_artist(artist) {
                continue;
            }
            if let Some(cached) = (self.cached)(album) {
                reads.insert(*at, Read::Cached(cached));
            } else if likely && asked < Self::MAX_READS {
                asked += 1;
                reads.insert(*at, Read::Running(self.read(album, false)));
            } else if warming < Self::MAX_BACKGROUND_READS {
                warming += 1;
                reads.insert(*at, Read::Warming(self.read(album, true)));
            }
        }

        // Every read in time, or as many as are back when the wait is up; the rest run on and
        // are cached for the next request. With only reads for the next request running (the
        // albums this request needs are cached), the wait is a short grace, so a repeated
        // search is not held up by tracklists it only warms.
        let running = |warming: bool| -> Vec<Running> {
            reads
                .values()
                .filter_map(|read| match read {
                    Read::Running(running) => Some(running.clone()),
                    Read::Warming(running) if warming => Some(running.clone()),
                    Read::Cached(_) | Read::Warming(_) => None,
                })
                .collect()
        };
        let asked_now = !running(false).is_empty();
        let left = if asked_now {
            wait.saturating_sub(clock.elapsed())
        } else {
            wait.saturating_sub(clock.elapsed()).min(Self::WARMING_GRACE)
        };
        let all = running(true);
        if !all.is_empty() && !left.is_zero() {
            let _ = tokio::time::timeout(left, futures::future::join_all(all)).await;
        }
        let asked_count = reads.len();
        let mut tracklists: HashMap<usize, Vec<CatalogTrack>> = HashMap::new();
        for (at, read) in reads {
            match read {
                Read::Cached(tracks) => {
                    tracklists.insert(at, tracks);
                }
                Read::Running(running) | Read::Warming(running) => {
                    if let Some(Some(tracks)) = running.peek() {
                        tracklists.insert(at, tracks.clone());
                    }
                }
            }
        }

        let standings: Vec<Standing> = outside
            .iter()
            .map(|(at, album)| {
                let same_name = index.same_album(Some(&album.title), Some(&album.artist));
                if !same_name.is_empty() {
                    return Standing {
                        index: *at,
                        album: (*album).clone(),
                        owned: None,
                        total: album.song_count.unwrap_or(0),
                        holders: Vec::new(),
                        same_name,
                    };
                }
                match tracklists.get(at).filter(|tracks| !tracks.is_empty()) {
                    Some(tracks) => Standing {
                        index: *at,
                        ..Self::judge_album(album, tracks, &index)
                    },
                    None => Standing {
                        index: *at,
                        album: (*album).clone(),
                        owned: None,
                        total: album.song_count.unwrap_or(0),
                        holders: Vec::new(),
                        same_name: Vec::new(),
                    },
                }
            })
            .collect();

        debug!(
            "album ownership: {} outside album(s), {} tracklist(s) asked, {} counted in {} ms",
            outside.len(),
            asked_count,
            standings.iter().filter(|s| s.owned.is_some()).count(),
            clock.elapsed().as_millis()
        );
        standings
    }

    /// What the library holds of a catalog album, song by song (`Judge`).
    pub fn judge_album(album: &Album, tracks: &[CatalogTrack], index: &LibraryAlbumIndex) -> Standing {
        let mut owned = 0;
        let mut holding: IndexMap<String, i32> = IndexMap::new();
        for track in tracks {
            let Some(row) = Self::find(index, album, track) else {
                continue;
            };
            owned += 1;
            if let Some(album_id) = row.album_id.as_deref().filter(|id| !id.is_empty()) {
                *holding.entry(album_id.to_string()).or_default() += 1;
            }
        }
        let mut ranked: Vec<(String, i32)> = holding.into_iter().collect();
        // Stable, so albums holding as many keep the order they were first found in.
        ranked.sort_by_key(|holding| std::cmp::Reverse(holding.1));
        let holders = ranked
            .into_iter()
            .filter_map(|(id, _)| index.album(&id).cloned())
            .collect();
        Standing {
            index: 0,
            album: album.clone(),
            owned: Some(owned),
            total: tracks.len() as i32,
            holders,
            same_name: Vec::new(),
        }
    }

    /// The library's copy of one catalog song of `album`, or None.
    pub fn find(
        index: &LibraryAlbumIndex,
        album: &Album,
        track: &CatalogTrack,
    ) -> Option<Arc<LibrarySongRow>> {
        let artist = match track.artist.as_deref() {
            Some(artist) if !is_null_or_white_space(Some(artist)) => artist,
            _ => album.artist.as_str(),
        };
        index.find(
            &track.title,
            Some(artist),
            track.duration,
            &[track.isrc.as_deref()],
            Some(&album.title),
        )
    }

    /// The shared library as it stands, waiting at most `wait`; None when it cannot be read in
    /// that time.
    pub async fn library(&self, wait: Duration) -> Option<Arc<LibraryAlbumIndex>> {
        self.library_in(wait, LibraryScope::Shared).await
    }

    /// The library as `scope` sees it, waiting at most `wait`; None when it cannot be read in
    /// that time.
    pub async fn library_in(&self, wait: Duration, scope: LibraryScope) -> Option<Arc<LibraryAlbumIndex>> {
        tokio::time::timeout(wait, (self.library)(scope))
            .await
            .ok()
            .flatten()
    }

    /// One tracklist read, on a task of its own so that it runs on (and is cached) when the
    /// request stops waiting for it.
    fn read(&self, album: &Album, background: bool) -> Running {
        let tracklist = Arc::clone(&self.tracklist);
        let album = album.clone();
        let task = tokio::spawn(async move {
            let (artist, title) = (album.artist.clone(), album.title.clone());
            let tracks = tracklist(album, background).await;
            if tracks.is_none() {
                debug!("album ownership: no tracklist for '{artist} - {title}'");
            }
            tracks
        });
        task.map(Result::unwrap_or_default).boxed().shared()
    }
}

#[cfg(test)]
#[path = "album_ownership_tests.rs"]
pub(crate) mod tests;
