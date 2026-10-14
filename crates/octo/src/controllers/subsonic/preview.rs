//! octoPreview version 1: the videos a song from outside the library could play from
//! (getPreviewChoices), and the one the listener picks (setPreview), which every later play
//! streams and a YouTube download fetches. Always JSON, and every call is checked with a ping
//! to Navidrome as the downloads drawer's are.

use axum::extract::{Request, State};
use axum::response::IntoResponse;
use axum::routing::get;

use super::action_helpers::int_try_parse;
use super::caller_checks::{SubsonicCall, check_caller};
use crate::app::AppState;
use crate::http::error::AppResult;
use crate::http::routes::RouteSet;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::soulseek::SoulseekMetadataService;
use crate::services::subsonic::SubsonicResponseBuilderExt;
use crate::services::you_tube::preview_choices::{preview_choices, set_preview};

const FORMAT: &str = "json";
const NOT_OUTSIDE: &str = "Only a song from outside the library has versions to choose from";

pub fn routes() -> RouteSet {
    RouteSet::new()
        .subsonic(
            "getPreviewChoices",
            get(get_preview_choices).post(get_preview_choices),
        )
        .subsonic("setPreview", get(set_preview_video).post(set_preview_video))
}

/// The trimmed value of a parameter, empty when it is missing.
fn trimmed(call: &SubsonicCall, name: &str) -> String {
    call.param_opt(name).unwrap_or_default().trim().to_string()
}

/// The registry id behind `id` when it names a song from outside the library.
fn outside_song(state: &AppState, id: &str) -> Option<String> {
    let (external, provider, external_id) = state.local_library.parse_song_id(id);
    if !external
        || !provider
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(SoulseekMetadataService::PROVIDER_NAME))
    {
        return None;
    }
    external_id.filter(|external_id| !external_id.is_empty())
}

/// The videos the song could play from, the one it plays from now marked.
pub async fn get_preview_choices(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }
    let builder = &state.subsonic_response_builder;
    let id = trimmed(&call, "id");
    if id.is_empty() {
        return Ok(builder
            .create_error(FORMAT, 10, "Required parameter is missing: id")
            .into_response());
    }
    let Some(external_id) = outside_song(&state, &id) else {
        return Ok(builder.create_error(FORMAT, 70, NOT_OUTSIDE).into_response());
    };
    let Some(choices) = preview_choices(
        &state.you_tube_resolver,
        &state.external_id_registry,
        &external_id,
    )
    .await
    else {
        return Ok(builder
            .create_error(FORMAT, 70, "No song with this id")
            .into_response());
    };
    Ok(builder
        .create_preview_choices_response(&id, &choices)
        .into_response())
}

/// Makes the song play from the video, and answers the song as getSong does, with the
/// video's length when `length` gave it.
pub async fn set_preview_video(State(state): State<AppState>, req: Request) -> AppResult {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return Ok(response),
    };
    if let Some(refused) = check_caller(&state, &call).await {
        return Ok(refused);
    }
    let builder = &state.subsonic_response_builder;
    let id = trimmed(&call, "id");
    let video = trimmed(&call, "videoId");
    for (name, value) in [("id", &id), ("videoId", &video)] {
        if value.is_empty() {
            return Ok(builder
                .create_error(FORMAT, 10, &format!("Required parameter is missing: {name}"))
                .into_response());
        }
    }
    let length = int_try_parse(&trimmed(&call, "length"));
    let Some(external_id) = outside_song(&state, &id) else {
        return Ok(builder.create_error(FORMAT, 70, NOT_OUTSIDE).into_response());
    };
    if !set_preview(&state.external_id_registry, &external_id, &video, length) {
        return Ok(builder
            .create_error(FORMAT, 70, "No song with this id")
            .into_response());
    }
    // The shim resolves the video now, so the play that follows starts from its cache.
    let youtube = state.you_tube_resolver.clone();
    let warmed = video.clone();
    tokio::spawn(async move {
        let _ = youtube.facts(&warmed, false).await;
    });
    let Some(mut song) = state
        .music_metadata
        .get_song(SoulseekMetadataService::PROVIDER_NAME, &external_id)
        .await
    else {
        return Ok(builder.create_error(FORMAT, 70, "Song not found").into_response());
    };
    state.music_metadata.complete_song_year(&mut song).await;
    Ok(builder.create_song_response(FORMAT, &song).into_response())
}
