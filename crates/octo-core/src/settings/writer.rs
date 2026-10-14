//! [`SettingsFileWriter`]: reads and writes `settings.json` for the admin API, on top of
//! [`Node`] so numbers keep their text and the indented output stays byte for byte what
//! existing files hold.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use parking_lot::Mutex;

use super::text::eq_ignore_case;
use crate::json::dom::Node;

/// A JSON object: properties in order, values as nodes.
pub type JsonObject = IndexMap<String, Node>;

/// Returned instead of writing when settings.json exists but cannot be parsed. Overwriting it
/// would replace everything the user had saved with whatever one form happened to send.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct SettingsFileCorruptError {
    pub path: PathBuf,
    pub message: String,
}

/// What a read-modify-write can fail with: a corrupt file it refuses to touch, or the disk.
#[derive(Debug, thiserror::Error)]
pub enum SettingsWriteError {
    #[error(transparent)]
    Corrupt(#[from] SettingsFileCorruptError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Reads and writes the editable settings JSON file. Writes do a deep merge
/// on top of any existing content so partial updates from the admin UI never
/// blow away unrelated keys (e.g. saving the LastFm tab won't drop Lidarr
/// settings).
///
/// Parsing accepts comments and trailing commas, because the configuration provider that reads
/// this same file does, so a hand-annotated file is valid for Octo. Comments are dropped on the
/// next save. A file that cannot be parsed at all is refused rather than overwritten.
#[derive(Debug)]
pub struct SettingsFileWriter {
    path: PathBuf,
    // One lock serializes every read-modify-write, and the fixed temp file name needs it.
    lock: Mutex<()>,
}

impl SettingsFileWriter {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    pub fn file_path(&self) -> &Path {
        &self.path
    }

    /// The file's content for display. Missing, empty or unreadable all give `{}`: display-only
    /// readers get an empty object; writers go through `read_for_write`, which refuses instead.
    pub fn load(&self) -> JsonObject {
        let _guard = self.lock.lock();
        self.read_for_write().unwrap_or_default()
    }

    /// False only when the file exists, has content, and that content is not a JSON
    /// object. Missing and empty files are fine: there is simply nothing saved yet.
    pub fn is_readable(&self) -> bool {
        let _guard = self.lock.lock();
        !matches!(self.read_for_write(), Err(SettingsWriteError::Corrupt(_)))
    }

    /// Deep-merge `patch` into the existing file and write atomically (write to .tmp, then
    /// rename). Returns the merged result so the caller can echo it back to the UI without
    /// re-reading.
    ///
    /// `replace_objects` names "Section.Key" objects that are replaced whole rather than
    /// merged. A dictionary setting needs this, or a key the user removed would survive in the
    /// file forever.
    ///
    /// A patch key matches an existing key ignoring case and keeps the existing spelling, so a
    /// save can never leave two spellings of one key for the configuration reader to reject.
    pub fn merge(
        &self,
        patch: &JsonObject,
        replace_objects: &[&str],
    ) -> Result<JsonObject, SettingsWriteError> {
        let _guard = self.lock.lock();
        let mut current = self.read_for_write()?;

        for path in replace_objects {
            let Some((section, key)) = path.split_once('.') else {
                continue;
            };
            let patch_has_object = get_ignore_case(patch, section)
                .and_then(Node::as_object)
                .and_then(|s| get_ignore_case(s, key))
                .is_some_and(Node::is_object);
            if !patch_has_object {
                continue;
            }
            if let Some(Node::Object(current_section)) = get_ignore_case_mut(&mut current, section)
                && let Some(existing) = find_key(current_section, key)
            {
                current_section.shift_remove(&existing);
            }
        }

        deep_merge(&mut current, patch);
        canonical_sections(&mut current);
        self.write(&current)?;
        Ok(current)
    }

    /// Read the file, let `change` edit it, and write it back, all under the one lock. For a
    /// change a patch cannot say, such as removing one entry from a dictionary, without a form
    /// save landing between the read and the write. Nothing is written when `change` returns
    /// false, so a change with nothing to do does not make every settings reader reload.
    pub fn update(&self, change: impl FnOnce(&mut JsonObject) -> bool) -> Result<bool, SettingsWriteError> {
        let _guard = self.lock.lock();
        let mut current = self.read_for_write()?;
        if !change(&mut current) {
            return Ok(false);
        }
        self.write(&current)?;
        Ok(true)
    }

    /// Replace the whole file with `content`. The Raw config editor's save, routed through here
    /// so it shares the lock and the atomic write with `merge`.
    pub fn replace(&self, content: &JsonObject) -> std::io::Result<()> {
        let _guard = self.lock.lock();
        let mut content = content.clone();
        canonical_sections(&mut content);
        self.write(&content)
    }

    fn read_for_write(&self) -> Result<JsonObject, SettingsWriteError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(JsonObject::new()),
            Err(e) => return Err(e.into()),
        };
        // A BOM is dropped and invalid UTF-8 becomes U+FFFD.
        let text = String::from_utf8_lossy(&bytes);
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        if text.trim().is_empty() {
            return Ok(JsonObject::new());
        }
        match Node::parse(text) {
            Ok(Node::Object(map)) => Ok(map),
            // Not JSON, or JSON that is not an object: refused alike.
            _ => Err(SettingsFileCorruptError {
                path: self.path.clone(),
                message: "settings.json is not valid JSON, so Octo will not write over it.".to_string(),
            }
            .into()),
        }
    }

