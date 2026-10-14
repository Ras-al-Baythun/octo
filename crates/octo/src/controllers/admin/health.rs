//! The dashboard's Library health, what the Octo app's Library health finds, worked out on the
//! server, and its fixes, for people on other Subsonic clients. Admins only: behind the dashboard
//! sign-in like every admin endpoint, every call needs a Navidrome admin (the recovery code is
//! not one, and the gate's `LIBRARY_FILE_ROUTES` refuses it first), and the writes act as that
//! admin, refused unless they are on the library actions allowed list, exactly as the app's
//! libraryAction is. Writes carry X-Octo-Admin, as every dashboard write does.

use std::collections::{HashMap, HashSet};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::{get, post};
use indexmap::IndexMap;
use octo_core::common::dotnet::{eq_ignore_case, is_blank, to_lower_invariant, utf16_len};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::info;

use super::json_body::{FromBody, Kind, Member, Shape};
use super::plumbing::{BROWSE_COOKIE_NAME, accepted, ok, request_cookie};
use super::request_binding::{SIGN_IN_FIRST, bind_body, error, query, split, status, utc, utc_opt};
use crate::app::AppState;
use crate::http::routes::RouteSet;
use crate::services::admin::{AdminCaller, AdminCallerKind};
use crate::services::health::health_fixes::{
    self as fixes, AlbumJoin, DuplicateFix, FixStep, TagChange, fix_actions,
};
use crate::services::health::health_words as words;
use crate::services::health::keep_marks::{self, TargetPick};
use crate::services::health::library_health::{
    self as health, DuplicateSet, HealthCheck, HealthTag, SplitAlbum,
};
use crate::services::health::library_health_service::Row;
use crate::services::health::{HealthRun, IHealthFields, LibraryHealthService, ServerSongFields};
use crate::services::library::library_tag_edits::song_tag_fields;
use crate::services::library::library_write_gate::LibraryWriteGate;
use crate::services::library::quality_upgrade_worker::compare_ordinal;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/health", get(get_report))
        .route("/api/admin/health/run", get(get_run))
        .route("/api/admin/health/preview", post(preview))
        .route("/api/admin/health/apply", post(apply))
        .route("/api/admin/health/lookup", post(lookup))
        .route("/api/admin/health/stop", post(stop))
        .route("/api/admin/health/undo", post(undo))
}

/// The Navidrome admin asking: signed in on the dashboard, or the app with an admin's sign-in.
fn navidrome_user(state: &AppState, parts: &Parts) -> Option<String> {
    if let Some(AdminCaller {
        user: Some(user),
        kind: AdminCallerKind::Dashboard | AdminCallerKind::NavidromeAdmin,
        ..
    }) = parts.extensions.get::<AdminCaller>()
    {
        return Some(user.clone());
    }
    let cookie = request_cookie(&parts.headers, BROWSE_COOKIE_NAME);
    state.browse_sessions.navidrome_user_of(cookie.as_deref())
}

// As every endpoint that reads or changes library files answers the recovery code: no.
fn not_admin() -> Response {
    error(StatusCode::UNAUTHORIZED, SIGN_IN_FIRST)
}

type Fields = ServerSongFields;

fn fields_of(fields: &Fields) -> &dyn IHealthFields<Row> {
    fields
}

// ---- reading ----------------------------------------------------------------------------

