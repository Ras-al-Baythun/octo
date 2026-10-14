//! Live takes are never what a request meant unless it said so. A title like "Song (Live)" is
//! already its own version to SongIdentity; this covers the places that rule cannot see: the
//! album folder a peer files a plainly named track under ("Decade (live at the El Mocambo)"),
//! and a recording MusicBrainz only ever lists on live albums.

use std::sync::LazyLock;

use regex::Regex;

use super::dotnet::{is_null_or_white_space, word_boundary_view};

static MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(live|unplugged|in concert|concert|bootleg)\b").expect("a fixed pattern compiles")
});

pub fn mentions(text: Option<&str>) -> bool {
    // Matched in the view where \b falls by word characters in one UTF-16 unit (see
    // `word_boundary_view`).
    !is_null_or_white_space(text) && text.is_some_and(|text| MARKER.is_match(&word_boundary_view(text)))
}

/// Whether the request itself asks for a live take, by its title or its album. Generous on
/// purpose: a title such as "Live Forever" lets a live take through rather than ever
/// refusing a song someone hearted from a live album.
pub fn requested(title: Option<&str>, album: Option<&str>) -> bool {
    mentions(title) || mentions(album)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The library's live-folder checks and download verification build on these; the cases here
    // test the live markers themselves.

    #[test]
    fn a_live_request_says_so_by_title_or_album() {
        assert!(requested(
            Some("Smile in Your Sleep"),
            Some("Decade (live at the El Mocambo)")
        ));
        assert!(requested(Some("Smile in Your Sleep (Live)"), None));
        assert!(requested(Some("Doll Parts"), Some("Live Through This")));
        assert!(!requested(Some("Smile in Your Sleep"), Some("Sad Songs Vol. 1")));
        assert!(!requested(Some("Smile in Your Sleep"), None));
    }

    #[test]
    fn mentions_reads_whole_words() {
        for text in [
            "Live at Wembley '86",
            "MTV Unplugged in New York",
            "In Concert",
            "2000-06-25 Katowice [bootleg]",
        ] {
            assert!(mentions(Some(text)), "{text}");
        }
        for text in ["Discovering the Waterfront", "Deliver Us", "Olive", "", "  "] {
            assert!(!mentions(Some(text)), "{text}");
        }
        assert!(!mentions(None));
    }
}
