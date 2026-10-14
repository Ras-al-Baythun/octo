//! The Last.fm radio state store: `lastfm-radio-state.json`.
//!
//! Bounded local state for Last.fm radio. This follows DownloadHistoryService's locked cache +
//! temporary-file rename pattern and is deliberately single-writer. The station ids and the
//! bounds are `octo_core::last_fm::last_fm_radio_state_store`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::{Clock, dotnet};
use octo_core::last_fm::last_fm_radio_recommendation_service::IgnoreCaseKeys;
use octo_core::last_fm::last_fm_radio_seed_normalizer as seed_normalizer;
use octo_core::last_fm::last_fm_radio_state_store::{
    CURRENT_VERSION, DUPLICATE_WINDOW, MAX_PLAYS_PER_USER, MAX_RADIO_BANS_PER_USER,
    MAX_UNAVAILABLE_TRACKS_PER_USER, MAX_USERS, UNAVAILABLE_TRACK_COOLDOWN, user_key,
};
use octo_core::models::radio::{
    LastFmRadioBan, LastFmRadioPlay, LastFmRadioStateDocument, LastFmRadioStation, LastFmRadioTrack,
    LastFmRadioUnavailableTrack, LastFmRadioUserState, LastFmRadioUserSummary,
};
use octo_core::outside::routing::{OutsideRouting, RoutingKind};
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use serde::Serialize;
use serde::ser::SerializeMap;
use tracing::warn;

use crate::services::framework::DotnetDictionary;
use crate::services::outside::{ExternalIdRegistry, OutsideMetadataService};
use crate::services::state_file;

pub use octo_core::last_fm::last_fm_radio_state_store::{station_id, to_base62};

/// The loaded document. `users` is keyed by the ignore-case key, each entry holding the key as
/// it was written and the user, in the order a [`DotnetDictionary`] enumerates.
struct Document {
    version: i32,
    users: DotnetDictionary<(String, LastFmRadioUserState)>,
}

impl Document {
    fn empty() -> Self {
        Document {
            version: CURRENT_VERSION,
            users: DotnetDictionary::new(),
        }
    }

    fn get(&self, key: &str) -> Option<&LastFmRadioUserState> {
        self.users
            .get(&dotnet::ordinal_ignore_case_key(key))
            .map(|(_, user)| user)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut LastFmRadioUserState> {
        self.users
            .get_mut(&dotnet::ordinal_ignore_case_key(key))
            .map(|(_, user)| user)
    }
}

/// Writes the document as `LastFmRadioStateDocument` would be written: `Version`, then `Users`
/// in dictionary order.
impl Serialize for Document {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        struct Users<'a>(&'a DotnetDictionary<(String, LastFmRadioUserState)>);
        impl Serialize for Users<'_> {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let mut map = serializer.serialize_map(Some(self.0.len()))?;
                for (key, user) in self.0.values() {
                    map.serialize_entry(key, user)?;
                }
                map.end()
            }
        }
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("Version", &self.version)?;
        map.serialize_entry("Users", &Users(&self.users))?;
        map.end()
    }
}

pub struct LastFmRadioStateStore {
    path: PathBuf,
    /// The Last.fm settings: the retention is read at every prune.
    settings: Arc<SettingsStore>,
    registry: Arc<ExternalIdRegistry>,
    clock: Clock,
    state: Mutex<Option<Document>>,
}

impl LastFmRadioStateStore {
    pub fn new(
        path: impl Into<PathBuf>,
        settings: Arc<SettingsStore>,
        registry: Arc<ExternalIdRegistry>,
    ) -> Self {
        Self::with_clock(path, settings, registry, Clock::system())
    }

