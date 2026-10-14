//! getArtistTopSongs and getTopChart (octoTopSongs, for the Octo apps), the standard getTopSongs
//! (every Subsonic app), and getCharts (octoTopSongs v2: the charts the server's country has).

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use octo_core::charts::chart_settings::{self, ChartKind, OVERALL};
use octo_core::charts::top_songs::MAX_ROWS;
use octo_core::models::domain::Song;
use octo_core::outside::routing::RoutingKind;
use octo_subsonic::subsonic_request_parser::Parameters;
use octo_subsonic::subsonic_response_builder::{ChartChoiceRow, RankedRow, TopSongRow, TopSongsHeader};
use serde_json::Value;
use tracing::{debug, info};

use super::caller_checks::{SubsonicCall, refuse_unless_signed_in, signed_in_user};
use super::native::has_external_id;
use crate::app::AppState;
use crate::http::error::AppResult;
use crate::http::routes::RouteSet;
use crate::services::common::{TopList, TopSongsService};
use crate::services::subsonic::SubsonicProxyService;

/// How many rows a list answers when the caller does not say.
const TOP_SONGS_DEFAULT_COUNT: usize = 20;
/// The count the Subsonic API gives getTopSongs when the caller does not say.
const STANDARD_TOP_SONGS_DEFAULT_COUNT: usize = 50;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic(
            "getArtistTopSongs",
            get(get_artist_top_songs).post(get_artist_top_songs),
        )
        .subsonic("getTopChart", get(get_top_chart).post(get_top_chart))
        .subsonic("getCharts", get(get_charts).post(get_charts))
        .subsonic("getTopSongs", get(get_top_songs).post(get_top_songs))
}

/// The service, while search discovery is on: its lists are outside songs.
fn top_songs(state: &AppState) -> Option<&TopSongsService> {
    state
        .settings
        .current()
        .subsonic
        .enable_search_discovery
        .then_some(&state.top_songs)
}

/// octoTopSongs v1: an artist's most played songs, ranked, for the apps' search. Each is marked
/// in the caller's library or not. `artist` is the name; `id` may be the artist's id from search,
/// which names the catalog artist surely. Always JSON.
pub async fn get_artist_top_songs(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = refuse_unless_signed_in(&state, &call, "json").await? {
        return Ok(refused);
    }
    let name = call.param_opt("artist").unwrap_or("").trim().to_string();
    if name.is_empty() {
        return Ok(state
            .subsonic_response_builder
            .create_error("json", 10, "Required parameter is missing: artist")
            .into_response());
    }
    let Some(service) = top_songs(&state) else {
        return Ok(empty_answer(&state));
    };
    let list = service.for_artist(&name, call.param_opt("id")).await;
    let count = count(&call, TOP_SONGS_DEFAULT_COUNT, MAX_ROWS);
    Ok(answer(&state, &call, &list, count).await)
}

/// octoTopSongs: a chart, ranked and marked like getArtistTopSongs. `chart` (v2) is `34` (the
/// default, Popular right now), `new`, `trending` or an Apple genre id. Always JSON.
pub async fn get_top_chart(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = refuse_unless_signed_in(&state, &call, "json").await? {
        return Ok(refused);
    }
    let chart = call
        .param_opt("chart")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or(OVERALL)
        .to_ascii_lowercase();
    if chart_settings::kind(&chart).is_none() {
        return Ok(state
            .subsonic_response_builder
            .create_error("json", 10, "Unknown chart.")
            .into_response());
    }
    let Some(service) = top_songs(&state) else {
        return Ok(empty_answer(&state));
    };
    let list = service.chart(&chart).await;
    let count = count(&call, TOP_SONGS_DEFAULT_COUNT, chart_settings::max_songs(&chart));
    Ok(answer(&state, &call, &list, count).await)
}

