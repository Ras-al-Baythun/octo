//! octoPreview: the videos a song could play from (getPreviewChoices), and the one the
//! listener picks (setPreview). For a song from outside the library (version 1) the pick is
//! what every later play streams and a YouTube download fetches; for a song that is a file in
//! the library (version 2) it is what `libraryAction?action=replace` downloads in the file's
//! place. Always JSON, and every call is checked with a ping to Navidrome as the downloads
//! drawer's are.

use axum::extract::{Request, State};
use axum::response::IntoResponse;
use axum::routing::get;

use super::action_helpers::int_try_parse;
use super::caller_checks::{SubsonicCall, check_caller, file};
use crate::app::AppState;
use crate::http::error::AppResult;
use crate::http::routes::RouteSet;
use crate::services::i_music_metadata_service::IMusicMetadataService;
use crate::services::library::library_song_routing::routing_for_library_song;
use crate::services::outside::OutsideMetadataService;
use crate::services::subsonic::SubsonicResponseBuilderExt;
use crate::services::you_tube::preview_choices::{preview_choices, set_preview};

const FORMAT: &str = "json";
const NO_SUCH_SONG: &str = "No song with this id";

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

/// The song `id` names, by the registry id its versions are chosen on.
enum PreviewSong {
    /// A song from outside the library: its own registry id.
    Outside(String),
    /// A song that is a file in the library: the routing that stands for it.
    Library(String),
}

impl PreviewSong {
    fn routing(&self) -> &str {
        match self {
            PreviewSong::Outside(id) | PreviewSong::Library(id) => id,
        }
    }
}

/// The song `id` names: an outside song by Octo's short id, else a library song whose file
/// can be worked out. None for anything else, such as another provider's song or an id
/// Navidrome does not know.
async fn preview_song(state: &AppState, id: &str) -> Option<PreviewSong> {
    let (external, provider, external_id) = state.local_library.parse_song_id(id);
    if external {
        let outside = provider
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(OutsideMetadataService::PROVIDER_NAME));
        return external_id
            .filter(|external_id| outside && !external_id.is_empty())
            .map(PreviewSong::Outside);
    }
    routing_for_library_song(state, id)
        .await
        .map(PreviewSong::Library)
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
    let Some(song) = preview_song(&state, &id).await else {
        return Ok(builder.create_error(FORMAT, 70, NO_SUCH_SONG).into_response());
    };
    let Some(choices) = preview_choices(
        &state.you_tube_resolver,
        &state.external_id_registry,
        song.routing(),
    )
    .await
    else {
        return Ok(builder.create_error(FORMAT, 70, NO_SUCH_SONG).into_response());
    };
    Ok(builder
        .create_preview_choices_response(&id, &choices, matches!(song, PreviewSong::Library(_)))
        .into_response())
}

/// Makes the song play from the video (a library song: makes a replace download it), and
/// answers the song as getSong does, an outside song with the video's length when `length`
/// gave it.
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
    let Some(song) = preview_song(&state, &id).await else {
        return Ok(builder.create_error(FORMAT, 70, NO_SUCH_SONG).into_response());
    };
    if !set_preview(&state.external_id_registry, song.routing(), &video, length) {
        return Ok(builder.create_error(FORMAT, 70, NO_SUCH_SONG).into_response());
    }
    // The shim resolves the video now, so the play that follows starts from its cache.
    let youtube = state.you_tube_resolver.clone();
    let warmed = video.clone();
    tokio::spawn(async move {
        let _ = youtube.facts(&warmed, false).await;
    });
    let external_id = match song {
        PreviewSong::Outside(external_id) => external_id,
        // A library song is Navidrome's to describe: its file is unchanged until a replace
        // goes in, so the answer is getSong's for it, as JSON.
        PreviewSong::Library(_) => {
            let mut request = call.parameters.clone();
            request.insert("f".into(), "json".into());
            for own in ["videoId", "length", "callback"] {
                request.shift_remove(own);
            }
            let relayed = call.proxy.relay("rest/getSong", &request).await?;
            let content_type = relayed
                .content_type
                .unwrap_or_else(|| format!("application/{FORMAT}"));
            return Ok(file(relayed.body, &content_type));
        }
    };
    let Some(mut song) = state
        .music_metadata
        .get_song(OutsideMetadataService::PROVIDER_NAME, &external_id)
        .await
    else {
        return Ok(builder.create_error(FORMAT, 70, "Song not found").into_response());
    };
    state.music_metadata.complete_song_year(&mut song).await;
    Ok(builder.create_song_response(FORMAT, &song).into_response())
}