    fn write(&self, content: &JsonObject) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        // UTF-8 without a BOM and without a trailing newline. rename(2) is atomic on one
        // filesystem; there is no fsync.
        std::fs::write(&tmp, to_indented_json(content))?;
        std::fs::rename(&tmp, &self.path)
    }
}

/// The object as indented JSON, as settings.json is written.
pub fn to_indented_json(content: &JsonObject) -> String {
    // Node::Object owns its map; the clone is cheap next to the disk write.
    Node::Object(content.clone()).to_json_string(true)
}

/// The settings sections Octo reads, spelled as its classes are: every bound section, and the
/// two read raw (`Library:DownloadPath`, `YouTube:ShimUrl`).
fn known_sections() -> impl Iterator<Item = &'static str> {
    super::AppSettings::SECTIONS
        .into_iter()
        .chain(["Library", "YouTube"])
}

/// Every top-level key that names a known section in another case is written with the section's
/// own spelling, and two spellings of one section become one (the later one's keys winning, in
/// the place of the first). Left as `"lidarr"` beside `"Lidarr"`, the configuration reader
/// would refuse the file on reload as a duplicate key.
pub fn canonical_sections(doc: &mut JsonObject) {
    for canonical in known_sections() {
        let spellings: Vec<String> = doc
            .keys()
            .filter(|key| eq_ignore_case(key, canonical))
            .cloned()
            .collect();
        if spellings.is_empty() || (spellings.len() == 1 && spellings[0] == canonical) {
            continue;
        }
        let Some(first) = doc.get_index_of(&spellings[0]) else {
            continue;
        };
        let mut merged: Option<Node> = None;
        for spelling in &spellings {
            let Some(value) = doc.shift_remove(spelling) else {
                continue;
            };
            merged = Some(match (merged, value) {
                (Some(Node::Object(mut into)), Node::Object(from)) => {
                    deep_merge(&mut into, &from);
                    Node::Object(into)
                }
                (_, value) => value,
            });
        }
        if let Some(value) = merged {
            let at = first.min(doc.len());
            doc.shift_insert(at, canonical.to_string(), value);
        }
    }
}

fn find_key(map: &JsonObject, key: &str) -> Option<String> {
    if map.contains_key(key) {
        return Some(key.to_string());
    }
    map.keys().find(|k| eq_ignore_case(k, key)).cloned()
}

fn get_ignore_case<'a>(map: &'a JsonObject, key: &str) -> Option<&'a Node> {
    find_key(map, key).and_then(|k| map.get(&k))
}

fn get_ignore_case_mut<'a>(map: &'a mut JsonObject, key: &str) -> Option<&'a mut Node> {
    let k = find_key(map, key)?;
    map.get_mut(&k)
}

fn deep_merge(target: &mut JsonObject, patch: &JsonObject) {
    for (key, value) in patch {
        let existing = find_key(target, key);
        match (value, existing.as_deref().and_then(|k| target.get_mut(k))) {
            (Node::Object(patch_child), Some(Node::Object(target_child))) => {
                deep_merge(target_child, patch_child);
            }
            (_, Some(slot)) => {
                // Replace primitives, arrays, or null values wholesale, in place.
                *slot = value.clone();
            }
            (_, None) => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

#[cfg(test)]
#[path = "writer_tests.rs"]
mod tests;
