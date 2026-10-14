//! octoLibraryActions version 3, as the apps' Library health uses it. Removing needs an admin and
//! can be put back from the trash, tags are written in place and can be undone, a split album is
//! joined by copying the album's own tags, and a cover goes only into a file that has none. Real
//! files in a temporary music folder, a real resolver and quarantine, Navidrome faked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use chrono::Utc;
use octo_core::models::domain::Song;
use octo_core::settings::{
    AppSettings, LibraryAction, LibraryActionDefinition, LibraryActionSettings, SubsonicSettings,
};
use octo_media::tags::TagFile;
use octo_media::tags::kept_identity::{KeptIdentity, KeptIdentityTags};
use octo_media::tags::tag_writer_extras::{self as extras, TagFields};
use parking_lot::Mutex;
use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request as MockRequest, Respond, ResponseTemplate};

use super::test_app::*;
use crate::app::{AppState, TestServices};
use crate::http::pipeline::App;
use crate::services::cover_art::{AlbumCoverQuery, FoundCover, IAlbumCoverFinder};
use crate::services::library::library_tag_edits::{self as tag_edits, TagValues};
use crate::services::library::{LibraryActionState, QuarantineManifest};

#[derive(Clone)]
struct FakeSong {
    id: String,
    relative_path: String,
    size: u64,
    title: String,
    artist: String,
    album: String,
    album_artist: Option<String>,
}

/// Navidrome as these calls need it. Each song's size is the one it had when Navidrome last
/// scanned, so after a write it reports the old size, as the real one does until it scans.
struct FakeNavidrome {
    library_path: String,
    admin: AtomicBool,
    songs: Mutex<HashMap<String, FakeSong>>,
}

struct Answers(Arc<FakeNavidrome>);

impl Respond for Answers {
    fn respond(&self, request: &MockRequest) -> ResponseTemplate {
        let fake = &self.0;
        let path = request.url.path();
        let query = query_of(request);
        if path.ends_with("/rest/ping") {
            return navidrome_json(if query.get("t").map(String::as_str) == Some("good") {
                NAVIDROME_OK
            } else {
                NAVIDROME_WRONG_PASSWORD
            });
        }
        if path.ends_with("/rest/getUser") {
            return navidrome_json(
                &json!({"subsonic-response": {"status": "ok", "version": "1.16.1", "user": {
                    "username": query.get("username"),
                    "adminRole": fake.admin.load(Ordering::SeqCst),
                }}})
                .to_string(),
            );
        }
        if path == "/auth/login" {
            return navidrome_json(
                r#"{"token":"admin-jwt","isAdmin":true,"username":"admin","subsonicToken":"st","subsonicSalt":"ss"}"#,
            );
        }
        if let Some(id) = path.strip_prefix("/api/song/")
            && let Some(song) = fake.songs.lock().get(id)
        {
            return navidrome_json(
                &json!({
                    "id": song.id, "path": song.relative_path, "libraryPath": fake.library_path,
                    "size": song.size, "suffix": "flac", "title": song.title, "artist": song.artist,
                    "album": song.album, "albumArtist": song.album_artist, "duration": 2,
                })
                .to_string(),
            );
        }
        if path.ends_with("/rest/startScan") {
            return navidrome_json(NAVIDROME_OK);
        }
        ResponseTemplate::new(404)
    }
}

/// `FixedFinder`: the same cover (or none) for every album, counting the asks.
struct FixedFinder {
    cover: Option<FoundCover>,
    asked: AtomicUsize,
}

#[async_trait]
impl IAlbumCoverFinder for FixedFinder {
    async fn find(&self, _query: &AlbumCoverQuery) -> Option<FoundCover> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.cover.clone()
    }
}

struct Factory {
    state: AppState,
    app: App,
    navidrome: Arc<FakeNavidrome>,
    finder: Arc<FixedFinder>,
    root: PathBuf,
    _server: MockServer,
    _dir: tempfile::TempDir,
}

impl Factory {
    async fn new() -> Factory {
        Self::with(|_| {}, None).await
    }

