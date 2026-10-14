//! The data half of download verification: the verdict, why it was inconclusive, and the
//! result a download carries. The service (fingerprinting, the AcoustID and MusicBrainz calls,
//! the decision and its helpers) is `octo::services::fingerprint::download_verification_service`.

use std::fmt;

use crate::common::song_identity::SongIdentity;
use crate::fingerprint::acoust_id_client::{AcoustIdLookup, AcoustIdRecording};
use crate::models::domain::song::Song;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum VerificationVerdict {
    /// AcoustID identified the file as the track that was asked for, or the ISRC that
    /// was asked for is on the file's tags or on the recording AcoustID named.
    Confirmed,

    /// Nothing could be established: verification off, no key, no fpcalc, AcoustID down or
    /// rate limited, no result above the score threshold, or no AcoustID entry at all. The
    /// file is kept and nothing is remembered.
    ///
    /// The last of those is the one that matters. A legitimately obscure track, which is the
    /// music Octo exists to find, has no AcoustID entry. Treating
    /// absence as evidence would make this feature worst exactly where the library is rarest.
    #[default]
    Inconclusive,

    /// The file was identified with confidence and it is a different recording, or it holds
    /// no decodable audio at all. The only verdict that discards a file or writes a denial.
    Mismatch,
}

impl VerificationVerdict {
    /// The verdict's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Confirmed => "Confirmed",
            Self::Inconclusive => "Inconclusive",
            Self::Mismatch => "Mismatch",
        }
    }
}

impl fmt::Display for VerificationVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a verdict was Inconclusive, or, for the last two, why the library sweep asks about a file
/// whose verdict was not. NoEntry, BelowThreshold and SourceDisagreed are questions a person can
/// settle by listening, so only those reach the Review playlist from a download (#47). The sweep
/// also asks about a confident answer, because it never acts on one (#72). Appended, never
/// inserted: the notice queue stores these by name, but order is still what the code reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum InconclusiveReason {
    #[default]
    None,
    Disabled,
    NotFingerprinted,
    LookupFailed,
    NoEntry,
    BelowThreshold,
    SourceDisagreed,
    SoundsLikeAnother,
    LengthOff,
}

impl InconclusiveReason {
    /// Every member, in declaration order.
    pub const ALL: [InconclusiveReason; 9] = [
        Self::None,
        Self::Disabled,
        Self::NotFingerprinted,
        Self::LookupFailed,
        Self::NoEntry,
        Self::BelowThreshold,
        Self::SourceDisagreed,
        Self::SoundsLikeAnother,
        Self::LengthOff,
    ];

    /// The reason's name, which the notice queue stores.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Disabled => "Disabled",
            Self::NotFingerprinted => "NotFingerprinted",
            Self::LookupFailed => "LookupFailed",
            Self::NoEntry => "NoEntry",
            Self::BelowThreshold => "BelowThreshold",
            Self::SourceDisagreed => "SourceDisagreed",
            Self::SoundsLikeAnother => "SoundsLikeAnother",
            Self::LengthOff => "LengthOff",
        }
    }
}

impl fmt::Display for InconclusiveReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What AcoustID said about a downloaded file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VerificationResult {
    pub verdict: VerificationVerdict,
    pub score: f64,
    pub matched_title: Option<String>,
    pub matched_artist: Option<String>,
    pub matched_album: Option<String>,
    pub matched_year: Option<i32>,
    pub recording_id: Option<String>,
    pub deny_reason: String,
    pub tags_authoritative: bool,

    pub reason: InconclusiveReason,

    /// The recording that agreed with the request. Set when Confirmed, and when the right song
    /// and artist were doubted only for their length (LengthOff), to say how long it runs.
    pub r#match: Option<AcoustIdRecording>,

    /// Kept so a person's confirmation can be sent back to AcoustID (#47).
    pub fingerprint: Option<String>,
    pub duration_seconds: i32,

    /// The whole answer, every result and release, so the chooser can weigh them all.
    /// Set whenever the service answered, even below the threshold.
    pub lookup: Option<AcoustIdLookup>,

    /// The service's id for the fingerprint that confirmed the recording.
    pub acoust_id: Option<String>,

    /// The one recording AcoustID proposed below the threshold that agrees with the request on
    /// title, artist and length. The first MusicBrainz id a person's Keep may submit, and None
    /// when there were none or more than one.
    pub candidate_recording_id: Option<String>,

    /// What confirmed or kept the file when it was not the fingerprint's title and artist alone,
    /// in words for the log: an ISRC in the file's tags, or one on the MusicBrainz recording the
    /// fingerprint named. None otherwise.
    pub evidence: Option<String>,
}

impl VerificationResult {
    /// `VerificationResult.Inconclusive`: verification is off.
    pub fn inconclusive() -> Self {
        Self {
            reason: InconclusiveReason::Disabled,
            ..Default::default()
        }
    }

