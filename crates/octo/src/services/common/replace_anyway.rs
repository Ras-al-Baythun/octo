//! Replacing a library song with a copy picked in Find songs that is no better than it: what
//! stands in the way, the words that ask the person, and the song a forced replacement is held
//! to.
//!
//! A pick that only fails on quality (the copy is the same or lower quality than the library's)
//! is asked about rather than refused. So is a pick of the plain song in place of a library file
//! that is another take of it (a live recording, a remix or edit): the file's own title and album
//! describe that take, so the plain song is what the copy is held to instead.

use octo_core::common::song_identity::SongIdentity;
use octo_core::common::{dotnet, live_version, version_variant};

use super::download_picks::AnywaySong;
use super::song_finder::{FindTarget, FoundCopy, is_lossless};

/// What stands between a picked copy and the library song it would replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceCheck {
    /// Nothing: the copy is better and fits the library song.
    Fits,
    /// Only the person's word. `words` ask them; `song` is what the copy is held to once they
    /// say yes; `take` is how the library file differs from the plain song, when it does.
    Confirm {
        words: String,
        song: AnywaySong,
        take: Option<OtherTake>,
    },
    /// The copy may not replace the library song, with the words why.
    Refused(String),
}

/// How the library file differs from the plain song.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtherTake {
    Live,
    Version,
}

/// Why a copy, by quality alone, would not replace the library's: None when it is better. The
/// same rule Better quality holds a replacement to when it arrives: only a FLAC-class file
/// replaces a lossy one, and nothing replaces a FLAC-class file.
pub fn quality_words(target: &FindTarget, copy: &FoundCopy) -> Option<String> {
    let owned_format = target.owned_format.as_deref();
    let yours = owned_short(target);
    let lidarr_choice = copy.pick.release().is_none();
    let theirs = if lidarr_choice { None } else { copy_short(copy) };
    let copy_lossless = is_lossless(first_word(copy.shown.format.as_deref()));

    if is_lossless(owned_format) {
        let Some(theirs) = theirs else {
            return Some(if lidarr_choice {
                format!("Yours is already {yours}, so Lidarr's choice cannot be better.")
            } else {
                format!("Yours is already {yours}, so this copy cannot be better.")
            });
        };
        return Some(if !copy_lossless {
            format!("This copy is {theirs} and yours is {yours}, so it sounds worse.")
        } else if dotnet::eq_ignore_case(&theirs, &yours) {
            format!("This copy is {theirs} like yours, so it is not better.")
        } else {
            format!("This copy is {theirs} and yours is {yours}, so it is not better.")
        });
    }
    // Lidarr's choice searches FLAC-class releases only for a lossy library song.
    if lidarr_choice || copy_lossless {
        return None;
    }
    let theirs = theirs.unwrap_or_else(|| "a copy of unknown quality".to_string());
    Some(if dotnet::eq_ignore_case(&theirs, &yours) {
        format!("This copy is {theirs} like yours, so it is not better.")
    } else if matches!((bit_rate_of(&theirs), bit_rate_of(&yours)), (Some(a), Some(b)) if a < b) {
        format!("This copy is {theirs} and yours is {yours}, so it sounds worse.")
    } else {
        format!("This copy is {theirs} and yours is {yours}, so Octo does not count it as better.")
    })
}

/// The song the library file is, as the download's checks judge a copy against it.
pub fn library_song(target: &FindTarget) -> AnywaySong {
    AnywaySong {
        title: target.title.clone(),
        album: target.album.clone(),
        length: target.duration,
        retitle: false,
    }
}