/// The findings, grouped, with counts, plain-words reasons, paths, and what can be fixed.
async fn get_report(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let Some(user) = navidrome_user(&state, &parts) else {
        return not_admin();
    };
    // "Check again": read the library from Navidrome now. 1 or true.
    let fresh = matches!(query(&parts.uri, "fresh").as_deref(), Some("1" | "true" | "True"));
    let health = state.library_health.clone();
    let Some(found) = health.report(fresh).await else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Octo could not read your library from Navidrome. It needs a Navidrome admin sign-in under Music server.",
        );
    };
    let can = health.abilities(&user);
    let report = &found.report;
    let fields = fields_of(&found.fields);
    let mut used: HashSet<String> = HashSet::new();
    let mut use_song = |song: &Row| -> Value {
        used.insert(song.id.clone());
        Value::String(song.id.clone())
    };

    let mut checks: Vec<Value> = Vec::new();
    for check in report.findings() {
        let count = report.count(check);
        let mut item = Map::new();
        item.insert("check".into(), json!(check_name(check)));
        item.insert("title".into(), json!(words::title(check)));
        item.insert("count".into(), json!(count));
        item.insert("countLabel".into(), json!(words::count_label(check, count)));
        item.insert("meaning".into(), json!(words::meaning(check)));
        item.insert("advice".into(), json!(words::advice(check)));
        item.insert("fixMeaning".into(), json!(words::fix_meaning(check)));
        match check {
            HealthCheck::Duplicates => {
                item.insert("fixAllLabel".into(), json!(words::fix_all_label(check, count)));
                // The list shows the best-sound plan; the preview reads people's marks.
                let marks_note = (can.edit && can.remove && health.keep_marks().is_some())
                    .then_some(words::MARKS_PICKED_IN_PREVIEW);
                let sets: Vec<Value> = report
                    .duplicates
                    .iter()
                    .map(|set| {
                        let fix = fixes::duplicate_fix(set, fields, None);
                        let copies: Vec<Value> = set.copies.iter().map(&mut use_song).collect();
                        let fills: Vec<Value> = if can.edit {
                            fix.fills.iter().map(change_view).collect()
                        } else {
                            Vec::new()
                        };
                        json!({
                            "key": set_key(set),
                            "heading": words::set_heading(set, fields),
                            "summary": words::set_summary(set, fields),
                            "basis": words::basis_words(set.basis),
                            "copies": copies,
                            "keep": fix.keep.id,
                            "why": fix.why,
                            "line": fix_line(&fix, &fix.fills, fields, can.edit),
                            "fills": fills,
                            "note": join_notes(fix.note.as_deref(), marks_note),
                        })
                    })
                    .collect();
                item.insert("sets".into(), Value::Array(sets));
            }
            HealthCheck::SplitAlbums => {
                item.insert("fixAllLabel".into(), json!(words::fix_all_label(check, count)));
                let albums: Vec<Value> = report
                    .split_albums
                    .iter()
                    .map(|album| {
                        let join = fixes::album_join(album);
                        let parts: Vec<Value> = album
                            .parts
                            .iter()
                            .map(|part| {
                                let songs: Vec<Value> = part.songs.iter().map(&mut use_song).collect();
                                json!({ "albumId": part.album_id, "songs": songs })
                            })
                            .collect();
                        json!({
                            "key": album_key(album),
                            "heading": words::album_heading(album),
                            "summary": words::album_summary(album),
                            "reasons": album.reasons.iter().map(words::reason_words).collect::<Vec<_>>(),
                            "differences": album.differences.iter().map(words::difference_words).collect::<Vec<_>>(),
                            "parts": parts,
                            "join": { "words": join.words(), "lead": join.lead.id, "moving": join.moving.len() },
                        })
                    })
                    .collect();
                item.insert("albums".into(), Value::Array(albums));
            }
            _ => {
                let songs = report.songs(check);
                let ids: Vec<Value> = songs.iter().map(&mut use_song).collect();
                item.insert("songs".into(), Value::Array(ids));
                match health::tag_of(check) {
                    Some(tag @ (HealthTag::Year | HealthTag::Genre | HealthTag::AlbumArtist)) => {
                        let fills = fixes::fills_from_album(&songs, &found.songs, tag, fields);
                        let views: Vec<Value> = fills
                            .iter()
                            .map(|(song, change)| {
                                json!({
                                    "id": song.id,
                                    "tag": change.tag,
                                    "value": change.value,
                                    "words": change.words(),
                                })
                            })
                            .collect();
                        item.insert("fills".into(), Value::Array(views));
                        item.insert(
                            "fixAllLabel".into(),
                            if fills.is_empty() {
                                Value::Null
                            } else {
                                json!(words::fix_all_label(check, fills.len()))
                            },
                        );
                    }
                    _ => {
                        item.insert(
                            "fixAllLabel".into(),
                            json!(words::fix_all_label(check, songs.len())),
                        );
                    }
                }
            }
        }
        checks.push(Value::Object(item));
    }

    let mut by_id = Map::new();
    for song in found.songs.iter().filter(|song| used.contains(&song.id)) {
        by_id.insert(song.id.clone(), song_view(song, fields));
    }
    ok(&json!({
        "checked": report.checked,
        "overview": words::overview(report),
        "clean": report.clean(),
        "allClear": words::ALL_CLEAR,
        "readUtc": utc(&found.read_utc),
        "user": user,
        "can": serde_json::to_value(&can).unwrap_or(Value::Null),
        "checks": checks,
        "songs": by_id,
        "run": run_view(health.run().as_deref()),
        "undoCount": health.undo_count(),
    }))
}

/// The run going now or last: how far, and how it went.
async fn get_run(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if navidrome_user(&state, &parts).is_none() {
        return not_admin();
    }
    let health = &state.library_health;
    ok(&json!({
        "run": run_view(health.run().as_deref()),
        "undoCount": health.undo_count(),
        "checking": health.checking_since().is_some(),
    }))
}

// ---- previews ---------------------------------------------------------------------------

/// `PreviewRequest(string? Check, List<string>? Keys, string? Keep, List<string>? Fills,
/// Dictionary<string, string>? Choose)`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PreviewRequest {
    check: Option<String>,
    keys: Option<Vec<String>>,
    keep: Option<String>,
    fills: Option<Vec<String>>,
    choose: Option<IndexMap<String, String>>,
}

