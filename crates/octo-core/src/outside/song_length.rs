//! The rules for the length a client is shown for a song Octo found outside the library.

use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::outside::routing::OutsideRouting;

/// Where the length shown for an outside song came from, weakest first. A shown length is
/// only ever replaced by one from the same or a stronger source.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize_repr, Deserialize_repr,
)]
#[repr(i32)]
pub enum LengthSource {
    /// Nothing is known. The song goes out with no length.
    #[default]
    None = 0,

    /// The length of a YouTube video found for the song. Weakest because a video
    /// can carry an intro, an outro or a whole music-video skit.
    Video = 1,

    /// Last.fm's length: the one handed in with a radio track, or track.getInfo.
    /// Often missing, rarely wrong.
    LastFm = 2,

    /// Deezer's catalog length for the matched track.
    Deezer = 3,

    /// The length of the audio in a file downloaded for the song: what it really runs.
    File = 4,
}

/// The rules for the length a client is shown for a song Octo found outside the library.
///
/// This is display only. It lives on [`OutsideRouting::shown_duration`], never on
/// [`OutsideRouting::duration`], because Duration is what a download ranks peer files by and
/// checks the finished file against, to within 8 seconds. A length filled in so a row can
/// show one must not start rejecting files a download used to accept.
///
/// A shared routing sits behind a mutex
/// (`octo::services::outside::external_id_registry::SharedRouting`), so these take the routing
/// the caller already holds locked.
pub struct SongLength;

impl SongLength {
    /// Shorter than this, a video is a clip or a teaser, not the song.
    pub const MIN_VIDEO_SECONDS: i32 = 30;

    /// Longer than this, a video is a live set, a mix or a whole album.
    pub const MAX_VIDEO_SECONDS: i32 = 20 * 60;

    /// The video's length when it is plausibly one song, or None.
    pub fn sane_video_length(seconds: Option<i32>) -> Option<i32> {
        seconds.filter(|&s| (Self::MIN_VIDEO_SECONDS..=Self::MAX_VIDEO_SECONDS).contains(&s))
    }

    /// Longer than this, even a video found as itself (an ambience, a mix) is a live stream or a
    /// day-long loop rather than something to play through.
    pub const MAX_LONG_VIDEO_SECONDS: i32 = 4 * 60 * 60;

    /// The length of a video found as itself ([`OutsideRouting::is_video`]), which may run for
    /// hours, or None.
    pub fn sane_long_video_length(seconds: Option<i32>) -> Option<i32> {
        seconds.filter(|&s| (Self::MIN_VIDEO_SECONDS..=Self::MAX_LONG_VIDEO_SECONDS).contains(&s))
    }

    /// How far a stream's length may sit from a known length and still replace it: 7 seconds,
    /// or 7 % of a long song.
    pub const STREAM_LENGTH_SLACK_SECONDS: i32 = 7;
    pub const STREAM_LENGTH_SLACK_PERCENT: i32 = 7;

    /// Whether a length learned from a stream may be written over `known`, the routing's own
    /// length. Only a length that could be one song counts ([`Self::sane_video_length`]). It
    /// fills an unknown length; a known one it replaces only when it is close, so a short clip
    /// or a long upload never stands in for the catalog's length.
    pub fn stream_length_may_replace(known: Option<i32>, learned: i32) -> bool {
        if Self::sane_video_length(Some(learned)).is_none() {
            return false;
        }
        let Some(known) = known.filter(|&k| k > 0) else {
            return true;
        };
        let slack = Self::STREAM_LENGTH_SLACK_SECONDS.max(known * Self::STREAM_LENGTH_SLACK_PERCENT / 100);
        (learned - known).abs() <= slack
    }

    /// Store a length on the routing unless a stronger source already gave one. Returns
    /// whether it was stored. A missing or zero length is never stored: no length beats a
    /// made-up one.
    pub fn remember(routing: &mut OutsideRouting, seconds: Option<i32>, source: LengthSource) -> bool {
        let Some(s) = seconds else { return false };
        if s <= 0 || source == LengthSource::None {
            return false;
        }
        let sane = if routing.is_video {
            Self::sane_long_video_length(Some(s))
        } else {
            Self::sane_video_length(Some(s))
        };
        if source == LengthSource::Video && sane.is_none() {
            return false;
        }
        if routing.shown_duration_source > source {
            return false;
        }
        routing.shown_duration = Some(s);
        routing.shown_duration_source = source;
        true
    }

    /// The shown length and where it came from, read together.
    pub fn shown(routing: &OutsideRouting) -> (Option<i32>, LengthSource) {
        (routing.shown_duration, routing.shown_duration_source)
    }

