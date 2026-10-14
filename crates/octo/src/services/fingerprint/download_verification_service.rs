//! The download verification service. Its types (the verdict, the reason, the result) are
//! `octo_core::fingerprint::verification`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use octo_core::common::SongIdentity;
use octo_core::common::dotnet;
use octo_core::common::song_versions::strip_versions;
use octo_core::fingerprint::{
    AcoustIdLookup, AcoustIdRecording, AcoustIdResult, InconclusiveReason, TrackMatchComparer,
    VerificationResult, VerificationVerdict,
};
use octo_core::settings::SettingsStore;
use octo_core::tagging::FingerprintVerifier;
use octo_media::audio::{AudioFingerprinter, FingerprintOutcome, SpectrumAnalyzer, SpectrumReport};
use octo_media::tags::{TagFile, tag_writer_extras};
use tracing::{debug, info, warn};

use super::acoust_id_client::AcoustIdClient;
use super::music_brainz_client::{MusicBrainzClient, MusicBrainzError};

/// The ISRCs MusicBrainz listed per recording id, with ids compared ignoring case.
pub type RecordingIsrcs = HashMap<String, Vec<String>>;

/// Asks what a finished download actually contains, rather than what its name and advertised
/// length claim.
///
/// Every failure path accepts the file. That is the correct trade and also the dominant risk:
/// a broken key, a missing binary, a mishandled gzip and a parse error all look exactly like
/// "everything is fine", which is why the pieces below log refusals at Warning.
pub struct DownloadVerificationService {
    fingerprinter: Arc<AudioFingerprinter>,
    client: Arc<AcoustIdClient>,
    /// The download settings, read at every use.
    settings: Arc<SettingsStore>,
    music_brainz: Option<Arc<MusicBrainzClient>>,
    spectrum: Option<Arc<SpectrumAnalyzer>>,
}

impl DownloadVerificationService {
    /// How many of the recordings a fingerprint named are asked for their ISRCs. Each is a
    /// MusicBrainz call a second apart, spent only when the request carried an ISRC and the
    /// recordings' names disagreed with it.
    pub const MAX_ISRC_LOOKUPS: usize = 3;

    pub fn new(
        fingerprinter: Arc<AudioFingerprinter>,
        client: Arc<AcoustIdClient>,
        settings: Arc<SettingsStore>,
        music_brainz: Option<Arc<MusicBrainzClient>>,
        spectrum: Option<Arc<SpectrumAnalyzer>>,
    ) -> Self {
        Self {
            fingerprinter,
            client,
            settings,
            music_brainz,
            spectrum,
        }
    }

    /// Whether a file that claims to be lossless really is, by its spectrum. Unknown, and no
    /// work at all, when the check is off, the file does not claim to be lossless, or ffmpeg
    /// cannot say. Separate from [`Self::verify`] because the answer never rejects a file: it
    /// only decides which of two right songs is kept.
    pub async fn check_lossless(
        &self,
        path: &str,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
    ) -> SpectrumReport {
        let settings = self.settings.current().downloads.clone();
        let spectrum = match &self.spectrum {
            Some(spectrum) if settings.detect_transcodes && SpectrumAnalyzer::claims_lossless(path) => {
                spectrum
            }
            _ => return SpectrumReport::unknown("not checked", 0),
        };

        let report = spectrum
            .analyze(
                Path::new(path),
                settings.effective_transcode_check_timeout_seconds(),
            )
            .await;
        let (artist, title) = (requested_artist.unwrap_or(""), requested_title.unwrap_or(""));
        if report.is_likely_lossy() {
            let format = dotnet::to_upper_invariant(get_extension(path).trim_start_matches('.'));
            warn!("the {format} for '{artist} - {title}' is {}", report.describe());
        } else {
            // Logged when it passes too, for the same reason a confirmation is: silence on
            // success looks exactly like a check that never ran.
            info!("spectrum of '{artist} - {title}': {}", report.describe());
        }
        report
    }

