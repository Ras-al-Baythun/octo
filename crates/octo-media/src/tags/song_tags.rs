//! The tag writing of a download (the tag body and the album gain rewrite), and the lyrics in a
//! song's own tags that `SongLyrics` and `LyricsSidecarWriter` read and write.
//!
//! The rest of a download's tagging stays with the download service: it opens the file, decides
//! the genres (the genre plan and its Last.fm fallback) and the cover (the cover chain, from the
//! picture the file already holds), then calls [`write_song`] and
//! [`embed_cover`] and saves:
//!
//! ```ignore
//! let mut file = TagFile::open(path)?;
//! let existing_genres = file.genres();
//! let embedded = file.pictures().into_iter().find(|p| p.picture_type == FRONT_COVER)
//!     .or_else(|| file.pictures().into_iter().next());
//! // ... the genre plan and the cover chain, awaited ...
//! write_song(&mut file, &song, &genres, filed_elsewhere);
//! if let Some((bytes, mime)) = embed { embed_cover(&mut file, bytes, &mime); }
//! file.save()?;
//! ```

use std::path::Path;

use octo_core::fingerprint::VerificationResult;
use octo_core::models::domain::Song;

use octo_core::common::song_identity::SongIdentity;

use octo_core::common::dotnet::is_null_or_white_space;

use super::release_fact_tags;
use super::tag_file::{TagError, TagFile, TagPicture};
use super::tag_writer_extras::{self as extras, TagFields};

/// What the genre step decided for the genre frame: the genre plan's action, or, with the
/// feature off, the song's own genre (`Write` of that one genre) or nothing (`Keep`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GenreWrite {
    /// Leave the frame as the file has it.
    Keep,
    /// Replace the genres with `genres`.
    Write(Vec<String>),
    /// Remove every genre: destructive, so the caller logs it.
    Clear,
}

