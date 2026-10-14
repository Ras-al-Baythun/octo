//! Building and sanitizing paths for downloaded music files.
//!
//! Paths are built as strings joined with `/`, as on Linux, where Octo runs. The characters a
//! file name may not hold are Linux's: NUL and `/`.

use std::path::Path;
use std::sync::LazyLock;

use regex::{Captures, Regex};

use crate::common::dotnet::{eq_ignore_case, is_blank, starts_with_ignore_case, utf16_len};
use crate::settings::subsonic::FolderStructure;

/// Helper class for path building and sanitization.
/// Provides utilities for creating safe file and folder paths for downloaded music files.
pub struct PathHelper;

/// The characters a Linux file name may not hold.
const INVALID_FILE_NAME_CHARS: [char; 2] = ['\0', '/'];

/// The characters a Linux folder name or path segment may not hold.
const INVALID_FOLDER_NAME_CHARS: [char; 2] = ['\0', '/'];

static ANNOTATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*[\(\[]([^\)\]]*)[\)\]]").expect("the annotation pattern is valid"));

static WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("the whitespace pattern is valid"));

/// Words that only ever describe how a video was uploaded, never which recording it is.
const UPLOAD_NOISE: [&str; 19] = [
    "official",
    "music",
    "video",
    "audio",
    "lyric",
    "lyrics",
    "visualizer",
    "visualiser",
    "hd",
    "hq",
    "4k",
    "8k",
    "1080p",
    "720p",
    "480p",
    "mv",
    "m/v",
    "clip",
    "videoclip",
];

fn is_upload_noise(word: &str) -> bool {
    UPLOAD_NOISE.iter().any(|noise| eq_ignore_case(noise, word))
}

impl PathHelper {
    /// Where a track goes for a given [`FolderStructure`].
    ///
    /// Every download path routes through here so one setting decides the layout for all
    /// of them. It used to be decided by a separate switch per source - the download's move,
    /// the YouTube download and the Lidarr import - and two of those carried a silent
    /// default branch, so adding a layout would have left them quietly filing into the old
    /// one while only the third obeyed the setting.
    ///
    /// The match is exhaustive on purpose: a new layout should fail to compile here rather
    /// than resolve to whatever the default arm happened to be.
    pub fn build_layout_path(
        structure: FolderStructure,
        download_path: &str,
        artist: &str,
        album: &str,
        title: &str,
        track_number: Option<i32>,
        extension: &str,
    ) -> String {
        let safe_artist = Self::sanitize_folder_name(artist);
        let safe_title = Self::sanitize_file_name(title);

        // A track with no album falls back to its own title as the folder. Before albums
        // existed the Organized layout always used the TRACK title, scattering an album's
        // tracks into a folder each; the routing carries the album now, and this keeps that
        // old shape only for a track that genuinely has none. The rule lives here so every
        // caller gets it instead of each one remembering to apply it.
        let effective_album = if is_blank(album) { title } else { album };

        match structure {
            FolderStructure::Flat => {
                combine(download_path, &format!("{safe_artist} - {safe_title}{extension}"))
            }

            // No album folder: the album stays in the tags, which is what the server reads.
            FolderStructure::ByArtist => combine(
                &combine(download_path, &safe_artist),
                &format!("{safe_title}{extension}"),
            ),

            FolderStructure::Organized => Self::build_track_path(
                download_path,
                artist,
                effective_album,
                title,
                track_number,
                extension,
            ),
        }
    }

