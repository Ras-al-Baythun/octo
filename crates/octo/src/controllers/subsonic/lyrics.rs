//! `getLyricsBySongId`, `getLyrics`, `getLyricsCandidates` and `setLyricsChoice`.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use octo_core::common::dotnet::{is_blank, is_null_or_white_space};
use octo_core::family::abilities::Role;
use octo_core::json::dom::Node;
use octo_core::lyrics::lyrics_choices::LyricsPin;
use octo_core::lyrics::lyrics_models::{LyricsQuery, LyricsResult, LyricsTiming};
use octo_core::lyrics::lyrics_text::LyricsText;
use octo_core::models::domain::Song;
use octo_subsonic::xml::XElement;
use tokio_util::sync::CancellationToken;

use super::caller_checks::{
    SubsonicCall, check_caller, file, is_successful_subsonic_response, is_true, native_username,
};
use super::media::draws_its_own_marks;
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::family::family_caller;
use crate::services::outside::OutsideMetadataService;

/// A lyrics lookup made while a client waits: a slow or overloaded source costs at most this,
/// and the song simply shows no lyrics this time.
const INTERACTIVE_LYRICS_BUDGET: Duration = Duration::from_secs(4);

/// How long a lookup the caller stopped waiting for may keep going.
const BACKGROUND_LYRICS_LIMIT: Duration = Duration::from_secs(30);

/// What a family member who may not pin lyrics is told.
const LYRICS_PIN_REFUSED: &str = "Lyrics choices change lyrics for the whole family, so this account cannot set them. Ask a family manager.";

/// How long getLyricsCandidates may take: every source that is on, several entries each, is a
/// slower thing than playing a song, and a person is choosing.
const CANDIDATES_BUDGET: Duration = Duration::from_secs(12);

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic(
            "getLyricsBySongId",
            get(get_lyrics_by_song_id).post(get_lyrics_by_song_id),
        )
        .subsonic("getLyrics", get(get_lyrics).post(get_lyrics))
        .subsonic(
            "getLyricsCandidates",
            get(get_lyrics_candidates).post(get_lyrics_candidates),
        )
        .subsonic("setLyricsChoice", get(set_lyrics_choice).post(set_lyrics_choice))
}

/// Whether live lookups run: the lyrics service is there (always, here) and `FetchLyrics` is on.
fn fetching(state: &AppState) -> bool {
    state.settings.current().metadata.fetch_lyrics
}