impl FromBody for PreviewRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.LibraryHealthController+PreviewRequest",
        by_constructor: true,
        members: &[
            Member {
                name: "check",
                kind: Kind::Text,
            },
            Member {
                name: "keys",
                kind: Kind::TextList,
            },
            Member {
                name: "keep",
                kind: Kind::Text,
            },
            Member {
                name: "fills",
                kind: Kind::TextList,
            },
            Member {
                name: "choose",
                kind: Kind::TextMap,
            },
        ],
    };
}

/// What a fix would do, before anything changes: the same plans the app makes. Duplicates keep
/// the best copy (or `keep`) with the reason, fill blanks from the copies (only the tags in
/// `fills` when given) and take the values picked in `choose`; split albums join onto their lead
/// part; missing year, genre and album artist fill from the album; covers are looked for. `keys`
/// narrows it to some sets, albums or songs. Writes nothing.
async fn preview(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: PreviewRequest = match bind_body(&parts, &body) {
        Ok(request) => request,
        Err(answer) => return *answer,
    };
    let Some(user) = navidrome_user(&state, &parts) else {
        return not_admin();
    };
    let Some(check) = parse_check(request.check.as_deref()) else {
        return error(StatusCode::BAD_REQUEST, "Say which finding to fix.");
    };
    let health = state.library_health.clone();
    let Some(found) = health.report(false).await else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Octo could not read your library from Navidrome.",
        );
    };
    let can = health.abilities(&user);
    let report = &found.report;
    let keys: Option<HashSet<&str>> = request
        .keys
        .as_ref()
        .filter(|keys| !keys.is_empty())
        .map(|keys| keys.iter().map(String::as_str).collect());
    let picked = |key: &str| keys.as_ref().is_none_or(|keys| keys.contains(key));
    // Which copy of each set keeps people's marks, read from Navidrome now, unless a person
    // picked the copy to keep.
    let keeping = can.edit && can.remove && check == HealthCheck::Duplicates;
    let mut marked: HashMap<String, TargetPick> = HashMap::new();
    if let Some(keep) = health.keep_marks().filter(|_| keeping) {
        let sets: Vec<&DuplicateSet<Row>> = report
            .duplicates
            .iter()
            .filter(|set| picked(&set_key(set)))
            .collect();
        if !(sets.len() == 1 && request.keep.is_some()) {
            let uses = keep.playlist_uses().await.unwrap_or_default();
            for set in sets {
                if let Some(pick) = keep.pick_for(set, &uses).await {
                    marked.insert(set_key(set), pick);
                }
            }
        }
    }
    let fields = fields_of(&found.fields);

    match check {
        HealthCheck::Duplicates => {
            let sets: Vec<&DuplicateSet<Row>> = report
                .duplicates
                .iter()
                .filter(|set| picked(&set_key(set)))
                .collect();
            if sets.is_empty() {
                return gone();
            }
            let one = sets.len() == 1;
            let plans: Vec<(&DuplicateSet<Row>, DuplicateFix<Row>, Vec<TagChange>)> = sets
                .iter()
                .map(|set| {
                    let keep = match (&request.keep, one) {
                        (Some(keep_id), true) => set.copies.iter().find(|copy| &copy.id == keep_id),
                        _ => None,
                    };
                    let fix = match (keep, marked.get(&set_key(set))) {
                        (None, Some(pick)) => keep_marks::plan(set, fields, Some(pick)),
                        _ => fixes::duplicate_fix(set, fields, keep),
                    };
                    let fills: Vec<TagChange> = if !can.edit {
                        Vec::new()
                    } else if let (true, Some(tags)) = (one, &request.fills) {
                        fix.fills
                            .iter()
                            .filter(|change| tags.contains(&change.tag))
                            .cloned()
                            .collect()
                    } else {
                        fix.fills.clone()
                    };
                    let chosen: Vec<TagChange> = if !can.edit || !one {
                        Vec::new()
                    } else {
                        fix.differs
                            .iter()
                            .filter_map(|choice| {
                                let now = choice
                                    .values
                                    .iter()
                                    .find(|(id, _)| *id == fix.keep.id)
                                    .map(|(_, value)| value.as_str());
                                let value = request.choose.as_ref()?.get(&choice.tag)?;
                                (Some(value.as_str()) != now
                                    && choice.values.iter().any(|(_, other)| other == value))
                                .then(|| TagChange::new(&choice.tag, now, value))
                            })
                            .collect()
                    };
                    let changes: Vec<TagChange> = fills.into_iter().chain(chosen).collect();
                    (*set, fix, changes)
                })
                .collect();
            let steps: Vec<Value> = plans
                .iter()
                .flat_map(|(_, fix, changes)| fixes::duplicate_steps(fix, fields, Some(changes)))
                .map(|step| step_view(&step))
                .collect();
            let removing: usize = plans
                .iter()
                .map(|(_, fix, _)| fix.remove.len() + usize::from(fix.audio.is_some()))
                .sum();
            let set_views: Vec<Value> = plans
                .iter()
                .map(|(set, fix, changes)| {
                    let fills: Vec<Value> = if can.edit {
                        fix.fills.iter().map(change_view).collect()
                    } else {
                        Vec::new()
                    };
                    let differs: Vec<Value> = if can.edit {
                        fix.differs
                            .iter()
                            .map(|choice| {
                                let picked = request
                                    .choose
                                    .as_ref()
                                    .and_then(|choose| choose.get(&choice.tag))
                                    .cloned()
                                    .or_else(|| {
                                        choice
                                            .values
                                            .iter()
                                            .find(|(id, _)| *id == fix.keep.id)
                                            .map(|(_, value)| value.clone())
                                    });
                                let values: Vec<Value> = choice
                                    .values
                                    .iter()
                                    .map(|(id, value)| json!({ "id": id, "value": value }))
                                    .collect();
                                json!({
                                    "tag": choice.tag,
                                    "label": fixes::tag_name(&choice.tag),
                                    "values": values,
                                    "picked": picked,
                                })
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };
                    let copies: Vec<Value> = set.copies.iter().map(|copy| song_view(copy, fields)).collect();
                    json!({
                        "key": set_key(set),
                        "heading": words::set_heading(set, fields),
                        "why": fix.why,
                        "line": fix_line(fix, changes, fields, can.edit),
                        "note": fix.note,
                        "keep": fix.keep.id,
                        "copies": copies,
                        "fills": fills,
                        "differs": differs,
                        "diff": if one { duplicate_diff(fix, changes, fields) } else { Value::Null },
                    })
                })
                .collect();
            ok(&json!({
                "title": if one {
                    "Fix these copies".to_string()
                } else {
                    format!("Fix {} you have twice", words::count_text(sets.len(), "song", "songs"))
                },
                "meaning": words::fix_meaning(check),
                "go": format!("Move {} to the trash", words::count_text(removing, "copy", "copies")),
                "label": "Fixing copies",
                "canFix": can.remove,
                "why": if can.remove {
                    None
                } else {
                    Some(can.why.clone().unwrap_or_else(|| {
                        "Removing songs is off: turn on the Delete action under Library actions.".to_string()
                    }))
                },
                "sets": set_views,
                "steps": steps,
            }))
        }
        HealthCheck::SplitAlbums => {
            let albums: Vec<&SplitAlbum<Row>> = report
                .split_albums
                .iter()
                .filter(|album| picked(&album_key(album)))
                .collect();
            if albums.is_empty() {
                return gone();
            }
            let joins: Vec<AlbumJoin<'_, Row>> =
                albums.iter().map(|album| fixes::album_join(album)).collect();
            let moving: usize = joins.iter().map(|join| join.moving.len()).sum();
            let album_views: Vec<Value> = joins
                .iter()
                .map(|join| {
                    json!({
                        "key": album_key(join.album),
                        "heading": words::album_heading(join.album),
                        "words": join.words(),
                        "reasons": join.album.reasons.iter().map(words::reason_words).collect::<Vec<_>>(),
                        "diff": join_diff(join, fields),
                    })
                })
                .collect();
            let steps: Vec<Value> = joins
                .iter()
                .flat_map(|join| fixes::join_steps(join, fields))
                .map(|step| step_view(&step))
                .collect();
            ok(&json!({
                "title": if joins.len() == 1 {
                    "Join this album".to_string()
                } else {
                    format!("Join {}", words::count_text(joins.len(), "album", "albums"))
                },
                "meaning": words::fix_meaning(check),
                "go": format!("Move {}", words::count_text(moving, "song", "songs")),
                "label": "Joining albums",
                "canFix": can.join_albums,
                "why": if can.join_albums { None } else { can.why.clone() },
                "albums": album_views,
                "steps": steps,
            }))
        }
        HealthCheck::NoYear | HealthCheck::NoGenre | HealthCheck::NoAlbumArtist => {
            let Some(tag) = health::tag_of(check) else {
                return gone();
            };
            let songs: Vec<Row> = report
                .songs(check)
                .into_iter()
                .filter(|song| picked(&song.id))
                .collect();
            let fills = fixes::fills_from_album(&songs, &found.songs, tag, fields);
            if fills.is_empty() {
                return ok(&json!({
                    "title": words::title(check),
                    "meaning": words::fix_meaning(check),
                    "canFix": false,
                    "why": "The rest of the album does not agree on it, so it can only be looked up.",
                    "steps": [],
                }));
            }
            let fill_in = format!("Fill in {}", words::count_text(fills.len(), "song", "songs"));
            let lines: Vec<Value> = fills
                .iter()
                .map(|(song, change)| json!({ "id": song.id, "title": song.title, "words": change.words() }))
                .collect();
            let steps: Vec<Value> = fills
                .iter()
                .map(|(song, change)| step_view(&FixStep::retag_with(&song.id, &song.title, [change])))
                .collect();
            ok(&json!({
                "title": fill_in,
                "meaning": words::fix_meaning(check),
                "go": fill_in,
                "label": "Filling in tags",
                "canFix": can.edit,
                "why": if can.edit { None } else { can.why.clone() },
                "lines": lines,
                "steps": steps,
            }))
        }
        HealthCheck::NoCover => {
            let songs: Vec<Row> = report
                .songs(check)
                .into_iter()
                .filter(|song| picked(&song.id))
                .collect();
            if songs.is_empty() {
                return gone();
            }
            let label = words::fix_all_label(check, songs.len());
            ok(&json!({
                "title": label,
                "meaning": words::fix_meaning(check),
                "go": label,
                "label": "Finding covers",
                "canFix": can.add_cover,
                "why": if can.add_cover { None } else { can.why.clone() },
                "lines": songs.iter().map(song_line).collect::<Vec<_>>(),
                "steps": songs
                    .iter()
                    .map(|song| step_view(&FixStep::new(fix_actions::COVER, &song.id, &song.title)))
                    .collect::<Vec<_>>(),
            }))
        }
        HealthCheck::NoLength => {
            let songs: Vec<Row> = report
                .songs(check)
                .into_iter()
                .filter(|song| picked(&song.id))
                .collect();
            if songs.is_empty() {
                return gone();
            }
            let label = words::fix_all_label(check, songs.len());
            // Better quality's own queue does this, with its own gates; the page posts these there.
            let upgrade: Vec<Value> = songs
                .iter()
                .map(|song| {
                    json!({
                        "navidromeId": song.id,
                        "title": song.title,
                        "artist": song.artist,
                        "album": song.album,
                    })
                })
                .collect();
            ok(&json!({
                "title": label,
                "meaning": words::fix_meaning(check),
                "go": label,
                "canFix": can.upgrade,
                "why": if can.upgrade {
                    None
                } else {
                    Some(can.why.clone().unwrap_or_else(|| {
                        "Turn on the Better quality action under Library actions, and set up Lidarr."
                            .to_string()
                    }))
                },
                "lines": songs.iter().map(song_line).collect::<Vec<_>>(),
                "upgrade": upgrade,
                "steps": [],
            }))
        }
        HealthCheck::NoTrackNumber => error(
            StatusCode::BAD_REQUEST,
            "Songs with no track number are looked up, not filled in: use lookup.",
        ),
    }
}

fn gone() -> Response {
    error(
        StatusCode::NOT_FOUND,
        "That finding is no longer in your library. Check again to see what is left.",
    )
}

// ---- writes -----------------------------------------------------------------------------

/// `StepRequest(string? Action, string? Id, string? Title, Dictionary<string, string>? With)`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct StepRequest {
    action: Option<String>,
    id: Option<String>,
    title: Option<String>,
    with: Option<IndexMap<String, String>>,
}

