//! The AcoustID client, its HTTP half. The records and the reading of the answer are in
//! `octo_core::fingerprint::acoust_id_client`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use octo_core::common::dotnet;
pub use octo_core::fingerprint::acoust_id_client::{
    AcoustIdCredit, AcoustIdLookup, AcoustIdRecording, AcoustIdRelease, AcoustIdResult, AcoustIdSubmission,
    META_FIELDS,
};
use octo_core::fingerprint::acoust_id_client::{
    build_lookup_form, build_submit_form, encode_form, error_message_or, parse_lookup,
};
use octo_core::json::element::{get_string, try_get_property};
use reqwest::StatusCode;
use tracing::{debug, warn};

use super::acoust_id_rate_limit_handler::AcoustIdRateLimitHandler;
use super::acoust_id_rate_limiter::AcoustIdRateLimiter;
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::parse_json;

pub struct AcoustIdClient {
    http: Arc<AcoustIdRateLimitHandler>,
}

impl AcoustIdClient {
    pub fn new(http: Arc<AcoustIdRateLimitHandler>) -> Self {
        Self { http }
    }

    /// Per-call rather than the client's own Timeout, so Downloads:AcoustIdTimeoutSeconds
    /// takes effect without a restart. The client keeps a generous ceiling behind this.
    async fn post_within(
        &self,
        relative: &str,
        body: String,
        timeout_seconds: i32,
    ) -> anyhow::Result<HttpAnswer> {
        if timeout_seconds < 0 {
            return Err(anyhow!(
                "The value needs to be either -1 (signifying an infinite timeout), 0 or a positive integer. (Parameter 'delay')"
            ));
        }
        let timeout = Duration::from_secs(timeout_seconds as u64);
        match tokio::time::timeout(timeout, self.http.post_form(relative, body)).await {
            Ok(answer) => answer,
            Err(_) => Err(anyhow!("A task was canceled.")),
        }
    }

    /// Ask AcoustID what this fingerprint is. Returns None on any transport or parse
    /// failure, which the caller reads as "no verdict" and keeps the file.
    pub async fn lookup(
        &self,
        api_key: &str,
        fingerprint: &str,
        duration_seconds: i32,
        timeout_seconds: i32,
    ) -> Option<AcoustIdLookup> {
        // POST rather than GET: a Chromaprint fingerprint is kilobytes of base64 and would
        // blow past URL length limits.
        let body = encode_form(&build_lookup_form(api_key, fingerprint, duration_seconds));

        let attempt = async {
            let answer = self.post_within("v2/lookup", body, timeout_seconds).await?;
            if !answer.is_success() {
                // The sweep yielding to a download, not AcoustID failing.
                if AcoustIdRateLimiter::in_background_now() && answer.status == StatusCode::TOO_MANY_REQUESTS
                {
                    debug!("acoustid background lookup deferred");
                } else {
                    warn!("acoustid lookup answered {}", answer.status.as_u16());
                }
                return anyhow::Ok(None);
            }
            let doc = parse_json(&answer.body)?;
            Ok(Some(parse_lookup(&doc)?))
        };
        match attempt.await {
            Ok(lookup) => lookup,
            Err(e) => {
                // Warning, not Debug. A parse failure here reads downstream as "accept every
                // file", so a silent degradation to a no-op is the worst outcome this feature
                // can have and must be visible in the log.
                warn!("acoustid lookup failed: {e}");
                None
            }
        }
    }