    async fn with(change: impl FnOnce(&mut AppSettings), cover: Option<FoundCover>) -> Factory {
        let dir = tempfile::tempdir().expect("a temp dir");
        let root = dir.path().join("music");
        std::fs::create_dir_all(&root).expect("the music folder");
        let navidrome = Arc::new(FakeNavidrome {
            library_path: root.to_string_lossy().into_owned(),
            admin: AtomicBool::new(true),
            songs: Mutex::new(HashMap::new()),
        });
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(Answers(Arc::clone(&navidrome)))
            .mount(&server)
            .await;
        let mut settings = AppSettings {
            subsonic: SubsonicSettings {
                url: Some(server.uri()),
                auto_detect_download_path: false,
                admin_username: Some("admin".into()),
                admin_password: Some("admin-password".into()),
                ..Default::default()
            },
            library_actions: LibraryActionSettings {
                enabled: true,
                dry_run: false,
                allowed_users: vec!["alice".into()],
                actions: vec![LibraryActionDefinition {
                    action: LibraryAction::Delete,
                    enabled: true,
                    ..Default::default()
                }],
                quarantine_retention_days: 14,
                ..Default::default()
            },
            ..Default::default()
        };
        change(&mut settings);
        let finder = Arc::new(FixedFinder {
            cover,
            asked: AtomicUsize::new(0),
        });
        let state = AppState::for_tests_with(
            settings,
            TestServices {
                config_dir: Some(dir.path().join("config")),
                raw_settings: vec![("Library:DownloadPath".into(), root.to_string_lossy().into_owned())],
                album_cover_finder: Some(finder.clone() as Arc<dyn IAlbumCoverFinder>),
                ..Default::default()
            },
        );
        Factory {
            app: app(&state),
            state,
            navidrome,
            finder,
            root,
            _server: server,
            _dir: dir,
        }
    }

    fn trash(&self) -> PathBuf {
        self.root.join(".octo-trash")
    }

    /// A FLAC in the music folder with these tags, known to Navidrome by id.
    #[allow(clippy::too_many_arguments)]
    fn song_with(
        &self,
        id: &str,
        relative: &str,
        title: &str,
        artist: &str,
        album: &str,
        album_artist: Option<&str>,
        year: u32,
        genre: Option<&str>,
    ) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().expect("a folder")).expect("folders");
        std::fs::write(&path, crate::services::test_support::flac()).expect("written");
        {
            let mut file = TagFile::open(&path).expect("opens");
            file.set_title(Some(title));
            file.set_performers(&[artist.to_string()]);
            file.set_album(Some(album));
            if let Some(album_artist) = album_artist {
                file.set_album_artists(&[album_artist.to_string()]);
            }
            file.set_year(year);
            if let Some(genre) = genre {
                file.set_genres(&[genre.to_string()]);
            }
            file.save().expect("saved");
        }
        self.navidrome.songs.lock().insert(
            id.to_string(),
            FakeSong {
                id: id.to_string(),
                relative_path: relative.to_string(),
                size: std::fs::metadata(&path).expect("written").len(),
                title: title.into(),
                artist: artist.into(),
                album: album.into(),
                album_artist: album_artist.map(str::to_string),
            },
        );
        path
    }

    fn song(&self, id: &str, relative: &str, title: &str, artist: &str, album: &str) -> PathBuf {
        self.song_with(id, relative, title, artist, album, None, 0, None)
    }

    /// Navidrome scanned the file again: it reports this size now.
    fn rescanned(&self, id: &str, size: u64) {
        if let Some(song) = self.navidrome.songs.lock().get_mut(id) {
            song.size = size;
        }
    }

    async fn get(&self, url: &str) -> Value {
        get(&self.app, url).await.json()
    }

    async fn call(&self, query: &str) -> Value {
        let doc = self
            .get(&format!("/rest/libraryAction.view?{AUTH}&{query}"))
            .await;
        let envelope = &doc["subsonic-response"];
        assert_eq!(envelope["status"], "ok", "{query}: {doc}");
        envelope["libraryAction"].clone()
    }

    fn removals(&self) -> Vec<crate::services::library::LibraryActionEntry> {
        self.state.library_action_journal.recent(200)
    }
}

const AUTH: &str = "u=alice&t=good&s=salt&v=1.16.1&c=octo";

fn state(action: &Value) -> &str {
    action["state"].as_str().expect("a state")
}

fn tags(path: &Path) -> TagValues {
    tag_edits::read(path).expect("the tags read")
}

fn tag(path: &Path, name: &str) -> Option<String> {
    tags(path).get(name).cloned().flatten()
}

fn kept(path: &Path) -> KeptIdentity {
    KeptIdentityTags::read(path, None).expect("the tags read")
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).expect("there").len()
}

// ---- Removing, the trash and putting back --------------------------------------------------