/// The shape a step's body is read as.
const STEP_REQUEST: &Shape = &Shape {
    name: "Octo.Controllers.LibraryHealthController+StepRequest",
    by_constructor: true,
    members: &[
        Member {
            name: "action",
            kind: Kind::Text,
        },
        Member {
            name: "id",
            kind: Kind::Text,
        },
        Member {
            name: "title",
            kind: Kind::Text,
        },
        Member {
            name: "with",
            kind: Kind::TextMap,
        },
    ],
};

/// `ApplyRequest(string? Label, string? Check, List<StepRequest>? Steps)`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ApplyRequest {
    label: Option<String>,
    check: Option<String>,
    steps: Option<Vec<StepRequest>>,
}

impl FromBody for ApplyRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.LibraryHealthController+ApplyRequest",
        by_constructor: true,
        members: &[
            Member {
                name: "label",
                kind: Kind::Text,
            },
            Member {
                name: "check",
                kind: Kind::Text,
            },
            Member {
                name: "steps",
                kind: Kind::RecordList(STEP_REQUEST),
            },
        ],
    };
}

/// Runs the steps a preview gave (or a person picked from a lookup), one song at a time, as the
/// signed-in admin. 202 when it started; 409 while another run is going.
async fn apply(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: ApplyRequest = match bind_body(&parts, &body) {
        Ok(request) => request,
        Err(answer) => return *answer,
    };
    let Some(user) = navidrome_user(&state, &parts) else {
        return not_admin();
    };
    let health = state.library_health.clone();
    if let Some(refused) = health.refusal(Some(&user)) {
        return error(StatusCode::FORBIDDEN, refused);
    }
    if let Some(busy) = job_writing(&state) {
        return busy;
    }
    let steps: Vec<FixStep> = request
        .steps
        .unwrap_or_default()
        .into_iter()
        .map(|step| {
            let id = step.id.clone().unwrap_or_default();
            let title = match step.title.as_deref() {
                Some(title) if !is_blank(title) => title.trim().to_string(),
                _ => id.clone(),
            };
            FixStep {
                action: step.action.unwrap_or_default().trim().to_string(),
                id: id.trim().to_string(),
                title,
                with: step.with.filter(|with| !with.is_empty()),
            }
        })
        .collect();
    if let Some(invalid) = LibraryHealthService::invalid(&steps) {
        return error(StatusCode::BAD_REQUEST, invalid);
    }
    let label = match request.label.as_deref() {
        Some(label) if !is_blank(label) => label.trim().to_string(),
        _ => "Fixing your library".to_string(),
    };
    let label = first_utf16(&label, 80);
    if let Some(busy) = health.start_fix(&label, parse_check(request.check.as_deref()), steps, &user) {
        return busy_answer(&busy);
    }
    accepted(&json!({ "started": true, "run": run_view(health.run().as_deref()) }))
}