/// The tag body of a download: the Song's fields onto the open file, in a fixed order (a new
/// frame goes at the end, so the order decides where each one lands). The genre frame takes
/// `genres`; the cover is [`embed_cover`]'s. `filed_elsewhere` says the file arrived filed
/// under another album (`release_fact_tags::filed_elsewhere`), whose facts the caller took off
/// first with `release_fact_tags::tidy`.
pub fn write_song(file: &mut TagFile, song: &Song, genres: &GenreWrite, filed_elsewhere: bool) {
    // Basic metadata. Title/artist we always have; only overwrite album + album-artist when we
    // actually resolved them, so a well-tagged FLAC keeps its own album if Deezer had
    // no match.
    if !song.title.is_empty() {
        file.set_title(Some(&song.title));
    }
    if !song.artist.is_empty() {
        file.set_performers(std::slice::from_ref(&song.artist));
    }
    // The full credit stays in the artist tag; each artist also gets a value of their own, so
    // Navidrome files a collaboration under every one of them (#49).
    if song.artists.len() > 1 {
        extras::set_multi_value(file, "ARTISTS", &song.artists);
    }
    if !song.album.is_empty() {
        file.set_album(Some(&song.album));
    }
    if let Some(album_artist) = song.album_artist.as_ref().filter(|name| !name.is_empty()) {
        file.set_album_artists(std::slice::from_ref(album_artist));
    } else if !song.artist.is_empty() {
        // A list of credits as album artist scatters the album view the way it scattered
        // folders, so the first credit stands in when a source named one.
        let first = song.primary_artist.clone().unwrap_or_else(|| song.artist.clone());
        file.set_album_artists(&[first]);
    }

    // Only write the track number when we actually have one, and only pair the total with it
    //: avoids a bogus "0/11" when Deezer's search result carried the album total but not this
    // track's position.
    if let Some(track @ 1..) = song.track {
        file.set_track(track as u32);
        if let Some(total) = song.total_tracks {
            file.set_track_count(total as u32);
        }
    }
    if let Some(disc) = song.disc_number {
        file.set_disc(disc as u32);
        if let Some(discs) = song.total_discs.filter(|discs| *discs > 0 && *discs >= disc) {
            file.set_disc_count(discs as u32);
        }
    }
    release_fact_tags::drop_other_totals(file);
    if let Some(year) = song.year {
        file.set_year(year as u32);
    }

    match genres {
        GenreWrite::Keep => {}
        GenreWrite::Write(genres) => file.set_genres(genres),
        GenreWrite::Clear => file.set_genres(&[]),
    }

    if let Some(bpm) = song.bpm {
        file.set_bpm(bpm as u32);
    }
    if !song.contributors.is_empty() {
        file.set_composers(&song.contributors);
    }
    if let Some(copyright) = song.copyright.as_deref().filter(|text| !text.is_empty()) {
        file.set_copyright(Some(copyright));
    }

    // What the fingerprint proved (#48), so no later pass has to identify this file again. No
    // album id: Navidrome groups albums by MUSICBRAINZ_ALBUMID before the album name, so one
    // track carrying it beside another without it splits an album. The group id is written
    // only when the album really is that release.
    if let Some(recording) = song
        .music_brainz_recording_id
        .as_deref()
        .filter(|id| !id.is_empty())
    {
        extras::set_recording_id(file, recording);
    }
    let album_is_release = song
        .music_brainz_release_group_id
        .as_deref()
        .is_some_and(|id| !id.is_empty())
        && VerificationResult::album_is_from_release(song);
    if album_is_release {
        file.set_music_brainz_release_group_id(song.music_brainz_release_group_id.as_deref());
    }
    if !song.music_brainz_artist_ids.is_empty() {
        extras::set_multi(file, TagFields::ARTIST_ID, &song.music_brainz_artist_ids);
    }
    // The flag is album-level: when the chooser set the album, a peer's stale flag from the
    // compilation the file was ripped from would file the studio album as one.
    if song.is_compilation {
        extras::set_compilation(file, true);
    } else if filed_elsewhere
        || song
            .tag_plan
            .as_ref()
            .is_some_and(|plan| plan.album_from_candidate() && !plan.rehearsed)
    {
        extras::set_compilation(file, false);
    }

    // The rest of what a release is: its code, its label and catalog number, its barcode, its
    // kind and status, where and when it came out, and the ids that name it. The code has its
    // own field now; the "ISRC: x" comment is no longer written, and a comment the file arrived
    // with is left alone. Every setter skips an empty value.
    let isrc = song.isrc.as_deref().and_then(SongIdentity::normalize_isrc);
    extras::set_text(file, TagFields::ISRC, isrc.as_deref());
    extras::set_text(file, TagFields::LABEL, song.label.as_deref());
    extras::set_text(file, TagFields::CATALOG_NUMBER, song.catalog_number.as_deref());
    extras::set_text(file, TagFields::BARCODE, song.barcode.as_deref());
    if let Some(release_type) = song.release_type.as_deref().filter(|text| !text.is_empty()) {
        let types: Vec<String> = release_type
            .split("; ")
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect();
        extras::set_multi(file, TagFields::RELEASE_TYPE, &types);
    }
    extras::set_text(file, TagFields::RELEASE_STATUS, song.release_status.as_deref());
    extras::set_text(file, TagFields::RELEASE_COUNTRY, song.release_country.as_deref());
    extras::set_original_date(file, song.original_date.as_deref());
    if album_is_release {
        extras::set_release_track_id(file, song.music_brainz_release_track_id.as_deref());
        extras::set_multi(
            file,
            TagFields::ALBUM_ARTIST_ID,
            &song.music_brainz_album_artist_ids,
        );
    }
    extras::set_text(file, TagFields::FINGERPRINT_ID, song.acoust_id.as_deref());
    extras::set_replay_gain(
        file,
        song.replay_gain_track_gain_db,
        song.replay_gain_track_peak,
        song.replay_gain_album_gain_db,
        song.replay_gain_album_peak,
    );
    // A value Octo wrote under one name is the only one: a peer's copy under another name the
    // library server also reads would be a second label or a second barcode.
    for (field, value) in [
        (TagFields::BARCODE, &song.barcode),
        (TagFields::LABEL, &song.label),
        (TagFields::RELEASE_TYPE, &song.release_type),
        (TagFields::RELEASE_STATUS, &song.release_status),
        (TagFields::RELEASE_COUNTRY, &song.release_country),
    ] {
        if !is_null_or_white_space(value.as_deref()) {
            release_fact_tags::drop_other_names(file, field);
        }
    }
    extras::set_advisory(file, song.advisory);
}

/// The cover the chain chose, as the only picture: a front cover described as "Cover", with
/// the MIME type of the bytes.
pub fn embed_cover(file: &mut TagFile, bytes: Vec<u8>, mime_type: &str) {
    file.set_pictures(&[TagPicture::front_cover(bytes, mime_type)]);
}

/// The front cover a file already holds, else its first picture.
pub fn front_cover(file: &TagFile) -> Option<TagPicture> {
    let pictures = file.pictures();
    let front = pictures
        .iter()
        .position(|picture| picture.picture_type == super::tag_file::FRONT_COVER);
    pictures.into_iter().nth(front.unwrap_or(0))
}

/// The album gain rewrite of one file: the album gain and peak, in place, so the library
/// server keeps the file's id.
pub fn write_album_gain(path: &Path, gain_db: f64, peak: f64) -> Result<(), TagError> {
    let mut file = TagFile::open(path)?;
    extras::set_replay_gain(&mut file, None, None, Some(gain_db), Some(peak));
    file.save()
}

