//! `getPlaylists`, `getPlaylist`, and the playlist and internet radio mutations, which refuse
//! Octo's own read-only ids.
//!
//! The playlists Octo keeps itself (`of` ids) hold songs from outside the library beside
//! library songs, which Navidrome's own playlists cannot. While Family is on they are the
//! family playlists: listed for the owner and every member, served per viewer, and changed by
//! whoever may (see [`crate::services::family::family_playlists`]). Otherwise each signed-in
//! account keeps its own, with the owner's say over them. `createPlaylist` makes one when a
//! `songId` is from outside the library or `octo=true` asks for it, and replaces its songs
//! when given its `playlistId`; `updatePlaylist` and `deletePlaylist` change it.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use indexmap::IndexMap;
use octo_core::charts::chart_settings;
use octo_core::common::dotnet;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::json::dom::Node;
use octo_core::library::for_you_lists;
use octo_core::radio::radio_provider;
use octo_core::settings::GeneratedPlaylistSettings;
use octo_core::settings::NoticeKind;
use octo_subsonic::SubsonicReply;
use octo_subsonic::subsonic_response_builder::chart_playlist_comment;
use octo_subsonic::xml::XElement;
use serde_json::Value;
use tracing::warn;

use super::action_helpers::{
    RequestAborted, SubsonicCall, bootstrap_radio_profile, is_octo_playlist_id,
    is_successful_subsonic_response, materialize_station, playlist_names, playlist_stations,
    queue_refresh_if_stale, xml_value,
};
use super::caller_checks::{self, refuse_unless_signed_in, signed_in_user};
use crate::app::AppState;
use crate::middleware::family_gate::{GateReading, Unverified};
use crate::services::family::audio_gate::ExplicitMark;
use crate::services::family::family_playlists::{
    self, FAMILY_OWNER, NewEntry, PlaylistError, SHARED_SCOPE, Serve, personal_scope,
};
use crate::services::family::{FamilyCaller, FamilyPlaylist, FamilyPlaylistEntry, family_caller, kid_filter};
use crate::services::library::library_action_playlist_provisioner::{REMOVE_PLAYLIST, REQUEST_PLAYLIST};
use crate::services::library::library_song_routing::outside_id_of;
use crate::services::library::{GeneratedPlaylist, LibraryActionPlaylistProvisioner};
use crate::services::subsonic::SubsonicResponseBuilderExt;
use octo_core::family::abilities::RequestQuality;
use tracing::info;

/// How many songs of a family playlist are read at once.
const FAMILY_SONGS_AT_ONCE: usize = 8;

pub async fn get_playlists(State(state): State<AppState>, req: Request) -> Response {
    let caller = family_caller(req.extensions()).cloned();
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.clone();
    let builder = &state.subsonic_response_builder;
    let relay = call.proxy.relay_safe("rest/getPlaylists", &call.parameters).await;
    let relay = match relay {
        Some(relay) if !relay.body.is_empty() && is_successful_subsonic_response(&relay.body, &format) => {
            relay
        }
        Some(relay) if !relay.body.is_empty() => {
            return call.file(&relay.body, relay.content_type.as_deref());
        }
        _ => {
            return builder
                .create_error(&format, 0, "Unable to authenticate with Navidrome")
                .into_response();
        }
    };

    let username = call.param_or("u", "").to_string();

    // This listener's Review and Duplicates playlists carry Octo's glyph cover.
    let notices = notice_titles(&state, &username);
    let mut relay = relay;
    if let Some(body) = mark_notice_rows(&relay.body, &format, &username, &notices) {
        relay.body = body.into();
    }

    // Navidrome answered ok above, so `u` is authenticated. Ensure this user's action
    // playlists exist, using the body we already have so the common case costs no extra
    // request. Fire-and-forget: creating a playlist must never delay the listing.
    let names = playlist_names(Some(&relay.body), &format);
    let provisioner = LibraryActionPlaylistProvisioner::new(call.proxy.clone(), state.settings.clone())
        .with_family(state.family_store.clone());
    let ensured_user = username.clone();
    let ensured_parameters = call.parameters.clone();
    tokio::spawn(async move {
        provisioner
            .ensure(&ensured_user, &names, &ensured_parameters)
            .await;
    });

    bootstrap_radio_profile(&state, &call.proxy, &username, &call.parameters).await;
    let stations = playlist_stations(&state, &username);
    queue_refresh_if_stale(&state, &username);
    let mix_settings = state.settings.current().generated_playlists.clone();
    // The mixes, and Made for you, which is on by default whatever the mixes switch says.
    let generated = if mix_settings.enabled || mix_settings.any_for_you() {
        state.generated_playlists.list(&username, &call.parameters).await
    } else {
        Vec::new()
    };
    // The chart lists, on whether or not the mixes are: after the stations, before the mixes.
    let charts = state.chart_playlists.list(&username, &call.parameters).await;
    // Liked Songs, listed before every other playlist.
    let liked = state
        .generated_playlists
        .liked_row(&username, &call.parameters)
        .await
        .map(|row| liked_playlist_fields(builder, &row, &mix_settings));
    let family = kept_rows(&state, &call, caller.as_ref()).await;
    if stations.is_empty()
        && family.is_empty()
        && charts.is_empty()
        && generated.is_empty()
        && liked.is_none()
    {
        return call.file(&relay.body, relay.content_type.as_deref());
    }

    let rows: Vec<serde_json::Map<String, Value>> = stations
        .iter()
        .map(|station| builder.radio_playlist_fields(station))
        .chain(family)
        .chain(charts.iter().map(|list| chart_playlist_fields(builder, list)))
        .chain(
            generated
                .iter()
                .map(|mix| builder.generated_playlist_fields(mix, &mix_settings)),
        )
        .collect();
    let merged = if format.eq_ignore_ascii_case("json") {
        merge_json(
            &relay.body,
            "playlists",
            "playlist",
            rows.into_iter().map(Value::Object),
        )
        .and_then(|body| match &liked {
            Some(row) => prepend_json(&body, "playlists", "playlist", Value::Object(row.clone())),
            None => Some(body),
        })
        .map(|body| SubsonicReply::file(body, "application/json"))
    } else {
        merge_xml(
            &relay.body,
            "playlists",
            "playlist",
            rows.iter().map(field_attributes),
        )
        .and_then(|body| match &liked {
            Some(row) => prepend_xml(&body, "playlists", "playlist", field_attributes(row)),
            None => Some(body),
        })
        .map(|body| SubsonicReply::file(body, "application/xml"))
    };
    match merged {
        Some(reply) => reply.into_response(),
        None => {
            warn!("Could not merge Radio stations and mixes into getPlaylists");
            call.file(&relay.body, relay.content_type.as_deref())
        }
    }
}