/// `LookupRequest(List<string>? Ids, string? Check)`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LookupRequest {
    ids: Option<Vec<Option<String>>>,
    check: Option<String>,
}

impl FromBody for LookupRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.LibraryHealthController+LookupRequest",
        by_constructor: true,
        members: &[
            Member {
                name: "ids",
                kind: Kind::TextList,
            },
            Member {
                name: "check",
                kind: Kind::Text,
            },
        ],
    };
}

/// Looks songs up the way a download is tagged, and writes nothing: the songs named, or with
/// none named, every song the check found. What it found comes back in the run as it goes, each
/// change marked picked or not, for a person to choose.
async fn lookup(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: LookupRequest = match bind_body(&parts, &body) {
        Ok(request) => request,
        Err(answer) => return *answer,
    };
    let Some(user) = navidrome_user(&state, &parts) else {
        return not_admin();
    };
    let health = state.library_health.clone();
    if let Some(refused) = health.refusal(Some(&user)) {
        return error(StatusCode::FORBIDDEN, refused);
    }
    let check = parse_check(request.check.as_deref());
    let mut ids: Vec<String> = Vec::new();
    for id in request.ids.unwrap_or_default().into_iter().flatten() {
        if !is_blank(&id) && !ids.contains(&id) {
            ids.push(id);
        }
    }
    let found = health.report(false).await;
    if ids.is_empty()
        && let (Some(check), Some(found)) = (check, found.as_ref())
    {
        ids = found
            .report
            .songs(check)
            .iter()
            .map(|song| song.id.clone())
            .collect();
    }
    if ids.is_empty() {
        return error(StatusCode::BAD_REQUEST, "No songs picked.");
    }
    if ids.len() > LibraryHealthService::MAX_STEPS {
        return error(
            StatusCode::BAD_REQUEST,
            format!(
                "At most {} songs at a time.",
                words::n0(LibraryHealthService::MAX_STEPS as i64)
            ),
        );
    }
    let titles: HashMap<&str, &str> = found
        .as_ref()
        .map(|found| {
            found
                .songs
                .iter()
                .map(|song| (song.id.as_str(), song.title.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let songs: Vec<(String, String)> = ids
        .iter()
        .map(|id| {
            (
                id.clone(),
                titles
                    .get(id.as_str())
                    .map_or_else(|| id.clone(), |title| (*title).to_string()),
            )
        })
        .collect();
    if let Some(busy) = health.start_lookup(songs, check, &user) {
        return busy_answer(&busy);
    }
    info!("{user} is looking up {} song(s) from Library health", ids.len());
    accepted(&json!({ "started": true, "run": run_view(health.run().as_deref()) }))
}

/// Ends the run going now after the song it is on.
async fn stop(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    if navidrome_user(&state, &parts).is_none() {
        return not_admin();
    }
    ok(&json!({ "stopping": state.library_health.stop() }))
}

/// Puts back everything the last finished run did: removed copies out of the trash, tags, albums
/// and covers as they were (each only while the file has not changed since).
async fn undo(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let Some(user) = navidrome_user(&state, &parts) else {
        return not_admin();
    };
    let health = state.library_health.clone();
    if let Some(refused) = health.refusal(Some(&user)) {
        return error(StatusCode::FORBIDDEN, refused);
    }
    if let Some(busy) = job_writing(&state) {
        return busy;
    }
    if let Some(why) = health.start_undo(&user) {
        return error(StatusCode::CONFLICT, why);
    }
    accepted(&json!({ "started": true, "run": run_view(health.run().as_deref()) }))
}

/// 409 while a job (the tidy, a re-tag, the cover upgrade) writes the music folder: a fix run
/// changes files too, and what it checks would not hold while they move.
fn job_writing(state: &AppState) -> Option<Response> {
    state
        .library_write_gate
        .job()
        .map(|holder| error(StatusCode::CONFLICT, LibraryWriteGate::busy_message(&holder)))
}

fn busy_answer(busy: &HealthRun) -> Response {
    let progress = busy.progress();
    status(
        StatusCode::CONFLICT,
        &json!({
            "error": format!(
                "{} is still going ({} of {}). Try again once it is done.",
                busy.label, progress.done, busy.total
            ),
            "run": run_view(Some(busy)),
        }),
    )
}

/// `label[..80]`: the first 80 UTF-16 units.
fn first_utf16(text: &str, units: usize) -> String {
    if utf16_len(text) <= units {
        return text.to_string();
    }
    let kept: Vec<u16> = text.encode_utf16().take(units).collect();
    String::from_utf16_lossy(&kept)
}

// ---- shapes -----------------------------------------------------------------------------

pub fn check_name(check: HealthCheck) -> &'static str {
    match check {
        HealthCheck::Duplicates => "duplicates",
        HealthCheck::SplitAlbums => "splitAlbums",
        HealthCheck::NoLength => "noLength",
        HealthCheck::NoTrackNumber => "noTrackNumber",
        HealthCheck::NoAlbumArtist => "noAlbumArtist",
        HealthCheck::NoCover => "noCover",
        HealthCheck::NoYear => "noYear",
        HealthCheck::NoGenre => "noGenre",
    }
}

pub fn parse_check(name: Option<&str>) -> Option<HealthCheck> {
    let name = name?;
    HealthCheck::ALL
        .into_iter()
        .find(|check| eq_ignore_case(check_name(*check), name))
}

fn sorted_join<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let mut values: Vec<&str> = values.collect();
    values.sort_by(|a, b| compare_ordinal(a, b));
    values.join(",")
}

fn set_key(set: &DuplicateSet<Row>) -> String {
    format!(
        "dup|{}",
        sorted_join(set.copies.iter().map(|copy| copy.id.as_str()))
    )
}

fn album_key(album: &SplitAlbum<Row>) -> String {
    format!(
        "split|{}",
        sorted_join(album.parts.iter().map(|part| part.album_id.as_str()))
    )
}

/// What fixing one set of copies does, in a line, as the desktop says it.
fn fix_line(
    fix: &DuplicateFix<Row>,
    fills: &[TagChange],
    fields: &dyn IHealthFields<Row>,
    can_fill: bool,
) -> String {
    let gone = words::count_text(
        fix.remove.len() + usize::from(fix.audio.is_some()),
        "copy",
        "copies",
    );
    let filled = if !can_fill || fills.is_empty() {
        String::new()
    } else {
        let names: Vec<String> = fills
            .iter()
            .map(|change| {
                if change.tag == song_tag_fields::ISRC {
                    "ISRC".to_string()
                } else {
                    to_lower_invariant(&fixes::tag_name(&change.tag))
                }
            })
            .collect();
        format!(" Fills in {}.", names.join(", "))
    };
    match &fix.audio {
        Some(audio) => format!(
            "Moves the sound of {} into {}, which keeps its marks, and moves {gone} to the trash.{filled}",
            fixes::copy_name(audio, fields),
            fixes::copy_name(&fix.keep, fields)
        ),
        None => format!(
            "Keeps {} and moves {gone} to the trash.{filled}",
            fixes::copy_name(&fix.keep, fields)
        ),
    }
}

/// Two notes as one, either or both absent.
fn join_notes(first: Option<&str>, second: Option<&str>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first} {second}")),
        (one, other) => one.or(other).map(str::to_string),
    }
}