    /// A length from a metadata source is known. A video length is only a
    /// stand-in, and still worth asking Deezer and Last.fm about.
    pub fn has_metadata_length(routing: &OutsideRouting) -> bool {
        Self::shown(routing).1 >= LengthSource::LastFm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn routing(artist: &str, title: &str) -> OutsideRouting {
        OutsideRouting {
            artist: Some(artist.into()),
            title: Some(title.into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_stream_length_replaces_a_known_length_only_when_close_to_it() {
        // Fills an unknown length, when it could be one song.
        assert!(SongLength::stream_length_may_replace(None, 200));
        assert!(SongLength::stream_length_may_replace(Some(0), 200));
        assert!(!SongLength::stream_length_may_replace(None, 3));
        assert!(!SongLength::stream_length_may_replace(None, 3600));
        // Within 7 seconds of a short song, within 7 % of a long one.
        assert!(SongLength::stream_length_may_replace(Some(100), 107));
        assert!(SongLength::stream_length_may_replace(Some(100), 93));
        assert!(!SongLength::stream_length_may_replace(Some(100), 108));
        assert!(SongLength::stream_length_may_replace(Some(201), 215));
        assert!(!SongLength::stream_length_may_replace(Some(201), 216));
        assert!(SongLength::stream_length_may_replace(Some(600), 642));
        assert!(!SongLength::stream_length_may_replace(Some(600), 643));
        // A short stub never stands in for a catalog length.
        assert!(!SongLength::stream_length_may_replace(Some(201), 3));
        assert!(!SongLength::stream_length_may_replace(Some(201), 0));
    }

    #[test]
    fn sane_video_length_keeps_only_what_could_be_one_song() {
        let cases = [
            (29, None),
            (30, Some(30)),
            (417, Some(417)),
            (1200, Some(1200)),
            (1201, None),
            (3600, None),
        ];
        for (seconds, expected) in cases {
            assert_eq!(
                SongLength::sane_video_length(Some(seconds)),
                expected,
                "{seconds}"
            );
        }
    }

    #[test]
    fn a_video_found_as_itself_keeps_its_hours_long_length() {
        let hour = 75 * 60;
        let mut song = routing("A", "Song");
        assert!(!SongLength::remember(&mut song, Some(hour), LengthSource::Video));
        let mut video = OutsideRouting {
            is_video: true,
            ..routing("Ambience Academy", "Ba Sing Se")
        };
        assert!(SongLength::remember(&mut video, Some(hour), LengthSource::Video));
        assert_eq!(SongLength::shown(&video).0, Some(hour));
        assert_eq!(SongLength::sane_long_video_length(Some(5 * 60 * 60)), None);
        assert_eq!(SongLength::sane_long_video_length(Some(10)), None);
    }

    #[test]
    fn remember_stronger_source_replaces_weaker_never_the_other_way() {
        let mut r = routing("Daft Punk", "Emotion");

        assert!(SongLength::remember(&mut r, Some(430), LengthSource::Video));
        assert!(SongLength::remember(&mut r, Some(418), LengthSource::LastFm));
        assert!(SongLength::remember(&mut r, Some(417), LengthSource::Deezer));
        assert_eq!(SongLength::shown(&r), (Some(417), LengthSource::Deezer));

        assert!(!SongLength::remember(&mut r, Some(418), LengthSource::LastFm));
        assert!(!SongLength::remember(&mut r, Some(430), LengthSource::Video));
        assert_eq!(SongLength::shown(&r), (Some(417), LengthSource::Deezer));

        assert!(SongLength::remember(&mut r, Some(419), LengthSource::File));
        assert!(!SongLength::remember(&mut r, Some(417), LengthSource::Deezer));
        assert_eq!(SongLength::shown(&r), (Some(419), LengthSource::File));
    }

    #[test]
    fn remember_never_stores_a_missing_zero_or_implausible_length() {
        let mut r = routing("A", "T");

        assert!(!SongLength::remember(&mut r, None, LengthSource::Deezer));
        assert!(!SongLength::remember(&mut r, Some(0), LengthSource::LastFm));
        assert!(!SongLength::remember(&mut r, Some(3600), LengthSource::Video));
        assert!(!SongLength::remember(&mut r, Some(12), LengthSource::Video));

        assert_eq!(SongLength::shown(&r), (None, LengthSource::None));
    }

    #[test]
    fn remember_a_long_metadata_length_is_not_bound_like_a_video() {
        // The range guards against a video carrying more than the song. A catalog length
        // for a 25-minute track is simply the track.
        let mut r = routing("A", "T");
        assert!(SongLength::remember(&mut r, Some(1500), LengthSource::Deezer));
    }

    #[test]
    fn has_metadata_length_is_last_fm_or_better() {
        let mut r = routing("A", "T");
        assert!(!SongLength::has_metadata_length(&r));
        SongLength::remember(&mut r, Some(200), LengthSource::Video);
        assert!(!SongLength::has_metadata_length(&r));
        SongLength::remember(&mut r, Some(201), LengthSource::LastFm);
        assert!(SongLength::has_metadata_length(&r));
    }
}
