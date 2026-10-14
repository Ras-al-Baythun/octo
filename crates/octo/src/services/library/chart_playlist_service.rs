//! The chart playlists: Popular right now, Best New Songs, Trending Songs and the genre charts,
//! each a read-only playlist for every listener, so every Subsonic app gets them beside the radio
//! stations and mixes. A song the listener's library has is their own copy, as Navidrome
//! describes it to them; the rest are outside songs that play right away and are added with a
//! heart, while search discovery is on and the explicit filter lets them.
//!
//! Per listener because the library match is theirs, and made with their own sign-in inside
//! their own request, as the mixes are: Octo keeps no one's Navidrome password. A list holds
//! still for [`REFRESH_HOURS`] so every app shows the same songs, then is made again the next
//! time that listener's app lists playlists. A source or library that does not answer never
//! empties a list that was made. Nothing is written to Navidrome, and nothing to disk.
//!
//! A listener's lists are made one after another (six lists searched at once would ask Navidrome
//! two dozen searches at a time), a song on two lists is searched once, a list is listed as soon
//! as its chart is known while search discovery is on (the warm worker fetches the charts at
//! start, so an app's first look after a restart has them), and a list's change time moves only
//! when its songs do.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::charts::chart_settings::{self, ChartKind, OVERALL};
use octo_core::charts::top_songs::{library_hit, library_query, one_row_per_library_song};
use octo_core::common::Clock;
use octo_core::library::generated_playlist_service::{period_index, playlist_id, user_key};
use octo_core::models::domain::Song;
use octo_core::models::radio::{LastFmRadioStation, LastFmRadioTrack};
use octo_core::outside::LengthSource;
use octo_core::outside::routing::{OutsideRouting, RoutingKind};
use octo_core::settings::{ExplicitFilter, SettingsStore};
use octo_subsonic::SubsonicResponseBuilder;
use octo_subsonic::subsonic_request_parser::Parameters;
use parking_lot::Mutex;
use serde_json::Value;
use tracing::{debug, info};

use crate::services::common::{TopSong, TopSongsService, top_songs_service::LIBRARY_SEARCHES_AT_ONCE};
use crate::services::family::{FamilyStore, kid_filter};
use crate::services::framework::InFlight;
use crate::services::library::GeneratedPlaylist;
use crate::services::outside::external_id_registry::ExternalIdRegistry;
use crate::services::subsonic::SubsonicProxyService;

/// The kind every chart list carries, as a mix carries "genre" or "decade".
pub const KIND: &str = "chart";

/// Marks the sync catalog's tracks that came from a chart list, whose ids are the list's own
/// rather than found again by search. (Popular right now's word, kept for every list.)
pub const TRACK_SOURCE: &str = "popular";

/// How long one list holds still.
pub const REFRESH_HOURS: i32 = 6;

const FIRST_LIST_WAIT: Duration = Duration::from_millis(1500);
const OPEN_WAIT: Duration = Duration::from_secs(10);
const BUILD_DEADLINE: Duration = Duration::from_secs(60);
const RETRY_AFTER_FAILURE: TimeDelta = TimeDelta::minutes(5);

/// The Subsonic parameters that say who is asking: a password, a token and its salt, or an API
/// key, with the protocol version and the app's name.
const SIGN_IN_KEYS: [&str; 7] = ["u", "p", "t", "s", "apiKey", "v", "c"];

/// A listener's list as made: for which period and settings, whether every library lookup
/// answered, its songs as Subsonic song JSON, and its outside songs.
#[derive(Debug, Clone)]
struct Made {
    period: i64,
    made_utc: DateTime<Utc>,
    changed_utc: DateTime<Utc>,
    rule: String,
    whole: bool,
    name: String,
    hash: u64,
    entries: Vec<Value>,
    outside: Vec<Song>,
}

