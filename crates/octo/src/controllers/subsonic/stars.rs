//! `star`, `setRating`, `scrobble` and `reportPlayback`.

use axum::extract::{Request, State};
use axum::response::IntoResponse;
use axum::routing::get;
use chrono::{DateTime, Utc};
use octo_core::common::playlist_id_helper;
use octo_core::last_fm::last_fm_radio_refresh_policy;
use octo_core::last_fm::last_fm_scrobble_service::LastFmTrack;
use octo_core::models::radio::LastFmRadioPlay;
use octo_core::outside::routing::RoutingKind;
use octo_core::settings::LibraryRatingScope;
use tracing::{debug, info};

use super::caller_checks::{
    SubsonicCall, favorite_credential, file, is_successful_subsonic_response, is_true,
    refuse_unless_signed_in, requester_for, signed_in_user,
};
use crate::app::AppState;
use crate::http::error::{AppError, AppResult};
use crate::http::routes::RouteSet;
use crate::services::common::AcquisitionTarget;
use crate::services::family::{HeartOutcome, SavedAlbum, SavedSong, family_caller};
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::last_fm::LastFmRadioTrackResolver;
use crate::services::library::RatingActionRequest;
use crate::services::outside::OutsideMetadataService;
use crate::services::subsonic::RelayError;
use octo_core::family::abilities::RequestQuality;
use octo_core::family::request::RequestKind;

/// How many outside songs after a scrobbled one have their stream readied to play at once.
const NEXT_STREAMS_WARMED: usize = 3;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic("star", get(star).post(star))
        .subsonic("unstar", get(unstar).post(unstar))
        .subsonic("setRating", get(set_rating).post(set_rating))
        .subsonic("scrobble", get(scrobble).post(scrobble))
        .subsonic("reportPlayback", get(report_playback).post(report_playback))
}

/// The radio's resolver over this request's proxy, so it is built per request.
fn resolver(state: &AppState, call: &SubsonicCall) -> LastFmRadioTrackResolver {
    LastFmRadioTrackResolver::new(
        call.proxy.clone(),
        state.music_metadata.clone(),
        state.external_id_registry.clone(),
    )
}

