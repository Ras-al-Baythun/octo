//! The verification decision (whether a file needs review is tested in
//! `octo_core::fingerprint::verification`), the ISRC check, live takes only on live albums, and
//! an agreeing AcoustID candidate that skips another version.

use octo_core::fingerprint::{AcoustIdCredit, AcoustIdRelease};
use octo_core::models::domain::Song;

use super::*;

const THRESHOLD: f64 = 0.85;

fn ok(results: Vec<AcoustIdResult>) -> AcoustIdLookup {
    AcoustIdLookup::new(true, None, results)
}

fn result(score: f64, recordings: Vec<AcoustIdRecording>) -> AcoustIdResult {
    AcoustIdResult::new(score, recordings)
}

fn recording(title: &str, artist: &str, album: Option<&str>, year: Option<i32>) -> AcoustIdRecording {
    AcoustIdRecording::new(format!("mbid-{title}"), title, [artist], album, year)
}

fn decide(lookup: &AcoustIdLookup, artist: &str, title: &str, authoritative: bool) -> VerificationResult {
    DownloadVerificationService::decide(
        lookup,
        Some(artist),
        Some(title),
        THRESHOLD,
        authoritative,
        0,
        false,
    )
}

// ---- the decision ---------------------------------------------------------------------------

#[test]
fn decide_confident_agreeing_match_is_confirmed() {
    let verdict = decide(
        &ok(vec![result(
            0.97,
            vec![recording(
                "Teardrop",
                "Massive Attack",
                Some("Mezzanine"),
                Some(1998),
            )],
        )]),
        "Massive Attack",
        "Teardrop",
        true,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.matched_album.as_deref(), Some("Mezzanine"));
    assert_eq!(verdict.matched_year, Some(1998));
}

/// The whole answer rides along on every verdict, so the chooser can weigh every release the
/// service named, and a confirmation carries the service's own id.
#[test]
fn decide_carries_the_lookup_on_every_branch_and_the_result_id_when_confirmed() {
    let mut confirmed_result = result(0.97, vec![recording("Teardrop", "Massive Attack", None, None)]);
    confirmed_result.id = Some("acoustid-1".into());
    let confirmed = ok(vec![confirmed_result]);
    let confirmed_verdict = decide(&confirmed, "Massive Attack", "Teardrop", false);
    assert_eq!(confirmed_verdict.lookup.as_ref(), Some(&confirmed));
    assert_eq!(confirmed_verdict.acoust_id.as_deref(), Some("acoustid-1"));

    let weak = ok(vec![result(
        0.60,
        vec![recording("Teardrop", "Massive Attack", None, None)],
    )]);
    let weak_verdict = decide(&weak, "Massive Attack", "Teardrop", false);
    assert_eq!(weak_verdict.lookup.as_ref(), Some(&weak));
    assert_eq!(weak_verdict.acoust_id, None);

    let mismatch = ok(vec![result(
        0.97,
        vec![recording("Angel", "Massive Attack", None, None)],
    )]);
    assert_eq!(
        decide(&mismatch, "Massive Attack", "Teardrop", false)
            .lookup
            .as_ref(),
        Some(&mismatch)
    );
}

/// The most important test in the feature. A weak match must never delete a file, which is also
/// why raising MinMatchScore makes Octo more permissive rather than stricter.
#[test]
fn decide_score_below_threshold_is_inconclusive_not_a_mismatch() {
    let verdict = decide(
        &ok(vec![result(
            0.60,
            vec![recording("Something Else Entirely", "Another Artist", None, None)],
        )]),
        "Massive Attack",
        "Teardrop",
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.deny_reason, "");
}

/// An obscure track has no AcoustID entry, and that is the music Octo exists to find.
/// Rejecting on absence would make verification worst where the library is rarest.
#[test]
fn decide_no_results_at_all_is_inconclusive() {
    let verdict = decide(&ok(vec![]), "Some Obscure Band", "A Song", false);
    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
}

