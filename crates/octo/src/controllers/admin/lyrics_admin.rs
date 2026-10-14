//! The dashboard's lyrics tools. "Find lyrics for the library", its review list, and a picker
//! that chooses, hides or resets one song's lyrics for every client, the same pins the Octo app
//! sets through setLyricsChoice. Everything here names files or changes what every listener
//! sees, so all of it needs a Navidrome admin sign-in, as the genre backfill does (`signed`,
//! which does not re-issue the cookie).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use octo_core::common::dotnet::{eq_ignore_case, escape_data_string, is_null_or_white_space};
use octo_core::lyrics::{LyricsChoiceCandidate, LyricsLibraryMode, LyricsPin, LyricsQuery, LyricsText};
use octo_core::settings::LyricsSaveTo;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::json_body::{FromBody, Kind, Member, Shape, text_record};
use super::request_binding::{
    anonymous, bind_body, error, ok, query, required, sign_in, signed, split, status, utc, utc_opt,
};
use crate::app::AppState;
use crate::http::error::{AppError, validation_problem};
use crate::http::routes::RouteSet;
use crate::services::library::navidrome_song_path_resolver::{get_full_path, is_inside};
use crate::services::lyrics::LyricsChoiceService;
use crate::services::outside::OutsideMetadataService;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route(
            "/api/admin/lyrics/library",
            get(get_library_run).post(start_library_run),
        )
        .route("/api/admin/lyrics/library/cancel", post(cancel_library_run))
        .route("/api/admin/lyrics/library/resume", post(resume_library_run))
        .route("/api/admin/lyrics/review/dismiss", post(dismiss_review))
        .route("/api/admin/lyrics/songs", get(search_songs))
        .route("/api/admin/lyrics/candidates", get(candidates))
        .route("/api/admin/lyrics/choice", post(choose))
        .route("/api/admin/lyrics/choices", get(choices))
}

/// How long the candidates and a choice may take before what was found is answered.
const CANDIDATES_BUDGET: Duration = Duration::from_secs(15);

const MODES: [&str; 5] = ["Walk", "Scan", "Preview", "Save", "Undo"];
const ALREADY_RUNNING: &str = "Lyrics are already being found for the library.";

fn mode_from(index: Option<usize>) -> LyricsLibraryMode {
    match index {
        Some(1) => LyricsLibraryMode::Scan,
        Some(2) => LyricsLibraryMode::Preview,
        Some(3) => LyricsLibraryMode::Save,
        Some(4) => LyricsLibraryMode::Undo,
        _ => LyricsLibraryMode::Walk,
    }
}

fn mode_name(mode: LyricsLibraryMode) -> &'static str {
    MODES[mode as usize]
}

fn status_name(status: octo_core::lyrics::LyricsLibraryStatus) -> &'static str {
    use octo_core::lyrics::LyricsLibraryStatus as S;
    match status {
        S::Idle => "Idle",
        S::Running => "Running",
        S::Completed => "Completed",
        S::Cancelled => "Cancelled",
        S::Interrupted => "Interrupted",
        S::Failed => "Failed",
    }
}

/// A token that cancels itself once its time is up, and stops its timer when dropped.
struct Budget {
    token: CancellationToken,
    timer: tokio::task::JoinHandle<()>,
}

impl Budget {
    fn start(after: Duration) -> Budget {
        let token = CancellationToken::new();
        let cancel = token.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(after).await;
            cancel.cancel();
        });
        Budget { token, timer }
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        self.timer.abort();
    }
}

// ---- Find lyrics for the library ------------------------------------------------------------

