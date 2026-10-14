//! Top songs and the charts: an artist's most played songs, for search, and every chart list
//! (Popular right now, Best New Songs, Trending Songs, the genre charts), for the chart
//! playlists, `getTopChart` and the apps' Charts page. Each comes back ranked, as outside songs
//! that play and can be added like any search row; which of them a caller's library already has
//! is settled per request by [`TopSongsService::match_library`], because the library changes and
//! the list does not.
//!
//! Sources, and why in this order:
//!   - The charts are Apple Music's (`AppleMusicPages`): current, per country, per genre, and
//!     each row carries its album, length, explicit badge and artwork, so a row costs no
//!     further lookup. Best New Songs and Trending Songs are the New page's song rooms.
//!   - When Apple's page cannot be read, Popular right now is Deezer's chart, then Last.fm's.
//!     The genre charts and the rooms have no stand-in: they keep their last list.
//!   - An artist's ranking is Last.fm's play counts when a key is set, each row dressed by
//!     Deezer's own top list; without a key, Deezer's list is the ranking.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use octo_core::charts::apple_pages::{ChartGenre, RoomKind};
use octo_core::charts::chart_settings::{
    self, ChartCountry, ChartKind, OVERALL, english_menu, genre_name, kind, max_songs, playlist_name,
};
use octo_core::charts::top_songs::{
    APPLE_SOURCE, CatalogTrack, CountedTrack, DEEZER_SOURCE, MAX_ROWS, PlannedRow, library_hit,
    library_query, merge, one_row_per_library_song,
};
use octo_core::common::SongIdentity;
use octo_core::common::dotnet::is_blank;
use octo_core::models::domain::Song;
use octo_core::outside::routing::RoutingKind;
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use crate::services::charts::apple_music_pages::{AppleMusicPages, PageError};
use crate::services::cover_art::ArtworkHints;
use crate::services::framework::{InFlight, MemoryCache};
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::last_fm_service::LastFmService;
use crate::services::library::for_you_lists;
use crate::services::metadata::deezer_metadata_service::{CHART_LIMIT, DeezerMetadataService};
use crate::services::outside::external_id_registry::ExternalIdRegistry;

/// A whole list keeps for this long once every source answered; one built while a source did
/// not answer, or that came out empty, is asked again soon.
const ARTIST_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const CHART_TTL: Duration = Duration::from_secs(60 * 60);
const SHORT_TTL: Duration = Duration::from_secs(10 * 60);
/// The country's genre menu changes rarely.
const MENU_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Ceiling on one build, shared by everyone waiting on it. A chart's page waits its turn at
/// Apple's gate behind the others, so it gets longer than an artist's list.
const ARTIST_BUILD_TIMEOUT: Duration = Duration::from_secs(15);
const CHART_BUILD_TIMEOUT: Duration = Duration::from_secs(45);

/// How many catalog artists of a name are weighed, as an outside artist page does.
const ARTIST_CANDIDATES: i32 = 5;

/// Library searches running at once for one list. Navidrome is local and quick.
pub const LIBRARY_SEARCHES_AT_ONCE: usize = 4;

/// How often a failing Apple list is warned about.
const WARN_EVERY: Duration = Duration::from_secs(60 * 60);

/// One ranked song. Plays and listeners are Last.fm's, when it gave them.
#[derive(Debug, Clone, Default)]
pub struct TopSong {
    pub rank: usize,
    pub song: Song,
    pub plays: Option<i64>,
    pub listeners: Option<i64>,
}

/// A ranked list and where its order came from.
#[derive(Debug, Clone, Default)]
pub struct TopList {
    /// The artist, on an artist's list; None on a chart.
    pub artist: Option<String>,
    /// `apple`, `deezer`, `lastfm`, or empty when nothing answered.
    pub source: String,
    /// The chart's id (`34`, `new`, `trending`, a genre id), on a chart.
    pub chart: Option<String>,
    /// The chart's playlist name, on a chart.
    pub name: Option<String>,
    /// The country the chart is for, when known.
    pub country: Option<String>,
    /// This week's room, on Best New Songs and Trending Songs.
    pub room_id: Option<String>,
    pub songs: Vec<TopSong>,
}