/// OpenSubsonic getLyricsBySongId. Feishin fetches this every time a song plays. An external
/// track has no lyrics in Navidrome, so relaying one returned code 70 "data not found" per
/// play; it now gets real lyrics when LYRICS_FETCH is on (#52), and an empty-but-ok list
/// otherwise. A library song's own lyrics (what Navidrome has, in its tags or beside it) rank
/// among the sources as "song": they are served when they win, and the live lookup's when it
/// does, so a song whose tags hold line-timed lyrics still plays word-timed ones from a source
/// above it, or from any source when word timing is preferred.
///
/// A song someone pinned lyrics for (setLyricsChoice, or the dashboard) answers with those,
/// for every client, found by the song's artist and title when its id has changed since; one
/// set to "none" answers with none. Word cues go only to a client that asked with
/// enhanced=true; anyone else gets the lines exactly as before.
pub async fn get_lyrics_by_song_id(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let id = call.param("id").to_string();
    let format = call.format();
    let enhanced = is_true(call.param("enhanced"));
    let (is_external, _, _) = state.local_library.parse_song_id(&id);
    let fetching = fetching(&state);
    let choices = &state.lyrics_choice_service;
    let builder = &state.subsonic_response_builder;
    let mut pin = if id.is_empty() { None } else { choices.pin_for(&id) };

    if is_external {
        let routing = state
            .external_id_registry
            .lookup(&id)
            .map(|r| r.snapshot())
            .or_else(|| OutsideMetadataService::try_decode_external_id(Some(&id)));
        let known = routing.as_ref().filter(|r| r.has_artist_title());
        let artist = known.and_then(|r| r.artist.clone()).unwrap_or_default();
        let title = known.and_then(|r| r.title.clone()).unwrap_or_default();
        if pin.is_none() {
            pin = choices.pin_for_song(&id, Some(&artist), Some(&title));
        }
        if let Some(pin) = pin {
            return builder
                .create_lyrics_list_response(
                    &format,
                    pin.lyrics().as_ref(),
                    pin.artist.as_deref().unwrap_or(&artist),
                    pin.title.as_deref().unwrap_or(&title),
                    enhanced,
                )
                .into_response();
        }

        let mut found = None;
        let mut still_looking = false;
        if fetching && let Some(routing) = known {
            (found, still_looking) = live_lyrics(
                &state,
                &artist,
                &title,
                routing.album.clone(),
                routing.duration,
                LyricsTiming::None,
            )
            .await;
        }
        if found.is_none() && still_looking && draws_its_own_marks(&call) {
            return still_looking_for_lyrics(&state, &format);
        }
        return builder
            .create_lyrics_list_response(&format, found.as_ref(), &artist, &title, enhanced)
            .into_response();
    }

    // Asked with word cues whenever the answer is JSON, so how the song's own lyrics are timed
    // is known; a client that did not ask gets them without (see without_cues).
    let json = format.eq_ignore_ascii_case("json");
    let mut asking = call.parameters.clone();
    if json && !enhanced {
        asking.insert("enhanced".into(), "true".into());
    }
    let Some(relay) = call.proxy.relay_safe("rest/getLyricsBySongId", &asking).await else {
        return builder.create_response(&format, "lyricsList").into_response();
    };

    // Navidrome answering ok is also what says the caller may see this song.
    let allowed = is_successful_subsonic_response(&relay.body, &format);
    let own = if json && allowed {
        navidrome_lyrics_timing(&relay.body)
    } else {
        None
    };
    let song = if allowed && ((fetching && own.is_some()) || (pin.is_none() && choices.any_pins())) {
        library_song(&call, &id).await
    } else {
        None
    };
    if pin.is_none()
        && let Some(song) = &song
    {
        pin = choices.pin_for_song(&id, Some(&song.artist), Some(&song.title));
    }
    if let Some(pin) = pin.filter(|_| allowed) {
        let artist = pin
            .artist
            .clone()
            .or_else(|| song.as_ref().map(|s| s.artist.clone()))
            .unwrap_or_default();
        let title = pin
            .title
            .clone()
            .or_else(|| song.as_ref().map(|s| s.title.clone()))
            .unwrap_or_default();
        return builder
            .create_lyrics_list_response(&format, pin.lyrics().as_ref(), &artist, &title, enhanced)
            .into_response();
    }

    // Read-only: nothing is written beside a file Octo did not download.
    if fetching
        && let Some(timing) = own
        && let Some(song) = &song
    {
        let (found, still_looking) = live_lyrics(
            &state,
            &song.artist,
            &song.title,
            Some(song.album.clone()),
            song.duration,
            timing,
        )
        .await;
        // The lookup ranked the song's own among the sources, so anything else it found won;
        // only "instrumental" never outranks lyrics the song has.
        if let Some(found) = found.as_ref().filter(|f| !f.is_songs_own())
            && (timing == LyricsTiming::None || !found.instrumental)
        {
            return builder
                .create_lyrics_list_response(&format, Some(found), &song.artist, &song.title, enhanced)
                .into_response();
        }
        if timing == LyricsTiming::None && still_looking && draws_its_own_marks(&call) {
            return still_looking_for_lyrics(&state, &format);
        }
    }
    let body = if json && !enhanced {
        without_cues(&relay.body)
    } else {
        relay.body.to_vec()
    };
    let content_type = relay
        .content_type
        .unwrap_or_else(|| format!("application/{format}"));
    file(body, &content_type)
}

/// The legacy lyrics call, by artist and title, which older clients (DSub, Subsonic's own)
/// use. Navidrome answers for its own songs; when it has nothing, the same live lookup as
/// getLyricsBySongId fills in, as plain text, and a pin for that artist and title wins.
pub async fn get_lyrics(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format();
    let artist = call.param("artist").trim().to_string();
    let title = call.param("title").trim().to_string();
    let builder = &state.subsonic_response_builder;

    let Some(relay) = call.proxy.relay_safe("rest/getLyrics", &call.parameters).await else {
        return builder
            .create_error(&format, 0, "Octo can't reach Navidrome")
            .into_response();
    };
    let content_type = relay
        .content_type
        .clone()
        .unwrap_or_else(|| format!("application/{format}"));
    if !is_successful_subsonic_response(&relay.body, &format) || artist.is_empty() || title.is_empty() {
        return file(relay.body, &content_type);
    }

    if let Some(pin) = state.lyrics_choice_service.pin_for_name(&artist, &title) {
        return builder
            .create_lyrics_response(&format, pin.lyrics().as_ref(), &artist, &title)
            .into_response();
    }

    if fetching(&state)
        && has_no_legacy_lyrics(&relay.body, &format)
        && let (Some(found), _) = live_lyrics(&state, &artist, &title, None, None, LyricsTiming::None).await
    {
        return builder
            .create_lyrics_response(&format, Some(&found), &artist, &title)
            .into_response();
    }
    file(relay.body, &content_type)
}

