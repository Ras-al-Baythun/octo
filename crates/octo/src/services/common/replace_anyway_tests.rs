//! What stands between a picked copy and the library song it would replace: a copy that is only
//! no better asks, and the plain song in place of another take asks.

use octo_core::models::download::AcquisitionCandidate;

use super::*;
use crate::services::common::download_picks::{LIDARR_SOURCE, PickedCopy};

fn library(title: &str, album: &str, length: i32, format: &str, quality: &str, path: &str) -> FindTarget {
    FindTarget {
        library_id: Some("nd-1".into()),
        owned_format: Some(format.into()),
        owned_quality: Some(quality.into()),
        owned_path: Some(path.into()),
        ..FindTarget::new("Air", title, Some(album), Some(length), "nd-1")
    }
}

fn studio_flac() -> FindTarget {
    library(
        "Sexy Boy",
        "Moon Safari",
        298,
        "flac",
        "FLAC 16-bit 44.1 kHz",
        "/music/Air/Moon Safari/02 - Sexy Boy.flac",
    )
}

fn studio_mp3(rate: i32) -> FindTarget {
    library(
        "Sexy Boy",
        "Moon Safari",
        298,
        "mp3",
        &format!("MP3 {rate} kbps"),
        "/music/Air/Moon Safari/02 - Sexy Boy.mp3",
    )
}

/// Lidarr's own choice of release, by the quality profile.
fn lidarr_choice() -> FoundCopy {
    FoundCopy::new(
        AcquisitionCandidate {
            source: LIDARR_SOURCE.into(),
            ..Default::default()
        },
        PickedCopy {
            source: LIDARR_SOURCE.into(),
            ..Default::default()
        },
    )
}

/// One release Lidarr listed, in its quality words ("FLAC", "FLAC 24bit", "MP3-320").
fn lidarr_release(quality: &str) -> FoundCopy {
    FoundCopy::new(
        AcquisitionCandidate {
            source: LIDARR_SOURCE.into(),
            format: Some(quality.into()),
            length: Some(298),
            ..Default::default()
        },
        PickedCopy {
            source: LIDARR_SOURCE.into(),
            release_guid: Some("guid-1".into()),
            indexer_id: Some(3),
            release_title: Some("Air - Moon Safari".into()),
            ..Default::default()
        },
    )
}

fn words(answer: &ReplaceCheck) -> &str {
    match answer {
        ReplaceCheck::Confirm { words, .. } => words,
        other => panic!("expected a question, got {other:?}"),
    }
}

// ---- Quality alone ------------------------------------------------------------------------

#[test]
fn a_better_copy_of_the_right_song_fits() {
    assert_eq!(
        check(&studio_mp3(320), &lidarr_release("FLAC")),
        ReplaceCheck::Fits
    );
    assert_eq!(check(&studio_mp3(320), &lidarr_choice()), ReplaceCheck::Fits);
}

#[test]
fn the_same_quality_asks_and_keeps_the_library_song() {
    let answer = check(&studio_flac(), &lidarr_release("FLAC 24bit"));
    assert_eq!(
        words(&answer),
        "This copy is FLAC like yours, so it is not better. Replace yours anyway?"
    );
    let ReplaceCheck::Confirm { song, .. } = answer else {
        unreachable!()
    };
    assert_eq!(
        song,
        AnywaySong {
            title: "Sexy Boy".into(),
            album: Some("Moon Safari".into()),
            length: Some(298),
            retitle: false,
        }
    );
}

#[test]
fn a_lower_quality_asks_and_says_it_sounds_worse() {
    assert_eq!(
        words(&check(&studio_flac(), &lidarr_release("MP3-320"))),
        "This copy is MP3 320 and yours is FLAC, so it sounds worse. Replace yours anyway?"
    );
    assert_eq!(
        words(&check(&studio_mp3(320), &lidarr_release("MP3-192"))),
        "This copy is MP3 192 and yours is MP3 320, so it sounds worse. Replace yours anyway?"
    );
}

#[test]
fn a_lossy_copy_over_a_lossy_one_asks_in_every_case() {
    assert_eq!(
        words(&check(&studio_mp3(320), &lidarr_release("MP3-320"))),
        "This copy is MP3 320 like yours, so it is not better. Replace yours anyway?"
    );
    assert_eq!(
        words(&check(&studio_mp3(128), &lidarr_release("MP3-320"))),
        "This copy is MP3 320 and yours is MP3 128, so Octo does not count it as better. Replace yours anyway?"
    );
}

#[test]
fn lidarrs_choice_asks_only_over_a_lossless_library_song() {
    assert_eq!(
        words(&check(&studio_flac(), &lidarr_choice())),
        "Yours is already FLAC, so Lidarr's choice cannot be better. Replace yours anyway?"
    );
}

