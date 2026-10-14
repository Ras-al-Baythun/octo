//! `GenreSettings` and `GenreMappingSettings` (the `Genre` section).

use serde::{Deserialize, Serialize};

use super::last_fm::DiscoveryStationSettings;
use super::text::{IgnoreCaseSet, lower_invariant, utf16_len};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum GenreFallbackSource {
    #[default]
    None,
    LastFm,
    MusicBrainz,
}

/// What to write when everything normalizes away to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum GenreEmptyBehavior {
    #[default]
    Leave,
    Clear,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, Serialize)]
pub enum GenreMatchMode {
    #[default]
    Contains,
    Exact,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct GenreMappingSettings {
    pub id: String,
    pub pattern: String,
    pub genre: String,
    #[serde(rename = "Match")]
    pub match_mode: GenreMatchMode,
    pub enabled: bool,
}

impl Default for GenreMappingSettings {
    fn default() -> Self {
        Self {
            id: String::new(),
            pattern: String::new(),
            genre: String::new(),
            match_mode: GenreMatchMode::Contains,
            enabled: true,
        }
    }
}

/// Collapses the genres downloads arrive with into a list a library can browse.
///
/// A top-level section rather than Metadata:Genre because the admin JS splits a control's
/// name on "." into exactly two segments, so only one nesting level works.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct GenreSettings {
    /// Normalize genres on every download. Off by default: this rewrites a tag the user may
    /// have curated.
    /// Environment variable: GENRE_NORMALIZE
    pub enabled: bool,

    /// Pattern to genre, APPLIED IN ORDER, first match wins. Not exposed as an env var:
    /// encoding structured rows in .env is brittle, the same call the pinned radio stations
    /// already make. Edit in the dashboard or settings.json.
    pub mappings: Vec<GenreMappingSettings>,

    /// Values that are not genres at all, dropped before mapping. Added to the built-in list,
    /// never replacing it.
    /// Environment variable: GENRE_BLOCKLIST (comma separated)
    ///
    /// (Nothing maps GENRE_BLOCKLIST or splits it on commas: the variable is named here but
    /// never read.)
    pub blocklist: Vec<String>,

    /// Where to look when a file has no usable genre left.
    /// Environment variable: GENRE_FALLBACK
    pub fallback: GenreFallbackSource,

    /// How many genres a track may keep.
    ///
    /// Defaults to the ceiling, which means "keep what is there". A low default would make
    /// simply switching normalization on destructive: with no mapping table at all, a track
    /// tagged "Cloud Rap, Emo, Hip Hop, Trap" would silently become "Cloud Rap". Dropping junk,
    /// years and duplicates is what the feature is for; throwing away accurate genres is not,
    /// and nobody asked for it as a default.
    ///
    /// Set it to 1 if you want one broad genre per track, which is what makes browsing by genre
    /// useful on a library whose tags are a mess.
    /// Environment variable: GENRE_MAX
    pub max_genres: i32,

    /// What to write when everything normalizes away to nothing.
    ///
    /// Clear is the default when the feature is on, and it is the whole point: genre was only
    /// ever written when non-empty and never cleared, so a file that arrived tagged
    /// "People & Blogs" kept it forever. A normalizer that cannot delete cannot fix that.
    /// Environment variable: GENRE_ON_EMPTY
    pub on_empty: GenreEmptyBehavior,

    /// Literal written when OnEmpty is Unknown.
    /// Environment variable: GENRE_UNKNOWN_LABEL
    pub unknown_label: String,

    /// How many files in a row may fail to be written before a backfill gives up. A read-only
    /// mount should produce one clear failure, not one per file. 0 never gives up.
    /// Environment variable: GENRE_BACKFILL_MAX_FAILURES
    pub backfill_max_consecutive_failures: i32,

    /// Extensions a whole-library backfill walks. Editable because which containers a library
    /// holds is the user's business, not Octo's.
    pub backfill_extensions: Vec<String>,
}

impl Default for GenreSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mappings: Vec::new(),
            blocklist: Vec::new(),
            fallback: GenreFallbackSource::None,
            max_genres: 10,
            on_empty: GenreEmptyBehavior::Clear,
            unknown_label: "Unknown".to_string(),
            backfill_max_consecutive_failures: 25,
            backfill_extensions: Vec::new(),
        }
    }
}

const DEFAULT_BACKFILL_EXTENSIONS: [&str; 9] = [
    ".flac", ".mp3", ".m4a", ".ogg", ".opus", ".wav", ".aiff", ".aif", ".wma",
];

/// Values that are never a genre regardless of settings: YouTube's category list, the
/// container words a downloader invents, and the format tags peers add. A user blocklist
/// ADDS to this and cannot remove from it, because "Music" is not a genre in any
/// configuration.
pub const BUILT_IN_BLOCKLIST: [&str; 33] = [
    "music",
    "people & blogs",
    "people and blogs",
    "gaming",
    "entertainment",
    "education",
    "news & politics",
    "science & technology",
    "howto & style",
    "film & animation",
    "autos & vehicles",
    "pets & animals",
    "sports",
    "travel & events",
    "comedy",
    "nonprofits & activism",
    "shows",
    "trailers",
    "unknown",
    "other",
    "misc",
    "miscellaneous",
    "genre",
    "audio",
    "soundtrack music",
    "youtube",
    "lossless",
    "flac",
    "mp3",
    "320kbps",
    "cd",
    "vinyl",
    "album",
];

