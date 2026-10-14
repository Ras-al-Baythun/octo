//! The Discord webhook sink. The payload is
//! `octo_core::notifications::discord_sink::build_payload`.

use std::sync::Arc;

use async_trait::async_trait;
use octo_core::common::dotnet;
use octo_core::notifications::discord_sink::build_payload;
use octo_core::settings::SettingsStore;
use reqwest::header::CONTENT_TYPE;

use super::ensure_success_status_code;
use super::i_notification_sink::{INotificationSink, NotificationMessage};

/// Discord webhook transport. One rich embed and no top-level content, which both
/// looks better (thumbnail album art) and sidesteps the 2000-character content
/// limit entirely; a request with embeds and no content is valid per the API.
pub struct DiscordSink {
    /// The "notifications" named client.
    http: reqwest::Client,
    /// The notification settings, read at every use.
    settings: Arc<SettingsStore>,
}

impl DiscordSink {
    pub fn new(http: reqwest::Client, settings: Arc<SettingsStore>) -> Self {
        Self { http, settings }
    }
}

#[async_trait]
impl INotificationSink for DiscordSink {
    fn name(&self) -> &str {
        "discord"
    }

    fn is_configured(&self) -> bool {
        !dotnet::is_blank(&self.settings.current().notifications.discord_webhook_url)
    }

    async fn send(&self, message: &NotificationMessage) -> anyhow::Result<()> {
        let url = self.settings.current().notifications.discord_webhook_url.clone();
        let body = octo_core::json::to_string(&build_payload(message, chrono::Utc::now()));
        let response = self
            .http
            .post(&url)
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .body(body)
            .send()
            .await?;
        ensure_success_status_code(response.status())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::test_support::received;
    use octo_core::notifications::NotificationEventType;
    use octo_core::settings::{AppSettings, NotificationSettings};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn post_goes_to_the_configured_webhook_as_json() {
        let server = MockServer::builder().start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let webhook = format!("{}/api/webhooks/1/abc", server.uri());
        let sink = DiscordSink::new(
            reqwest::Client::new(),
            Arc::new(SettingsStore::for_tests(AppSettings {
                notifications: NotificationSettings {
                    discord_webhook_url: webhook.clone(),
                    ..Default::default()
                },
                ..Default::default()
            })),
        );

        sink.send(&NotificationMessage::new(
            NotificationEventType::DownloadCompleted,
            "Downloaded: A \u{2013} B",
            "FLAC via Lidarr, 33.1 MB",
            Some("https://cdn.example/cover.jpg"),
        ))
        .await
        .expect("sent");

        let requests = received(&server).await;
        let request = requests.first().expect("a request");
        assert_eq!(request.url.path(), "/api/webhooks/1/abc");
        let content_type = request
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(content_type.split(';').next(), Some("application/json"));
        // The default encoder: non-ASCII written as escapes, as JsonObject.ToJsonString() did.
        assert!(String::from_utf8_lossy(&request.body).contains(r"Downloaded: A \u2013 B"));
    }
}
