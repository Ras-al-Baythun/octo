//! The download log's lines ([`AcquisitionEvent`] and [`AcquisitionCandidate`]), as a
//! fetched-songs entry keeps them in `downloads-history.json`. The tracker that writes them is
//! `octo::services::common::acquisition_tracker`.

use chrono::{DateTime, Utc};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};

use crate::common::dotnet::{format_optional_decimals, to_upper_invariant};
use crate::json::datetime;
use crate::models::null_as_default;

/// What one line of a download's log is about, so an app can draw it. Sent as is; an app that
/// meets a kind it does not know draws a plain line.
pub mod acquisition_event_kinds {
    pub const QUEUED: &str = "queued";
    pub const SEARCH: &str = "search";
    pub const FOUND: &str = "found";
    pub const TRY: &str = "try";
    pub const TRANSFER: &str = "transfer";
    pub const CHECK: &str = "check";
    pub const TAGS: &str = "tags";
    pub const COVER: &str = "cover";
    pub const LYRICS: &str = "lyrics";
    pub const LIBRARY: &str = "library";
    pub const DONE: &str = "done";
    pub const FAILED: &str = "failed";
    pub const NOTE: &str = "note";
}

/// One line of a download's log: when, what kind of step, the words, an optional second line,
/// and the copies a search offered when the step is about them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AcquisitionEvent {
    #[serde(default = "datetime::min_value", with = "datetime::utc")]
    pub at: DateTime<Utc>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub kind: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub text: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub candidates: Option<Vec<AcquisitionCandidate>>,
}

/// One copy a source offered, as the log and Find songs show it. Every figure the source gave is
/// kept; any of them may be missing. Rank is Octo's order of preference among the copies it would
/// try (1 is first), None for a copy it would pass over, and Note says why it would.
///
/// Written by hand ([`Serialize`] below) because the computed `Quality` is written too.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "PascalCase", default)]
pub struct AcquisitionCandidate {
    #[serde(deserialize_with = "null_as_default")]
    pub source: String,
    pub file: Option<String>,
    pub folder: Option<String>,
    pub format: Option<String>,
    pub bit_rate: Option<i32>,
    pub bit_depth: Option<i32>,
    pub sample_rate: Option<i32>,
    pub size: Option<i64>,
    pub length: Option<i32>,
    pub rank: Option<i32>,
    pub note: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
}

impl AcquisitionCandidate {
    /// "FLAC 16-bit 44.1 kHz", "MP3 320 kbps", or just the format.
    pub fn quality(&self) -> Option<String> {
        quality_text(
            self.format.as_deref(),
            self.bit_rate,
            self.bit_depth,
            self.sample_rate,
        )
    }
}

/// A candidate's quality in words: the format upper-cased without its dot, then the bit depth
/// and sample rate ("16-bit 44.1 kHz", the kHz with at most one decimal), else the bit rate.
pub fn quality_text(
    format: Option<&str>,
    bit_rate: Option<i32>,
    bit_depth: Option<i32>,
    sample_rate: Option<i32>,
) -> Option<String> {
    let kind = to_upper_invariant(format?.trim().trim_start_matches('.'));
    if kind.is_empty() {
        return None;
    }
    if let (Some(depth), Some(rate)) = (bit_depth.filter(|d| *d > 0), sample_rate.filter(|r| *r > 0)) {
        let khz = format_optional_decimals(f64::from(rate) / 1000.0, 1);
        return Some(format!("{kind} {depth}-bit {khz} kHz"));
    }
    if let Some(rate) = bit_rate.filter(|r| *r > 0) {
        return Some(format!("{kind} {rate} kbps"));
    }
    Some(kind)
}

impl Serialize for AcquisitionCandidate {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("AcquisitionCandidate", 14)?;
        s.serialize_field("Source", &self.source)?;
        s.serialize_field("File", &self.file)?;
        s.serialize_field("Folder", &self.folder)?;
        s.serialize_field("Format", &self.format)?;
        s.serialize_field("BitRate", &self.bit_rate)?;
        s.serialize_field("BitDepth", &self.bit_depth)?;
        s.serialize_field("SampleRate", &self.sample_rate)?;
        s.serialize_field("Size", &self.size)?;
        s.serialize_field("Length", &self.length)?;
        s.serialize_field("Rank", &self.rank)?;
        s.serialize_field("Note", &self.note)?;
        s.serialize_field("Title", &self.title)?;
        s.serialize_field("Album", &self.album)?;
        s.serialize_field("Quality", &self.quality())?;
        s.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_reads_as_the_dashboard_shows_it() {
        let cases: [(Option<&str>, Option<i32>, Option<i32>, Option<i32>, Option<&str>); 7] = [
            (
                Some("flac"),
                None,
                Some(16),
                Some(44100),
                Some("FLAC 16-bit 44.1 kHz"),
            ),
            (
                Some(".flac"),
                None,
                Some(24),
                Some(96000),
                Some("FLAC 24-bit 96 kHz"),
            ),
            (Some("mp3"), Some(320), None, None, Some("MP3 320 kbps")),
            (Some("m4a"), Some(0), Some(16), None, Some("M4A")),
            (Some("  "), Some(320), None, None, None),
            (None, Some(320), None, None, None),
            (
                Some("flac"),
                None,
                Some(16),
                Some(22050),
                Some("FLAC 16-bit 22.1 kHz"),
            ),
        ];
        for (format, bit_rate, depth, rate, expected) in cases {
            assert_eq!(
                quality_text(format, bit_rate, depth, rate).as_deref(),
                expected,
                "{format:?}"
            );
        }
    }
}
