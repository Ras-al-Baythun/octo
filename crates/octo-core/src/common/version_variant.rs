//! Which versions of a song a peer's file is, read from the places SongIdentity does not look:
//! the album folder a plainly named track sits in ("Too Close (Radio Edit) - Single", "Club Hits
//! (Extended Mixes)", "Mezzanine Remix Tapes '98"), and a file name that runs its version on with
//! no brackets ("09 Next-Too Close-radio edit"). Live takes keep their own, more generous rule in
//! [`live_version`].
//!
//! Some of these patterns need lookarounds, which the `regex` crate has none of: `(?!...)` after
//! "extended" and the `(?<![\p{L}\p{N}])...(?![\p{L}\p{N}])` name bounds are checked by hand
//! around each match, trying every start a backtracking engine would try.

use std::collections::{BTreeSet, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use unicode_general_category::{GeneralCategory, get_general_category};

use super::dotnet::{is_blank, is_letter, utf16_class_view, utf16_len, word_boundary_view};
use super::live_version;
use super::song_identity::SongIdentity;

static TITLE_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[\(\[\{]([^\(\)\[\]\{\}]*)[\)\]\}]").expect("a fixed pattern"));

static TITLE_WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("a fixed pattern"));

/// The part of a title a file name has to carry: the title without its guest credit and its
/// version tags. "Take Care (feat. Rihanna)" used to demand the words "feat" and "Rihanna",
/// and "Too Close (Radio Edit)" the words "radio" and "edit", so a plainly named
/// "04 - Take Care.flac" never answered either and only files that spelled the tag out were
/// ever downloaded. Which version a file is gets decided by the version rules, not here. A
/// title with nothing left once they are off is kept whole.
pub fn name_title(title: &str) -> String {
    fn is_version(tag: &str) -> bool {
        !SongIdentity::parse_title(&format!("x ({tag})"), None)
            .versions
            .is_empty()
    }

    let text = SongIdentity::strip_features(title);
    let mut text = TITLE_BRACKET
        .replace_all(&text, |caps: &regex::Captures| {
            if is_version(&caps[1]) {
                " ".to_string()
            } else {
                caps[0].to_string()
            }
        })
        .into_owned();
    // " - Radio Edit", " - Remastered 2011": a dash tail that is only a version tag.
    for _ in 0..3 {
        match text.rfind(" - ") {
            Some(dash) if dash > 0 && is_version(&text[dash + 3..]) => text.truncate(dash),
            _ => break,
        }
    }
    let text = TITLE_WHITESPACE.replace_all(&text, " ").trim().to_string();
    if SongIdentity::key(&text).is_empty() {
        title.to_string()
    } else {
        text
    }
}

/// A case-insensitive pattern, with no locale.
fn rxi(pattern: &str) -> Regex {
    Regex::new(&format!("(?i){pattern}")).expect("a fixed pattern compiles")
}

/// One word that names a version wherever it stands, and what may not follow it.
struct Word {
    pattern: Regex,
    not_followed_by: Option<Regex>,
    name: &'static str,
}

fn word(pattern: &str, name: &'static str) -> Word {
    Word {
        pattern: rxi(pattern),
        not_followed_by: None,
        name,
    }
}

/// Words that name a version wherever they stand, bracketed or not, under SongIdentity's own
/// names so the two readings can be compared. Only words no album, artist or label is likely
/// to be called: no bare "clean" (Clean Bandit) or "acoustic" (Acoustic Soul); a bracket
/// still reads those.
static WORDS: LazyLock<Vec<Word>> = LazyLock::new(|| {
    vec![
        word(r"\bradio\s+(?:edit|version|mix|cut)s?\b", "radio edit"),
        // `\bextended\b(?!\s+(?:edition|play)\b)`.
        Word {
            pattern: rxi(r"\bextended\b"),
            not_followed_by: Some(rxi(r"^\s+(?:edition|play)\b")),
            name: "extended",
        },
        word(r"\b(?:re-?mix(?:es|ed)?|rmx)\b", "remix"),
        word(r"\b(?:sped|speed)\s*up\b", "sped up"),
        word(r"\bslowed\b", "slowed"),
        word(r"\bnightcore\b", "nightcore"),
        word(r"\binstrumentals?\b", "instrumental"),
        word(r"\b(?:a\s*cappellas?|acapellas?)\b", "acapella"),
        word(r"\bkaraoke\b", "karaoke"),
        word(r"\bdrumless\b", "drumless"),
        word(r"\bmash[\s-]?ups?\b", "mashup"),
        word(r"\bmedley\b", "medley"),
        word(r"\bmega[\s-]?mix\b", "megamix"),
        word(r"\b8d\s+audio\b", "8d"),
        word(
            r"\b(?:dj|continuous|non-?stop)\s+mix(?:es)?\b|\bmixed\s+by\b",
            "dj mix",
        ),
    ]
});