/// A chart list as a playlist row: the keys a mix has, read-only, with its own comment and its
/// real song count.
pub(crate) fn chart_playlist_fields(
    builder: &octo_subsonic::SubsonicResponseBuilder,
    list: &GeneratedPlaylist,
) -> serde_json::Map<String, Value> {
    let wide = GeneratedPlaylistSettings {
        track_count: 500,
        ..Default::default()
    };
    let mut fields = builder.generated_playlist_fields(list, &wide);
    fields.insert("songCount".into(), list.pool_size.into());
    let chart = chart_settings::chart_from_key(&list.key).unwrap_or_default();
    let kind = chart_settings::kind_word(&chart).unwrap_or("overall");
    let genre = list.name.strip_prefix("Top ").unwrap_or(&list.name);
    fields.insert("comment".into(), chart_playlist_comment(kind, genre).into());
    // octoCoverTitle: the words the Octo apps put on the list's cover in place of its name.
    if let Some(title) = super::media::chart_cover_title(&chart) {
        fields.insert("octoCoverTitle".into(), title.into());
    }
    fields
}

/// Where a notice playlist's cover is drawn: `getCoverArt` paints it with the notice's glyph.
pub(crate) const NOTICE_COVER_PREFIX: &str = "oc-notice-";

/// The notice kinds a playlist row can be marked with, as `octoNotice` names them.
pub(crate) const NOTICE_REVIEW: &str = "review";
pub(crate) const NOTICE_DUPLICATES: &str = "duplicates";

/// The kind's word as rows and cover ids carry it.
pub(crate) fn notice_word(kind: NoticeKind) -> &'static str {
    match kind {
        NoticeKind::Review => NOTICE_REVIEW,
        NoticeKind::Duplicates => NOTICE_DUPLICATES,
    }
}

/// The notice kind a word names.
pub(crate) fn notice_kind(word: &str) -> Option<NoticeKind> {
    match word {
        NOTICE_REVIEW => Some(NoticeKind::Review),
        NOTICE_DUPLICATES => Some(NoticeKind::Duplicates),
        _ => None,
    }
}

/// The notice playlists that are on, by title, with their kind's word. Empty for no listener.
fn notice_titles(state: &AppState, username: &str) -> Vec<(String, &'static str)> {
    if username.trim().is_empty() {
        return Vec::new();
    }
    let settings = state.settings.current().library_actions.clone();
    settings
        .enabled_notice_kinds()
        .into_iter()
        .map(|kind| (settings.notice_title(kind), notice_word(kind)))
        .collect()
}