/// Whether Navidrome's legacy answer has no words: its `lyrics.value` (JSON) or the text of
/// its `lyrics` element (XML) is blank. Anything unreadable has some.
fn has_no_legacy_lyrics(body: &[u8], format: &str) -> bool {
    if format.eq_ignore_ascii_case("json") {
        let Ok(text) = std::str::from_utf8(body) else {
            return false;
        };
        let Ok(root) = Node::parse(text) else {
            return false;
        };
        let value = index(Some(&root), "subsonic-response")
            .and_then(|n| index(n, "lyrics"))
            .and_then(|n| index(n, "value"));
        return match value {
            Ok(None) => true,
            Ok(Some(Node::String(text))) => is_null_or_white_space(Some(text)),
            // Anything else is not empty.
            _ => false,
        };
    }
    match XElement::parse(&String::from_utf8_lossy(body)) {
        Ok(root) => root
            .elements()
            .find(|element| element.name == "lyrics")
            .is_none_or(|element| is_null_or_white_space(Some(&element.value()))),
        Err(_) => false,
    }
}

/// octoLyrics v1: every lyrics entry the sources that are on hold for a song, for choosing
/// between them, with what the song is set to now. title and artist, when given, search
/// for those instead of the song's own tags, for a song that is tagged wrong. Always JSON.
/// Credentials are checked with a ping to Navidrome, as getAcquisitions checks them.
pub async fn get_lyrics_candidates(State(state): State<AppState>, req: Request) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    const FORMAT: &str = "json";
    let builder = &state.subsonic_response_builder;
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    if !fetching(&state) {
        return builder
            .create_error(FORMAT, 0, "Lyrics lookups are off on this server")
            .into_response();
    }

    let id = call.param("id").to_string();
    if is_blank(&id) {
        return builder
            .create_error(FORMAT, 10, "Required parameter is missing: id")
            .into_response();
    }
    let Some(song) = song_for_lyrics(&state, &call, &id).await else {
        return builder.create_error(FORMAT, 70, "Song not found").into_response();
    };

    let artist = Some(call.param("artist").trim())
        .filter(|a| !a.is_empty())
        .map_or_else(|| song.artist.clone(), str::to_string);
    let title = Some(call.param("title").trim())
        .filter(|t| !t.is_empty())
        .map_or_else(|| song.title.clone(), str::to_string);
    let budget = budget(CANDIDATES_BUDGET);
    let choices = &state.lyrics_choice_service;
    let query = LyricsQuery::new(
        artist.clone(),
        LyricsText::query_title(&title, &artist),
        Some(song.album.clone()),
        song.duration,
    );
    let candidates = choices.candidates(&query, &budget).await;
    budget.cancel();
    builder
        .create_lyrics_candidates_response(
            &id,
            &choices.choice_for_song(&id, Some(&song.artist), Some(&song.title)),
            &candidates,
        )
        .into_response()
}

