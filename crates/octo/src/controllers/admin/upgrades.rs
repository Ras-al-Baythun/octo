//! The Better quality page: the library's lossy songs and the upgrade queue, which acts as the
//! Navidrome admin signed in on the page, who must be on the library actions allowed list.

use std::collections::HashMap;
use std::sync::LazyLock;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use chrono::{DateTime, TimeDelta, Utc};
use octo_core::common::dotnet::{eq_ignore_case, ordinal_ignore_case_key, to_lower_invariant};
use octo_core::settings::LibraryAction;
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use super::json_body::{FromBody, Kind, Member, Shape};
use super::request_binding::{
    bind_body, browse_user, error, ok, query, required, sign_in, split, status, utc, utc_opt,
};
use crate::app::AppState;
use crate::http::error::validation_problem;
use crate::http::routes::RouteSet;
use crate::services::common::acquisition_tracker::AcquisitionSnapshot;
use crate::services::library::duplicate_scan_worker::is_lossless_file;
use crate::services::library::quality_upgrade_worker::{LibrarySongRow, QualityUpgradeWorker};
use crate::services::library::upgrade_queue::{UpgradeAsk, UpgradeJob, UpgradeResult, UpgradeStates};
use crate::services::library::upgrade_sources::UpgradeSources;

pub fn routes() -> RouteSet {
    RouteSet::new()
        .route("/api/admin/lossy", get(get_lossy))
        .route("/api/admin/upgrades", get(get_upgrades).post(queue_upgrades))
        .route("/api/admin/upgrades/cancel", post(cancel_upgrades))
        .route("/api/admin/upgrades/clear", post(clear_upgrades))
}

/// Most songs one press of the page's button may queue.
pub const MAX_UPGRADES_PER_REQUEST: usize = 2000;

const NO_ADMIN_CREDENTIAL: &str = "Octo needs a Navidrome admin credential to read the whole library.";

/// The library's lossy songs, listed from Navidrome. Walking a big library takes a while, so the
/// answer is kept a few minutes, shared by every request; "refresh" asks again.
static LOSSY_CACHE: LazyLock<Mutex<Option<(DateTime<Utc>, Vec<LibrarySongRow>)>>> =
    LazyLock::new(|| Mutex::new(None));

const LOSSY_FOR: TimeDelta = TimeDelta::minutes(5);