/// The lyrics in the song's own tags, or an error when the file cannot be read as audio.
pub fn read_lyrics(path: &Path) -> Result<Option<String>, TagError> {
    Ok(TagFile::open(path)?.lyrics())
}

/// Writes the lyrics into the song's own tags and saves: None or empty removes them.
pub fn write_lyrics(path: &Path, lyrics: Option<&str>) -> Result<(), TagError> {
    let mut file = TagFile::open(path)?;
    file.set_lyrics(lyrics);
    file.save()
}

/// The branches of the tag body the fixture scenarios do not reach. The tests of the whole
/// download go with the download service.
#[cfg(test)]
mod tests {
    use octo_core::tagging::{ScoredCandidate, TagConfidence, TagPlan};

    use super::*;
    use crate::tags::test_support::{flac, write};

    fn tagged_compilation(dir: &Path) -> std::path::PathBuf {
        let path = write(dir, "song.flac", &flac());
        let mut file = TagFile::open(&path).expect("opens");
        extras::set_compilation(&mut file, true);
        file.save().expect("saved");
        path
    }

    fn chosen(rehearsed: bool) -> Box<TagPlan> {
        Box::new(TagPlan {
            confidence: TagConfidence::Strong,
            chosen: Some(ScoredCandidate::default()),
            rehearsed,
            ..Default::default()
        })
    }

    /// The flag is album-level: an album the chooser set clears a peer's stale flag, unless the
    /// plan was only rehearsed.
    #[test]
    fn a_chosen_album_clears_a_peers_compilation_flag_unless_rehearsed() {
        for (rehearsed, expected) in [(false, false), (true, true)] {
            let dir = tempfile::tempdir().expect("a temp dir");
            let path = tagged_compilation(dir.path());
            let song = Song {
                title: "T".into(),
                tag_plan: Some(chosen(rehearsed)),
                ..Default::default()
            };
            let mut file = TagFile::open(&path).expect("opens");
            write_song(&mut file, &song, &GenreWrite::Keep, false);
            file.save().expect("saved");
            assert_eq!(
                extras::is_compilation(&TagFile::open(&path).expect("opens")),
                expected,
                "{rehearsed}"
            );
        }
    }

    /// The group id, release track id and album artist ids go on only when the album is the
    /// release the fingerprint named; the recording id always does.
    #[test]
    fn release_ids_are_written_only_when_the_album_is_that_release() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = write(dir.path(), "song.flac", &flac());
        let song = Song {
            title: "Teardrop".into(),
            album: "Mezzanine (Deluxe)".into(),
            music_brainz_album_title: Some("Mezzanine".into()),
            music_brainz_release_group_id: Some("group".into()),
            music_brainz_release_track_id: Some("track".into()),
            music_brainz_album_artist_ids: vec!["artist".into()],
            music_brainz_recording_id: Some("recording".into()),
            ..Default::default()
        };
        let mut file = TagFile::open(&path).expect("opens");
        write_song(&mut file, &song, &GenreWrite::Clear, false);
        file.save().expect("saved");

        let mut file = TagFile::open(&path).expect("opens");
        assert_eq!(file.music_brainz_release_group_id(), None);
        assert_eq!(extras::read_text(&file, TagFields::RELEASE_TRACK_ID), None);
        assert_eq!(extras::read_text(&file, TagFields::ALBUM_ARTIST_ID), None);
        assert_eq!(extras::read_recording_id(&mut file).as_deref(), Some("recording"));
    }

    #[test]
    fn the_album_gain_is_written_in_place_and_a_cover_is_the_front_cover() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = write(dir.path(), "song.flac", &flac());
        write_album_gain(&path, -7.25, 0.5).expect("written");
        let mut file = TagFile::open(&path).expect("opens");
        assert_eq!(
            extras::read_text(&file, TagFields::ALBUM_GAIN).as_deref(),
            Some("-7.25 dB")
        );
        assert_eq!(
            extras::read_text(&file, TagFields::ALBUM_PEAK).as_deref(),
            Some("0.500000")
        );
        assert_eq!(front_cover(&file), None);

        embed_cover(&mut file, vec![0xFF, 0xD8, 0xFF, 0xD9], "image/jpeg");
        file.save().expect("saved");
        let cover = front_cover(&TagFile::open(&path).expect("opens")).expect("a cover");
        assert_eq!(
            cover,
            TagPicture::front_cover(vec![0xFF, 0xD8, 0xFF, 0xD9], "image/jpeg")
        );
    }
}