/// How a chart's last fetch went, for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartFetch {
    pub chart: String,
    pub country: Option<String>,
    pub source: String,
    pub songs: usize,
    pub fetched_utc: DateTime<Utc>,
    pub error: Option<String>,
    pub room_id: Option<String>,
}

/// Cheap to clone: the clones share one cache.
#[derive(Clone)]
pub struct TopSongsService {
    inner: Arc<Inner>,
}

struct Inner {
    deezer: DeezerMetadataService,
    last_fm: Option<Arc<LastFmService>>,
    metadata: Arc<dyn IMusicMetadataService>,
    registry: Arc<ExternalIdRegistry>,
    apple: Arc<AppleMusicPages>,
    hints: Arc<ArtworkHints>,
    settings: Arc<SettingsStore>,
    cache: MemoryCache<Arc<TopList>>,
    menus: MemoryCache<Arc<Vec<ChartGenre>>>,
    /// Whether a [`TopSongsService::learn`] started by a request is going.
    learning: AtomicBool,
    builds: InFlight<(Arc<TopList>, bool)>,
    fetches: Mutex<HashMap<String, ChartFetch>>,
    warned: Mutex<HashMap<String, Instant>>,
}

/// What a chart build came back with: the list, whether every source answered, and why not.
struct Built {
    list: TopList,
    whole: bool,
    error: Option<String>,
}