/// Stars (favorites) an item. For playlists, triggers download. For external songs and
/// albums, triggers a download and, outside Octo's own apps, favorites it once it arrives.
pub async fn star(State(state): State<AppState>, req: Request) -> AppResult {
    // Who the family gate found behind this request, read before the request is taken apart.
    let caller = family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = call.format();
    let builder = &state.subsonic_response_builder;

    let item_id = call.param("id").to_string();
    // What a heart on an outside song or album does for this person: a download into the
    // shared library (everyone outside the family, as always), into a member's own library, or
    // a saved link.
    let heart = state.family_write_gate.heart(caller.as_ref());

    // Check if this is a playlist
    if !item_id.is_empty() && playlist_id_helper::is_external_playlist(Some(&item_id)) {
        // Starring an outside playlist is not supported.
        return Ok(builder
            .create_error(&format, 0, "Playlist functionality is not enabled")
            .into_response());
    }

    let sources = state
        .settings
        .current()
        .subsonic
        .effective_heart_download_sources();

    // Starring a whole album. Subsonic sends the album under albumId, though some
    // clients reuse id, so accept either.
    //
    // Two lookups with different jobs: the REGISTRY decides whether this is an album,
    // because ParseSongId reports every registry id as a "song" and would otherwise
    // send an entire album down the single-track download path. ParseSongId still
    // supplies the provider string.
    let album_candidate = if !item_id.is_empty() {
        item_id.clone()
    } else {
        call.param("albumId").to_string()
    };
    if !album_candidate.is_empty()
        && state
            .external_id_registry
            .lookup(&album_candidate)
            .is_some_and(|routing| routing.snapshot().kind == RoutingKind::Album)
    {
        // Navidrome never sees this id, so nothing else would check who is asking.
        if let Some(refused) = refuse_unless_signed_in(&state, &call, &format).await? {
            return Ok(refused);
        }

        if let HeartOutcome::Save { user, request } = &heart {
            save_album(&state, user, &album_candidate).await;
            if let Some(full) = request {
                ask_for_copy(&state, user, &album_candidate, RequestKind::Album, full).await;
            }
            return Ok(builder.create_response(&format, "starred").into_response());
        }

        if !sources.iter().any(|step| step.album_enabled == Some(true)) {
            info!("Starred album {album_candidate} but no album-heart source is enabled; ignoring");
            return Ok(builder.create_response(&format, "starred").into_response());
        }

        // No storage-mode gate here, unlike the song branch. That gate exists because
        // Permanent mode already downloads a song when it is played; an album star is
        // a request for tracks the user has NOT played, so it must work in every mode.
        let album_provider = state
            .local_library
            .parse_song_id(&album_candidate)
            .1
            .unwrap_or_else(|| OutsideMetadataService::PROVIDER_NAME.to_string());

        info!("Starring external album {album_candidate}, triggering full album download");

        let metadata = state.music_metadata.clone();
        let (provider, album_id) = (album_provider.clone(), album_candidate.clone());
        tokio::spawn(async move {
            // Log the size up front: downloads are serialized, so a large album is
            // a multi-hour job and the user should be able to see what they started.
            let album = metadata.get_album(&provider, &album_id).await;
            info!(
                "Album star: '{}' by {} has {} track(s); downloads run one at a time",
                album.as_ref().map_or("", |a| a.title.as_str()),
                album.as_ref().map_or("", |a| a.artist.as_str()),
                album.as_ref().map_or(0, |a| a.songs.len())
            );
        });

        // An empty exclude means "download every track". The engine already skips
        // tracks that are downloaded or in flight and isolates per-track failures.
        //
        // The progress list is claimed first, so the chain's first step already has a row
        // to move. Its name is the one the request signed in as (for an API key, its owner
        // as Navidrome names it), not RequesterFor: it decides who may see the row, and it
        // is never written anywhere.
        let who = signed_in_user(&state, &call).await?;
        let target = match &heart {
            HeartOutcome::Download(target) => target.clone(),
            HeartOutcome::Save { .. } => AcquisitionTarget::Shared,
        };
        // Held before the download is queued, so one that finishes at once still finds it. A
        // member's album songs are starred for them one by one where they land.
        if target == AcquisitionTarget::Shared
            && let Some(credential) = favorite_credential(&call)
        {
            state
                .star_on_arrival
                .hold_album(&album_provider, &album_candidate, &credential, Some(&who));
        }
        state
            .acquisition_tracker
            .begin_album(&album_provider, &album_candidate, Some(&who));
        state.heart_acquisition_coordinator.queue_album_for(
            &album_provider,
            &album_candidate,
            requester_for(&state, Some(&who)).as_deref(),
            target,
        );

        // Navidrome has never seen this id, so relaying the star would just error.
        return Ok(builder.create_response(&format, "starred").into_response());
    }

    // Check if this is an external song (enables download-on-star)
    let (is_external, provider, external_id) = state.local_library.parse_song_id(&item_id);

    // A saved link needs no download source.
    let saves = matches!(heart, HeartOutcome::Save { .. });
    if is_external && (saves || sources.iter().any(|step| step.song_enabled == Some(true))) {
        // Navidrome never sees this id, so nothing else would check who is asking.
        if let Some(refused) = refuse_unless_signed_in(&state, &call, &format).await? {
            return Ok(refused);
        }
        let provider = provider.unwrap_or_default();
        let external_id = external_id.unwrap_or_default();

        if let HeartOutcome::Save { user, request } = &heart {
            save_song(&state, user, &item_id, &provider, &external_id).await;
            if let Some(full) = request {
                ask_for_copy(&state, user, &item_id, RequestKind::Song, full).await;
            }
            return Ok(builder.create_response(&format, "starred").into_response());
        }
        let target = match heart {
            HeartOutcome::Download(target) => target,
            HeartOutcome::Save { .. } => AcquisitionTarget::Shared,
        };

        // No storage-mode gate any more. It used to exclude Permanent on the grounds
        // that playing a track there already downloads it, but that was only ever true
        // through the blocking play path: so in Permanent mode a star fell through to
        // the relay and errored on an id Navidrome has never seen.
        //
        // Stars ride the queue's own channel: explicit user intent is never shed under
        // load the way a play is, and the request carries the album-walk flag rather
        // than inheriting whatever a concurrent play happened to ask for.
        info!("Starring external song {item_id}, queueing permanent download");

        // Keyed by what the pipeline knows, labeled with the id the client starred so the
        // app can find its row. Named from the routing, which is already in memory.
        let who = signed_in_user(&state, &call).await?;
        // Held before the download is queued, so one that finishes at once still finds it.
        if let Some(credential) = favorite_credential(&call) {
            state
                .star_on_arrival
                .hold_song(&provider, &external_id, &credential, Some(&who));
        }
        let routing = state
            .external_id_registry
            .lookup(&external_id)
            .map(|r| r.snapshot());
        state.acquisition_tracker.begin(
            &provider,
            &external_id,
            Some(&item_id),
            Some(&who),
            routing.as_ref().and_then(|r| r.artist.as_deref()),
            routing.as_ref().and_then(|r| r.title.as_deref()),
            routing.as_ref().and_then(|r| r.album.as_deref()),
        );
        state.heart_acquisition_coordinator.queue_track_for(
            &provider,
            &external_id,
            requester_for(&state, Some(&who)).as_deref(),
            target,
        );

        // Return success response immediately
        return Ok(builder.create_response(&format, "starred").into_response());
    }

    // For non-external items or when download-on-star is disabled, relay to real Subsonic server
    match call.proxy.relay("rest/star", &call.parameters).await {
        Ok(result) => {
            let username = if is_successful_subsonic_response(&result.body, &format) {
                listener(&state, &call).await
            } else {
                None
            };
            if let Some(username) = username.as_deref() {
                state.generated_playlists.forget_marks(username);
            }
            if let Some(username) = username.as_deref()
                && !item_id.is_empty()
                && let Some(song) = resolver(&state, &call)
                    .resolve_scrobble(&item_id, &call.parameters)
                    .await
            {
                state
                    .last_fm_radio_state
                    .mark_heart(username, &item_id, &song.artist, &song.title);
            }
            let content_type = result
                .content_type
                .unwrap_or_else(|| format!("application/{format}"));
            Ok(file(result.body, &content_type))
        }
        Err(RelayError::Http(message)) => Ok(builder
            .create_error(
                &format,
                0,
                &format!("Error connecting to Subsonic server: {message}"),
            )
            .into_response()),
        Err(other) => Err(other.into()),
    }
}