#[tokio::test]
async fn remove_someone_who_is_not_an_admin_is_turned_away_and_the_file_stays() {
    let f = Factory::new().await;
    let path = f.song(
        "s1",
        "Massive Attack/Mezzanine/Teardrop.flac",
        "Teardrop",
        "Massive Attack",
        "Mezzanine",
    );
    f.navidrome.admin.store(false, Ordering::SeqCst);

    let action = f.call("id=s1&action=remove").await;

    assert_eq!(state(&action), "skipped");
    assert!(action["detail"].as_str().expect("words").contains("admin"));
    assert!(path.exists());
    assert!(!f.trash().exists());
    assert!(f.removals().is_empty());

    let doc = f.get(&format!("/rest/getLibraryActions.view?{AUTH}")).await;
    assert_eq!(doc["subsonic-response"]["libraryActions"]["admin"], false);
}

#[tokio::test]
async fn remove_goes_to_the_trash_asks_for_a_scan_is_listed_and_can_be_put_back_then_removed_again() {
    let f = Factory::new().await;
    let path = f.song(
        "s1",
        "Massive Attack/Mezzanine/Teardrop.flac",
        "Teardrop",
        "Massive Attack",
        "Mezzanine",
    );
    let bytes = std::fs::read(&path).expect("there");

    assert_eq!(state(&f.call("id=s1&action=remove").await), "applied");
    assert!(!path.exists());
    assert_eq!(f.state.library_rescan.asked(), 1);

    let doc = f.get(&format!("/rest/getLibraryTrash.view?{AUTH}")).await;
    let trash = &doc["subsonic-response"]["libraryTrash"];
    assert_eq!(trash["keepDays"], 14);
    let songs = trash["songs"].as_array().expect("songs");
    assert_eq!(songs.len(), 1, "{doc}");
    assert_eq!(songs[0]["id"], "s1");
    assert_eq!(songs[0]["title"], "Teardrop");
    assert_eq!(songs[0]["removedBy"], "alice");
    assert!(songs[0]["goneAt"].is_string());

    let restored = f.call("id=s1&action=restore").await;
    assert_eq!(state(&restored), "applied", "{restored}");
    assert_eq!(std::fs::read(&path).expect("back"), bytes);
    let removals = f.removals();
    assert_eq!(removals.len(), 1);
    assert_eq!(removals[0].state, LibraryActionState::Restored);
    assert_eq!(f.state.library_rescan.asked(), 2);
    // Put back, so no longer a song the person said they never want again.
    assert!(
        !f.state
            .library_action_journal
            .is_never_requested(Some("Massive Attack"), Some("Teardrop"))
    );

    let doc = f.get(&format!("/rest/getLibraryTrash.view?{AUTH}")).await;
    assert_eq!(doc["subsonic-response"]["libraryTrash"]["songs"], json!([]));

    // The same file, the same size and time: a second remove is a real one, not "Already done".
    assert_eq!(state(&f.call("id=s1&action=remove").await), "applied");
    assert!(!path.exists());
}

#[tokio::test]
async fn remove_takes_the_songs_lyrics_with_it_and_put_back_returns_them_leaving_the_albums_cover() {
    let f = Factory::new().await;
    let path = f.song(
        "s1",
        "Daft Punk/Discovery/03 - Digital Love.flac",
        "Digital Love",
        "Daft Punk",
        "Discovery",
    );
    let folder = path.parent().expect("a folder").to_path_buf();
    let lyrics = folder.join("03 - Digital Love.lrc");
    let plain = folder.join("03 - Digital Love.txt");
    let cover = folder.join("cover.jpg");
    let other = folder.join("04 - Harder Better Faster Stronger.lrc");
    for file in [&lyrics, &plain, &cover, &other] {
        std::fs::write(
            file,
            file.file_name().expect("a name").to_string_lossy().as_bytes(),
        )
        .expect("written");
    }

    assert_eq!(state(&f.call("id=s1&action=remove").await), "applied");

    assert!(!lyrics.exists());
    assert!(!plain.exists());
    assert!(cover.exists());
    assert!(other.exists());
    let trashed = f.removals()[0].quarantine_path.clone().expect("in the trash");
    assert!(Path::new(&trashed).with_extension("lrc").exists());
    assert!(
        std::fs::read_to_string(format!("{trashed}.octo-action.json"))
            .expect("the note")
            .contains("03 - Digital Love.lrc")
    );

    assert_eq!(state(&f.call("id=s1&action=restore").await), "applied");

    assert_eq!(
        std::fs::read_to_string(&lyrics).expect("back"),
        "03 - Digital Love.lrc"
    );
    assert_eq!(
        std::fs::read_to_string(&plain).expect("back"),
        "03 - Digital Love.txt"
    );
    assert!(!Path::new(&trashed).with_extension("lrc").exists());
}