/// The run, its rows and the review list.
async fn get_library_run(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    let job = &state.lyrics_library_worker;
    let run = job.current();
    let metadata = state.settings.current().metadata.clone();
    let rows: Vec<Value> = run
        .rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id, "path": row.path, "artist": row.artist, "title": row.title,
                "album": row.album, "has": row.has, "result": row.result, "source": row.source,
                "kind": row.kind, "candidateId": row.candidate_id, "doubt": row.doubt,
                "preview": row.preview,
            })
        })
        .collect();
    let mut review = run.review.clone();
    // Newest first; the sort is stable, so equal times keep their order.
    review.sort_by_key(|entry| std::cmp::Reverse(entry.at_utc));
    let review: Vec<Value> = review
        .iter()
        .map(|entry| {
            json!({
                "path": entry.path, "artist": entry.artist, "title": entry.title,
                "album": entry.album, "durationSeconds": entry.duration_seconds,
                "source": entry.source, "kind": entry.kind, "candidateId": entry.candidate_id,
                "reason": entry.reason, "atUtc": utc(&entry.at_utc),
            })
        })
        .collect();
    let body = anonymous(vec![
        ("runId", json!(run.run_id)),
        ("status", json!(status_name(run.status))),
        ("scope", json!(run.scope)),
        ("upgrade", json!(run.upgrade)),
        ("startedUtc", utc_opt(&run.started_utc)),
        ("finishedUtc", utc_opt(&run.finished_utc)),
        ("total", json!(run.total)),
        ("processed", json!(run.processed)),
        ("written", json!(run.written)),
        ("wordTimed", json!(run.word_timed)),
        ("upgraded", json!(run.upgraded)),
        ("alreadyHad", json!(run.already_had)),
        ("notFound", json!(run.not_found)),
        ("instrumental", json!(run.instrumental)),
        ("busy", json!(run.busy)),
        ("skipped", json!(run.skipped)),
        ("failed", json!(run.failed)),
        ("lastPath", json!(run.last_path)),
        ("reason", json!(run.reason)),
        ("errors", json!(run.errors)),
        ("canResume", json!(run.can_resume())),
        ("mode", json!(mode_name(run.mode))),
        ("wordAlready", json!(run.word_already)),
        ("picked", json!(run.picked.as_ref().map(Vec::len))),
        // What a preview found stays here; the dashboard gets the first lines.
        ("rows", Value::Array(rows)),
        // Not "busy": that is the run's count of songs no service answered for, and two
        // properties of one name cannot be written.
        ("running", json!(job.is_running())),
        ("canUndo", json!(job.can_undo())),
        (
            "saveTo",
            json!(LyricsSaveTo::normalize(Some(&metadata.save_lyrics_to))),
        ),
        ("review", Value::Array(review)),
        ("writesBesideAll", json!(metadata.write_lyrics_beside_all_songs)),
        ("fetching", json!(metadata.fetch_lyrics)),
    ]);
    match body {
        Ok(body) => ok(&body),
        Err(e) => e.into_response(),
    }
}

/// The library start body. `upgrade` is for a walk. `mode` is a step of the lyrics page (Scan,
/// Preview, Save, Undo), `scope` is for a scan, `picked` the rows for Preview and Save.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LibraryStartRequest {
    upgrade: bool,
    mode: Option<String>,
    scope: Option<String>,
    picked: Option<Vec<String>>,
}

impl FromBody for LibraryStartRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.LyricsAdminController+LibraryStartRequest",
        by_constructor: true,
        members: &[
            Member {
                name: "upgrade",
                kind: Kind::Bool,
            },
            Member {
                name: "mode",
                kind: Kind::Text,
            },
            Member {
                name: "scope",
                kind: Kind::Text,
            },
            Member {
                name: "picked",
                kind: Kind::TextList,
            },
        ],
    };
}