impl GenreSettings {
    /// 0 means never give up, so it is not clamped upward.
    pub fn effective_backfill_max_consecutive_failures(&self) -> i32 {
        if self.backfill_max_consecutive_failures <= 0 {
            0
        } else {
            self.backfill_max_consecutive_failures.clamp(1, 10_000)
        }
    }

    /// Normalized to a leading dot and lowercase, so a user typing "FLAC" works.
    pub fn effective_backfill_extensions(&self) -> IgnoreCaseSet {
        let configured: Vec<String> = self
            .backfill_extensions
            .iter()
            .map(|entry| lower_invariant(entry.trim()))
            .filter(|entry| (1..=10).contains(&utf16_len(entry)))
            .map(|entry| {
                if entry.starts_with('.') {
                    entry
                } else {
                    format!(".{entry}")
                }
            })
            .collect();

        if configured.is_empty() {
            DEFAULT_BACKFILL_EXTENSIONS.iter().copied().collect()
        } else {
            configured.into_iter().collect()
        }
    }

    // Bounds are computed at READ time, hand-written, no data annotations: the house pattern.
    // A value that arrived from a hand-edited settings.json is sanitized where it is used.
    pub fn effective_max_genres(&self) -> i32 {
        self.max_genres.clamp(1, 10)
    }

    pub fn effective_unknown_label(&self) -> String {
        let label = self.unknown_label.trim();
        if (1..=60).contains(&utf16_len(label)) {
            label.to_string()
        } else {
            "Unknown".to_string()
        }
    }

    pub fn effective_blocklist(&self) -> IgnoreCaseSet {
        let mut set: IgnoreCaseSet = BUILT_IN_BLOCKLIST.iter().copied().collect();
        for entry in self.blocklist.iter().take(500) {
            let value = DiscoveryStationSettings::normalize_tag(entry);
            if (1..=60).contains(&utf16_len(&value)) {
                set.insert(value);
            }
        }
        set
    }

    /// Rows in the order the user put them, sanitized. Order is meaning here, so a duplicate
    /// pattern keeps the FIRST occurrence: the later one could never fire anyway.
    pub fn effective_mappings(&self) -> Vec<GenreMappingSettings> {
        let mut seen_patterns = IgnoreCaseSet::new();
        let mut seen_ids = IgnoreCaseSet::new();
        let mut result = Vec::new();

        for source in self.mappings.iter().take(200) {
            let pattern = DiscoveryStationSettings::normalize_tag(&source.pattern);
            let pattern_len = utf16_len(&pattern);
            if pattern_len == 0 || pattern_len > 60 || !seen_patterns.insert(pattern.clone()) {
                continue;
            }

            // Genre is kept VERBATIM apart from trimming: the user typed "Hip-Hop" and that
            // exact spelling is what lands in the file. Normalizing it here would write
            // "hip-hop" into a genre browser.
            let genre = source.genre.trim().to_string();
            if utf16_len(&genre) > 60 {
                continue;
            }

            let mut id = DiscoveryStationSettings::normalize_id(&source.id);
            if id.is_empty() {
                id = DiscoveryStationSettings::deterministic_id(&pattern, &[genre.as_str()]);
            }
            if !seen_ids.insert(id.clone()) {
                continue;
            }

            result.push(GenreMappingSettings {
                id,
                pattern,
                genre,
                match_mode: source.match_mode,
                enabled: source.enabled,
            });
        }
        result
    }

    /// The broad-genre collapse, ordered MOST SPECIFIC FIRST because matching stops at the
    /// first hit: "trap latino" has to be seen before "trap", or Latin music becomes Hip-Hop.
    ///
    /// Lives here and only here. The dashboard fetches it from /api/admin/genre/presets rather
    /// than keeping a second copy in admin.js that would drift.
    pub fn broad_genre_preset() -> Vec<GenreMappingSettings> {
        BROAD_GENRE_PRESET
            .iter()
            .map(|(pattern, genre)| GenreMappingSettings {
                id: DiscoveryStationSettings::deterministic_id(pattern, &[*genre]),
                pattern: pattern.to_string(),
                genre: genre.to_string(),
                match_mode: GenreMatchMode::Contains,
                enabled: true,
            })
            .collect()
    }
}