/// The notice word for a playlist row: the listener's own playlist named like a notice that is on.
fn notice_of(
    owner: Option<&str>,
    name: Option<&str>,
    username: &str,
    notices: &[(String, &'static str)],
) -> Option<&'static str> {
    let (owner, name) = (owner?, name?);
    if !dotnet::eq_ignore_case(owner, username) {
        return None;
    }
    notices
        .iter()
        .find(|(title, _)| dotnet::eq_ignore_case(title, name))
        .map(|(_, word)| *word)
}

/// Marks the listener's notice playlists in a getPlaylists or getPlaylist answer: `coverArt`
/// becomes the notice's painted cover and `octoNotice` names the notice, so every client shows
/// the glyph and the Octo apps know which list it is. A playlist of anyone else, or one named
/// like a notice that is off, is left as Navidrome sent it. None when nothing was marked, or the
/// body is not one this reads.
pub(super) fn mark_notice_rows(
    body: &[u8],
    format: &str,
    username: &str,
    notices: &[(String, &'static str)],
) -> Option<Vec<u8>> {
    if notices.is_empty() {
        return None;
    }
    if format.eq_ignore_ascii_case("json") {
        let mut root = Node::parse(std::str::from_utf8(body).ok()?).ok()?;
        let response = root
            .as_object_mut()?
            .get_mut("subsonic-response")?
            .as_object_mut()?;
        let mut rows: Vec<&mut IndexMap<String, Node>> = Vec::new();
        for (key, value) in response.iter_mut() {
            match (key.as_str(), value) {
                ("playlists", Node::Object(holder)) => match holder.get_mut("playlist") {
                    Some(Node::Array(list)) => rows.extend(list.iter_mut().filter_map(Node::as_object_mut)),
                    Some(Node::Object(one)) => rows.push(one),
                    _ => {}
                },
                ("playlist", Node::Object(one)) => rows.push(one),
                _ => {}
            }
        }
        let mut marked = false;
        for row in rows {
            let word = notice_of(
                row.get("owner").and_then(Node::as_str),
                row.get("name").and_then(Node::as_str),
                username,
                notices,
            );
            if let Some(word) = word {
                row.insert(
                    "coverArt".into(),
                    Node::String(format!("{NOTICE_COVER_PREFIX}{word}")),
                );
                row.insert("octoNotice".into(), Node::String(word.to_string()));
                marked = true;
            }
        }
        return marked.then(|| root.to_json_string(false).into_bytes());
    }
    if !format.eq_ignore_ascii_case("xml") {
        return None;
    }
    let mut root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    let mut marked = false;
    let mut mark = |row: &mut XElement| {
        let word = notice_of(row.attribute("owner"), row.attribute("name"), username, notices);
        if let Some(word) = word {
            row.set_attr("coverArt", format!("{NOTICE_COVER_PREFIX}{word}"));
            row.set_attr("octoNotice", word.to_string());
            marked = true;
        }
    };
    for element in root.elements_mut() {
        if element.name == "playlists" {
            for row in element.elements_mut().filter(|row| row.name == "playlist") {
                mark(row);
            }
        } else if element.name == "playlist" {
            mark(element);
        }
    }
    marked.then(|| root.to_xml_string().into_bytes())
}

/// Liked Songs as a playlist row: the keys a mix has, read-only, with its real song count and
/// `octoList: "liked"` so the Octo apps put it first on Home and draw its heart cover.
pub(crate) fn liked_playlist_fields(
    builder: &octo_subsonic::SubsonicResponseBuilder,
    list: &GeneratedPlaylist,
    settings: &GeneratedPlaylistSettings,
) -> serde_json::Map<String, Value> {
    let mut fields = builder.generated_playlist_fields(list, settings);
    fields.insert("songCount".into(), list.pool_size.into());
    fields.insert("duration".into(), (i64::from(list.pool_size) * 210).into());
    fields.insert("comment".into(), "Your hearted songs, newest first".into());
    // Always current, so there is no time it is good until.
    fields.remove("validUntil");
    fields.insert("octoList".into(), for_you_lists::LIKED_KIND.into());
    fields
}

/// [`merge_json`]'s row put first instead of last.
fn prepend_json(body: &[u8], container: &str, row: &str, value: Value) -> Option<Vec<u8>> {
    let mut root = Node::parse(std::str::from_utf8(body).ok()?).ok()?;
    let holder = root
        .as_object_mut()?
        .get_mut("subsonic-response")?
        .as_object_mut()?
        .get_mut(container)?
        .as_object_mut()?;
    let Some(Node::Array(list)) = holder.get_mut(row) else {
        return None;
    };
    list.insert(0, to_node(&value));
    Some(root.to_json_string(false).into_bytes())
}

/// [`merge_xml`]'s row put first instead of last.
fn prepend_xml(
    body: &[u8],
    container: &str,
    row: &str,
    attributes: Vec<(String, String)>,
) -> Option<Vec<u8>> {
    let mut root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    let ns = root.namespace.clone();
    let holder = root.elements_mut().find(|element| element.name == container)?;
    let mut element = XElement::in_namespace(ns.as_deref(), row);
    for (name, value) in attributes {
        element.set_attr(name, value);
    }
    holder.add_first(vec![element]);
    Some(root.to_xml_string().into_bytes())
}

/// The playlists Octo keeps, as rows for this caller: while Family is on, the family playlists
/// for the owner and every member (none for an account outside the family); otherwise the
/// signed-in account's own.
async fn kept_rows(
    state: &AppState,
    call: &SubsonicCall,
    caller: Option<&FamilyCaller>,
) -> Vec<serde_json::Map<String, Value>> {
    let family_on = state.settings.current().family.enabled;
    let own;
    let caller = if family_on {
        let Some(caller) = caller else {
            return Vec::new();
        };
        caller
    } else {
        // Navidrome answered getPlaylists ok, so the sign-in is good: its account is the viewer.
        let user = signed_in_user(state, &caller_call(call))
            .await
            .unwrap_or_default();
        if user.trim().is_empty() {
            return Vec::new();
        }
        own = kept_caller(&user);
        &own
    };
    let mut rows = Vec::new();
    for playlist in visible_kept_playlists(state, caller) {
        let (count, seconds) = family_playlist_totals(state, &playlist, caller).await;
        rows.push(family_playlist_fields(
            &playlist, caller, family_on, count, seconds,
        ));
    }
    rows
}

/// The call as the caller checks read it: the same request, parameters and proxy.
fn caller_call(call: &SubsonicCall) -> caller_checks::SubsonicCall {
    caller_checks::SubsonicCall {
        incoming: Arc::clone(&call.incoming),
        parameters: call.parameters.clone(),
        proxy: call.proxy.clone(),
    }
}

/// With Family off, the account behind a kept playlist call: nobody manages it, and it has the
/// owner's say over its own playlists.
fn kept_caller(user: &str) -> FamilyCaller {
    FamilyCaller::unmanaged(user, true)
}

/// Whether `viewer` sees this kept playlist: while Family is on, as the family rules say;
/// otherwise only the account that made it.
fn sees_kept_playlist(state: &AppState, viewer: &FamilyCaller, playlist: &FamilyPlaylist) -> bool {
    state.settings.current().family.enabled || eq_ignore_case(&playlist.created_by, &viewer.user)
}

/// The kept playlists `viewer` sees.
fn visible_kept_playlists(state: &AppState, viewer: &FamilyCaller) -> Vec<FamilyPlaylist> {
    family_playlists::visible_playlists(&state.family_playlists, viewer)
        .into_iter()
        .filter(|playlist| sees_kept_playlist(state, viewer, playlist))
        .collect()
}

/// One kept playlist, when `viewer` sees it.
fn find_kept_playlist(state: &AppState, viewer: &FamilyCaller, id: &str) -> Option<FamilyPlaylist> {
    family_playlists::find(&state.family_playlists, viewer, id)
        .filter(|playlist| sees_kept_playlist(state, viewer, playlist))
}

/// What a family playlist's row counts for `viewer`: the songs they are shown and how many
/// seconds those last. A kid profile's count and length leave out explicit songs, library and
/// outside alike, as their `getPlaylist` answer does.
async fn family_playlist_totals(
    state: &AppState,
    playlist: &FamilyPlaylist,
    viewer: &FamilyCaller,
) -> (usize, i64) {
    let kid = kid_filter::is_kid(viewer);
    let unrated = state.settings.current().family.kid_unrated_is_explicit;
    let index = &state.library_index;
    let songs: Vec<(i64, bool)> = futures::stream::iter(family_playlists::shown_entries(playlist, viewer))
        .map(|shown| async move {
            match shown.serve {
                Serve::Library { id, .. } => {
                    let seconds = index.song(&id).map_or(0, |song| i64::from(song.duration));
                    let explicit = kid && kid_filter::song_is_explicit(index, &id, None, unrated);
                    (seconds, explicit)
                }
                Serve::Outside(id) => {
                    let stored = playlist.entries.get(shown.stored);
                    let (seconds, mark) = outside_facts(state, stored, &id).await;
                    (seconds.unwrap_or(0), kid && mark.blocks_clean_only(unrated))
                }
            }
        })
        .buffered(FAMILY_SONGS_AT_ONCE)
        .collect()
        .await;
    songs
        .iter()
        .filter(|(_, explicit)| !explicit)
        .fold((0, 0), |(count, total), (seconds, _)| {
            (count + 1, total + seconds)
        })
}

/// An outside song's length and explicit mark: as the entry kept them when it was added, else
/// from the catalog now.
async fn outside_facts(
    state: &AppState,
    stored: Option<&FamilyPlaylistEntry>,
    id: &str,
) -> (Option<i64>, ExplicitMark) {
    if let Some(entry) = stored.filter(|e| e.outside_id.as_deref() == Some(id))
        && (entry.duration.is_some() || entry.explicit_status.is_some())
    {
        return (
            entry.duration,
            ExplicitMark::from_status(entry.explicit_status.as_deref().unwrap_or("")),
        );
    }
    let (duration, status) = outside_lookup(state, id).await;
    (
        duration,
        ExplicitMark::from_status(status.as_deref().unwrap_or("")),
    )
}

/// An outside song's length in seconds and its explicit mark word, from the catalog.
async fn outside_lookup(state: &AppState, id: &str) -> (Option<i64>, Option<String>) {
    let (external, provider, external_id) = state.local_library.parse_song_id(id);
    if !external {
        return (None, None);
    }
    let Some(song) = state
        .metadata_service
        .get_song(
            provider.as_deref().unwrap_or_default(),
            external_id.as_deref().unwrap_or_default(),
        )
        .await
    else {
        return (None, None);
    };
    let status = match ExplicitMark::from_catalog(song.explicit_content_lyrics) {
        ExplicitMark::Explicit => "explicit",
        ExplicitMark::Clean => "clean",
        ExplicitMark::Unrated => "",
    };
    (song.duration.map(i64::from), Some(status.to_string()))
}

fn utc_millis(at: &DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// One family playlist's row as `viewer` sees it: owned by "Family", public, with the number
/// of songs this viewer is shown and how long they last. Read-only for a viewer who cannot
/// change it.
fn family_playlist_fields(
    playlist: &FamilyPlaylist,
    viewer: &FamilyCaller,
    family_on: bool,
    count: usize,
    seconds: i64,
) -> serde_json::Map<String, Value> {
    let changed = playlist
        .entries
        .iter()
        .map(|entry| entry.added)
        .chain([playlist.created])
        .max()
        .unwrap_or(playlist.created);
    let mut fields = serde_json::Map::new();
    fields.insert("id".into(), playlist.id.clone().into());
    fields.insert("name".into(), playlist.name.clone().into());
    if family_on {
        fields.insert("comment".into(), "Family playlist".into());
        fields.insert("owner".into(), FAMILY_OWNER.into());
        fields.insert("public".into(), true.into());
    } else {
        fields.insert("comment".into(), "Octo playlist".into());
        fields.insert("owner".into(), playlist.created_by.clone().into());
        fields.insert("public".into(), false.into());
    }
    // octoPlaylists v1: a client tells a kept playlist by this, not by its id.
    fields.insert("octoPlaylist".into(), true.into());
    fields.insert("songCount".into(), count.into());
    fields.insert("duration".into(), seconds.into());
    fields.insert("created".into(), utc_millis(&playlist.created).into());
    fields.insert("changed".into(), utc_millis(&changed).into());
    fields.insert("readonly".into(), (!family_playlists::may_edit(viewer)).into());
    fields
}

/// A row's fields as XML attributes (`XmlValue`).
fn field_attributes(fields: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    fields
        .iter()
        .map(|(name, value)| (name.clone(), xml_value(value)))
        .collect()
}

/// The JSON merge: `subsonic-response.{container}.{row}` made an array if it was not one, and
/// the rows appended. None when the body is not the object it should be.
pub(super) fn merge_json(
    body: &[u8],
    container: &str,
    row: &str,
    rows: impl IntoIterator<Item = Value>,
) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(body).ok()?;
    let mut root = Node::parse(text).ok()?;
    let response = root
        .as_object_mut()?
        .get_mut("subsonic-response")?
        .as_object_mut()?;
    if !matches!(response.get(container), Some(Node::Object(_))) {
        response.insert(container.to_string(), Node::object());
    }
    let holder = response.get_mut(container)?.as_object_mut()?;
    if !matches!(holder.get(row), Some(Node::Array(_))) {
        holder.insert(row.to_string(), Node::Array(Vec::new()));
    }
    let Some(Node::Array(list)) = holder.get_mut(row) else {
        return None;
    };
    list.extend(rows.into_iter().map(|value| to_node(&value)));
    Some(root.to_json_string(false).into_bytes())
}

/// The XML merge: the root's first `{container}` child (by local name), added if missing, and
/// one `{row}` element per attribute list in the root's namespace.
pub(super) fn merge_xml(
    body: &[u8],
    container: &str,
    row: &str,
    rows: impl IntoIterator<Item = Vec<(String, String)>>,
) -> Option<Vec<u8>> {
    let mut root = XElement::parse(&String::from_utf8_lossy(body)).ok()?;
    let ns = root.namespace.clone();
    if !root.elements().any(|element| element.name == container) {
        root.push(XElement::in_namespace(ns.as_deref(), container));
    }
    let holder = root.elements_mut().find(|element| element.name == container)?;
    for attributes in rows {
        let mut element = XElement::in_namespace(ns.as_deref(), row);
        for (name, value) in attributes {
            element.set_attr(name, value);
        }
        holder.push(element);
    }
    Some(root.to_xml_string().into_bytes())
}

/// A serialized value as a node: doubles in their shortest form.
pub(super) fn to_node(value: &Value) -> Node {
    match value {
        Value::Null => Node::Null,
        Value::Bool(b) => Node::Bool(*b),
        Value::Number(n) if n.is_i64() || n.is_u64() => Node::Number(n.to_string()),
        Value::Number(n) => Node::Number(octo_core::json::format_double(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => Node::String(s.clone()),
        Value::Array(items) => Node::Array(items.iter().map(to_node).collect()),
        Value::Object(map) => Node::Object(map.iter().map(|(k, v)| (k.clone(), to_node(v))).collect()),
    }
}

pub async fn get_playlist(State(state): State<AppState>, req: Request) -> Response {
    let aborted = RequestAborted::new();
    let response = get_playlist_inner(&state, req, &aborted).await;
    aborted.answered();
    response
}

async fn get_playlist_inner(state: &AppState, req: Request, aborted: &RequestAborted) -> Response {
    let call = match SubsonicCall::read(state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    let builder = &state.subsonic_response_builder;
    let id = call.param_or("id", "").to_string();
    let username = call.param_or("u", "").to_string();

    // Liked Songs: the listener's hearts as Navidrome holds them now, newest first. Only for a
    // caller that names its listener.
    if !id.is_empty()
        && !octo_core::common::dotnet::is_blank(&username)
        && id == state.generated_playlists.liked_id(&username)
        && state.settings.current().generated_playlists.liked_songs
    {
        let mut auth_only = call.parameters.clone();
        auth_only.shift_remove("id");
        let check = call.proxy.relay_safe("rest/ping", &auth_only).await;
        if !check.is_some_and(|check| is_successful_subsonic_response(&check.body, format)) {
            return builder
                .create_error(format, 40, "Wrong username or password")
                .into_response();
        }
        let settings = state.settings.current().generated_playlists.clone();
        let Some(mut entries) = state
            .generated_playlists
            .liked_songs(&username, &call.parameters)
            .await
        else {
            return builder
                .create_error(format, 0, "Unable to read your hearted songs")
                .into_response();
        };
        if let Some(marks) = state.generated_playlists.marks(&username, &call.parameters).await {
            state.generated_playlists.overlay(&username, &mut entries, &marks);
        }
        let row = state
            .generated_playlists
            .liked_row(&username, &call.parameters)
            .await
            .unwrap_or_else(|| GeneratedPlaylist {
                id: id.clone(),
                key: for_you_lists::LIKED_KIND.to_string(),
                kind: for_you_lists::LIKED_KIND.to_string(),
                label: settings.effective_liked_songs_name(),
                name: settings.effective_liked_songs_name(),
                owner: username.trim().to_string(),
                pool_size: 0,
                period_start_utc: chrono::Utc::now(),
                period_end_utc: chrono::Utc::now(),
            });
        let fields = liked_playlist_fields(builder, &row, &settings);
        return builder
            .create_entries_playlist_response(format, fields, &entries)
            .into_response();
    }

    if is_kept_playlist_id(&id) {
        return family_playlist(state, &call, &id).await;
    }

    // A mix is the listener's own library, served as a playlist Navidrome has never heard of.
    // The ping is the auth check, as for a station: nothing about a user is revealed first.
    if let Some(mix) = state.generated_playlists.find(&username, &id) {
        if !signed_in(state, &call, format).await {
            return builder
                .create_error(format, 40, "Wrong username or password")
                .into_response();
        }
        // A failure here answers 500.
        let mut entries = match state
            .generated_playlists
            .materialize(&username, &mix, &call.parameters, &aborted.token)
            .await
        {
            Ok(entries) => entries,
            Err(error) => return crate::http::error::AppError::Internal(error).into_response(),
        };
        // Drawn earlier, so hearts and ratings given since are put on as they are now.
        if let Some(marks) = state.generated_playlists.marks(&username, &call.parameters).await {
            state.generated_playlists.overlay(&username, &mut entries, &marks);
        }
        let settings = state.settings.current().generated_playlists.clone();
        return builder
            .create_generated_playlist_response(format, &mix, &settings, &entries)
            .into_response();
    }

    // A chart list, with the same ping first.
    if let Some(chart) = state.chart_playlists.which(&username, &id) {
        if !signed_in(state, &call, format).await {
            return builder
                .create_error(format, 40, "Wrong username or password")
                .into_response();
        }
        let entries = state
            .chart_playlists
            .songs(&username, &chart, &call.parameters)
            .await;
        let outside = state.chart_playlists.outside_songs(&username, &chart);
        // Made hours ago, so the library songs' hearts and ratings are put on as they are now.
        let entries = match state.generated_playlists.marks(&username, &call.parameters).await {
            Some(marks) => entries
                .into_iter()
                .map(|entry| {
                    if entry.get("isExternal").and_then(Value::as_bool) == Some(true) {
                        return entry;
                    }
                    let mut one = [Node::from_value(&entry)];
                    state.generated_playlists.overlay(&username, &mut one, &marks);
                    let [one] = one;
                    one.to_value()
                })
                .collect(),
            None => entries,
        };
        // A song this listener's sync catalog holds goes out as the catalog describes it, as on
        // a station: a syncing client keeps whichever description it read last.
        let entries: Vec<Value> = entries
            .into_iter()
            .map(|entry| {
                let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
                if !outside.iter().any(|song| song.id == id) {
                    return entry;
                }
                match state.sync_catalog.try_get_song(&username, id) {
                    Some(synced) => Value::Object(builder.convert_song_to_json(&synced)),
                    None => entry,
                }
            })
            .collect();
        state
            .radio_queues
            .register(outside.iter().map(|song| song.id.clone()));
        let metadata = Arc::clone(&state.metadata_service);
        tokio::spawn(async move { metadata.prewarm_you_tube_ids(&outside, 8).await });
        let list = state
            .chart_playlists
            .made_playlist(&username, &chart)
            .unwrap_or_else(|| {
                state
                    .chart_playlists
                    .playlist(&username, &chart, String::new(), 0)
            });
        let fields = chart_playlist_fields(builder, &list);
        return builder
            .create_chart_playlist_response(format, fields, &entries)
            .into_response();
    }

    let Some(station) = playlist_stations(state, &username)
        .into_iter()
        .find(|item| item.id == id)
    else {
        return match call.proxy.relay_safe("rest/getPlaylist", &call.parameters).await {
            Some(relay) if is_successful_subsonic_response(&relay.body, format) => {
                let notices = notice_titles(state, &username);
                match mark_notice_rows(&relay.body, format, &username, &notices) {
                    Some(body) => call.file(&body.into(), relay.content_type.as_deref()),
                    None => call.file(&relay.body, relay.content_type.as_deref()),
                }
            }
            Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
            None => builder
                .create_error(format, 0, "Playlist not found")
                .into_response(),
        };
    };
    if !signed_in(state, &call, format).await {
        return builder
            .create_error(format, 40, "Wrong username or password")
            .into_response();
    }

    let mut songs = materialize_station(state, &call.proxy, &station, &call.parameters).await;
    let mut suggested: HashMap<String, String> = HashMap::new();
    for song in &songs {
        if let Some(by) = &song.suggested_by {
            suggested.entry(song.id.clone()).or_insert_with(|| by.clone());
        }
    }
    songs = state
        .generated_playlists
        .blend_into_discovery(&username, &station, songs, &call.parameters)
        .await;

    // Before the catalog swap below: these rows are this response's own, while a catalog
    // row is shared with every other response that serves it.
    state.metadata_service.complete_song_lengths(&mut songs);

    // A song this user's sync catalog also holds goes out as the catalog describes it:
    // same album, and filed under the library's own artist where there is one. A syncing
    // client stores whichever description it read last, so the two must not disagree.
    let songs: Vec<_> = songs
        .into_iter()
        .map(|song| {
            if song.is_local {
                return song;
            }
            state
                .sync_catalog
                .try_get_song(&username, &song.id)
                .unwrap_or(song)
        })
        .collect();
    // The discovery blend and the catalog swap hand back other objects; the source rides on by
    // id, on a copy, since a catalog row is shared with other responses.
    let songs: Vec<_> = songs
        .into_iter()
        .map(|mut song| {
            if let Some(by) = suggested.get(&song.id)
                && song.suggested_by.as_ref() != Some(by)
            {
                song.suggested_by = Some(by.clone());
            }
            song
        })
        .collect();
    // A station refetched by a syncing client is not the listener choosing it, so it never
    // renews the window in which a start counts for radio.
    state.radio_outcomes.served(
        Some(&username),
        songs.iter().map(|song| {
            (
                song.id.as_str(),
                radio_provider::from_display_name(song.suggested_by.as_deref()),
            )
        }),
        None,
        false,
    );
    state
        .radio_queues
        .register(songs.iter().map(|song| song.id.clone()));
    let metadata = Arc::clone(&state.metadata_service);
    let prewarm = songs.clone();
    tokio::spawn(async move { metadata.prewarm_you_tube_ids(&prewarm, 8).await });
    queue_refresh_if_stale(state, &username);
    builder
        .create_radio_playlist_response(format, &station, &songs)
        .into_response()
}

pub async fn mutate_internet_radio_station(State(state): State<AppState>, req: Request) -> Response {
    mutate(
        state,
        req,
        "id",
        "updateInternetRadioStation",
        "Unable to update internet radio station",
    )
    .await
}

pub async fn mutate_playlist(State(state): State<AppState>, req: Request) -> Response {
    mutate(
        state,
        req,
        "playlistId",
        "updatePlaylist",
        "Unable to update playlist",
    )
    .await
}

/// What a family action playlist does with a song put in it.
enum FamilyPlaylistAction {
    Request(RequestQuality),
    Remove,
}

/// `updatePlaylist` on one of a family member's own action playlists ("Request a copy",
/// "Request a copy in FLAC", "Remove from my library"): each song added becomes a request or
/// leaves their library, and so does anything already in the playlist; the playlist is then
/// emptied, since the requests are the record. None for any other playlist.
async fn family_action_playlist(
    state: &AppState,
    call: &SubsonicCall,
    id: &str,
    user: &str,
) -> Option<Response> {
    const AUTH_KEYS: [&str; 9] = ["u", "p", "t", "s", "v", "c", "apiKey", "jwt", "f"];
    if id.trim().is_empty() {
        return None;
    }
    let mut auth: Vec<(String, String)> = call
        .parameters
        .iter()
        .filter(|(key, _)| {
            AUTH_KEYS.iter().any(|k| k.eq_ignore_ascii_case(key)) && !key.eq_ignore_ascii_case("f")
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    auth.push(("f".into(), "json".into()));
    let mut ask = auth.clone();
    ask.push(("id".into(), id.to_string()));
    let read = call
        .proxy
        .relay_safe("rest/getPlaylist", ask.iter().map(|(k, v)| (k, v)))
        .await?;
    let body: Value = serde_json::from_slice(&read.body).ok()?;
    let playlist = body.pointer("/subsonic-response/playlist")?;
    let name = playlist.get("name").and_then(Value::as_str)?.trim().to_string();
    let action = if name.eq_ignore_ascii_case(REQUEST_PLAYLIST) {
        FamilyPlaylistAction::Request(RequestQuality::Best)
    } else if name.eq_ignore_ascii_case(REMOVE_PLAYLIST) {
        FamilyPlaylistAction::Remove
    } else {
        return None;
    };
    let already: Vec<String> = playlist
        .get("entry")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let added: Vec<String> = call
        .param("songIdToAdd")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|song| !song.is_empty())
        .map(str::to_string)
        .collect();
    for song in already.iter().chain(added.iter()) {
        match &action {
            FamilyPlaylistAction::Request(quality) => {
                match state
                    .family_request_service
                    .create(user, song, *quality, None)
                    .await
                {
                    Ok(request) => info!("FAMILY {user} asked for {song} through {name} ({})", request.id),
                    Err(e) => info!("FAMILY {user} could not ask for {song}: {}", e.message),
                }
            }
            FamilyPlaylistAction::Remove => {
                if let Err(e) = state.personal_library.remove_from_my_library(user, song).await {
                    info!("FAMILY {user} could not remove {song}: {}", e.message);
                }
            }
        }
    }
    if !already.is_empty() {
        let mut empty = auth.clone();
        empty.push(("playlistId".into(), id.to_string()));
        empty.extend((0..already.len()).map(|at| ("songIndexToRemove".to_string(), at.to_string())));
        if call
            .proxy
            .relay_safe("rest/updatePlaylist", empty.iter().map(|(k, v)| (k, v)))
            .await
            .is_none()
        {
            warn!("Could not empty {user}'s {name} playlist");
        }
    }
    Some(
        state
            .subsonic_response_builder
            .create_response(&call.format, "updatePlaylist")
            .into_response(),
    )
}

/// Refuses Octo's own ids, changes the playlists Octo keeps (and makes one when a new
/// playlist holds a song from outside the library), and relays everything else as
/// `rest/<last path segment>` in the client's own spelling.
async fn mutate(state: AppState, req: Request, id_key: &str, _fallback: &str, failure: &str) -> Response {
    let call = match SubsonicCall::read(&state, req).await {
        Ok(call) => call,
        Err(response) => return response,
    };
    let format = call.format.as_str();
    // updatePlaylist names its playlist `playlistId`, falling back to `id`.
    let id = call
        .param(id_key)
        .or_else(|| call.param("id"))
        .unwrap_or("")
        .to_string();
    // A family member's request and remove playlists turn what is added to them into requests
    // or removals, and stay empty.
    if id_key == "playlistId"
        && let Some(member) = family_caller(&call.parts.extensions)
            .filter(|caller| caller.managed)
            .map(|caller| caller.user.clone())
        && let Some(answer) = family_action_playlist(&state, &call, &id, &member).await
    {
        return answer;
    }
    let change = playlist_change(&call);
    if is_kept_playlist_id(&id)
        && let Some(change) = change
    {
        return family_mutate(&state, &call, &id, change).await;
    }
    if is_octo_playlist_id(&id) {
        return state
            .subsonic_response_builder
            .create_error(format, 70, "Octo's generated playlists are read-only")
            .into_response();
    }
    // A new playlist with a song from outside the library (or asked for with `octo=true`) is
    // one Octo keeps, since Navidrome cannot hold such a song; Navidrome's own playlists
    // refuse one rather than lose it quietly.
    if change == Some(PlaylistChange::Create) && id.is_empty() && wants_kept_playlist(&state, &call) {
        return create_kept_playlist(&state, &call).await;
    }
    let songs = match change {
        Some(PlaylistChange::Create) => "songId",
        Some(PlaylistChange::Update) => "songIdToAdd",
        _ => "",
    };
    if !songs.is_empty()
        && let Some(outside) = outside_song_param(&state, &call, songs)
    {
        return family_error(
            &state,
            &call,
            70,
            &format!(
                "This playlist lives in Navidrome and cannot hold {outside}, a song not in the library. Put it in an Octo playlist instead."
            ),
        );
    }
    // The endpoint is the path's last segment, without `.view`.
    let path = call.path();
    let endpoint = path.split('/').next_back().unwrap_or("").replace(".view", "");
    match call
        .proxy
        .relay_safe(&format!("rest/{endpoint}"), &call.parameters)
        .await
    {
        Some(relay) => call.file(&relay.body, relay.content_type.as_deref()),
        None => state
            .subsonic_response_builder
            .create_error(format, 0, failure)
            .into_response(),
    }
}

/// Whether `id` is a playlist Octo keeps itself.
fn is_kept_playlist_id(id: &str) -> bool {
    id.starts_with("of") && is_octo_playlist_id(id)
}

/// The first value of `key` that names a song from outside the library, if any.
fn outside_song_param(state: &AppState, call: &SubsonicCall, key: &str) -> Option<String> {
    call.incoming
        .parameter_values(key)
        .into_iter()
        .map(|id| id.trim().to_string())
        .find(|id| !id.is_empty() && state.local_library.parse_song_id(id).0)
}

/// Whether a new playlist is one for Octo to keep: `octo=true`, or a `songId` from outside the
/// library.
fn wants_kept_playlist(state: &AppState, call: &SubsonicCall) -> bool {
    call.param("octo")
        .is_some_and(|octo| octo.eq_ignore_ascii_case("true") || octo == "1")
        || outside_song_param(state, call, "songId").is_some()
}

/// `createPlaylist` with no playlist id, for a playlist Octo keeps: made with its `songId`s in
/// order, and answered with the playlist as Navidrome answers.
async fn create_kept_playlist(state: &AppState, call: &SubsonicCall) -> Response {
    let caller = match playlist_caller(state, call).await {
        Ok(caller) => caller,
        Err(answer) => return answer,
    };
    let Some(name) = call.param("name").map(str::trim).filter(|name| !name.is_empty()) else {
        return family_error(state, call, 10, "Required parameter is missing: name");
    };
    let store = &state.family_playlists;
    let made = match family_playlists::create(store, &caller, name, false, Utc::now()) {
        Ok(made) => made,
        Err(error) => return playlist_error(state, call, error),
    };
    if let Err(error) = add_songs(state, call, &caller, &made.id, "songId").await {
        return playlist_error(state, call, error);
    }
    family_playlist(state, call, &made.id).await
}

/// Adds the songs named by every value of `key` to the end of a kept playlist.
async fn add_songs(
    state: &AppState,
    call: &SubsonicCall,
    caller: &FamilyCaller,
    id: &str,
    key: &str,
) -> Result<usize, PlaylistError> {
    let mut additions = Vec::new();
    for song in call.incoming.parameter_values(key) {
        if let Some(entry) = new_entry(state, &song).await {
            additions.push(entry);
        }
    }
    if additions.is_empty() {
        return Ok(0);
    }
    let requests = state.family_playlist_requests.get();
    family_playlists::add(
        &state.family_playlists,
        requests.as_ref(),
        caller,
        id,
        additions,
        Utc::now(),
    )
}

/// A playlist call that may name a family playlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaylistChange {
    Create,
    Update,
    Delete,
}

/// Which playlist call this is, from the last segment of its path.
fn playlist_change(call: &SubsonicCall) -> Option<PlaylistChange> {
    let path = call.path();
    let name = path.rsplit('/').next().unwrap_or("");
    let name = name.strip_suffix(".view").unwrap_or(name);
    [
        ("createPlaylist", PlaylistChange::Create),
        ("updatePlaylist", PlaylistChange::Update),
        ("deletePlaylist", PlaylistChange::Delete),
    ]
    .into_iter()
    .find(|(endpoint, _)| eq_ignore_case(endpoint, name))
    .map(|(_, change)| change)
}

/// A Subsonic error in the call's format.
fn family_error(state: &AppState, call: &SubsonicCall, code: i32, message: &str) -> Response {
    state
        .subsonic_response_builder
        .create_error(&call.format, code, message)
        .into_response()
}

fn playlist_error(state: &AppState, call: &SubsonicCall, error: PlaylistError) -> Response {
    let code = match error {
        PlaylistError::NotFound => 70,
        PlaylistError::NotAllowed(_) => 50,
        PlaylistError::BlankName => 10,
    };
    family_error(state, call, code, error.message())
}

/// Whether the request's sign-in is good, before Octo answers with something of the caller's
/// own. While Family is on, the family gate's reading checks exactly the sign-in it read;
/// otherwise Navidrome is pinged with the request's own parameters, as it always was.
async fn signed_in(state: &AppState, call: &SubsonicCall, format: &str) -> bool {
    if let Some(reading) = GateReading::of(&call.parts.extensions) {
        return reading.verify(state).await.is_ok();
    }
    let mut auth_only = call.parameters.clone();
    auth_only.shift_remove("id");
    let check = call.proxy.relay_safe("rest/ping", &auth_only).await;
    check.is_some_and(|check| is_successful_subsonic_response(&check.body, format))
}

/// The caller of a family playlist call, checked by the family gate's reading with exactly the
/// sign-in it read, or the answer to send instead.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
async fn checked_caller(state: &AppState, call: &SubsonicCall) -> Result<FamilyCaller, Response> {
    let Some(reading) = GateReading::of(&call.parts.extensions) else {
        return Err(playlist_error(state, call, PlaylistError::NotFound));
    };
    match reading.verify(state).await {
        Ok(verified) => Ok(verified.caller().clone()),
        Err(Unverified::Missing) => Err(playlist_error(state, call, PlaylistError::NotFound)),
        Err(Unverified::Refused) => Err(family_error(state, call, 40, "Wrong username or password")),
        Err(Unverified::Unreachable) => Err(family_error(
            state,
            call,
            0,
            "Octo can't reach Navidrome to check who is asking",
        )),
    }
}

/// The caller of a kept playlist call: while Family is on, as the family gate read and checked
/// it; otherwise the account Navidrome accepts the request's own sign-in for.
#[allow(clippy::result_large_err)] // the error is the answer itself, sent at once
async fn playlist_caller(state: &AppState, call: &SubsonicCall) -> Result<FamilyCaller, Response> {
    if state.settings.current().family.enabled {
        return checked_caller(state, call).await;
    }
    match refuse_unless_signed_in(state, &caller_call(call), &call.format).await {
        Ok(None) => {}
        Ok(Some(answer)) => return Err(answer),
        Err(error) => return Err(error.into_response()),
    }
    let user = match signed_in_user(state, &caller_call(call)).await {
        Ok(user) => user,
        Err(error) => return Err(error.into_response()),
    };
    if user.trim().is_empty() {
        return Err(family_error(state, call, 40, "Wrong username or password"));
    }
    Ok(kept_caller(&user))
}

/// A kept playlist, each entry resolved for the caller: the library song (read as the caller)
/// when it is shared or the caller's own, otherwise its outside version.
async fn family_playlist(state: &AppState, call: &SubsonicCall, id: &str) -> Response {
    let caller = match playlist_caller(state, call).await {
        Ok(caller) => caller,
        Err(answer) => return answer,
    };
    let caller = &caller;
    let Some(playlist) = find_kept_playlist(state, caller, id) else {
        return playlist_error(state, call, PlaylistError::NotFound);
    };

    let shown = family_playlists::shown_entries(&playlist, caller);
    let entries: Vec<Value> = futures::stream::iter(shown)
        .map(|entry| async move {
            match entry.serve {
                Serve::Library { id, outside } => match library_song_json(call, &id).await {
                    Some(song) => Some(song),
                    None => match outside {
                        Some(outside) => outside_song_json(state, &outside).await,
                        None => None,
                    },
                },
                Serve::Outside(outside) => outside_song_json(state, &outside).await,
            }
        })
        .buffered(FAMILY_SONGS_AT_ONCE)
        .filter_map(|song| async move { song })
        .collect()
        .await;
    // The whole list's count and length; a kid profile's answer rule then takes out the
    // explicit songs and lowers both.
    let seconds = entries
        .iter()
        .map(|song| song.get("duration").and_then(Value::as_i64).unwrap_or(0))
        .sum();
    let family_on = state.settings.current().family.enabled;
    let fields = family_playlist_fields(&playlist, caller, family_on, entries.len(), seconds);
    state
        .subsonic_response_builder
        .create_chart_playlist_response(&call.format, fields, &entries)
        .into_response()
}

/// A library song as Navidrome describes it to the caller, in JSON.
async fn library_song_json(call: &SubsonicCall, id: &str) -> Option<Value> {
    let mut request = call.parameters.clone();
    request.insert("id".into(), id.to_string());
    request.insert("f".into(), "json".into());
    request.shift_remove("callback");
    let relay = call.proxy.relay_safe("rest/getSong", &request).await?;
    let answer: Value = serde_json::from_slice(&relay.body).ok()?;
    let response = answer.get("subsonic-response")?;
    if response.get("status").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    response.get("song").filter(|song| song.is_object()).cloned()
}

/// An outside song in the shape every outside song has.
async fn outside_song_json(state: &AppState, id: &str) -> Option<Value> {
    let (external, provider, external_id) = state.local_library.parse_song_id(id);
    if !external {
        return None;
    }
    let song = state
        .metadata_service
        .get_song(
            provider.as_deref().unwrap_or_default(),
            external_id.as_deref().unwrap_or_default(),
        )
        .await?;
    Some(Value::Object(
        state.subsonic_response_builder.convert_song_to_json(&song),
    ))
}

/// `createPlaylist` (with the playlist's id: its songs become exactly `songId`, in order),
/// `updatePlaylist` and `deletePlaylist` on a kept playlist.
async fn family_mutate(state: &AppState, call: &SubsonicCall, id: &str, change: PlaylistChange) -> Response {
    let store = &state.family_playlists;
    // Nothing here goes to Navidrome, so the caller is checked before a change is made in
    // anyone's name.
    let caller = match playlist_caller(state, call).await {
        Ok(caller) => caller,
        Err(answer) => return answer,
    };
    let caller = &caller;
    let Some(playlist) = find_kept_playlist(state, caller, id) else {
        return playlist_error(state, call, PlaylistError::NotFound);
    };
    let done = || {
        state
            .subsonic_response_builder
            .create_response(&call.format, "playlist")
            .into_response()
    };
    match change {
        PlaylistChange::Create => {
            if !family_playlists::may_edit(caller) {
                return playlist_error(
                    state,
                    call,
                    PlaylistError::NotAllowed("This account cannot change family playlists."),
                );
            }
            let name = call.param("name").map(str::trim).filter(|n| !n.is_empty());
            if let Some(name) = name
                && name != playlist.name
            {
                if !family_playlists::may_rename(caller, &playlist) {
                    return playlist_error(
                        state,
                        call,
                        PlaylistError::NotAllowed(
                            "Only the person who made this playlist or a family manager can change it.",
                        ),
                    );
                }
                if let Err(error) = family_playlists::rename(store, caller, id, name) {
                    return playlist_error(state, call, error);
                }
            }
            // Everything the caller is shown goes, then the new list goes in.
            let shown = family_playlists::shown_entries(&playlist, caller);
            let all: Vec<usize> = (0..shown.len()).collect();
            if let Err(error) = family_playlists::remove_shown(store, caller, id, &all) {
                return playlist_error(state, call, error);
            }
            if let Err(error) = add_songs(state, call, caller, id, "songId").await {
                return playlist_error(state, call, error);
            }
            family_playlist(state, call, id).await
        }
        PlaylistChange::Delete => match family_playlists::delete(store, caller, id) {
            Ok(()) => done(),
            Err(error) => playlist_error(state, call, error),
        },
        PlaylistChange::Update => {
            if !family_playlists::may_edit(caller) {
                return playlist_error(
                    state,
                    call,
                    PlaylistError::NotAllowed("This account cannot change family playlists."),
                );
            }
            let name = call.param("name").map(str::trim).filter(|n| !n.is_empty());
            let renames = name.is_some_and(|name| name != playlist.name);
            let comments = call.param("comment").is_some_and(|c| !c.trim().is_empty());
            if (renames || comments) && !family_playlists::may_rename(caller, &playlist) {
                return playlist_error(
                    state,
                    call,
                    PlaylistError::NotAllowed(
                        "Only the person who made this playlist or a family manager can change it.",
                    ),
                );
            }
            if renames
                && let Some(name) = name
                && let Err(error) = family_playlists::rename(store, caller, id, name)
            {
                return playlist_error(state, call, error);
            }
            let removals: Vec<usize> = call
                .incoming
                .parameter_values("songIndexToRemove")
                .iter()
                .filter_map(|index| index.trim().parse().ok())
                .collect();
            if !removals.is_empty()
                && let Err(error) = family_playlists::remove_shown(store, caller, id, &removals)
            {
                return playlist_error(state, call, error);
            }
            if let Err(error) = add_songs(state, call, caller, id, "songIdToAdd").await {
                return playlist_error(state, call, error);
            }
            done()
        }
    }
}

/// What a family playlist keeps for a song id: a library song with its scope (a member's own
/// library, else shared) and its outside id when Octo knows one, or an outside id alone.
async fn new_entry(state: &AppState, id: &str) -> Option<NewEntry> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let (external, _, _) = state.local_library.parse_song_id(id);
    if external {
        let (duration, explicit_status) = outside_lookup(state, id).await;
        return Some(NewEntry {
            outside_id: Some(id.to_string()),
            duration,
            explicit_status,
            ..NewEntry::default()
        });
    }
    let indexed = state.library_index.song(id);
    let scope = indexed
        .as_ref()
        .and_then(|song| song.library_id)
        .and_then(|library| state.library_scope.personal_library_owner(library))
        .map_or_else(|| SHARED_SCOPE.to_string(), |owner| personal_scope(&owner));
    let outside_id = outside_id_of(state, id, indexed.as_ref().map(|song| song.path.as_str())).await;
    let (duration, explicit_status) = match &outside_id {
        Some(outside) => outside_lookup(state, outside).await,
        None => (None, None),
    };
    Some(NewEntry {
        library_id: Some(id.to_string()),
        scope,
        outside_id,
        duration,
        explicit_status,
    })
}