    /// AcoustID can answer at all.
    pub fn has_api_key(&self) -> bool {
        !dotnet::is_blank(&self.settings.current().downloads.acoust_id_api_key)
    }

    /// The user asked Octo to check what a source delivers. Separate from the key, because the
    /// duration check needs none and its verdicts are just as durable.
    pub fn verifies_downloads(&self) -> bool {
        self.settings.current().downloads.verify_downloads
    }

    /// A lookup can actually happen. Split from the above deliberately, in the shape
    /// LastFmService uses for HasApiKey/IsRadioEnabled: collapsing them would mean a user who
    /// switches verification on without a key gets no duration check either.
    pub fn is_fingerprinting_enabled(&self) -> bool {
        self.verifies_downloads() && self.has_api_key()
    }

    /// No cancellation token, deliberately. There must be no way for a caller who has already
    /// given up to skip verification on a file that is about to enter the library.
    ///
    /// `refuse_live`: a download that did not ask for a live take: a recording MusicBrainz only
    /// ever lists on live albums is the wrong file. Off for songs already in the library, which
    /// may be live on purpose.
    pub async fn verify(
        &self,
        path: &str,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        requested_isrc: Option<&str>,
        refuse_live: bool,
    ) -> VerificationResult {
        if !self.verifies_downloads() {
            return VerificationResult::inconclusive();
        }

        // What the file's own tags say it is, when the request named an ISRC to hold them to.
        // A header read, and it needs no API key: with no key it is the only question asked.
        let isrc = requested_isrc.and_then(SongIdentity::normalize_isrc);
        let tagged = if isrc.is_none() {
            Vec::new()
        } else {
            Self::read_isrcs(path)
        };
        let tagged_match = isrc.as_ref().is_some_and(|isrc| tagged.contains(isrc));

        let verdict = if self.has_api_key() {
            self.identify(
                path,
                requested_artist,
                requested_title,
                isrc.as_deref(),
                tagged_match,
                refuse_live,
            )
            .await
        } else {
            VerificationResult::inconclusive()
        };

        let verdict = Self::with_tagged_isrc(verdict, isrc.as_deref(), tagged_match);
        let (artist, title) = (requested_artist.unwrap_or(""), requested_title.unwrap_or(""));
        let isrc_text = isrc.as_deref().unwrap_or("");
        match (&verdict.verdict, &verdict.evidence) {
            (VerificationVerdict::Confirmed, Some(evidence)) => {
                info!("confirmed '{artist} - {title}' by ISRC {isrc_text}: {evidence}")
            }
            _ if isrc.is_some() && !tagged.is_empty() && !tagged_match => {
                // Not a rejection: a re-release or a remaster is often given a new code.
                info!(
                    "the file for '{artist} - {title}' is tagged ISRC {}, not the {isrc_text} asked for; \
                     that alone decides nothing",
                    tagged.join(", ")
                )
            }
            _ => {}
        }
        verdict
    }

    /// A file whose own tags carry the ISRC that was asked for is that recording, when nothing
    /// better could be established: AcoustID off, down, without an entry or below the threshold.
    /// A confident fingerprint of something else is not overruled by a tag, since tags are
    /// copied and audio is not, and neither is a fingerprint that named a recording whose ISRCs
    /// were not found, or the right recording at a length far from the file's (those go to a
    /// person).
    pub fn with_tagged_isrc(
        verdict: VerificationResult,
        isrc: Option<&str>,
        tagged_match: bool,
    ) -> VerificationResult {
        if !tagged_match
            || verdict.verdict != VerificationVerdict::Inconclusive
            || verdict.reason == InconclusiveReason::SourceDisagreed
            || verdict.reason == InconclusiveReason::LengthOff
        {
            return verdict;
        }
        VerificationResult {
            verdict: VerificationVerdict::Confirmed,
            reason: InconclusiveReason::None,
            evidence: Some(format!(
                "the file's own tags carry the requested ISRC {}",
                isrc.unwrap_or("")
            )),
            ..verdict
        }
    }

