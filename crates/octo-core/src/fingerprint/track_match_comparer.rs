use crate::common::dotnet::utf16_len;
use crate::common::song_identity::{ArtistAgreement, SongIdentity, SongVerdict};

/// Decides whether the recording AcoustID identified is the recording that was asked for.
///
/// Exact string equality would reject constantly on real data. The requested side comes
/// from Last.fm and Deezer ("Teardrop - Remastered 2011", "Bjork" for "Björk"); the matched
/// side is MusicBrainz's canonical credit, which adds featured artists the request never
/// mentioned. Both are read by [`SongIdentity`]; what is decided here is how much
/// benefit of the doubt a verdict gets.
///
/// Biased toward "yes" on purpose, except about the version. A false NO discards a good file
/// AND writes a deny-list entry that stands for thirty days; a false YES costs a wrongly-tagged
/// track that the duration check and the ranking heuristics have already had two chances to
/// catch. A different version is never a yes: a live take or a remix the request did not ask
/// for is the wrong file however alike the rest reads.
///
/// A missing title or artist is passed as `""`; the two read the same.
pub struct TrackMatchComparer;

impl TrackMatchComparer {
    /// The shortest core allowed to satisfy the prefix rule. Without a floor, "Go" matches
    /// "Gold" and a correct file is discarded for being a different song.
    const MIN_PREFIX_CORE: usize = 6;

    /// The same song, by [`SongIdentity`]'s title keys, or one title the start of the
    /// other: the prefix rule absorbs tails no vocabulary names, such as a subtitle MusicBrainz
    /// leaves off. The version rule is what stops it absorbing "(Mad Professor mix)" too.
    pub fn title_matches(requested: &str, matched: &str) -> bool {
        let want = SongIdentity::parse_title(requested, None);
        let got = SongIdentity::parse_title(matched, None);
        // Nothing to judge on. Absence of evidence is not a mismatch.
        if want.key.is_empty() || got.key.is_empty() {
            return true;
        }

        let same_core = SongIdentity::same_title(requested, matched, None).verdict != SongVerdict::Different
            || want.loose_key == got.loose_key
            || Self::prefix_of(&want.key, &got.key);
        if !same_core {
            return false;
        }

        !Self::has_unrequested_variant_marker(requested, matched)
    }

    fn prefix_of(a: &str, b: &str) -> bool {
        let (shorter, longer) = if utf16_len(a) <= utf16_len(b) {
            (a, b)
        } else {
            (b, a)
        };
        utf16_len(shorter) >= Self::MIN_PREFIX_CORE && longer.starts_with(shorter)
    }

    /// One-directional: a match carrying "dub" the request never asked for is a different take;
    /// a request that says "live" against a MusicBrainz title that does not is just MusicBrainz
    /// being tidy, since it writes a live recording's venue and date in the disambiguation and
    /// not the title. The same reading OutsideDownloadService ranks and filters peers by.
    pub fn has_unrequested_variant_marker(requested: &str, matched: &str) -> bool {
        !SongIdentity::added_versions(requested, matched, None).is_empty()
    }

