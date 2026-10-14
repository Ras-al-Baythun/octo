//! Importing songs from Spotify, a public link or a file. The Spotify sign-in, reading every
//! list, matching each song against the library, keeping a list as a Navidrome playlist, and the
//! trickle that fetches the missing songs a few an hour; and what the import keeps on disk
//! (`imports.json`, `imports-trickle.json`, `spotify-accounts.json`), in the shape existing files
//! already have.

pub mod import_file_reader;
pub mod import_matcher;
pub mod import_models;
pub mod import_playlists;
pub mod import_service;
pub mod import_store;
pub mod import_views;
pub mod spotify_account_store;
pub mod spotify_auth;
pub mod spotify_link_reader;
pub mod spotify_web_api;
pub mod text_list;
pub mod trickle_queue;
pub mod trickle_worker;
pub mod tunemymusic;

#[cfg(test)]
pub(crate) mod test_support;

pub use import_file_reader::{FileList, ImportFileReader};
pub use import_matcher::{ImportMatcher, LibraryIndex};
pub use import_models::{ImportList, ImportTrack, TrickleJob};
pub use import_playlists::{ImportPlaylists, KeptPlaylists, NavidromePlaylists, PlaylistNavidrome};
pub use import_service::{ImportFamily, ImportParts, ImportService};
pub use import_store::ImportStore;
pub use import_views::ImportActionResult;
pub use spotify_account_store::{SpotifyAccount, SpotifyAccountStore};
pub use spotify_auth::SpotifyAuth;
pub use spotify_link_reader::SpotifyLinkReader;
pub use spotify_web_api::{ImportKeys, SpotifyException, SpotifyWebApi};
pub use trickle_queue::TrickleQueue;
pub use trickle_worker::{TrickleServices, TrickleWorker};
pub use tunemymusic::ImportServices;

use std::path::Path;

use serde::de::DeserializeOwned;
use tracing::warn;

use super::state_file;

/// How the import stores read their file at start: missing is empty; one that cannot be read
/// is a warning ("the imported lists could not be read: ...") and empty too.
pub(crate) fn load_or_default<T: DeserializeOwned + Default>(path: &Path, what: &str) -> T {
    let loaded = (|| -> anyhow::Result<Option<T>> {
        let Some(text) = state_file::read_text(path)? else {
            return Ok(None);
        };
        Ok(serde_json::from_str::<Option<T>>(&text)?)
    })();
    match loaded {
        Ok(value) => value.unwrap_or_default(),
        Err(e) => {
            warn!("{what} could not be read: {e}");
            T::default()
        }
    }
}

/// How they wrote it: the whole file through `<file>.tmp`, compact, with a warning when it
/// fails. False when it could not be written.
pub(crate) fn save<T: serde::Serialize>(path: &Path, value: &T, what: &str) -> bool {
    match state_file::save_atomic(path, &octo_core::json::to_string(value)) {
        Ok(()) => true,
        Err(e) => {
            warn!("{what} could not be written: {e}");
            false
        }
    }
}