/// What a folder's brackets can say that is no version of the files in it: a generic
/// "(Japanese Version)" or "(Mix)", a label called Reprise, a "[+cover]" scan, a VIP pack.
/// The live words are LiveVersion's.
const NOT_FROM_A_FOLDER: [&str; 9] = [
    "mix",
    "edit",
    "version",
    "reprise",
    "cover",
    "vip",
    "live",
    "unplugged",
    "bootleg",
];

const LIVE_NAMES: [&str; 3] = ["live", "unplugged", "bootleg"];

/// An album edition, not a version of its songs.
static EDITION: LazyLock<Regex> = LazyLock::new(|| rxi(r"\bextended\s+(?:edition|play)\b"));

/// Requested versions a file may carry under another name and still be what was asked for:
/// a shortened cut is the same recording, and peers often name a single's radio edit plainly.
/// Every other requested version must show in the file's name or its folder.
const CUTS: [&str; 2] = ["radio edit", "edit"];

/// The words in a text, in the list's order. Matched in the view where `\b` falls by word
/// characters in one UTF-16 unit.
fn words_in(text: Option<&str>) -> Vec<&'static str> {
    let Some(text) = text.filter(|text| !is_blank(text)) else {
        return Vec::new();
    };
    let view = word_boundary_view(text);
    WORDS
        .iter()
        .filter(|word| {
            word.pattern.find_iter(&view).any(|found| {
                word.not_followed_by
                    .as_ref()
                    .is_none_or(|after| !after.is_match(&view[found.end()..]))
            })
        })
        .map(|word| word.name)
        .collect()
}

/// A title's versions that make it a different recording, as SongIdentity reads them.
fn distinct(title: &str) -> BTreeSet<String> {
    SongIdentity::distinct_versions(&SongIdentity::parse_title(title, None), None)
}

/// The versions a request asks for, by its title and its album.
pub fn requested(title: Option<&str>, album: Option<&str>) -> BTreeSet<String> {
    let mut wanted = BTreeSet::new();
    for text in [title, album].into_iter().flatten() {
        if is_blank(text) {
            continue;
        }
        wanted.extend(distinct(text));
        wanted.extend(words_in(Some(text)).into_iter().map(str::to_string));
    }
    wanted
}

/// `[\p{L}\p{N}]` on one UTF-16 code unit: a supplementary-plane character arrives as two
/// surrogates, neither a letter nor a number.
fn letter_or_number(c: char) -> bool {
    (c as u32) <= 0xFFFF
        && (is_letter(c)
            || matches!(
                get_general_category(c),
                GeneralCategory::DecimalNumber | GeneralCategory::LetterNumber | GeneralCategory::OtherNumber
            ))
}

/// Whether a match at this byte range stands alone: no letter or number right before or after.
fn stands_alone(text: &str, start: usize, end: usize) -> bool {
    !text[..start].chars().next_back().is_some_and(letter_or_number)
        && !text[end..].chars().next().is_some_and(letter_or_number)
}

/// Every match of the pattern that stands alone, left to right and never overlapping, as
/// `(?<![\p{L}\p{N}])...(?![\p{L}\p{N}])` would find them: a start where the bounds fail is
/// tried again one character on.
fn alone_matches(pattern: &Regex, text: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut at = 0;
    while at <= text.len() {
        let Some(candidate) = pattern.find_at(text, at) else {
            break;
        };
        if candidate.end() > candidate.start() && stands_alone(text, candidate.start(), candidate.end()) {
            found.push((candidate.start(), candidate.end()));
            at = candidate.end();
        } else {
            at = candidate.start() + text[candidate.start()..].chars().next().map_or(1, char::len_utf8);
        }
    }
    found
}

