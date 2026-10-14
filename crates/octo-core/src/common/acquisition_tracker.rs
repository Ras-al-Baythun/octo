//! The pure half of the acquisition tracker: a transfer's progress from a source's figures, and the
//! short, path-free error a phone is shown. The tracker itself (the live list behind the download
//! ring) is `octo::services::common::acquisition_tracker`.

use unicode_general_category::{GeneralCategory, get_general_category};

use super::dotnet;

/// The longest error a row carries, in UTF-16 code units.
pub const MAX_ERROR_LENGTH: usize = 160;

/// Progress from 0 to 1. The bytes win when both are known, because a source's percentage is
/// rounded; the percentage (0 to 100) is the fallback. `None` when neither says anything.
pub fn fraction_of(
    bytes_done: Option<i64>,
    bytes_total: Option<i64>,
    percent_complete: Option<f64>,
) -> Option<f64> {
    let fraction = match (bytes_done, bytes_total) {
        (Some(done), Some(total)) if done >= 0 && total > 0 => Some(done as f64 / total as f64),
        _ => percent_complete
            .filter(|percent| !percent.is_nan())
            .map(|percent| percent / 100.0),
    };
    fraction.map(|value| dotnet::round(value.clamp(0.0, 1.0), 4))
}

/// The first sentence of an error, with server paths taken out and the length capped. The
/// messages behind a failed download are written for the log, and the rest of them (which
/// indexer, which setting) means nothing on a phone.
pub fn user_safe(message: Option<&str>) -> Option<String> {
    let message = message.filter(|m| !dotnet::is_blank(m))?;
    let first = message.split('\n').next().unwrap_or_default().trim();
    let sentence = match sentence_end(first) {
        Some(end) => &first[..end],
        None => first,
    };
    let replaced = replace_server_paths(sentence);
    let mut text = replaced.trim().trim_end_matches('.').to_string();
    if dotnet::utf16_len(&text) > MAX_ERROR_LENGTH {
        text = format!("{}...", utf16_prefix(&text, MAX_ERROR_LENGTH - 3).trim_end());
    }
    if text.is_empty() { None } else { Some(text) }
}

/// `\p{N}` on one UTF-16 code unit: a number of any kind (Nd, Nl, No).
fn is_number_utf16(c: char) -> bool {
    (c as u32) <= 0xFFFF
        && matches!(
            get_general_category(c),
            GeneralCategory::DecimalNumber | GeneralCategory::LetterNumber | GeneralCategory::OtherNumber
        )
}

fn is_letter_or_number_utf16(c: char) -> bool {
    dotnet::is_letter_utf16(c) || is_number_utf16(c)
}

/// Where the first sentence ends, by the pattern `(?<=[\p{L}\p{N}'"\)]{4})\.\s+(?=\p{Lu})`. A
/// sentence ends at a full stop after a word of four or more characters and before a capital, so
/// "Mr. Brightside" and "St. Vincent" stay whole. The byte index of the full stop, which the
/// match began with.
fn sentence_end(text: &str) -> Option<usize> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let word = |c: char| is_letter_or_number_utf16(c) || matches!(c, '\'' | '"' | ')');
    for k in 4..chars.len() {
        if chars[k].1 != '.' || !chars[k - 4..k].iter().all(|&(_, c)| word(c)) {
            continue;
        }
        // `\s+` then a capital: the whitespace run is greedy, and giving any of it back only
        // puts whitespace where the capital must be.
        let mut j = k + 1;
        while j < chars.len() && chars[j].1.is_whitespace() {
            j += 1;
        }
        if j > k + 1 && j < chars.len() && dotnet::is_upper_utf16(chars[j].1) {
            return Some(chars[k].0);
        }
    }
    None
}

