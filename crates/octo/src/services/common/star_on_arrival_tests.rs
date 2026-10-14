//! Star on arrival (#71): with StarDownloadsForRequester on (off by default), a star
//! from another app on a song Octo found becomes a Navidrome favorite once the song lands, for
//! the person who starred it, signed as them, and only once. A song already owned is favorited
//! whatever it says.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{TimeDelta, TimeZone};
use futures::FutureExt;
use md5::{Digest, Md5};
use octo_core::settings::{AppSettings, SubsonicSettings};

use super::*;
use crate::services::common::test_fakes::until;

type Calls = Arc<Mutex<Vec<(String, IndexMap<String, String>)>>>;

struct Fixture {
    calls: Calls,
    settings: Arc<SettingsStore>,
    already_starred: Arc<AtomicBool>,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            calls: Arc::default(),
            settings: Arc::new(SettingsStore::for_tests(AppSettings {
                subsonic: SubsonicSettings {
                    star_downloads_for_requester: true,
                    ..Default::default()
                },
                ..Default::default()
            })),
            already_starred: Arc::default(),
        }
    }

    fn set_starring(&self, on: bool) {
        self.settings.set(AppSettings {
            subsonic: SubsonicSettings {
                star_downloads_for_requester: on,
                ..Default::default()
            },
            ..Default::default()
        });
    }

    /// The tracker. Each song turns up in Navidrome under its title, lower case: "T1" is nd-t1.
    fn tracker(&self, can_look: bool) -> Arc<AcquisitionTracker> {
        let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
        tracker.configure_watch(|watch| {
            watch.visibility_poll = Duration::from_millis(10);
            if can_look {
                watch.library_lookup = Some(Arc::new(|_, title: String, _| {
                    async move { Ok(Some(format!("nd-{}", title.to_lowercase()))) }.boxed()
                }));
            }
        });
        tracker
    }

    fn stars(&self, tracker: &Arc<AcquisitionTracker>, clock: Clock) -> Arc<StarOnArrival> {
        let stars = StarOnArrival::new(tracker.clone(), None, self.settings.clone(), clock);
        let (calls, starred) = (self.calls.clone(), self.already_starred.clone());
        stars.configure(|seams| {
            seams.visibility_poll = Duration::from_millis(10);
            seams.call = Some(Arc::new(move |endpoint: String, parameters: IndexMap<String, String>| {
                calls.lock().push((endpoint.clone(), parameters.clone()));
                let body = if endpoint == "rest/getSong" {
                    format!(
                        r#"{{"subsonic-response":{{"status":"ok","song":{{"id":"{}","albumId":"al-1"{}}}}}}}"#,
                        parameters["id"],
                        if starred.load(Ordering::SeqCst) {
                            r#","starred":"2026-10-01T10:00:00Z""#
                        } else {
                            ""
                        }
                    )
                } else {
                    r#"{"subsonic-response":{"status":"ok"}}"#.to_string()
                };
                async move { Ok(body.into_bytes()) }.boxed()
            }));
        });
        stars
    }

    fn calls_to(&self, endpoint: &str) -> Vec<IndexMap<String, String>> {
        self.calls
            .lock()
            .iter()
            .filter(|(e, _)| e == endpoint)
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn star_count(&self) -> usize {
        self.calls_to("rest/star").len()
    }
}

fn credential(user: &str) -> SubsonicCredential {
    let pairs: Vec<(String, String)> = vec![
        ("u".into(), user.into()),
        ("t".into(), format!("token-{user}")),
        ("s".into(), format!("salt-{user}")),
        ("c".into(), "Symfonium".into()),
    ];
    SubsonicCredential::from(pairs.iter().map(|(k, v)| (k, v))).expect("a sign-in")
}

fn begin(tracker: &AcquisitionTracker, id: &str, user: &str, artist: Option<&str>, title: Option<&str>) {
    tracker.begin("outside", id, Some(id), Some(user), artist, title, None);
}

#[tokio::test]
async fn a_starred_song_is_favorited_for_the_starrer_when_it_arrives() {
    let f = Fixture::new();
    let tracker = f.tracker(true);
    let stars = f.stars(&tracker, Clock::system());
    begin(&tracker, "abc", "alice", Some("Massive Attack"), Some("Teardrop"));
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));

    tracker.imported(
        "outside",
        "abc",
        Some("Massive Attack"),
        Some("Teardrop"),
        Some("/music/teardrop.flac"),
    );

    until(|| f.star_count() == 1).await;
    let calls = f.calls.lock().clone();
    assert_eq!(calls[0].0, "rest/getSong");
    assert_eq!(calls[0].1["id"], "nd-teardrop");
    assert_eq!(calls[0].1["u"], "alice");
    let star = &f.calls_to("rest/star")[0];
    assert_eq!(star["id"], "nd-teardrop");
    assert_eq!(star["u"], "alice");
    assert_eq!(star["t"], "token-alice");
    assert_eq!(star["s"], "salt-alice");
    assert_eq!(stars.held(), 0);
}