    pub fn needs_review(&self) -> bool {
        self.verdict == VerificationVerdict::Inconclusive
            && matches!(
                self.reason,
                InconclusiveReason::NoEntry
                    | InconclusiveReason::BelowThreshold
                    | InconclusiveReason::SourceDisagreed
                    | InconclusiveReason::LengthOff
            )
    }

    /// Whether a song's album IS the MusicBrainz release its fingerprint matched, so the ids and
    /// artwork that belong to that release describe the album the tags name. A download tagged
    /// with a compilation's name must not get the original album's cover or group id.
    pub fn album_is_from_release(song: &Song) -> bool {
        match song.music_brainz_album_title.as_deref() {
            Some(title) if !crate::common::dotnet::is_blank(title) => {
                SongIdentity::key(&song.album) == SongIdentity::key(title)
            }
            _ => false,
        }
    }

    pub fn describe(&self) -> String {
        let artist = self.matched_artist.as_deref().unwrap_or("");
        let title = self.matched_title.as_deref().unwrap_or("");
        if artist.is_empty() && title.is_empty() {
            "a different recording".to_string()
        } else {
            format!("'{artist} - {title}'")
        }
    }

    /// Record what a confirmed match proved, and overwrite the song's name from it when
    /// tagging from MusicBrainz is on.
    ///
    /// The ids are written whenever the match is confirmed. An id says what the file IS and
    /// changes nothing a person reads, and it is what lets every later pass skip identifying
    /// the same file again (#48). The release id stays in memory for the cover lookup and is
    /// never written: Navidrome groups albums by MUSICBRAINZ_ALBUMID before the album name, so
    /// one track carrying it beside another without it would split an album in two.
    ///
    /// The name overwrite is unconditional where the Deezer fill is conditional. That one fills
    /// only what is missing, because a peer's own tags beat nothing. A confirmed fingerprint
    /// match beats the peer, which is the entire point of the setting.
    pub fn apply_tags_to(&self, song: &mut Song) {
        if self.verdict != VerificationVerdict::Confirmed {
            return;
        }

        if let Some(id) = self.recording_id.as_deref().filter(|id| !id.is_empty()) {
            song.music_brainz_recording_id = Some(id.to_string());
        }
        if let Some(found) = &self.r#match {
            if found.credits.len() > 1 {
                song.artists = found.credits.iter().map(|credit| credit.name.clone()).collect();
            }
            if found.credits.len() == 1
                && let Some(artist_id) = found.credits[0].artist_id.as_deref().filter(|id| !id.is_empty())
            {
                song.music_brainz_artist_ids = vec![artist_id.to_string()];
            }
            if let Some(primary) = found.primary_artist().filter(|primary| !primary.is_empty()) {
                song.primary_artist = Some(primary.to_string());
            }
            song.music_brainz_release_id = found.release.as_ref().and_then(|r| r.release_id.clone());
            song.music_brainz_release_group_id =
                found.release.as_ref().and_then(|r| r.release_group_id.clone());
            song.music_brainz_album_title = found.album_title.clone();
        }

        if !self.tags_authoritative {
            return;
        }
        if let Some(title) = self.matched_title.as_deref().filter(|v| !v.is_empty()) {
            song.title = title.to_string();
        }
        if let Some(artist) = self.matched_artist.as_deref().filter(|v| !v.is_empty()) {
            song.artist = artist.to_string();
        }
        let matched_album = self.matched_album.as_deref().filter(|v| !v.is_empty());
        if let Some(album) = matched_album {
            song.album = album.to_string();
        }
        if let Some(year) = self.matched_year.filter(|&year| year > 0) {
            song.year = Some(year);
        }