/// octoTopSongs v2: the charts the server's country has, Popular right now first, then Best New
/// Songs and Trending Songs, then the genres in Apple's order, each with whether it is a
/// playlist for everyone and, while it is one, this listener's playlist id. Always JSON.
pub async fn get_charts(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = refuse_unless_signed_in(&state, &call, "json").await? {
        return Ok(refused);
    }
    let Some(service) = top_songs(&state) else {
        return Ok(state
            .subsonic_response_builder
            .create_charts_response(None, &[])
            .into_response());
    };
    let username = signed_in_user(&state, &call).await?;
    let (country, menu) = service.menu().await;
    let lists = state.settings.current().generated_playlists.clone();
    let on: Vec<String> = lists.chart_ids();
    let mut ids: Vec<String> = vec![
        OVERALL.into(),
        chart_settings::NEW_SONGS.into(),
        chart_settings::TRENDING.into(),
    ];
    ids.extend(menu.iter().map(|g| g.id.clone()).filter(|id| id != OVERALL));
    let rows: Vec<ChartChoiceRow> = ids
        .iter()
        .filter_map(|id| {
            let (name, label) = TopSongsService::names(&menu, id)?;
            let kind = match chart_settings::kind(id)? {
                ChartKind::Overall => "overall",
                ChartKind::NewSongs => "new",
                ChartKind::Trending => "trending",
                ChartKind::Genre => "genre",
            };
            let is_on = if id == OVERALL {
                lists.popular_now
            } else {
                on.contains(id)
            };
            Some(ChartChoiceRow {
                id: id.clone(),
                name,
                label,
                kind,
                playlist: is_on
                    .then(|| state.chart_playlists.listed_id(&username, id))
                    .flatten(),
                on: is_on,
            })
        })
        .collect();
    Ok(state
        .subsonic_response_builder
        .create_charts_response(country.as_deref(), &rows)
        .into_response())
}

/// The standard getTopSongs (`artist` by name, `count`), for every Subsonic client, and by the
/// artist's `id` instead of the name. Navidrome answers it from the library alone, often with
/// nothing. Octo answers with the artist's songs ranked as getArtistTopSongs ranks them: a song
/// the caller's library has is the library's own entry, and the rest are outside songs that play
/// and are added with a heart. Outside songs follow search discovery. When Octo has nothing to
/// give, Navidrome's own answer is passed on, as before.
pub async fn get_top_songs(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    let format = call.param_opt("f").unwrap_or("xml").to_string();
    let mut name = call.param_opt("artist").unwrap_or("").trim().to_string();
    let artist_id = call.param_opt("id").unwrap_or("").trim().to_string();
    // A missing artist is Navidrome's to refuse, in its own words.
    if name.is_empty() && artist_id.is_empty() {
        return Ok(no_top_songs(&state, &call, &format).await);
    }
    if let Some(refused) = refuse_unless_signed_in(&state, &call, &format).await? {
        return Ok(refused);
    }
    if name.is_empty() {
        name = artist_name(&state, &call, &artist_id).await.unwrap_or_default();
    }
    if name.is_empty() {
        return Ok(no_top_songs(&state, &call, &format).await);
    }

    let count = count(&call, STANDARD_TOP_SONGS_DEFAULT_COUNT, MAX_ROWS);
    let list = state
        .top_songs
        .for_artist(&name, Some(artist_id.as_str()).filter(|id| !id.is_empty()))
        .await;
    let outside = state.settings.current().subsonic.enable_search_discovery;

    // With outside songs shown, only the rows asked for are looked up in the library. Without
    // them every row is, because the library's rows are all that is left to list.
    let candidates: Vec<_> = if outside {
        list.songs.iter().take(count).cloned().collect()
    } else {
        list.songs.clone()
    };
    let library = if candidates.is_empty() {
        Vec::new()
    } else {
        TopSongsService::match_library(&candidates, |query| {
            library_songs_matching(&call.proxy, &call.parameters, query)
        })
        .await
    };

    let username = call.param_opt("u").unwrap_or("").to_string();
    let mut rows: Vec<TopSongRow> = Vec::new();
    for (i, top) in candidates.iter().enumerate() {
        if rows.len() >= count {
            break;
        }
        match library.get(i).cloned().flatten() {
            Some(owned) => rows.push(TopSongRow::Library(owned)),
            None if outside => rows.push(TopSongRow::Outside(synced_song(&state, &username, &top.song))),
            None => {}
        }
    }
    if rows.is_empty() {
        return Ok(no_top_songs(&state, &call, &format).await);
    }
    let in_library = rows
        .iter()
        .filter(|r| matches!(r, TopSongRow::Library(_)))
        .count();
    info!(
        "getTopSongs '{name}' ({}): {in_library} library and {} outside songs, ranked by {}",
        call.param_opt("c").unwrap_or(""),
        rows.len() - in_library,
        list.source
    );
    Ok(state
        .subsonic_response_builder
        .create_standard_top_songs_response(&format, &rows)
        .into_response())
}

