//! Mixes YouTube Music's song search into the outside rows of a search.
//!
//! Last.fm's track search (or Deezer's, when Last.fm is late) and YouTube Music's song search
//! each rank the words their own way. Neither is trusted over the other: each row scores
//! `1 / (K + rank)` in every list it is in (reciprocal rank fusion), so a song both find rises,
//! and a song only one finds still places by its rank there. YouTube Music finds what the other
//! two miss, such as a name written in another script ("svetlana" for "СВЕТЛАНА!"), but only its
//! releases' own audio tracks join, never music videos or uploads, and at most
//! [`YOU_TUBE_MUSIC_ONLY_MAX`] of them. Versions nobody asked for (slowed, live, a remix) sink.

use std::collections::HashMap;

use octo_core::common::SongIdentity;
use octo_core::common::dotnet::ordinal_ignore_case_key;
use octo_core::outside::SongLength;

use crate::services::you_tube::you_tube_music_client::{RELEASE, YtmRow};

/// The rank damping of reciprocal rank fusion: the usual 60, so the first rows of a list do
/// not drown the other list.
const K: f64 = 60.0;

/// At most this many songs only YouTube Music found.
pub const YOU_TUBE_MUSIC_ONLY_MAX: usize = 10;

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

    #[test]
    fn the_list_stops_at_max() {
        let list = rows(&[("A", "1"), ("B", "2"), ("C", "3")]);
        assert_eq!(fuse("q", &list, &[], 2).len(), 2);
    }
}
