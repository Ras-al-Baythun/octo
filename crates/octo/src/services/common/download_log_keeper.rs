//! Keeps each finished download's log with its fetched-songs entry.

use std::sync::{Arc, Weak};

use tracing::debug;

use super::acquisition_tracker::{AcquisitionState, AcquisitionTracker};
use crate::services::local::DownloadHistoryService;

/// Saves each finished download's log with its entry in the fetched-songs log, so the dashboard
/// can show how an older download went after the live list has forgotten it (three hours). A
/// line that arrives after the end (lyrics, an upgrade's verdict) saves the log again. Only
/// watches: a save that fails is logged and never reaches the download.
pub struct DownloadLogKeeper {
    /// Weak: the tracker holds the keeper's listeners, so a strong reference back would keep both
    /// alive for good.
    tracker: Weak<AcquisitionTracker>,
    history: Arc<DownloadHistoryService>,
}

impl DownloadLogKeeper {
    /// Starts saving the logs of this tracker's finished downloads into this history.
    pub fn attach(
        tracker: &Arc<AcquisitionTracker>,
        history: Arc<DownloadHistoryService>,
    ) -> Arc<DownloadLogKeeper> {
        let keeper = Arc::new(DownloadLogKeeper {
            tracker: Arc::downgrade(tracker),
            history,
        });
        let on_end = keeper.clone();
        tracker.subscribe_ended(Arc::new(move |end| {
            if end.done {
                on_end.save(&end.key);
            }
        }));
        let after_end = keeper.clone();
        tracker.subscribe_logged_after_end(Arc::new(move |key| after_end.save(key)));
        keeper
    }

    /// Saves the log of the row with this key, when it ended in the library.
    pub fn save(&self, key: &str) {
        let Some(tracker) = self.tracker.upgrade() else {
            return;
        };
        let Some(row) = tracker.detail(key, None) else {
            return;
        };
        let Some(events) = row.events.filter(|events| !events.is_empty()) else {
            return;
        };
        if row.state != AcquisitionState::Done {
            return;
        }
        if !self.history.attach_log(key, row.started_at, &events) {
            debug!("No fetched-songs entry for {key} to keep its log with");
        }
    }
}

#[cfg(test)]
mod tests {
    use octo_core::common::Clock;
    use octo_core::models::download::{AcquisitionCandidate, DownloadHistoryEntry};

    use super::*;
    use crate::services::common::acquisition_tracker::acquisition_event_kinds;

    /// Now, in round-trip form with seven fractional digits.
    fn now_text() -> String {
        let now = chrono::Utc::now();
        format!(
            "{}.{:07}Z",
            now.format("%Y-%m-%dT%H:%M:%S"),
            now.timestamp_subsec_nanos() / 100
        )
    }

    #[test]
    fn a_finished_downloads_log_is_saved_and_a_line_after_the_end_saves_it_again_and_a_failure_saves_nothing()
    {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("downloads-history.json");
        let history = Arc::new(DownloadHistoryService::new(&path));
        let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
        DownloadLogKeeper::attach(&tracker, history.clone());

        tracker.begin(
            "outside",
            "song-1",
            Some("song-1"),
            Some("alice"),
            Some("Daft Punk"),
            Some("Digital Love"),
            None,
        );
        tracker.found(
            "outside",
            "song-1",
            "Found 2 copies",
            vec![
                AcquisitionCandidate {
                    source: "Lidarr".into(),
                    rank: Some(1),
                    ..Default::default()
                },
                AcquisitionCandidate {
                    source: "Lidarr".into(),
                    note: Some("Too short".into()),
                    ..Default::default()
                },
            ],
            None,
        );
        tracker.trying(
            "outside",
            "song-1",
            AcquisitionCandidate {
                source: "Lidarr".into(),
                format: Some("flac".into()),
                ..Default::default()
            },
            "Trying the first copy",
            Some("The best copy"),
        );
        history.record(DownloadHistoryEntry {
            artist: "Daft Punk".into(),
            title: "Digital Love".into(),
            downloaded_at: now_text(),
            key: Some("outside:song-1".into()),
            ..Default::default()
        });
        tracker.complete("outside", "song-1", Some("nd-1"));

        let log = history.get_recent(200)[0].log.clone().expect("a saved log");
        assert_eq!(log[log.len() - 1].kind, acquisition_event_kinds::DONE);
        assert!(log.iter().any(|line| line.kind == acquisition_event_kinds::FOUND
            && line.candidates.as_ref().map(Vec::len) == Some(2)));

        tracker.log(
            "outside",
            "song-1",
            acquisition_event_kinds::LYRICS,
            "Found word-timed lyrics",
            None,
        );
        let reopened = DownloadHistoryService::new(&path).get_recent(200);
        let log = reopened[0].log.clone().expect("a saved log");
        assert_eq!(log[log.len() - 1].text, "Found word-timed lyrics");

        tracker.begin(
            "outside",
            "song-2",
            Some("song-2"),
            Some("alice"),
            Some("Daft Punk"),
            Some("Aerodynamic"),
            None,
        );
        tracker.fail("outside", "song-2", Some("No copy that fits was found."));
        assert_eq!(DownloadHistoryService::new(&path).get_recent(200).len(), 1);
    }
}