/// AcoustID knows the audio but has no MusicBrainz link, so nothing can be decided.
#[test]
fn decide_qualifying_result_with_no_recordings_is_inconclusive() {
    let verdict = decide(
        &ok(vec![result(0.99, vec![])]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
}

#[test]
fn decide_confident_different_recording_is_a_mismatch_and_names_what_it_got() {
    let verdict = decide(
        &ok(vec![result(
            0.96,
            vec![recording("Picture to Burn", "Taylor Swift", None, None)],
        )]),
        "Taylor Swift",
        "Cruel Summer",
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert!(verdict.deny_reason.contains("Picture to Burn"));
    assert_eq!(verdict.describe(), "'Taylor Swift - Picture to Burn'");
}

/// One AcoustID id maps to several MusicBrainz recordings when the same audio ships on an album
/// and a compilation. Demanding the first would reject correct files.
#[test]
fn decide_second_recording_agrees_is_confirmed() {
    let verdict = decide(
        &ok(vec![result(
            0.98,
            vec![
                recording(
                    "Teardrop",
                    "Massive Attack",
                    Some("Now That's What I Call Music! 42"),
                    None,
                ),
                recording("Teardrop", "Massive Attack", Some("Mezzanine"), Some(1998)),
            ],
        )]),
        "Massive Attack",
        "Teardrop",
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
}

/// The highest-scoring qualifying result wins, not the first one listed.
#[test]
fn decide_prefers_the_highest_scoring_qualifying_result() {
    let verdict = decide(
        &ok(vec![
            result(0.86, vec![recording("Wrong Song", "Wrong Artist", None, None)]),
            result(0.99, vec![recording("Teardrop", "Massive Attack", None, None)]),
        ]),
        "Massive Attack",
        "Teardrop",
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert!((verdict.score - 0.99).abs() < 0.0005);
}

fn peer_song() -> Song {
    Song {
        title: "peer title".into(),
        artist: "peer artist".into(),
        album: "peer album".into(),
        ..Default::default()
    }
}

#[test]
fn apply_tags_to_with_tagging_off_leaves_the_song_alone() {
    let mut song = peer_song();
    decide(
        &ok(vec![result(
            0.99,
            vec![recording(
                "Teardrop",
                "Massive Attack",
                Some("Mezzanine"),
                Some(1998),
            )],
        )]),
        "Massive Attack",
        "Teardrop",
        false,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.title, "peer title");
    assert_eq!(song.artist, "peer artist");
    assert_eq!(song.year, None);
}

#[test]
fn apply_tags_to_confirmed_and_tagging_on_overwrites_the_peers_tags() {
    let mut song = peer_song();
    decide(
        &ok(vec![result(
            0.99,
            vec![recording(
                "Teardrop",
                "Massive Attack",
                Some("Mezzanine"),
                Some(1998),
            )],
        )]),
        "Massive Attack",
        "Teardrop",
        true,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.title, "Teardrop");
    assert_eq!(song.artist, "Massive Attack");
    assert_eq!(song.album, "Mezzanine");
    assert_eq!(song.year, Some(1998));
}

/// A mismatch must never retag; the file is about to be deleted.
#[test]
fn apply_tags_to_mismatch_leaves_the_song_alone() {
    let mut song = Song {
        title: "peer title".into(),
        artist: "peer artist".into(),
        ..Default::default()
    };
    decide(
        &ok(vec![result(
            0.96,
            vec![recording("Picture to Burn", "Taylor Swift", None, None)],
        )]),
        "Taylor Swift",
        "Cruel Summer",
        true,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.title, "peer title");
    assert_eq!(song.music_brainz_recording_id, None);
}

/// Below the threshold nothing is decided, but the one recording that agrees on title, artist
/// and length is remembered: it is the MusicBrainz id a person's Keep may send back (#47).
#[test]
fn decide_below_threshold_says_why_and_names_the_one_agreeing_candidate() {
    let mut teardrop = recording("Teardrop", "Massive Attack", None, None);
    teardrop.duration_seconds = Some(331);
    let verdict = DownloadVerificationService::decide(
        &ok(vec![result(
            0.60,
            vec![teardrop, recording("Something Else", "Someone Else", None, None)],
        )]),
        Some("Massive Attack"),
        Some("Teardrop"),
        THRESHOLD,
        false,
        330,
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::BelowThreshold);
    assert_eq!(verdict.candidate_recording_id.as_deref(), Some("mbid-Teardrop"));
    assert!(verdict.needs_review());
}

/// MusicBrainz holds near-duplicate recordings; two agreeing ids is a guess, not an answer.
#[test]
fn decide_two_agreeing_candidates_names_none() {
    let mut first = AcoustIdRecording::new("id-1", "Teardrop", ["Massive Attack"], None, None);
    first.duration_seconds = Some(315);
    let mut second = AcoustIdRecording::new("id-2", "Teardrop", ["Massive Attack"], None, None);
    second.duration_seconds = Some(315);
    let verdict = DownloadVerificationService::decide(
        &ok(vec![result(0.60, vec![first, second])]),
        Some("Massive Attack"),
        Some("Teardrop"),
        THRESHOLD,
        false,
        315,
        false,
    );

    assert_eq!(verdict.candidate_recording_id, None);
}

#[test]
fn decide_candidate_of_the_wrong_length_is_not_named() {
    let mut live = recording("Teardrop", "Massive Attack", None, None);
    live.duration_seconds = Some(400);
    let verdict = DownloadVerificationService::decide(
        &ok(vec![result(0.60, vec![live])]),
        Some("Massive Attack"),
        Some("Teardrop"),
        THRESHOLD,
        false,
        330,
        false,
    );

    assert_eq!(verdict.candidate_recording_id, None);
}

/// A fingerprint AcoustID knows but MusicBrainz does not is exactly what a person can settle.
#[test]
fn decide_high_score_with_no_recordings_is_no_entry() {
    let verdict = decide(
        &ok(vec![result(0.99, vec![])]),
        "Massive Attack",
        "Teardrop",
        false,
    );
    assert_eq!(verdict.reason, InconclusiveReason::NoEntry);
}

/// An id says what the file IS and changes nothing a person reads, so a confirmed match records
/// it even with tagging from MusicBrainz off (#48).
#[test]
fn apply_tags_to_confirmed_with_tagging_off_still_records_the_recording_id() {
    let mut song = Song {
        title: "peer title".into(),
        artist: "Bizarrap, Rauw Alejandro".into(),
        ..Default::default()
    };
    let title = "Rauw Alejandro: Bzrp Music Sessions, Vol. 56";
    let mut found = AcoustIdRecording::new(
        "rec-1",
        title,
        ["Bizarrap", "Rauw Alejandro"],
        Some(title),
        Some(2023),
    );
    found.credits = vec![
        AcoustIdCredit::new("Bizarrap", Some("a1"), " & "),
        AcoustIdCredit::new("Rauw Alejandro", Some("a2"), ""),
    ];
    found.release = Some(AcoustIdRelease::new(
        Some("rel-1".into()),
        Some("rg-1".into()),
        Some(title.into()),
        Some(2023),
        Some(1),
        Some(1),
        Some(1),
        Some("Bizarrap & Rauw Alejandro".into()),
        false,
    ));
    decide(
        &ok(vec![result(0.99, vec![found])]),
        "Bizarrap, Rauw Alejandro",
        title,
        false,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.title, "peer title");
    assert_eq!(song.music_brainz_recording_id.as_deref(), Some("rec-1"));
    assert_eq!(song.primary_artist.as_deref(), Some("Bizarrap"));
    assert_eq!(song.artists, ["Bizarrap", "Rauw Alejandro"]);
    assert_eq!(song.music_brainz_release_id.as_deref(), Some("rel-1"));
    assert_eq!(song.music_brainz_release_group_id.as_deref(), Some("rg-1"));
    // Only with tags authoritative does the track number and album artist come across.
    assert_eq!(song.track, None);
    assert_eq!(song.album_artist, None);
}

#[test]
fn apply_tags_to_confirmed_and_tagging_on_takes_the_releases_track_and_album_artist() {
    let mut song = Song {
        title: "t".into(),
        artist: "a".into(),
        ..Default::default()
    };
    let mut found = AcoustIdRecording::new(
        "rec-1",
        "Teardrop",
        ["Massive Attack"],
        Some("Mezzanine"),
        Some(1998),
    );
    found.release = Some(AcoustIdRelease::new(
        Some("rel-1".into()),
        Some("rg-1".into()),
        Some("Mezzanine".into()),
        Some(1998),
        Some(3),
        Some(11),
        Some(1),
        Some("Massive Attack".into()),
        false,
    ));
    decide(
        &ok(vec![result(0.99, vec![found])]),
        "Massive Attack",
        "Teardrop",
        true,
    )
    .apply_tags_to(&mut song);

    assert_eq!(song.track, Some(3));
    assert_eq!(song.total_tracks, Some(11));
    assert_eq!(song.disc_number, Some(1));
    assert_eq!(song.album_artist.as_deref(), Some("Massive Attack"));
}

// ---- the ISRC check --------------------------------------------------------------------------

const ISRC: &str = "JPU901901234";

/// The fingerprint confidently names the song in its own script, and the request was made in its
/// romanised name: by the text alone, a mismatch.
fn native_script() -> AcoustIdLookup {
    ok(vec![result(
        0.97,
        vec![
            AcoustIdRecording::new("mbid-gurenge", "紅蓮華", ["LiSA"], None, None),
            AcoustIdRecording::new("mbid-other", "紅蓮華 (TV Size)", ["LiSA"], None, None),
        ],
    )])
}

fn text_verdict() -> VerificationResult {
    VerificationResult {
        fingerprint: Some("AQAD".into()),
        duration_seconds: 239,
        ..decide(&native_script(), "LiSA", "Gurenge", false)
    }
}

fn isrcs(entries: &[(&str, &[&str])]) -> RecordingIsrcs {
    entries
        .iter()
        .map(|(id, codes)| (id.to_string(), codes.iter().map(|c| c.to_string()).collect()))
        .collect()
}

#[test]
fn the_text_alone_calls_the_native_title_a_mismatch() {
    assert_eq!(text_verdict().verdict, VerificationVerdict::Mismatch);
}

#[test]
fn a_recording_listing_the_requested_isrc_confirms() {
    let isrcs = isrcs(&[("mbid-gurenge", &[ISRC]), ("mbid-other", &[])]);

    let verdict = DownloadVerificationService::settle_by_isrc(
        text_verdict(),
        &native_script(),
        THRESHOLD,
        true,
        ISRC,
        &isrcs,
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.recording_id.as_deref(), Some("mbid-gurenge"));
    assert_eq!(
        verdict.r#match.as_ref().map(|m| m.recording_id.as_str()),
        Some("mbid-gurenge")
    );
    assert!(verdict.tags_authoritative);
    assert_eq!(verdict.fingerprint.as_deref(), Some("AQAD"));
    assert!(
        verdict
            .evidence
            .as_deref()
            .is_some_and(|e| e.contains("MusicBrainz lists the requested ISRC"))
    );
    assert_eq!(verdict.deny_reason, "");
}

/// Different codes are not a rejection by themselves, and not a rescue either: the verdict the
/// text gave stands.
#[test]
fn recordings_listing_other_isrcs_change_nothing() {
    let isrcs = isrcs(&[("mbid-gurenge", &["USRC17607839"])]);
    let before = text_verdict();

    let verdict = DownloadVerificationService::settle_by_isrc(
        before.clone(),
        &native_script(),
        THRESHOLD,
        false,
        ISRC,
        &isrcs,
        true,
    );

    assert_eq!(verdict, before);
}

/// The tags carry the code, MusicBrainz lists none to contradict them: two sources disagree
/// about names only, and the file is kept and asked about instead of deleted.
#[test]
fn tagged_isrc_and_a_recording_with_none_keeps_the_file_for_review() {
    let isrcs = isrcs(&[("mbid-gurenge", &[]), ("mbid-other", &[])]);

    let verdict = DownloadVerificationService::settle_by_isrc(
        text_verdict(),
        &native_script(),
        THRESHOLD,
        false,
        ISRC,
        &isrcs,
        true,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::SourceDisagreed);
    assert!(verdict.needs_review());
    assert_eq!(verdict.deny_reason, "");
    // A tag does not then promote it to Confirmed: the fingerprint did name something else.
    assert_eq!(
        DownloadVerificationService::with_tagged_isrc(verdict, Some(ISRC), true).verdict,
        VerificationVerdict::Inconclusive
    );
}

#[test]
fn no_tagged_isrc_and_a_recording_with_none_stays_a_mismatch() {
    let isrcs = isrcs(&[("mbid-gurenge", &[])]);

    let verdict = DownloadVerificationService::settle_by_isrc(
        text_verdict(),
        &native_script(),
        THRESHOLD,
        false,
        ISRC,
        &isrcs,
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
}

/// MusicBrainz could not be asked at all: nothing is known, so nothing changes.
#[test]
fn music_brainz_unreachable_changes_nothing() {
    let before = text_verdict();
    let verdict = DownloadVerificationService::settle_by_isrc(
        before.clone(),
        &native_script(),
        THRESHOLD,
        false,
        ISRC,
        &RecordingIsrcs::new(),
        true,
    );

    assert_eq!(verdict, before);
}

#[test]
fn a_tagged_isrc_confirms_when_acoust_id_could_not_say() {
    for reason in [
        InconclusiveReason::Disabled,
        InconclusiveReason::NotFingerprinted,
        InconclusiveReason::LookupFailed,
        InconclusiveReason::NoEntry,
        InconclusiveReason::BelowThreshold,
    ] {
        let verdict = DownloadVerificationService::with_tagged_isrc(
            VerificationResult {
                reason,
                fingerprint: Some("AQAD".into()),
                ..Default::default()
            },
            Some(ISRC),
            true,
        );

        assert_eq!(verdict.verdict, VerificationVerdict::Confirmed, "{reason}");
        assert_eq!(verdict.reason, InconclusiveReason::None, "{reason}");
        assert!(!verdict.needs_review(), "{reason}");
        assert_eq!(verdict.fingerprint.as_deref(), Some("AQAD"), "{reason}");
        assert_eq!(
            verdict.evidence.as_deref(),
            Some(format!("the file's own tags carry the requested ISRC {ISRC}").as_str()),
            "{reason}"
        );
    }
}

#[test]
fn a_tagged_isrc_never_overrules_a_confident_fingerprint() {
    let mismatch = text_verdict();
    assert_eq!(
        DownloadVerificationService::with_tagged_isrc(mismatch.clone(), Some(ISRC), true),
        mismatch
    );
}

#[test]
fn no_tagged_match_changes_nothing() {
    let inconclusive = VerificationResult {
        reason: InconclusiveReason::NoEntry,
        ..Default::default()
    };
    assert_eq!(
        DownloadVerificationService::with_tagged_isrc(inconclusive.clone(), Some(ISRC), false),
        inconclusive
    );
}

#[test]
fn read_isrcs_a_file_that_is_not_there_is_empty() {
    let missing = std::env::temp_dir().join(format!("missing-{}.flac", uuid::Uuid::new_v4().simple()));
    assert!(DownloadVerificationService::read_isrcs(&missing.to_string_lossy()).is_empty());
}

/// The tags as another program wrote them, read through TagLib's one ISRC property: a FLAC's
/// Vorbis ISRC and an MP3's ID3 TSRC and user text ISRC written by ffmpeg, and an MP4's iTunes
/// ISRC atom (which ffmpeg does not write) written by the tag writer into a file ffmpeg made.
#[test]
fn read_isrcs_reads_every_containers_isrc_tag() {
    if !crate::services::test_support::ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on the PATH");
        return;
    }
    let dir = tempfile::tempdir().expect("a temp dir");
    let ffmpeg = |arguments: &str| crate::services::test_support::run_ffmpeg(dir.path(), arguments);
    const TONE: &str = "-f lavfi -i sine=frequency=440:duration=1";
    ffmpeg(&format!(
        "{TONE} -metadata ISRC=JP-U90-19-01234 -c:a flac tagged.flac"
    ));
    // TSRC is ID3's own ISRC frame; ffmpeg writes a key named ISRC as a user text frame.
    ffmpeg(&format!(
        "{TONE} -metadata TSRC=JPU901901234 -c:a libmp3lame -b:a 128k -id3v2_version 4 tagged.mp3"
    ));
    ffmpeg(&format!(
        "{TONE} -metadata ISRC=JPU901901234 -c:a libmp3lame -b:a 128k -id3v2_version 4 user_frame.mp3"
    ));
    ffmpeg(&format!("{TONE} -c:a aac -b:a 128k tagged.m4a"));
    {
        let m4a = dir.path().join("tagged.m4a");
        let mut file = TagFile::open(&m4a).expect("ffmpeg's m4a opens");
        tag_writer_extras::set_text(&mut file, octo_media::tags::TagFields::ISRC, Some("JPU901901234"));
        file.save().expect("saved");
    }
    ffmpeg(&format!("{TONE} -c:a flac untagged.flac"));

    let read = |name: &str| DownloadVerificationService::read_isrcs(&dir.path().join(name).to_string_lossy());
    for name in ["tagged.flac", "tagged.mp3", "user_frame.mp3", "tagged.m4a"] {
        assert_eq!(read(name), [ISRC], "{name}");
    }
    assert!(read("untagged.flac").is_empty());
}

// ---- live versions ---------------------------------------------------------------------------

fn live_recording(id: &str, albums: &[(&str, &[&str])]) -> AcoustIdRecording {
    let mut found = AcoustIdRecording::new(
        id,
        "Smile in Your Sleep",
        ["Silverstein"],
        albums.first().map(|(group, _)| *group),
        Some(2010),
    );
    found.releases = albums
        .iter()
        .map(|(group, types)| {
            let mut release = AcoustIdRelease::new(
                None,
                Some(format!("rg-{group}")),
                Some(group.to_string()),
                Some(2010),
                None,
                None,
                None,
                Some("Silverstein".into()),
                false,
            );
            release.group_title = Some(group.to_string());
            release.primary_type = Some("Album".into());
            release.secondary_types = types.iter().map(|t| t.to_string()).collect();
            release
        })
        .collect();
    found
}

fn live_lookup(recordings: Vec<AcoustIdRecording>) -> AcoustIdLookup {
    ok(vec![result(1.0, recordings)])
}

fn live_only() -> AcoustIdRecording {
    live_recording("live", &[("Decade (live at the El Mocambo)", &["Live"])])
}

fn studio() -> AcoustIdRecording {
    live_recording(
        "studio",
        &[
            ("Discovering the Waterfront", &[]),
            ("Decade (live at the El Mocambo)", &["Live"]),
        ],
    )
}

fn decide_live(lookup: &AcoustIdLookup, refuse_live: bool) -> VerificationResult {
    DownloadVerificationService::decide(
        lookup,
        Some("Silverstein"),
        Some("Smile in Your Sleep"),
        0.85,
        true,
        0,
        refuse_live,
    )
}

#[test]
fn a_download_refuses_a_recording_only_live_albums_list() {
    let verdict = decide_live(&live_lookup(vec![live_only()]), true);

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(
        verdict.deny_reason,
        "is a live recording, from 'Decade (live at the El Mocambo)'"
    );
}

#[test]
fn a_song_already_in_the_library_is_never_refused_for_being_live() {
    // The Review sweep and the tag preview ask without refuseLive: a live album may be yours on purpose.
    assert_eq!(
        decide_live(&live_lookup(vec![live_only()]), false).verdict,
        VerificationVerdict::Confirmed
    );
}

#[test]
fn a_studio_recording_that_is_also_on_a_live_album_is_fine() {
    assert_eq!(
        decide_live(&live_lookup(vec![studio()]), true).verdict,
        VerificationVerdict::Confirmed
    );
}

#[test]
fn the_same_audio_listed_both_ways_confirms_as_the_studio_recording() {
    let verdict = decide_live(&live_lookup(vec![live_only(), studio()]), true);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.recording_id.as_deref(), Some("studio"));
}

#[test]
fn a_recording_with_no_albums_listed_is_not_judged() {
    assert!(!DownloadVerificationService::only_on_live_albums(
        &AcoustIdRecording::new("x", "Song", ["A"], None, None)
    ));
}

// ---- the AcoustID candidate case -------------------------------------------------------------

#[test]
fn acoust_id_agreeing_candidate_skips_another_version() {
    let mut live = AcoustIdRecording::new("live", "Creep (Live)", ["Radiohead"], None, None);
    live.duration_seconds = Some(238);
    let mut studio = AcoustIdRecording::new("studio", "Creep", ["Radiohead"], None, None);
    studio.duration_seconds = Some(238);
    let lookup = AcoustIdLookup::new(true, None, vec![result(0.95, vec![live, studio])]);

    assert_eq!(
        DownloadVerificationService::agreeing_candidate(&lookup, Some("Radiohead"), Some("Creep"), 238)
            .as_deref(),
        Some("studio")
    );
}

/// The verifier the tag preview asks reports the switches, and with verification off
/// a file is never even fingerprinted.
#[tokio::test]
async fn the_preview_verifier_answers_through_the_service() {
    use octo_core::settings::{AppSettings, DownloadSettings};

    use crate::services::fingerprint::{AcoustIdRateLimitHandler, AcoustIdRateLimiter};

    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        downloads: DownloadSettings {
            verify_downloads: true,
            ..Default::default()
        },
        ..Default::default()
    }));
    let client = Arc::new(AcoustIdClient::new(Arc::new(AcoustIdRateLimitHandler::new(
        Arc::new(AcoustIdRateLimiter::new()),
    ))));
    let service = DownloadVerificationService::new(
        Arc::new(AudioFingerprinter::new()),
        client,
        Arc::clone(&settings),
        None,
        None,
    );
    let verifier: &dyn FingerprintVerifier = &service;
    // No key: nothing to ask AcoustID with.
    assert!(!verifier.is_fingerprinting_enabled());
    assert!(service.verifies_downloads());
    assert_eq!(
        verifier.verify("/nowhere.flac", "A", "T").await.reason,
        InconclusiveReason::Disabled
    );

    settings.set(AppSettings::default());
    assert_eq!(
        service
            .verify("/nowhere.flac", Some("A"), Some("T"), Some(ISRC), true)
            .await,
        VerificationResult::inconclusive()
    );
    assert_eq!(
        service.check_lossless("/nowhere.flac", None, None).await,
        SpectrumReport::unknown("not checked", 0)
    );
}

// ---- Stray links: a weak recording beside a much stronger one of another song ---------------

fn sized(title: &str, artist: &str, sources: i32, seconds: Option<i32>) -> AcoustIdRecording {
    let mut r = recording(title, artist, None, None);
    r.sources = sources;
    r.duration_seconds = seconds;
    r
}

fn decide_at(lookup: &AcoustIdLookup, artist: &str, title: &str, seconds: i32) -> VerificationResult {
    DownloadVerificationService::decide(
        lookup,
        Some(artist),
        Some(title),
        THRESHOLD,
        false,
        seconds,
        false,
    )
}

/// AcoustID ties this audio to "bury a friend" with 1,865 submissions and to "my boy" with one.
fn bury_a_friend() -> AcoustIdLookup {
    let mut stray = sized("my boy", "Billie Eilish", 1, Some(171));
    stray.recording_id = "mbid-my-boy".into();
    let mut acoustic = sized("Bury a Friend (acoustic)", "Billie Eilish", 1, Some(190));
    acoustic.recording_id = "mbid-acoustic".into();
    let mut single = sized("bury a friend", "Billie Eilish", 2, Some(193));
    single.recording_id = "mbid-single".into();
    ok(vec![result(
        0.97,
        vec![
            stray,
            sized("bury a friend", "Billie Eilish", 1865, Some(193)),
            acoustic,
            single,
        ],
    )])
}

#[test]
fn a_stray_link_beside_a_strong_one_does_not_confirm() {
    let verdict = decide_at(&bury_a_friend(), "Billie Eilish", "my boy", 193);

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(verdict.matched_title.as_deref(), Some("bury a friend"));
    assert_eq!(verdict.recording_id.as_deref(), Some("mbid-bury a friend"));
}

#[test]
fn a_stray_link_with_the_right_length_still_does_not_confirm() {
    // Same lengths everywhere: only the submissions tell the stray apart.
    let mut stray = sized("my boy", "Billie Eilish", 1, Some(193));
    stray.recording_id = "mbid-my-boy".into();
    let lookup = ok(vec![result(
        0.97,
        vec![stray, sized("bury a friend", "Billie Eilish", 1865, Some(193))],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 193);

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(verdict.matched_title.as_deref(), Some("bury a friend"));
}

#[test]
fn the_strong_link_still_confirms() {
    let verdict = decide_at(&bury_a_friend(), "Billie Eilish", "bury a friend", 193);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
}

#[test]
fn a_far_off_length_does_not_confirm() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 5, Some(171))],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 193);

    assert_ne!(verdict.verdict, VerificationVerdict::Confirmed);
}

