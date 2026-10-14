//! `DownloadSettings` (the `Downloads` section): how a downloaded file is checked, named and
//! placed, whichever source it came from.

use serde::{Deserialize, Serialize};

/// Settings that apply to every download, whichever source produced it: the AcoustID
/// verification, the transcode check, naming from the match and how many transfers run at once.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct DownloadSettings {
    /// Fingerprint every finished download with Chromaprint and ask AcoustID what it actually
    /// is before accepting it. A file identified as a different recording is not placed as the
    /// song it was fetched for; a YouTube file has no second candidate, so it is kept and, with
    /// the Review playlist on, asked about. Off by default: it needs a free AcoustID key and
    /// the fpcalc binary, and without both it can only ever be a no-op.
    /// Environment variable: DOWNLOADS__VerifyDownloads
    pub verify_downloads: bool,

    /// AcoustID application key, free from https://acoustid.org/new-application. Blank
    /// disables the lookup half of VerifyDownloads; the duration check keeps working without
    /// it. Named ...ApiKey so the Config-sources tab masks it.
    /// Environment variable: DOWNLOADS__AcoustIdApiKey
    pub acoust_id_api_key: String,

    /// On a confident AcoustID match, write that recording's title, artist, album and year,
    /// which are MusicBrainz's, onto the file instead of trusting the source's tags. Does
    /// nothing unless VerifyDownloads is on and a key is set.
    /// Environment variable: DOWNLOADS__TagFromMusicBrainz
    pub tag_from_music_brainz: bool,

    /// On a confident AcoustID match, name the file from the matched recording as well as
    /// tagging it: artist folder, title, album and track number all come from MusicBrainz, so
    /// the path and the tags are one decision (#48). Implies TagFromMusicBrainz for that file,
    /// because a path from MusicBrainz beside tags from the source is the split this removes.
    /// Off by default: a canonical name is not always the one a user wants on disk (a legal
    /// name, or a composer where the library files the performer). Only files Octo downloads
    /// and confirms are affected; nothing already in the library is renamed.
    /// Environment variable: DOWNLOADS__NameFromMatch
    pub name_from_match: bool,

    /// Minimum AcoustID fingerprint score, as a percentage, before a lookup result is
    /// allowed to decide anything at all.
    ///
    /// Counter-intuitively this is a permissiveness dial, not a strictness dial: results
    /// below it are ignored entirely rather than treated as rejections, so a HIGHER value
    /// rejects FEWER files. The admin help text says so out loud because the intuition runs
    /// the other way.
    /// Environment variable: DOWNLOADS__MinMatchScore
    pub min_match_score: i32,

    /// How many seconds of audio to fingerprint. AcoustID's own tools submit 120, and more
    /// costs decode time without identifying anything extra.
    /// Environment variable: DOWNLOADS__FingerprintSeconds
    pub fingerprint_seconds: i32,

    /// How long fpcalc may take before it is treated as hung. A FLAC fingerprints in about a
    /// second, so thirty is a hang rather than a slow disk; raise it for very slow storage.
    /// Environment variable: DOWNLOADS__FingerprintTimeoutSeconds
    pub fingerprint_timeout_seconds: i32,

    /// How long an AcoustID lookup may take. Verification sits between a finished transfer and
    /// the file joining the library, so a slow AcoustID must cost seconds, never the download.
    /// Environment variable: DOWNLOADS__AcoustIdTimeoutSeconds
    pub acoust_id_timeout_seconds: i32,

    /// Check a download that claims to be lossless (FLAC, WAV, AIFF and the like) for a lossy
    /// file converted to it, by the cutoff in its spectrum. A likely transcode is kept, marked
    /// as transcoded: it is still the right song. Needs no API key and never fails a download;
    /// a missing ffmpeg or a file it cannot read is no opinion. On by default, since it costs a
    /// second or so of decoding per lossless download and only ever changes how the copy is
    /// marked. Also used by the better-quality library action and the duplicate scan.
    /// Environment variable: DOWNLOADS__DetectTranscodes
    pub detect_transcodes: bool,

    /// How long the transcode check may decode for, every window included. A FLAC takes well
    /// under a second, so twenty is a hang rather than a slow disk.
    /// Environment variable: DOWNLOADS__TranscodeCheckTimeoutSeconds
    pub transcode_check_timeout_seconds: i32,

    /// How many downloads may transfer at once. Placing files into the library stays one at a
    /// time either way. 1 is the old behavior exactly. Read live.
    /// Environment variable: DOWNLOADS__ParallelDownloads
    pub parallel_downloads: i32,

    /// Send AcoustID the fingerprints a person confirmed with Keep, so the next person who
    /// downloads that recording gets Confirmed instead of Inconclusive (#47). Only a fingerprint
    /// whose MusicBrainz recording is unambiguous, only after a human kept it, and never one
    /// AcoustID confidently called something else. Needs AcoustIdUserApiKey. Off by default: it
    /// writes to a public database.
    /// Environment variable: DOWNLOADS__SubmitConfirmedFingerprints
    pub submit_confirmed_fingerprints: bool,

    /// Your personal AcoustID key, shown at acoustid.org after signing in. Separate from the
    /// application key, because AcoustID credits a submission to a person, not an app. Named
    /// ...ApiKey so the Config-sources tab masks it.
    /// Environment variable: DOWNLOADS__AcoustIdUserApiKey
    pub acoust_id_user_api_key: String,
}