/// How the library file is another take of the song, and the plain song a copy is held to in
/// its place; None when nothing says it is one.
///
/// Its title, album, or album folder naming a version (live, remix, edit, ...) makes it one.
///
/// The plain song is the title without its version, the album only when it names no version,
/// and the library file's own length, which is all that is known of it.
pub fn other_take(target: &FindTarget) -> Option<(OtherTake, AnywaySong)> {
    let artist = Some(target.artist.as_str());
    let title_versions = version_variant::requested(Some(&target.title), None);
    let title_live = live_version::mentions(Some(&target.title));
    let album = target.album.as_deref();
    let album_versions = album
        .map(|a| version_variant::requested(None, Some(a)))
        .unwrap_or_default();
    let album_live =
        album.is_some_and(|a| live_version::mentions(Some(&version_variant::without_artist(a, artist))));
    let folder_versions = target
        .owned_path
        .as_deref()
        .map(|path| version_variant::carried(path, artist))
        .unwrap_or_default();

    let marked_title = title_live || !title_versions.is_empty();
    let marked_album = album_live || !album_versions.is_empty() || !folder_versions.is_empty();
    let title = if marked_title {
        let core = SongIdentity::parse_title(&target.title, artist).core;
        if dotnet::is_blank(&core) {
            target.title.clone()
        } else {
            core
        }
    } else {
        target.title.clone()
    };
    let album = if marked_album { None } else { target.album.clone() };

    let take = if title_live || album_live || folder_versions.contains("live") {
        OtherTake::Live
    } else if marked_title || marked_album {
        OtherTake::Version
    } else {
        return None;
    };
    Some((
        take,
        AnywaySong {
            title,
            album,
            length: target.duration.filter(|d| *d > 0),
            retitle: true,
        },
    ))
}

/// How the library file is another take of the song, read before anything is picked, for the
/// list's header: [`other_take`]'s reading.
pub fn library_take(target: &FindTarget) -> Option<(OtherTake, AnywaySong)> {
    other_take(target)
}

/// What stands between this copy and the library song: nothing, only the person's word, or a
/// refusal. Read again when the person says yes, so the answer never rests on an earlier one.
pub fn check(target: &FindTarget, copy: &FoundCopy) -> ReplaceCheck {
    let quality = quality_words(target, copy);
    let own = library_song(target);
    let Some((take, plain)) = other_take(target) else {
        return match quality {
            None => ReplaceCheck::Fits,
            Some(quality) => ReplaceCheck::Confirm {
                words: format!("{quality} Replace yours anyway?"),
                song: own,
                take: None,
            },
        };
    };
    let take_words = match take {
        OtherTake::Live => "Yours looks like a live recording and this copy is the plain song.",
        OtherTake::Version => "Yours looks like another version and this copy is the plain song.",
    };
    let words = match quality {
        Some(quality) => format!("{take_words} {quality} Replace yours anyway?"),
        None => format!("{take_words} Replace yours anyway?"),
    };
    ReplaceCheck::Confirm {
        words,
        song: plain,
        take: Some(take),
    }
}

/// The list's header line for a library file that is another take of the song.
pub fn take_header(take: OtherTake) -> String {
    match take {
        OtherTake::Live => {
            "Yours looks like a live recording. Pick the plain song to replace it.".to_string()
        }
        OtherTake::Version => {
            "Yours looks like another version of this song. Pick the plain song to replace it.".to_string()
        }
    }
}

/// The take's kind as the apps read it: "live" or "version".
pub fn take_kind(take: OtherTake) -> &'static str {
    match take {
        OtherTake::Live => "live",
        OtherTake::Version => "version",
    }
}

/// What the library file is, in a few words, beside the copy.
fn take_name(take: OtherTake) -> &'static str {
    match take {
        OtherTake::Live => "live recording",
        OtherTake::Version => "another version",
    }
}