fn change_view(change: &TagChange) -> Value {
    json!({
        "tag": change.tag,
        "label": fixes::tag_name(&change.tag),
        "now": change.now,
        "value": change.value,
        "from": change.from,
        "words": change.words(),
    })
}

fn step_view(step: &FixStep) -> Value {
    json!({ "action": step.action, "id": step.id, "title": step.title, "with": step.with })
}

/// Every tag of every copy, side by side, and what the kept copy says after the fix.
fn duplicate_diff(fix: &DuplicateFix<Row>, changes: &[TagChange], fields: &dyn IHealthFields<Row>) -> Value {
    let copies: Vec<&Row> = std::iter::once(&fix.keep)
        .chain(fix.audio.iter())
        .chain(fix.remove.iter())
        .collect();
    let values: Vec<_> = copies
        .iter()
        .map(|copy| fixes::tag_values(*copy, fields))
        .collect();
    let mut after: IndexMap<&str, &str> = IndexMap::new();
    for change in changes {
        after.insert(change.tag.as_str(), change.value.as_str());
    }
    let rows: Vec<Value> = song_tag_fields::ALL
        .iter()
        .filter_map(|tag| {
            let row: Vec<Option<&str>> = values.iter().map(|tags| tags.get(tag)).collect();
            if row.iter().all(Option::is_none) {
                return None;
            }
            Some(json!({
                "tag": tag,
                "label": fixes::tag_name(tag),
                "values": row,
                "after": after.get(tag).copied().or(values[0].get(tag)),
                "changes": after.contains_key(tag),
            }))
        })
        .collect();
    let copies: Vec<Value> = copies
        .iter()
        .map(|copy| {
            json!({
                "id": copy.id,
                "kept": copy.id == fix.keep.id,
                "sound": fix.audio.as_ref().is_some_and(|audio| audio.id == copy.id),
                "quality": words::quality_text(*copy, fields),
            })
        })
        .collect();
    json!({ "copies": copies, "rows": rows })
}