/// The right song and artist whose only doubt is the length is a question for a person, never
/// a mismatch that calls the requested song another one.
#[test]
fn a_right_song_off_only_in_length_needs_a_look_and_is_never_called_another_song() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 5, Some(171))],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 193);

    assert_eq!(verdict.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(verdict.reason, InconclusiveReason::LengthOff);
    assert!(verdict.needs_review());
    assert_eq!(verdict.deny_reason, "");
    assert_eq!(verdict.matched_title.as_deref(), Some("my boy"));
    assert_eq!(
        verdict.r#match.as_ref().and_then(|m| m.duration_seconds),
        Some(171)
    );
}

#[test]
fn a_tagged_isrc_never_settles_a_length_doubt() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 5, Some(171))],
    )]);
    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 193);

    let kept = DownloadVerificationService::with_tagged_isrc(verdict, Some("USUM71900764"), true);

    assert_eq!(kept.verdict, VerificationVerdict::Inconclusive);
    assert_eq!(kept.reason, InconclusiveReason::LengthOff);
}

/// A stray link stays a mismatch even when its length is off too: the stronger song is named.
#[test]
fn a_stray_link_with_an_off_length_is_still_a_mismatch() {
    let verdict = decide_at(&bury_a_friend(), "Billie Eilish", "my boy", 240);

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
    assert_eq!(verdict.matched_title.as_deref(), Some("bury a friend"));
}