/// What a list's cover is drawn from.
#[derive(Debug, Clone)]
pub struct ChartCover {
    pub name: String,
    pub chart: String,
    pub entries: Vec<Value>,
    pub outside: Vec<Song>,
}

/// Cheap to clone: the clones share one set of lists.
#[derive(Clone)]
pub struct ChartPlaylistService {
    inner: Arc<Inner>,
}

struct Inner {
    top_songs: TopSongsService,
    responses: Arc<SubsonicResponseBuilder>,
    registry: Arc<ExternalIdRegistry>,
    proxy: SubsonicProxyService,
    settings: Arc<SettingsStore>,
    clock: Clock,
    made: Mutex<HashMap<String, HashMap<String, Made>>>,
    failed_at: Mutex<HashMap<String, DateTime<Utc>>>,
    builds: InFlight<bool>,
    /// The family, whose kid profiles get clean lists.
    family: Option<Arc<FamilyStore>>,
}

impl ChartPlaylistService {
    pub fn new(
        top_songs: TopSongsService,
        responses: Arc<SubsonicResponseBuilder>,
        registry: Arc<ExternalIdRegistry>,
        proxy: SubsonicProxyService,
        settings: Arc<SettingsStore>,
        clock: Clock,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                top_songs,
                responses,
                registry,
                proxy,
                settings,
                clock,
                made: Mutex::new(HashMap::new()),
                failed_at: Mutex::new(HashMap::new()),
                builds: InFlight::new(),
                family: None,
            }),
        }
    }

    /// The service with the family, so a kid profile's lists are clean. Call it before the
    /// service is cloned.
    pub fn with_family(mut self, family: Arc<FamilyStore>) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.family = Some(family);
        }
        self
    }

    /// The explicit filter for one listener's lists: the server's, or clean only for a kid
    /// profile.
    fn explicit_filter_for(&self, username: &str) -> ExplicitFilter {
        kid_filter::explicit_filter_for(
            &self.inner.settings.current(),
            self.inner.family.as_deref(),
            Some(username),
        )
    }

    /// The charts that are lists, in their order: Popular right now first while it is on, then
    /// the `Charts` setting's. Genres are checked against the country's menu by [`Self::list`].
    pub fn lists_on(&self) -> Vec<String> {
        let settings = &self.inner.settings.current().generated_playlists;
        let mut on = Vec::new();
        if settings.popular_now {
            on.push(OVERALL.to_string());
        }
        on.extend(settings.chart_ids());
        on
    }

    /// The lists that are on and the country has, with their names.
    async fn charts_on(&self) -> Vec<(String, String)> {
        let (_, menu) = self.inner.top_songs.menu().await;
        self.lists_on()
            .into_iter()
            .filter_map(|chart| {
                if chart_settings::kind(&chart) == Some(ChartKind::Genre)
                    && !menu.iter().any(|g| g.id == chart)
                {
                    return None;
                }
                let (name, _) = TopSongsService::names(&menu, &chart)?;
                Some((chart, name))
            })
            .collect()
    }

    /// The listener's lists, made again first when their period ended or the settings they were
    /// made under changed. A listener's very first lists wait a moment, so they show on their
    /// first look; later ones are made behind the answer.
    pub async fn list(&self, username: &str, auth: &Parameters) -> Vec<GeneratedPlaylist> {
        if username.trim().is_empty() {
            return Vec::new();
        }
        let (any_made, building) = self.ensure(username, auth).await;
        if !any_made && let Some(building) = building {
            let _ = tokio::time::timeout(FIRST_LIST_WAIT, building).await;
        }
        let discovery = self.inner.settings.current().subsonic.enable_search_discovery;
        let mut lists = Vec::new();
        for (chart, name) in self.charts_on().await {
            let made = self.current(username, &chart);
            let pool = match &made {
                Some(made) if !made.entries.is_empty() => made.entries.len(),
                _ if discovery => match self.inner.top_songs.peek_chart(&chart).await {
                    Some(source) => source.songs.len().min(chart_settings::max_songs(&chart)),
                    None => 0,
                },
                _ => 0,
            };
            if pool > 0 {
                lists.push(self.playlist(username, &chart, made.map_or(name, |m| m.name), pool));
            }
        }
        lists
    }

    /// Which chart a playlist id is, for this listener, while that list is on: an app that kept
    /// the id across a restart still opens it.
    pub fn which(&self, username: &str, id: &str) -> Option<String> {
        if username.trim().is_empty() || id.is_empty() {
            return None;
        }
        self.lists_on()
            .into_iter()
            .find(|chart| playlist_id(username, &chart_settings::playlist_key(chart)) == id)
    }

    /// The listener's playlist id for a chart, while the list is made and holds songs.
    pub fn listed_id(&self, username: &str, chart: &str) -> Option<String> {
        let made = self.current(username, chart)?;
        (!made.entries.is_empty()).then(|| playlist_id(username, &chart_settings::playlist_key(chart)))
    }

    /// The list as a playlist: its period, and `pool` songs.
    pub fn playlist(&self, username: &str, chart: &str, name: String, pool: usize) -> GeneratedPlaylist {
        let period = self.current(username, chart).map_or_else(
            || period_index(self.inner.clock.now(), REFRESH_HOURS),
            |m| m.period,
        );
        let start = period_start(period);
        GeneratedPlaylist {
            id: playlist_id(username, &chart_settings::playlist_key(chart)),
            key: chart_settings::playlist_key(chart),
            kind: KIND.to_string(),
            label: name.clone(),
            name,
            owner: username.trim().to_string(),
            pool_size: i32::try_from(pool).unwrap_or(i32::MAX),
            period_start_utc: start,
            period_end_utc: start + TimeDelta::hours(i64::from(REFRESH_HOURS)),
        }
    }

    /// The list's playlist as made, for getPlaylist; None while nothing is made.
    pub fn made_playlist(&self, username: &str, chart: &str) -> Option<GeneratedPlaylist> {
        let made = self.current(username, chart)?;
        Some(self.playlist(username, chart, made.name.clone(), made.entries.len()))
    }

    /// Starts making the listener's lists when they are due, without waiting.
    pub async fn warm(&self, username: &str, auth: &Parameters) {
        if !username.trim().is_empty() {
            let _ = self.ensure(username, auth).await;
        }
    }

    /// The list's songs, made again first when due (waiting a few seconds for that, then
    /// answering with the last one rather than keep an app waiting).
    pub async fn songs(&self, username: &str, chart: &str, auth: &Parameters) -> Vec<Value> {
        if username.trim().is_empty() {
            return Vec::new();
        }
        let (_, building) = self.ensure(username, auth).await;
        if let Some(building) = building {
            let made = self
                .current(username, chart)
                .is_some_and(|m| !m.entries.is_empty());
            let wait = if made { OPEN_WAIT } else { BUILD_DEADLINE };
            let _ = tokio::time::timeout(wait, building).await;
        }
        let Some(made) = self.current(username, chart) else {
            return Vec::new();
        };
        self.keep_playable(&made.outside);
        made.entries
    }

    /// What the list's cover is drawn from, as last made. Never makes one.
    pub fn cover(&self, username: &str, id: &str) -> Option<ChartCover> {
        let chart = self.which(username, id)?;
        let made = self.current(username, &chart)?;
        Some(ChartCover {
            name: made.name,
            chart,
            entries: made.entries,
            outside: made.outside,
        })
    }

    /// The list's outside songs as last made, in order. Never makes one.
    pub fn outside_songs(&self, username: &str, chart: &str) -> Vec<Song> {
        self.current(username, chart)
            .map(|m| m.outside)
            .unwrap_or_default()
    }

    /// Outside songs play by their id, which Octo remembers in memory for a while. A list holds
    /// still for hours, so before it is handed out each of its outside songs is made known again
    /// if it was forgotten: the same song gives the same id.
    pub fn keep_playable(&self, songs: &[Song]) {
        let registry = &self.inner.registry;
        for song in songs {
            if registry.lookup(&song.id).is_some() {
                continue;
            }
            if song
                .artist_id
                .as_deref()
                .is_some_and(|id| registry.lookup(id).is_none())
            {
                registry.register(OutsideRouting {
                    kind: RoutingKind::Artist,
                    artist: Some(song.artist.clone()),
                    ..Default::default()
                });
            }
            if song
                .album_id
                .as_deref()
                .is_some_and(|id| registry.lookup(id).is_none())
                && !song.album.trim().is_empty()
            {
                registry.register(OutsideRouting {
                    kind: RoutingKind::Album,
                    artist: Some(song.artist.clone()),
                    album: Some(song.album.clone()),
                    ..Default::default()
                });
            }
            let id = registry.register(OutsideRouting {
                kind: RoutingKind::Song,
                artist: Some(song.artist.clone()),
                title: Some(song.title.clone()),
                album: Some(song.album.clone()).filter(|a| !a.is_empty()),
                duration: song.duration,
                ..Default::default()
            });
            registry.remember_length(&id, song.duration, LengthSource::Deezer);
        }
    }

    /// Each list's outside songs as a station, for the sync catalog of a client that copies the
    /// library to the device: it then holds these songs under the ids the playlist lists, so the
    /// playlist finds them there.
    pub fn as_sync_stations(&self, username: &str) -> Vec<LastFmRadioStation> {
        let on = self.lists_on();
        let user = user_key(username);
        let made = self.inner.made.lock().get(&user).cloned().unwrap_or_default();
        on.iter()
            .filter_map(|chart| {
                let made = made.get(chart).filter(|m| !m.outside.is_empty())?;
                Some(LastFmRadioStation {
                    id: playlist_id(username, &chart_settings::playlist_key(chart)),
                    key: chart_settings::playlist_key(chart),
                    name: made.name.clone(),
                    owner: username.trim().to_string(),
                    created_utc: made.made_utc,
                    changed_utc: made.changed_utc,
                    valid_until_utc: period_start(made.period + 1),
                    tracks: made
                        .outside
                        .iter()
                        .map(|song| LastFmRadioTrack {
                            artist: song.artist.clone(),
                            title: song.title.clone(),
                            album: Some(song.album.clone()),
                            duration: song.duration,
                            source: TRACK_SOURCE.to_string(),
                            resolved_id: Some(song.id.clone()),
                            external_provider: song.external_provider.clone(),
                            ..Default::default()
                        })
                        .collect(),
                    ..Default::default()
                })
            })
            .collect()
    }

    fn current(&self, username: &str, chart: &str) -> Option<Made> {
        self.inner
            .made
            .lock()
            .get(&user_key(username))?
            .get(chart)
            .cloned()
    }

    /// What decides whether a made list still holds: outside songs shown or not, which of them
    /// the listener's explicit filter lets through, and whose charts they are.
    async fn rule(&self, username: &str) -> String {
        let subsonic = self.inner.settings.current().subsonic.clone();
        let country = self
            .inner
            .top_songs
            .country()
            .await
            .unwrap_or_else(|| "auto".into());
        format!(
            "{}|{:?}|{country}",
            subsonic.enable_search_discovery,
            self.explicit_filter_for(username)
        )
    }

    /// Whether any list is made, and the making of new ones when some are due.
    async fn ensure(
        &self,
        username: &str,
        auth: &Parameters,
    ) -> (
        bool,
        Option<futures::future::Shared<futures::future::BoxFuture<'static, bool>>>,
    ) {
        let user = user_key(username);
        let now = self.inner.clock.now();
        let period = period_index(now, REFRESH_HOURS);
        let rule = self.rule(username).await;
        // Only the lists the country has: a genre it lacks is never made, and must not make the
        // others due on every request.
        let on: Vec<String> = self
            .charts_on()
            .await
            .into_iter()
            .map(|(chart, _)| chart)
            .collect();
        let (any_made, due) = {
            let made = self.inner.made.lock();
            let mine = made.get(&user);
            let any_made =
                mine.is_some_and(|m| on.iter().any(|c| m.get(c).is_some_and(|x| !x.entries.is_empty())));
            let due = on.iter().any(|chart| {
                mine.and_then(|m| m.get(chart))
                    .is_none_or(|m| m.period != period || m.rule != rule || !m.whole)
            });
            (any_made, due)
        };
        let failed_recently = self
            .inner
            .failed_at
            .lock()
            .get(&user)
            .is_some_and(|at| now - *at < RETRY_AFTER_FAILURE);
        if !due || failed_recently || on.is_empty() {
            return (any_made, None);
        }
        let sign_in: Parameters = auth
            .iter()
            .filter(|(k, _)| SIGN_IN_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let this = self.clone();
        let name = username.to_string();
        let building = self.inner.builds.run(
            &user,
            async move { this.make(&name, sign_in, period, rule).await },
        );
        (any_made, Some(building))
    }

    /// Make the listener's lists, one after another: each chart, each song matched against their
    /// library by the rule radio uses to play a library copy, the rest kept as outside songs when
    /// they may be shown.
    async fn make(&self, username: &str, auth: Parameters, period: i64, rule: String) -> bool {
        let user = user_key(username);
        let subsonic = self.inner.settings.current().subsonic.clone();
        let explicit_filter = self.explicit_filter_for(username);
        let mut searched: HashMap<String, Option<Vec<Value>>> = HashMap::new();
        let mut trouble = false;
        let mut any = false;
        for (chart, name) in self.charts_on().await {
            let list = self.inner.top_songs.chart(&chart).await;
            let rows: Vec<TopSong> = list
                .songs
                .iter()
                .take(chart_settings::max_songs(&chart))
                .cloned()
                .collect();
            if rows.is_empty() {
                trouble = true;
                info!("{name} for {username}: the chart did not answer, the last list stays");
                continue;
            }

            // Search what no earlier list of this listener searched, four at a time.
            let queries: Vec<String> = rows
                .iter()
                .map(|r| library_query(&r.song.artist, &r.song.title))
                .collect();
            let missing: Vec<String> = {
                let mut seen = Vec::new();
                for q in &queries {
                    if !searched.contains_key(q) && !seen.contains(q) {
                        seen.push(q.clone());
                    }
                }
                seen
            };
            // Four at a time, as plain futures behind a semaphore (see `match_library`).
            let gate = tokio::sync::Semaphore::new(LIBRARY_SEARCHES_AT_ONCE);
            let asks = missing.into_iter().map(|query| {
                let (proxy, auth, gate) = (self.inner.proxy.clone(), auth.clone(), &gate);
                async move {
                    let _turn = gate.acquire().await.ok();
                    let found = search_library(&proxy, &auth, &query).await;
                    (query, found)
                }
            });
            let answers: Vec<(String, Option<Vec<Value>>)> = futures::future::join_all(asks).await;
            searched.extend(answers);

            let mut unanswered = 0;
            let mut library: Vec<Option<Value>> = rows
                .iter()
                .zip(&queries)
                .map(|(row, query)| match searched.get(query).cloned().flatten() {
                    Some(hits) => library_hit(&row.song.artist, &row.song.title, &hits).cloned(),
                    None => {
                        unanswered += 1;
                        None
                    }
                })
                .collect();
            one_row_per_library_song(&mut library);

            let before = self.current(username, &chart);
            // A library that answered nothing at all must not turn a list of the listener's own
            // songs into outside ones.
            if unanswered == rows.len() && before.as_ref().is_some_and(|b| !b.entries.is_empty()) {
                trouble = true;
                continue;
            }
            if unanswered > 0 {
                trouble = true;
            }

            let (entries, outside) = assemble(
                &rows,
                &library,
                subsonic.enable_search_discovery,
                explicit_filter,
                |song| Value::Object(self.inner.responses.convert_song_to_json(song)),
            );
            let hash = entries_hash(&entries);
            let now = self.inner.clock.now();
            let changed_utc = before
                .as_ref()
                .filter(|b| b.hash == hash)
                .map_or(now, |b| b.changed_utc);
            info!(
                "{name} for {username}: {} library and {} outside songs from the {} chart",
                entries.len() - outside.len(),
                outside.len(),
                if list.source.is_empty() {
                    "unknown"
                } else {
                    list.source.as_str()
                }
            );
            let made = Made {
                period,
                made_utc: now,
                changed_utc,
                rule: rule.clone(),
                whole: unanswered == 0,
                name,
                hash,
                entries,
                outside,
            };
            self.inner
                .made
                .lock()
                .entry(user.clone())
                .or_default()
                .insert(chart, made);
            any = true;
        }
        let mut failed = self.inner.failed_at.lock();
        if trouble {
            failed.insert(user, self.inner.clock.now());
        } else {
            failed.remove(&user);
        }
        if !any {
            debug!("chart lists for {username}: nothing could be made");
        }
        any
    }
}