    /// The fingerprint and the AcoustID lookup, and the ISRC check of the recordings it named.
    /// The question `verify` asked alone before ISRCs were evidence.
    async fn identify(
        &self,
        path: &str,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        isrc: Option<&str>,
        tagged_match: bool,
        refuse_live: bool,
    ) -> VerificationResult {
        let settings = self.settings.current().downloads.clone();
        let fingerprint = self
            .fingerprinter
            .fingerprint(
                Path::new(path),
                settings.effective_fingerprint_seconds(),
                settings.effective_fingerprint_timeout_seconds(),
            )
            .await;

        if fingerprint.outcome == FingerprintOutcome::Undecodable {
            return VerificationResult {
                verdict: VerificationVerdict::Mismatch,
                deny_reason: "delivered a file with no decodable audio".to_string(),
                ..Default::default()
            };
        }

        let Some(print) = fingerprint
            .fingerprint
            .clone()
            .filter(|f| fingerprint.outcome == FingerprintOutcome::Ok && !f.is_empty())
        else {
            return reason(InconclusiveReason::NotFingerprinted);
        };

        // TagLib's duration, not fpcalc's. -length pins how much audio is fingerprinted, and
        // staking a rejection on whether that also truncates the reported duration would reject
        // every track over two minutes if it does.
        let mut seconds = read_duration_seconds(path);
        if seconds <= 0 {
            seconds = fingerprint.decoded_seconds;
        }
        if seconds <= 0 {
            return reason(InconclusiveReason::NotFingerprinted);
        }

        let Some(lookup) = self
            .client
            .lookup(
                &settings.acoust_id_api_key,
                &print,
                seconds,
                settings.effective_acoust_id_timeout_seconds(),
            )
            .await
        else {
            return reason(InconclusiveReason::LookupFailed);
        };
        if !lookup.is_ok {
            warn!(
                "acoustid refused the lookup for {path}: {}",
                lookup.error.as_deref().unwrap_or("")
            );
            return reason(InconclusiveReason::LookupFailed);
        }
        let (artist, title) = (requested_artist.unwrap_or(""), requested_title.unwrap_or(""));
        if lookup.results.is_empty() {
            info!(
                "acoustid has no entry for '{artist} - {title}'; keeping the file. Obscure music is \
                 exactly what Octo is for, so an absent match is never treated as a mismatch."
            );
            // Kept with the fingerprint: this is the one case a person listening can settle, and
            // the one where their answer is worth sending back to AcoustID.
            return VerificationResult {
                reason: InconclusiveReason::NoEntry,
                fingerprint: Some(print),
                duration_seconds: seconds,
                lookup: Some(lookup),
                ..Default::default()
            };
        }

        // NameFromMatch implies authoritative tags: a path from MusicBrainz beside tags from the
        // source is exactly the split it exists to remove (#48).
        let threshold = settings.effective_min_score_fraction();
        let authoritative = settings.tag_from_music_brainz || settings.name_from_match;
        let mut verdict = VerificationResult {
            fingerprint: Some(print),
            duration_seconds: seconds,
            ..Self::decide(
                &lookup,
                requested_artist,
                requested_title,
                threshold,
                authoritative,
                seconds,
                refuse_live,
            )
        };

        // A recording whose name reads differently from the request may still be it: a title in
        // its own script, or translated. Its ISRCs settle that when the request carried one.
        if let (VerificationVerdict::Mismatch, Some(isrc), Some(music_brainz)) =
            (verdict.verdict, isrc, &self.music_brainz)
        {
            let recording_isrcs = Self::recording_isrcs(
                music_brainz,
                &lookup,
                threshold,
                settings.effective_acoust_id_timeout_seconds(),
            )
            .await;
            verdict = Self::settle_by_isrc(
                verdict,
                &lookup,
                threshold,
                authoritative,
                isrc,
                &recording_isrcs,
                tagged_match,
            );
        }

        // A confirmation is logged too, not just a refusal. The dominant risk in this feature is
        // that a broken key, a missing binary or a mangled request makes it accept everything
        // while looking healthy, and silence on success is indistinguishable from never running.
        if verdict.verdict == VerificationVerdict::Confirmed && verdict.evidence.is_none() {
            let album = match verdict.matched_album.as_deref().filter(|a| !a.is_empty()) {
                Some(album) => format!(" from '{album}'"),
                None => String::new(),
            };
            info!(
                "acoustid confirmed '{artist} - {title}' at {}{album}",
                percent(verdict.score)
            );
        }

        if verdict.verdict == VerificationVerdict::Inconclusive
            && verdict.reason == InconclusiveReason::SourceDisagreed
        {
            info!(
                "acoustid names {} for '{artist} - {title}', but {}; keeping the file and asking about it",
                verdict.describe(),
                verdict.evidence.as_deref().unwrap_or("")
            );
        } else if verdict.verdict == VerificationVerdict::Inconclusive {
            let best = lookup
                .results
                .iter()
                .map(|r| r.score)
                .fold(f64::NEG_INFINITY, f64::max);
            info!(
                "acoustid's best match for '{artist} - {title}' scored {} against a {} \
                 threshold, so it decides nothing and the file is kept",
                percent(best),
                percent(threshold)
            );
        }

        verdict
    }

