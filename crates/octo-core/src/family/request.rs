//! A member's request for a copy of a song (or an album, a better copy, an import list, or a
//! family playlist addition), as `family-requests.json` keeps it, and the weekly quota.

use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::abilities::RequestQuality;
use crate::common::dotnet::eq_ignore_case;
use crate::json::datetime;

/// What is being asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RequestKind {
    Song,
    Album,
    /// A better copy of a shared-library song.
    Upgrade,
    /// One import list's missing songs.
    Import,
    /// An addition to a family playlist that needs approval.
    PlaylistAdd,
}

/// Where a request has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RequestState {
    Pending,
    Approved,
    Declined,
    Cancelled,
    Done,
    Failed,
}

/// How a `Done` request was met.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RequestOutcome {
    /// Octo downloaded a copy.
    Downloaded,
    /// The song was already in the shared library.
    AlreadyShared,
    /// Another member already had it on disk; Octo linked it (no download, no new disk).
    AddedFromFamily,
}

/// One request, as `family-requests.json` keeps it (PascalCase like every Octo state file).
/// [`FamilyRequest::api_json`] gives the `octoFamily` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FamilyRequest {
    pub id: String,
    pub username: String,
    pub kind: RequestKind,
    /// An outside id (`ext-...`), a library song id for `Upgrade`, an import list id for
    /// `Import`, or `playlist:<id>:<entry>` for `PlaylistAdd`.
    pub target: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub cover_art: String,
    pub quality: RequestQuality,
    pub state: RequestState,
    #[serde(with = "datetime::utc")]
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub decided_by: Option<String>,
    #[serde(default, with = "datetime::utc_option")]
    pub decided: Option<DateTime<Utc>>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub library_song_id: Option<String>,
    #[serde(default)]
    pub failure: Option<String>,
    #[serde(default)]
    pub outcome: Option<RequestOutcome>,
}

impl FamilyRequest {
    /// The request as `getFamilyRequests` and `/api/family/*` answer it.
    pub fn api_json(&self, display_name: &str) -> Value {
        json!({
            "id": self.id,
            "username": self.username,
            "displayName": display_name,
            "kind": self.kind,
            "target": self.target,
            "title": self.title,
            "artist": self.artist,
            "album": self.album,
            "coverArt": self.cover_art,
            "quality": self.quality,
            "state": self.state,
            "created": datetime::format_utc(&self.created),
            "decided": self.decided.as_ref().map(datetime::format_utc),
            "decidedBy": self.decided_by,
            "note": self.note,
            "failure": self.failure,
            "librarySongId": self.library_song_id,
            "outcome": self.outcome,
        })
    }

    /// Whether this request takes one of the member's weekly slots: asked for in the last 7
    /// days, not declined, cancelled or failed, and not met without a download (a song that
    /// was already shared, or linked from another member's copy), and not a playlist addition.
    pub fn counts_toward_quota(&self, now: DateTime<Utc>) -> bool {
        let live = matches!(
            self.state,
            RequestState::Pending | RequestState::Approved | RequestState::Done
        );
        let free = matches!(
            self.outcome,
            Some(RequestOutcome::AlreadyShared | RequestOutcome::AddedFromFamily)
        );
        let copy = self.kind != RequestKind::PlaylistAdd;
        live && !free && copy && self.created > now - TimeDelta::days(7) && self.created <= now
    }
}

/// How many of `user`'s weekly request slots are in use at `now` (names compared ignoring
/// case).
pub fn quota_used(requests: &[FamilyRequest], user: &str, now: DateTime<Utc>) -> u32 {
    requests
        .iter()
        .filter(|r| eq_ignore_case(&r.username, user) && r.counts_toward_quota(now))
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 18, 0, 0).unwrap()
    }

    fn request(user: &str, state: RequestState, days_ago: i64) -> FamilyRequest {
        FamilyRequest {
            id: format!("r_{user}_{days_ago}"),
            username: user.to_string(),
            kind: RequestKind::Song,
            target: "ext-deezer-song-1".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            cover_art: String::new(),
            quality: RequestQuality::Best,
            state,
            created: now() - TimeDelta::days(days_ago),
            decided_by: None,
            decided: None,
            note: String::new(),
            library_song_id: None,
            failure: None,
            outcome: None,
        }
    }

    #[test]
    fn declined_and_cancelled_do_not_count() {
        let requests = [
            request("alex", RequestState::Pending, 1),
            request("alex", RequestState::Approved, 1),
            request("alex", RequestState::Done, 1),
            request("alex", RequestState::Declined, 1),
            request("alex", RequestState::Cancelled, 1),
            request("alex", RequestState::Failed, 1),
        ];
        assert_eq!(quota_used(&requests, "alex", now()), 3);
    }

    #[test]
    fn older_than_a_week_does_not_count() {
        let requests = [
            request("alex", RequestState::Done, 6),
            request("alex", RequestState::Done, 7),
            request("alex", RequestState::Pending, 30),
        ];
        assert_eq!(quota_used(&requests, "alex", now()), 1);
    }

    #[test]
    fn other_members_do_not_count() {
        let requests = [
            request("alex", RequestState::Pending, 0),
            request("ALEX", RequestState::Pending, 0),
            request("sam", RequestState::Pending, 0),
        ];
        assert_eq!(quota_used(&requests, "Alex", now()), 2);
        assert_eq!(quota_used(&requests, "sam", now()), 1);
        assert_eq!(quota_used(&requests, "nobody", now()), 0);
    }

    #[test]
    fn songs_met_without_a_download_do_not_count() {
        let mut shared = request("alex", RequestState::Done, 0);
        shared.outcome = Some(RequestOutcome::AlreadyShared);
        let mut linked = request("alex", RequestState::Done, 0);
        linked.outcome = Some(RequestOutcome::AddedFromFamily);
        let mut downloaded = request("alex", RequestState::Done, 0);
        downloaded.outcome = Some(RequestOutcome::Downloaded);
        assert_eq!(quota_used(&[shared, linked, downloaded], "alex", now()), 1);
    }

    #[test]
    fn store_and_api_shapes() {
        let mut r = request("alex", RequestState::Done, 0);
        r.outcome = Some(RequestOutcome::AddedFromFamily);
        r.library_song_id = Some("nd1".into());
        let stored = serde_json::to_value(&r).expect("serializes");
        assert_eq!(stored["Kind"], "Song");
        assert_eq!(stored["Outcome"], "AddedFromFamily");
        assert_eq!(stored["Created"], "2026-10-20T18:00:00Z");
        let back: FamilyRequest = serde_json::from_value(stored).expect("reads back");
        assert_eq!(back, r);

        let api = r.api_json("Alex");
        assert_eq!(api["displayName"], "Alex");
        assert_eq!(api["outcome"], "AddedFromFamily");
        assert_eq!(api["librarySongId"], "nd1");
        assert_eq!(api["decided"], Value::Null);
        let keys: Vec<&str> = api
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "id",
                "username",
                "displayName",
                "kind",
                "target",
                "title",
                "artist",
                "album",
                "coverArt",
                "quality",
                "state",
                "created",
                "decided",
                "decidedBy",
                "note",
                "failure",
                "librarySongId",
                "outcome"
            ]
        );
    }
}
