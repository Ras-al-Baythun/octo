//! Turns raw genre values into the small set of genres a library can browse.

use std::sync::LazyLock;

use regex::Regex;
use unicode_general_category::{GeneralCategory, get_general_category};

use crate::common::dotnet::{
    eq_ignore_case, is_blank, is_letter_utf16, is_upper_utf16, to_lower_char, to_upper_char,
    utf16_class_view, utf16_len,
};
use crate::settings::{
    DiscoveryStationSettings, GenreEmptyBehavior, GenreMappingSettings, GenreMatchMode, GenreSettings,
    IgnoreCaseSet,
};

/// The normalized genres for a set of raw values, and which rule decided the first one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenreNormalizationResult {
    pub genres: Vec<String>,
    pub primary: Option<String>,
    pub matched_rule: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GenreTagAction {
    None,
    Write,
    Clear,
}

/// What should actually be written to a file's genre frame, including the decision
/// to write NOTHING and the decision to write an EMPTY set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenreTagPlan {
    pub action: GenreTagAction,
    pub genres: Vec<String>,
    pub primary: Option<String>,
    pub matched_rule: Option<String>,
}

impl GenreTagPlan {
    fn nothing(action: GenreTagAction) -> Self {
        Self {
            action,
            genres: Vec::new(),
            primary: None,
            matched_rule: None,
        }
    }
}

/// Characters a tagger uses to cram several genres into one string. '&' is NOT here:
/// splitting on it destroys "R&B" and "Drum & Bass".
const SEPARATORS: [char; 5] = [';', ',', '|', '\0', '/'];

/// `^\s*\(\d+\)\s*`, matched over [`utf16_class_view`] so `\d` sees the digits it should.
static NUMERIC_GENRE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*\(\d+\)\s*").expect("a fixed pattern compiles"));

struct Token<'a> {
    key: String,
    display: String,
    rule: Option<&'a GenreMappingSettings>,
}

/// Turns whatever a downloader, a release's tagger or Deezer called a genre into the small set
/// of genres a library can browse.
///
/// Static and synchronous on purpose: the fallback lookup is the caller's async problem, and
/// a backfill over two thousand files has to be able to run this with no network at all.
pub struct GenreNormalizer;

impl GenreNormalizer {
    /// A "1998" or a "90s" says when, not what.
    ///
    /// Radio's kinship filter and the genre blocklist both need this rule, so it exists once:
    /// two hand-rolled copies is how one of them quietly stops dropping "2020s" when someone
    /// fixes the other.
    pub fn is_year_like(tag: &str) -> bool {
        if tag.is_empty() {
            return false;
        }
        if tag.chars().all(|c| c.is_ascii_digit()) {
            return true; // 1998, 2026, 80
        }
        let Some(stem) = tag.strip_suffix('s') else {
            return false;
        };
        // 90s, 1990s, 00s
        (1..=4).contains(&utf16_len(stem)) && stem.chars().all(|c| c.is_ascii_digit())
    }

    /// Decide what to write to a file, given the frame it already carries and whatever genre
    /// was resolved for it.
    pub fn plan<S: AsRef<str>>(
        existing_frame: &[S],
        resolved: Option<&str>,
        settings: &GenreSettings,
        fallback_tags: Option<&[S]>,
    ) -> GenreTagPlan {
        // Priority: the value we resolved, which is Deezer's curated album genre, leads. Then
        // the file's own frame in frame order. A stranger's tag is evidence, not authority.
        let mut candidates: Vec<Option<&str>> = Vec::with_capacity(existing_frame.len() + 1);
        candidates.push(resolved);
        candidates.extend(existing_frame.iter().map(|value| Some(value.as_ref())));

        let mut result = Self::normalize(candidates, settings);
        if result.genres.is_empty()
            && let Some(fallback) = fallback_tags.filter(|tags| !tags.is_empty())
        {
            result = Self::normalize(fallback.iter().map(|tag| Some(tag.as_ref())), settings);
        }

        if !result.genres.is_empty() {
            return GenreTagPlan {
                action: GenreTagAction::Write,
                genres: result.genres,
                primary: result.primary,
                matched_rule: result.matched_rule,
            };
        }

        // Nothing survived. Whether that CLEARS the frame or leaves it is the difference
        // between fixing the bug and reproducing it: genre was only ever written when
        // non-empty, so a file that arrived tagged "People & Blogs" kept it forever.
        //
        // The one guard: a file that had no frame and resolved to nothing is left completely
        // alone. Writing an empty frame over an absent one is a rewrite with no benefit, and
        // it dirties a file the run should have left byte-identical.
        let had_something = !existing_frame.is_empty() || !resolved.is_none_or(is_blank);
        if !had_something {
            return GenreTagPlan::nothing(GenreTagAction::None);
        }

        match settings.on_empty {
            GenreEmptyBehavior::Clear => GenreTagPlan::nothing(GenreTagAction::Clear),
            GenreEmptyBehavior::Unknown => {
                let label = settings.effective_unknown_label();
                GenreTagPlan {
                    action: GenreTagAction::Write,
                    genres: vec![label.clone()],
                    primary: Some(label),
                    matched_rule: None,
                }
            }
            GenreEmptyBehavior::Leave => GenreTagPlan::nothing(GenreTagAction::None),
        }
    }

