//! The dashboard's Last.fm scrobbling Connect, Finish and Disconnect, and what the admin API
//! shows of them.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Router;
use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use indexmap::IndexMap;
use octo_core::json::dom::Node;
use octo_core::last_fm::last_fm_scrobble_service::sign;
use octo_core::settings::{AppSettings, SettingsFileWriter, SettingsStore};
use parking_lot::Mutex;

use super::plumbing::SECRET_PLACEHOLDER;
use super::test_app::{admin_write, app, get, send};
use crate::app::AppState;
use crate::http::pipeline::App;
use crate::services::last_fm::last_fm_scrobble_service::{
    LastFmScrobbleService, ScrobbleTime, ScrobbleTuning,
};

const API_KEY: &str = "0123456789abcdef0123456789abcdef";
const SECRET: &str = "s3cr3t";

type Call = IndexMap<String, String>;

/// Last.fm's web service as far as the Connect flow uses it. Refuses a wrong signature with
/// error 13 as the real one does, so every call a test sees was signed correctly.
#[derive(Default)]
struct FakeLastFm {
    calls: Mutex<Vec<Call>>,
    /// Whether the admin has approved Octo on last.fm yet.
    approved: AtomicBool,
}

impl FakeLastFm {
    fn calls_to(&self, method: &str) -> Vec<Call> {
        self.calls
            .lock()
            .iter()
            .filter(|call| call.get("method").map(String::as_str) == Some(method))
            .cloned()
            .collect()
    }

    async fn serve(self: &Arc<Self>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let address = listener.local_addr().expect("an address");
        let app = Router::new().fallback(respond).with_state(Arc::clone(self));
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{address}/2.0/")
    }
}