#[test]
fn a_lone_small_link_still_confirms() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 1, Some(171))],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 172);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
}

#[test]
fn unknown_lengths_are_not_held_against_a_link() {
    let lookup = ok(vec![result(
        0.97,
        vec![sized("my boy", "Billie Eilish", 3, None)],
    )]);

    let verdict = decide_at(&lookup, "Billie Eilish", "my boy", 0);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
}

/// The same audio on an album and as a radio edit: a weaker recording of the same song is a
/// version of it, not a stray.
#[test]
fn a_weaker_version_of_the_same_song_still_confirms() {
    let lookup = ok(vec![result(
        0.97,
        vec![
            sized("Song (Radio Edit)", "Artist", 5, Some(200)),
            sized("Song", "Artist", 900, Some(200)),
        ],
    )]);

    let verdict = decide_at(&lookup, "Artist", "Song (Radio Edit)", 200);

    assert_eq!(verdict.verdict, VerificationVerdict::Confirmed);
    assert_eq!(verdict.recording_id.as_deref(), Some("mbid-Song (Radio Edit)"));
}

/// A request carrying the stray's own ISRC does not bring the stray back.
#[test]
fn the_isrc_of_a_stray_link_does_not_confirm_it() {
    let lookup = bury_a_friend();
    let before = VerificationResult {
        duration_seconds: 193,
        ..decide_at(&lookup, "Billie Eilish", "my boy", 193)
    };
    let isrcs = isrcs(&[("mbid-my-boy", &["USUM71900764"]), ("mbid-bury a friend", &[])]);

    let verdict = DownloadVerificationService::settle_by_isrc(
        before,
        &lookup,
        THRESHOLD,
        false,
        "USUM71900764",
        &isrcs,
        false,
    );

    assert_eq!(verdict.verdict, VerificationVerdict::Mismatch);
}