#[tokio::test]
async fn put_back_links_the_outside_song_to_the_file_again() {
    let f = Factory::new().await;
    let path = f.song(
        "s1",
        "Daft Punk/Discovery/03 - Digital Love.flac",
        "Digital Love",
        "Daft Punk",
        "Discovery",
    );
    let library = f.state.local_library.clone();
    library
        .register_downloaded_song(
            &Song {
                external_provider: Some("outside".into()),
                external_id: Some("4H0vAhIr2YJ67Riz3Uv6wG".into()),
                title: "Digital Love".into(),
                artist: "Daft Punk".into(),
                album: "Discovery".into(),
                source_peer: Some("peer1".into()),
                ..Song::default()
            },
            &path.to_string_lossy(),
        )
        .await
        .expect("registered");

    assert_eq!(state(&f.call("id=s1&action=remove").await), "applied");
    assert_eq!(
        library
            .get_local_path_for_external_song("outside", "4H0vAhIr2YJ67Riz3Uv6wG")
            .await,
        None
    );
    assert!(library.get_mappings().await.is_empty());
    // The link waits in the trash note beside the file, which is all Put back reads.
    let trashed = f.removals()[0].quarantine_path.clone().expect("in the trash");
    let note: QuarantineManifest = serde_json::from_str(
        &std::fs::read_to_string(format!("{trashed}.octo-action.json")).expect("the note"),
    )
    .expect("reads");
    let mappings = note.mappings.expect("the links");
    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].external_id, "4H0vAhIr2YJ67Riz3Uv6wG");

    assert_eq!(state(&f.call("id=s1&action=restore").await), "applied");

    assert_eq!(
        library
            .get_local_path_for_external_song("outside", "4H0vAhIr2YJ67Riz3Uv6wG")
            .await
            .as_deref(),
        Some(path.to_string_lossy().as_ref())
    );
    let mappings = library.get_mappings().await;
    assert_eq!(mappings.len(), 1);
    assert_eq!(mappings[0].source_peer.as_deref(), Some("peer1"));
}

#[tokio::test]
async fn removing_one_format_of_a_song_leaves_the_lyrics_the_other_copy_shares() {
    let f = Factory::new().await;
    let flac = f.song(
        "s1",
        "Daft Punk/Discovery/03 - Digital Love.flac",
        "Digital Love",
        "Daft Punk",
        "Discovery",
    );
    let folder = flac.parent().expect("a folder");
    std::fs::write(folder.join("03 - Digital Love.mp3"), "the other copy").expect("written");
    let lyrics = folder.join("03 - Digital Love.lrc");
    std::fs::write(&lyrics, "shared").expect("written");

    assert_eq!(state(&f.call("id=s1&action=remove&copy=true").await), "applied");

    assert!(lyrics.exists());
}

#[tokio::test]
async fn removing_a_second_copy_leaves_the_song_wanted_but_deleting_the_song_does_not() {
    let f = Factory::new().await;
    let copy = f.song(
        "copy",
        "Bon Iver/Holocene/Holocene.flac",
        "Holocene",
        "Bon Iver",
        "Holocene",
    );
    f.song(
        "song",
        "Bon Iver/Bon Iver/Towers.flac",
        "Towers",
        "Bon Iver",
        "Bon Iver",
    );

    let removed = f.call("id=copy&action=remove&copy=true").await;
    assert_eq!(state(&removed), "applied");
    assert!(
        removed["detail"]
            .as_str()
            .expect("words")
            .contains("stays in the library")
    );
    assert!(!copy.exists());
    // The kept copy can still be upgraded, or the song hearted, later.
    let journal = &f.state.library_action_journal;
    assert!(!journal.is_never_requested(Some("Bon Iver"), Some("Holocene")));

    assert_eq!(state(&f.call("id=song&action=remove").await), "applied");
    assert!(journal.is_never_requested(Some("Bon Iver"), Some("Towers")));
}

#[tokio::test]
async fn restore_a_song_not_in_the_trash_changes_nothing() {
    let f = Factory::new().await;
    f.song("s1", "A/B/Song.flac", "Song", "A", "B");

    assert_eq!(state(&f.call("id=s1&action=restore").await), "skipped");
}

