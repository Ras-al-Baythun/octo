//! The settings classes, the live store that binds them from the layered configuration, and
//! the admin helpers that edit `settings.json` (`SettingsFileWriter`, `RestartTracker`).
//!
//! Each struct derives `Deserialize` with `#[serde(default)]` and carries its defaults in its
//! `Default`; [`crate::config::bind`] fills it from a configuration section. `effective_*`
//! methods clamp at read time.

pub mod admin;
pub mod downloads;
pub mod family;
pub mod generated_playlist;
pub mod genre;
pub mod import;
pub mod last_fm;
pub mod library_action;
pub mod lidarr;
pub mod listen_brainz;
pub mod metadata;
pub mod notification;
pub mod radio_source;
pub mod restart_tracker;
pub mod server;
pub mod store;
pub mod subsonic;
mod text;
pub mod update;
pub mod writer;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::{BindWarning, ConfigTree, bind};

pub use admin::AdminSettings;
pub use downloads::DownloadSettings;
pub use family::FamilySettings;
pub use generated_playlist::GeneratedPlaylistSettings;
pub use genre::{
    GenreEmptyBehavior, GenreFallbackSource, GenreMappingSettings, GenreMatchMode, GenreSettings,
};
pub use import::ImportSettings;
pub use last_fm::{DiscoveryStationSettings, LastFmSettings, LastFmUserSession};
pub use library_action::{
    LibraryAction, LibraryActionDefinition, LibraryActionSettings, LibraryActionTrigger, LibraryRatingScope,
    NoticeKind, UpgradeSourceChoice,
};
pub use lidarr::{LidarrCompletionMode, LidarrSettings};
pub use listen_brainz::ListenBrainzSettings;
pub use metadata::{LyricsSaveTo, MetadataSettings};
pub use notification::NotificationSettings;
pub use radio_source::RadioSourceSettings;
pub use restart_tracker::RestartTracker;
pub use server::ServerSettings;
pub use store::{RawConfig, SettingsStore};
pub use subsonic::{
    DownloadMode, DownloadSource, ExplicitFilter, FolderStructure, HeartDownloadSource, HeartDownloadStep,
    StorageMode, SubsonicSettings,
};
pub use text::IgnoreCaseSet;
pub use update::UpdateSettings;
pub use writer::{JsonObject, SettingsFileCorruptError, SettingsFileWriter, SettingsWriteError};

/// Every bound settings section, as one snapshot. Each class is bound from the configuration
/// section of its name; the section names are the field names in PascalCase.
#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct AppSettings {
    pub subsonic: SubsonicSettings,
    pub downloads: DownloadSettings,
    pub lidarr: LidarrSettings,
    pub last_fm: LastFmSettings,
    pub radio_sources: RadioSourceSettings,
    pub genre: GenreSettings,
    pub library_actions: LibraryActionSettings,
    pub generated_playlists: GeneratedPlaylistSettings,
    pub notifications: NotificationSettings,
    pub metadata: MetadataSettings,
    pub server: ServerSettings,
    pub listen_brainz: ListenBrainzSettings,
    pub updates: UpdateSettings,
    pub admin: AdminSettings,
    pub imports: ImportSettings,
    pub family: FamilySettings,
}

impl AppSettings {
    /// The configuration section names, in registration order.
    pub const SECTIONS: [&'static str; 16] = [
        "Genre",
        "LibraryActions",
        "GeneratedPlaylists",
        "Subsonic",
        "Downloads",
        "Lidarr",
        "LastFm",
        "RadioSources",
        "Notifications",
        "Metadata",
        "Server",
        "ListenBrainz",
        "Updates",
        "Admin",
        "Imports",
        "Family",
    ];

    /// Binds every section from the merged configuration. A value that does not convert
    /// keeps its default (or a lower layer's value) and comes back as a warning whose path
    /// starts with the section name.
    pub fn bind(tree: &ConfigTree) -> (AppSettings, Vec<BindWarning>) {
        let mut warnings = Vec::new();
        let mut settings = AppSettings {
            genre: bind_section(tree, "Genre", &mut warnings),
            library_actions: bind_section(tree, "LibraryActions", &mut warnings),
            generated_playlists: bind_section(tree, "GeneratedPlaylists", &mut warnings),
            subsonic: bind_section(tree, "Subsonic", &mut warnings),
            downloads: bind_section(tree, "Downloads", &mut warnings),
            lidarr: bind_section(tree, "Lidarr", &mut warnings),
            last_fm: bind_section(tree, "LastFm", &mut warnings),
            radio_sources: bind_section(tree, "RadioSources", &mut warnings),
            notifications: bind_section(tree, "Notifications", &mut warnings),
            metadata: bind_section(tree, "Metadata", &mut warnings),
            server: bind_section(tree, "Server", &mut warnings),
            listen_brainz: bind_section(tree, "ListenBrainz", &mut warnings),
            updates: bind_section(tree, "Updates", &mut warnings),
            admin: bind_section(tree, "Admin", &mut warnings),
            imports: bind_section(tree, "Imports", &mut warnings),
            family: bind_section(tree, "Family", &mut warnings),
        };
        // Navidrome's address is read without the spaces and trailing slashes it may have been
        // typed with, so every upstream URL Octo builds from it has one slash (a trailing slash
        // would make the stream relay ask for `//rest/stream`).
        if let Some(url) = settings.subsonic.url.as_mut() {
            *url = normalize_server_url(url);
        }
        (settings, warnings)
    }
}

/// A server address as Octo keeps it: trimmed, with no trailing slash (`http://nd:4533/ ` is
/// `http://nd:4533`).
pub fn normalize_server_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// The config section `name` bound into `T`, collecting conversion warnings under the
/// section's path. The dashboard reads `Updates` this way on every request.
pub fn bind_section<T: DeserializeOwned + Default>(
    tree: &ConfigTree,
    name: &str,
    warnings: &mut Vec<BindWarning>,
) -> T {
    let (value, w) = bind::<T>(&tree.section(name));
    warnings.extend(w.into_iter().map(|w| BindWarning {
        path: if w.path.is_empty() {
            name.to_string()
        } else {
            format!("{name}:{}", w.path)
        },
        message: w.message,
    }));
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navidromes_address_is_read_without_trailing_slashes_or_spaces() {
        let mut tree = ConfigTree::new();
        tree.set("Subsonic:Url", Some(" http://navidrome:4533// ".into()));
        let (settings, warnings) = AppSettings::bind(&tree);
        assert!(warnings.is_empty());
        assert_eq!(settings.subsonic.url.as_deref(), Some("http://navidrome:4533"));
        assert_eq!(
            normalize_server_url("http://nd:4533/base/"),
            "http://nd:4533/base"
        );
        assert_eq!(normalize_server_url(""), "");
    }

    /// The Downloads section binds by its name, like every other section.
    #[test]
    fn the_downloads_section_binds_by_its_name() {
        let mut tree = ConfigTree::new();
        tree.set("Downloads:ParallelDownloads", Some("5".into()));
        let (settings, warnings) = AppSettings::bind(&tree);
        assert!(warnings.is_empty());
        assert_eq!(settings.downloads.parallel_downloads, 5);
    }
}