    /// The store on another clock, for tests.
    pub fn with_clock(
        path: impl Into<PathBuf>,
        settings: Arc<SettingsStore>,
        registry: Arc<ExternalIdRegistry>,
        clock: Clock,
    ) -> Self {
        LastFmRadioStateStore {
            path: path.into(),
            settings,
            registry,
            clock,
            state: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record_play(&self, username: &str, mut play: LastFmRadioPlay) -> bool {
        if dotnet::is_blank(username) || dotnet::is_blank(&play.artist) || dotnet::is_blank(&play.title) {
            return false;
        }
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        let key = seed_normalizer::track_key(&play.artist, &play.title);
        let duplicate = user.plays.iter().any(|existing| {
            seed_normalizer::track_key(&existing.artist, &existing.title) == key
                && total_minutes(existing.played_at_utc - play.played_at_utc).abs()
                    <= total_minutes(DUPLICATE_WINDOW)
        });
        if duplicate {
            return false;
        }

        play.artist = seed_normalizer::artist(&play.artist);
        play.title = seed_normalizer::title(&play.title);
        user.plays.insert(0, play);
        user.new_plays_since_refresh += 1;
        user.last_seen_utc = now;
        self.prune_locked(state, now);
        self.save_locked(state);
        true
    }

    /// A copy of the listener's state, or an empty one named `username`.
    pub fn get_user(&self, username: &str) -> LastFmRadioUserState {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        match state.get(&user_key(username)) {
            Some(user) => user.clone(),
            None => LastFmRadioUserState {
                username: username.to_string(),
                ..Default::default()
            },
        }
    }

    pub fn mark_heart(&self, username: &str, song_id: &str, artist: &str, title: &str) -> bool {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let Some(user) = state.get_mut(&user_key(username)) else {
            return false;
        };
        let key = seed_normalizer::track_key(artist, title);
        let Some(play) = user.plays.iter_mut().find(|item| {
            item.song_id == song_id || seed_normalizer::track_key(&item.artist, &item.title) == key
        }) else {
            return false;
        };
        if play.hearted {
            return false;
        }
        play.hearted = true;
        self.save_locked(state);
        true
    }

    pub fn get_summaries(&self) -> Vec<LastFmRadioUserSummary> {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let mut users: Vec<&LastFmRadioUserState> = state.users.values().map(|(_, user)| user).collect();
        users.sort_by_key(|a| std::cmp::Reverse(a.last_seen_utc));
        users
            .into_iter()
            .map(|user| LastFmRadioUserSummary {
                username: user.username.clone(),
                play_count: user.plays.len() as i32,
                station_count: user.stations.len() as i32,
                new_plays_since_refresh: user.new_plays_since_refresh,
                last_refresh_success_utc: user.last_refresh_success_utc,
                last_refresh_error: user.last_refresh_error.clone(),
                refreshing: user.refreshing,
            })
            .collect()
    }

    pub fn known_users(&self) -> Vec<String> {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        state
            .users
            .values()
            .map(|(_, user)| user.username.clone())
            .collect()
    }

    pub fn find_station(&self, username: &str, station_id: &str) -> Option<LastFmRadioStation> {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        state
            .get(&user_key(username))?
            .stations
            .iter()
            .find(|item| item.id == station_id)
            .cloned()
    }

    /// Removes an unplayable song from every current station and prevents an immediate
    /// deterministic refresh from selecting it again.
    pub fn reject_track(
        &self,
        username: &str,
        track: &LastFmRadioTrack,
        now_utc: Option<DateTime<Utc>>,
    ) -> usize {
        if dotnet::is_blank(username) || dotnet::is_blank(&track.artist) || dotnet::is_blank(&track.title) {
            return 0;
        }
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = now_utc.unwrap_or_else(|| self.clock.now());
        let user = get_or_create(state, username, self.clock.now());
        let key = seed_normalizer::track_key(&track.artist, &track.title);
        let mut removed = 0;
        for station in &mut user.stations {
            let before = station.tracks.len();
            station
                .tracks
                .retain(|candidate| seed_normalizer::track_key(&candidate.artist, &candidate.title) != key);
            removed += before - station.tracks.len();
            if before != station.tracks.len() {
                station.changed_utc = now;
            }
        }
        let mut unavailable: Vec<LastFmRadioUnavailableTrack> =
            std::iter::once(LastFmRadioUnavailableTrack {
                key: key.clone(),
                artist: track.artist.clone(),
                title: track.title.clone(),
                failed_at_utc: now,
                retry_after_utc: now + UNAVAILABLE_TRACK_COOLDOWN,
            })
            .chain(
                std::mem::take(&mut user.unavailable_tracks)
                    .into_iter()
                    .filter(|item| item.retry_after_utc > now && item.key != key),
            )
            .collect();
        unavailable.sort_by_key(|a| std::cmp::Reverse(a.failed_at_utc));
        unavailable.truncate(MAX_UNAVAILABLE_TRACKS_PER_USER);
        user.unavailable_tracks = unavailable;
        user.last_seen_utc = now;
        self.save_locked(state);
        removed
    }

    /// A song rated one star never plays on this user's radio again and leaves the stations it is
    /// on now; any other rating lifts that. Returns whether anything changed.
    pub fn set_radio_ban(
        &self,
        username: &str,
        song_id: &str,
        artist: &str,
        title: &str,
        banned: bool,
    ) -> bool {
        if dotnet::is_blank(username) || dotnet::is_blank(artist) || dotnet::is_blank(title) {
            return false;
        }
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        if !banned && state.get(&user_key(username)).is_none() {
            return false;
        }
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        let key = seed_normalizer::track_key(artist, title);
        let before = user.radio_bans.len();
        user.radio_bans
            .retain(|ban| !(ban.key == key || (!song_id.is_empty() && ban.song_id == song_id)));
        let had = user.radio_bans.len() != before;
        if banned {
            user.radio_bans.insert(
                0,
                LastFmRadioBan {
                    key: key.clone(),
                    song_id: song_id.to_string(),
                    artist: artist.to_string(),
                    title: title.to_string(),
                    banned_at_utc: now,
                },
            );
            for station in &mut user.stations {
                let before = station.tracks.len();
                station
                    .tracks
                    .retain(|track| seed_normalizer::track_key(&track.artist, &track.title) != key);
                if before != station.tracks.len() {
                    station.changed_utc = now;
                }
            }
        } else if !had {
            return false;
        }
        self.save_locked(state);
        true
    }

    /// The keys of the songs this user rated one star, without copying the rest of their state.
    pub fn radio_ban_keys(&self, username: &str) -> IgnoreCaseKeys {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        state
            .get(&user_key(username))
            .map(|user| user.radio_bans.iter().map(|ban| ban.key.as_str()).collect())
            .unwrap_or_default()
    }

    pub fn replace_stations(&self, username: &str, stations: &[LastFmRadioStation]) {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        let existing = std::mem::take(&mut user.stations);
        let prior_of = |id: &str| existing.iter().find(|station| station.id == id);
        user.stations = stations
            .iter()
            .map(|source| {
                let mut station = source.clone();
                if let Some(prior) = prior_of(&station.id) {
                    station.created_utc = prior.created_utc;
                    if same_snapshot(prior, &station) {
                        station.changed_utc = prior.changed_utc;
                    }
                }
                station
            })
            .collect();
        user.new_plays_since_refresh = 0;
        user.last_refresh_success_utc = Some(now);
        user.last_refresh_attempt_utc = user.last_refresh_success_utc;
        user.last_refresh_error = None;
        user.refreshing = false;
        user.last_seen_utc = now;
        rehydrate_routes(&self.registry, user);
        self.save_locked(state);
    }

    pub fn mark_refreshing(&self, username: &str) {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        user.refreshing = true;
        user.last_refresh_attempt_utc = Some(now);
        user.last_refresh_error = None;
        self.save_locked(state);
    }

    pub fn mark_refresh_failed(&self, username: &str, message: &str) {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let now = self.clock.now();
        let user = get_or_create(state, username, now);
        user.refreshing = false;
        user.last_refresh_attempt_utc = Some(now);
        user.last_refresh_error = Some(first_utf16_units(message, 500));
        self.save_locked(state);
    }

    pub fn reset(&self, username: &str) -> bool {
        let mut guard = self.state.lock();
        let state = self.load_locked(&mut guard);
        let removed = state
            .users
            .remove(&dotnet::ordinal_ignore_case_key(&user_key(username)))
            .is_some();
        if removed {
            self.save_locked(state);
        }
        removed
    }

    fn load_locked<'a>(&self, guard: &'a mut Option<Document>) -> &'a mut Document {
        if guard.is_none() {
            *guard = Some(self.load());
        }
        guard.as_mut().expect("loaded above")
    }

