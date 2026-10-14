//! ListenBrainz as a radio source, against answers captured from the live labs service
//! (`crates/octo/tests/fixtures/listenbrainz`, 2026-10-04): a recording found by name or by its tag, its
//! similar recordings scaled to 0 to 1, and LB Radio only with a token.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use octo_core::models::domain::Song;
use octo_core::settings::{AppSettings, ListenBrainzSettings};
use parking_lot::Mutex;
use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::*;
use crate::services::listen_brainz::listen_brainz_radio_client::collect_rows;

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/listenbrainz/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("the fixture is in the repo")
}

type Answer = Box<dyn Fn(&wiremock::Request) -> (u16, String) + Send + Sync>;

struct LabsState {
    requests: Mutex<Vec<(String, Option<String>)>>,
    answer: Answer,
}

#[derive(Clone)]
struct Labs(Arc<LabsState>);

impl Respond for Labs {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let authorization = request
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.0.requests.lock().push((
            format!("{}?{}", request.url.path(), request.url.query().unwrap_or("")),
            authorization,
        ));
        let (status, body) = (self.0.answer)(request);
        ResponseTemplate::new(status).set_body_raw(body, "application/json")
    }
}

fn live(request: &wiremock::Request) -> (u16, String) {
    match request.url.path() {
        "/acr-lookup/json" => (
            200,
            if request.url.query().unwrap_or("").contains("Kordhell") {
                fixture("acr-lookup.json")
            } else {
                fixture("acr-lookup-none.json")
            },
        ),
        "/similar-recordings/json" => (200, fixture("similar-recordings.json")),
        "/1/explore/lb-radio" => (
            200,
            r#"{"payload":{"jspf":{"playlist":{"track":[{"title":"Close Eyes","creator":"DVRST","duration":132000},{"title":"RAVE","creator":"Dxrk"}]}}}}"#.into(),
        ),
        _ => (404, "{}".into()),
    }
}

async fn source(token: &str, answer: Option<Answer>) -> (ListenBrainzRadioSource, Labs, MockServer) {
    let server = MockServer::builder().start().await;
    let labs = Labs(Arc::new(LabsState {
        requests: Mutex::new(Vec::new()),
        answer: answer.unwrap_or_else(|| Box::new(live)),
    }));
    Mock::given(any()).respond_with(labs.clone()).mount(&server).await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings {
        listen_brainz: ListenBrainzSettings {
            token: token.into(),
            ..Default::default()
        },
        ..Default::default()
    }));
    let client = ListenBrainzRadioClient::with_urls(settings.clone(), &server.uri(), &server.uri())
        .with_pacing(Duration::ZERO, ListenBrainzRadioClient::MAX_QUEUE_WAIT);
    (
        ListenBrainzRadioSource::new(Arc::new(client), settings),
        labs,
        server,
    )
}

fn requests(labs: &Labs) -> Vec<(String, Option<String>)> {
    labs.0.requests.lock().clone()
}

fn murder() -> RadioSeed {
    RadioSeed::new("Kordhell", "Murder In My Mind", Some(145), None)
}

async fn ask(source: &ListenBrainzRadioSource, seed: &RadioSeed) -> RadioAnswer {
    source
        .songs_like(seed, 50, &IndexMap::new())
        .await
        .expect("an answer")
}