/// The server path pattern, `(?<![\p{L}\p{N}])(?:[A-Za-z]:\\|/)[^\s'"(),;]+`, replaced with
/// "a path". A path on the server is nobody's business away from home. "AC/DC" is not a path: a
/// letter or digit before the slash rules it out.
fn replace_server_paths(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let body = |c: char| !c.is_whitespace() && !matches!(c, '\'' | '"' | '(' | ')' | ',' | ';');
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let free = i == 0 || !is_letter_or_number_utf16(chars[i - 1]);
        let start = if !free {
            None
        } else if i + 2 < chars.len()
            && chars[i].is_ascii_alphabetic()
            && chars[i + 1] == ':'
            && chars[i + 2] == '\\'
        {
            Some(i + 3)
        } else if chars[i] == '/' {
            Some(i + 1)
        } else {
            None
        };
        if let Some(start) = start {
            let mut end = start;
            while end < chars.len() && body(chars[end]) {
                end += 1;
            }
            if end > start {
                out.push_str("a path");
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// The longest prefix of at most `units` UTF-16 code units (`text[..units]`), never splitting a
/// character, since a Rust string cannot hold half of a surrogate pair.
fn utf16_prefix(text: &str, units: usize) -> &str {
    let mut count = 0;
    for (index, c) in text.char_indices() {
        count += c.len_utf16();
        if count > units {
            return &text[..index];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_comes_from_the_bytes_then_the_percentage() {
        let cases: [(Option<i64>, Option<i64>, Option<f64>, f64); 6] = [
            (Some(7_250_000), Some(29_000_000), None, 0.25),
            (Some(12_180_000), Some(29_000_000), Some(99.0), 0.42),
            (None, None, Some(42.0), 0.42),
            (None, Some(29_000_000), Some(42.0), 0.42),
            (Some(30_000_000), Some(29_000_000), None, 1.0),
            (Some(0), Some(29_000_000), None, 0.0),
        ];
        for (done, total, percent, expected) in cases {
            assert_eq!(
                fraction_of(done, total, percent),
                Some(expected),
                "{done:?} / {total:?} / {percent:?}"
            );
        }
    }

    #[test]
    fn no_figures_means_no_progress() {
        assert_eq!(fraction_of(None, None, None), None);
        assert_eq!(fraction_of(Some(100), Some(0), None), None);
    }

    #[test]
    fn errors_are_short_and_keep_server_paths_home() {
        let cases = [
            (
                "Lidarr found no lossless copy of 'The Killers - Mr. Brightside'. Last error: timed out. If Lidarr shows the album as imported, its root folder is not the directory Octo watches (/music); set LIDARR_ROOT_FOLDER=/music.",
                "Lidarr found no lossless copy of 'The Killers - Mr. Brightside'",
            ),
            (
                "Could not open /music/.octo-incoming/abc.mp3 for reading",
                "Could not open a path for reading",
            ),
            (
                r"Access to C:\Music\x.flac is denied",
                "Access to a path is denied",
            ),
            (
                "No YouTube match for 'AC/DC - Thunderstruck'",
                "No YouTube match for 'AC/DC - Thunderstruck'",
            ),
            ("Song not found", "Song not found"),
        ];
        for (message, expected) in cases {
            assert_eq!(user_safe(Some(message)).as_deref(), Some(expected), "{message}");
        }
    }

    #[test]
    fn an_overlong_error_is_cut() {
        let safe = user_safe(Some(&"x".repeat(400))).expect("an error");
        assert!(safe.len() <= MAX_ERROR_LENGTH);
        assert!(safe.ends_with("..."));
    }

    /// Nothing to say is no error, only the first line counts, and a path alone is a path.
    #[test]
    fn blank_and_multi_line_errors() {
        assert_eq!(user_safe(None), None);
        assert_eq!(user_safe(Some("  \n ")), None);
        assert_eq!(
            user_safe(Some("first line.\nsecond")).as_deref(),
            Some("first line")
        );
        assert_eq!(user_safe(Some("/")).as_deref(), Some("/"));
        assert_eq!(
            user_safe(Some("x/y and ./rel")).as_deref(),
            Some("x/y and .a path")
        );
        // A full stop after a short word, or before a lower-case word, ends nothing.
        assert_eq!(
            user_safe(Some("Mr. Brightside and St. Vincent. then more")).as_deref(),
            Some("Mr. Brightside and St. Vincent. then more")
        );
    }
}