    /// Send confirmed fingerprints back to AcoustID (#47). Each carries a MusicBrainz recording a
    /// person vouched for by keeping the track; nothing is ever sent on Octo's own judgment.
    /// Posted through the same client as lookups, so one 3/s budget covers both. True
    /// when AcoustID accepted the batch.
    pub async fn submit(
        &self,
        client_key: &str,
        user_key: &str,
        items: &[AcoustIdSubmission],
        timeout_seconds: i32,
    ) -> bool {
        if items.is_empty() {
            return true;
        }
        let body = encode_form(&build_submit_form(client_key, user_key, items));
        let attempt = async {
            let answer = self.post_within("v2/submit", body, timeout_seconds).await?;
            let doc = parse_json(&answer.body)?;
            let status = match try_get_property(&doc, "status")? {
                Some(s) => get_string(s)?,
                None => None,
            };
            if status.is_some_and(|s| dotnet::eq_ignore_case(s, "ok")) {
                return anyhow::Ok(true);
            }

            // "invalid user API key" arrives as a 200 with an error envelope, like lookup refusals.
            let message = error_message_or(&doc, status)?;
            warn!(
                "acoustid refused the submission: {}",
                message.as_deref().unwrap_or("(null)")
            );
            Ok(false)
        };
        match attempt.await {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!("acoustid submission failed: {e}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn client(server: &MockServer) -> AcoustIdClient {
        let base = url::Url::parse(&format!("{}/", server.uri())).expect("parses");
        let handler = AcoustIdRateLimitHandler::with_base_url(Arc::new(AcoustIdRateLimiter::new()), base);
        AcoustIdClient::new(Arc::new(handler))
    }

    /// The lookup posts the form AcoustID reads, with the meta fields space separated (a '+'
    /// on the wire), and reads the gzipped answer the `compress` field asks for.
    #[tokio::test]
    async fn lookup_posts_the_form_and_reads_a_gzipped_answer() {
        use std::io::Write;
        let server = MockServer::builder().start().await;
        let json = r#"{"status":"ok","results":[{"id":"x","score":0.9,"recordings":[{"id":"r1","title":"Song","artists":[{"name":"A"}]}]}]}"#;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(json.as_bytes()).expect("compresses");
        let compressed = gz.finish().expect("compresses");
        Mock::given(method("POST"))
            .and(path("/v2/lookup"))
            .and(body_string_contains(
                "meta=recordings+releasegroups+releases+tracks+compress+isrcs+sources",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", "gzip")
                    .set_body_bytes(compressed),
            )
            .mount(&server)
            .await;

        let lookup = client(&server)
            .await
            .lookup("key", "AQAD", 200, 10)
            .await
            .expect("answered");

        assert!(lookup.is_ok);
        assert_eq!(lookup.results[0].recordings[0].title, "Song");
        let request = &server.received_requests().await.expect("recorded")[0];
        let body = String::from_utf8_lossy(&request.body);
        assert_eq!(
            body,
            "client=key&format=json&duration=200&fingerprint=AQAD&meta=recordings+releasegroups+releases+tracks+compress+isrcs+sources"
        );
        assert_eq!(
            request.headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("application/x-www-form-urlencoded")
        );
        assert!(
            request
                .headers
                .get("accept-encoding")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("gzip"))
        );
    }

    #[tokio::test]
    async fn a_refusal_or_an_unreadable_answer_is_no_verdict() {
        let server = MockServer::start().await;
        Mock::given(path("/v2/lookup"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(path("/v2/lookup"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let client = client(&server).await;

        assert!(client.lookup("key", "AQAD", 200, 10).await.is_none());
        assert!(client.lookup("key", "AQAD", 200, 10).await.is_none());
    }

    #[tokio::test]
    async fn submit_reads_ok_from_the_body_not_the_status() {
        let server = MockServer::start().await;
        Mock::given(path("/v2/submit"))
            .and(body_string_contains("user=bad"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"status":"error","error":{"message":"invalid user API key"}}"#),
            )
            .mount(&server)
            .await;
        Mock::given(path("/v2/submit"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"status":"ok"}"#))
            .mount(&server)
            .await;
        let client = client(&server).await;
        let items = [AcoustIdSubmission {
            fingerprint: "AQAD".into(),
            duration_seconds: 200,
            recording_id: "mbid".into(),
            file_format: None,
        }];

        assert!(client.submit("key", "good", &items, 10).await);
        assert!(!client.submit("key", "bad", &items, 10).await);
        assert!(client.submit("key", "bad", &[], 10).await);
    }
}