    /// Artist credits legitimately nest. MusicBrainz writes "Massive Attack feat. Elizabeth
    /// Fraser" where Last.fm says "Massive Attack", and sometimes the reverse, so a shared artist
    /// is enough, and so is one key containing the other ("Bjork" in "Björk Guðmundsdóttir").
    /// Two credits that each name a guest the other lacks ("Bizarrap, Duki" against "Bizarrap &
    /// Rauw Alejandro") are a different collaboration.
    ///
    /// `credits` is empty when the match carries no credit list.
    pub fn artist_matches<S: AsRef<str>>(requested: &str, credited_joined: &str, credits: &[S]) -> bool {
        let a = SongIdentity::key(requested);
        let b = SongIdentity::key(credited_joined);
        if a.is_empty() || (b.is_empty() && credits.is_empty()) {
            return true;
        }

        match SongIdentity::compare_artists_with_credits(requested, credited_joined, credits) {
            ArtistAgreement::Agree | ArtistAgreement::Loose | ArtistAgreement::Unknown => return true,
            ArtistAgreement::Conflict => return false,
            ArtistAgreement::None => {}
        }
        !b.is_empty() && (a.contains(&b) || b.contains(&a))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_CREDITS: &[&str] = &[];

    #[test]
    fn normalize_folds_diacritics_ampersands_and_punctuation() {
        for (a, b) in [
            ("Björk", "Bjork"),
            ("Simon & Garfunkel", "Simon and Garfunkel"),
            ("Don't Stop Me Now", "Dont Stop Me Now"),
            ("Blue (Da Ba Dee)", "Blue [Da Ba Dee]"),
        ] {
            assert_eq!(SongIdentity::key(a), SongIdentity::key(b), "{a} / {b}");
        }
    }

    /// Last.fm appends remaster and video tails that MusicBrainz does not carry.
    #[test]
    fn title_matches_remaster_and_video_tails_are_the_same_recording() {
        for (requested, matched) in [
            ("Teardrop - Remastered 2011", "Teardrop"),
            ("Karma Police (Official Video)", "Karma Police"),
            ("Paranoid Android", "Paranoid Android"),
        ] {
            assert!(
                TrackMatchComparer::title_matches(requested, matched),
                "{requested} / {matched}"
            );
        }
    }

    /// The case the length check cannot catch. "Group Four" and "Group Four (Security Forces
    /// dub)" are two seconds apart, so only the variant marker separates them.
    #[test]
    fn title_matches_unrequested_variant_marker_is_a_different_recording() {
        for (requested, matched) in [
            ("Group Four", "Group Four (Security Forces dub)"),
            ("Teardrop", "Teardrop (Mad Professor mix)"),
            ("Creep", "Creep (Live at Glastonbury)"),
        ] {
            assert!(
                !TrackMatchComparer::title_matches(requested, matched),
                "{requested} / {matched}"
            );
        }
    }

    /// Asking for the remix and getting the remix is a match, not a mismatch.
    #[test]
    fn title_matches_requested_variant_matches_the_variant_recording() {
        assert!(TrackMatchComparer::title_matches(
            "Teardrop (Mad Professor mix)",
            "Teardrop (Mad Professor mix)"
        ));
    }

    #[test]
    fn title_matches_different_song_is_a_mismatch() {
        for (requested, matched) in [("Group Four", "Four Seasons"), ("Hotline Bling", "Marvins Room")] {
            assert!(
                !TrackMatchComparer::title_matches(requested, matched),
                "{requested} / {matched}"
            );
        }
    }

    /// Without a length floor on the prefix rule, "Go" matches "Gold" and a correct file is
    /// discarded for being a different song.
    #[test]
    fn title_matches_short_title_is_not_extended_into_another_word() {
        assert!(!TrackMatchComparer::title_matches("Go", "Gold"));
    }

    /// Absence of evidence is not a mismatch; there is nothing to judge on.
    #[test]
    fn title_matches_either_side_empty_passes() {
        for (requested, matched) in [("", "Teardrop"), ("Teardrop", "")] {
            assert!(
                TrackMatchComparer::title_matches(requested, matched),
                "{requested} / {matched}"
            );
        }
    }

    /// MusicBrainz credits features that Last.fm leaves off, and sometimes the reverse.
    #[test]
    fn artist_matches_nested_and_article_credits_are_the_same_artist() {
        for (requested, credited) in [
            ("Massive Attack", "Massive Attack feat. Elizabeth Fraser"),
            ("Massive Attack feat. Elizabeth Fraser", "Massive Attack"),
            ("The Beatles", "Beatles"),
        ] {
            assert!(
                TrackMatchComparer::artist_matches(requested, credited, NO_CREDITS),
                "{requested} / {credited}"
            );
        }
    }

    #[test]
    fn artist_matches_one_of_several_credits_is_enough() {
        assert!(TrackMatchComparer::artist_matches(
            "Elizabeth Fraser",
            "Massive Attack, Elizabeth Fraser",
            &["Massive Attack", "Elizabeth Fraser"]
        ));
    }

    #[test]
    fn artist_matches_different_artist_is_a_mismatch() {
        assert!(!TrackMatchComparer::artist_matches(
            "Drake",
            "Kendrick Lamar",
            NO_CREDITS
        ));
    }

    /// The credit used to be joined with ", ", which happened to equal Last.fm's and Deezer's
    /// spelling of a collaboration. MusicBrainz's own join phrase does not, and a confident match
    /// that fails this reads as a DIFFERENT recording: the file is deleted and the peer
    /// blacklisted.
    #[test]
    fn artist_matches_request_naming_every_credit_matches_whatever_the_join() {
        for (requested, credited) in [
            ("Bizarrap, Rauw Alejandro", "Bizarrap & Rauw Alejandro"),
            ("Queen, David Bowie", "Queen & David Bowie"),
            ("Bizarrap x Rauw Alejandro", "Bizarrap & Rauw Alejandro"),
        ] {
            let credits: Vec<&str> = credited.split(" & ").collect();
            assert!(
                TrackMatchComparer::artist_matches(requested, credited, &credits),
                "{requested} / {credited}"
            );
        }
    }

    #[test]
    fn artist_matches_request_naming_only_some_of_several_credits_is_not_every_credit() {
        assert!(!TrackMatchComparer::artist_matches(
            "Bizarrap, Duki",
            "Bizarrap & Rauw Alejandro",
            &["Bizarrap", "Rauw Alejandro"]
        ));
    }

    /// Same recording AND same version: what decides that two files are duplicates (#53), and
    /// which MusicBrainz recording a kept fingerprint belongs to (#47). A featured artist and a
    /// remaster note are the same take; a version word, a part number or a volume that is not
    /// shared is a different one.
    #[test]
    fn same_version_the_same_take_is_the_same() {
        let strict = SongIdentity::strict_titles();
        for (a, b) in [
            ("Song", "Song"),
            ("Song feat. Someone", "Song"),
            ("Song (feat. Someone)", "Song"),
            ("Mixtape Vol. 53", "Mixtape Vol. 53/66"),
            ("Song (Live)", "Song [Live]"),
            ("Song - Remastered 2011", "Song"),
            ("Song (2011 Remaster)", "Song"),
            ("Song (Remastered Version)", "Song"),
        ] {
            assert!(
                SongIdentity::same_title(a, b, Some(&strict)).is_same(),
                "{a} / {b}"
            );
            assert!(
                SongIdentity::same_title(b, a, Some(&strict)).is_same(),
                "{b} / {a}"
            );
        }
    }

    #[test]
    fn same_version_a_different_take_is_not() {
        let strict = SongIdentity::strict_titles();
        for (a, b) in [
            ("Song", "Song (Live)"),
            ("Song (Remix)", "Song (Live)"),
            ("Shotta Flow", "Shotta Flow 4"),
            ("Crazy Story", "Crazy Story Pt. 3"),
            ("Song", "Song (Radio Edit)"),
            ("Song (Radio Edit)", "Song (Edit)"),
            ("Song", "Another Song"),
            ("", ""),
        ] {
            assert!(
                !SongIdentity::same_title(a, b, Some(&strict)).is_same(),
                "{a} / {b}"
            );
            assert!(
                !SongIdentity::same_title(b, a, Some(&strict)).is_same(),
                "{b} / {a}"
            );
        }
    }

    // ---- AcoustID matches read by SongIdentity ----------------------------------------------

    #[test]
    fn acoust_id_a_match_of_another_version_is_a_mismatch() {
        for (requested, matched) in [
            ("Heat Waves", "Heat Waves (Sped Up)"),
            ("Heat Waves", "Heat Waves (Slowed + Reverb)"),
            ("Creep", "Creep - Live at Glastonbury"),
            ("Song", "Song (Instrumental)"),
            ("Song", "Song (Karaoke Version)"),
            ("Song (Skrillex Remix)", "Song (Skrillex Remix) (Live)"),
            ("Mask Off", "Mask Off Remix"),
        ] {
            assert!(
                !TrackMatchComparer::title_matches(requested, matched),
                "{requested} / {matched}"
            );
        }
    }

    #[test]
    fn acoust_id_the_same_recording_written_otherwise_matches() {
        for (requested, matched) in [
            ("$UICIDE", "Suicide"),
            ("Strobe", "Strobe (Original Mix)"),
            ("Song", "Song (Album Version)"),
            ("Huntin\u{2019} Wabbitz", "Huntin' Wabbitz"),
            ("Real Friends (Explicit)", "Real Friends"),
        ] {
            assert!(
                TrackMatchComparer::title_matches(requested, matched),
                "{requested} / {matched}"
            );
        }
    }

    #[test]
    fn acoust_id_the_same_artist_written_otherwise_matches() {
        for (requested, credited) in [
            ("$uicideboy$", "Suicideboys"),
            ("Kanye West", "Ye"),
            ("Tyler, The Creator", "Tyler, The Creator"),
            ("Lil Peep/iLoveMakonnen", "Lil Peep & iLoveMakonnen"),
        ] {
            assert!(
                TrackMatchComparer::artist_matches(requested, credited, NO_CREDITS),
                "{requested} / {credited}"
            );
        }
    }
}