    /// The pipeline, and the ORDER is the design: split, canonicalize, blocklist, map, case,
    /// dedupe, cap.
    pub fn normalize<'a>(
        raw: impl IntoIterator<Item = Option<&'a str>>,
        settings: &GenreSettings,
    ) -> GenreNormalizationResult {
        let blocked = settings.effective_blocklist();
        let rules = settings.effective_mappings();
        let cap = settings.effective_max_genres().max(0) as usize;

        let mut kept: Vec<String> = Vec::new();
        let mut seen = IgnoreCaseSet::new();
        let mut matched_rule: Option<String> = None;

        for value in raw {
            let Some(value) = value.filter(|value| !is_blank(value)) else {
                continue;
            };
            for token in tokenize(value, &rules) {
                if blocked.contains(&token.key) || Self::is_year_like(&token.key) {
                    continue;
                }

                // An empty target is a delete, documented rather than accidental.
                if token.display.is_empty() {
                    continue;
                }
                if !seen.insert(token.display.clone()) {
                    continue;
                }

                kept.push(token.display);
                if matched_rule.is_none() {
                    matched_rule = token
                        .rule
                        .map(|rule| format!("{} -> {}", rule.pattern, rule.genre));
                }
                if kept.len() >= cap {
                    return done(kept, matched_rule);
                }
            }
        }

        done(kept, matched_rule)
    }
}

fn done(kept: Vec<String>, rule: Option<String>) -> GenreNormalizationResult {
    GenreNormalizationResult {
        primary: kept.first().cloned(),
        genres: kept,
        matched_rule: rule,
    }
}

/// One raw value becomes one or more candidate tokens.
///
/// The whole string gets its own pass at the rules BEFORE it is split, so a user rule
/// written for a compound form ("hip-hop/rap") can fire before '/' tears it in half. It is
/// only emitted when a rule actually matches it, because otherwise an unmapped "Rock, Pop"
/// would be title-cased whole and kept as a single bogus genre.
fn tokenize<'a>(value: &str, rules: &'a [GenreMappingSettings]) -> Vec<Token<'a>> {
    let whole_key = canonicalize(value);
    if (1..=60).contains(&utf16_len(&whole_key))
        && let Some(whole_rule) = first_match(rules, &whole_key)
    {
        return vec![Token {
            key: whole_key,
            display: whole_rule.genre.clone(),
            rule: Some(whole_rule),
        }];
    }

    let mut tokens = Vec::new();
    for part in value
        .split(SEPARATORS)
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let key = canonicalize(part);
        let length = utf16_len(&key);
        if length == 0 || length > 60 {
            continue;
        }
        let rule = first_match(rules, &key);
        let display = match rule {
            Some(rule) => rule.genre.clone(),
            None => title_case_genre(&key, part),
        };
        tokens.push(Token { key, display, rule });
    }
    tokens
}

/// Lowercase and single-space via the primitive the radio code already uses, with ID3v1
/// numeric residue stripped first: an ID3v1 tag inside a v2 frame sometimes leaves the
/// literal text "(17)" or "(17)Rock".
fn canonicalize(value: &str) -> String {
    DiscoveryStationSettings::normalize_tag(&strip_numeric_genre(value))
}

/// "(17)" and "(17)Rock" are ID3v1 numeric genres leaking through as text. Applied
/// to the display copy as well as the matching key, or preserving the source's own spelling
/// would preserve the residue with it.
fn strip_numeric_genre(value: &str) -> String {
    let view = utf16_class_view(value);
    let rest = match NUMERIC_GENRE.find(&view) {
        Some(found) => &value[found.end()..],
        None => value,
    };
    rest.trim()
        .split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// First rule wins and mapping STOPS for that token. No re-entry: letting "rap -> Rap"
/// feed "Rap -> Hip-Hop" means a user's table can loop.
fn first_match<'a>(rules: &'a [GenreMappingSettings], key: &str) -> Option<&'a GenreMappingSettings> {
    rules.iter().find(|rule| {
        if !rule.enabled || rule.pattern.is_empty() {
            return false;
        }
        match rule.match_mode {
            GenreMatchMode::Exact => eq_ignore_case(key, &rule.pattern),
            GenreMatchMode::Contains => contains_ignore_case(key, &rule.pattern),
        }
    })
}

/// What an ordinal case-insensitive comparison compares a character as (ı and ſ stay apart
/// from I and S, as they do in `eq_ignore_case`).
fn ordinal_case_key(c: char) -> char {
    if c == '\u{0131}' || c == '\u{017F}' {
        c
    } else {
        to_upper_char(c)
    }
}

/// Whether `text` contains `value`, ordinally and ignoring case.
fn contains_ignore_case(text: &str, value: &str) -> bool {
    let text: String = text.chars().map(ordinal_case_key).collect();
    let value: String = value.chars().map(ordinal_case_key).collect();
    text.contains(&value)
}

