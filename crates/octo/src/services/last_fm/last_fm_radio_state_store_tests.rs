//! The Last.fm radio state store, and the fixture through the store.

use std::sync::Arc;

use chrono::{TimeDelta, Utc};
use octo_core::models::radio::{LastFmRadioStateDocument, LastFmRadioUserState};
use octo_core::settings::{AppSettings, LastFmSettings};
use tempfile::TempDir;

use super::*;

pub(crate) fn play(artist: &str, title: &str, at: Option<DateTime<Utc>>) -> LastFmRadioPlay {
    LastFmRadioPlay {
        artist: artist.into(),
        title: title.into(),
        played_at_utc: at.unwrap_or_else(Utc::now),
        ..Default::default()
    }
}

pub(crate) fn settings_store(last_fm: LastFmSettings) -> Arc<SettingsStore> {
    Arc::new(SettingsStore::for_tests(AppSettings {
        last_fm,
        ..Default::default()
    }))
}

struct StateFixture {
    _dir: TempDir,
    path: PathBuf,
    store: LastFmRadioStateStore,
}

impl StateFixture {
    fn new(json: Option<&str>, settings: Option<LastFmSettings>) -> Self {
        let dir = TempDir::new().expect("a temp dir");
        let path = dir.path().join("state.json");
        if let Some(json) = json {
            std::fs::write(&path, json).expect("writes");
        }
        let store = LastFmRadioStateStore::new(
            path.clone(),
            settings_store(settings.unwrap_or_default()),
            Arc::new(ExternalIdRegistry::in_memory()),
        );
        StateFixture {
            _dir: dir,
            path,
            store,
        }
    }
}

fn document_with(user: LastFmRadioUserState) -> String {
    let mut document = LastFmRadioStateDocument::default();
    document.users.insert("alice".into(), user);
    octo_core::json::to_string(&document)
}

#[test]
fn missing_and_corrupt_files_recover_and_persist_atomically() {
    let fixture = StateFixture::new(Some("not json"), None);
    assert!(fixture.store.known_users().is_empty());
    assert!(fixture.store.record_play("alice", play("A", "One", None)));
    assert!(fixture.path.exists());
    assert!(!state_file::tmp_path(&fixture.path).exists());
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&fixture.path).unwrap()).unwrap();
    assert_eq!(written["Version"], CURRENT_VERSION);
}