    /// A title as a file name. Drops a leading "Artist - " and any bracket that is nothing but
    /// upload noise ("(Official Video)", "[HD]"), and keeps every other annotation, because
    /// "(Live)", "[Remix]" and "(feat. X)" each name a different recording.
    ///
    /// Naming used to strip EVERY bracket, so "Song (Live)" and "Song" landed on one path and
    /// the second download deleted the first: the silent collapse of versions a library must
    /// never suffer (#53).
    pub fn file_title(title: &str, artist: &str) -> String {
        let mut t = title.trim().to_string();
        if t.is_empty() {
            return t;
        }
        let a = artist.trim();
        let prefix = format!("{a} - ");
        if !a.is_empty() && starts_with_ignore_case(&t, &prefix) {
            // OrdinalIgnoreCase matches one character for one, so the prefix spans as many
            // characters of the title as it has itself.
            t = t
                .chars()
                .skip(prefix.chars().count())
                .collect::<String>()
                .trim()
                .to_string();
        }

        let kept = ANNOTATION.replace_all(&t, |found: &Captures<'_>| {
            let words: Vec<&str> = found[1]
                .split([' ', '-', '_'])
                .filter(|w| !w.is_empty())
                .collect();
            if !words.is_empty() && words.iter().all(|word| is_upload_noise(word)) {
                String::new()
            } else {
                found[0].to_string()
            }
        });
        let kept = WHITESPACE.replace_all(&kept, " ").trim().to_string();
        // A title that is only noise ("(Official Video)") keeps its original text rather than
        // becoming an empty file name.
        if kept.is_empty() { t } else { kept }
    }

    /// Gets the cache directory path for temporary file storage.
    /// Uses system temp directory combined with octo-cache subfolder.
    /// Respects TMPDIR environment variable on Linux/macOS.
    pub fn get_cache_path() -> String {
        combine(&std::env::temp_dir().to_string_lossy(), "octo-cache")
    }

    /// Builds the output path for a downloaded track following the Artist/Album/Track structure.
    ///
    /// `download_path` is the base download directory; the artist, album and title are
    /// sanitized; `track_number`, when given, is a two-digit prefix; `extension` is the file
    /// extension with its dot (".flac", ".mp3"), or empty.
    pub fn build_track_path(
        download_path: &str,
        artist: &str,
        album: &str,
        title: &str,
        track_number: Option<i32>,
        extension: &str,
    ) -> String {
        let safe_artist = Self::sanitize_folder_name(artist);
        let safe_album = Self::sanitize_folder_name(album);
        let safe_title = Self::sanitize_file_name(title);

        let artist_folder = combine(download_path, &safe_artist);
        let album_folder = combine(&artist_folder, &safe_album);

        let track_prefix = match track_number {
            // "D2": at least two digits, the sign in front of them.
            Some(n) if n < 0 => format!("-{:02} - ", n.unsigned_abs()),
            Some(n) => format!("{n:02} - "),
            None => String::new(),
        };
        let file_name = format!("{track_prefix}{safe_title}{extension}");

        combine(&album_folder, &file_name)
    }

    /// Sanitizes a file name by removing invalid characters.
    pub fn sanitize_file_name(file_name: &str) -> String {
        if is_blank(file_name) {
            return "Unknown".to_string();
        }

        let mut sanitized: String = file_name
            .chars()
            .map(|c| {
                if INVALID_FILE_NAME_CHARS.contains(&c) {
                    '_'
                } else {
                    c
                }
            })
            .collect();

        if utf16_len(&sanitized) > 100 {
            sanitized = truncate_utf16(&sanitized, 100);
        }

        sanitized.trim().to_string()
    }

    /// Sanitizes a folder name by removing invalid path characters.
    pub fn sanitize_folder_name(folder_name: &str) -> String {
        if is_blank(folder_name) {
            return "Unknown".to_string();
        }

        let sanitized: String = folder_name
            .chars()
            .map(|c| {
                if INVALID_FOLDER_NAME_CHARS.contains(&c) {
                    '_'
                } else {
                    c
                }
            })
            .collect();

        // Remove leading/trailing dots and spaces (Windows folder restrictions)
        let mut sanitized = sanitized.trim().trim_end_matches('.').to_string();

        if utf16_len(&sanitized) > 100 {
            sanitized = truncate_utf16(&sanitized, 100).trim_end_matches('.').to_string();
        }

        // Ensure we have a valid name
        if is_blank(&sanitized) {
            return "Unknown".to_string();
        }

        sanitized
    }

    /// True when the path starts with a Windows drive letter ("E:\" or "E:/").
    /// On a non-Windows host such a path is not a location: the filesystem
    /// treats it as a literal directory name.
    pub fn looks_like_windows_drive_path(path: Option<&str>) -> bool {
        let Some(path) = path else { return false };
        let mut chars = path.chars();
        matches!(
            (chars.next(), chars.next(), chars.next()),
            (Some(drive), Some(':'), Some('\\' | '/')) if drive.is_ascii_alphabetic()
        )
    }

    /// Where `path` is once the folder `from` has moved to `to`: `to` for `from` itself, the
    /// same place inside `to` for a path inside `from`, and None for any other path.
    pub fn moved_path(path: &str, from: &str, to: &str) -> Option<String> {
        let from = from.trim_end_matches('/');
        let to = to.trim_end_matches('/');
        if from.is_empty() {
            return None;
        }
        if path == from {
            return Some(to.to_string());
        }
        path.strip_prefix(from)
            .filter(|rest| rest.starts_with('/'))
            .map(|rest| format!("{to}{rest}"))
    }

    /// Resolves a unique file path by appending a counter if the file already exists.
    pub fn resolve_unique_path(base_path: &str) -> String {
        if !file_exists(base_path) {
            return base_path.to_string();
        }

        let directory = directory_name(base_path);
        let extension = extension_of(base_path);
        let file_name_without_ext = file_name_without_extension(base_path);

        let mut counter = 1;
        loop {
            let unique_path = combine(
                &directory,
                &format!("{file_name_without_ext} ({counter}){extension}"),
            );
            counter += 1;
            if !file_exists(&unique_path) {
                return unique_path;
            }
        }
    }
}

/// Whether the path names an existing file, not a directory.
fn file_exists(path: &str) -> bool {
    !path.is_empty() && Path::new(path).is_file()
}

/// Joins two paths: `b` when it is rooted or `a` is empty, else the two joined by one `/`.
fn combine(a: &str, b: &str) -> String {
    if b.is_empty() {
        return a.to_string();
    }
    if a.is_empty() || b.starts_with('/') {
        return b.to_string();
    }
    if a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

/// `s[..units]` in UTF-16 code units. A character split in two by the cut is left out whole,
/// since a Rust string cannot hold a lone surrogate.
fn truncate_utf16(s: &str, units: usize) -> String {
    let mut taken = 0;
    s.chars()
        .take_while(|c| {
            taken += c.len_utf16();
            taken <= units
        })
        .collect()
}

/// The file name part of a path: everything after the last `/`.
fn file_name(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// The directory of a path that names a file: everything before the last `/` ("/" itself for a
/// file in the root), or empty when there is none.
fn directory_name(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_string(),
        Some(at) => path[..at].trim_end_matches('/').to_string(),
        None => String::new(),
    }
}

/// The extension: from the last `.` of the file name, or empty when there is none or it
/// is the last character.
fn extension_of(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(at) if at + 1 < name.len() => &name[at..],
        _ => "",
    }
}

/// The file name up to its last `.`.
fn file_name_without_extension(path: &str) -> &str {
    let name = file_name(path);
    match name.rfind('.') {
        Some(at) => &name[..at],
        None => name,
    }
}

#[cfg(test)]
#[path = "path_helper_tests.rs"]
mod tests;