impl TopSongsService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        deezer: DeezerMetadataService,
        last_fm: Option<Arc<LastFmService>>,
        metadata: Arc<dyn IMusicMetadataService>,
        registry: Arc<ExternalIdRegistry>,
        apple: Arc<AppleMusicPages>,
        hints: Arc<ArtworkHints>,
        settings: Arc<SettingsStore>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                deezer,
                last_fm,
                metadata,
                registry,
                apple,
                hints,
                settings,
                cache: MemoryCache::new(512),
                menus: MemoryCache::new(64),
                learning: AtomicBool::new(false),
                builds: InFlight::new(),
                fetches: Mutex::new(HashMap::new()),
                warned: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The pages this reads Apple Music through, for the dashboard's country list.
    pub fn apple(&self) -> &Arc<AppleMusicPages> {
        &self.inner.apple
    }

    fn has_last_fm_key(&self) -> bool {
        self.inner.last_fm.as_ref().is_some_and(|l| l.has_api_key())
    }

    // ---- artists ---------------------------------------------------------------------------

    /// An artist's most played songs. `artist_id` may be an outside artist's id from search,
    /// which already names the catalog artist; a library id, or none, finds the artist by name.
    pub async fn for_artist(&self, name: &str, artist_id: Option<&str>) -> Arc<TopList> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Arc::default();
        }
        let known = artist_id
            .filter(|id| !is_blank(id))
            .and_then(|id| self.inner.registry.lookup(id))
            .map(|routing| routing.snapshot())
            .filter(|r| {
                r.kind == RoutingKind::Artist
                    && SongIdentity::same_artist_name(r.artist.as_deref().unwrap_or(""), &name)
            })
            .and_then(|r| r.external_artist_id)
            .filter(|id| !id.is_empty());
        let key = match &known {
            Some(id) => format!("artist|id|{id}"),
            None => format!("artist|name|{}", SongIdentity::key(&name)),
        };
        let this = self.clone();
        self.cached(key, ARTIST_TTL, ARTIST_BUILD_TIMEOUT, async move {
            let (list, whole) = this.build_artist(&name, known).await;
            (list, whole)
        })
        .await
    }

    async fn build_artist(&self, name: &str, known: Option<String>) -> (TopList, bool) {
        let last_fm = self.inner.last_fm.clone().filter(|l| l.has_api_key());
        let name_owned = name.to_string();
        let counts_task = async move {
            match last_fm {
                Some(l) => l.get_artist_chart(&name_owned, MAX_ROWS as i32).await,
                None => None,
            }
        };
        let catalog_task = async {
            let id = match known {
                Some(id) => Some(id),
                None => self.find_catalog_artist(name).await,
            };
            match id {
                None => Some(Arc::new(Vec::new())),
                Some(id) => self.inner.deezer.get_artist_top(&id, CHART_LIMIT).await,
            }
        };
        let (catalog, counts) = tokio::join!(catalog_task, counts_task);
        let (rows, source) = merge(
            catalog.as_deref().map(Vec::as_slice),
            counts.as_deref(),
            true,
            MAX_ROWS,
            DEEZER_SOURCE,
        );
        // Whole: every source that could answer did. A catalog miss on the name is an answer.
        let whole = catalog.is_some() && (counts.is_some() || !self.has_last_fm_key());
        let shown = rows
            .iter()
            .find_map(|row| row.catalog.as_ref().map(|c| c.artist.clone()))
            .unwrap_or_else(|| name.to_string());
        let songs = self.build_songs(&rows).await;
        info!("Top songs for '{name}': {} from {source}", songs.len());
        let artist = if SongIdentity::same_artist_name(&shown, name) {
            shown
        } else {
            name.to_string()
        };
        (
            TopList {
                artist: Some(artist),
                source: source.to_string(),
                songs,
                ..Default::default()
            },
            whole,
        )
    }

    /// The catalog artist of this exact name that more people follow, as an outside artist's
    /// page settles it when it holds no id.
    async fn find_catalog_artist(&self, name: &str) -> Option<String> {
        let hits = self.inner.deezer.search_artists(name, ARTIST_CANDIDATES).await;
        hits.into_iter()
            .filter(|hit| SongIdentity::same_artist_name(&hit.name, name))
            .max_by_key(|hit| hit.fans)
            .map(|hit| hit.deezer_id)
    }

    // ---- charts ----------------------------------------------------------------------------

    /// The country the charts are read for: the setting, or the one Apple was last seen to put
    /// the server in. Never waits on Apple: while Automatic has not learned the country yet this
    /// is None (Apple's own redirect picks it) and the learning starts behind the answer.
    pub async fn country(&self) -> Option<String> {
        match self.inner.settings.current().generated_playlists.chart_country() {
            ChartCountry::Fixed(code) => Some(code),
            ChartCountry::Auto => {
                let known = self.inner.apple.known_country();
                if known.is_none() {
                    self.learn_behind();
                }
                known
            }
        }
    }

    /// The country's genre menu, "All Genres" first, as Apple lists it; Octo's English list
    /// while Apple's is not known yet (learned behind the answer, never waited on).
    pub async fn menu(&self) -> (Option<String>, Arc<Vec<ChartGenre>>) {
        let country = self.country().await;
        let Some(code) = country.clone() else {
            return (None, Arc::new(english_menu()));
        };
        if let Some(menu) = self.inner.menus.get(&code) {
            return (country, menu);
        }
        self.learn_behind();
        (country, Arc::new(english_menu()))
    }

    /// Learns the country (when Automatic) and its genre menu from Apple, waiting for them: for
    /// the warm worker and tests, never a request. What cannot be learned stays unknown, so the
    /// English menu and Apple's redirect go on serving.
    pub async fn learn(&self) -> (Option<String>, Arc<Vec<ChartGenre>>) {
        let country = match self.inner.settings.current().generated_playlists.chart_country() {
            ChartCountry::Fixed(code) => Some(code),
            ChartCountry::Auto => self.inner.apple.detected_country().await,
        };
        let Some(code) = country.clone() else {
            return (None, Arc::new(english_menu()));
        };
        if let Some(menu) = self.inner.menus.get(&code) {
            return (country, menu);
        }
        match self.inner.apple.chart(Some(&code), OVERALL).await {
            Ok(page) if !page.genres.is_empty() => {
                let menu = Arc::new(page.genres.clone());
                self.inner.menus.set(code, menu.clone(), 1, MENU_TTL);
                (country, menu)
            }
            _ => (country, Arc::new(english_menu())),
        }
    }

    /// [`Self::learn`] in the background, one at a time; Apple's own memo of what failed keeps a
    /// server that cannot reach it from asking again for a while.
    fn learn_behind(&self) {
        if self.inner.learning.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            this.learn().await;
            this.inner.learning.store(false, Ordering::Release);
        });
    }

    /// A chart's name as a playlist and a chip: `Popular right now`, `Best New Songs`,
    /// `Trending Songs` or `Top {genre}`, with the genre's own name for the chip.
    pub fn names(menu: &[ChartGenre], id: &str) -> Option<(String, String)> {
        let label = match kind(id)? {
            ChartKind::Overall => "Top songs".to_string(),
            ChartKind::NewSongs => chart_settings::NEW_SONGS_NAME.to_string(),
            ChartKind::Trending => chart_settings::TRENDING_NAME.to_string(),
            ChartKind::Genre => genre_name(menu, id)?,
        };
        Some((playlist_name(id, &label), label))
    }

    /// The list a chart already holds for the current country, without building it.
    pub async fn peek_chart(&self, id: &str) -> Option<Arc<TopList>> {
        let country = self.country().await;
        self.inner.cache.get(&chart_key(country.as_deref(), id))
    }

    /// A chart: `34` (Popular right now), `new`, `trending` or a genre id. Empty for an id that is
    /// no chart, or a genre the country does not have, without asking anyone.
    pub async fn chart(&self, id: &str) -> Arc<TopList> {
        let Some(chart_kind) = kind(id) else {
            return Arc::default();
        };
        let (country, menu) = self.menu().await;
        if chart_kind == ChartKind::Genre && !menu.iter().any(|g| g.id == id) {
            return Arc::default();
        }
        let Some((name, _)) = Self::names(&menu, id) else {
            return Arc::default();
        };
        let key = chart_key(country.as_deref(), id);
        let this = self.clone();
        let id_owned = id.to_string();
        self.cached(key, CHART_TTL, CHART_BUILD_TIMEOUT, async move {
            let started = Instant::now();
            let built = this.build_chart(&id_owned, chart_kind, country.clone()).await;
            let Built {
                mut list,
                whole,
                error,
            } = built;
            list.chart = Some(id_owned.clone());
            list.name = Some(name);
            list.country = list.country.or(country.clone());
            this.record(&id_owned, &list, error.as_deref());
            info!(
                "Chart {id_owned} for {}: {} songs from {} in {} ms",
                list.country.as_deref().unwrap_or("the server's country"),
                list.songs.len(),
                if list.source.is_empty() {
                    "nowhere"
                } else {
                    list.source.as_str()
                },
                started.elapsed().as_millis()
            );
            (list, whole)
        })
        .await
    }

    async fn build_chart(&self, id: &str, chart_kind: ChartKind, country: Option<String>) -> Built {
        let max = max_songs(id);
        let apple = match chart_kind {
            ChartKind::Overall | ChartKind::Genre => self
                .inner
                .apple
                .chart(country.as_deref(), id)
                .await
                .map(|page| (None, page)),
            ChartKind::NewSongs | ChartKind::Trending => match country.as_deref() {
                None => Err(PageError::Unavailable("the country is not known".into())),
                Some(code) => {
                    let room = if chart_kind == ChartKind::NewSongs {
                        RoomKind::BestNewSongs
                    } else {
                        RoomKind::TrendingSongs
                    };
                    self.inner
                        .apple
                        .room(code, room)
                        .await
                        .map(|(room_id, page)| (Some(room_id), page))
                }
            },
        };
        match apple {
            Ok((room_id, page)) => {
                if chart_kind == ChartKind::Overall
                    && !page.genres.is_empty()
                    && octo_core::charts::apple_pages::is_country_code(&page.country)
                {
                    self.inner
                        .menus
                        .set(page.country.clone(), Arc::new(page.genres.clone()), 1, MENU_TTL);
                }
                let catalog: Vec<CatalogTrack> = page.songs.iter().map(CatalogTrack::from_apple).collect();
                let (rows, source) = merge(Some(catalog.as_slice()), None, false, max, APPLE_SOURCE);
                let songs = self.build_songs(&rows).await;
                let whole = !songs.is_empty();
                Built {
                    list: TopList {
                        source: source.to_string(),
                        country: Some(page.country.clone()).filter(|c| !c.is_empty()),
                        room_id,
                        songs,
                        ..Default::default()
                    },
                    whole,
                    error: None,
                }
            }
            Err(error) => {
                let why = error.describe();
                self.warn_once(id, &why);
                if chart_kind != ChartKind::Overall {
                    return Built {
                        list: TopList::default(),
                        whole: false,
                        error: Some(why),
                    };
                }
                self.backup_overall(why).await
            }
        }
    }

    /// Popular right now when Apple's chart cannot be read: Deezer's, then Last.fm's.
    async fn backup_overall(&self, apple_error: String) -> Built {
        if let Some(catalog) = self.inner.deezer.get_chart(CHART_LIMIT).await {
            let (rows, source) = merge(
                Some(catalog.as_slice()),
                None,
                false,
                max_songs(OVERALL),
                DEEZER_SOURCE,
            );
            let songs = self.build_songs(&rows).await;
            return Built {
                list: TopList {
                    source: source.to_string(),
                    songs,
                    ..Default::default()
                },
                whole: true,
                error: Some(apple_error),
            };
        }
        let counts: Option<Vec<CountedTrack>> = match self.inner.last_fm.as_ref().filter(|l| l.has_api_key())
        {
            Some(l) => l.get_chart(max_songs(OVERALL) as i32).await,
            None => None,
        };
        let (rows, source) = merge(None, counts.as_deref(), false, max_songs(OVERALL), DEEZER_SOURCE);
        let songs = self.build_songs(&rows).await;
        Built {
            list: TopList {
                source: source.to_string(),
                songs,
                ..Default::default()
            },
            whole: false,
            error: Some(apple_error),
        }
    }

    /// Every chart's last fetch, for the dashboard.
    pub fn fetches(&self) -> Vec<ChartFetch> {
        self.inner.fetches.lock().values().cloned().collect()
    }

    fn record(&self, id: &str, list: &TopList, error: Option<&str>) {
        let fetch = ChartFetch {
            chart: id.to_string(),
            country: list.country.clone(),
            source: list.source.clone(),
            songs: list.songs.len(),
            fetched_utc: Utc::now(),
            error: error.map(str::to_string),
            room_id: list.room_id.clone(),
        };
        self.inner.fetches.lock().insert(id.to_string(), fetch);
    }

    fn warn_once(&self, id: &str, why: &str) {
        let now = Instant::now();
        let mut warned = self.inner.warned.lock();
        let due = warned
            .get(id)
            .is_none_or(|at| now.duration_since(*at) >= WARN_EVERY);
        if due {
            warned.insert(id.to_string(), now);
            warn!("Chart {id}: {why}");
        } else {
            debug!("Chart {id}: {why}");
        }
    }

    // ---- shared ----------------------------------------------------------------------------

    async fn cached<F>(&self, key: String, ttl: Duration, timeout: Duration, build: F) -> Arc<TopList>
    where
        F: Future<Output = (TopList, bool)> + Send + 'static,
    {
        if let Some(cached) = self.inner.cache.get(&key) {
            return cached;
        }
        let this = self.clone();
        let cache_key = key.clone();
        let (list, _) = self
            .inner
            .builds
            .run(&key, async move {
                let (list, whole) = match tokio::time::timeout(timeout, build).await {
                    Ok(built) => built,
                    Err(_) => {
                        // Top songs are an addition, never a reason for anything to fail.
                        debug!("top songs '{cache_key}' timed out");
                        return (Arc::new(TopList::default()), false);
                    }
                };
                let list = Arc::new(list);
                let keep = if whole && !list.songs.is_empty() {
                    ttl
                } else {
                    SHORT_TTL
                };
                this.inner.cache.set(cache_key, list.clone(), 1, keep);
                (list, whole)
            })
            .await;
        list
    }

    /// Outside songs for the rows, registered so they play, open their album and are added like
    /// any other outside song. A row a catalog dressed carries its album, cover and length; the
    /// rest are looked up the way search looks its rows up.
    async fn build_songs(&self, rows: &[PlannedRow]) -> Vec<TopSong> {
        let mut found: Vec<Option<(Song, &PlannedRow)>> = Vec::with_capacity(rows.len());
        let mut bare_at: Vec<usize> = Vec::new();
        for row in rows {
            match &row.catalog {
                Some(track) => found.push(Some((
                    catalog_song(&self.inner.registry, &self.inner.hints, track),
                    row,
                ))),
                None => {
                    let hits = self
                        .inner
                        .metadata
                        .search_songs_by_artist_title(&row.artist, &row.title, 1, None)
                        .await;
                    match hits.into_iter().next() {
                        Some(song) => {
                            bare_at.push(found.len());
                            found.push(Some((song, row)));
                        }
                        None => found.push(None),
                    }
                }
            }
        }
        if !bare_at.is_empty() {
            let mut bare: Vec<Song> = bare_at
                .iter()
                .filter_map(|at| found[*at].as_ref().map(|(song, _)| song.clone()))
                .collect();
            self.inner.metadata.enrich_external_songs(&mut bare).await;
            for (at, song) in bare_at.iter().zip(bare) {
                if let Some((slot, _)) = found[*at].as_mut() {
                    *slot = song;
                }
            }
        }
        let songs: Vec<TopSong> = found
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(i, (song, row))| TopSong {
                rank: i + 1,
                song,
                plays: row.plays,
                listeners: row.listeners,
            })
            .collect();

        // A list is opened to be played: warm the covers it shows and the first few streams, as
        // search does for its own first screen. Never awaited.
        let all: Vec<Song> = songs.iter().map(|top| top.song.clone()).collect();
        let metadata = Arc::clone(&self.inner.metadata);
        tokio::spawn(async move {
            metadata.prewarm_cover_art(&all, 20).await;
            metadata.prewarm_you_tube_ids(&all, 5).await;
        });
        songs
    }

    /// The library song each row already is, or None, in row order. `search_songs` searches the
    /// caller's library and answers its song list (Navidrome's searchResult3 songs), or None when
    /// it could not. A row is the library's only when it is the same recording, by the rule radio
    /// uses to play a library copy in place of an outside one.
    pub async fn match_library<F, Fut>(rows: &[TopSong], search_songs: F) -> Vec<Option<Value>>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Option<Vec<Value>>>,
    {
        // Plain futures behind a semaphore, not a stream combinator: a combinator's closures
        // make a handler's future fail rustc's Send check ("not general enough").
        let gate = tokio::sync::Semaphore::new(LIBRARY_SEARCHES_AT_ONCE);
        let searches = rows.iter().map(|row| {
            let (artist, title) = (row.song.artist.clone(), row.song.title.clone());
            let search = search_songs(library_query(&artist, &title));
            let gate = &gate;
            async move {
                let _turn = gate.acquire().await.ok()?;
                let hits = search.await?;
                library_hit(&artist, &title, &hits).cloned()
            }
        });
        let mut found: Vec<Option<Value>> = futures::future::join_all(searches).await;
        one_row_per_library_song(&mut found);
        found
    }
}