#[tokio::test]
async fn restore_never_puts_a_file_outside_the_music_folder_whatever_its_manifest_says() {
    let f = Factory::new().await;
    f.song("s1", "A/B/Song.flac", "Song", "A", "B");
    assert_eq!(state(&f.call("id=s1&action=remove").await), "applied");

    // Someone edits the note beside the trashed file to point somewhere else.
    let trashed = f.removals()[0].quarantine_path.clone().expect("in the trash");
    let elsewhere = f._dir.path().join("elsewhere").join("Song.flac");
    let note = QuarantineManifest {
        original_path: elsewhere.to_string_lossy().into_owned(),
        navidrome_id: "s1".into(),
        action: "Delete".into(),
        username: "alice".into(),
        at_utc: Utc::now(),
        sidecars: None,
        mappings: None,
    };
    std::fs::write(
        format!("{trashed}.octo-action.json"),
        octo_core::json::to_string(&note),
    )
    .expect("written");

    let action = f.call("id=s1&action=restore").await;

    assert_eq!(state(&action), "failed");
    assert!(!elsewhere.exists());
    assert!(Path::new(&trashed).exists());
}

// ---- Tags ----------------------------------------------------------------------------------

#[tokio::test]
async fn retag_writes_only_what_was_sent_and_answers_before_and_after() {
    let f = Factory::new().await;
    let path = f.song_with(
        "s1",
        "Massive Attack/Mezzanine/Teardrop.flac",
        "Teardrop",
        "Massive Attack",
        "Mezzanine",
        None,
        1998,
        None,
    );

    let action = f.call("id=s1&action=retag&genre=Trip%20Hop&track=3").await;

    assert_eq!(state(&action), "applied", "{action}");
    assert_eq!(action["detail"], "Changed the genre and track number.");
    assert_eq!(action["before"]["genre"], Value::Null);
    assert_eq!(action["after"]["genre"], "Trip Hop");
    assert_eq!(tag(&path, "genre").as_deref(), Some("Trip Hop"));
    assert_eq!(tag(&path, "track").as_deref(), Some("3"));
    assert_eq!(tag(&path, "year").as_deref(), Some("1998"));
    assert_eq!(tag(&path, "title").as_deref(), Some("Teardrop"));
    assert_eq!(f.state.library_rescan.asked(), 1);
    assert_eq!(f.state.tag_edit_journal.recent(200).len(), 1);
}

#[tokio::test]
async fn retag_a_second_edit_before_navidrome_scans_still_finds_the_file_and_undo_puts_back_only_the_last() {
    let f = Factory::new().await;
    let path = f.song("s1", "A/B/Song.flac", "Song", "A", "B");

    assert_eq!(state(&f.call("id=s1&action=retag&year=2001").await), "applied");
    // A tag that outgrew the file's padding: Navidrome reports the size from before the write
    // until it scans, so the resolver no longer recognizes the file.
    f.rescanned("s1", size(&path) + 4096);
    assert_eq!(state(&f.call("id=s1&action=retag&genre=Rock").await), "applied");

    let undo = f.call("id=s1&action=undo").await;

    assert_eq!(state(&undo), "applied", "{undo}");
    assert_eq!(tag(&path, "genre"), None);
    assert_eq!(tag(&path, "year").as_deref(), Some("2001"));
}