/// The list in chart order: the library's song where the row has one, else the outside song when
/// outside songs may be shown and the explicit filter lets it through. The filter is the one
/// stations follow: a list Octo picks follows it, a search does not.
pub fn assemble(
    rows: &[TopSong],
    library: &[Option<Value>],
    outside_shown: bool,
    filter: ExplicitFilter,
    as_json: impl Fn(&Song) -> Value,
) -> (Vec<Value>, Vec<Song>) {
    let mut entries = Vec::new();
    let mut outside = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if let Some(owned) = library.get(i).and_then(Option::as_ref) {
            entries.push(owned.clone());
        } else if outside_shown && passes(&row.song, filter) {
            entries.push(as_json(&row.song));
            outside.push(row.song.clone());
        }
    }
    (entries, outside)
}

/// `ExplicitStatus.ForClients` against the filter: a clean-only list leaves out what is marked
/// explicit (1, or 4 for a partly explicit album), an explicit-only one what is a clean edit (3).
pub fn passes(song: &Song, filter: ExplicitFilter) -> bool {
    let explicit = matches!(song.explicit_content_lyrics, Some(1 | 4));
    let clean = song.explicit_content_lyrics == Some(3);
    match filter {
        ExplicitFilter::CleanOnly => !explicit,
        ExplicitFilter::ExplicitOnly => !clean,
        _ => true,
    }
}

fn entries_hash(entries: &[Value]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for entry in entries {
        entry
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .hash(&mut hasher);
    }
    hasher.finish()
}

fn period_start(period: i64) -> DateTime<Utc> {
    DateTime::UNIX_EPOCH + TimeDelta::hours(period * i64::from(REFRESH_HOURS))
}

/// Navidrome's songs for a query, searched as the listener with only their sign-in: Some (maybe
/// empty) when it answered, None when it did not. Library only.
async fn search_library(proxy: &SubsonicProxyService, auth: &Parameters, query: &str) -> Option<Vec<Value>> {
    let mut search = auth.clone();
    search.insert("query".into(), query.to_string());
    search.insert("songCount".into(), "5".into());
    search.insert("albumCount".into(), "0".into());
    search.insert("artistCount".into(), "0".into());
    search.insert("f".into(), "json".into());
    let relay = proxy.relay_safe("rest/search3", &search).await?;
    let doc: Value = serde_json::from_slice(&relay.body).ok()?;
    let response = doc.get("subsonic-response")?;
    if response.get("status").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    Some(
        response
            .pointer("/searchResult3/song")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    )
}

#[cfg(test)]
#[path = "chart_playlist_service_tests.rs"]
mod tests;