/// The text with the artist's own name taken out, so a band called Live, or an artist folder
/// named after one, is never read as a version.
pub fn without_artist(text: &str, artist: Option<&str>) -> String {
    let Some(artist) = artist.filter(|artist| !is_blank(artist)) else {
        return text.to_string();
    };
    if text.is_empty() {
        return text.to_string();
    }
    let mut names: Vec<String> = SongIdentity::parse_artists(artist).names;
    names.push(artist.trim().to_string());
    names.retain(|name| utf16_len(name.trim()) > 1);
    // OrderByDescending(name.Length): stable, longest first, by UTF-16 length.
    names.sort_by_key(|name| std::cmp::Reverse(utf16_len(name)));
    let mut text = text.to_string();
    for name in names {
        let pattern = rxi(&regex::escape(name.trim()));
        let found = alone_matches(&pattern, &text);
        if found.is_empty() {
            continue;
        }
        let mut replaced = String::with_capacity(text.len());
        let mut last = 0;
        for (start, end) in found {
            replaced.push_str(&text[last..start]);
            replaced.push(' ');
            last = end;
        }
        replaced.push_str(&text[last..]);
        text = replaced;
    }
    text
}

/// The versions one folder name says its files are, live words left out.
pub fn in_folder(folder: &str, artist: Option<&str>) -> BTreeSet<String> {
    let without = without_artist(folder, artist);
    let text = {
        let view = word_boundary_view(&without);
        let mut replaced = String::with_capacity(without.len());
        let mut last = 0;
        for found in EDITION.find_iter(&view) {
            replaced.push_str(&without[last..found.start()]);
            replaced.push(' ');
            last = found.end();
        }
        replaced.push_str(&without[last..]);
        replaced
    };
    let mut found: BTreeSet<String> = distinct(&text)
        .into_iter()
        .filter(|version| !NOT_FROM_A_FOLDER.contains(&version.as_str()))
        .collect();
    found.extend(words_in(Some(&text)).into_iter().map(str::to_string));
    found
}