    /// The ISRCs MusicBrainz lists for the first few recordings of the best qualifying result, by
    /// recording id. A recording that could not be asked is left out; one asked that lists none
    /// is present with an empty list. Bounded by one timeout for all of them.
    async fn recording_isrcs(
        music_brainz: &MusicBrainzClient,
        lookup: &AcoustIdLookup,
        threshold: f64,
        timeout_seconds: i32,
    ) -> RecordingIsrcs {
        let mut found = RecordingIsrcs::new();
        let Some(best) = best_qualifying(lookup, threshold) else {
            return found;
        };

        let budget = Duration::from_secs((timeout_seconds.max(0) as u64) + 2 * Self::MAX_ISRC_LOOKUPS as u64);
        let mut asked: Vec<&str> = Vec::new();
        let recordings = best
            .recordings
            .iter()
            .filter(|r| !r.recording_id.is_empty())
            .filter(|r| {
                // Each recording id once, compared exactly; the first one wins.
                if asked.contains(&r.recording_id.as_str()) {
                    false
                } else {
                    asked.push(&r.recording_id);
                    true
                }
            })
            .take(Self::MAX_ISRC_LOOKUPS)
            .collect::<Vec<_>>();
        let ask_all = async {
            for recording in recordings {
                match music_brainz.fetch_isrcs(&recording.recording_id).await {
                    Ok(Some(isrcs)) => {
                        insert_ignore_case(&mut found, &recording.recording_id, isrcs);
                    }
                    Ok(None) => {}
                    Err(MusicBrainzError::TimedOut) => return Err(()),
                    Err(error) => {
                        // A malformed answer ends the lookup the way a timeout does, logged.
                        warn!("musicbrainz answered the ISRC lookup with something unreadable: {error}");
                        return Err(());
                    }
                }
            }
            Ok(())
        };
        let done = tokio::time::timeout(budget, ask_all).await;
        if !matches!(done, Ok(Ok(()))) {
            info!("musicbrainz took too long to list ISRCs; deciding on what it answered");
        }
        found
    }

