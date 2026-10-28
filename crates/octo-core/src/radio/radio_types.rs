//! What a radio starts from, how closely a source's answer is about it, and the answer.
//! `IRadioSource` (a source with I/O) is `octo::services::radio::radio_source`.

use crate::last_fm::SimilarTrack;
use crate::models::domain::Song;

/// What a radio starts from.
///
/// `title`: empty for an artist's radio. `library_song`: the library song, when it is one: its
/// album, genre and file. `you_tube_id`: a video already known to play the seed. `is_video`: the
/// seed is a YouTube video found as itself (an ambience, a mix), whose radio may suggest videos.
#[derive(Debug, Clone, Default)]
pub struct RadioSeed {
    pub artist: String,
    pub title: String,
    pub duration: Option<i32>,
    pub library_song: Option<Song>,
    pub you_tube_id: Option<String>,
    pub is_video: bool,
}

impl RadioSeed {
    pub fn new(
        artist: impl Into<String>,
        title: impl Into<String>,
        duration: Option<i32>,
        library_song: Option<Song>,
    ) -> Self {
        RadioSeed {
            artist: artist.into(),
            title: title.into(),
            duration,
            library_song,
            you_tube_id: None,
            is_video: false,
        }
    }

    pub fn is_artist(&self) -> bool {
        self.title.is_empty()
    }
}

/// How closely a source's answer is about the seed itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RadioMatch {
    #[default]
    None,
    /// Songs like this very song.
    Song,
    /// Songs like the song a nightcore, sped up or remixed seed came from.
    Original,
    /// The most played songs of the seed's genre.
    Genre,
    /// Songs of artists like the seed's artist, an artist the source knows for who they are.
    ArtistsTrusted,
    /// Songs of artists like a name the source may only know as an uploader.
    ArtistsGuessed,
}

impl std::fmt::Display for RadioMatch {
    /// The member's name, as the log lines write it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RadioMatch::None => "None",
            RadioMatch::Song => "Song",
            RadioMatch::Original => "Original",
            RadioMatch::Genre => "Genre",
            RadioMatch::ArtistsTrusted => "ArtistsTrusted",
            RadioMatch::ArtistsGuessed => "ArtistsGuessed",
        })
    }
}

/// One source's answer: songs outside the library to resolve, or library songs.
///
/// `library_songs_complete`: false when the library songs carry only an id, a title and an
/// artist (Sounds alike builds them from its own store): those are read from Navidrome, with the
/// listener's sign-in, before they go out.
#[derive(Debug, Clone)]
pub struct RadioAnswer {
    pub provider: String,
    pub r#match: RadioMatch,
    pub tracks: Vec<SimilarTrack>,
    pub library_songs: Vec<Song>,
    pub library_songs_complete: bool,
}

/// One item of an answer: `(artist, title, library song, track)`.
pub type RadioItem<'a> = (&'a str, &'a str, Option<&'a Song>, Option<&'a SimilarTrack>);

impl RadioAnswer {
    pub fn new(
        provider: impl Into<String>,
        r#match: RadioMatch,
        tracks: Vec<SimilarTrack>,
        library_songs: Vec<Song>,
    ) -> Self {
        RadioAnswer {
            provider: provider.into(),
            r#match,
            tracks,
            library_songs,
            library_songs_complete: true,
        }
    }

    pub fn nothing(provider: impl Into<String>) -> Self {
        Self::new(provider, RadioMatch::None, Vec::new(), Vec::new())
    }

    pub fn count(&self) -> usize {
        self.tracks.len() + self.library_songs.len()
    }

    /// Library songs first, then tracks, in the source's own order.
    pub fn items(&self) -> impl Iterator<Item = RadioItem<'_>> {
        self.library_songs
            .iter()
            .map(|song| (song.artist.as_str(), song.title.as_str(), Some(song), None))
            .chain(
                self.tracks
                    .iter()
                    .map(|track| (track.artist.as_str(), track.title.as_str(), None, Some(track))),
            )
    }
}