/// The last part of a path already written with forward slashes.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The Better quality page's list: every song in the library that is not lossless, with whether
/// Octo got it from YouTube, when the weekly upgrade last tried it, and any job for it now.
/// Session-gated, because it lists paths.
async fn get_lossy(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    if !state.navidrome_identity.has_admin_identity() {
        return browse.finish(error(StatusCode::BAD_REQUEST, NO_ADMIN_CREDENTIAL));
    }

    // "1", "true" or "yes": a bool parameter refused "1" with a bare 400 before this ran.
    let fresh = query(&parts.uri, "refresh")
        .is_some_and(|r| matches!(to_lower_invariant(r.trim()).as_str(), "1" | "true" | "yes"));
    let now = Utc::now();
    let mut rows: Vec<LibrarySongRow> = {
        let cache = LOSSY_CACHE.lock();
        match &*cache {
            Some((at, rows)) if !fresh && now - *at < LOSSY_FOR => rows.clone(),
            _ => Vec::new(),
        }
    };
    if rows.is_empty() {
        let (songs, complete) = state.quality_upgrade_worker.list_songs().await;
        if !complete && songs.is_empty() {
            return browse.finish(error(
                StatusCode::BAD_GATEWAY,
                "Navidrome did not list the library.",
            ));
        }
        rows = songs
            .into_iter()
            .filter(|song| !is_lossless_file(&song.suffix, song.bit_rate))
            .collect();
        *LOSSY_CACHE.lock() = Some((Utc::now(), rows.clone()));
    }

    // Octo's own record of what it fetched from YouTube, matched by file name, then by path.
    let mut you_tube: HashMap<String, Vec<String>> = HashMap::new();
    for entry in state.download_history.get_recent(i32::MAX) {
        if !eq_ignore_case(&entry.source, "YouTube") || entry.path.is_empty() {
            continue;
        }
        let path = entry.path.replace('\\', "/");
        you_tube
            .entry(ordinal_ignore_case_key(file_name(&path)))
            .or_default()
            .push(path);
    }
    let tried = state.quality_upgrade_worker.tried().attempts;
    // Keyed by Navidrome id: ids are unique in the queue.
    let jobs: HashMap<String, UpgradeJob> = state
        .upgrade_queue
        .snapshot()
        .into_iter()
        .map(|job| (job.navidrome_id.clone(), job))
        .collect();

    let songs: Vec<Value> = rows
        .iter()
        .map(|row| {
            let key = QualityUpgradeWorker::key_of(row);
            let relative = &key[..key.rfind('|').unwrap_or(key.len())];
            let from_you_tube = you_tube
                .get(&ordinal_ignore_case_key(file_name(relative)))
                .is_some_and(|named| {
                    named.iter().any(|path| {
                        ends_with_ignore_case(path, &format!("/{relative}")) || path == relative
                    })
                });
            json!({
                "id": row.id,
                "title": row.title,
                "artist": row.artist,
                "album": row.album,
                "suffix": row.suffix,
                "bitRate": row.bit_rate,
                "size": row.size,
                "path": relative,
                "fromYouTube": from_you_tube,
                "attemptKey": key,
                "lastTried": tried.get(&key).map(|a| json!({ "atUtc": utc(&a.at_utc), "outcome": a.outcome })),
                "job": jobs.get(&row.id).map(|j| json!({ "state": j.state, "detail": j.detail })),
            })
        })
        .collect();
    browse.finish(ok(&json!({ "total": rows.len(), "songs": songs })))
}

/// Whether `text` ends with `suffix`, ignoring case.
fn ends_with_ignore_case(text: &str, suffix: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let suffix: Vec<char> = suffix.chars().collect();
    text.len() >= suffix.len()
        && eq_ignore_case(
            &text[text.len() - suffix.len()..].iter().collect::<String>(),
            &suffix.iter().collect::<String>(),
        )
}

/// `UpgradeSourcesNow?.Ready`.
fn source_ready(sources: &UpgradeSources) -> bool {
    sources.ready()
}

/// An `UpgradeResult` as the controller wrote it (camelCase).
fn result_json(result: &UpgradeResult) -> Value {
    json!({
        "before": result.before,
        "beforeBytes": result.before_bytes,
        "after": result.after,
        "afterBytes": result.after_bytes,
        "newFile": result.new_file,
        "keptAt": result.kept_at,
        "checks": result.checks,
        "seconds": result.seconds,
    })
}

/// The upgrade queue, how many run at once and why, and the gate.
async fn get_upgrades(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    let Some(user) = browse.user.clone() else {
        return browse.finish(sign_in());
    };
    // One row per provider and external id: the first one seen.
    let mut live: HashMap<String, AcquisitionSnapshot> = HashMap::new();
    for row in state.acquisition_tracker.all() {
        live.entry(format!("{}:{}", row.provider, row.external_id))
            .or_insert(row);
    }
    let settings = state.settings.current();
    let actions = settings.library_actions.clone();
    let jobs: Vec<Value> = state
        .upgrade_queue
        .snapshot()
        .iter()
        .map(|job| {
            // The replacement's own download row, while it runs: which stage, and how far.
            let row = match &job.acquisition_key {
                Some(key) if job.state == UpgradeStates::WORKING => live.get(key),
                _ => None,
            };
            json!({
                "id": job.navidrome_id,
                "title": job.title,
                "artist": job.artist,
                "album": job.album,
                "suffix": job.suffix,
                "state": job.state,
                "detail": job.detail,
                "requestedBy": job.requested_by,
                "origin": job.origin,
                "queuedUtc": utc(&job.queued_utc),
                "updatedUtc": utc(&job.updated_utc),
                "startedUtc": utc_opt(&job.started_utc),
                "progress": row.and_then(|r| r.progress),
                "stage": row.map(|r| r.state.name()),
                "source": row.and_then(|r| r.source.clone()),
                "bytesDone": row.and_then(|r| r.bytes_done),
                "bytesTotal": row.and_then(|r| r.bytes_total),
                "note": row.and_then(|r| r.note.clone()),
                "result": job.result.as_ref().map(result_json),
            })
        })
        .collect();
    let sources = &state.upgrade_sources;
    let plan: Vec<&str> = sources.plan().into_iter().map(UpgradeSources::word).collect();
    browse.finish(ok(&json!({
        "jobs": jobs,
        "parallel": state.download_concurrency.current(),
        // Where an upgrade looks, and whether that source is set up, so the page never assumes.
        "source": sources.name(),
        "sourceReady": source_ready(sources),
        "plan": plan,
        "why": state.download_concurrency.why(),
        "gate": {
            "user": user,
            "enabled": actions.enabled,
            "allowed": actions.is_allowed(Some(&user)),
            "dryRun": actions.dry_run,
            "betterQuality": better_quality_on(&actions),
        },
    })))
}