#[tokio::test]
async fn undo_a_left_alone_file_changed_since_is_left_alone() {
    let f = Factory::new().await;
    let path = f.song("s1", "A/B/Song.flac", "Song", "A", "B");
    assert_eq!(state(&f.call("id=s1&action=retag&year=2001").await), "applied");

    {
        let mut file = TagFile::open(&path).expect("opens");
        file.set_genres(&["someone else was here".to_string()]);
        file.save().expect("saved");
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("opens")
        .set_modified(SystemTime::now() + Duration::from_secs(300))
        .expect("the time set");

    assert_eq!(state(&f.call("id=s1&action=undo").await), "failed");
    assert_eq!(tag(&path, "year").as_deref(), Some("2001"));
}

#[tokio::test]
async fn retag_a_stray_space_is_a_change_so_a_split_album_can_be_mended() {
    let f = Factory::new().await;
    let path = f.song("s1", "A/B/Song.flac", "Song", "A", "Mezzanine ");

    assert_eq!(
        state(&f.call("id=s1&action=retag&album=Mezzanine").await),
        "applied"
    );
    assert_eq!(tag(&path, "album").as_deref(), Some("Mezzanine"));
}

#[tokio::test]
async fn retag_a_value_a_tag_cannot_hold_writes_nothing() {
    for query in ["year=abc", "title=", "track=0", "isrc=nope"] {
        let f = Factory::new().await;
        let path = f.song_with("s1", "A/B/Song.flac", "Song", "A", "B", None, 1999, None);
        let before = std::fs::read(&path).expect("there");

        assert_eq!(
            state(&f.call(&format!("id=s1&action=retag&{query}")).await),
            "failed",
            "{query}"
        );
        assert_eq!(std::fs::read(&path).expect("there"), before, "{query}");
        assert!(f.state.tag_edit_journal.recent(200).is_empty(), "{query}");
    }
}

#[tokio::test]
async fn retag_in_a_dry_run_is_rehearsed_and_writes_nothing() {
    let f = Factory::with(|s| s.library_actions.dry_run = true, None).await;
    let path = f.song("s1", "A/B/Song.flac", "Song", "A", "B");
    let before = std::fs::read(&path).expect("there");

    assert_eq!(state(&f.call("id=s1&action=retag&year=2001").await), "rehearsed");
    assert_eq!(std::fs::read(&path).expect("there"), before);
}

#[tokio::test]
async fn retag_someone_not_on_the_allowed_list_is_turned_away() {
    let f = Factory::new().await;
    let path = f.song("s1", "A/B/Song.flac", "Song", "A", "B");
    let before = std::fs::read(&path).expect("there");

    let body = get(
        &f.app,
        "/rest/libraryAction.view?u=bob&t=good&s=salt&v=1.16.1&c=octo&id=s1&action=retag&year=2001",
    )
    .await
    .text();

    assert!(body.contains("\"skipped\""), "{body}");
    assert_eq!(std::fs::read(&path).expect("there"), before);
}

// ---- Albums --------------------------------------------------------------------------------

#[tokio::test]
async fn join_album_gives_the_song_the_leads_album_tags_and_year_keeps_its_own_title_and_undoes() {
    let f = Factory::new().await;
    let lead = f.song_with(
        "lead",
        "Massive Attack/Mezzanine/01 Angel.flac",
        "Angel",
        "Massive Attack",
        "Mezzanine",
        Some("Massive Attack"),
        1998,
        None,
    );
    let stray = f.song_with(
        "stray",
        "Massive Attack/Mezzanine (2)/03 Teardrop.flac",
        "Teardrop",
        "Massive Attack",
        "Mezzanine ",
        None,
        0,
        None,
    );

    let action = f.call("id=stray&action=joinAlbum&like=lead").await;

    assert_eq!(state(&action), "applied", "{action}");
    let joined = kept(&stray);
    let leading = kept(&lead);
    assert_eq!(joined.album, leading.album);
    assert_eq!(joined.album_artist, leading.album_artist);
    assert_eq!(joined.release_date, leading.release_date);
    assert_eq!(joined.title, "Teardrop");
    assert_eq!(tag(&stray, "year").as_deref(), Some("1998"));

    assert_eq!(state(&f.call("id=stray&action=undo").await), "applied");
    assert_eq!(kept(&stray).album.as_deref(), Some("Mezzanine "));
    assert_eq!(tag(&stray, "year"), None);
}

#[tokio::test]
async fn join_album_across_album_artists_gives_the_song_the_leads_album_artist_keeps_its_own_artist_and_undoes()
 {
    // One album filed under each of its artists, as the live library had it: Drake's songs on a
    // part of their own, with no year, beside PARTYNEXTDOOR's.
    let f = Factory::new().await;
    let lead = f.song_with(
        "lead",
        "PARTYNEXTDOOR/$ome $exy $ongs 4 U/CELIBACY.flac",
        "CELIBACY",
        "PARTYNEXTDOOR",
        "$ome $exy $ongs 4 U",
        Some("PARTYNEXTDOOR"),
        2025,
        None,
    );
    let stray = f.song_with(
        "stray",
        "Drake/$ome $exy $ongs 4 U/GIMME A HUG.flac",
        "GIMME A HUG",
        "Drake",
        "$ome $exy $ongs 4 U",
        Some("Drake"),
        0,
        None,
    );

    assert_eq!(
        state(&f.call("id=stray&action=joinAlbum&like=lead").await),
        "applied"
    );

    let joined = kept(&stray);
    let leading = kept(&lead);
    assert_eq!(joined.album_artist, leading.album_artist);
    assert_eq!(joined.album_artists, leading.album_artists);
    let untitled = |identity: &KeptIdentity| {
        KeptIdentityTags::pid_inputs(&KeptIdentity {
            title: String::new(),
            ..identity.clone()
        })
    };
    assert_eq!(untitled(&leading), untitled(&joined));
    assert_eq!(tag(&stray, "artist").as_deref(), Some("Drake"));
    assert_eq!(tag(&stray, "year").as_deref(), Some("2025"));

    assert_eq!(state(&f.call("id=stray&action=undo").await), "applied");
    assert_eq!(kept(&stray).album_artist, ["Drake"]);
    assert_eq!(tag(&stray, "year"), None);
}

#[tokio::test]
async fn join_album_takes_away_a_peers_stray_album_id_release_date_and_version_when_the_album_has_none() {
    let f = Factory::new().await;
    let lead = f.song_with(
        "lead",
        "Uzi/Eternal Atake/01 Baby Pluto.flac",
        "Baby Pluto",
        "Lil Uzi Vert",
        "Eternal Atake",
        Some("Lil Uzi Vert"),
        2020,
        None,
    );
    let stray = f.song_with(
        "stray",
        "Uzi/Eternal Atake/02 Lo Mein.flac",
        "Lo Mein",
        "Lil Uzi Vert",
        "Eternal Atake",
        Some("Lil Uzi Vert"),
        2020,
        None,
    );
    // What a release's tagger left on one song, and its album-mates lack: Navidrome groups
    // albums by these, so this song shows as an album of its own.
    {
        let mut file = TagFile::open(&stray).expect("opens");
        extras::set_text(
            &mut file,
            TagFields::ALBUM_ID,
            Some("4c6d8f8a-77a5-4a3b-9b49-6ac41a2b3e2c"),
        );
        extras::set_text(&mut file, TagFields::RELEASE_DATE, Some("2020-03-06"));
        extras::set_exact(&mut file, TagFields::ALBUM_VERSION, &["Deluxe".to_string()]);
        file.save().expect("saved");
    }
    f.rescanned("stray", size(&stray));

    assert_eq!(
        state(&f.call("id=stray&action=joinAlbum&like=lead").await),
        "applied"
    );

    let joined = kept(&stray);
    let lead = kept(&lead);
    assert_eq!(joined.album_id, None);
    assert_eq!(joined.album_version, None);
    assert_eq!(joined.release_date, lead.release_date);
    let untitled = |identity: &KeptIdentity| {
        KeptIdentityTags::pid_inputs(&KeptIdentity {
            title: String::new(),
            ..identity.clone()
        })
    };
    assert_eq!(untitled(&lead), untitled(&joined));
}

// ---- Covers --------------------------------------------------------------------------------

fn jpeg(side: u32) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(side, side, image::Rgb([40, 80, 120]));
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Jpeg)
        .expect("encodes");
    bytes
}

