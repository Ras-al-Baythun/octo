//! The library and job endpoints through the assembled pipeline (`oneshot`): the genre
//! backfill, the Better quality page, the duplicate scan's Scan now, the update card, the genre
//! presets, and the recorded facts about sessions, request binding and the lyrics `busy`
//! collision.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use octo_core::common::Clock;
use octo_core::lyrics::{LyricsLibraryMode, LyricsLibraryRow, LyricsLibraryRun, LyricsLibraryStatus};
use octo_core::settings::{
    AppSettings, LibraryAction, LibraryActionDefinition, LibraryActionSettings, LidarrSettings,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::app::AppState;
use crate::http::pipeline::{App, build};
use crate::http::static_files::StaticAssets;
use crate::services::updates::release_check::ReleaseCheck;
use crate::services::updates::update_host::UpdateHost;

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.body))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

fn app(state: &AppState) -> App {
    build(state.clone(), &StaticAssets::default(), 0)
}

/// A request as the dashboard sends it: writes carry `X-Octo-Admin`, a body is JSON.
fn request(method: Method, uri: &str, token: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method.clone()).uri(uri);
    if method != Method::GET {
        builder = builder.header("X-Octo-Admin", "1");
    }
    if let Some(token) = token {
        builder = builder.header("X-Octo-Browse-Token", token);
    }
    match body {
        Some(body) => builder
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn send(app: &App, request: Request<Body>) -> Reply {
    let response = app.clone().oneshot(request).await.expect("the app answers");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.expect("a body").to_bytes();
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

// ---- The genre backfill -----------------------------------------------------------------------

/// A button that rewrites every tag in a music library cannot be an unauthenticated destructive
/// surface, so every backfill endpoint needs a browse session.
#[tokio::test]
async fn every_backfill_endpoint_requires_a_browse_session() {
    let state = AppState::for_tests(AppSettings::default());
    let app = app(&state);
    let cases = [
        (Method::POST, "/api/admin/genre/backfill"),
        (Method::GET, "/api/admin/genre/backfill"),
        (Method::POST, "/api/admin/genre/backfill/cancel"),
        (Method::POST, "/api/admin/genre/backfill/resume"),
        (Method::POST, "/api/admin/genre/backfill/undo"),
        // Library actions list filenames and usernames, and the resolver answers with a real
        // path, so both are gated the same way.
        (Method::GET, "/api/admin/library-actions"),
        (Method::GET, "/api/admin/library/resolve?id=abc"),
    ];
    for (method, uri) in cases {
        // Sent so the request gets past the admin write guard and reaches the session gate.
        let body = (method == Method::POST).then(|| json!({ "scope": "OctoDownloads", "dryRun": true }));
        let reply = send(&app, request(method.clone(), uri, None, body)).await;
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{method} {uri}");
        assert_eq!(
            reply.body, r#"{"error":"Sign in with your Navidrome admin account first."}"#,
            "{method} {uri}"
        );
    }
}

/// The preset is read-only and has no destructive surface, so it stays open the way the rest of
/// the settings API is.
#[tokio::test]
async fn genre_presets_is_readable_without_a_session() {
    let state = AppState::for_tests(AppSettings::default());
    let reply = send(
        &app(&state),
        request(Method::GET, "/api/admin/genre/presets", None, None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.body.contains("Hip-Hop"));
}

/// The presets are posted straight back to the settings API, so exact PascalCase keys and the
/// match mode as its NAME.
#[tokio::test]
async fn genre_presets_come_back_in_the_shape_the_settings_api_accepts() {
    let state = AppState::for_tests(AppSettings::default());
    let reply = send(
        &app(&state),
        request(Method::GET, "/api/admin/genre/presets", None, None),
    )
    .await;
    let first = &reply.json()["broad"][0];
    for key in ["Pattern", "Genre", "Enabled"] {
        assert!(first.get(key).is_some(), "{key} must be PascalCase");
    }
    assert_eq!(first["Match"], json!("Contains"));
    // The recorded first rule, byte for byte.
    assert!(reply.body.starts_with(
        r#"{"broad":[{"Id":"e7575dd6a3ed6042ec3f6649","Pattern":"trap latino","Genre":"Latin","Match":"Contains","Enabled":true},"#
    ));
}

/// An admin read from another origin carries no CORS headers.
#[tokio::test]
async fn admin_read_from_another_origin_carries_no_cors_headers() {
    let state = AppState::for_tests(AppSettings::default());
    let request = Request::builder()
        .uri("/api/admin/genre/presets")
        .header("Origin", "http://evil.example")
        .body(Body::empty())
        .unwrap();
    let reply = send(&app(&state), request).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.header("access-control-allow-origin").is_none());
}

// ---- The Better quality page's endpoints -----------------------------------------------------

/// The test state for `allowed` and `dryRun`: library actions on, Better quality on, Lidarr
/// set up.
fn page(allowed: bool, dry_run: bool) -> AppState {
    AppState::for_tests(AppSettings {
        library_actions: LibraryActionSettings {
            enabled: true,
            dry_run,
            allowed_users: vec![if allowed { "admin" } else { "someone-else" }.to_string()],
            actions: vec![LibraryActionDefinition {
                action: LibraryAction::BetterQuality,
                enabled: true,
                ..Default::default()
            }],
            ..Default::default()
        },
        lidarr: LidarrSettings {
            base_url: Some("http://127.0.0.1:1".into()),
            api_key: Some("k".into()),
            root_folder_path: Some("/music".into()),
            quality_profile_id: 1,
            metadata_profile_id: 1,
            ..Default::default()
        },
        ..Default::default()
    })
}

fn upgrade_body() -> Value {
    json!({ "songs": [{ "navidromeId": "nd-1", "title": "Teardrop" }] })
}

#[tokio::test]
async fn the_page_queues_only_for_a_signed_in_admin() {
    let state = page(true, false);
    let app = app(&state);
    let anonymous = send(
        &app,
        request(Method::POST, "/api/admin/upgrades", None, Some(upgrade_body())),
    )
    .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    for uri in ["/api/admin/upgrades", "/api/admin/lossy"] {
        assert_eq!(
            send(&app, request(Method::GET, uri, None, None)).await.status,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
    assert!(state.upgrade_queue.snapshot().is_empty());

    let token = state.browse_sessions.create("admin");
    let reply = send(
        &app,
        request(
            Method::POST,
            "/api/admin/upgrades",
            Some(&token),
            Some(upgrade_body()),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.body);
    assert_eq!(reply.body, r#"{"ok":true,"queued":1,"refused":null}"#);
    let jobs = state.upgrade_queue.snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].requested_by, "admin");
    assert_eq!(jobs[0].origin, "page");

    let doc = send(
        &app,
        request(Method::GET, "/api/admin/upgrades", Some(&token), None),
    )
    .await
    .json();
    let listed = doc["jobs"].as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["id"], json!("nd-1"));
    assert_eq!(doc["gate"]["allowed"], json!(true));
}

/// Refreshing the list is never a validation error ("1" and "true").
#[tokio::test]
async fn refreshing_the_list_is_never_a_validation_error() {
    let state = page(true, false);
    let app = app(&state);
    let token = state.browse_sessions.create("admin");
    for refresh in ["1", "true"] {
        let reply = send(
            &app,
            request(
                Method::GET,
                &format!("/api/admin/lossy?refresh={refresh}"),
                Some(&token),
                None,
            ),
        )
        .await;
        // The test host has no Navidrome admin credential, so the answer is that plain reason,
        // never a "the value is not valid" for the parameter itself.
        assert!(!reply.body.contains("errors"), "{refresh}: {}", reply.body);
        assert!(
            reply.body.contains("Navidrome admin credential"),
            "{refresh}: {}",
            reply.body
        );
    }
}

#[tokio::test]
async fn an_admin_not_on_the_allowed_list_is_refused() {
    let state = page(false, false);
    let token = state.browse_sessions.create("admin");
    let reply = send(
        &app(&state),
        request(
            Method::POST,
            "/api/admin/upgrades",
            Some(&token),
            Some(upgrade_body()),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.body.contains("allowed list"));
    assert!(state.upgrade_queue.snapshot().is_empty());
}

#[tokio::test]
async fn while_dry_run_is_on_nothing_is_queued() {
    let state = page(true, true);
    let token = state.browse_sessions.create("admin");
    let reply = send(
        &app(&state),
        request(
            Method::POST,
            "/api/admin/upgrades",
            Some(&token),
            Some(upgrade_body()),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(state.upgrade_queue.snapshot().is_empty());
}

#[tokio::test]
async fn a_write_from_another_site_is_refused_even_signed_in() {
    let state = page(true, false);
    let token = state.browse_sessions.create("admin");
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/admin/upgrades")
        .header("X-Octo-Browse-Token", token)
        .header("Content-Type", "application/json")
        .body(Body::from(upgrade_body().to_string()))
        .unwrap();
    let reply = send(&app(&state), request).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(state.upgrade_queue.snapshot().is_empty());
}

/// Every ask names a Navidrome id (`UpgradeAsk.NavidromeId` is required), checked before the
/// session; cancel and clear answer counts.
#[tokio::test]
async fn an_ask_without_an_id_is_a_validation_error_and_cancel_and_clear_count() {
    let state = page(true, false);
    let app = app(&state);
    let reply = send(
        &app,
        request(
            Method::POST,
            "/api/admin/upgrades",
            None,
            Some(json!({ "songs": [{ "title": "x" }] })),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json()["errors"],
        json!({ "Songs[0].NavidromeId": ["The NavidromeId field is required."] })
    );

    let token = state.browse_sessions.create("admin");
    send(
        &app,
        request(
            Method::POST,
            "/api/admin/upgrades",
            Some(&token),
            Some(upgrade_body()),
        ),
    )
    .await;
    let reply = send(
        &app,
        request(
            Method::POST,
            "/api/admin/upgrades/cancel",
            Some(&token),
            Some(json!({ "ids": ["nd-1", "nothing"] })),
        ),
    )
    .await;
    assert_eq!(reply.body, r#"{"ok":true,"cancelled":1}"#);
    let reply = send(
        &app,
        request(Method::POST, "/api/admin/upgrades/clear", Some(&token), None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.body.starts_with(r#"{"ok":true,"cleared":"#),
        "{}",
        reply.body
    );
}

// ---- The duplicate scan ------------------------------------------------------------------------

#[tokio::test]
async fn scan_now_with_duplicates_off_says_what_to_turn_on() {
    let state = AppState::for_tests(AppSettings::default());
    let reply = send(
        &app(&state),
        request(Method::POST, "/api/admin/duplicates/scan", None, None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("Duplicates"));
}

// ---- The update card --------------------------------------------------------------------------

/// GitHub's release list, apps' and draft releases included.
const RELEASES: &str = r####"
[
  { "tag_name": "desktop-v1.3.2", "name": "Octo for Windows and Linux 1.3.2", "draft": false, "prerelease": false, "body": "app", "html_url": "https://github.com/winters27/octo/releases/tag/desktop-v1.3.2", "published_at": "2026-10-03T10:42:57Z" },
  { "tag_name": "2026.10.05", "name": "2026.10.05", "draft": true, "prerelease": false, "body": "draft", "html_url": "", "published_at": null },
  { "tag_name": "2026.10.04", "name": "2026.10.04", "draft": false, "prerelease": true, "body": "pre", "html_url": "", "published_at": "2026-10-04T10:00:00Z" },
  { "tag_name": "android-v1.3.0", "name": "Octo for Android 1.3.0", "draft": false, "prerelease": false, "body": "app", "html_url": "", "published_at": "2026-10-03T11:20:07Z" },
  { "tag_name": "2026.10.02.1", "name": "", "draft": false, "prerelease": false, "body": "### Fix\n- one", "html_url": "https://github.com/winters27/octo/releases/tag/2026.10.02.1", "published_at": "2026-10-02T22:00:00Z" },
  { "tag_name": "2026.10.02", "name": "2026.10.02", "draft": false, "prerelease": false, "body": "two", "html_url": "https://github.com/winters27/octo/releases/tag/2026.10.02", "published_at": "2026-10-02T20:00:00Z" },
  { "tag_name": "2026.10.01", "name": "2026.10.01", "draft": false, "prerelease": false, "body": "one", "html_url": "https://github.com/winters27/octo/releases/tag/2026.10.01", "published_at": "2026-10-01T21:33:35Z" }
]
"####;

/// The update card's API, with a release check that knows a newer release.
struct UpdateFixture {
    dir: tempfile::TempDir,
    _github: MockServer,
    state: AppState,
}

impl UpdateFixture {
    async fn start(running: &str) -> UpdateFixture {
        let dir = tempfile::tempdir().expect("a temp dir");
        let update_dir = dir.path().join("update");
        std::fs::create_dir_all(&update_dir).unwrap();
        let github = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/winters27/octo/releases"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(RELEASES.as_bytes().to_vec(), "application/json"),
            )
            .mount(&github)
            .await;
        let state = AppState::for_tests(AppSettings::default());
        let check = ReleaseCheck::with_parts(
            update_dir.join("release.json"),
            reqwest::Client::new(),
            &github.uri(),
            state.settings.clone(),
            Clock::system(),
            running,
        );
        check.check(false).await;
        let mut inner = Arc::into_inner(state.inner).expect("nothing else holds a test state");
        inner.release_check = Arc::new(check);
        inner.update_host = Arc::new(UpdateHost::new(&update_dir, Clock::system()));
        UpdateFixture {
            dir,
            _github: github,
            state: AppState {
                inner: Arc::new(inner),
            },
        }
    }

    fn update_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("update")
    }

    fn helper(&self) {
        self.helper_of("2", "git");
    }

    fn helper_of(&self, version: &str, mode: &str) {
        std::fs::write(
            self.update_dir().join("helper"),
            format!("version={version}\nmode={mode}\ndir=/opt/octo\n"),
        )
        .unwrap();
    }

    fn requested(&self) -> bool {
        self.update_dir().join("request").exists()
    }

    async fn get(&self) -> Value {
        send(
            &app(&self.state),
            request(Method::GET, "/api/admin/update", None, None),
        )
        .await
        .json()
    }

    async fn post(&self, tag: &str) -> Reply {
        send(
            &app(&self.state),
            request(
                Method::POST,
                "/api/admin/update",
                None,
                Some(json!({ "tag": tag })),
            ),
        )
        .await
    }
}

fn started_now() -> String {
    format!("started={}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"))
}

#[tokio::test]
async fn the_card_says_what_is_new_and_what_to_run() {
    let f = UpdateFixture::start("2026.10.01").await;
    let view = f.get().await;
    assert_eq!(view["running"], json!("2026.10.01"));
    assert_eq!(view["updateAvailable"], json!(true));
    assert_eq!(view["standing"], json!("behind"));
    assert_eq!(view["latest"]["tag"], json!("2026.10.02.1"));
    assert_eq!(view["newer"].as_array().unwrap().len(), 2);
    assert_eq!(view["helper"]["installed"], json!(false));
    assert_eq!(
        view["command"],
        json!(
            "git fetch --tags && git checkout --detach 2026.10.02.1 && (docker compose pull octo yt-dlp-shim octo-sonic || docker compose --profile source build octo-source yt-dlp-shim-source octo-sonic-source) && docker compose up -d"
        )
    );
    assert_eq!(
        view["imageCommand"],
        json!(
            "docker compose pull octo yt-dlp-shim octo-sonic && docker compose up -d octo yt-dlp-shim octo-sonic"
        )
    );
}

#[tokio::test]
async fn an_old_helper_is_not_asked_and_the_card_says_to_reinstall_it() {
    let f = UpdateFixture::start("2026.10.01").await;
    // The helper of 2026.10.05 and before, left in place when reinstalling it failed.
    f.helper_of("1", "build");
    let reply = f.post("2026.10.02.1").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(
        reply.json()["error"]
            .as_str()
            .unwrap()
            .contains("scripts/updater/install-updater.sh")
    );
    assert!(!f.requested());
    let view = f.get().await;
    assert_eq!(view["helper"]["outdated"], json!(true));
    assert_eq!(
        view["reinstallCommand"],
        json!("scripts/updater/install-updater.sh")
    );
}

#[tokio::test]
async fn the_current_helper_is_not_called_outdated() {
    let f = UpdateFixture::start("2026.10.01").await;
    f.helper();
    let helper = f.get().await["helper"].clone();
    assert_eq!(helper["outdated"], json!(false));
    assert_eq!(helper["mode"], json!("git"));
}

#[tokio::test]
async fn without_the_helper_nothing_is_requested() {
    let f = UpdateFixture::start("2026.10.01").await;
    let reply = f.post("2026.10.02.1").await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert!(reply.json()["error"].as_str().unwrap().contains("helper"));
    assert!(!f.requested());
}

#[tokio::test]
async fn only_the_newest_release_can_be_asked_for() {
    let f = UpdateFixture::start("2026.10.01").await;
    f.helper();
    assert_eq!(f.post("2026.10.02").await.status, StatusCode::CONFLICT);
    assert!(!f.requested());
}

#[tokio::test]
async fn update_now_hands_the_release_to_the_helper_once() {
    let f = UpdateFixture::start("2026.10.01").await;
    f.helper();
    let reply = f.post("2026.10.02.1").await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.body);
    let id = reply.json()["id"].as_str().unwrap().to_string();
    let lines = std::fs::read_to_string(f.update_dir().join("request")).unwrap();
    let lines: Vec<&str> = lines.lines().collect();
    assert!(lines.contains(&format!("id={id}").as_str()));
    assert!(lines.contains(&"tag=2026.10.02.1"));
    assert!(lines.contains(&"by=dashboard"));

    assert_eq!(f.get().await["pending"], json!(true));
    assert_eq!(f.post("2026.10.02.1").await.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn nothing_is_newer_so_nothing_is_requested() {
    let f = UpdateFixture::start("2026.10.02.1").await;
    f.helper();
    assert_eq!(f.post("2026.10.02.1").await.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_page_on_another_site_cannot_start_an_update() {
    let f = UpdateFixture::start("2026.10.01").await;
    f.helper();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/admin/update")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"tag":"2026.10.02.1"}"#))
        .unwrap();
    assert_eq!(send(&app(&f.state), request).await.status, StatusCode::FORBIDDEN);
    assert!(!f.requested());
}

#[tokio::test]
async fn octo_back_on_the_release_means_the_run_finished() {
    let f = UpdateFixture::start("2026.10.02.1").await;
    std::fs::write(
        f.update_dir().join("status"),
        [
            "id=a",
            "tag=2026.10.02.1",
            "from=2026.10.01",
            "state=restarting",
            "step=Restarting Octo",
            &started_now(),
        ]
        .join("\n"),
    )
    .unwrap();
    assert_eq!(f.get().await["run"]["state"], json!("done"));
}

#[tokio::test]
async fn a_failed_run_carries_the_end_of_the_log() {
    let f = UpdateFixture::start("2026.10.01").await;
    std::fs::write(
        f.update_dir().join("status"),
        [
            "id=a",
            "tag=2026.10.02.1",
            "state=failed",
            "error=The build failed, so nothing was restarted.",
            &started_now(),
        ]
        .join("\n"),
    )
    .unwrap();
    let log: Vec<String> = (1..=60).map(|i| format!("line {i}")).collect();
    std::fs::write(f.update_dir().join("log"), log.join("\n")).unwrap();
    let run = f.get().await["run"].clone();
    assert_eq!(run["state"], json!("failed"));
    let log = run["log"].as_array().unwrap();
    assert_eq!(log.len(), 40);
    assert_eq!(log[39], json!("line 60"));
}

/// The card with checks off, byte for byte (`09-admin/update`: the relaxed encoder, so `&` and
/// `<` go out unescaped), and the refusal it gives an update. The commands and `reinstallCommand`
/// pull Octo and install helper version 2.
#[tokio::test]
async fn with_checks_off_the_card_and_the_refusal_match_the_baseline() {
    let mut settings = AppSettings::default();
    settings.updates.check = false;
    let state = AppState::for_tests(settings);
    let app = app(&state);
    let reply = send(&app, request(Method::GET, "/api/admin/update", None, None)).await;
    let running = state.release_check.running().to_string();
    assert_eq!(
        reply.body,
        format!(
            r#"{{"enabled":false,"repo":"winters27/octo","running":"{running}","latest":null,"newer":[],"updateAvailable":false,"standing":"unknown","checkedUtc":null,"error":null,"helper":{{"installed":false}},"reinstallCommand":"scripts/updater/install-updater.sh","pending":false,"pendingId":null,"unanswered":null,"run":null,"command":"git fetch --tags && git checkout --detach <release> && (docker compose pull octo yt-dlp-shim octo-sonic || docker compose --profile source build octo-source yt-dlp-shim-source octo-sonic-source) && docker compose up -d","imageCommand":"docker compose pull octo yt-dlp-shim octo-sonic && docker compose up -d octo yt-dlp-shim octo-sonic"}}"#
        )
    );
    let reply = send(
        &app,
        request(
            Method::POST,
            "/api/admin/update",
            None,
            Some(json!({ "tag": "2099.01.01.1" })),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(
        reply.body,
        r#"{"error":"Update checks are off, so Octo does not know which release is newest."}"#
    );
}

// ---- Sessions, request binding and the recorded answers ----------------------------------------

/// `browse_user` re-issues the cookie on the settings and browse endpoints; the cover-upgrade and
/// lyrics endpoints only check `signed`, so they never set it.
#[tokio::test]
async fn only_admin_controller_endpoints_renew_the_cookie() {
    let state = AppState::for_tests(AppSettings::default());
    let app = app(&state);
    let token = state.browse_sessions.create("admin");
    let with_cookie = |uri: &str| {
        Request::builder()
            .uri(uri)
            .header("Cookie", format!("octo_browse={token}"))
            .body(Body::empty())
            .unwrap()
    };
    let reply = send(&app, with_cookie("/api/admin/notices")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.body, r#"{"entries":[],"duplicateScan":null}"#);
    assert_eq!(
        reply.header("set-cookie"),
        Some(
            format!("octo_browse={token}; max-age=7776000; path=/api/admin; samesite=strict; httponly")
                .as_str()
        )
    );
    let reply = send(&app, with_cookie("/api/admin/covers/upgrade")).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.header("set-cookie").is_none());
    // A header token alone is never answered with a cookie.
    let reply = send(
        &app,
        request(Method::GET, "/api/admin/notices", Some(&token), None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.header("set-cookie").is_none());
    // A stale cookie hides a valid header.
    let stale = Request::builder()
        .uri("/api/admin/library-actions")
        .header("Cookie", "octo_browse=stale")
        .header("X-Octo-Browse-Token", token.as_str())
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, stale).await.status, StatusCode::UNAUTHORIZED);
}

/// The idle runs as the reference recording has them (`09-admin/genre-backfill`,
/// `covers-upgrade`; the genre run also says whether one is `pending`), with the music path the
/// configuration gives.
#[tokio::test]
async fn the_idle_runs_answer_the_baselines_shape() {
    let state = AppState::for_tests(AppSettings::default());
    let app = app(&state);
    let token = state.browse_sessions.create("admin");
    let reply = send(
        &app,
        request(Method::GET, "/api/admin/genre/backfill", Some(&token), None),
    )
    .await;
    // The handler-test state points Library:DownloadPath at a temp folder; the recorded
    // shape is the same with that path in place of "./downloads".
    let music_path = state
        .settings
        .raw("Library:DownloadPath")
        .expect("a test download path");
    assert_eq!(
        reply.body,
        format!(
            r#"{{"runId":"","status":"Idle","pending":false,"scope":"OctoDownloads","dryRun":true,"startedUtc":null,"finishedUtc":null,"total":0,"processed":0,"changed":0,"cleared":0,"skipped":0,"failed":0,"lastPath":null,"reason":null,"errors":[],"preview":[],"canResume":false,"canUndo":false,"settingsChanged":false,"musicPath":{}}}"#,
            octo_core::json::to_string(&music_path)
        )
    );
    let reply = send(
        &app,
        request(Method::GET, "/api/admin/covers/upgrade", Some(&token), None),
    )
    .await;
    let mut run = reply.json();
    run.as_object_mut().unwrap().remove("musicPath");
    assert_eq!(
        octo_core::json::to_string(&run),
        r#"{"runId":"","status":"Idle","scope":"OctoDownloads","mode":"Scan","dryRun":true,"smallerThan":1000,"picked":null,"soft":0,"folderCovers":true,"fullSize":false,"undo":false,"startedUtc":null,"finishedUtc":null,"total":0,"processed":0,"upgraded":0,"kept":0,"files":0,"failed":0,"lastFolder":null,"reason":null,"songsTotal":0,"songsRead":0,"albumsTotal":0,"albumsDone":0,"errors":[],"preview":[],"canResume":false,"busy":false,"canUndo":false}"#
    );
}

/// The lyrics page's status is written with the run's busy count beside `running` (it once could
/// not be written at all, two members being `busy`), and only once signed in.
#[tokio::test]
async fn the_lyrics_status_with_rows_and_a_busy_count_is_written() {
    let state = AppState::for_tests(AppSettings::default());
    state.lyrics_library_store.replace(LyricsLibraryRun {
        run_id: "r1".to_string(),
        status: LyricsLibraryStatus::Completed,
        mode: LyricsLibraryMode::Preview,
        busy: 2,
        rows: vec![LyricsLibraryRow {
            id: "a".to_string(),
            path: "/music/a.mp3".to_string(),
            artist: "A".to_string(),
            title: "T".to_string(),
            result: "found".to_string(),
            kind: Some("word".to_string()),
            source: Some("KuGou".to_string()),
            preview: vec!["first".to_string()],
            found_synced: Some("[00:01.00]secret text".to_string()),
            ..LyricsLibraryRow::default()
        }],
        ..LyricsLibraryRun::default()
    });
    let app = app(&state);
    let reply = send(
        &app,
        request(Method::GET, "/api/admin/lyrics/library", None, None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let token = state.browse_sessions.create("admin");
    let reply = send(
        &app,
        request(Method::GET, "/api/admin/lyrics/library", Some(&token), None),
    )
    .await;

    assert_eq!(reply.status, StatusCode::OK);
    let json = reply.json();
    assert_eq!(json["busy"], json!(2));
    assert_eq!(json["running"], json!(false));
    assert_eq!(json["rows"][0]["result"], json!("found"));
    // The found lyrics stay on the server.
    assert!(!reply.body.contains("secret text"));
}

/// The cover page's status, as the same kind of page.
#[tokio::test]
async fn the_cover_status_is_written() {
    let state = AppState::for_tests(AppSettings::default());
    let app = app(&state);
    let token = state.browse_sessions.create("admin");
    let reply = send(
        &app,
        request(Method::GET, "/api/admin/covers/upgrade", Some(&token), None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
}

/// Request binding runs before the action: a missing required member, a bad bool and an empty
/// body are 400s whether or not anyone is signed in; a body that is not JSON is a 415.
/// Every endpoint with a JSON body refuses a body as read into that endpoint's own shape: the
/// path it had reached, its words, where the reader stood, and the parameter's own entry first
/// (an optional body has none).
#[tokio::test]
async fn a_body_that_cannot_be_read_is_refused_as_each_endpoint_record_read_it() {
    let state = AppState::for_tests(AppSettings::default());
    let app = app(&state);
    let depth = "Expected depth to be zero at the end of the JSON payload. There is an open JSON object or array that should be closed.";
    let cases: [(&str, &str, Option<&str>, &str, String); 6] = [
        // The reference recording (17-covers-explicit/covers-start-bad-json).
        (
            "/api/admin/covers/upgrade",
            "{\"scope\": ",
            Some("request"),
            "$.scope",
            format!("{depth} Path: $.scope | LineNumber: 0 | BytePositionInLine: 10."),
        ),
        (
            "/api/admin/health/apply",
            "{\"steps\": [{\"with\": {\"a\": 1}}]}",
            Some("request"),
            "$.steps[0].with.a",
            "The JSON value could not be converted to System.String. Path: $.steps[0].with.a | LineNumber: 0 | BytePositionInLine: 27.".into(),
        ),
        (
            "/api/admin/upgrades",
            "{\"songs\": [{\"navidromeId\": \"x\", \"pick\": {\"size\": \"big\"}}]}",
            Some("request"),
            "$.songs[0].pick.size",
            "The JSON value could not be converted to System.Int64. Path: $.songs[0].pick.size | LineNumber: 0 | BytePositionInLine: 54.".into(),
        ),
        (
            "/api/admin/genre/backfill",
            "{\"dryRun\": \"yes\"}",
            Some("request"),
            "$.dryRun",
            "The JSON value could not be converted to Octo.Controllers.AdminController+GenreBackfillStartRequest. Path: $.dryRun | LineNumber: 0 | BytePositionInLine: 16.".into(),
        ),
        (
            "/api/admin/browse/auth",
            "{\"username\": [",
            Some("req"),
            "$.username",
            format!("{depth} Path: $.username | LineNumber: 0 | BytePositionInLine: 14."),
        ),
        (
            "/api/admin/find",
            "[1]",
            None,
            "$",
            "The JSON value could not be converted to Octo.Controllers.AdminDownloadsController+FindRequest. Path: $ | LineNumber: 0 | BytePositionInLine: 1.".into(),
        ),
    ];
    for (uri, body, parameter, path, message) in cases {
        let request = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("X-Octo-Admin", "1")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let reply = send(&app, request).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{uri}: {}", reply.body);
        let errors = reply.json()["errors"].clone();
        let keys: Vec<String> = errors.as_object().expect("errors").keys().cloned().collect();
        let mut expected_keys: Vec<String> = parameter.map(str::to_string).into_iter().collect();
        expected_keys.push(path.to_string());
        assert_eq!(keys, expected_keys, "{uri}");
        if let Some(parameter) = parameter {
            assert_eq!(
                errors[parameter][0],
                format!("The {parameter} field is required."),
                "{uri}"
            );
        }
        assert_eq!(errors[path][0], message.as_str(), "{uri}");
    }
}

#[tokio::test]
async fn binding_failures_come_before_the_session() {
    let state = AppState::for_tests(AppSettings::default());
    let app = app(&state);
    let reply = send(
        &app,
        request(
            Method::POST,
            "/api/admin/lyrics/review/dismiss",
            None,
            Some(json!({})),
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.header("content-type"),
        Some("application/problem+json; charset=utf-8")
    );
    assert_eq!(
        reply.json()["errors"],
        json!({ "Path": ["The Path field is required."] })
    );

    let reply = send(
        &app,
        request(
            Method::POST,
            "/api/admin/lyrics/choice",
            None,
            Some(json!({ "id": "x" })),
        ),
    )
    .await;
    assert_eq!(
        reply.json()["errors"],
        json!({ "Candidate": ["The Candidate field is required."] })
    );

    let reply = send(
        &app,
        request(
            Method::GET,
            "/api/admin/covers/upgrade/thumb/abc?found=maybe",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json()["errors"],
        json!({ "found": ["The value 'maybe' is not valid."] })
    );

    let empty = Request::builder()
        .method(Method::POST)
        .uri("/api/admin/genre/backfill")
        .header("X-Octo-Admin", "1")
        .header("Content-Type", "application/json")
        .body(Body::empty())
        .unwrap();
    let reply = send(&app, empty).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        reply.json()["errors"],
        json!({ "": ["A non-empty request body is required."], "request": ["The request field is required."] })
    );

    let plain = Request::builder()
        .method(Method::POST)
        .uri("/api/admin/update")
        .header("X-Octo-Admin", "1")
        .header("Content-Type", "text/plain")
        .body(Body::from("{}"))
        .unwrap();
    assert_eq!(send(&app, plain).await.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

/// The refusals the reference recording has for a signed-in admin with everything off.
#[tokio::test]
async fn the_refusals_match_the_baseline() {
    // The recorded stack fetches lyrics; everything else is off.
    let mut settings = AppSettings::default();
    settings.metadata.fetch_lyrics = true;
    let state = AppState::for_tests(settings);
    let app = app(&state);
    let token = state.browse_sessions.create("admin");
    let cases: [(Method, &str, Option<Value>, StatusCode, &str); 9] = [
        (
            Method::POST,
            "/api/admin/covers/upgrade",
            Some(json!({ "albums": [] })),
            StatusCode::BAD_REQUEST,
            r#"{"error":"Pick at least one album."}"#,
        ),
        (
            Method::POST,
            "/api/admin/lyrics/library",
            Some(json!({ "mode": "Preview", "picked": [] })),
            StatusCode::BAD_REQUEST,
            r#"{"error":"Pick at least one song."}"#,
        ),
        (
            Method::POST,
            "/api/admin/genre/backfill",
            Some(json!({ "dryRun": true })),
            StatusCode::BAD_REQUEST,
            r#"{"error":"Turn genre normalization on first, or a run would change nothing."}"#,
        ),
        (
            Method::POST,
            "/api/admin/upgrades",
            Some(json!({ "songs": [] })),
            StatusCode::FORBIDDEN,
            r#"{"error":"admin is not on the library actions allowed list, so Octo will not change files for them."}"#,
        ),
        (
            Method::POST,
            "/api/admin/review-sweep/start",
            None,
            StatusCode::BAD_REQUEST,
            r#"{"error":"Turn on library actions and Review, and set how many songs an hour to check, first."}"#,
        ),
        (
            Method::POST,
            "/api/admin/genre/backfill/resume",
            None,
            StatusCode::BAD_REQUEST,
            r#"{"error":"There is nothing to resume."}"#,
        ),
        (
            Method::POST,
            "/api/admin/genre/backfill/undo",
            None,
            StatusCode::BAD_REQUEST,
            r#"{"error":"There is no backfill to undo."}"#,
        ),
        (
            Method::POST,
            "/api/admin/covers/upgrade/undo",
            None,
            StatusCode::BAD_REQUEST,
            r#"{"error":"There is no cover upgrade to undo."}"#,
        ),
        (
            Method::GET,
            "/api/admin/library/resolve",
            None,
            StatusCode::BAD_REQUEST,
            r#"{"error":"Pass the Navidrome song id as ?id="}"#,
        ),
    ];
    for (method, uri, body, status, expected) in cases {
        let reply = send(&app, request(method.clone(), uri, Some(&token), body)).await;
        assert_eq!(reply.status, status, "{method} {uri}: {}", reply.body);
        assert_eq!(reply.body, expected, "{method} {uri}");
    }
    for (uri, expected) in [
        ("/api/admin/review-sweep/pause", r#"{"ok":true}"#),
        ("/api/admin/review-sweep/reset", r#"{"ok":true}"#),
        ("/api/admin/genre/backfill/cancel", r#"{"cancelling":true}"#),
        ("/api/admin/covers/upgrade/cancel", r#"{"cancelling":true}"#),
        ("/api/admin/lyrics/library/cancel", r#"{"cancelling":true}"#),
    ] {
        let reply = send(&app, request(Method::POST, uri, Some(&token), None)).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED, "{uri}");
        assert_eq!(reply.body, expected, "{uri}");
    }
    // A thumbnail nobody has is a ProblemDetails 404 that still carries the cache header.
    let reply = send(
        &app,
        request(
            Method::GET,
            "/api/admin/covers/upgrade/thumb/abc?found=true",
            Some(&token),
            None,
        ),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.header("cache-control"), Some("private, max-age=300"));
    assert_eq!(
        reply.header("content-type"),
        Some("application/problem+json; charset=utf-8")
    );
}

/// While another library job (the tidy) writes the music folder, a genre re-tag is refused
/// with that job's name; a preview, which writes nothing, still starts.
#[tokio::test]
async fn a_genre_apply_waits_for_another_library_job() {
    let settings = AppSettings {
        genre: octo_core::settings::GenreSettings {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let state = AppState::for_tests(settings);
    let app = app(&state);
    let token = state.browse_sessions.create("admin");
    let tidy = state
        .library_write_gate
        .begin_job("Tidy artists")
        .await
        .expect("free");

    let apply = send(
        &app,
        request(
            Method::POST,
            "/api/admin/genre/backfill",
            Some(&token),
            Some(json!({ "scope": "OctoDownloads", "dryRun": false })),
        ),
    )
    .await;
    assert_eq!(apply.status, StatusCode::CONFLICT, "{}", apply.body);
    assert!(
        apply.body.contains("Tidy artists is changing the music folder"),
        "{}",
        apply.body
    );

    let preview = send(
        &app,
        request(
            Method::POST,
            "/api/admin/genre/backfill",
            Some(&token),
            Some(json!({ "scope": "OctoDownloads", "dryRun": true })),
        ),
    )
    .await;
    assert_eq!(preview.status, StatusCode::ACCEPTED, "{}", preview.body);
    drop(tidy);
}