/// How a copy compares with the library's by quality alone, for a small tag on the list: None
/// when it is better (or the look is not for a library song).
pub fn vs_yours(target: &FindTarget, copy: &FoundCopy) -> Option<(&'static str, &'static str)> {
    target.library_id.as_ref()?;
    quality_words(target, copy)?;
    let yours = owned_short(target);
    let lidarr_choice = copy.pick.release().is_none();
    let theirs = if lidarr_choice { None } else { copy_short(copy) };
    let copy_lossless = theirs
        .as_deref()
        .is_some_and(|t| is_lossless(first_word(Some(t))));
    let owned_lossless = is_lossless(target.owned_format.as_deref());
    let same = theirs
        .as_deref()
        .is_some_and(|t| dotnet::eq_ignore_case(t, &yours))
        || (owned_lossless && copy_lossless);
    let lower = theirs.as_deref().is_some_and(|t| {
        (owned_lossless && !copy_lossless)
            || matches!((bit_rate_of(t), bit_rate_of(&yours)), (Some(a), Some(b)) if a < b)
    });
    Some(if same {
        ("same", "Same as yours")
    } else if lower {
        ("lower", "Lower than yours")
    } else {
        ("notBetter", "Not better than yours")
    })
}

/// One side of the comparison the confirm shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyFacts {
    pub quality: Option<String>,
    pub length: Option<i32>,
    pub take: Option<String>,
}

/// The library's copy and the picked one, side by side.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickCompare {
    pub yours: CopyFacts,
    pub copy: CopyFacts,
}

/// The two copies side by side: their quality in a word or two, their lengths, and, when the
/// library file is another take, what each one is.
pub fn compare(target: &FindTarget, copy: &FoundCopy, take: Option<OtherTake>) -> PickCompare {
    let lidarr_choice = copy.pick.release().is_none();
    PickCompare {
        yours: CopyFacts {
            quality: Some(owned_short(target)),
            length: target.duration.filter(|d| *d > 0),
            take: take.map(|take| take_name(take).to_string()),
        },
        copy: CopyFacts {
            quality: if lidarr_choice {
                Some("Lidarr's choice".to_string())
            } else {
                copy_short(copy)
            },
            length: copy.shown.length.filter(|l| *l > 0),
            take: take.map(|_| "the plain song".to_string()),
        },
    }
}

/// The library file's quality in a word or two: "FLAC", "MP3 320".
fn owned_short(target: &FindTarget) -> String {
    let format = target
        .owned_format
        .as_deref()
        .map(|f| f.trim().trim_start_matches('.'))
        .unwrap_or("");
    let upper = dotnet::to_upper_invariant(format);
    if is_lossless(Some(format)) {
        return upper;
    }
    let rate = target.owned_quality.as_deref().and_then(kbps_in);
    match rate {
        Some(rate) if !upper.is_empty() => format!("{upper} {rate}"),
        _ if !upper.is_empty() => upper,
        _ => "a copy of unknown quality".to_string(),
    }
}

/// The copy's quality in a word or two: "FLAC", "MP3 320". Lidarr names its qualities "FLAC",
/// "FLAC 24bit", "MP3-320".
fn copy_short(copy: &FoundCopy) -> Option<String> {
    let format = copy
        .shown
        .format
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())?;
    if is_lossless(first_word(Some(format))) {
        return first_word(Some(format)).map(dotnet::to_upper_invariant);
    }
    Some(dotnet::to_upper_invariant(&format.replace('-', " ")))
}

/// "MP3 320 kbps" -> 320.
fn kbps_in(quality: &str) -> Option<i32> {
    let words: Vec<&str> = quality.split_whitespace().collect();
    words
        .windows(2)
        .find(|pair| dotnet::eq_ignore_case(pair[1], "kbps"))
        .and_then(|pair| pair[0].parse().ok())
}

/// "MP3 320" -> 320.
fn bit_rate_of(short: &str) -> Option<i32> {
    short.split_whitespace().nth(1).and_then(|rate| rate.parse().ok())
}

fn first_word(quality: Option<&str>) -> Option<&str> {
    quality?.split([' ', '-']).find(|part| !part.is_empty())
}

#[cfg(test)]
#[path = "replace_anyway_tests.rs"]
mod tests;