    /// A fingerprint named recordings whose titles and artists read differently from the
    /// request, and the request carried an ISRC. One of those recordings listing that ISRC makes
    /// it the recording asked for, whatever its name. None of them listing any ISRC at all,
    /// while the file's own tags carry the requested one, is a disagreement between two sources
    /// a person can settle, so the file is kept and asked about rather than deleted. Recordings
    /// that list other ISRCs change nothing: the verdict stays as it was.
    pub fn settle_by_isrc(
        verdict: VerificationResult,
        lookup: &AcoustIdLookup,
        threshold: f64,
        tags_authoritative: bool,
        isrc: &str,
        recording_isrcs: &RecordingIsrcs,
        tagged_match: bool,
    ) -> VerificationResult {
        if verdict.verdict != VerificationVerdict::Mismatch {
            return verdict;
        }
        let Some(best) = best_qualifying(lookup, threshold) else {
            return verdict;
        };

        // A stray link is not believed on its ISRC either: the request's ISRC belongs to the
        // song asked for, so it names the stray as surely as its title did.
        let agreed = best.recordings.iter().find(|recording| {
            get_ignore_case(recording_isrcs, &recording.recording_id)
                .is_some_and(|isrcs| isrcs.iter().any(|code| code == isrc))
                && trustworthy(recording, best, verdict.duration_seconds)
        });
        if let Some(agreed) = agreed {
            return VerificationResult {
                fingerprint: verdict.fingerprint,
                duration_seconds: verdict.duration_seconds,
                lookup: Some(lookup.clone()),
                evidence: Some(format!(
                    "MusicBrainz lists the requested ISRC {isrc} on '{} - {}'",
                    agreed.artist_credit(),
                    agreed.title
                )),
                ..confirm(best, agreed, tags_authoritative)
            };
        }

        if tagged_match && !recording_isrcs.is_empty() && recording_isrcs.values().all(Vec::is_empty) {
            return VerificationResult {
                verdict: VerificationVerdict::Inconclusive,
                reason: InconclusiveReason::SourceDisagreed,
                deny_reason: String::new(),
                evidence: Some(format!(
                    "the file's own tags carry the requested ISRC {isrc} and MusicBrainz lists none to contradict it"
                )),
                ..verdict
            };
        }

        verdict
    }

    /// The whole decision, separated from the I/O so it can be driven directly. Every branch
    /// here either keeps a file or deletes one, and the fingerprinter and HTTP client above make
    /// the orchestration awkward to fake for no benefit.
    pub fn decide(
        lookup: &AcoustIdLookup,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        threshold: f64,
        tags_authoritative: bool,
        duration_seconds: i32,
        refuse_live: bool,
    ) -> VerificationResult {
        VerificationResult {
            lookup: Some(lookup.clone()),
            ..decide_core(
                lookup,
                requested_artist.unwrap_or(""),
                requested_title.unwrap_or(""),
                threshold,
                tags_authoritative,
                duration_seconds,
                refuse_live,
            )
        }
    }

    /// Whether MusicBrainz lists this recording only on live albums: every release group it is
    /// on is typed Live. A studio recording that also appears on a live album is not one; a
    /// recording with no albums listed is not judged.
    pub fn only_on_live_albums(recording: &AcoustIdRecording) -> bool {
        // One release per group, keyed by the group id, else the group title, else the title;
        // the first release of each group stands for it.
        let mut keys: Vec<Option<&str>> = Vec::new();
        let mut groups = Vec::new();
        for release in &recording.releases {
            let key = release
                .release_group_id
                .as_deref()
                .or(release.group_title.as_deref())
                .or(release.title.as_deref());
            if !keys.contains(&key) {
                keys.push(key);
                groups.push(release);
            }
        }
        !groups.is_empty()
            && groups.iter().all(|release| {
                release
                    .secondary_types
                    .iter()
                    .any(|kind| dotnet::eq_ignore_case(kind, "Live"))
            })
    }

