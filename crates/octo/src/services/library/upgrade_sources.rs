use std::sync::Arc;

use octo_core::common::dotnet;
use octo_core::settings::{DownloadSource, LidarrSettings, SettingsStore, UpgradeSourceChoice};

/// Where Better quality looks for a lossless copy, decided in one place so the dashboard, the
/// apps, the queue and the weekly worker never disagree. Only lossless sources count: YouTube can
/// only produce an MP3, which a Better quality check refuses anyway.
pub struct UpgradeSources {
    /// The library action and Lidarr settings, read at every use.
    settings: Arc<SettingsStore>,
}

impl UpgradeSources {
    pub fn new(settings: Arc<SettingsStore>) -> Self {
        UpgradeSources { settings }
    }

    /// Lidarr's address, key and the choices it needs to add an album are set.
    pub fn lidarr_set_up(settings: &LidarrSettings) -> bool {
        !dotnet::is_null_or_white_space(settings.base_url.as_deref())
            && !dotnet::is_null_or_white_space(settings.api_key.as_deref())
            && !dotnet::is_null_or_white_space(settings.root_folder_path.as_deref())
            && settings.quality_profile_id > 0
            && settings.metadata_profile_id > 0
    }

    /// The sources the setting allows, in the order they are tried, set up or not. Every choice
    /// comes to Lidarr, the one lossless source.
    pub fn wanted(&self) -> Vec<DownloadSource> {
        match self.settings.current().library_actions.upgrade_source {
            UpgradeSourceChoice::Auto | UpgradeSourceChoice::Lidarr => vec![DownloadSource::Lidarr],
        }
    }

    /// The allowed sources that are set up, in the order they are tried.
    pub fn plan(&self) -> Vec<DownloadSource> {
        self.wanted().into_iter().filter(|s| self.set_up(*s)).collect()
    }

    /// Whether any allowed source is set up, so Better quality can be offered at all.
    pub fn ready(&self) -> bool {
        !self.plan().is_empty()
    }

    /// The sources in words, for "Looking for a higher quality copy on X": the ones that are set
    /// up, or, when none is, the ones the setting asks for.
    pub fn name(&self) -> String {
        let plan = self.plan();
        Self::words(if plan.is_empty() { self.wanted() } else { plan })
    }

    pub fn words(sources: impl IntoIterator<Item = DownloadSource>) -> String {
        let mut words: Vec<&str> = Vec::new();
        for source in sources {
            let word = Self::word(source);
            if !words.contains(&word) {
                words.push(word);
            }
        }
        words.join(" or ")
    }

    pub fn word(source: DownloadSource) -> &'static str {
        match source {
            DownloadSource::Lidarr => "Lidarr",
            DownloadSource::YouTube => "YouTube",
        }
    }

    fn set_up(&self, source: DownloadSource) -> bool {
        let settings = self.settings.current();
        match source {
            DownloadSource::Lidarr => Self::lidarr_set_up(&settings.lidarr),
            DownloadSource::YouTube => false,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Where Better quality looks for a lossless copy: Lidarr, when it is set up. One place
    //! decides it, so the dashboard, the apps and the workers never disagree.

    use octo_core::settings::{AppSettings, LibraryActionSettings};

    use super::*;

    fn lidarr() -> LidarrSettings {
        LidarrSettings {
            base_url: Some("http://lidarr:8686".into()),
            api_key: Some("k".into()),
            root_folder_path: Some("/music".into()),
            quality_profile_id: 1,
            metadata_profile_id: 1,
            ..Default::default()
        }
    }

    fn sources(choice: UpgradeSourceChoice, lidarr: Option<LidarrSettings>) -> UpgradeSources {
        UpgradeSources::new(Arc::new(SettingsStore::for_tests(AppSettings {
            library_actions: LibraryActionSettings {
                upgrade_source: choice,
                ..Default::default()
            },
            lidarr: lidarr.unwrap_or_default(),
            ..Default::default()
        })))
    }

    #[test]
    fn auto_uses_lidarr_when_it_is_set_up() {
        let sources = sources(UpgradeSourceChoice::Auto, Some(lidarr()));

        assert_eq!(sources.plan(), [DownloadSource::Lidarr]);
        assert!(sources.ready());
        assert_eq!(sources.name(), "Lidarr");
    }

    #[test]
    fn a_chosen_source_is_the_only_one() {
        assert_eq!(
            sources(UpgradeSourceChoice::Lidarr, Some(lidarr())).plan(),
            [DownloadSource::Lidarr]
        );
    }

    #[test]
    fn nothing_set_up_is_not_ready_and_names_what_it_wants() {
        for choice in [UpgradeSourceChoice::Auto, UpgradeSourceChoice::Lidarr] {
            let sources = sources(choice, None);

            assert!(sources.plan().is_empty());
            assert!(!sources.ready());
            assert_eq!(sources.name(), "Lidarr");
        }
    }

    #[test]
    fn lidarr_needs_its_root_folder_and_profiles_to_add_an_album() {
        let half = LidarrSettings {
            base_url: Some("http://lidarr:8686".into()),
            api_key: Some("k".into()),
            ..Default::default()
        };

        assert!(!UpgradeSources::lidarr_set_up(&half));
        assert!(UpgradeSources::lidarr_set_up(&lidarr()));
    }

    #[test]
    fn youtube_is_never_a_lossless_source() {
        assert_eq!(UpgradeSources::word(DownloadSource::YouTube), "YouTube");
        assert_eq!(
            UpgradeSources::words([DownloadSource::Lidarr, DownloadSource::Lidarr]),
            "Lidarr"
        );
    }
}