#[tokio::test]
async fn each_starrer_gets_their_own_favorite() {
    let f = Fixture::new();
    let tracker = f.tracker(true);
    let stars = f.stars(&tracker, Clock::system());
    begin(&tracker, "abc", "alice", Some("A"), Some("Song"));
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));
    stars.hold_song("outside", "abc", &credential("bob"), Some("bob"));

    tracker.imported(
        "outside",
        "abc",
        Some("A"),
        Some("Song"),
        Some("/music/song.flac"),
    );

    until(|| f.star_count() == 2).await;
    let mut users: Vec<String> = f.calls_to("rest/star").iter().map(|c| c["u"].clone()).collect();
    users.sort();
    assert_eq!(users, ["alice", "bob"]);
    assert!(f.calls_to("rest/star").iter().all(|c| c["id"] == "nd-song"));
}

#[tokio::test]
async fn a_failed_download_drops_the_hold_without_a_call() {
    let f = Fixture::new();
    let tracker = f.tracker(true);
    let stars = f.stars(&tracker, Clock::system());
    begin(&tracker, "abc", "alice", None, None);
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));

    tracker.fail("outside", "abc", Some("No source had it"));

    assert_eq!(stars.held(), 0);
    assert!(f.calls.lock().is_empty());
}

#[tokio::test]
async fn a_song_navidrome_never_shows_is_not_favorited() {
    let f = Fixture::new();
    let tracker = f.tracker(false);
    let stars = f.stars(&tracker, Clock::system());
    begin(&tracker, "abc", "alice", None, None);
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));

    tracker.imported(
        "outside",
        "abc",
        Some("A"),
        Some("Song"),
        Some("/music/song.flac"),
    );

    assert_eq!(stars.held(), 0);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.calls.lock().is_empty());
}

fn start_album(tracker: &AcquisitionTracker, stars: &StarOnArrival) {
    tracker.begin_album("outside", "alb", Some("alice"));
    stars.hold_album("outside", "alb", &credential("alice"), Some("alice"));
    tracker.announce(
        "outside",
        Some("alb"),
        None,
        &[
            (
                "t1".into(),
                Some("Air".into()),
                Some("T1".into()),
                Some("Moon Safari".into()),
            ),
            (
                "t2".into(),
                Some("Air".into()),
                Some("T2".into()),
                Some("Moon Safari".into()),
            ),
        ],
    );
}

#[tokio::test]
async fn an_album_star_favorites_the_album_once() {
    let f = Fixture::new();
    let tracker = f.tracker(true);
    let stars = f.stars(&tracker, Clock::system());
    start_album(&tracker, &stars);

    tracker.imported("outside", "t1", Some("Air"), Some("T1"), Some("/music/t1.flac"));
    tracker.imported("outside", "t2", Some("Air"), Some("T2"), Some("/music/t2.flac"));

    until(|| f.star_count() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let all = f.calls_to("rest/star");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["albumId"], "al-1");
    assert!(!all[0].contains_key("id"));
    assert_eq!(all[0]["u"], "alice");
    assert_eq!(stars.held(), 0);
}

#[tokio::test]
async fn an_album_hold_survives_a_failed_track() {
    let f = Fixture::new();
    let tracker = f.tracker(true);
    let stars = f.stars(&tracker, Clock::system());
    start_album(&tracker, &stars);

    tracker.fail("outside", "t1", Some("No source had it"));
    assert_eq!(stars.held(), 1);
    tracker.imported("outside", "t2", Some("Air"), Some("T2"), Some("/music/t2.flac"));

    until(|| f.star_count() == 1).await;
    assert_eq!(f.calls_to("rest/star")[0]["albumId"], "al-1");
}

#[tokio::test]
async fn setting_off_at_arrival_stars_nothing() {
    let f = Fixture::new();
    let tracker = f.tracker(true);
    let stars = f.stars(&tracker, Clock::system());
    begin(&tracker, "abc", "alice", None, None);
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));
    f.set_starring(false);

    tracker.imported(
        "outside",
        "abc",
        Some("A"),
        Some("Song"),
        Some("/music/song.flac"),
    );

    until(|| stars.held() == 0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.calls.lock().is_empty());
}

#[tokio::test]
async fn the_default_is_that_a_download_is_not_favorited() {
    assert!(!SubsonicSettings::default().star_downloads_for_requester);
    let f = Fixture::new();
    let tracker = f.tracker(true);
    f.settings.set(AppSettings::default());
    let stars = f.stars(&tracker, Clock::system());
    begin(&tracker, "abc", "alice", None, None);
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));

    tracker.imported(
        "outside",
        "abc",
        Some("A"),
        Some("Song"),
        Some("/music/song.flac"),
    );

    until(|| stars.held() == 0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.calls.lock().is_empty());
}