/// Saves an outside song for a family member as a link (it plays from its source and shows in
/// their starred lists).
async fn save_song(state: &AppState, user: &str, item_id: &str, provider: &str, external_id: &str) {
    let song = state.metadata_service.get_song(provider, external_id).await;
    let routing = state
        .external_id_registry
        .lookup(external_id)
        .map(|r| r.snapshot());
    let text = |from_song: Option<&str>, from_routing: Option<&str>| {
        from_song
            .filter(|t| !t.is_empty())
            .or(from_routing)
            .unwrap_or_default()
            .to_string()
    };
    let saved = SavedSong {
        id: item_id.to_string(),
        title: text(
            song.as_ref().map(|s| s.title.as_str()),
            routing.as_ref().and_then(|r| r.title.as_deref()),
        ),
        artist: text(
            song.as_ref().map(|s| s.artist.as_str()),
            routing.as_ref().and_then(|r| r.artist.as_deref()),
        ),
        album: text(
            song.as_ref().map(|s| s.album.as_str()),
            routing.as_ref().and_then(|r| r.album.as_deref()),
        ),
        cover_art: item_id.to_string(),
        duration: song
            .as_ref()
            .and_then(|s| s.duration)
            .or_else(|| routing.as_ref().and_then(|r| r.duration))
            .map_or(0, i64::from),
        explicit: song.as_ref().and_then(|s| s.explicit_content_lyrics).unwrap_or(0),
        saved: Utc::now(),
    };
    if state.family_saved.add(user, saved) {
        info!("FAMILY {user} saved {item_id}");
    }
}