    fn load(&self) -> Document {
        let text = match self.path.is_file() {
            false => return Document::empty(),
            true => state_file::read_all_text(&self.path),
        };
        let parsed = text.map_err(|e| e.to_string()).and_then(|text| {
            serde_json::from_str::<LastFmRadioStateDocument>(&text).map_err(|e| e.to_string())
        });
        let mut document = match parsed {
            Ok(document) => document,
            Err(message) => {
                warn!("Last.fm radio state load failed ({message}); starting clean");
                return Document::empty();
            }
        };
        if document.version != CURRENT_VERSION {
            warn!(
                "Unsupported Last.fm radio state version {}; starting clean",
                document.version
            );
            document = LastFmRadioStateDocument::default();
        }
        // Keyed ignoring case: two keys that differ only in case fail the load, and it starts
        // clean.
        let mut state = Document {
            version: document.version,
            users: DotnetDictionary::new(),
        };
        for (key, user) in document.users {
            let folded = dotnet::ordinal_ignore_case_key(&key);
            if state.users.contains_key(&folded) {
                warn!(
                    "Last.fm radio state load failed (An item with the same key has already been added. Key: {key}); starting clean"
                );
                return Document::empty();
            }
            state.users.set(folded, (key, user));
        }
        self.prune_locked(&mut state, self.clock.now());
        for (_, user) in state.users.values_mut() {
            rehydrate_routes(&self.registry, user);
        }
        state
    }