impl Default for DownloadSettings {
    fn default() -> Self {
        Self {
            verify_downloads: false,
            acoust_id_api_key: String::new(),
            tag_from_music_brainz: false,
            name_from_match: false,
            min_match_score: 85,
            fingerprint_seconds: 120,
            fingerprint_timeout_seconds: 30,
            acoust_id_timeout_seconds: 10,
            detect_transcodes: true,
            transcode_check_timeout_seconds: 20,
            parallel_downloads: 3,
            submit_confirmed_fingerprints: false,
            acoust_id_user_api_key: String::new(),
        }
    }
}

impl DownloadSettings {
    pub fn effective_fingerprint_seconds(&self) -> i32 {
        self.fingerprint_seconds.clamp(15, 600)
    }

    pub fn effective_fingerprint_timeout_seconds(&self) -> i32 {
        self.fingerprint_timeout_seconds.clamp(5, 300)
    }

    pub fn effective_acoust_id_timeout_seconds(&self) -> i32 {
        self.acoust_id_timeout_seconds.clamp(2, 120)
    }

    pub fn effective_transcode_check_timeout_seconds(&self) -> i32 {
        self.transcode_check_timeout_seconds.clamp(5, 300)
    }

    /// Six at most: more transfers at once gain little.
    pub fn effective_parallel_downloads(&self) -> i32 {
        self.parallel_downloads.clamp(1, 6)
    }

    /// Below 50 an AcoustID score is noise and acting on it manufactures false rejections;
    /// 100 is a score no real fingerprint reaches, which would silently disable the feature.
    pub fn effective_min_match_score(&self) -> i32 {
        self.min_match_score.clamp(50, 99)
    }

    /// The same threshold as AcoustID reports it, in 0..1.
    pub fn effective_min_score_fraction(&self) -> f64 {
        f64::from(self.effective_min_match_score()) / 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_setting_is_clamped() {
        let with = |n| DownloadSettings {
            parallel_downloads: n,
            ..Default::default()
        };
        assert_eq!(with(50).effective_parallel_downloads(), 6);
        assert_eq!(with(0).effective_parallel_downloads(), 1);
        assert_eq!(DownloadSettings::default().parallel_downloads, 3);
    }

    #[test]
    fn match_score_is_clamped_and_read_as_a_fraction() {
        let s = DownloadSettings {
            min_match_score: 100,
            ..Default::default()
        };
        assert_eq!(s.effective_min_match_score(), 99);
        assert_eq!(DownloadSettings::default().effective_min_score_fraction(), 0.85);
    }

    #[test]
    fn the_section_binds_by_its_pascal_case_names() {
        let s: DownloadSettings = serde_json::from_str(
            r#"{"VerifyDownloads":true,"AcoustIdApiKey":"k","FingerprintSeconds":5,"DetectTranscodes":false}"#,
        )
        .expect("a valid section");
        assert!(s.verify_downloads);
        assert_eq!(s.acoust_id_api_key, "k");
        assert_eq!(s.effective_fingerprint_seconds(), 15);
        assert!(!s.detect_transcodes);
        assert_eq!(s.transcode_check_timeout_seconds, 20);
    }
}