/// The parts of a remote path, either separator, empty ones dropped.
fn path_parts(filename: &str) -> Vec<String> {
    filename
        .replace('\\', "/")
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// The album folder and the one above it (a disc folder sits between), never the share's top
/// level, as LiveVersion reads them.
pub fn album_folders(filename: &str) -> Vec<String> {
    let parts = path_parts(filename);
    if parts.len() <= 1 {
        return Vec::new();
    }
    let folders = &parts[..parts.len() - 1];
    folders[folders.len().saturating_sub(2)..].to_vec()
}

/// The file's own name without its extension.
fn leaf_title(filename: &str) -> String {
    let leaf = path_parts(filename).pop().unwrap_or_default();
    match leaf.rfind('.') {
        Some(dot) if dot > 0 => leaf[..dot].to_string(),
        _ => leaf,
    }
}

/// The versions a peer's file is, by its own name and its album folders, live words included.
/// Neutral markers (remaster, explicit, album version) are no version and are left out.
pub fn carried(filename: &str, artist: Option<&str>) -> BTreeSet<String> {
    let leaf = without_artist(&leaf_title(filename), artist);
    let mut carried = distinct(&leaf);
    carried.extend(words_in(Some(&leaf)).into_iter().map(str::to_string));
    for folder in album_folders(filename) {
        carried.extend(in_folder(&folder, artist));
        if live_version::mentions(Some(&without_artist(&folder, artist))) {
            carried.insert("live".to_string());
        }
    }
    carried
}

/// Versions the file's album folders say it is that the request never asked for: "Too Close"
/// from "Too Close (Radio Edit) - Single", "Angel" from "Mezzanine Remix Tapes '98". Live
/// folders are LiveVersion's.
pub fn unrequested_from_folder(
    filename: &str,
    title: Option<&str>,
    album: Option<&str>,
    artist: Option<&str>,
) -> BTreeSet<String> {
    let wanted = requested(title, album);
    album_folders(filename)
        .iter()
        .flat_map(|folder| in_folder(folder, artist))
        .filter(|version| !wanted.contains(version))
        .collect()
}

/// Versions the file's own name runs on without brackets that the title never asked for,
/// which SongIdentity cannot read: "Too Close-radio edit".
pub fn unrequested_in_name(filename: &str, title: Option<&str>, artist: Option<&str>) -> BTreeSet<String> {
    let wanted = requested(title, None);
    words_in(Some(&without_artist(&leaf_title(filename), artist)))
        .into_iter()
        .filter(|version| !wanted.contains(*version))
        .map(str::to_string)
        .collect()
}

/// Versions the request's title asks for that the file shows nowhere, its name or its album
/// folders. Used for ranking: the copy that says it is the asked-for version goes first.
pub fn missing(filename: &str, title: Option<&str>, artist: Option<&str>) -> BTreeSet<String> {
    let wanted = distinct(title.unwrap_or(""));
    if wanted.is_empty() {
        return wanted;
    }
    let carried = carried(filename, artist);
    wanted
        .into_iter()
        .filter(|version| {
            !carried.contains(version)
                && !(LIVE_NAMES.contains(&version.as_str()) && carried.contains("live"))
        })
        .collect()
}

/// Whether the request asked for a version the file does not show, other than a shortened
/// cut: "Song (Remix)", "(Acoustic)", "(Sped Up)" or "(Clean)" against a plainly named file
/// from a plain folder, which is the original. A radio edit may be named plainly; the length
/// check tells it from the album cut.
pub fn lacks_requested(filename: &str, title: Option<&str>, artist: Option<&str>) -> bool {
    missing(filename, title, artist)
        .iter()
        .any(|version| !CUTS.contains(&version.as_str()))
}

// ---- A shortened cut named plainly, and songs joined into one ----------------------------

/// Whether the request asks for a shortened cut ("(Radio Edit)") the file does not name, and
/// the file's name is more than the song's title. A plainly named file is taken for the cut
/// only when it is named as just the song: "01. Get Lucky", "Daft Punk - Get Lucky (feat.
/// Pharrell Williams)". "Get Lucky x Alakazam!" of the same length is another record.
pub fn cut_not_plain(filename: &str, title: Option<&str>, artist: Option<&str>) -> bool {
    missing(filename, title, artist)
        .iter()
        .any(|version| CUTS.contains(&version.as_str()))
        && !just_the_title(filename, title, artist)
}

/// A format or quality a peer puts beside a song's name: "[FLAC]", "(16-44)", "320".
static FORMAT_TAG: LazyLock<Regex> =
    LazyLock::new(|| rxi(r"^(?:flac|mp3|wav|alac|aiff?|ape|lossless|hires|web|cd|vinyl|bit|khz|kbps|\d+)+$"));

static NAME_PARTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+[-\u{2013}]\s+").expect("a fixed pattern"));

static TRACK_NUMBER_ONLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{1,3}\.?$").expect("a fixed pattern"));

/// Whether the file's own name is the song's title and nothing else: a track number, the
/// artist (with a guest credit), a guest credit, a year or a format tag may stand beside it,
/// split by the usual " - ".
pub fn just_the_title(filename: &str, title: Option<&str>, artist: Option<&str>) -> bool {
    let wanted = SongIdentity::key(&SongIdentity::parse_title(title.unwrap_or(""), None).core);
    if wanted.is_empty() {
        return true;
    }
    let artists: HashSet<String> = match artist.filter(|artist| !is_blank(artist)) {
        None => HashSet::new(),
        Some(artist) => SongIdentity::parse_artists(artist)
            .names
            .iter()
            .map(String::as_str)
            .chain([artist])
            .map(SongIdentity::key)
            .filter(|key| !key.is_empty())
            .collect(),
    };
    let folded = SongIdentity::fold(&leaf_title(filename));
    let parts: Vec<&str> = NAME_PARTS
        .split(&folded)
        .filter(|part| !SongIdentity::key(part).is_empty())
        .collect();
    let mut titled = false;
    for part in parts {
        let read = SongIdentity::parse_title(part, None);
        if !titled
            && SongIdentity::key(&read.core) == wanted
            && read
                .extras
                .iter()
                .all(|extra| FORMAT_TAG.is_match(&utf16_class_view(extra)))
        {
            titled = true;
            continue;
        }
        if TRACK_NUMBER_ONLY.is_match(&utf16_class_view(part.trim())) {
            continue;
        }
        if artists.contains(&SongIdentity::key(&SongIdentity::strip_features(part))) {
            continue;
        }
        return false;
    }
    titled
}

/// The versions that put two songs or more in one file.
const JOINED: [&str; 4] = ["mashup", "medley", "megamix", "blend"];

/// A word that joins another song onto this one's title: "Get Lucky x Alakazam!",
/// "Get Lucky vs Billie Jean".
static JOIN_WORD: LazyLock<Regex> = LazyLock::new(|| rxi(r"^\s*(?:x|vs\.?|versus)\s+(\S.*)$"));

/// A song run straight on after the title: "Get Lucky-Billie Jean". (`^-(?=\S)(.+)$`.)
static RUN_ON: LazyLock<Regex> = LazyLock::new(|| rxi(r"^-(\S.*)$"));

/// Whether a joining word stands alone anywhere in the text, as
/// `(?<![\p{L}\p{N}])(?:x|vs\.?|versus)(?![\p{L}\p{N}])` (case-sensitive) reads it: at each
/// start, the alternatives in the order a backtracking engine tries them.
fn has_join_word(text: &str) -> bool {
    text.char_indices().any(|(start, _)| {
        if text[..start].chars().next_back().is_some_and(letter_or_number) {
            return false;
        }
        let rest = &text[start..];
        ["x", "vs.", "vs", "versus"].iter().any(|word| {
            rest.starts_with(word) && !rest[word.len()..].chars().next().is_some_and(letter_or_number)
        })
    })
}

/// Whether the file is a mash-up, medley, megamix or blend of the song with others, which the
/// request did not ask for. Read from the words after the title, never before it, where " x "
/// and "vs" are as likely an artist's credit ("Skrillex x Diplo - ..."), and never when the
/// title itself has the word.
pub fn mash_up(filename: &str, title: Option<&str>, artist: Option<&str>) -> bool {
    let Some(title) = title.filter(|title| !is_blank(title)) else {
        return false;
    };
    let wanted = requested(Some(title), None);
    if carried(filename, artist)
        .iter()
        .any(|version| JOINED.contains(&version.as_str()) && !wanted.contains(version))
    {
        return true;
    }

    let name = SongIdentity::plain(&name_title(title)).trim().to_string();
    if name.is_empty() || has_join_word(&name) {
        return false;
    }
    let leaf = SongIdentity::plain(&without_artist(&leaf_title(filename), artist));
    let pattern = Regex::new(&regex::escape(&name)).expect("an escaped name compiles");
    let Some(&(_, end)) = alone_matches(&pattern, &leaf).first() else {
        return false;
    };
    let mut tail = &leaf[end..];
    if let Some(bracket) = tail.find(['(', '[', '{']) {
        tail = &tail[..bracket];
    }
    let tail = tail.trim_end();

    // Whoever the title credits may follow it: "Get Lucky x Pharrell Williams" is not two songs.
    let mut credited: HashSet<String> = SongIdentity::parse_title(title, None)
        .featured
        .iter()
        .map(|name| SongIdentity::key(name))
        .collect();
    if let Some(artist) = artist.filter(|artist| !is_blank(artist)) {
        credited.extend(
            SongIdentity::parse_artists(artist)
                .names
                .iter()
                .map(|name| SongIdentity::key(name)),
        );
    }
    let another_song = |words: &str| {
        let read = SongIdentity::parse_title(&format!("x ({})", words.trim()), None);
        read.versions.is_empty()
            && read.featured.is_empty()
            && !read.extras.is_empty()
            && !credited.contains(&SongIdentity::key(words))
    };
    if let Some(joined) = JOIN_WORD.captures(tail) {
        return another_song(&joined[1]);
    }
    RUN_ON
        .captures(tail)
        .is_some_and(|run_on| another_song(&run_on[1]))
}

#[cfg(test)]
mod name_title_tests {
    use super::name_title;

    #[test]
    fn the_name_title_drops_only_guests_and_version_tags() {
        for (title, expected) in [
            ("Too Close (Radio Edit)", "Too Close"),
            ("Too Close - Radio Edit", "Too Close"),
            ("Teardrop - Remastered 2011", "Teardrop"),
            ("Take Care (feat. Rihanna)", "Take Care"),
            ("Blue (Da Ba Dee)", "Blue (Da Ba Dee)"),
            ("Song (Pt. 2)", "Song (Pt. 2)"),
            ("(Exchange)", "(Exchange)"),
        ] {
            assert_eq!(name_title(title), expected, "{title}");
        }
    }

    #[test]
    fn a_guest_credit_inside_a_version_tag_leaves_the_bare_title() {
        for title in [
            "Get Lucky (Radio Edit - feat. Pharrell Williams and Nile Rodgers)",
            "Get Lucky [Radio Edit - feat. Pharrell Williams and Nile Rodgers]",
            "Get Lucky (Radio Edit, feat. Pharrell Williams & Nile Rodgers)",
            "Get Lucky - Radio Edit - feat. Pharrell Williams and Nile Rodgers",
            "Get Lucky (feat. Pharrell Williams and Nile Rodgers) - Radio Edit",
            "Get Lucky feat. Pharrell Williams and Nile Rodgers (Radio Edit)",
        ] {
            assert_eq!(name_title(title), "Get Lucky", "{title}");
        }
    }
}