async fn respond(State(fake): State<Arc<FakeLastFm>>, body: String) -> Response {
    let call: Call = url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    fake.calls.lock().push(call.clone());
    let signature = sign(call.iter().map(|(k, v)| (k.as_str(), v.as_str())), SECRET);
    if call.get("api_sig") != Some(&signature) {
        return error(13, "Invalid method signature supplied");
    }
    match call.get("method").map(String::as_str) {
        Some("auth.getToken") => ok(r#"{"token":"tok-1"}"#),
        Some("auth.getSession") if fake.approved.load(Ordering::SeqCst) => {
            ok(r#"{"session":{"name":"lfm-alice","key":"sk-alice","subscriber":0}}"#)
        }
        Some("auth.getSession") => error(14, "Unauthorized Token - This token has not been authorized"),
        _ => error(3, "Invalid Method - No method with that name in this package"),
    }
}

fn ok(json: &'static str) -> Response {
    (StatusCode::OK, [("content-type", "application/json")], json).into_response()
}

fn error(code: i32, message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [("content-type", "application/json")],
        serde_json::json!({ "error": code, "message": message }).to_string(),
    )
        .into_response()
}

/// `ScrobbleAdminFactory`: Octo with its settings file in a temporary folder, read back as
/// configuration on top of the environment the way /app/config/settings.json is, and Last.fm
/// faked.
struct ScrobbleAdminFactory {
    directory: PathBuf,
    last_fm: Arc<FakeLastFm>,
    store: Arc<SettingsStore>,
    state: AppState,
    app: App,
}

impl ScrobbleAdminFactory {
    async fn new(settings: &str) -> ScrobbleAdminFactory {
        let directory =
            std::env::temp_dir().join(format!("octo-lastfm-admin-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).expect("a temp folder");
        let path = directory.join("settings.json");
        std::fs::write(&path, settings).expect("the settings file");

        let env = [
            ("Subsonic__Url", "http://127.0.0.1:1"),
            ("Subsonic__AutoDetectDownloadPath", "false"),
            ("YouTube__ShimUrl", "http://127.0.0.1:1"),
            ("Library__DownloadPath", &directory.to_string_lossy()),
            ("LastFm__ApiKey", API_KEY),
            ("LastFm__ApiSecret", SECRET),
            // These tests are about what the endpoints do, not who may call them.
            ("Admin__SignIn", "off"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let store = Arc::new(SettingsStore::from_env(env, Some(path.clone())));
        let writer = Arc::new(SettingsFileWriter::new(&path));

        let last_fm = Arc::new(FakeLastFm::default());
        let api_url = last_fm.serve().await;
        let scrobbles = Arc::new(LastFmScrobbleService::with_parts(
            reqwest::Client::new(),
            &api_url,
            Arc::clone(&store),
            Arc::clone(&writer),
            ScrobbleTuning::default(),
            ScrobbleTime::system(),
        ));

        let mut state = AppState::for_tests(AppSettings::default());
        {
            let inner = Arc::get_mut(&mut state.inner).expect("nothing else holds the test state yet");
            inner.settings = Arc::clone(&store);
            inner.settings_writer = writer;
            inner.last_fm_scrobbles = scrobbles;
        }
        let app = app(state.clone());
        ScrobbleAdminFactory {
            directory,
            last_fm,
            store,
            state,
            app,
        }
    }

    fn settings_path(&self) -> PathBuf {
        self.directory.join("settings.json")
    }

    fn saved(&self) -> String {
        std::fs::read_to_string(self.settings_path()).expect("the settings file")
    }
}

impl Drop for ScrobbleAdminFactory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn user(name: &str) -> String {
    serde_json::json!({ "user": name }).to_string()
}

#[tokio::test]
async fn writes_without_the_admin_header_are_refused() {
    for url in [
        "/api/admin/lastfm/scrobble/connect",
        "/api/admin/lastfm/scrobble/finish",
        "/api/admin/lastfm/scrobble/disconnect",
        "/api/admin/lastfm/scrobble/cancel",
        "/api/admin/lastfm/check",
    ] {
        let factory = ScrobbleAdminFactory::new("{}").await;
        let reply = send(&factory.app, Method::POST, url, &[], Some(&user("alice"))).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{url}");
        assert!(factory.last_fm.calls.lock().is_empty(), "{url}");
    }
}

#[tokio::test]
async fn connect_finish_disconnect_links_and_unlinks_a_user() {
    let factory = ScrobbleAdminFactory::new("{}").await;
    let app = &factory.app;

    // Connect: a signed auth.getToken, and the page to approve Octo on.
    let connect = admin_write(
        app,
        Method::POST,
        "/api/admin/lastfm/scrobble/connect",
        Some(&user("alice")),
    )
    .await;
    assert_eq!(connect.status, StatusCode::OK);
    assert_eq!(
        connect.json()["url"],
        format!("https://www.last.fm/api/auth/?api_key={API_KEY}&token=tok-1")
    );
    assert_eq!(connect.json()["user"], "alice");
    assert_eq!(factory.last_fm.calls_to("auth.getToken").len(), 1);

    // Finish before approving: Last.fm says not yet, and nothing is saved.
    let early = admin_write(
        app,
        Method::POST,
        "/api/admin/lastfm/scrobble/finish",
        Some(&user("alice")),
    )
    .await;
    assert_eq!(early.status, StatusCode::CONFLICT);
    assert!(!factory.saved().contains("sk-alice"));

    factory.last_fm.approved.store(true, Ordering::SeqCst);
    let finish = admin_write(
        app,
        Method::POST,
        "/api/admin/lastfm/scrobble/finish",
        Some(&user("alice")),
    )
    .await;
    assert_eq!(finish.status, StatusCode::OK);
    assert_eq!(finish.json()["lastFmUser"], "lfm-alice");
    assert_eq!(
        factory
            .last_fm
            .calls_to("auth.getSession")
            .last()
            .and_then(|c| c.get("token"))
            .map(String::as_str),
        Some("tok-1")
    );
    assert!(factory.saved().contains("sk-alice"));
    // The settings reload the file watcher would do.
    factory.store.reload_now().expect("the file reloads");

    // The dashboard shows who alice is on Last.fm; no admin read shows the key.
    let status = get(app, "/api/admin/lastfm/scrobble").await.text();
    assert!(status.contains("\"connected\":true"), "{status}");
    assert!(status.contains("lfm-alice"), "{status}");
    assert!(status.contains("\"libraryPlays\":true"), "{status}");
    for read in [
        "/api/admin/lastfm/scrobble",
        "/api/admin/settings",
        "/api/admin/raw-config",
    ] {
        let body = get(app, read).await.text();
        assert!(!body.contains("sk-alice"), "{read}");
    }
    let settings = get(app, "/api/admin/settings").await.json();
    assert_eq!(
        settings["LastFm"]["UserSessions"]["alice"],
        serde_json::json!({ "SessionKey": SECRET_PLACEHOLDER, "LastFmUser": "lfm-alice" })
    );

    let disconnect = admin_write(
        app,
        Method::POST,
        "/api/admin/lastfm/scrobble/disconnect",
        Some(&user("alice")),
    )
    .await;
    assert_eq!(disconnect.status, StatusCode::OK);
    assert_eq!(
        disconnect.text(),
        r#"{"ok":true,"user":"alice","message":"Disconnected. To revoke Octo on Last.fm too, remove it from that account's applications."}"#
    );
    assert!(!factory.saved().contains("sk-alice"));
    assert!(!factory.state.last_fm_scrobbles.is_enabled_for("alice"));
}

/// The page never sees a saved secret, only the placeholder; checking with it checks the
/// stored one. A typed secret is checked as typed.
#[tokio::test]
async fn check_with_the_placeholder_checks_the_stored_secret() {
    let factory = ScrobbleAdminFactory::new("{}").await;
    let saved = admin_write(
        &factory.app,
        Method::POST,
        "/api/admin/lastfm/check",
        Some(&serde_json::json!({ "apiKey": API_KEY, "apiSecret": SECRET_PLACEHOLDER }).to_string()),
    )
    .await;
    let typed = admin_write(
        &factory.app,
        Method::POST,
        "/api/admin/lastfm/check",
        Some(&serde_json::json!({ "apiKey": API_KEY, "apiSecret": "typed-wrong" }).to_string()),
    )
    .await;
    assert_eq!(saved.json()["secret"], "ok");
    assert_eq!(typed.json()["secret"], "invalid");
}

/// The Raw editor writes back what it was shown. The masked session and secret must come back
/// as what is stored, not as the placeholder.
#[tokio::test]
async fn raw_config_round_trip_keeps_the_secret_and_sessions() {
    let factory = ScrobbleAdminFactory::new(
        r#"{ "LastFm": { "ApiSecret": "stored-secret", "UserSessions": { "alice": { "SessionKey": "sk-alice", "LastFmUser": "lfm-alice" } } } }"#,
    )
    .await;
    let shown = get(&factory.app, "/api/admin/raw-config").await.text();
    assert!(!shown.contains("stored-secret"));
    assert!(!shown.contains("sk-alice"));
    let put = admin_write(&factory.app, Method::PUT, "/api/admin/raw-config", Some(&shown)).await;
    assert_eq!(put.status, StatusCode::OK, "{}", put.text());

    let saved = Node::parse(&factory.saved()).expect("the file parses");
    let lastfm = saved.get("LastFm").expect("LastFm");
    assert_eq!(
        lastfm.get("ApiSecret").and_then(Node::as_str),
        Some("stored-secret")
    );
    assert_eq!(
        lastfm
            .get("UserSessions")
            .and_then(|s| s.get("alice"))
            .and_then(|s| s.get("SessionKey"))
            .and_then(Node::as_str),
        Some("sk-alice")
    );
    // The whole effective document went to disk, as a Raw save writes it.
    assert_eq!(
        put.json()["bytes"].as_u64(),
        Some(factory.saved().encode_utf16().count() as u64)
    );
}

/// A form save echoes the placeholder for a secret nobody touched. That keeps what is stored;
/// it is never saved as the secret.
#[tokio::test]
async fn form_save_with_the_placeholder_keeps_the_stored_secret() {
    let factory = ScrobbleAdminFactory::new(r#"{ "LastFm": { "ApiSecret": "stored-secret" } }"#).await;
    let body =
        serde_json::json!({ "LastFm": { "ApiKey": API_KEY, "ApiSecret": SECRET_PLACEHOLDER } }).to_string();
    let save = admin_write(&factory.app, Method::POST, "/api/admin/settings", Some(&body)).await;
    assert_eq!(save.status, StatusCode::OK);
    assert!(!save.text().contains("stored-secret"));
    let saved = Node::parse(&factory.saved()).expect("the file parses");
    assert_eq!(
        saved
            .get("LastFm")
            .and_then(|l| l.get("ApiSecret"))
            .and_then(Node::as_str),
        Some("stored-secret")
    );
}

/// A session key typed onto the end of the placeholder is refused, like the secret is: saved,
/// it would be a key Last.fm rejects, and the listener would be cut off for it.
#[tokio::test]
async fn raw_config_with_a_session_key_typed_onto_the_placeholder_is_refused() {
    let stored = r#"{ "LastFm": { "UserSessions": { "alice": { "SessionKey": "sk-alice", "LastFmUser": "lfm-alice" } } } }"#;
    let factory = ScrobbleAdminFactory::new(stored).await;
    let body = format!(
        r#"{{ "LastFm": {{ "UserSessions": {{ "alice": {{ "SessionKey": "{SECRET_PLACEHOLDER}x" }} }} }} }}"#
    );
    let put = admin_write(&factory.app, Method::PUT, "/api/admin/raw-config", Some(&body)).await;
    assert_eq!(put.status, StatusCode::BAD_REQUEST);
    assert!(put.text().contains("alice"));
    assert_eq!(factory.saved(), stored);
}

/// Last.fm handed over the session but settings.json could not be written. The admin hears
/// that plainly, as a conflict, rather than as a server error. (The writer's temporary file is
/// made a folder, which no write can replace.)
#[tokio::test]
async fn finish_when_the_settings_file_cannot_be_written_says_so() {
    let factory = ScrobbleAdminFactory::new("{}").await;
    let connect = admin_write(
        &factory.app,
        Method::POST,
        "/api/admin/lastfm/scrobble/connect",
        Some(&user("alice")),
    )
    .await;
    assert_eq!(connect.status, StatusCode::OK);
    factory.last_fm.approved.store(true, Ordering::SeqCst);

    let blocker = factory.directory.join("settings.json.tmp");
    std::fs::create_dir_all(&blocker).expect("a folder in the way");
    let finish = admin_write(
        &factory.app,
        Method::POST,
        "/api/admin/lastfm/scrobble/finish",
        Some(&user("alice")),
    )
    .await;
    assert_eq!(finish.status, StatusCode::CONFLICT);
    assert!(finish.text().contains("could not be saved"), "{}", finish.text());
    assert!(!factory.saved().contains("sk-alice"));
}

/// Navidrome's address is saved without its trailing slash (and the spaces it was typed with), so
/// the stream relay never asks for `//rest/stream`; the Raw config save the same.
#[tokio::test]
async fn navidromes_address_is_saved_without_a_trailing_slash() {
    let factory = ScrobbleAdminFactory::new("{}").await;
    let url = |factory: &ScrobbleAdminFactory| {
        Node::parse(&factory.saved())
            .expect("the file parses")
            .get("Subsonic")
            .and_then(|s| s.get("Url"))
            .and_then(Node::as_str)
            .map(str::to_string)
    };
    let body = r#"{ "Subsonic": { "Url": " http://navidrome:4533/ " } }"#;
    let save = admin_write(&factory.app, Method::POST, "/api/admin/settings", Some(body)).await;
    assert_eq!(save.status, StatusCode::OK, "{}", save.text());
    assert_eq!(url(&factory).as_deref(), Some("http://navidrome:4533"));

    let body = r#"{ "Subsonic": { "Url": "http://navidrome:4533//" } }"#;
    let put = admin_write(&factory.app, Method::PUT, "/api/admin/raw-config", Some(body)).await;
    assert_eq!(put.status, StatusCode::OK, "{}", put.text());
    assert_eq!(url(&factory).as_deref(), Some("http://navidrome:4533"));

    // A section sent in another case is saved under its own name.
    let body = r#"{ "lidarr": { "BaseUrl": "http://lidarr:8686" } }"#;
    let save = admin_write(&factory.app, Method::POST, "/api/admin/settings", Some(body)).await;
    assert_eq!(save.status, StatusCode::OK, "{}", save.text());
    let saved = Node::parse(&factory.saved()).expect("the file parses");
    let sections: Vec<&String> = saved.as_object().expect("an object").keys().collect();
    assert_eq!(sections, ["Subsonic", "Lidarr"]);
}