#[tokio::test]
async fn cover_goes_only_into_a_file_with_no_picture_one_lookup_per_album_and_undo_takes_it_out() {
    let f = Factory::with(|_| {}, Some(FoundCover::new(jpeg(600), "Apple Music", 600))).await;
    let one = f.song_with("s1", "A/B/One.flac", "One", "A", "B", Some("A"), 0, None);
    let two = f.song_with("s2", "A/B/Two.flac", "Two", "A", "B", Some("A"), 0, None);

    let preview = f.call("id=s1&action=cover&preview=true").await;
    assert_eq!(state(&preview), "found", "{preview}");
    assert!(!tag_edits::has_picture(&one));

    assert_eq!(state(&f.call("id=s1&action=cover").await), "applied");
    assert_eq!(state(&f.call("id=s2&action=cover").await), "applied");
    assert!(tag_edits::has_picture(&one));
    assert!(tag_edits::has_picture(&two));
    assert_eq!(f.finder.asked.load(Ordering::SeqCst), 1);

    // A picture of its own already: left alone.
    assert_eq!(state(&f.call("id=s1&action=cover").await), "skipped");

    assert_eq!(state(&f.call("id=s1&action=undo").await), "applied");
    assert!(!tag_edits::has_picture(&one));
    assert!(tag_edits::has_picture(&two));
}

#[tokio::test]
async fn cover_nothing_found_says_so_and_writes_nothing() {
    let f = Factory::new().await;
    let one = f.song("s1", "A/B/One.flac", "One", "A", "B");

    assert_eq!(state(&f.call("id=s1&action=cover").await), "failed");
    assert!(!tag_edits::has_picture(&one));
}

// ---- Lookup --------------------------------------------------------------------------------