/// The answer when Octo has none: Navidrome's own, except for an outside artist's id, which
/// Navidrome has never heard of. That gets an empty list.
async fn no_top_songs(state: &AppState, call: &SubsonicCall, format: &str) -> Response {
    let builder = &state.subsonic_response_builder;
    if has_external_id(state, &call.parameters) {
        return builder
            .create_standard_top_songs_response(format, &[])
            .into_response();
    }
    match call.proxy.relay_safe("rest/getTopSongs", &call.parameters).await {
        Some(relay) if !relay.body.is_empty() => {
            let content_type = relay
                .content_type
                .unwrap_or_else(|| format!("application/{format}"));
            ([(axum::http::header::CONTENT_TYPE, content_type)], relay.body).into_response()
        }
        _ => builder
            .create_error(format, 0, "Unable to reach Navidrome")
            .into_response(),
    }
}

/// The name of the artist an id names: an outside artist's from what Octo remembers of it, a
/// library artist's from Navidrome, asked as the caller. None when neither knows.
async fn artist_name(state: &AppState, call: &SubsonicCall, artist_id: &str) -> Option<String> {
    if let Some(routing) = state.external_id_registry.lookup(artist_id) {
        let routing = routing.snapshot();
        if routing.kind == RoutingKind::Artist
            && let Some(name) = routing.artist.filter(|a| !a.is_empty())
        {
            return Some(name);
        }
    }
    if has_external_id(state, &call.parameters) {
        return None;
    }
    let mut lookup = call.parameters.clone();
    lookup.insert("f".into(), "json".into());
    lookup.shift_remove("count");
    let relay = call.proxy.relay_safe("rest/getArtist", &lookup).await?;
    let doc: Value = serde_json::from_slice(&relay.body).ok()?;
    doc.pointer("/subsonic-response/artist/name")
        .and_then(Value::as_str)
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
}

/// An outside song as this listener's sync catalog describes it, when it holds the song: a
/// client that copies the library keeps whichever description it read last, so the two must
/// agree, as on a station playlist.
fn synced_song(state: &AppState, username: &str, song: &Song) -> Song {
    if song.is_local {
        return song.clone();
    }
    state
        .sync_catalog
        .try_get_song(username, &song.id)
        .unwrap_or_else(|| song.clone())
}

/// The first `count` rows, each matched against the caller's own library.
async fn answer(state: &AppState, call: &SubsonicCall, list: &TopList, count: usize) -> Response {
    let shown: Vec<_> = list.songs.iter().take(count).cloned().collect();
    let library = TopSongsService::match_library(&shown, |query| {
        library_songs_matching(&call.proxy, &call.parameters, query)
    })
    .await;
    let rows: Vec<RankedRow<'_>> = shown
        .iter()
        .enumerate()
        .map(|(i, top)| RankedRow {
            rank: top.rank,
            song: &top.song,
            plays: top.plays,
            listeners: top.listeners,
            library: library.get(i).and_then(Option::as_ref),
        })
        .collect();
    let header = TopSongsHeader {
        artist: list.artist.as_deref(),
        source: &list.source,
        chart: list.chart.as_deref(),
        name: list.name.as_deref(),
        country: list.country.as_deref(),
    };
    state
        .subsonic_response_builder
        .create_top_songs_response(&header, &rows)
        .into_response()
}

fn empty_answer(state: &AppState) -> Response {
    state
        .subsonic_response_builder
        .create_top_songs_response(&TopSongsHeader::default(), &[])
        .into_response()
}

/// `count`, when given and above 0, at most `max`; else the default.
fn count(call: &SubsonicCall, default: usize, max: usize) -> usize {
    call.param_opt("count")
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|c| *c > 0)
        .map_or(default, |c| c.min(max))
}

/// Navidrome's songs for a query, searched as the caller: Some (maybe empty) when it answered,
/// None when it did not. Nothing outside the library: this goes to Navidrome, not through Octo's
/// search.
pub(crate) async fn library_songs_matching(
    proxy: &SubsonicProxyService,
    parameters: &Parameters,
    query: String,
) -> Option<Vec<Value>> {
    let mut search = parameters.clone();
    search.insert("query".into(), query.clone());
    search.insert("songCount".into(), "5".into());
    search.insert("albumCount".into(), "0".into());
    search.insert("artistCount".into(), "0".into());
    search.insert("f".into(), "json".into());
    search.shift_remove("count");
    search.shift_remove("id");
    search.shift_remove("artist");
    search.shift_remove("chart");
    let Some(relay) = proxy.relay_safe("rest/search3", &search).await else {
        debug!("top songs: library search '{query}' had no answer");
        return None;
    };
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
#[path = "top_songs_tests.rs"]
mod tests;