/// For each part that moves, its album tags now and after the join.
fn join_diff(join: &AlbumJoin<'_, Row>, fields: &dyn IHealthFields<Row>) -> Value {
    let lead = fixes::tag_values(&join.lead, fields);
    let parts: Vec<Value> = join
        .album
        .parts
        .iter()
        .skip(1)
        .map(|part| {
            let first = fixes::tag_values(&part.songs[0], fields);
            let tags: Vec<Value> = [
                song_tag_fields::ALBUM,
                song_tag_fields::ALBUM_ARTIST,
                song_tag_fields::YEAR,
            ]
            .iter()
            .map(|tag| {
                let after = if *tag == song_tag_fields::YEAR {
                    lead.get(tag).or(first.get(tag))
                } else {
                    lead.get(tag)
                };
                json!({
                    "tag": tag,
                    "label": fixes::tag_name(tag),
                    "now": first.get(tag),
                    "after": after,
                })
            })
            .collect();
            json!({ "albumId": part.album_id, "songs": part.songs.len(), "fields": tags })
        })
        .collect();
    Value::Array(parts)
}

fn song_view(song: &Row, fields: &dyn IHealthFields<Row>) -> Value {
    json!({
        "id": song.id,
        "title": song.title,
        "artist": song.artist,
        "album": song.album,
        "albumArtist": fields.album_artist(song),
        "year": song.year.filter(|year| *year > 0),
        "quality": words::quality_text(song, fields),
        "lossless": fields.lossless(song),
        "path": song.path,
    })
}

