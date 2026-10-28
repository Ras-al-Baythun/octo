//! Mixes YouTube Music's song search into the outside rows of a search.
//!
//! Last.fm's track search (or Deezer's, when Last.fm is late) and YouTube Music's song search
//! each rank the words their own way. Neither is trusted over the other: each row scores
//! `1 / (K + rank)` in every list it is in (reciprocal rank fusion), so a song both find rises,
//! and a song only one finds still places by its rank there. YouTube Music finds what the other
//! two miss, such as a name written in another script ("svetlana" for "СВЕТЛАНА!"), but only its
//! releases' own audio tracks join, never music videos or uploads, and at most
//! [`YOU_TUBE_MUSIC_ONLY_MAX`] of them. Versions nobody asked for (slowed, live, a remix) sink.
//!
//! YouTube's videos are a list of their own ([`video_rows`]): an ambience, a mix or a fan upload
//! no catalog lists, found by its title, and a link pasted into the search
//! ([`you_tube_video_id`]).

use std::collections::HashMap;

use octo_core::common::SongIdentity;
use octo_core::common::dotnet::ordinal_ignore_case_key;
use octo_core::outside::SongLength;

use crate::services::you_tube::you_tube_music_client::{PODCAST_EPISODE, RELEASE, YtmRow};

/// The rank damping of reciprocal rank fusion: the usual 60, so the first rows of a list do
/// not drown the other list.
const K: f64 = 60.0;

/// At most this many songs only YouTube Music found.
pub const YOU_TUBE_MUSIC_ONLY_MAX: usize = 10;

/// `video_type` of an official music video.
const OFFICIAL_VIDEO: &str = "MUSIC_VIDEO_TYPE_OMV";

/// At most this many YouTube videos follow the songs of a search.
pub const YOU_TUBE_VIDEOS_WITH_SONGS: usize = 5;

/// At most this many YouTube videos make a search for videos alone.
pub const YOU_TUBE_VIDEOS_ALONE: usize = 20;

/// The share of a query's words a video's title and channel must have between them: two of
/// three, so a word misspelt ("ba sing sai") still finds it, and a loose match does not.
const VIDEO_WORDS_NEEDED: f64 = 2.0 / 3.0;

/// What a version nobody asked for keeps of its score.
const OTHER_VERSION_FACTOR: f64 = 0.5;

/// Words in a title that make a version of a song rather than the song, unless the query has
/// them too.
const VERSION_WORDS: &[&str] = &[
    "slowed",
    "sped up",
    "speed up",
    "nightcore",
    "reverb",
    "8d",
    "live",
    "remix",
    "karaoke",
    "instrumental",
    "cover",
    "acoustic",
];

/// One outside row of a search: its artist and title, and when YouTube Music has it, the
/// video that plays it and that video's length.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusedRow {
    pub artist: String,
    pub title: String,
    pub video: Option<String>,
    pub length: Option<i32>,
    /// The cover YouTube Music shows for it, for a row only YouTube Music found.
    pub thumbnail: Option<String>,
}

struct Candidate {
    row: FusedRow,
    score: f64,
    /// Where it first appeared, the tie breaker: earlier lists, then earlier rows, first.
    first_seen: usize,
    from_rows: bool,
}