#[tokio::test]
async fn lookup_an_unknown_song_is_unresolved_with_the_lookup_shape() {
    let f = Factory::new().await;

    let action = f.call("id=nope&action=lookup").await;

    assert_eq!(state(&action), "unresolved");
    assert!(action["current"].is_object());
    assert!(action["suggested"].is_object());
}

// ---- The dashboard's trash, and undoing a cleared tag ------------------------------------

/// The dashboard's Recently removed list, which answers from the same trash:
/// signed out it is refused, signed in it lists the song with its own files and puts it back.
#[tokio::test]
async fn the_dashboard_lists_the_trash_and_puts_a_song_back() {
    use crate::controllers::admin::test_app::{app as admin_app, send};
    use axum::http::{Method, StatusCode};

    let f = Factory::new().await;
    let path = f.song("s1", "A/B/Song.flac", "Song", "A", "B");
    std::fs::write(path.with_extension("lrc"), "words").expect("written");
    assert_eq!(state(&f.call("id=s1&action=remove").await), "applied");
    let app = admin_app(f.state.clone());

    let anonymous = send(&app, Method::GET, "/api/admin/trash", &[], None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let token = f.state.browse_sessions.create("alice");
    let signed = [("X-Octo-Admin", "1"), ("X-Octo-Browse-Token", token.as_str())];
    let listed = send(&app, Method::GET, "/api/admin/trash", &signed, None).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.text());
    let doc = listed.json();
    assert_eq!(doc["keepDays"], 14);
    assert_eq!(doc["dryRun"], false);
    let song = &doc["songs"][0];
    assert_eq!(song["id"], "s1");
    assert_eq!(song["removedBy"], "alice");
    assert_eq!(song["sidecars"], json!(["Song.lrc"]));
    assert_eq!(song["blocksDownloads"], true);
    assert!(song["key"].is_string());

    let missing = send(
        &app,
        Method::POST,
        "/api/admin/trash/restore",
        &signed,
        Some("{}"),
    )
    .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    assert_eq!(missing.text(), r#"{"error":"Which song? The id is missing."}"#);

    let put_back = send(
        &app,
        Method::POST,
        "/api/admin/trash/restore",
        &signed,
        Some(r#"{"id":"s1"}"#),
    )
    .await;
    assert_eq!(
        put_back.text(),
        r#"{"state":"applied","detail":"Put back in your library."}"#
    );
    assert!(path.exists());
    assert!(path.with_extension("lrc").exists());

    let bob = f.state.browse_sessions.create("bob");
    let refused = send(
        &app,
        Method::GET,
        "/api/admin/trash",
        &[("X-Octo-Browse-Token", bob.as_str())],
        None,
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(
        refused.text(),
        r#"{"error":"bob is not on the library actions allowed list."}"#
    );
}

/// The undo of an ISRC retag takes the code off the file, as its answer says.
#[tokio::test]
async fn undoing_an_isrc_retag_takes_the_code_off_the_file() {
    let f = Factory::new().await;
    let path = f.song("s1", "A/B/Song.flac", "Song", "A", "B");

    let retag = f.call("id=s1&action=retag&isrc=qz-par-20-00001").await;
    assert_eq!(retag["after"]["isrc"], "QZPAR2000001");
    assert_eq!(tag(&path, "isrc").as_deref(), Some("QZPAR2000001"));

    let undo = f.call("id=s1&action=undo").await;

    assert_eq!(state(&undo), "applied", "{undo}");
    assert_eq!(undo["after"]["isrc"], Value::Null);
    let file = TagFile::open(&path).expect("opens");
    assert_eq!(extras::read_text(&file, TagFields::ISRC), None);
}

#[tokio::test]
async fn retag_waits_while_a_job_writes_the_music_folder() {
    let f = Factory::new().await;
    let path = f.song(
        "s1",
        "Massive Attack/Mezzanine/Teardrop.flac",
        "Teardrop",
        "Massive Attack",
        "Mezzanine",
    );
    let tidy = f
        .state
        .library_write_gate
        .begin_job("Tidy artists")
        .await
        .expect("free");

    let edits = f.state.library_edits.clone();
    let editing = tokio::spawn(async move {
        let changes = indexmap::IndexMap::from([("genre".to_string(), Some("Trip Hop".to_string()))]);
        edits.retag("s1", "alice", changes).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!editing.is_finished(), "the tags were written during a job");
    assert_eq!(tag(&path, "genre"), None);

    drop(tidy);
    let outcome = editing.await.expect("joined");
    assert_eq!(outcome.state, "applied", "{outcome:?}");
    assert_eq!(tag(&path, "genre").as_deref(), Some("Trip Hop"));
}