fn chart_key(country: Option<&str>, id: &str) -> String {
    format!("chart|{}|{id}", country.unwrap_or("auto"))
}

/// A catalog track as an outside song: registered through New Releases' shared
/// [`for_you_lists::catalog_song`] (artist, album and song, so it plays, opens its album and is
/// added like any other), then given what a chart row carries besides: every credited artist,
/// the explicit mark, and its artwork remembered for its cover.
pub fn catalog_song(registry: &ExternalIdRegistry, hints: &ArtworkHints, track: &CatalogTrack) -> Song {
    let album = track.album.as_deref().filter(|a| !is_blank(a));
    let mut song = for_you_lists::catalog_song(
        registry,
        &for_you_lists::CatalogTrackSong {
            title: &track.title,
            artist: &track.artist,
            artist_id: track.artist_id.as_deref(),
            album,
            album_id: track.album_id.as_deref(),
            cover_url: track.cover_url.as_deref(),
            duration: track.duration,
        },
    );
    if let Some(url) = track.cover_url.as_deref() {
        hints.remember(RoutingKind::Song, &track.artist, &track.title, url);
        if let Some(album) = album {
            hints.remember(RoutingKind::Album, &track.artist, album, url);
        }
    }
    song.explicit_content_lyrics = track
        .explicit_content
        .or(track.explicit.map(|e| if e { 1 } else { 0 }));
    song.artists = track.artists.clone();
    song.primary_artist = (!track.artists.is_empty()).then(|| track.artist.clone());
    song
}

#[cfg(test)]
#[path = "top_songs_service_tests.rs"]
mod tests;