/// Title-case an unmapped genre for display.
///
/// The acronym guard has to read the ORIGINAL text, not the canonical key: the key has
/// already been lowercased, so asking whether it is all-caps always answers no and EDM,
/// IDM and UKG all come back as Edm, Idm and Ukg.
///
/// Invariant culture throughout, because under a Turkish locale title casing turns "indie" into
/// something that no longer matches "Indie", and a container's locale is not something a user
/// thinks of as a genre setting.
fn title_case_genre(key: &str, original: &str) -> String {
    let trimmed = strip_numeric_genre(original);
    if trimmed.is_empty() {
        return to_title_case(key);
    }
    // Letters and capitals are judged per UTF-16 unit; a supplementary character is neither,
    // which the `_utf16` tests answer the same way.
    if (1..=4).contains(&utf16_len(&trimmed))
        && trimmed.chars().any(is_letter_utf16)
        && trimmed.chars().all(|c| !is_letter_utf16(c) || is_upper_utf16(c))
    {
        return trimmed;
    }

    // Already capitalized by whoever wrote it, so leave their spelling alone. Octo has no
    // business re-capitalizing a genre it does not recognize, and doing so counted as a
    // "change": a whole-library run proposed rewriting a file purely to turn
    // "Alternatif et Indé" into "Alternatif Et Indé". Tidying case is only worth a write
    // when the source clearly did not bother, which is when it is entirely lower case.
    if trimmed.chars().any(is_upper_utf16) {
        return trimmed;
    }

    to_title_case(key)
}

fn is_letter_category(category: GeneralCategory) -> bool {
    matches!(
        category,
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
    )
}

/// The categories title casing ends a word on: spaces, controls, format characters, punctuation
/// and symbols. Letters, marks, numbers, private use and unassigned characters continue it.
fn is_word_separator(category: GeneralCategory) -> bool {
    matches!(
        category,
        GeneralCategory::SpaceSeparator
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
            | GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::ConnectorPunctuation
            | GeneralCategory::DashPunctuation
            | GeneralCategory::OpenPunctuation
            | GeneralCategory::ClosePunctuation
            | GeneralCategory::InitialPunctuation
            | GeneralCategory::FinalPunctuation
            | GeneralCategory::OtherPunctuation
            | GeneralCategory::MathSymbol
            | GeneralCategory::CurrencySymbol
            | GeneralCategory::ModifierSymbol
            | GeneralCategory::OtherSymbol
    )
}

/// The first letter of a word as title casing writes it: the titlecase forms of the Latin
/// digraphs, otherwise the invariant upper case.
fn title_letter(c: char) -> char {
    match c {
        '\u{01C4}' | '\u{01C5}' | '\u{01C6}' => '\u{01C5}',
        '\u{01C7}' | '\u{01C8}' | '\u{01C9}' => '\u{01C8}',
        '\u{01CA}' | '\u{01CB}' | '\u{01CC}' => '\u{01CB}',
        '\u{01F1}' | '\u{01F2}' | '\u{01F3}' => '\u{01F2}',
        _ => to_upper_char(c),
    }
}

/// Invariant-culture title casing: each word's first letter upper-cased and the rest of it
/// lower-cased, unless the word has no lower-case letter (an acronym), which is left as it is. An
/// apostrophe inside a word starts a new stretch that is lower-cased ("Rock'n'roll").
pub fn to_title_case(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut result = String::with_capacity(text.len());
    let mut i = 0;
    // Appends chars[from..to], lower-cased when the stretch had a lower-case letter.
    let append = |result: &mut String, from: usize, to: usize, lower: bool| {
        for &c in &chars[from..to] {
            result.push(if lower { to_lower_char(c) } else { c });
        }
    };
    while i < chars.len() {
        let category = get_general_category(chars[i]);
        if !is_letter_category(category) {
            result.push(chars[i]);
            i += 1;
            continue;
        }

        // Do the titlecasing for the first character of the word.
        result.push(title_letter(chars[i]));
        i += 1;

        // Convert the characters until the end of this word to lowercase.
        let mut lowercase_start = i;
        // `has_lower_case` keeps acronyms (like "URT", "USA") from being lowercased.
        let mut has_lower_case = category == GeneralCategory::LowercaseLetter;
        while i < chars.len() {
            let category = get_general_category(chars[i]);
            if is_letter_category(category) {
                if category == GeneralCategory::LowercaseLetter {
                    has_lower_case = true;
                }
                i += 1;
            } else if chars[i] == '\'' {
                i += 1;
                append(&mut result, lowercase_start, i, has_lower_case);
                lowercase_start = i;
                has_lower_case = true;
            } else if !is_word_separator(category) {
                // This category is considered to be part of the word.
                i += 1;
            } else {
                // A word separator. Break out of the loop.
                break;
            }
        }
        append(&mut result, lowercase_start, i, has_lower_case);

        if i < chars.len() {
            // Not a letter, just append it.
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

#[cfg(test)]
#[path = "genre_normalizer_tests.rs"]
mod tests;