fn better_quality_on(actions: &octo_core::settings::LibraryActionSettings) -> bool {
    actions
        .effective_actions()
        .iter()
        .any(|a| a.action == LibraryAction::BetterQuality && a.enabled)
}

/// The queue body: an optional list of `songs` to upgrade.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UpgradeQueueRequest {
    songs: Option<Vec<UpgradeAskBody>>,
}

impl FromBody for UpgradeQueueRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.AdminController+UpgradeQueueRequest",
        by_constructor: true,
        members: &[Member {
            name: "songs",
            kind: Kind::RecordList(UPGRADE_ASK),
        }],
    };
}

/// One song to upgrade, as the body carries it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UpgradeAskBody {
    navidrome_id: Option<String>,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    suffix: Option<String>,
    attempt_key: Option<String>,
}

/// A body with an optional list of job `ids`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UpgradeIdsRequest {
    ids: Option<Vec<String>>,
}

/// The shape of one song to upgrade, and of the picked copy it can carry.
const UPGRADE_ASK: &Shape = &Shape {
    name: "Octo.Services.Library.UpgradeAsk",
    by_constructor: true,
    members: &[
        Member {
            name: "navidromeId",
            kind: Kind::Text,
        },
        Member {
            name: "title",
            kind: Kind::Text,
        },
        Member {
            name: "artist",
            kind: Kind::Text,
        },
        Member {
            name: "album",
            kind: Kind::Text,
        },
        Member {
            name: "suffix",
            kind: Kind::Text,
        },
        Member {
            name: "attemptKey",
            kind: Kind::Text,
        },
        Member {
            name: "pick",
            kind: Kind::Record(PICKED_COPY),
        },
    ],
};

const PICKED_COPY: &Shape = &Shape {
    name: "Octo.Services.Common.PickedCopy",
    by_constructor: false,
    members: &[
        Member {
            name: "source",
            kind: Kind::Text,
        },
        Member {
            name: "peer",
            kind: Kind::Text,
        },
        Member {
            name: "file",
            kind: Kind::Text,
        },
        Member {
            name: "size",
            kind: Kind::Int64,
        },
        Member {
            name: "format",
            kind: Kind::Text,
        },
        Member {
            name: "bitRate",
            kind: Kind::OptionalInt32,
        },
        Member {
            name: "bitDepth",
            kind: Kind::OptionalInt32,
        },
        Member {
            name: "sampleRate",
            kind: Kind::OptionalInt32,
        },
        Member {
            name: "length",
            kind: Kind::OptionalInt32,
        },
        Member {
            name: "releaseGuid",
            kind: Kind::Text,
        },
        Member {
            name: "indexerId",
            kind: Kind::OptionalInt32,
        },
        Member {
            name: "releaseTitle",
            kind: Kind::Text,
        },
    ],
};

impl FromBody for UpgradeIdsRequest {
    const SHAPE: &'static Shape = &Shape {
        name: "Octo.Controllers.AdminController+UpgradeIdsRequest",
        by_constructor: true,
        members: &[Member {
            name: "ids",
            kind: Kind::TextList,
        }],
    };
}