/// Saves an outside album for a family member as a link.
async fn save_album(state: &AppState, user: &str, album_id: &str) {
    let provider = state
        .local_library
        .parse_song_id(album_id)
        .1
        .unwrap_or_else(|| OutsideMetadataService::PROVIDER_NAME.to_string());
    let album = state.metadata_service.get_album(&provider, album_id).await;
    let saved = SavedAlbum {
        id: album_id.to_string(),
        name: album.as_ref().map(|a| a.title.clone()).unwrap_or_default(),
        artist: album.as_ref().map(|a| a.artist.clone()).unwrap_or_default(),
        cover_art: album_id.to_string(),
        song_count: album.as_ref().map_or(0, |a| a.songs.len() as i64),
        year: album.as_ref().and_then(|a| a.year),
        saved: Utc::now(),
    };
    if state.family_saved.add_album(user, saved) {
        info!("FAMILY {user} saved album {album_id}");
    }
}

/// A family member whose library is full asks for a copy instead of downloading one.
async fn ask_for_copy(state: &AppState, user: &str, id: &str, kind: RequestKind, why: &str) {
    match state
        .family_request_service
        .create(user, id, RequestQuality::Best, Some(kind))
        .await
    {
        Ok(request) => info!("FAMILY {why}: {user} asked for a copy of {id} ({})", request.id),
        Err(e) => info!("FAMILY {user} could not ask for a copy of {id}: {}", e.message),
    }
}