        if let (Some(release), Some(_)) = (
            self.r#match.as_ref().and_then(|m| m.release.as_ref()),
            matched_album,
        ) {
            if let Some(track) = release.track_number.filter(|&n| n > 0) {
                song.track = Some(track);
            }
            if let Some(count) = release.track_count.filter(|&n| n > 0) {
                song.total_tracks = Some(count);
            }
            if let Some(disc) = release.disc_number.filter(|&n| n > 0) {
                song.disc_number = Some(disc);
            }
            if let Some(album_artist) = release.album_artist.as_deref().filter(|v| !v.is_empty()) {
                song.album_artist = Some(album_artist.to_string());
            }
            song.is_compilation = release.is_compilation;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint::acoust_id_client::{AcoustIdCredit, AcoustIdRelease};

    #[test]
    fn needs_review_only_for_questions_a_person_can_settle() {
        let cases = [
            (InconclusiveReason::Disabled, false),
            (InconclusiveReason::NotFingerprinted, false),
            (InconclusiveReason::LookupFailed, false),
            (InconclusiveReason::NoEntry, true),
            (InconclusiveReason::BelowThreshold, true),
            (InconclusiveReason::SourceDisagreed, true),
            (InconclusiveReason::LengthOff, true),
            (InconclusiveReason::SoundsLikeAnother, false),
        ];
        for (reason, expected) in cases {
            let result = VerificationResult {
                reason,
                ..Default::default()
            };
            assert_eq!(result.needs_review(), expected, "{reason}");
        }
    }

    #[test]
    fn defaults_are_an_inconclusive_result_with_no_reason() {
        let result = VerificationResult::default();
        assert_eq!(result.verdict, VerificationVerdict::Inconclusive);
        assert_eq!(result.reason, InconclusiveReason::None);
        assert_eq!(result.deny_reason, "");
        assert_eq!(
            VerificationResult::inconclusive().reason,
            InconclusiveReason::Disabled
        );
        assert_eq!(result.describe(), "a different recording");
        let named = VerificationResult {
            matched_artist: Some("Massive Attack".into()),
            matched_title: Some("Teardrop".into()),
            ..Default::default()
        };
        assert_eq!(named.describe(), "'Massive Attack - Teardrop'");
    }

    /// Applying tags from a confirmed result: when its tags are not authoritative it still
    /// records the recording id, and when they are it takes the release's track and album
    /// artist. The result is the one the service builds for a confirmation (the decision itself
    /// is tested with the service).
    #[test]
    fn apply_tags_to_records_ids_and_takes_the_release_only_when_authoritative() {
        let release = AcoustIdRelease {
            release_id: Some("rel-1".into()),
            release_group_id: Some("rg-1".into()),
            title: Some("Mezzanine".into()),
            year: Some(1998),
            track_number: Some(3),
            track_count: Some(11),
            disc_number: Some(1),
            album_artist: Some("Massive Attack".into()),
            ..Default::default()
        };
        let found = AcoustIdRecording {
            credits: vec![
                AcoustIdCredit::new("Bizarrap", Some("a1"), " & "),
                AcoustIdCredit::new("Rauw Alejandro", Some("a2"), ""),
            ],
            release: Some(release),
            ..AcoustIdRecording::new(
                "rec-1",
                "Teardrop",
                ["Bizarrap", "Rauw Alejandro"],
                Some("Mezzanine"),
                Some(1998),
            )
        };
        let confirmed = |authoritative: bool| VerificationResult {
            verdict: VerificationVerdict::Confirmed,
            score: 0.99,
            matched_title: Some(found.title.clone()),
            matched_artist: Some(found.artist_credit()),
            matched_album: found.album_title.clone(),
            matched_year: found.year,
            recording_id: Some(found.recording_id.clone()),
            tags_authoritative: authoritative,
            r#match: Some(found.clone()),
            ..Default::default()
        };

        let mut song = Song {
            title: "peer title".into(),
            artist: "a".into(),
            ..Default::default()
        };
        confirmed(false).apply_tags_to(&mut song);
        assert_eq!(song.title, "peer title");
        assert_eq!(song.music_brainz_recording_id.as_deref(), Some("rec-1"));
        assert_eq!(song.primary_artist.as_deref(), Some("Bizarrap"));
        assert_eq!(song.artists, ["Bizarrap", "Rauw Alejandro"]);
        assert_eq!(song.music_brainz_release_id.as_deref(), Some("rel-1"));
        assert_eq!(song.music_brainz_release_group_id.as_deref(), Some("rg-1"));
        assert_eq!(song.track, None);
        assert_eq!(song.album_artist, None);

        let mut song = Song::default();
        confirmed(true).apply_tags_to(&mut song);
        assert_eq!(song.title, "Teardrop");
        assert_eq!(song.album, "Mezzanine");
        assert_eq!(song.year, Some(1998));
        assert_eq!(song.track, Some(3));
        assert_eq!(song.total_tracks, Some(11));
        assert_eq!(song.disc_number, Some(1));
        assert_eq!(song.album_artist.as_deref(), Some("Massive Attack"));

        let mut song = Song {
            title: "peer title".into(),
            ..Default::default()
        };
        VerificationResult {
            verdict: VerificationVerdict::Mismatch,
            ..confirmed(true)
        }
        .apply_tags_to(&mut song);
        assert_eq!(song.title, "peer title");
        assert_eq!(song.music_brainz_recording_id, None);
    }

    #[test]
    fn album_is_from_release_compares_the_album_keys() {
        let mut song = Song {
            album: "Mezzanine (Deluxe)".into(),
            ..Default::default()
        };
        assert!(!VerificationResult::album_is_from_release(&song));
        song.music_brainz_album_title = Some("  ".into());
        assert!(!VerificationResult::album_is_from_release(&song));
        song.music_brainz_album_title = Some("mezzanine".into());
        song.album = "Mezzanine".into();
        assert!(VerificationResult::album_is_from_release(&song));
    }
}