#[test]
fn plays_are_deduplicated_and_isolated_per_user_even_when_disabled() {
    let fixture = StateFixture::new(
        None,
        Some(LastFmSettings {
            enable_radio: false,
            ..Default::default()
        }),
    );
    assert!(fixture.store.record_play("alice", play("Artist", "Title", None)));
    assert!(!fixture.store.record_play("alice", play("Artist", "Title", None)));
    assert!(fixture.store.record_play("bob", play("Artist", "Title", None)));
    assert_eq!(fixture.store.get_user("alice").plays.len(), 1);
    assert_eq!(fixture.store.get_user("bob").plays.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_do_not_lose_distinct_plays() {
    let fixture = Arc::new(StateFixture::new(None, None));
    let tasks: Vec<_> = (0..30)
        .map(|index| {
            let fixture = fixture.clone();
            tokio::task::spawn_blocking(move || {
                fixture
                    .store
                    .record_play("alice", play("Artist", &format!("Title {index}"), None))
            })
        })
        .collect();
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(fixture.store.get_user("alice").plays.len(), 30);
}

#[test]
fn load_prunes_old_and_over_bound_history() {
    let plays = (0..2100)
        .map(|index| {
            play(
                "A",
                &format!("T{index}"),
                Some(Utc::now() - TimeDelta::minutes(index)),
            )
        })
        .collect();
    let json = document_with(LastFmRadioUserState {
        username: "alice".into(),
        plays,
        ..Default::default()
    });
    let fixture = StateFixture::new(Some(&json), None);
    assert_eq!(fixture.store.get_user("alice").plays.len(), 2000);
}

#[test]
fn unsupported_version_starts_clean_and_retention_drops_expired_plays() {
    let unsupported = StateFixture::new(
        Some(r#"{"Version":99,"Users":{"alice":{"Username":"alice"}}}"#),
        None,
    );
    assert!(unsupported.store.known_users().is_empty());

    let json = document_with(LastFmRadioUserState {
        username: "alice".into(),
        plays: vec![
            play("A", "old", Some(Utc::now() - TimeDelta::days(30))),
            play("A", "new", None),
        ],
        ..Default::default()
    });
    let retained = StateFixture::new(
        Some(&json),
        Some(LastFmSettings {
            history_retention_days: 7,
            ..Default::default()
        }),
    );
    let plays = retained.store.get_user("alice").plays;
    assert_eq!(plays.len(), 1);
    assert_eq!(plays[0].title, "new");
}

#[test]
fn external_routes_are_rehydrated_after_restart() {
    let fixture = StateFixture::new(None, None);
    let station = LastFmRadioStation {
        id: station_id("alice", "mix"),
        name: "Mix".into(),
        owner: "alice".into(),
        tracks: vec![LastFmRadioTrack {
            artist: "A".into(),
            title: "T".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    fixture.store.replace_stations("alice", &[station]);
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let restarted = LastFmRadioStateStore::new(
        fixture.path.clone(),
        settings_store(LastFmSettings::default()),
        registry.clone(),
    );
    let user = restarted.get_user("alice");
    assert_eq!(user.stations.len(), 1);
    assert_eq!(user.stations[0].tracks.len(), 1);
    let track = &user.stations[0].tracks[0];
    let id = track.resolved_id.as_deref().expect("a route id");
    assert!(registry.lookup(id).is_some());
    assert_eq!(track.external_provider.as_deref(), Some("outside"));
}

#[test]
fn installing_identical_snapshot_preserves_created_and_changed_metadata() {
    let fixture = StateFixture::new(None, None);
    let mut station = LastFmRadioStation {
        id: station_id("alice", "mix"),
        name: "Mix".into(),
        owner: "alice".into(),
        created_utc: Utc::now() - TimeDelta::days(2),
        changed_utc: Utc::now() - TimeDelta::days(1),
        tracks: vec![LastFmRadioTrack {
            artist: "A".into(),
            title: "T".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    fixture
        .store
        .replace_stations("alice", std::slice::from_ref(&station));
    station.created_utc = Utc::now();
    station.changed_utc = Utc::now();
    fixture
        .store
        .replace_stations("alice", std::slice::from_ref(&station));
    let user = fixture.store.get_user("alice");
    assert_eq!(user.stations.len(), 1);
    let installed = &user.stations[0];
    assert_eq!(
        (station.created_utc - TimeDelta::days(2)).date_naive(),
        installed.created_utc.date_naive()
    );
    assert!(installed.changed_utc < Utc::now() - TimeDelta::hours(23));
}

#[test]
fn reject_track_removes_it_from_every_station_and_persists_cooldown() {
    let fixture = StateFixture::new(None, None);
    let bad = LastFmRadioTrack {
        artist: "Bad Artist".into(),
        title: "Bad Song".into(),
        ..Default::default()
    };
    let other = |artist: &str, title: &str| LastFmRadioTrack {
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    };
    fixture.store.replace_stations(
        "alice",
        &[
            LastFmRadioStation {
                id: "one".into(),
                tracks: vec![bad.clone(), other("A", "T")],
                ..Default::default()
            },
            LastFmRadioStation {
                id: "two".into(),
                tracks: vec![bad.clone(), other("B", "U")],
                ..Default::default()
            },
        ],
    );

    assert_eq!(fixture.store.reject_track("alice", &bad, None), 2);
    let user = fixture.store.get_user("alice");
    assert!(
        user.stations
            .iter()
            .all(|station| station.tracks.iter().all(|track| track.title != "Bad Song"))
    );
    assert_eq!(user.unavailable_tracks.len(), 1);
    let unavailable = &user.unavailable_tracks[0];
    assert_eq!(
        seed_normalizer::track_key("Bad Artist", "Bad Song"),
        unavailable.key
    );
    assert!(unavailable.retry_after_utc > unavailable.failed_at_utc);
}

/// The store writes the document indented, `Version` then `Users`, and keeps a user's key as
/// it was written; a hand-edited key in another case is still found.
#[test]
fn the_store_keeps_keys_and_finds_them_ignoring_case() {
    let at = Utc::now();
    let json = format!(
        r#"{{"Version":1,"Users":{{"Brandon":{{"Username":"Brandon","LastSeenUtc":"{}"}}}}}}"#,
        octo_core::json::datetime::format_utc(&at)
    );
    let fixture = StateFixture::new(Some(&json), None);
    assert_eq!(fixture.store.get_user(" BRANDON ").username, "Brandon");
    fixture.store.mark_refreshing("brandon");
    let written = std::fs::read_to_string(&fixture.path).unwrap();
    assert!(
        written.starts_with("{\n  \"Version\": 1,\n  \"Users\": {\n    \"Brandon\": {"),
        "{written}"
    );
    assert!(fixture.store.reset("BRANDON"));
    assert!(fixture.store.known_users().is_empty());
}

/// Two keys that differ only in case fail the load, so it starts clean.
#[test]
fn keys_that_differ_only_in_case_start_clean() {
    let fixture = StateFixture::new(
        Some(r#"{"Version":1,"Users":{"a":{"Username":"a"},"A":{"Username":"A"}}}"#),
        None,
    );
    assert!(fixture.store.known_users().is_empty());
}

/// A failed refresh keeps its first 500 characters, and the summaries come newest first.
#[test]
fn failures_are_bounded_and_summaries_are_newest_first() {
    let fixture = StateFixture::new(None, None);
    fixture.store.mark_refresh_failed("alice", &"x".repeat(600));
    std::thread::sleep(std::time::Duration::from_millis(2));
    fixture.store.mark_refreshing("bob");
    fixture.store.record_play("bob", play("A", "T", None));
    let summaries = fixture.store.get_summaries();
    assert_eq!(summaries[0].username, "bob");
    assert_eq!(summaries[0].play_count, 1);
    assert!(summaries[0].refreshing);
    assert_eq!(
        summaries[1].last_refresh_error.as_deref().map(str::len),
        Some(500)
    );
    assert!(fixture.store.mark_heart("bob", "", "A", "T"));
    assert!(!fixture.store.mark_heart("bob", "", "A", "T"));
    assert!(fixture.store.get_user("bob").plays[0].hearted);
}

/// The fixture through the store: loading and saving it again with nothing pruned (a clock on
/// the fixture's day, a long retention) gives the same file back. The resolved id of the
/// non-local track is the registry's own.
#[test]
fn the_fixture_survives_a_load_and_a_save() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/rust-migration/fixtures/state/lastfm-radio-state.json"
    ))
    .unwrap();
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("lastfm-radio-state.json");
    std::fs::write(&path, &text).unwrap();
    let at = chrono::DateTime::parse_from_rfc3339("2026-10-03T19:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let registry = Arc::new(ExternalIdRegistry::in_memory());
    let store = LastFmRadioStateStore::with_clock(
        path.clone(),
        settings_store(LastFmSettings {
            history_retention_days: 365,
            ..Default::default()
        }),
        registry.clone(),
        Clock::fixed(at),
    );
    assert!(!store.mark_heart("brandon", "4Kq3cS0bWq9dHq7y1nZb2e", "", ""));
    assert!(store.mark_heart("brandon", "1aB2cD3eF4gH5iJ6kL7mN8", "", ""));
    let written = std::fs::read_to_string(&path).unwrap();
    let minted = store.get_user("brandon").stations[0].tracks[0]
        .resolved_id
        .clone()
        .unwrap();
    let expected = text.replace("7YzXwVu6TsRq5PoN4mLk3J", &minted).replace(
        "\"Hearted\": false,\n          \"LearnedSignal\": false",
        "\"Hearted\": true,\n          \"LearnedSignal\": false",
    );
    assert_eq!(written, expected);
}

#[test]
fn playlist_serializer_has_json_xml_symmetry_and_read_only_metadata() {
    use octo_core::models::domain::Song;

    use crate::services::subsonic::subsonic_response_builder::new_subsonic_response_builder;

    let builder = new_subsonic_response_builder(Arc::new(ExternalIdRegistry::in_memory()));
    let station = LastFmRadioStation {
        id: station_id("alice", "mix"),
        key: "mix".into(),
        name: "Your Mix".into(),
        owner: "alice".into(),
        personalized: true,
        created_utc: Utc::now(),
        changed_utc: Utc::now(),
        valid_until_utc: Utc::now() + TimeDelta::hours(12),
        tracks: vec![LastFmRadioTrack {
            artist: "Artist".into(),
            title: "Title".into(),
            duration: Some(200),
            ..Default::default()
        }],
        ..Default::default()
    };
    let song = Song {
        id: "local-1".into(),
        artist: "Artist".into(),
        title: "Title".into(),
        album: "Album".into(),
        duration: Some(200),
        is_local: true,
        ..Default::default()
    };
    let json: serde_json::Value = serde_json::from_str(
        &builder
            .create_radio_playlist_response("json", &station, std::slice::from_ref(&song))
            .text(),
    )
    .unwrap();
    let playlist = &json["subsonic-response"]["playlist"];
    assert_eq!(playlist["readonly"], true);
    assert_eq!(playlist["entry"][0]["id"], "local-1");
    let xml = builder
        .create_radio_playlist_response("xml", &station, std::slice::from_ref(&song))
        .text()
        .into_owned();
    let element = xml.lines().nth(1).unwrap();
    assert!(element.contains(r#"readonly="true""#), "{element}");
    let child = xml.lines().nth(2).unwrap();
    assert!(child.contains(r#"id="local-1""#), "{child}");
}