async fn start_library_run(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: LibraryStartRequest = match bind_body(&parts, &body) {
        Ok(r) => r,
        Err(answer) => return *answer,
    };
    if !signed(&state, &parts) {
        return sign_in();
    }
    let mode = mode_from(super::request_binding::enum_try_parse(
        &MODES,
        request.mode.as_deref(),
    ));
    let job = &state.lyrics_library_worker;
    // A scan and an undo look nothing up.
    if !state.settings.current().metadata.fetch_lyrics
        && matches!(mode, LyricsLibraryMode::Walk | LyricsLibraryMode::Preview)
    {
        return error(
            StatusCode::BAD_REQUEST,
            "Turn on Fetch lyrics first, or a run would find nothing.",
        );
    }
    if matches!(mode, LyricsLibraryMode::Preview | LyricsLibraryMode::Save)
        && !request.picked.as_ref().is_some_and(|p| !p.is_empty())
    {
        return error(StatusCode::BAD_REQUEST, "Pick at least one song.");
    }
    if mode == LyricsLibraryMode::Undo && !job.can_undo() {
        return error(StatusCode::BAD_REQUEST, "There is nothing to undo.");
    }
    let picked = match &request.picked {
        None => "no pick".to_string(),
        Some(p) => format!("{} picked", p.len()),
    };
    let scope = request.scope.clone();
    let enqueued = job.try_enqueue(octo_core::lyrics::LyricsLibraryRequest {
        upgrade: request.upgrade,
        resume: false,
        mode,
        scope: request.scope,
        picked: request.picked,
    });
    if !enqueued {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    info!(
        "Lyrics for the library requested: {}, scope {}, {picked} (upgrade {})",
        mode_name(mode),
        scope.as_deref().unwrap_or("default"),
        if request.upgrade { "True" } else { "False" }
    );
    status(StatusCode::ACCEPTED, &json!({ "started": true }))
}

async fn cancel_library_run(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    state.lyrics_library_worker.request_cancel();
    status(StatusCode::ACCEPTED, &json!({ "cancelling": true }))
}

async fn resume_library_run(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    let job = &state.lyrics_library_worker;
    let current = job.current();
    if !current.can_resume() {
        return error(StatusCode::BAD_REQUEST, "There is nothing to resume.");
    }
    let enqueued = job.try_enqueue(octo_core::lyrics::LyricsLibraryRequest {
        upgrade: current.upgrade,
        resume: true,
        mode: current.mode,
        ..Default::default()
    });
    if !enqueued {
        return error(StatusCode::CONFLICT, ALREADY_RUNNING);
    }
    status(StatusCode::ACCEPTED, &json!({ "resumed": true }))
}

/// The body of a review dismissal: `Path` is required.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ReviewDismissRequest {
    path: Option<String>,
}

impl FromBody for ReviewDismissRequest {
    const SHAPE: &'static Shape = text_record!(
        "Octo.Controllers.LyricsAdminController+ReviewDismissRequest",
        "path"
    );
}

async fn dismiss_review(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: ReviewDismissRequest = match bind_body(&parts, &body) {
        Ok(r) => r,
        Err(answer) => return *answer,
    };
    if !required(request.path.as_deref()) {
        return validation_problem(&[("Path", &["The Path field is required."])]);
    }
    if !signed(&state, &parts) {
        return sign_in();
    }
    state
        .lyrics_library_worker
        .dismiss_review(request.path.as_deref().unwrap_or_default());
    ok(&json!({ "ok": true }))
}

// ---- Choosing one song's lyrics -------------------------------------------------------------

/// A string member, or None.
fn str_of(element: &Value, name: &str) -> Option<String> {
    element.get(name).and_then(Value::as_str).map(str::to_string)
}

/// A whole-number member that fits in 32 bits, or None.
fn int_of(element: &Value, name: &str) -> Option<i32> {
    element
        .get(name)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
}