const BROAD_GENRE_PRESET: &[(&str, &str)] = &[
    // Latin before rap, or "trap latino" and "latin trap" become Hip-Hop.
    ("trap latino", "Latin"),
    ("latin trap", "Latin"),
    ("reggaeton", "Latin"),
    ("musica mexicana", "Latin"),
    ("regional mexican", "Latin"),
    ("salsa", "Latin"),
    ("bachata", "Latin"),
    ("cumbia", "Latin"),
    ("latin", "Latin"),
    // Compound electronic names before their single-word parts.
    ("drum and bass", "Electronic"),
    ("drum & bass", "Electronic"),
    ("dance-pop", "Pop"),
    ("dance pop", "Pop"),
    ("k-pop", "Pop"),
    ("j-pop", "Pop"),
    ("synthpop", "Pop"),
    ("indie pop", "Pop"),
    ("pop rap", "Hip-Hop"),
    ("hip hop", "Hip-Hop"),
    ("hip-hop", "Hip-Hop"),
    ("trap", "Hip-Hop"),
    ("drill", "Hip-Hop"),
    ("grime", "Hip-Hop"),
    ("phonk", "Hip-Hop"),
    ("rap", "Hip-Hop"),
    ("r&b", "R&B"),
    ("rnb", "R&B"),
    ("rhythm and blues", "R&B"),
    ("neo-soul", "R&B"),
    ("soul", "R&B"),
    ("funk", "R&B"),
    ("motown", "R&B"),
    ("house", "Dance"),
    ("techno", "Dance"),
    ("trance", "Dance"),
    ("edm", "Dance"),
    ("dubstep", "Dance"),
    ("garage", "Dance"),
    ("disco", "Dance"),
    ("electronica", "Electronic"),
    ("electro", "Electronic"),
    ("ambient", "Electronic"),
    ("idm", "Electronic"),
    ("downtempo", "Electronic"),
    ("synthwave", "Electronic"),
    ("electronic", "Electronic"),
    ("metal", "Metal"),
    ("hardcore", "Metal"),
    ("grindcore", "Metal"),
    ("doom", "Metal"),
    ("punk", "Rock"),
    ("grunge", "Rock"),
    ("emo", "Rock"),
    ("shoegaze", "Rock"),
    ("indie rock", "Rock"),
    ("alternative", "Rock"),
    ("rock", "Rock"),
    ("country", "Country"),
    ("americana", "Country"),
    ("bluegrass", "Country"),
    ("blues", "Blues"),
    ("jazz", "Jazz"),
    ("bebop", "Jazz"),
    ("swing", "Jazz"),
    ("classical", "Classical"),
    ("baroque", "Classical"),
    ("orchestral", "Classical"),
    ("opera", "Classical"),
    ("folk", "Folk"),
    ("singer-songwriter", "Folk"),
    ("acoustic", "Folk"),
    ("reggae", "Reggae"),
    ("dancehall", "Reggae"),
    ("ska", "Reggae"),
    ("dub", "Reggae"),
    ("pop", "Pop"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_mappings_keeps_the_first_duplicate_and_the_users_casing() {
        let settings = GenreSettings {
            mappings: vec![
                GenreMappingSettings {
                    pattern: " RAP ".into(),
                    genre: "Hip-Hop".into(),
                    ..Default::default()
                },
                GenreMappingSettings {
                    pattern: "rap".into(),
                    genre: "Rap".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let rules = settings.effective_mappings();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].pattern, "rap");
        assert_eq!(rules[0].genre, "Hip-Hop");
    }

    #[test]
    fn effective_blocklist_user_entries_add_and_cannot_remove_built_ins() {
        let settings = GenreSettings {
            blocklist: vec!["My Rip".into()],
            ..Default::default()
        };
        let blocked = settings.effective_blocklist();

        assert!(blocked.contains("music"));
        assert!(blocked.contains("my rip"));
    }

    #[test]
    fn backfill_extensions_get_a_dot_and_fall_back_when_empty() {
        let defaults = GenreSettings::default().effective_backfill_extensions();
        assert_eq!(defaults.len(), 9);
        assert!(defaults.contains(".FLAC"));

        let custom = GenreSettings {
            backfill_extensions: vec![" FLAC ".into(), ".mp3".into(), "".into(), "x".repeat(11)],
            ..Default::default()
        }
        .effective_backfill_extensions();
        assert_eq!(custom.iter().collect::<Vec<_>>(), [".flac", ".mp3"]);
    }

    #[test]
    fn unknown_label_and_counts_are_sanitized() {
        let s = GenreSettings {
            unknown_label: "  ".into(),
            max_genres: 0,
            backfill_max_consecutive_failures: -1,
            ..Default::default()
        };
        assert_eq!(s.effective_unknown_label(), "Unknown");
        assert_eq!(s.effective_max_genres(), 1);
        assert_eq!(s.effective_backfill_max_consecutive_failures(), 0);
    }

    #[test]
    fn broad_genre_preset_is_ordered_with_stable_ids() {
        let preset = GenreSettings::broad_genre_preset();
        assert_eq!(preset.len(), 76);
        assert_eq!(preset[0].pattern, "trap latino");
        assert_eq!(preset.last().map(|m| m.pattern.as_str()), Some("pop"));
        assert_eq!(
            preset[0].id,
            DiscoveryStationSettings::deterministic_id("trap latino", &["Latin"])
        );
        assert!(
            preset
                .iter()
                .all(|m| m.enabled && m.match_mode == GenreMatchMode::Contains)
        );
    }
}