    /// Every valid ISRC the file's tags carry: ID3's TSRC, a Vorbis comment's ISRC and the MP4
    /// iTunes ISRC atom all come through one TagLib property. ffmpeg, and whatever converted a
    /// file with it, writes an MP3's ISRC as a user text frame named ISRC instead, so that is
    /// read too. A tag that holds several joins them, so it is split. Empty when there is none or
    /// the tags cannot be read.
    ///
    /// The same reading the release identifier gets from `tag_writer_extras::read_facts`, which
    /// gathers the codes this way.
    pub fn read_isrcs(path: &str) -> Vec<String> {
        tag_writer_extras::read_facts(path, true).isrcs
    }

    /// The single recording, at any score, that agrees on title, artist and length (7 seconds
    /// either way). Two or more is ambiguity, and ambiguity submits nothing.
    pub fn agreeing_candidate(
        lookup: &AcoustIdLookup,
        requested_artist: Option<&str>,
        requested_title: Option<&str>,
        duration_seconds: i32,
    ) -> Option<String> {
        let (artist, title) = (requested_artist.unwrap_or(""), requested_title.unwrap_or(""));
        let mut ids: Vec<&str> = Vec::new();
        for recording in lookup.results.iter().flat_map(|r| &r.recordings) {
            let agrees = !recording.recording_id.is_empty()
                && TrackMatchComparer::title_matches(title, &recording.title)
                && TrackMatchComparer::artist_matches(artist, &recording.artist_credit(), &recording.artists)
                && (duration_seconds <= 0
                    || recording
                        .duration_seconds
                        .is_none_or(|d| (d - duration_seconds).abs() <= 7));
            if agrees
                && !ids
                    .iter()
                    .any(|id| dotnet::eq_ignore_case(id, &recording.recording_id))
            {
                ids.push(&recording.recording_id);
            }
        }
        (ids.len() == 1).then(|| ids[0].to_string())
    }
}

#[async_trait]
impl FingerprintVerifier for DownloadVerificationService {
    fn is_fingerprinting_enabled(&self) -> bool {
        DownloadVerificationService::is_fingerprinting_enabled(self)
    }

    async fn verify(&self, path: &str, artist: &str, title: &str) -> VerificationResult {
        DownloadVerificationService::verify(self, path, Some(artist), Some(title), None, false).await
    }
}

fn reason(reason: InconclusiveReason) -> VerificationResult {
    VerificationResult {
        reason,
        ..Default::default()
    }
}

fn best_qualifying(lookup: &AcoustIdLookup, threshold: f64) -> Option<&AcoustIdResult> {
    qualifying(lookup, threshold).into_iter().next()
}

/// The results at or above the threshold that name a recording, best first (a stable sort, so
/// equal scores keep their order).
fn qualifying(lookup: &AcoustIdLookup, threshold: f64) -> Vec<&AcoustIdResult> {
    let mut qualifying: Vec<&AcoustIdResult> = lookup
        .results
        .iter()
        .filter(|r| r.score >= threshold && !r.recordings.is_empty())
        .collect();
    qualifying.sort_by(|a, b| b.score.total_cmp(&a.score));
    qualifying
}

/// A link this much weaker than the strongest recording of another song is taken as a stray.
const STRAY_RATIO: i32 = 20;
/// The least length gap, in seconds, that counts as a different recording.
const MIN_LENGTH_SLACK: i32 = 10;
/// The share of the file's length that counts as a different recording on long songs.
const LENGTH_SLACK: f64 = 0.07;

/// Whether an agreeing recording can be believed: not dwarfed by a recording of another song
/// in the same result, and not far from the file's length. The comparison is only with other
/// songs, since the same audio is often tied to several versions of one song (album, single,
/// radio edit) with very different counts. A lone weak link still counts, and so does a
/// length nobody knows.
pub(crate) fn trustworthy(agreed: &AcoustIdRecording, result: &AcoustIdResult, file_seconds: i32) -> bool {
    !is_stray(agreed, result) && !length_off(agreed, file_seconds)
}