    fn prune_locked(&self, state: &mut Document, now: DateTime<Utc>) {
        let retention = self.settings.current().last_fm.effective_history_retention_days();
        let cutoff = now - TimeDelta::days(i64::from(retention));
        for (_, user) in state.users.values_mut() {
            let mut plays: Vec<LastFmRadioPlay> = std::mem::take(&mut user.plays)
                .into_iter()
                .filter(|play| play.played_at_utc >= cutoff)
                .collect();
            plays.sort_by_key(|a| std::cmp::Reverse(a.played_at_utc));
            plays.truncate(MAX_PLAYS_PER_USER);
            user.plays = plays;
            let mut unavailable: Vec<LastFmRadioUnavailableTrack> =
                std::mem::take(&mut user.unavailable_tracks)
                    .into_iter()
                    .filter(|track| track.retry_after_utc > now)
                    .collect();
            unavailable.sort_by_key(|a| std::cmp::Reverse(a.failed_at_utc));
            unavailable.truncate(MAX_UNAVAILABLE_TRACKS_PER_USER);
            user.unavailable_tracks = unavailable;
            user.radio_bans
                .sort_by_key(|a| std::cmp::Reverse(a.banned_at_utc));
            user.radio_bans.truncate(MAX_RADIO_BANS_PER_USER);
        }

        let mut by_last_seen: Vec<(String, DateTime<Utc>)> = state
            .users
            .iter()
            .map(|(key, (_, user))| (key.clone(), user.last_seen_utc))
            .collect();
        by_last_seen.sort_by_key(|a| std::cmp::Reverse(a.1));
        for (key, _) in by_last_seen.into_iter().skip(MAX_USERS) {
            state.users.remove(&key);
        }
    }

    fn save_locked(&self, state: &Document) {
        let json = octo_core::json::to_string_indented(state);
        if let Err(error) = state_file::save_atomic(&self.path, &json) {
            warn!("Last.fm radio state save failed: {error}");
        }
    }
}

fn get_or_create<'a>(
    state: &'a mut Document,
    username: &str,
    now: DateTime<Utc>,
) -> &'a mut LastFmRadioUserState {
    let key = user_key(username);
    let folded = dotnet::ordinal_ignore_case_key(&key);
    if !state.users.contains_key(&folded) {
        let user = LastFmRadioUserState {
            username: username.trim().to_string(),
            last_seen_utc: now,
            ..Default::default()
        };
        state.users.set(folded.clone(), (key, user));
    }
    state
        .users
        .get_mut(&folded)
        .map(|(_, user)| user)
        .expect("added above")
}

/// After a load and after every install, the external tracks are registered again so their
/// short ids resolve: the registry may have forgotten them.
fn rehydrate_routes(registry: &ExternalIdRegistry, user: &mut LastFmRadioUserState) {
    for track in user
        .stations
        .iter_mut()
        .flat_map(|station| station.tracks.iter_mut())
        .filter(|track| !track.is_local)
    {
        track.resolved_id = Some(registry.register(OutsideRouting {
            kind: RoutingKind::Song,
            artist: Some(track.artist.clone()),
            title: Some(track.title.clone()),
            album: track.album.clone(),
            duration: track.duration,
            you_tube_id: track.you_tube_id.clone(),
            ..Default::default()
        }));
        if track.external_provider.is_none() {
            track.external_provider = Some(OutsideMetadataService::PROVIDER_NAME.to_string());
        }
    }
}

fn same_snapshot(left: &LastFmRadioStation, right: &LastFmRadioStation) -> bool {
    left.definition_version == right.definition_version
        && left.name == right.name
        && left
            .tracks
            .iter()
            .map(|track| seed_normalizer::track_key(&track.artist, &track.title))
            .eq(right
                .tracks
                .iter()
                .map(|track| seed_normalizer::track_key(&track.artist, &track.title)))
}

/// The span in fractional minutes.
fn total_minutes(span: TimeDelta) -> f64 {
    span.num_microseconds().map_or_else(
        || span.num_seconds() as f64 / 60.0,
        |micros| micros as f64 / 60_000_000.0,
    )
}

/// `message[..500]` when longer: the first 500 UTF-16 code units.
fn first_utf16_units(message: &str, units: usize) -> String {
    if dotnet::utf16_len(message) <= units {
        return message.to_string();
    }
    let utf16: Vec<u16> = message.encode_utf16().take(units).collect();
    String::from_utf16_lossy(&utf16)
}

#[cfg(test)]
#[path = "last_fm_radio_state_store_tests.rs"]
mod tests;