#[tokio::test]
async fn a_song_already_owned_is_favorited_with_the_setting_off() {
    let f = Fixture::new();
    let tracker = f.tracker(true);
    f.set_starring(false);
    let stars = f.stars(&tracker, Clock::system());
    begin(&tracker, "abc", "alice", None, None);
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));

    assert!(stars.favorite_owned(
        "outside",
        "abc",
        Some("nd-owned"),
        "A",
        "Song",
        "/music/song.flac"
    ));

    until(|| f.star_count() == 1).await;
    assert_eq!(f.calls_to("rest/star")[0]["id"], "nd-owned");
    assert_eq!(stars.held(), 0);
}

#[tokio::test]
async fn an_owned_song_from_octos_own_apps_is_not_favorited() {
    let f = Fixture::new();
    let stars = f.stars(&f.tracker(true), Clock::system());
    assert!(!stars.favorite_owned(
        "outside",
        "abc",
        Some("nd-owned"),
        "A",
        "Song",
        "/music/song.flac"
    ));
    assert!(f.calls.lock().is_empty());
}

#[tokio::test]
async fn already_a_favorite_is_left_alone() {
    let f = Fixture::new();
    f.already_starred.store(true, Ordering::SeqCst);
    let tracker = f.tracker(true);
    let stars = f.stars(&tracker, Clock::system());
    begin(&tracker, "abc", "alice", None, None);
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));

    tracker.imported(
        "outside",
        "abc",
        Some("A"),
        Some("Song"),
        Some("/music/song.flac"),
    );

    until(|| f.calls_to("rest/getSong").len() == 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.calls_to("rest/star").is_empty());
}

#[tokio::test]
async fn holds_expire_after_a_day() {
    let f = Fixture::new();
    let now = Arc::new(Mutex::new(Utc.with_ymd_and_hms(2026, 10, 2, 18, 0, 0).unwrap()));
    let read = now.clone();
    let stars = f.stars(&f.tracker(true), Clock::new(move || *read.lock()));
    stars.hold_song("outside", "old", &credential("alice"), Some("alice"));

    *now.lock() += TimeDelta::hours(25);
    stars.hold_song("outside", "new", &credential("alice"), Some("alice"));

    assert_eq!(stars.held(), 1);
}

#[test]
fn is_octo_app() {
    let cases = [
        (Some("Octo"), true),
        (Some("octo"), true),
        (Some(" Octo "), true),
        (Some("octo-android"), false),
        (Some("Symfonium"), false),
        (None, false),
    ];
    for (client, expected) in cases {
        assert_eq!(StarOnArrival::is_octo_app(client), expected, "{client:?}");
    }
}

#[tokio::test]
async fn star_when_visible_favorites_the_replacement() {
    let f = Fixture::new();
    let stars = f.stars(&f.tracker(true), Clock::system());
    stars.configure(|seams| {
        seams.library_lookup = Some(Arc::new(|_, _, _| {
            async { Ok(Some("nd-new".to_string())) }.boxed()
        }));
    });

    stars.star_when_visible(
        &credential("alice"),
        "alice",
        "Air",
        "La Femme d'Argent",
        "/music/femme.flac",
    );

    until(|| f.star_count() == 1).await;
    let star = &f.calls_to("rest/star")[0];
    assert_eq!(star["id"], "nd-new");
    assert_eq!(star["u"], "alice");
}

#[tokio::test]
async fn a_password_is_never_held_only_a_token_made_from_it() {
    for sent in ["s3cret", "enc:733363726574"] {
        let f = Fixture::new();
        let tracker = f.tracker(true);
        let stars = f.stars(&tracker, Clock::system());
        begin(&tracker, "abc", "alice", None, None);
        let pairs: Vec<(String, String)> = vec![
            ("u".into(), "alice".into()),
            ("p".into(), sent.into()),
            ("c".into(), "Symfonium".into()),
        ];
        let credential = SubsonicCredential::from(pairs.iter().map(|(k, v)| (k, v))).expect("a sign-in");
        stars.hold_song("outside", "abc", &credential, Some("alice"));

        tracker.imported(
            "outside",
            "abc",
            Some("A"),
            Some("Song"),
            Some("/music/song.flac"),
        );

        until(|| f.star_count() == 1).await;
        for (_, parameters) in f.calls.lock().iter() {
            assert!(!parameters.contains_key("p"), "{sent}");
            assert_eq!(parameters["u"], "alice");
            let salt = &parameters["s"];
            assert!(
                salt.len() == 12
                    && salt
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{salt}"
            );
            let expected = hex::encode(Md5::digest(format!("s3cret{salt}").as_bytes()));
            assert_eq!(parameters["t"], expected, "{sent}");
        }
    }
}