/// Whether a recording is dwarfed by a recording of another song in the same result.
fn is_stray(agreed: &AcoustIdRecording, result: &AcoustIdResult) -> bool {
    let song = SongIdentity::key(&strip_versions(&agreed.title));
    let strongest_other = result
        .recordings
        .iter()
        .filter(|r| SongIdentity::key(&strip_versions(&r.title)) != song)
        .map(|r| r.sources)
        .max()
        .unwrap_or(0);
    strongest_other > 0 && agreed.sources.saturating_mul(STRAY_RATIO) < strongest_other
}

/// Whether a recording's known length is far from the file's known length.
fn length_off(agreed: &AcoustIdRecording, file_seconds: i32) -> bool {
    let Some(length) = agreed.duration_seconds.filter(|l| *l > 0) else {
        return false;
    };
    if file_seconds <= 0 {
        return false;
    }
    let slack = MIN_LENGTH_SLACK.max((f64::from(file_seconds) * LENGTH_SLACK).round() as i32);
    (length - file_seconds).abs() > slack
}

fn decide_core(
    lookup: &AcoustIdLookup,
    requested_artist: &str,
    requested_title: &str,
    threshold: f64,
    tags_authoritative: bool,
    duration_seconds: i32,
    refuse_live: bool,
) -> VerificationResult {
    let qualifying = qualifying(lookup, threshold);

    // Below the threshold an answer is ignored, never acted on. That is why raising
    // MinMatchScore makes Octo MORE permissive rather than less.
    let Some(best) = qualifying.first().copied() else {
        // A result above the threshold with no recordings is a fingerprint AcoustID knows and
        // MusicBrainz does not: exactly the gap a person's confirmation can fill.
        let known = lookup.results.iter().any(|r| r.score >= threshold);
        return VerificationResult {
            reason: if known {
                InconclusiveReason::NoEntry
            } else {
                InconclusiveReason::BelowThreshold
            },
            candidate_recording_id: DownloadVerificationService::agreeing_candidate(
                lookup,
                Some(requested_artist),
                Some(requested_title),
                duration_seconds,
            ),
            ..Default::default()
        };
    };

    // Any, not first: one AcoustID id maps to several MusicBrainz recordings when the same audio
    // ships on an album and a compilation, and demanding the first would reject correct files.
    let agreeing: Vec<&AcoustIdRecording> = best
        .recordings
        .iter()
        .filter(|r| {
            TrackMatchComparer::title_matches(requested_title, &r.title)
                && TrackMatchComparer::artist_matches(requested_artist, &r.artist_credit(), &r.artists)
                && trustworthy(r, best, duration_seconds)
        })
        .collect();
    // The studio recording first, when the same audio is listed under both.
    let agreed = agreeing
        .iter()
        .find(|r| !DownloadVerificationService::only_on_live_albums(r))
        .or(agreeing.first())
        .copied();

    // The right song and artist, but a live take the request never asked for: the same title and
    // nearly the same length, so only the albums it is on tell it apart.
    if let Some(agreed) = agreed
        && refuse_live
        && DownloadVerificationService::only_on_live_albums(agreed)
    {
        return VerificationResult {
            verdict: VerificationVerdict::Mismatch,
            score: best.score,
            matched_title: Some(agreed.title.clone()),
            matched_artist: Some(agreed.artist_credit()),
            matched_album: agreed.album_title.clone(),
            matched_year: agreed.year,
            recording_id: Some(agreed.recording_id.clone()),
            deny_reason: match agreed.album_title.as_deref().filter(|a| !a.is_empty()) {
                None => "is a live recording".to_string(),
                Some(album) => format!("is a live recording, from '{album}'"),
            },
            ..Default::default()
        };
    }

    if let Some(agreed) = agreed {
        return confirm(best, agreed, tags_authoritative);
    }

    // The right song and artist, believed but for its length: a person can tell whether it is a
    // different cut, and the requested song is never named as another one.
    if let Some(doubted) = best.recordings.iter().find(|r| {
        TrackMatchComparer::title_matches(requested_title, &r.title)
            && TrackMatchComparer::artist_matches(requested_artist, &r.artist_credit(), &r.artists)
            && !is_stray(r, best)
    }) {
        return VerificationResult {
            verdict: VerificationVerdict::Inconclusive,
            reason: InconclusiveReason::LengthOff,
            score: best.score,
            matched_title: Some(doubted.title.clone()),
            matched_artist: Some(doubted.artist_credit()),
            matched_album: doubted.album_title.clone(),
            matched_year: doubted.year,
            recording_id: Some(doubted.recording_id.clone()),
            r#match: Some(doubted.clone()),
            ..Default::default()
        };
    }

    // The recording most submissions tie this audio to, the first listed on a tie.
    let actual = best
        .recordings
        .iter()
        .rev()
        .max_by_key(|r| r.sources)
        .unwrap_or(&best.recordings[0]);
    VerificationResult {
        verdict: VerificationVerdict::Mismatch,
        score: best.score,
        matched_title: Some(actual.title.clone()),
        matched_artist: Some(actual.artist_credit()),
        matched_album: actual.album_title.clone(),
        matched_year: actual.year,
        recording_id: Some(actual.recording_id.clone()),
        deny_reason: format!("is '{} - {}'", actual.artist_credit(), actual.title),
        ..Default::default()
    }
}

