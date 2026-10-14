//! The handoff of files a Lidarr heart imported to the download pipeline.

use std::collections::HashMap;

use parking_lot::Mutex;

/// One file a Lidarr heart brought in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LidarrImport {
    /// Where Lidarr put the file, as Octo sees it.
    pub path: String,
    /// No notice for this one: the album's other songs, or an album heart, which gets one notice
    /// for the whole album instead.
    pub quiet: bool,
}

/// The files a Lidarr heart brought in, waiting for the download pipeline to take them by the
/// song's external id. Lidarr is then only how the file was found: the pipeline checks it with
/// AcoustID and the spectrum, identifies, tags, places and records it exactly as it does a
/// YouTube download.
#[derive(Default)]
pub struct LidarrImportHandoff {
    offers: Mutex<HashMap<String, LidarrImport>>,
}

impl LidarrImportHandoff {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn offer(&self, external_id: &str, path: &str, quiet: bool) {
        self.offers.lock().insert(
            external_id.to_string(),
            LidarrImport {
                path: path.to_string(),
                quiet,
            },
        );
    }

    /// The file for this song, once; None when none is waiting.
    pub fn take(&self, external_id: &str) -> Option<LidarrImport> {
        self.offers.lock().remove(external_id)
    }

    /// Takes an offer back. True when it was still waiting, so the pipeline never used it.
    pub fn withdraw(&self, external_id: &str) -> bool {
        self.offers.lock().remove(external_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offer_is_taken_once_and_by_exact_id() {
        let handoff = LidarrImportHandoff::new();
        handoff.offer("abc", "/music/a.flac", true);
        assert!(handoff.take("ABC").is_none());
        assert_eq!(
            handoff.take("abc"),
            Some(LidarrImport {
                path: "/music/a.flac".into(),
                quiet: true
            })
        );
        assert!(handoff.take("abc").is_none());
        handoff.offer("abc", "/music/a.flac", false);
        assert!(handoff.withdraw("abc"));
        assert!(!handoff.withdraw("abc"));
    }
}