/// `unstar`: a family member's saved outside song or album is forgotten. Everything else goes
/// where it always went.
pub async fn unstar(State(state): State<AppState>, req: Request) -> axum::response::Response {
    let member = family_caller(req.extensions())
        .filter(|caller| caller.managed)
        .map(|caller| caller.user.clone());
    let Some(user) = member else {
        return crate::http::catch_all::catch_all(State(state), req).await;
    };
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format();
    let ids: Vec<String> = ["id", "albumId"]
        .iter()
        .map(|key| call.param(key).trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    let outside: Vec<&String> = ids
        .iter()
        .filter(|id| state.family_saved.is_saved(&user, id) || state.local_library.parse_song_id(id).0)
        .collect();
    if outside.is_empty() {
        return match call.proxy.relay("rest/unstar", &call.parameters).await {
            Ok(result) => {
                let content_type = result
                    .content_type
                    .unwrap_or_else(|| format!("application/{format}"));
                file(result.body, &content_type)
            }
            Err(e) => state
                .subsonic_response_builder
                .create_error(&format, 0, &format!("Error connecting to Subsonic server: {e}"))
                .into_response(),
        };
    }
    for id in outside {
        if state.family_saved.remove(&user, id) {
            info!("FAMILY {user} unsaved {id}");
        }
    }
    state
        .subsonic_response_builder
        .create_response(&format, "unstar")
        .into_response()
}

/// Star ratings.
///
/// Two jobs, in this order and never the other way round:
///
/// 1. ALWAYS relay to Navidrome and return ITS response verbatim. A rating the server did
///    not record snaps back in the client on the next refresh, and rating a track is a
///    legitimate thing to do for its own sake. Octo reading meaning into it must never cost
///    the user the rating itself.
/// 2. Only then, if library actions are on, the rating maps to an enabled action and the
///    user is on the allowlist, queue it. Queue, not execute: nothing that removes a file
///    runs inside a request.
///
/// Before this there was no handler at all and setRating fell through to the catch-all,
/// where an Octo id became a synthetic OK that never reached Navidrome. That behavior is
/// preserved exactly.
pub async fn set_rating(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = call.format();
    let item_id = call.param("id").to_string();
    let rating_text = call.param("rating").to_string();
    let builder = &state.subsonic_response_builder;

    // An external id is not a Navidrome song, and relaying one errors "data not found".
    if !item_id.is_empty() && state.local_library.parse_song_id(&item_id).0 {
        // Nothing to relay, so a ping is the auth check before the listener's radio changes.
        if let Some(external_stars) = try_parse_int(&rating_text)
            && let Some(external_rater) = call.param_opt("u").filter(|u| !u.is_empty())
        {
            let mut auth = call.parameters.clone();
            auth.shift_remove("id");
            auth.shift_remove("rating");
            if call
                .proxy
                .relay_safe("rest/ping", &auth)
                .await
                .is_some_and(|check| is_successful_subsonic_response(&check.body, &format))
            {
                record_radio_ban(&state, &call, external_rater, &item_id, external_stars).await;
            }
        }
        return Ok(builder.create_response(&format, "setRating").into_response());
    }

    let result = match call.proxy.relay("rest/setRating", &call.parameters).await {
        Ok(result) => result,
        Err(RelayError::Http(message)) => {
            return Ok(builder
                .create_error(
                    &format,
                    0,
                    &format!("Error connecting to Subsonic server: {message}"),
                )
                .into_response());
        }
        Err(other) => return Err(other.into()),
    };

    if is_successful_subsonic_response(&result.body, &format)
        && let Some(stars) = try_parse_int(&rating_text)
        && !item_id.is_empty()
        && let Some(rater) = listener(&state, &call).await
    {
        state
            .generated_playlists
            .rated(&rater, &item_id, i64::from(stars));
        record_radio_ban(&state, &call, &rater, &item_id, stars).await;
    }

    let settings = state.settings.current().library_actions.clone();
    if is_successful_subsonic_response(&result.body, &format)
        && let Some(rating) = try_parse_int(&rating_text)
        && !item_id.is_empty()
        && let Some(username) = call.param_opt("u").filter(|v| !v.is_empty())
        && let Some(token) = call.param_opt("t").filter(|v| !v.is_empty())
        && let Some(salt) = call.param_opt("s").filter(|v| !v.is_empty())
        && let Some(definition) = settings.action_for_rating(rating)
        && rating_in_scope(&state, username, &item_id)
    {
        // Navidrome answered ok to a call carrying this u/t/s, which IS the auth check.
        // Octo does not validate the password itself; it trusts it exactly as far as
        // Navidrome just did, the same idiom getPlaylist and star already use.
        state
            .library_action_rating_worker
            .try_enqueue(RatingActionRequest {
                action: definition.action,
                navidrome_id: item_id.clone(),
                username: username.to_string(),
                auth_user: username.to_string(),
                auth_token: token.to_string(),
                auth_salt: salt.to_string(),
            });
    }

    let content_type = result
        .content_type
        .unwrap_or_else(|| format!("application/{format}"));
    Ok(file(result.body, &content_type))
}

/// The listener a request Navidrome accepted speaks for, named the way every request's identity
/// is: `u`, or for an API key sign-in its owner as Navidrome names it. None when there is nobody
/// to name or Navidrome could not be asked.
async fn listener(state: &AppState, call: &SubsonicCall) -> Option<String> {
    state
        .request_identity
        .username(&call.parameters, &call.proxy)
        .await
        .ok()
        .flatten()
        .filter(|u| !u.is_empty())
}

/// One star keeps a song off this listener's radio; any other rating lifts that. Never fails the
/// rating itself: the radio is a side effect of it.
async fn record_radio_ban(state: &AppState, call: &SubsonicCall, username: &str, item_id: &str, stars: i32) {
    let resolver = resolver(state, call);
    if let Some(rated) = resolver.resolve_scrobble(item_id, &call.parameters).await {
        state
            .last_fm_radio_state
            .set_radio_ban(username, item_id, &rated.artist, &rated.title, stars == 1);
    }
    if stars == 1 {
        state.radio_outcomes.disliked(username, item_id, Utc::now());
    }
}

/// Whether a star is a command here. NoticeOnly: only on a track in one of Octo's notice
/// playlists, where the only reason to rate it is to answer (#47). Global: any track.
fn rating_in_scope(state: &AppState, username: &str, item_id: &str) -> bool {
    state.settings.current().library_actions.effective_ratings_scope() == LibraryRatingScope::Global
        || state.notice_queue.is_queued(username, item_id)
}

/// An integer parsed from text: surrounding white space and a sign are allowed.
fn try_parse_int(text: &str) -> Option<i32> {
    text.trim().parse::<i32>().ok()
}

/// Scrobbles a play. Plays of external songs go to Last.fm (when configured) and the radio's
/// listening profile; once Navidrome accepts the caller, the first id also prewarms the next
/// songs of a queue.
///
/// Library songs are relayed to Navidrome too, because real scrobbling
/// (last-played stats, the Now Playing panel) is the upstream's job. Outside
/// songs are not: Navidrome has no such media to record.
pub async fn scrobble(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let mut ids = call.incoming.parameter_values("id");
    let submissions = call.incoming.parameter_values("submission");
    let mut times = call.incoming.parameter_values("time");
    if ids.is_empty()
        && let Some(single_id) = call.param_opt("id").filter(|id| !id.is_empty())
    {
        ids = vec![single_id.to_string()];
    }
    let id = ids.first().cloned().unwrap_or_default();
    let format = call.format();

    // The next songs of a radio queue start resolving only once Navidrome has accepted the
    // caller: an unchecked request never spends YouTube lookups.
    let prewarm_queue = || {
        if id.is_empty() {
            return;
        }
        let upcoming = state.radio_queues.get_upcoming_from(&id, 16);
        if !upcoming.is_empty() {
            debug!("scrobble {id}: prewarming next {} from queue", upcoming.len());
            let metadata = state.music_metadata.clone();
            // The next three outside songs first, with their streams resolved, so whichever
            // plays next starts from the shim's cache; then videos for the rest.
            tokio::spawn(async move {
                let readied = metadata
                    .prewarm_next_streams(&upcoming, NEXT_STREAMS_WARMED)
                    .await;
                let rest: Vec<String> = upcoming
                    .into_iter()
                    .filter(|song| !readied.contains(song))
                    .collect();
                metadata.prewarm_you_tube_ids_for_song_ids(&rest, 8).await;
            });
        }
    };

    // Library songs pass through so Navidrome's last-played/Now Playing stays accurate.
    // An outside id is not Navidrome's: relayed, it logs "data not found" on every play
    // and records nothing (#60). So only library ids go upstream, each keeping its
    // own submission and time when the client sent one per id.
    let library: Vec<usize> = (0..ids.len())
        .filter(|index| !state.local_library.parse_song_id(&ids[*index]).0)
        .collect();
    // Times that do not pair up with the ids are dropped, not relayed. Navidrome refuses
    // such a scrobble outright, and leaving the outside ids out could make the counts
    // match by accident and pin an outside song's time on a library song.
    if !times.is_empty() && times.len() != ids.len() {
        times = Vec::new();
    }
    let library_only = |values: &[String]| -> Vec<String> {
        if values.len() == ids.len() {
            library.iter().map(|index| values[*index].clone()).collect()
        } else {
            values.to_vec()
        }
    };

    let mut relay_parameters: Vec<(String, String)> = call
        .parameters
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "id" | "submission" | "time"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let outcome: Result<axum::response::Response, RelayError> = async {
        if !ids.is_empty() && library.is_empty() {
            // Nothing for Navidrome to record, but its answer was also the credential
            // check that gates learning below. A ping checks the same credentials and
            // answers in the very shape a scrobble does, failures included.
            let check = call
                .proxy
                .relay("rest/ping", relay_parameters.iter().map(|(k, v)| (k, v)))
                .await?;
            if is_successful_subsonic_response(&check.body, &format) {
                prewarm_queue();
                learn_from_scrobbles(&state, &call, &ids, &submissions, &times).await?;
            }
            let content_type = check
                .content_type
                .unwrap_or_else(|| format!("application/{format}"));
            return Ok(file(check.body, &content_type));
        }
        relay_parameters.extend(library_only(&ids).into_iter().map(|v| ("id".to_string(), v)));
        relay_parameters.extend(
            library_only(&submissions)
                .into_iter()
                .map(|v| ("submission".to_string(), v)),
        );
        relay_parameters.extend(library_only(&times).into_iter().map(|v| ("time".to_string(), v)));
        let result = call
            .proxy
            .relay("rest/scrobble", relay_parameters.iter().map(|(k, v)| (k, v)))
            .await?;
        if is_successful_subsonic_response(&result.body, &format) {
            prewarm_queue();
            learn_from_scrobbles(&state, &call, &ids, &submissions, &times).await?;
        }
        let content_type = result
            .content_type
            .unwrap_or_else(|| format!("application/{format}"));
        Ok(file(result.body, &content_type))
    }
    .await;

    match outcome {
        Ok(response) => Ok(response),
        // Even if upstream is briefly unhappy, return 200 so the client
        // doesn't think scrobble is broken.
        Err(RelayError::Http(_)) => Ok(state
            .subsonic_response_builder
            .create_response(&format, "scrobble")
            .into_response()),
        Err(other) => Err(AppError::from(other)),
    }
}

/// What a completed scrobble teaches: the radio profile (when personalised radio is
/// on), the listener's Last.fm history (outside songs, and library songs unless they are
/// left to Navidrome) and, for an external track, ListenBrainz. A start-of-play event
/// becomes Last.fm's Now Playing.
async fn learn_from_scrobbles(
    state: &AppState,
    call: &SubsonicCall,
    ids: &[String],
    submissions: &[String],
    times: &[String],
) -> Result<(), RelayError> {
    // Called only once Navidrome has accepted these credentials. An API key sign-in has no
    // u, so its owner is asked of Navidrome; nobody to name means nothing is learned.
    let Some(username) = state
        .request_identity
        .username(&call.parameters, &call.proxy)
        .await?
        .filter(|u| !u.is_empty())
    else {
        return Ok(());
    };
    // What radio learns: a song it suggested, started and then played through, or started and
    // never finished. Before the early return below, so it works with stations off.
    state
        .radio_outcomes
        .observe(&username, ids, submissions, Utc::now());
    let last_fm_settings = state.settings.current().last_fm.clone();
    let learning = last_fm_settings.enable_radio && last_fm_settings.enable_personalized_stations;
    let submitting = state.listen_brainz.is_enabled_for(&username);
    let scrobbling = state.last_fm_scrobbles.is_enabled_for(&username);
    if !learning && !submitting && !scrobbling {
        return Ok(());
    }
    let resolver = resolver(state, call);
    let mut recorded = false;
    for (index, song_id) in ids.iter().enumerate() {
        // Explicit start/now-playing scrobbles are not completed plays. Clients that
        // omit submission are accepted because many only send one credible event. One
        // submission for several ids is for all of them, as Navidrome reads it.
        let completed = if submissions.len() == 1 {
            is_true(&submissions[0])
        } else {
            index >= submissions.len() || is_true(&submissions[index])
        };
        // Last.fm hears about outside songs, and library songs too unless the admin left
        // those to a Navidrome that scrobbles them itself (else they would count twice).
        let outside = state.local_library.parse_song_id(song_id).0;
        let last_fm_takes = scrobbling && (outside || state.last_fm_scrobbles.takes_library_plays());
        if !completed && !last_fm_takes {
            continue;
        }
        // A client that sends the same completed play again is not playing it again.
        let time = times.get(index).map(String::as_str);
        let reported_at = state.recent_scrobbles.now();
        if completed
            && !state
                .recent_scrobbles
                .first_report(&username, song_id, time, reported_at)
        {
            debug!("Scrobble of {song_id} for {username} repeats one already learned from");
            continue;
        }
        // Until something has learned from the play, it is not taken: a song that could not
        // be looked up this time is withdrawn, so the client's retry counts.
        let song = resolver.resolve_scrobble(song_id, &call.parameters).await;
        let Some(song) = song.filter(|s| !s.artist.is_empty() && !s.title.is_empty()) else {
            if completed {
                state
                    .recent_scrobbles
                    .withdraw(&username, song_id, time, reported_at);
            }
            continue;
        };
        let track = LastFmTrack::new(
            song.artist.clone(),
            song.title.clone(),
            Some(&song.album),
            song.duration,
        );
        if !completed {
            if last_fm_takes {
                state.last_fm_scrobbles.now_playing(&username, track);
            }
            continue;
        }
        let played_at = time
            .and_then(try_parse_long)
            .and_then(from_unix_time_milliseconds)
            .unwrap_or_else(Utc::now);
        if learning {
            recorded |= state.last_fm_radio_state.record_play(
                &username,
                LastFmRadioPlay {
                    song_id: song_id.clone(),
                    artist: song.artist.clone(),
                    title: song.title.clone(),
                    album: Some(song.album.clone()),
                    genre: song.genre.clone(),
                    duration: song.duration,
                    is_local: song.is_local,
                    played_at_utc: played_at,
                    source: "scrobble".into(),
                    ..Default::default()
                },
            );
        }
        // Queued, not awaited: the client's answer never waits on Last.fm.
        if last_fm_takes {
            state
                .last_fm_scrobbles
                .scrobble(&username, track, played_at, true);
        }
        if submitting && !song.is_local {
            state
                .listen_brainz
                .submit_listen(
                    &username,
                    &song.artist,
                    &song.title,
                    Some(&song.album),
                    song.duration,
                    played_at,
                )
                .await;
        }
    }
    if !learning || !recorded {
        return Ok(());
    }
    let user = state.last_fm_radio_state.get_user(&username);
    if last_fm_radio_refresh_policy::should_refresh_after_play(&user, &last_fm_settings, Utc::now()) {
        state.last_fm_radio_refresh_queue.enqueue(&username, None);
    }
    Ok(())
}

/// A 64-bit integer: surrounding white space and a sign are allowed.
fn try_parse_long(text: &str) -> Option<i64> {
    text.trim().parse::<i64>().ok()
}

/// A time from Unix milliseconds: `None` outside years 1 to 9999.
fn from_unix_time_milliseconds(unix: i64) -> Option<DateTime<Utc>> {
    const MIN: i64 = -62_135_596_800_000;
    const MAX: i64 = 253_402_300_799_999;
    if !(MIN..=MAX).contains(&unix) {
        return None;
    }
    DateTime::from_timestamp_millis(unix)
}

/// OpenSubsonic reportPlayback: Feishin pings this on play-start and during playback (176
/// hits in one session). For external ids Navidrome has no such media and returns an error,
/// so we ack with ok; local ids relay through so Navidrome's now-playing stays accurate.
pub async fn report_playback(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = call.format();
    let media_id = call
        .param_opt("mediaId")
        .unwrap_or_else(|| call.param("id"))
        .to_string();
    let (is_external, _, _) = state.local_library.parse_song_id(&media_id);

    if !is_external
        && !media_id.is_empty()
        && let Some(relay) = call
            .proxy
            .relay_safe("rest/reportPlayback", &call.parameters)
            .await
    {
        let content_type = relay
            .content_type
            .unwrap_or_else(|| format!("application/{format}"));
        return Ok(file(relay.body, &content_type));
    }
    Ok(state
        .subsonic_response_builder
        .create_response(&format, "reportPlayback")
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_milliseconds_outside_years_1_to_9999_are_refused() {
        assert!(from_unix_time_milliseconds(0).is_some());
        assert!(from_unix_time_milliseconds(253_402_300_799_999).is_some());
        assert!(from_unix_time_milliseconds(253_402_300_800_000).is_none());
        assert!(from_unix_time_milliseconds(-62_135_596_800_001).is_none());
        assert_eq!(try_parse_long(" 12 "), Some(12));
        assert_eq!(try_parse_int("+3"), Some(3));
        assert_eq!(try_parse_int("x"), None);
    }
}

#[cfg(test)]
#[path = "stars_family_tests.rs"]
mod family_tests;

#[cfg(test)]
#[path = "family_library_tests.rs"]
mod family_library_tests;