/// The rows of both searches as one list, best first, at most `max`.
///
/// `rows` are Last.fm's (or Deezer's) in their order; `ytm` YouTube Music's song search in its
/// order. A song in both keeps the artist and title of `rows` (the names its metadata and cover
/// are found by) and takes YouTube Music's video and length.
pub fn fuse(query: &str, rows: &[(String, String)], ytm: &[YtmRow], max: usize) -> Vec<FusedRow> {
    let asked = query.to_lowercase();
    let mut by_key: HashMap<String, usize> = HashMap::new();
    let mut candidates: Vec<Candidate> = Vec::new();

    for (rank, (artist, title)) in rows.iter().enumerate() {
        let key = ordinal_ignore_case_key(&SongIdentity::match_key(artist, title));
        if by_key.contains_key(&key) {
            continue;
        }
        by_key.insert(key, candidates.len());
        candidates.push(Candidate {
            row: FusedRow {
                artist: artist.clone(),
                title: title.clone(),
                video: None,
                length: None,
                thumbnail: None,
            },
            score: score(rank),
            first_seen: candidates.len(),
            from_rows: true,
        });
    }

    let mut ytm_rank = 0;
    for row in ytm {
        if row.artists.is_empty() || row.title.trim().is_empty() {
            continue;
        }
        let release = row.video_type.as_deref() == Some(RELEASE);
        let length = SongLength::sane_video_length(row.duration_seconds);
        let artist = row.artists.join(", ");
        let key = ordinal_ignore_case_key(&SongIdentity::match_key(&artist, &row.title));
        let rank = ytm_rank;
        ytm_rank += 1;
        match by_key.get(&key) {
            Some(&at) => {
                let found = &mut candidates[at];
                if !found.from_rows {
                    // YouTube Music listing one song twice: its first row stands.
                    continue;
                }
                found.score += score(rank);
                // Only a release's own audio, of a sane length, plays the song as the other
                // list names it; a music video can carry a skit, so it is left to the usual
                // lookup on play.
                if release && length.is_some() && found.row.video.is_none() {
                    found.row.video = Some(row.video_id.clone());
                    found.row.length = length;
                }
            }
            None => {
                if !release || length.is_none() {
                    continue;
                }
                by_key.insert(key, candidates.len());
                candidates.push(Candidate {
                    row: FusedRow {
                        artist,
                        title: row.title.clone(),
                        video: Some(row.video_id.clone()),
                        length,
                        thumbnail: row.thumbnail.clone(),
                    },
                    score: score(rank),
                    first_seen: candidates.len(),
                    from_rows: false,
                });
            }
        }
    }

    for candidate in &mut candidates {
        if is_other_version(&candidate.row.title, &asked) {
            candidate.score *= OTHER_VERSION_FACTOR;
        }
    }
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.first_seen.cmp(&b.first_seen))
    });

    let mut only_ytm = 0;
    candidates
        .into_iter()
        .filter(|c| {
            if c.from_rows {
                return true;
            }
            only_ytm += 1;
            only_ytm <= YOU_TUBE_MUSIC_ONLY_MAX
        })
        .take(max)
        .map(|c| c.row)
        .collect()
}

/// YouTube's videos for `query` as rows, in YouTube's order, at most `max`: only those whose
/// title and channel have most of the query's words ([`VIDEO_WORDS_NEEDED`]), since a video
/// search answers something for any words. Each plays its own video, its channel as the
/// artist, with its length when it is one to play through (up to four hours, see
/// [`SongLength::sane_long_video_length`]); podcast episodes never. With `uploads_only` (videos
/// after the songs of a search), a release's audio and an official music video are left out too:
/// the songs already have them.
pub fn video_rows(query: &str, videos: &[YtmRow], max: usize, uploads_only: bool) -> Vec<FusedRow> {
    let asked = words(query);
    let needed = (asked.len() as f64 * VIDEO_WORDS_NEEDED).ceil() as usize;
    let mut seen = std::collections::HashSet::new();
    videos
        .iter()
        .filter(|row| row.video_type.as_deref() != Some(PODCAST_EPISODE))
        .filter(|row| !uploads_only || !matches!(row.video_type.as_deref(), Some(RELEASE | OFFICIAL_VIDEO)))
        .filter(|row| {
            let has = words(&format!("{} {}", row.title, row.artists.join(" ")));
            asked
                .iter()
                .filter(|word| has.iter().any(|h| h == *word || h.starts_with(word.as_str())))
                .count()
                >= needed.max(1)
        })
        .filter(|row| seen.insert(row.video_id.clone()))
        .take(max)
        .map(video_row)
        .collect()
}

/// A video as a row as it is: for a link pasted into the search, which asks for this video.
pub fn video_row(row: &YtmRow) -> FusedRow {
    let artist = row.artists.join(", ");
    FusedRow {
        artist: if artist.trim().is_empty() { "YouTube".to_string() } else { artist },
        title: row.title.clone(),
        video: Some(row.video_id.clone()),
        length: SongLength::sane_long_video_length(row.duration_seconds),
        thumbnail: row.thumbnail.clone(),
    }
}

/// The lowercase words of `text`, split at everything that is not a letter or a digit.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// The video a YouTube link points at ("youtube.com/watch?v=", "youtu.be/", YouTube Music's,
/// a short's, a live's or an embed's), when the whole query is one; else None.
pub fn you_tube_video_id(query: &str) -> Option<String> {
    let text = query.trim();
    if text.contains(char::is_whitespace) {
        return None;
    }
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))
        .unwrap_or(text);
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let candidate = match host {
        "youtu.be" => path.split(['?', '&', '#', '/']).next(),
        "youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            let (route, params) = path.split_once('?').unwrap_or((path, ""));
            if route.trim_end_matches('/') == "watch" {
                params
                    .split(['&', '#'])
                    .find_map(|pair| pair.strip_prefix("v="))
            } else {
                ["shorts/", "live/", "embed/"]
                    .iter()
                    .find_map(|prefix| route.strip_prefix(prefix))
                    .and_then(|id| id.split(['/', '#']).next())
            }
        }
        _ => None,
    }?;
    let is_id = candidate.len() == 11
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    is_id.then(|| candidate.to_string())
}

fn score(rank: usize) -> f64 {
    1.0 / (K + rank as f64 + 1.0)
}