/// octoLyrics v1: set a song's lyrics to one candidate from getLyricsCandidates, to "none"
/// to show no lyrics, or to "auto" to go back to finding them. The choice is the server's,
/// so every client and every user sees it, and getLyricsBySongId and getLyrics honor it.
pub async fn set_lyrics_choice(State(state): State<AppState>, req: Request) -> Response {
    let caller = family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    const FORMAT: &str = "json";
    let builder = &state.subsonic_response_builder;
    let choices = &state.lyrics_choice_service;
    if let Some(refused) = check_caller(&state, &call).await {
        return refused;
    }
    // A pin changes lyrics for the whole family: members from the Member preset up may set one.
    if caller
        .as_ref()
        .is_some_and(|caller| caller.managed && matches!(caller.role, Role::Listener | Role::Kid))
    {
        return builder
            .create_error(FORMAT, 50, LYRICS_PIN_REFUSED)
            .into_response();
    }

    let id = call.param("id").to_string();
    let candidate = call.param("candidate").trim().to_string();
    if is_blank(&id) || candidate.is_empty() {
        return builder
            .create_error(FORMAT, 10, "Required parameter is missing: id and candidate")
            .into_response();
    }

    let who = native_username(&state, &call);
    let song = song_for_lyrics(&state, &call, &id).await;
    if candidate.eq_ignore_ascii_case(LyricsPin::AUTO) {
        choices.clear_song(
            &id,
            song.as_ref().map(|s| s.artist.as_str()),
            song.as_ref().map(|s| s.title.as_str()),
        );
        return builder
            .create_lyrics_choice_response(&id, LyricsPin::AUTO)
            .into_response();
    }

    let Some(song) = song else {
        return builder.create_error(FORMAT, 70, "Song not found").into_response();
    };
    if candidate.eq_ignore_ascii_case(LyricsPin::HIDDEN) {
        choices.hide(&id, Some(&song.artist), Some(&song.title), Some(&who));
        return builder
            .create_lyrics_choice_response(&id, LyricsPin::HIDDEN)
            .into_response();
    }

    if !fetching(&state) {
        return builder
            .create_error(FORMAT, 0, "Lyrics lookups are off on this server")
            .into_response();
    }
    let budget = budget(CANDIDATES_BUDGET);
    let pinned = choices
        .pin(
            &id,
            &candidate,
            Some(&song.artist),
            Some(&song.title),
            Some(&who),
            &budget,
        )
        .await;
    budget.cancel();
    if !pinned {
        return builder
            .create_error(
                FORMAT,
                70,
                "Those lyrics could not be found; ask for the candidates again",
            )
            .into_response();
    }
    builder
        .create_lyrics_choice_response(&id, &candidate)
        .into_response()
}

/// A token canceled after `limit`. The caller cancels it once done, which ends the timer.
fn budget(limit: Duration) -> CancellationToken {
    let token = CancellationToken::new();
    let timer = token.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = tokio::time::sleep(limit) => timer.cancel(),
            _ = timer.cancelled() => {}
        }
    });
    token
}

/// What lyrics are looked up by, for an outside song from the registry and for a library song
/// from Navidrome as the caller sees it.
async fn song_for_lyrics(state: &AppState, call: &SubsonicCall, id: &str) -> Option<Song> {
    let (is_external, _, _) = state.local_library.parse_song_id(id);
    if !is_external {
        return library_song(call, id).await;
    }
    let routing = state
        .external_id_registry
        .lookup(id)
        .map(|r| r.snapshot())
        .or_else(|| OutsideMetadataService::try_decode_external_id(Some(id)))?;
    routing.has_artist_title().then(|| Song {
        artist: routing.artist.clone().unwrap_or_default(),
        title: routing.title.clone().unwrap_or_default(),
        album: routing.album.clone().unwrap_or_default(),
        duration: routing.duration,
        ..Default::default()
    })
}

/// Lyrics for a song as it plays, within the interactive budget. A lookup that runs out of
/// time keeps going in the background (up to [`BACKGROUND_LYRICS_LIMIT`]), so the service has
/// the answer cached for the next ask; the caller learns it is still looking.
async fn live_lyrics(
    state: &AppState,
    artist: &str,
    title: &str,
    album: Option<String>,
    duration: Option<i32>,
    songs_own: LyricsTiming,
) -> (Option<LyricsResult>, bool) {
    let query = LyricsQuery::new(artist, LyricsText::query_title(title, artist), album, duration);
    // Not tied to the request: a client that stops waiting must not stop the lookup.
    let limit = budget(BACKGROUND_LYRICS_LIMIT);
    let service = Arc::clone(&state.lyrics_service);
    let lookup = tokio::spawn(async move {
        let answer = service.find(&query, &limit, songs_own).await;
        limit.cancel();
        answer
    });
    match tokio::time::timeout(INTERACTIVE_LYRICS_BUDGET, lookup).await {
        Err(_) => (None, true),
        Ok(Err(_)) => (None, true),
        Ok(Ok(answer)) => {
            let still_looking = answer.result.is_none() && answer.transient;
            (answer.result, still_looking)
        }
    }
}

/// Told only to the Octo app: the lookup is not finished, so this is "not yet", never "none".
/// Other clients keep getting the ordinary empty list, which is what the spec gives them.
fn still_looking_for_lyrics(state: &AppState, format: &str) -> Response {
    state
        .subsonic_response_builder
        .create_error(format, 0, "Still looking for lyrics; ask again shortly")
        .into_response()
}

