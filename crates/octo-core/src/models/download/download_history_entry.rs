//! The entries of `downloads-history.json`.

use serde::{Deserialize, Serialize};

use crate::tagging::tag_plan::TagReport;

/// One entry in the running log of songs Octo has fetched (via download-on-star or
/// permanent-mode playback). Surfaced in the admin dashboard's "Fetched songs" view.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct DownloadHistoryEntry {
    // A JSON null in these strings reads as the default.
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub artist: String,
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub title: String,
    pub album: Option<String>,

    /// Absolute path the file was saved to.
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub path: String,

    /// File format, upper-cased from the extension (FLAC, MP3, M4A).
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub format: String,

    /// Where it came from: "Lidarr" (FLAC) or "YouTube" (MP3).
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub source: String,

    /// Cover art URL (Deezer), for the thumbnail in the log.
    pub cover_art_url: Option<String>,

    pub size_bytes: i64,

    /// For a file that claims to be lossless and whose spectrum says it was made from a lossy
    /// one, what it was likely made from ("about 128 kbps MP3"). Null otherwise.
    pub transcoded_from: Option<String>,

    /// How the file was tagged: the release that won, by how much, and where each field
    /// came from. Null for entries written before this existed.
    pub tagging: Option<TagReport>,

    /// When it was saved (ISO 8601, UTC).
    #[serde(deserialize_with = "crate::models::null_as_default")]
    pub downloaded_at: String,

    /// Who asked for this file, when Octo could tell. A star or a play carries the Subsonic
    /// username; an acquisition Octo started itself carries nobody, and so does every entry
    /// written before this field existed.
    ///
    /// A list rather than one name, because a second user starring a track that is already
    /// being fetched joins that transfer instead of starting another. Recording only whoever
    /// got there first would attribute the file to one of them and silently drop the rest.
    pub requested_by: Option<Vec<String>>,

    /// The download's row in the live downloads list ("provider:id"), the key getAcquisition and
    /// the dashboard read its log by. None for entries written before this existed.
    pub key: Option<String>,

    /// The download's log, saved when it ended so the dashboard can show it after the live list
    /// forgets it (three hours). Trimmed to stay small (`DownloadHistoryService::attach_log`).
    /// None for entries written before this existed, and for older entries whose log was let go
    /// to keep the file small.
    pub log: Option<Vec<super::acquisition_event::AcquisitionEvent>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downloads_history_fixture_round_trips_byte_for_byte() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/rust-migration/fixtures/state/downloads-history.json"
        );
        let text = std::fs::read_to_string(path).expect("the fixture is in the repo");

        let entries: Vec<DownloadHistoryEntry> = serde_json::from_str(&text).expect("the fixture reads");
        assert!(entries.iter().any(|entry| entry.tagging.is_some()));
        assert!(entries.iter().any(|entry| entry.tagging.is_none()));

        assert!(entries.iter().any(|entry| entry.log.is_some()));
        assert_eq!(
            crate::json::to_string_without_nulls(&entries),
            text.trim_end_matches('\n')
        );
    }

    #[test]
    fn a_null_in_a_string_field_still_reads() {
        // A hand-edited null in a string field must not lose the whole log.
        let entries: Vec<DownloadHistoryEntry> = serde_json::from_str(
            r#"[{"Artist":null,"Title":"T","Tagging":{"Confidence":null,"Notes":null}}]"#,
        )
        .expect("reads");
        assert_eq!(entries[0].artist, "");
        assert_eq!(entries[0].title, "T");
        assert!(
            entries[0]
                .tagging
                .as_ref()
                .is_some_and(|tagging| tagging.notes.is_empty())
        );
    }
}
