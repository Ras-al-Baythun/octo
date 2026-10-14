//! The notification fan-out. Rendering and the per-event switches are
//! `octo_core::notifications::notification_service`.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use octo_core::notifications::notification_service::{is_enabled, render, render_for_member};
use octo_core::notifications::{NotificationEvent, NotificationEventType, NotificationTestResult};
use octo_core::settings::SettingsStore;
use tracing::warn;

use super::i_notification_sink::INotificationSink;
use crate::services::framework::http::client_builder;
use crate::services::metadata::DeezerMetadataService;

/// Fans one domain event out to every configured transport, if its toggle is on.
///
/// The one unforgivable failure here would be disturbing a download, so the public
/// entry point is fire-and-forget and the whole dispatch is caught at two levels:
/// once around the body, and once per sink so one transport being down cannot delay
/// or kill the other. Rendering happens exactly once so ntfy and Discord always say
/// the same thing.
pub struct NotificationService {
    sinks: Vec<Arc<dyn INotificationSink>>,
    /// The notification settings, read at every use.
    settings: Arc<SettingsStore>,
    deezer: Option<Arc<DeezerMetadataService>>,
}

impl NotificationService {
    /// The sinks' shared client, with a short timeout: a slow notification server must never be
    /// felt anywhere near the download path.
    pub const CLIENT_NAME: &'static str = "notifications";
    pub const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);

    /// The "notifications" named client the sinks share.
    pub fn client() -> reqwest::Client {
        client_builder()
            .timeout(Self::CLIENT_TIMEOUT)
            .build()
            .expect("the notifications client builds")
    }

    // Deezer is optional so tests can construct the service without an HTTP
    // stack; the app state passes it in production.
    pub fn new(
        sinks: Vec<Arc<dyn INotificationSink>>,
        settings: Arc<SettingsStore>,
        deezer: Option<Arc<DeezerMetadataService>>,
    ) -> Self {
        Self {
            sinks,
            settings,
            deezer,
        }
    }

    /// THE publish call. Never fails and never blocks the caller.
    pub fn notify(self: &Arc<Self>, evt: NotificationEvent) {
        // Outside a runtime there is nowhere to send from; a notification is never worth a panic.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let service = Arc::clone(self);
        runtime.spawn(async move { service.notify_internal(evt).await });
    }

    /// Awaitable core, so tests can pin behavior deterministically instead of racing a
    /// discarded task.
    pub(crate) async fn notify_internal(&self, evt: NotificationEvent) {
        let outcome = AssertUnwindSafe(self.dispatch(evt)).catch_unwind().await;
        if let Err(panic) = outcome {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("a panic");
            warn!("Notification dispatch failed: {message}");
        }
    }

    async fn dispatch(&self, evt: NotificationEvent) {
        let settings = self.settings.current();
        if !is_enabled(&settings.notifications, evt.r#type) {
            return;
        }

        let configured: Vec<&Arc<dyn INotificationSink>> =
            self.sinks.iter().filter(|sink| sink.is_configured()).collect();
        if configured.is_empty() {
            return;
        }

        // Events fired from deep in the download path (Started, Fallback,
        // Failed) only carry artist/title: the routing has no artwork. This
        // whole method is already off the caller's path, so a cached Deezer
        // lookup here is free and gives every event art instead of only the
        // ones that happened to have it in scope.
        let evt = self.ensure_cover_art(evt).await;

        let message = render(&evt);
        futures::future::join_all(configured.into_iter().map(|sink| {
            let message = &message;
            async move {
                if let Err(e) = sink.send(message).await {
                    warn!("Notification via {} failed: {e}", sink.name());
                }
            }
        }))
        .await;
    }

    /// Sends a family member's own push to their ntfy topic, in the background. Never fails
    /// and never blocks the caller. A blank or malformed topic sends nothing.
    pub fn notify_member(self: &Arc<Self>, topic: &str, evt: NotificationEvent) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let service = Arc::clone(self);
        let topic = topic.to_string();
        runtime.spawn(async move {
            if let Err(e) = service.push_to_member(&topic, &evt).await {
                warn!("A family member's push failed: {e}");
            }
        });
    }

    /// Publishes a member's push: to the owner's ntfy server when one is set (ntfy.sh
    /// otherwise), on the member's topic, and never with the owner's access token, which
    /// belongs to the owner's own topics.
    pub(crate) async fn push_to_member(&self, topic: &str, evt: &NotificationEvent) -> anyhow::Result<()> {
        let topic = topic.trim();
        if !is_member_topic(topic) {
            return Ok(());
        }
        let settings = self.settings.current();
        let root = member_push_root(&settings.notifications.ntfy_url);
        let message = render_for_member(evt);
        let mut payload = serde_json::Map::new();
        payload.insert("topic".into(), serde_json::json!(topic));
        payload.insert("title".into(), serde_json::json!(message.title));
        payload.insert(
            "message".into(),
            serde_json::json!(octo_core::notifications::ntfy_sink::build_body(&message)),
        );
        payload.insert(
            "tags".into(),
            serde_json::json!([octo_core::notifications::ntfy_sink::tag_for(message.r#type)]),
        );
        let body = octo_core::json::to_string_with(
            &serde_json::Value::Object(payload),
            octo_core::json::Options {
                escaping: octo_core::json::Escaping::Relaxed,
                indented: false,
            },
        );
        let response = member_client()
            .post(&root)
            .header(reqwest::header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(body)
            .send()
            .await?;
        super::ensure_success_status_code(response.status())
    }

    /// Admin test button. Unlike notify this awaits every sink and reports
    /// each outcome, including the transport's real error text on failure.
    pub async fn send_test(&self) -> Vec<NotificationTestResult> {
        let message = render(&NotificationEvent::new(NotificationEventType::Test));
        let mut results = Vec::new();
        for sink in &self.sinks {
            let result = |configured: bool, ok: bool, detail: String| NotificationTestResult {
                sink: sink.name().to_string(),
                configured,
                ok,
                detail,
            };
            if !sink.is_configured() {
                results.push(result(false, false, "not configured".to_string()));
                continue;
            }
            match sink.send(&message).await {
                Ok(()) => results.push(result(true, true, "delivered".to_string())),
                Err(e) => results.push(result(true, false, e.to_string())),
            }
        }
        results
    }

    /// Best-effort: fills a missing cover (and album) from Deezer's cached
    /// lookup. Any failure returns the event untouched: art is decoration, and
    /// decoration must never delay or break a notification.
    async fn ensure_cover_art(&self, evt: NotificationEvent) -> NotificationEvent {
        let Some(deezer) = &self.deezer else { return evt };
        if evt.cover_art_url.as_deref().is_some_and(|url| !url.is_empty()) {
            return evt;
        }
        let (Some(artist), Some(title)) = (
            evt.artist.as_deref().filter(|a| !a.is_empty()),
            evt.title.as_deref().filter(|t| !t.is_empty()),
        ) else {
            return evt;
        };
        let lookup = AssertUnwindSafe(deezer.enrich_track(artist, title, false, false))
            .catch_unwind()
            .await;
        let Ok(Some(meta)) = lookup else { return evt };
        NotificationEvent {
            cover_art_url: meta.album_cover_url,
            album: if evt.album.as_deref().is_none_or(str::is_empty) {
                meta.album_title
            } else {
                evt.album.clone()
            },
            ..evt
        }
    }
}

/// Whether `topic` can be an ntfy topic: 1 to 64 letters, digits, `-` and `_`.
pub fn is_member_topic(topic: &str) -> bool {
    (1..=64).contains(&topic.len())
        && topic
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The ntfy server members' pushes go to: the server of the owner's topic URL, or ntfy.sh.
pub fn member_push_root(owner_ntfy_url: &str) -> String {
    octo_core::notifications::ntfy_sink::parse_topic_url(owner_ntfy_url)
        .map(|(root, _)| root)
        .unwrap_or_else(|_| "https://ntfy.sh".to_string())
}

/// The client members' pushes go out on (the notifications timeout), made once.
fn member_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(NotificationService::client).clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use octo_core::notifications::NotificationMessage;
    use octo_core::settings::{AppSettings, NotificationSettings};
    use parking_lot::Mutex;

    #[derive(Default)]
    struct RecordingSink {
        sent: Mutex<Vec<NotificationMessage>>,
        unconfigured: bool,
        throw: bool,
    }

    #[async_trait]
    impl INotificationSink for RecordingSink {
        fn name(&self) -> &str {
            "recording"
        }

        fn is_configured(&self) -> bool {
            !self.unconfigured
        }

        async fn send(&self, message: &NotificationMessage) -> anyhow::Result<()> {
            if self.throw {
                anyhow::bail!("sink down");
            }
            self.sent.lock().push(message.clone());
            Ok(())
        }
    }

    fn build(settings: NotificationSettings, sinks: &[&Arc<RecordingSink>]) -> NotificationService {
        NotificationService::new(
            sinks
                .iter()
                .map(|sink| Arc::clone(*sink) as Arc<dyn INotificationSink>)
                .collect(),
            Arc::new(SettingsStore::for_tests(AppSettings {
                notifications: settings,
                ..Default::default()
            })),
            None,
        )
    }

    fn completed() -> NotificationEvent {
        NotificationEvent {
            artist: Some("Randy Rogers Band".into()),
            title: Some("In My Arms Instead".into()),
            format: Some("FLAC".into()),
            source: Some("Lidarr".into()),
            size_bytes: Some(34_684_600),
            ..NotificationEvent::new(NotificationEventType::DownloadCompleted)
        }
    }

    #[tokio::test]
    async fn disabled_event_types_are_dropped() {
        let sink = Arc::new(RecordingSink::default());
        let svc = build(
            NotificationSettings {
                notify_download_completed: false,
                ..Default::default()
            },
            &[&sink],
        );

        svc.notify_internal(completed()).await;

        assert!(sink.sent.lock().is_empty());
    }

    #[tokio::test]
    async fn events_fan_out_only_to_configured_sinks() {
        let on = Arc::new(RecordingSink::default());
        let off = Arc::new(RecordingSink {
            unconfigured: true,
            ..Default::default()
        });
        let svc = build(NotificationSettings::default(), &[&on, &off]);

        svc.notify_internal(completed()).await;

        assert_eq!(on.sent.lock().len(), 1);
        assert!(off.sent.lock().is_empty());
    }

    #[tokio::test]
    async fn one_sink_failing_does_not_stop_the_other() {
        let broken = Arc::new(RecordingSink {
            throw: true,
            ..Default::default()
        });
        let healthy = Arc::new(RecordingSink::default());
        let svc = build(NotificationSettings::default(), &[&broken, &healthy]);

        svc.notify_internal(completed()).await;

        assert_eq!(healthy.sent.lock().len(), 1);
    }

    #[tokio::test]
    async fn notify_never_throws() {
        let broken = Arc::new(RecordingSink {
            throw: true,
            ..Default::default()
        });
        let svc = Arc::new(build(NotificationSettings::default(), &[&broken]));

        // A throwing sink plus an event with every optional field missing: the
        // combination most likely to surface a swallowed failure.
        svc.notify_internal(NotificationEvent::new(NotificationEventType::DownloadFailed))
            .await;
        svc.notify(NotificationEvent::new(NotificationEventType::DownloadCompleted));
    }

    #[tokio::test]
    async fn test_send_reports_each_sink_separately() {
        let healthy = Arc::new(RecordingSink::default());
        let broken = Arc::new(RecordingSink {
            throw: true,
            ..Default::default()
        });
        let svc = build(NotificationSettings::default(), &[&healthy, &broken]);

        let results = svc.send_test().await;

        assert_eq!(results.len(), 2);
        assert!(results.iter().any(|r| r.ok));
        assert!(results.iter().any(|r| !r.ok && r.detail.contains("sink down")));
    }

    /// The test button lists an unconfigured sink without calling it, and `notify` outside a
    /// runtime does nothing rather than panic.
    #[test]
    fn test_send_skips_unconfigured_sinks_and_notify_needs_no_runtime() {
        let off = Arc::new(RecordingSink {
            unconfigured: true,
            ..Default::default()
        });
        let svc = Arc::new(build(NotificationSettings::default(), &[&off]));
        svc.notify(completed());

        let results = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
            .block_on(svc.send_test());
        assert_eq!(
            results,
            [NotificationTestResult {
                sink: "recording".into(),
                configured: false,
                ok: false,
                detail: "not configured".into()
            }]
        );
        assert!(off.sent.lock().is_empty());
    }

    #[tokio::test]
    async fn member_push_never_carries_owner_token() {
        use wiremock::matchers::any;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let svc = build(
            NotificationSettings {
                ntfy_url: format!("{}/owner-topic", server.uri()),
                ntfy_token: "owner-secret-token".into(),
                ..Default::default()
            },
            &[],
        );
        let evt = NotificationEvent {
            title: Some("Hyperballad".into()),
            family_member: Some("Alex".into()),
            ..NotificationEvent::new(NotificationEventType::FamilyRequestAdded)
        };
        svc.push_to_member("alex-topic-k3J9x2mQ7pL4vB8n", &evt)
            .await
            .expect("sent");
        // A topic that is not one sends nothing at all.
        svc.push_to_member("../owner-topic", &evt).await.expect("ignored");
        svc.push_to_member("", &evt).await.expect("ignored");

        let requests = server.received_requests().await.expect("recorded");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert!(request.headers.get("authorization").is_none());
        let body = String::from_utf8_lossy(&request.body).into_owned();
        assert!(
            body.contains(r#""topic":"alex-topic-k3J9x2mQ7pL4vB8n""#),
            "{body}"
        );
        assert!(!body.contains("owner-topic"), "{body}");
        assert!(!body.contains("owner-secret-token"), "{body}");
        assert!(body.contains("Added: Hyperballad is in your library"), "{body}");
        assert_eq!(request.url.path(), "/");
    }

    #[test]
    fn member_pushes_go_to_the_owner_server_or_ntfy_sh() {
        assert_eq!(
            member_push_root("https://push.example/topic"),
            "https://push.example"
        );
        assert_eq!(member_push_root(""), "https://ntfy.sh");
        assert!(is_member_topic("Abc_123-x"));
        assert!(!is_member_topic("a/b"));
        assert!(!is_member_topic(&"a".repeat(65)));
    }
}