#[tokio::test]
async fn a_song_found_by_name_is_the_similar_recordings_scaled_to_its_best() {
    let (source, labs, _server) = source("", None).await;
    let answer = ask(&source, &murder()).await;

    assert_eq!(answer.r#match, RadioMatch::Song);
    assert!(answer.tracks.len() > 20);
    let first = &answer.tracks[0];
    assert_eq!(
        (first.artist.as_str(), first.title.as_str(), first.r#match),
        ("Dxrk ダーク", "RAVE", 1.0)
    );
    assert!((answer.tracks[1].r#match - 149.0 / 216.0).abs() < 1e-6);
    assert!(answer.tracks.iter().all(|t| (0.0..=1.0).contains(&t.r#match)));
    assert!(
        answer
            .tracks
            .iter()
            .all(|t| t.provider.as_deref() == Some(LISTEN_BRAINZ))
    );
    let calls = requests(&labs);
    assert!(
        calls
            .iter()
            .any(|(call, _)| call.contains("recording_mbids=eb27c076-7686-465a-8737-8722611486c5"))
    );
    assert!(
        calls
            .iter()
            .any(|(call, _)| call.contains("session_based_days_9000"))
    );
}

#[tokio::test]
async fn a_song_with_its_recording_tagged_needs_no_lookup() {
    let (source, labs, _server) = source("", None).await;
    let tagged = Song {
        id: "lib-1".into(),
        music_brainz_recording_id: Some("eb27c076-7686-465a-8737-8722611486c5".into()),
        ..Default::default()
    };
    ask(
        &source,
        &RadioSeed::new("Kordhell", "Murder In My Mind", Some(145), Some(tagged)),
    )
    .await;
    assert!(
        !requests(&labs)
            .iter()
            .any(|(call, _)| call.starts_with("/acr-lookup/json"))
    );
}

#[tokio::test]
async fn a_song_listen_brainz_does_not_know_is_nothing() {
    let (source, labs, _server) = source("", None).await;
    let answer = ask(
        &source,
        &RadioSeed::new("Nobody Zzq", "Nothing Qqz", Some(100), None),
    )
    .await;
    assert_eq!(answer.r#match, RadioMatch::None);
    assert!(
        !requests(&labs)
            .iter()
            .any(|(call, _)| call.starts_with("/similar-recordings/json"))
    );
}

#[tokio::test]
async fn lb_radio_needs_a_token_and_sends_it() {
    let (without, quiet, _server) = source("", None).await;
    let artist_seed = RadioSeed::new("Kordhell", "", None, None);
    assert_eq!(
        without
            .songs_like(&artist_seed, 20, &IndexMap::new())
            .await
            .unwrap()
            .r#match,
        RadioMatch::None
    );
    assert_eq!(without.tag("phonk", 20).await.unwrap().r#match, RadioMatch::None);
    assert!(requests(&quiet).is_empty());

    let (with, labs, _server) = source("lb-token", None).await;
    let artist = with.songs_like(&artist_seed, 20, &IndexMap::new()).await.unwrap();
    let tag = with.tag("phonk", 20).await.unwrap();

    assert_eq!(artist.r#match, RadioMatch::ArtistsTrusted);
    assert_eq!(tag.r#match, RadioMatch::Genre);
    assert_eq!(artist.tracks[0].duration, Some(132));
    let calls = requests(&labs);
    assert!(
        calls
            .iter()
            .all(|(_, auth)| auth.as_deref() == Some("Token lb-token"))
    );
    let decoded: Vec<String> = calls
        .iter()
        .map(|(call, _)| octo_core::common::dotnet::unescape_data_string(call))
        .collect();
    assert!(
        decoded
            .iter()
            .any(|call| call.contains("prompt=artist:(Kordhell)"))
    );
    assert!(decoded.iter().any(|call| call.contains("prompt=tag:(phonk)")));
}

#[tokio::test]
async fn slow_down_is_nothing_not_an_error() {
    let (source, _labs, _server) = source("", Some(Box::new(|_| (429, "{}".into())))).await;
    assert_eq!(ask(&source, &murder()).await.r#match, RadioMatch::None);
}

#[test]
fn rows_read_both_shapes_the_labs_datasets_use() {
    let flat: Value = serde_json::from_str(r#"[{"recording_name":"A"},{"recording_name":"B"}]"#).unwrap();
    let blocks: Value = serde_json::from_str(
        r#"[{"type":"markup","data":"<p/>"},{"type":"dataset","data":[{"recording_name":"C"}]}]"#,
    )
    .unwrap();
    let mut rows = Vec::new();
    collect_rows(&flat, &mut rows);
    assert_eq!(rows.len(), 2);
    let mut rows = Vec::new();
    collect_rows(&blocks, &mut rows);
    assert_eq!(rows.last().unwrap()["recording_name"], "C");
}

#[tokio::test]
async fn a_failed_lookup_is_asked_again_not_remembered_as_unknown() {
    let down = Arc::new(AtomicBool::new(true));
    let flag = down.clone();
    let (source, _labs, _server) = source(
        "",
        Some(Box::new(move |request| {
            if flag.load(Ordering::SeqCst) {
                (502, "{}".into())
            } else {
                live(request)
            }
        })),
    )
    .await;
    assert_eq!(ask(&source, &murder()).await.r#match, RadioMatch::None);
    down.store(false, Ordering::SeqCst);
    assert_eq!(ask(&source, &murder()).await.r#match, RadioMatch::Song);
}

#[tokio::test]
async fn an_odd_score_leaves_the_rest_of_the_answer() {
    let (source, _labs, _server) = source(
        "",
        Some(Box::new(|request| {
            if request.url.path() == "/similar-recordings/json" {
                (
                    200,
                    r#"[{"recording_name":"RAVE","artist_credit_name":"Dxrk","score":"lots"},{"recording_name":"Close Eyes","artist_credit_name":"DVRST","score":40}]"#.into(),
                )
            } else {
                live(request)
            }
        })),
    )
    .await;
    let answer = ask(&source, &murder()).await;
    assert!(answer.tracks.iter().any(|t| t.title == "Close Eyes"));
}

/// A call that would wait past the longest queue wait is skipped as nothing.
#[tokio::test]
async fn a_call_too_far_back_in_the_queue_is_skipped() {
    let server = MockServer::builder().start().await;
    let labs = Labs(Arc::new(LabsState {
        requests: Mutex::new(Vec::new()),
        answer: Box::new(live),
    }));
    Mock::given(any()).respond_with(labs.clone()).mount(&server).await;
    let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
    let client = ListenBrainzRadioClient::with_urls(settings, &server.uri(), &server.uri())
        .with_pacing(Duration::from_secs(10), Duration::from_millis(1));
    assert!(
        client
            .recording_mbid("Kordhell", "Murder In My Mind")
            .await
            .is_some()
    );
    // The next slot is ten seconds away: past the wait allowed, so nothing is asked.
    assert!(client.recording_mbid("Other", "Song").await.is_none());
    assert_eq!(requests(&labs).len(), 1);
}

// LB Radio asks in the difficulty the dashboard chose, easy unless set, and an answer in one
// mode is never handed out for another.
#[tokio::test]
async fn lb_radio_asks_in_the_chosen_mode_and_keeps_each_modes_answer_apart() {
    use octo_core::settings::RadioSourceSettings;
    let server = MockServer::start().await;
    let labs = Labs(Arc::new(LabsState {
        requests: Mutex::new(Vec::new()),
        answer: Box::new(live),
    }));
    Mock::given(any()).respond_with(labs.clone()).mount(&server).await;
    let with_mode = |mode: &str| AppSettings {
        listen_brainz: ListenBrainzSettings {
            token: "lb-token".into(),
            ..Default::default()
        },
        radio_sources: RadioSourceSettings {
            listen_brainz_radio_mode: mode.into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let settings = Arc::new(SettingsStore::for_tests(with_mode("")));
    let client = ListenBrainzRadioClient::with_urls(settings.clone(), &server.uri(), &server.uri())
        .with_pacing(Duration::ZERO, ListenBrainzRadioClient::MAX_QUEUE_WAIT);
    let source = ListenBrainzRadioSource::new(Arc::new(client), settings.clone());

    assert_eq!(source.tag("phonk", 20).await.unwrap().r#match, RadioMatch::Genre);
    settings.set(with_mode("hard"));
    assert_eq!(source.tag("phonk", 20).await.unwrap().r#match, RadioMatch::Genre);
    assert_eq!(source.tag("phonk", 20).await.unwrap().r#match, RadioMatch::Genre);

    let modes: Vec<String> = requests(&labs)
        .into_iter()
        .filter(|(call, _)| call.starts_with("/1/explore/lb-radio"))
        .map(|(call, _)| call.split("mode=").nth(1).unwrap_or("").to_string())
        .collect();
    assert_eq!(modes, ["easy", "hard"]);
}