/// Queue songs for a higher quality copy, acting as the Navidrome admin signed in on this page.
/// Refused unless every gate of the Better quality action is open for that person.
async fn queue_upgrades(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: UpgradeQueueRequest = match bind_body(&parts, &body) {
        Ok(r) => r,
        Err(answer) => return *answer,
    };
    // Every ask's NavidromeId is required, checked before the action runs.
    let missing: Vec<String> = request
        .songs
        .iter()
        .flatten()
        .enumerate()
        .filter(|(_, ask)| !required(ask.navidrome_id.as_deref()))
        .map(|(i, _)| format!("Songs[{i}].NavidromeId"))
        .collect();
    if !missing.is_empty() {
        let message = ["The NavidromeId field is required."];
        let errors: Vec<(&str, &[&str])> = missing.iter().map(|key| (key.as_str(), &message[..])).collect();
        return validation_problem(&errors);
    }

    let browse = browse_user(&state, &parts);
    let Some(user) = browse.user.clone() else {
        return browse.finish(sign_in());
    };
    let settings = state.settings.current().library_actions.clone();
    if !settings.is_allowed(Some(&user)) {
        return browse.finish(error(
            StatusCode::FORBIDDEN,
            format!(
                "{user} is not on the library actions allowed list, so Octo will not change files for them."
            ),
        ));
    }
    let closed = if !source_ready(&state.upgrade_sources) {
        Some(format!(
            "Better quality looks for copies on {}, which is not set up here.",
            state.upgrade_sources.name()
        ))
    } else if !settings.enabled {
        Some("Turn on library actions first.".to_string())
    } else if !better_quality_on(&settings) {
        Some("Turn on the Better quality action first.".to_string())
    } else if settings.dry_run {
        Some("Library actions only rehearse while dry run is on; turn it off first.".to_string())
    } else {
        None
    };
    if let Some(closed) = closed {
        return browse.finish(error(StatusCode::BAD_REQUEST, closed));
    }
    let songs = request.songs.unwrap_or_default();
    if songs.is_empty() {
        return browse.finish(error(StatusCode::BAD_REQUEST, "No songs picked."));
    }
    if songs.len() > MAX_UPGRADES_PER_REQUEST {
        return browse.finish(error(
            StatusCode::BAD_REQUEST,
            format!("At most {MAX_UPGRADES_PER_REQUEST} songs at a time."),
        ));
    }
    let asks: Vec<UpgradeAsk> = songs
        .into_iter()
        .map(|ask| UpgradeAsk {
            navidrome_id: ask.navidrome_id.unwrap_or_default(),
            title: ask.title,
            artist: ask.artist,
            album: ask.album,
            suffix: ask.suffix,
            attempt_key: ask.attempt_key,
            pick: None,
        })
        .collect();
    let (jobs, refused) = state.upgrade_queue.add(asks, &user, "page");
    info!(
        "{user} queued {} songs for higher quality from the dashboard",
        jobs.len()
    );
    browse.finish(status(
        StatusCode::ACCEPTED,
        &json!({ "ok": true, "queued": jobs.len(), "refused": refused }),
    ))
}

/// Take back songs that have not started.
async fn cancel_upgrades(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = split(request).await;
    let request: UpgradeIdsRequest = match bind_body(&parts, &body) {
        Ok(r) => r,
        Err(answer) => return *answer,
    };
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let cancelled = state.upgrade_queue.cancel(&request.ids.unwrap_or_default());
    browse.finish(ok(&json!({ "ok": true, "cancelled": cancelled })))
}

/// Forget finished jobs.
async fn clear_upgrades(State(state): State<AppState>, request: Request) -> Response {
    let (parts, _) = request.into_parts();
    let browse = browse_user(&state, &parts);
    if !browse.signed_in() {
        return browse.finish(sign_in());
    }
    let cleared = state.upgrade_queue.clear_finished();
    browse.finish(ok(&json!({ "ok": true, "cleared": cleared })))
}