/// `new { id, title, words = $"{song.Artist} · {song.Album}" }`.
fn song_line(song: &Row) -> Value {
    json!({
        "id": song.id,
        "title": song.title,
        "words": format!("{} \u{b7} {}", song.artist, song.album.as_deref().unwrap_or("")),
    })
}

pub fn run_view(run: Option<&HealthRun>) -> Value {
    let Some(run) = run else {
        return Value::Null;
    };
    let progress = run.progress();
    let changed: Option<Vec<&str>> = progress.outcome.as_ref().map(|outcome| {
        let mut ids: Vec<&str> = Vec::new();
        for step in &outcome.done {
            if !ids.contains(&step.id.as_str()) {
                ids.push(&step.id);
            }
        }
        ids
    });
    let failed: Option<Vec<Value>> = progress.outcome.as_ref().map(|outcome| {
        outcome
            .failed
            .iter()
            .map(|(step, why)| json!({ "id": step.id, "title": step.title, "why": why }))
            .collect()
    });
    let lookups: Value = if run.kind != "lookup" {
        Value::Null
    } else {
        Value::Array(
            run.lookups_now()
                .iter()
                .map(|lookup| {
                    let changes: Vec<Value> = lookup
                        .changes
                        .iter()
                        .map(|(change, pick)| {
                            json!({
                                "tag": change.tag,
                                "label": fixes::tag_name(&change.tag),
                                "now": change.now,
                                "value": change.value,
                                "words": change.words(),
                                "pick": pick,
                            })
                        })
                        .collect();
                    json!({
                        "id": lookup.id,
                        "title": lookup.title,
                        "state": lookup.state,
                        "detail": lookup.detail,
                        "origin": lookup.origin,
                        "changes": changes,
                    })
                })
                .collect(),
        )
    };
    json!({
        "id": run.id,
        "kind": run.kind,
        "label": run.label,
        "check": run.check.map(check_name),
        "state": progress.state,
        "done": progress.done,
        "total": run.total,
        "current": progress.current,
        "startedUtc": utc(&run.started_utc),
        "finishedUtc": utc_opt(&progress.finished_utc),
        "error": progress.error,
        "summary": progress.outcome.as_ref().map(|outcome| outcome.summary()),
        "changed": changed,
        "failed": failed,
        "lookups": lookups,
    })
}