#[test]
fn without_password_leaves_a_token_sign_in_as_it_is() {
    let credential = credential("alice");
    assert_eq!(credential.without_password(), credential);
}

// ---- sign-in keys, song facts and unsubscribing ------------------------------------------

/// An API key names nobody, so its fingerprint stands in; a second star from the same person
/// replaces the first.
#[test]
fn an_api_key_is_named_by_its_fingerprint_and_one_star_each() {
    let f = Fixture::new();
    let stars = f.stars(&f.tracker(true), Clock::system());
    let pairs: Vec<(String, String)> = vec![("apiKey".into(), "bob-key".into())];
    let key = SubsonicCredential::from(pairs.iter().map(|(k, v)| (k, v))).expect("a sign-in");
    stars.hold_song("outside", "abc", &key, None);
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));
    stars.hold_song("outside", "abc", &credential("ALICE"), Some("ALICE"));
    assert_eq!(stars.held(), 2);
    let holds = stars.holds.lock();
    assert_eq!(holds.songs[0].who, format!("API key {}", &key.fingerprint()[..8]));
    assert_eq!(holds.songs[1].who, "ALICE");
}

/// What `getSong` says: a body that is not JSON is nothing, a node that is not an object fails.
#[test]
fn song_facts_reads_starred_and_the_album() {
    assert_eq!(song_facts(b"not json").expect("no answer"), None);
    assert_eq!(song_facts(br#"{"subsonic-response":{}}"#).expect("no song"), None);
    assert_eq!(
        song_facts(br#"{"subsonic-response":{"song":{"starred":"2026","albumId":"al"}}}"#).expect("read"),
        Some(SongFacts {
            starred: true,
            album_id: Some("al".into())
        })
    );
    assert!(song_facts(b"[]").is_err());
}

/// Dropping the last handle stops listening.
#[tokio::test]
async fn dropping_it_stops_listening() {
    let f = Fixture::new();
    let tracker = f.tracker(true);
    let stars = f.stars(&tracker, Clock::system());
    begin(&tracker, "abc", "alice", None, None);
    stars.hold_song("outside", "abc", &credential("alice"), Some("alice"));
    drop(stars);
    tracker.imported(
        "outside",
        "abc",
        Some("A"),
        Some("Song"),
        Some("/music/song.flac"),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(f.calls.lock().is_empty());
}

/// A member's heart that joined a download landing in a shared library they cannot see: the
/// favorite goes on their own pool copy, signed as them, never on the shared song.
#[tokio::test]
async fn the_member_heart_stars_their_own_linked_song() {
    use crate::services::family::test_support::{World, member, navidrome_with_songs, song_row, write_file};
    use crate::services::family::{FamilyPlacement, SharedLibraries};
    use octo_core::family::abilities::Preset;

    let f = Fixture::new();
    let world = World::new(None);
    let mut alex = member("alex", Preset::Member, 3);
    alex.shared_libraries = SharedLibraries::Ids(vec![2]);
    world.family.upsert_member(alex);
    let relative = "Artist/Album/1.flac";
    let navidrome = navidrome_with_songs(serde_json::json!([
        song_row("shared-1", 1, &world.music(), relative, 10),
        song_row("alex-1", 3, &format!("{}/alex", world.members()), relative, 10),
    ]))
    .await;
    world.point_at(&navidrome.uri());
    world.index.refresh().await.expect("read");
    let shared = format!("{}/{relative}", world.music());
    write_file(&shared, 10);
    // As the worker settles the owner's download that Alex's heart joined.
    world
        .pool
        .settle(&shared, &["alex".to_string()], true, Some("test:1"))
        .await;

    // Navidrome shows the download as the shared library's song.
    let tracker = Arc::new(AcquisitionTracker::new(None, Clock::system()));
    tracker.configure_watch(|watch| {
        watch.visibility_poll = Duration::from_millis(10);
        watch.library_lookup = Some(Arc::new(|_, _, _| {
            async { Ok(Some("shared-1".to_string())) }.boxed()
        }));
    });
    let stars = f.stars(&tracker, Clock::system());
    stars.use_member_copies(world.pool.clone());
    tracker.begin(
        "test",
        "1",
        Some("1"),
        Some("alex"),
        Some("Artist"),
        Some("Song"),
        None,
    );
    stars.hold_song("test", "1", &credential("alex"), Some("alex"));
    tracker.imported("test", "1", Some("Artist"), Some("Song"), Some(&shared));

    until(|| f.star_count() == 1).await;
    let star = &f.calls_to("rest/star")[0];
    assert_eq!(star["id"], "alex-1", "their own linked song");
    assert_eq!(star["u"], "alex");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.star_count(), 1, "the shared song is never starred");
}