/// Whether `title` is a version of a song (slowed, live, a remix...) the query did not ask for.
fn is_other_version(title: &str, asked: &str) -> bool {
    let title = title.to_lowercase();
    VERSION_WORDS
        .iter()
        .any(|word| has_word(&title, word) && !has_word(asked, word))
}

/// Whether `text` has `word` as a whole word (or words), not inside another one: "live" is in
/// "Live at Wembley" but not in "Oliver" or "Alive".
fn has_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + word.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(a, t)| (a.to_string(), t.to_string())).collect()
    }

    fn ytm(id: &str, artist: &str, title: &str, kind: &str) -> YtmRow {
        YtmRow {
            video_id: id.into(),
            title: title.into(),
            artists: vec![artist.into()],
            album: None,
            duration_seconds: Some(200),
            video_type: Some(kind.into()),
            thumbnail: Some(format!("https://img/{id}")),
        }
    }

    const OMV: &str = "MUSIC_VIDEO_TYPE_OMV";

    fn names(fused: &[FusedRow]) -> Vec<String> {
        fused
            .iter()
            .map(|r| format!("{} - {}", r.artist, r.title))
            .collect()
    }

    #[test]
    fn a_song_both_find_rises_and_plays_youtube_musics_video() {
        let fused = fuse(
            "love",
            &rows(&[("A", "One"), ("B", "Two"), ("C", "Three")]),
            &[ytm("v3", "C", "Three", RELEASE), ytm("vx", "X", "Other", RELEASE)],
            60,
        );
        assert_eq!(names(&fused)[0], "C - Three");
        assert_eq!(fused[0].video.as_deref(), Some("v3"));
        assert_eq!(fused[0].length, Some(200));
        // A song in both keeps the other list's names and no thumbnail: its cover is found by them.
        assert_eq!(fused[0].thumbnail, None);
        assert!(names(&fused).contains(&"X - Other".to_string()));
        assert_eq!(fused.iter().find(|r| r.artist == "A").unwrap().video, None);
    }

    #[test]
    fn neither_list_is_trusted_over_the_other() {
        let fused = fuse(
            "q",
            &rows(&[("A", "One"), ("B", "Two")]),
            &[
                ytm("y1", "Y", "First", RELEASE),
                ytm("y2", "Z", "Second", RELEASE),
            ],
            60,
        );
        // Equal ranks score alike; the tie goes to the order they came in.
        assert_eq!(names(&fused), ["A - One", "Y - First", "B - Two", "Z - Second"]);
    }

    #[test]
    fn a_song_only_youtube_music_finds_joins_with_its_names_and_cover() {
        let fused = fuse(
            "svetlana",
            &rows(&[("Svetlana Loboda", "Moloko")]),
            &[ytm("sv", "NEXTIME", "СВЕТЛАНА!", RELEASE)],
            60,
        );
        let found = fused.iter().find(|r| r.title == "СВЕТЛАНА!").expect("joined");
        assert_eq!(found.artist, "NEXTIME");
        assert_eq!(found.video.as_deref(), Some("sv"));
        assert_eq!(found.thumbnail.as_deref(), Some("https://img/sv"));
    }

    #[test]
    fn music_videos_and_uploads_never_join_on_their_own() {
        let fused = fuse(
            "q",
            &rows(&[("A", "One")]),
            &[
                ytm("o", "M", "Video", OMV),
                ytm("u", "U", "Upload", "MUSIC_VIDEO_TYPE_UGC"),
                YtmRow {
                    video_type: None,
                    ..ytm("n", "N", "Unknown", RELEASE)
                },
            ],
            60,
        );
        assert_eq!(names(&fused), ["A - One"]);
    }

    #[test]
    fn a_music_video_of_a_listed_song_lifts_it_but_does_not_play_it() {
        let fused = fuse(
            "q",
            &rows(&[("A", "One"), ("B", "Two")]),
            &[ytm("o", "B", "Two", OMV)],
            60,
        );
        assert_eq!(names(&fused)[0], "B - Two");
        assert_eq!(fused[0].video, None);
    }

    #[test]
    fn at_most_ten_songs_only_youtube_music_found() {
        let only: Vec<YtmRow> = (0..15)
            .map(|i| {
                ytm(
                    &format!("v{i}"),
                    &format!("Artist {i}"),
                    &format!("Song {i}"),
                    RELEASE,
                )
            })
            .collect();
        let fused = fuse("q", &rows(&[("A", "One")]), &only, 60);
        assert_eq!(fused.len(), 1 + YOU_TUBE_MUSIC_ONLY_MAX);
        assert!(fused.iter().any(|r| r.artist == "A"));
    }

    #[test]
    fn versions_nobody_asked_for_sink_unless_asked_for() {
        let list = rows(&[
            ("Tom Odell", "Another Love (Slowed)"),
            ("Tom Odell", "Another Love"),
        ]);
        let plain = fuse("tom odell another love", &list, &[], 60);
        assert_eq!(plain[0].title, "Another Love");
        let asked = fuse("another love slowed", &list, &[], 60);
        assert_eq!(asked[0].title, "Another Love (Slowed)");
    }

    #[test]
    fn a_version_word_counts_only_as_a_whole_word() {
        assert!(is_other_version("Song (Live at Wembley)", "song"));
        assert!(!is_other_version("Oliver", "oliver"));
        assert!(!is_other_version("Alive", "alive"));
        assert!(is_other_version("Song - Sped Up", "song"));
        assert!(!is_other_version("Song - Sped Up", "song sped up"));
    }

    fn video(id: &str, channel: &str, title: &str, seconds: i32) -> YtmRow {
        YtmRow {
            duration_seconds: Some(seconds),
            ..ytm(id, channel, title, "MUSIC_VIDEO_TYPE_UGC")
        }
    }

    #[test]
    fn a_video_whose_title_has_the_words_joins_with_its_hour_long_length() {
        let videos = [
            video(
                "kJjDhKOZPQs",
                "Ambience Academy",
                "Ba Sing Se | Bustling City Ambience: Emotional Avatar - The Last Airbender Music",
                4512,
            ),
            video("other", "Someone", "Rainy Tokyo Night", 3600),
            video("day", "Loops", "Ba Sing Se ambience 24 hours", 24 * 3600),
        ];
        let rows = video_rows("ba sing se ambience", &videos, 5, true);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].artist, "Ambience Academy");
        assert_eq!(rows[0].video.as_deref(), Some("kJjDhKOZPQs"));
        assert_eq!(rows[0].length, Some(4512));
        // A day-long loop is shown without a length rather than left out.
        assert_eq!(rows[1].length, None);
        // A word misspelt still finds it; most words missing does not.
        assert_eq!(video_rows("ba sing sai", &videos, 5, true).len(), 2);
        assert!(video_rows("tokyo ambience jazz cafe", &videos, 5, true).is_empty());
    }

    #[test]
    fn videos_stop_at_max_each_once_and_never_a_podcast() {
        let videos = [
            video("a", "C", "Lofi beats 1", 3600),
            video("a", "C", "Lofi beats 1", 3600),
            video("b", "C", "Lofi beats 2", 3600),
            YtmRow {
                video_type: Some(PODCAST_EPISODE.into()),
                ..video("p", "C", "Lofi beats talk", 3600)
            },
            video("c", "C", "Lofi beats 3", 3600),
        ];
        let ids: Vec<_> = video_rows("lofi beats", &videos, 2, false)
            .into_iter()
            .filter_map(|r| r.video)
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn after_songs_only_uploads_join_and_alone_every_video_may() {
        let videos = [
            ytm("omv", "Tom Odell", "Another Love (Official Video)", "MUSIC_VIDEO_TYPE_OMV"),
            ytm("atv", "Tom Odell", "Another Love", RELEASE),
            ytm("ugc", "Fan", "Another Love piano cover", "MUSIC_VIDEO_TYPE_UGC"),
        ];
        let ids = |rows: Vec<FusedRow>| rows.into_iter().filter_map(|r| r.video).collect::<Vec<_>>();
        assert_eq!(ids(video_rows("another love", &videos, 5, true)), ["ugc"]);
        assert_eq!(ids(video_rows("another love", &videos, 5, false)), ["omv", "atv", "ugc"]);
    }

    #[test]
    fn a_pasted_link_names_its_video() {
        let id = Some("kJjDhKOZPQs".to_string());
        assert_eq!(
            you_tube_video_id("https://www.youtube.com/watch?v=kJjDhKOZPQs&list=RDkJjDhKOZPQs&start_radio=1"),
            id
        );
        assert_eq!(you_tube_video_id("youtu.be/kJjDhKOZPQs?t=42"), id);
        assert_eq!(you_tube_video_id(" https://music.youtube.com/watch?v=kJjDhKOZPQs "), id);
        assert_eq!(you_tube_video_id("https://m.youtube.com/watch?feature=share&v=kJjDhKOZPQs"), id);
        assert_eq!(you_tube_video_id("https://youtube.com/shorts/kJjDhKOZPQs"), id);
        assert_eq!(you_tube_video_id("ba sing se"), None);
        assert_eq!(you_tube_video_id("https://example.com/watch?v=kJjDhKOZPQs"), None);
        assert_eq!(you_tube_video_id("https://www.youtube.com/watch?v=short"), None);
    }

    #[test]
    fn the_list_stops_at_max() {
        let list = rows(&[("A", "1"), ("B", "2"), ("C", "3")]);
        assert_eq!(fuse("q", &list, &[], 2).len(), 2);
    }
}