/// A member of a node: nothing for a missing node or a JSON null, the member of an object, and
/// an error on anything else.
fn index<'a>(node: Option<&'a Node>, key: &str) -> Result<Option<&'a Node>, ()> {
    match node {
        None | Some(Node::Null) => Ok(None),
        Some(Node::Object(fields)) => Ok(fields.get(key).filter(|n| !matches!(n, Node::Null))),
        Some(_) => Err(()),
    }
}

/// How the lyrics Navidrome sent are timed, the best entry's: word cues, timed lines, plain,
/// or None when it has none. `None` when the answer is not one Octo can read.
pub(crate) fn navidrome_lyrics_timing(body: &[u8]) -> Option<LyricsTiming> {
    let text = std::str::from_utf8(body).ok()?;
    let root = Node::parse(text).ok()?;
    let lyrics = index(Some(&root), "subsonic-response")
        .and_then(|n| index(n, "lyricsList"))
        .and_then(|n| index(n, "structuredLyrics"))
        .ok()?;
    let Some(lyrics) = lyrics else {
        return Some(LyricsTiming::None);
    };
    let Node::Array(entries) = lyrics else {
        return None;
    };
    let mut best = LyricsTiming::None;
    for entry in entries {
        let non_empty_array =
            |node: Option<&Node>| matches!(node, Some(Node::Array(items)) if !items.is_empty());
        if !non_empty_array(index(Some(entry), "line").ok()?) {
            continue;
        }
        let timing = if non_empty_array(index(Some(entry), "cueLine").ok()?) {
            LyricsTiming::Word
        } else if matches!(index(Some(entry), "synced").ok()?, Some(Node::Bool(true))) {
            LyricsTiming::Line
        } else {
            LyricsTiming::Plain
        };
        if timing > best {
            best = timing;
        }
    }
    Some(best)
}

/// Navidrome's lyrics as a client that did not ask for word cues gets them: without the cue
/// lines and the kind that came with them. Unchanged when they cannot be read.
pub(crate) fn without_cues(body: &[u8]) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(body) else {
        return body.to_vec();
    };
    let Ok(mut root) = Node::parse(text) else {
        return body.to_vec();
    };
    let mut changed = false;
    {
        let entries = root
            .as_object_mut()
            .and_then(|r| r.get_mut("subsonic-response"))
            .and_then(Node::as_object_mut)
            .and_then(|r| r.get_mut("lyricsList"))
            .and_then(Node::as_object_mut)
            .and_then(|r| r.get_mut("structuredLyrics"));
        let Some(Node::Array(entries)) = entries else {
            return body.to_vec();
        };
        for entry in entries.iter_mut() {
            if let Node::Object(fields) = entry {
                let cue = fields.shift_remove("cueLine").is_some();
                let kind = fields.shift_remove("kind").is_some();
                changed |= cue | kind;
            }
        }
    }
    if changed {
        root.to_json_string(false).into_bytes()
    } else {
        body.to_vec()
    }
}

/// Artist, title, album and length of a library song, asked as the calling user.
async fn library_song(call: &SubsonicCall, id: &str) -> Option<Song> {
    let mut request = call.parameters.clone();
    request.insert("id".into(), id.to_string());
    request.insert("f".into(), "json".into());
    let relay = call.proxy.relay_safe("rest/getSong", &request).await?;
    let text = std::str::from_utf8(&relay.body).ok()?;
    let root = Node::parse(text).ok()?;
    let song = index(Some(&root), "subsonic-response")
        .and_then(|n| index(n, "song"))
        .ok()?;
    // A field that is not a string reads as "no song".
    let string = |key: &str| -> Result<Option<String>, ()> {
        match index(song, key)? {
            None => Ok(None),
            Some(Node::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(()),
        }
    };
    let artist = string("artist").ok()?;
    let title = string("title").ok()?;
    let (Some(artist), Some(title)) = (artist, title) else {
        return None;
    };
    if is_blank(&artist) || is_blank(&title) {
        return None;
    }
    let album = string("album").ok()?.unwrap_or_default();
    let duration = match index(song, "duration").ok()? {
        None => None,
        Some(Node::Number(text)) => Some(text.parse::<i32>().ok()?),
        Some(_) => return None,
    };
    Some(Song {
        artist,
        title,
        album,
        duration,
        ..Default::default()
    })
}

#[cfg(test)]
#[path = "lyrics_timing_tests.rs"]
mod tests;