/// A Subsonic call as Octo's admin account, or None without one (or when it fails).
async fn admin_subsonic(state: &AppState, endpoint: &str, query: &str) -> Option<Value> {
    let base_url = state.settings.current().subsonic.url.clone();
    if is_null_or_white_space(base_url.as_deref()) {
        return None;
    }
    let base_url = base_url.unwrap_or_default();
    let (user, token, salt) = state.navidrome_identity.get_scan_auth()?;
    let url = format!(
        "{}/rest/{endpoint}?f=json&c=octo&v=1.16.1&{query}&u={}&t={token}&s={salt}",
        base_url.trim_end_matches('/'),
        escape_data_string(&user)
    );
    let answer = async {
        let response = state.http.get(&url).send().await?;
        if !response.status().is_success() {
            return Ok(None);
        }
        let bytes = response.bytes().await?;
        Ok::<_, anyhow::Error>(serde_json::from_slice::<Value>(&bytes).ok())
    }
    .await;
    match answer {
        Ok(doc) => {
            if doc.is_none() {
                debug!("admin {endpoint} failed: not JSON");
            }
            doc
        }
        Err(e) => {
            debug!("admin {endpoint} failed: {e}");
            None
        }
    }
}

/// Library songs by name, as Octo's admin account sees them, each with its current choice, for
/// picking one to fix.
async fn search_songs(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    let Some(q) = query(&parts.uri, "q").filter(|q| !q.trim().is_empty()) else {
        return ok(&json!({ "songs": [] }));
    };
    let query = format!(
        "query={}&songCount=25&albumCount=0&artistCount=0",
        escape_data_string(q.trim())
    );
    let Some(doc) = admin_subsonic(&state, "search3", &query).await else {
        return error(
            StatusCode::BAD_REQUEST,
            "Octo needs a Navidrome admin sign-in to search the library.",
        );
    };
    let list = doc
        .get("subsonic-response")
        .and_then(|e| e.get("searchResult3"))
        .and_then(|r| r.get("song"))
        .and_then(Value::as_array);
    let songs: Vec<Value> = list
        .map(|list| {
            list.iter()
                .map(|song| {
                    let id = str_of(song, "id");
                    let artist = str_of(song, "artist");
                    let title = str_of(song, "title");
                    let choice = state.lyrics_choice_service.choice_for_song(
                        id.as_deref().unwrap_or(""),
                        artist.as_deref(),
                        title.as_deref(),
                    );
                    json!({
                        "id": id,
                        "title": title,
                        "artist": artist,
                        "album": str_of(song, "album"),
                        "duration": int_of(song, "duration"),
                        "choice": choice,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    ok(&json!({ "songs": songs }))
}

/// A song by id or by the file a review entry names.
struct SongRef {
    id: Option<String>,
    path: Option<String>,
    artist: String,
    title: String,
    album: Option<String>,
    duration: Option<i32>,
}

/// A song by id (outside songs from the registry, library songs from Navidrome) or by a file the
/// review list names. A path is only taken from the review list, never as given, so this cannot
/// be pointed at any file on the disk.
async fn song_ref(state: &AppState, id: Option<&str>, path: Option<&str>) -> Option<SongRef> {
    if !is_null_or_white_space(path) {
        let path = path.unwrap_or_default();
        let entry = state
            .lyrics_library_worker
            .current()
            .review
            .into_iter()
            .find(|review| review.path == path)?;
        let worker = state.lyrics_library_worker.clone();
        let entry_path = entry.path.clone();
        let tags = tokio::task::spawn_blocking(move || worker.read_tags(&entry_path))
            .await
            .ok()
            .flatten()?;
        let found = state
            .navidrome_song_path_resolver
            .find_id_by_path(&tags.artist, &tags.title, &entry.path)
            .await;
        return Some(SongRef {
            id: found,
            path: Some(entry.path),
            artist: tags.artist,
            title: tags.title,
            album: tags.album,
            duration: tags.duration_seconds,
        });
    }
    if is_null_or_white_space(id) {
        return None;
    }
    let id = id.unwrap_or_default();

    let routing = state
        .external_id_registry
        .lookup(id)
        .map(|shared| shared.snapshot())
        .or_else(|| OutsideMetadataService::try_decode_external_id(Some(id)));
    if let Some(routing) = routing.filter(|r| r.has_artist_title()) {
        return Some(SongRef {
            id: Some(id.to_string()),
            path: None,
            artist: routing.artist.unwrap_or_default(),
            title: routing.title.unwrap_or_default(),
            album: routing.album,
            duration: routing.duration,
        });
    }

    let doc = admin_subsonic(state, "getSong", &format!("id={}", escape_data_string(id))).await?;
    let song = doc.get("subsonic-response")?.get("song")?;
    let artist = str_of(song, "artist").filter(|a| !a.trim().is_empty())?;
    let title = str_of(song, "title").filter(|t| !t.trim().is_empty())?;
    Some(SongRef {
        id: Some(id.to_string()),
        path: None,
        artist,
        title,
        album: str_of(song, "album"),
        duration: int_of(song, "duration"),
    })
}

/// A candidate as the dashboard reads it.
fn candidate_json(c: &LyricsChoiceCandidate) -> Value {
    json!({
        "id": c.id,
        "source": c.source,
        "title": c.title,
        "artist": c.artist,
        "album": c.album,
        "durationSeconds": c.duration_seconds,
        "kind": c.kind,
        "sameSong": c.same_song,
        "preview": c.preview,
    })
}

/// Every lyrics entry for one song, by its id (a library or an outside song), or by the file a
/// review entry names. artist and title, when given, search for those instead.
async fn candidates(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    if !state.settings.current().metadata.fetch_lyrics {
        return error(StatusCode::BAD_REQUEST, "Turn on Fetch lyrics first.");
    }
    let id = query(&parts.uri, "id");
    let path = query(&parts.uri, "path");
    let artist = query(&parts.uri, "artist").filter(|a| !a.trim().is_empty());
    let title = query(&parts.uri, "title").filter(|t| !t.trim().is_empty());

    let Some(song) = song_ref(&state, id.as_deref(), path.as_deref()).await else {
        return error(StatusCode::NOT_FOUND, "Octo could not find that song.");
    };
    let query_artist = artist
        .as_deref()
        .map_or(song.artist.as_str(), str::trim)
        .to_string();
    let query_title = LyricsText::query_title(
        title.as_deref().map_or(song.title.as_str(), str::trim),
        &query_artist,
    );
    let lyrics_query = LyricsQuery::new(query_artist, query_title, song.album.clone(), song.duration);
    let budget = Budget::start(CANDIDATES_BUDGET);
    let found = state
        .lyrics_choice_service
        .candidates(&lyrics_query, &budget.token)
        .await;
    let choice = match &song.id {
        None => LyricsPin::AUTO.to_string(),
        Some(id) => state
            .lyrics_choice_service
            .choice_for_song(id, Some(&song.artist), Some(&song.title)),
    };
    ok(&json!({
        "id": song.id,
        "path": song.path,
        "artist": song.artist,
        "title": song.title,
        "album": song.album,
        "duration": song.duration,
        "choice": choice,
        "candidates": found.iter().map(candidate_json).collect::<Vec<_>>(),
    }))
}

/// The choice body: an optional `id` and `path`, and the `candidate`, which is required.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ChoiceRequest {
    id: Option<String>,
    path: Option<String>,
    candidate: Option<String>,
}

impl FromBody for ChoiceRequest {
    const SHAPE: &'static Shape = text_record!(
        "Octo.Controllers.LyricsAdminController+ChoiceRequest",
        "id",
        "path",
        "candidate"
    );
}

/// The download rule: beside Octo's own downloads, and beside everything only when "Write lyrics
/// files beside all library songs" is on.
async fn may_write_beside(state: &AppState, path: &str) -> bool {
    if !is_inside(
        &get_full_path(path),
        &state.navidrome_song_path_resolver.music_root(),
    ) {
        return false;
    }
    if state.settings.current().metadata.write_lyrics_beside_all_songs {
        return true;
    }
    state
        .local_library
        .get_mappings()
        .await
        .iter()
        .any(|mapping| eq_ignore_case(&mapping.local_path, path))
}

/// Set one song's lyrics: a candidate, "none" to show none, or "auto". The choice is a pin every
/// client sees. From the review list it also rewrites the lyrics file, but only one Octo may
/// write (its own, beside a song it is allowed to write beside).
async fn choose(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: ChoiceRequest = match bind_body(&parts, &body) {
        Ok(r) => r,
        Err(answer) => return *answer,
    };
    if !required(request.candidate.as_deref()) {
        return validation_problem(&[("Candidate", &["The Candidate field is required."])]);
    }
    if !signed(&state, &parts) {
        return sign_in();
    }
    let candidate = request
        .candidate
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    if candidate.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "Say which lyrics: a candidate, none or auto.",
        );
    }
    let Some(song) = song_ref(&state, request.id.as_deref(), request.path.as_deref()).await else {
        return error(StatusCode::NOT_FOUND, "Octo could not find that song.");
    };
    let choices: &Arc<LyricsChoiceService> = &state.lyrics_choice_service;

    if eq_ignore_case(&candidate, LyricsPin::AUTO) {
        if let Some(id) = &song.id {
            choices.clear_song(id, Some(&song.artist), Some(&song.title));
        }
        return ok(&json!({ "id": song.id, "choice": LyricsPin::AUTO }));
    }
    if eq_ignore_case(&candidate, LyricsPin::HIDDEN) {
        let Some(id) = &song.id else {
            return error(
                StatusCode::BAD_REQUEST,
                "Navidrome has not scanned this song yet, so it cannot be hidden. Try again after a scan.",
            );
        };
        choices.hide(id, Some(&song.artist), Some(&song.title), Some("dashboard"));
        if let Some(path) = &song.path {
            state.lyrics_library_worker.dismiss_review(path);
        }
        return ok(&json!({ "id": song.id, "choice": LyricsPin::HIDDEN }));
    }

    if !state.settings.current().metadata.fetch_lyrics {
        return error(StatusCode::BAD_REQUEST, "Turn on Fetch lyrics first.");
    }
    let budget = Budget::start(CANDIDATES_BUDGET);
    let Some(lyrics) = choices.lyrics_of(&candidate, &budget.token).await else {
        return error(
            StatusCode::NOT_FOUND,
            "Those lyrics could not be found; search again.",
        );
    };
    let pinned = match &song.id {
        Some(id) => {
            choices
                .pin(
                    id,
                    &candidate,
                    Some(&song.artist),
                    Some(&song.title),
                    Some("dashboard"),
                    &budget.token,
                )
                .await
        }
        None => false,
    };
    let rewrote = match &song.path {
        Some(path) if may_write_beside(&state, path).await => {
            match state
                .lyrics_sidecar_writer
                .replace(Path::new(path), &lyrics, &budget.token)
                .await
            {
                Ok(rewrote) => rewrote,
                Err(e) => return AppError::Internal(e).into_response(),
            }
        }
        _ => false,
    };
    if !pinned && !rewrote {
        return error(
            StatusCode::BAD_REQUEST,
            "Navidrome has not scanned this song yet and its lyrics file is not Octo's to change.",
        );
    }
    if let Some(path) = &song.path {
        state.lyrics_library_worker.dismiss_review(path);
    }
    ok(&json!({ "id": song.id, "choice": candidate, "pinned": pinned, "rewroteFile": rewrote }))
}

/// Every song whose lyrics someone chose or hid, newest first.
async fn choices(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if !signed(&state, &parts) {
        return sign_in();
    }
    let choices: Vec<Value> = state
        .lyrics_choice_service
        .all()
        .iter()
        .map(|pin| {
            let kind = if pin.is_hidden() {
                "hidden"
            } else {
                pin.lyrics()
                    .map_or("instrumental", |l| LyricsChoiceService::kind_of(&l))
            };
            json!({
                "id": pin.song_id,
                "artist": pin.artist,
                "title": pin.title,
                "choice": pin.choice,
                "source": pin.source,
                "kind": kind,
                "setBy": pin.set_by,
                "setUtc": utc(&pin.set_utc),
            })
        })
        .collect();
    ok(&json!({ "choices": choices }))
}