#[test]
fn no_question_ever_says_lossless() {
    for picked in [lidarr_release("FLAC"), lidarr_release("MP3-320"), lidarr_choice()] {
        if let ReplaceCheck::Confirm { words, .. } = check(&studio_flac(), &picked) {
            assert!(!words.to_lowercase().contains("lossless"), "{words}");
            assert!(!words.contains('\u{2014}'), "{words}");
        }
    }
}

#[test]
fn the_list_tags_a_copy_beside_the_librarys() {
    assert_eq!(vs_yours(&studio_mp3(320), &lidarr_release("FLAC")), None);
    assert_eq!(
        vs_yours(&studio_flac(), &lidarr_release("FLAC")),
        Some(("same", "Same as yours"))
    );
    assert_eq!(
        vs_yours(&studio_flac(), &lidarr_release("MP3-320")),
        Some(("lower", "Lower than yours"))
    );
    assert_eq!(
        vs_yours(&studio_mp3(128), &lidarr_release("MP3-320")),
        Some(("notBetter", "Not better than yours"))
    );
    assert_eq!(
        vs_yours(
            &FindTarget::new("Air", "Sexy Boy", None, None, "x"),
            &lidarr_release("MP3-320")
        ),
        None
    );
}

// ---- Another take in the library -------------------------------------------------------------

fn live_library() -> FindTarget {
    library(
        "Sexy Boy (Live)",
        "Live at the Olympia",
        402,
        "flac",
        "FLAC 16-bit 44.1 kHz",
        "/music/Air/Live at the Olympia/05 - Sexy Boy (Live).flac",
    )
}

#[test]
fn the_plain_song_in_place_of_a_live_take_asks_and_is_held_to_the_plain_song() {
    let answer = check(&live_library(), &lidarr_release("FLAC"));
    assert_eq!(
        words(&answer),
        "Yours looks like a live recording and this copy is the plain song. \
         This copy is FLAC like yours, so it is not better. Replace yours anyway?"
    );
    let ReplaceCheck::Confirm { song, take, .. } = answer else {
        unreachable!()
    };
    assert_eq!(take, Some(OtherTake::Live));
    assert_eq!(
        song,
        AnywaySong {
            title: "Sexy Boy".into(),
            album: None,
            length: Some(402),
            retitle: true,
        }
    );
    assert_eq!(
        library_take(&live_library()).map(|(take, _)| take),
        Some(OtherTake::Live)
    );
    assert_eq!(take_kind(OtherTake::Live), "live");
    assert!(take_header(OtherTake::Live).starts_with("Yours looks like a live recording."));
}

#[test]
fn a_live_take_found_only_by_its_folder_counts_and_a_better_copy_still_asks() {
    let target = library(
        "Sexy Boy",
        "Olympia 1998",
        402,
        "mp3",
        "MP3 320 kbps",
        "/music/Air/Olympia 1998 (Live)/05 - Sexy Boy.mp3",
    );
    assert_eq!(
        words(&check(&target, &lidarr_release("FLAC"))),
        "Yours looks like a live recording and this copy is the plain song. Replace yours anyway?"
    );
}

#[test]
fn another_version_in_the_library_asks_for_the_plain_song() {
    let target = library(
        "Sexy Boy (Radio Edit)",
        "Sexy Boy",
        214,
        "flac",
        "FLAC 16-bit 44.1 kHz",
        "/music/Air/Sexy Boy/01 - Sexy Boy (Radio Edit).flac",
    );
    let answer = check(&target, &lidarr_release("FLAC"));
    assert!(words(&answer).starts_with("Yours looks like another version and this copy is the plain song."));
    let ReplaceCheck::Confirm { song, take, .. } = answer else {
        unreachable!()
    };
    assert_eq!(take, Some(OtherTake::Version));
    assert_eq!(song.title, "Sexy Boy");
    assert_eq!(song.album, None);
    assert_eq!(take_kind(OtherTake::Version), "version");
}

#[test]
fn the_studio_song_in_the_library_is_no_other_take() {
    assert_eq!(library_take(&studio_flac()), None);
    assert_eq!(other_take(&studio_mp3(320)), None);
}

#[test]
fn the_comparison_shows_both_copies_side_by_side() {
    let compared = compare(&live_library(), &lidarr_release("FLAC"), Some(OtherTake::Live));
    assert_eq!(compared.yours.quality.as_deref(), Some("FLAC"));
    assert_eq!(compared.yours.length, Some(402));
    assert_eq!(compared.yours.take.as_deref(), Some("live recording"));
    assert_eq!(compared.copy.quality.as_deref(), Some("FLAC"));
    assert_eq!(compared.copy.length, Some(298));
    assert_eq!(compared.copy.take.as_deref(), Some("the plain song"));

    let choice = compare(&studio_mp3(320), &lidarr_choice(), None);
    assert_eq!(choice.yours.quality.as_deref(), Some("MP3 320"));
    assert_eq!(choice.copy.quality.as_deref(), Some("Lidarr's choice"));
    assert_eq!(choice.copy.take, None);
}