fn confirm(
    result: &AcoustIdResult,
    recording: &AcoustIdRecording,
    tags_authoritative: bool,
) -> VerificationResult {
    VerificationResult {
        verdict: VerificationVerdict::Confirmed,
        score: result.score,
        acoust_id: result.id.clone(),
        matched_title: Some(recording.title.clone()),
        matched_artist: Some(recording.artist_credit()),
        matched_album: recording.album_title.clone(),
        matched_year: recording.year,
        recording_id: Some(recording.recording_id.clone()),
        tags_authoritative,
        r#match: Some(recording.clone()),
        ..Default::default()
    }
}

fn read_duration_seconds(path: &str) -> i32 {
    match TagFile::open(path) {
        Ok(file) => file.duration_seconds(),
        Err(error) => {
            debug!("could not read a duration from {path}: {error}");
            0
        }
    }
}

fn get_ignore_case<'a>(map: &'a RecordingIsrcs, key: &str) -> Option<&'a Vec<String>> {
    map.get(key).or_else(|| {
        map.iter()
            .find(|(k, _)| dotnet::eq_ignore_case(k, key))
            .map(|(_, v)| v)
    })
}

fn insert_ignore_case(map: &mut RecordingIsrcs, key: &str, value: Vec<String>) {
    let existing = map.keys().find(|k| dotnet::eq_ignore_case(k, key)).cloned();
    map.insert(existing.unwrap_or_else(|| key.to_string()), value);
}

/// A fraction for the log as a whole percentage, such as `85%`.
fn percent(fraction: f64) -> String {
    format!("{}%", dotnet::round(fraction * 100.0, 0))
}

/// The extension of the file name: its last dot and what follows, or empty when the name has
/// no dot or ends with one.
fn get_extension(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(dot) if dot + 1 < name.len() => &name[dot..],
        _ => "",
    }
}

#[cfg(test)]
#[path = "download_verification_service_tests.rs"]
mod tests;

/// The review sweep's check: a verification of the path, artist and title with no ISRC and
/// live takes not refused.
#[async_trait::async_trait]
impl crate::services::library::library_review_sweep_worker::SweepVerification
    for DownloadVerificationService
{
    fn is_fingerprinting_enabled(&self) -> bool {
        DownloadVerificationService::is_fingerprinting_enabled(self)
    }

    async fn verify(&self, path: &str, artist: Option<&str>, title: Option<&str>) -> VerificationResult {
        DownloadVerificationService::verify(self, path, artist, title, None, false).await
    }
}
