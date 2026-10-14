//! Cover art from Last.fm.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use octo_core::common::dotnet;
use octo_core::json::element::{enumerate_array, get_string, try_get_property};
use octo_core::metadata::accept_language_header;
use octo_core::outside::routing::{OutsideRouting, RoutingKind};
use octo_core::settings::{LastFmSettings, MetadataSettings};
use reqwest::header::{ACCEPT_LANGUAGE, HeaderMap, HeaderValue};
use serde_json::Value;
use tracing::debug;

use super::i_cover_art_source::ICoverArtSource;
use crate::services::framework::HttpAnswer;
use crate::services::framework::http::client_builder;

/// Cover art via Last.fm's `track.getInfo` / `album.getInfo`. Uses
/// the API key Octo already has configured for radio. Coverage overlaps a lot
/// with iTunes/Deezer: included as the third fallback so we still get
/// something for tracks the other two whiffed (often live recordings, remixes,
/// or very recent releases that haven't been indexed yet by streaming APIs).
///
/// Notable wart: Last.fm *artist* images were officially deprecated in
/// 2019: the API now returns a star-shaped placeholder for `artist.getInfo`
/// images. We don't bother calling that endpoint for artist routings.
pub struct LastFmCoverArtLookup {
    client: reqwest::Client,
    /// The Last.fm API key, captured at construction, deliberately.
    api_key: String,
    base_url: String,
}

impl LastFmCoverArtLookup {
    pub const BASE_URL: &'static str = "https://ws.audioscrobbler.com/2.0/";

    pub fn new(last_fm: &LastFmSettings, metadata: &MetadataSettings) -> Self {
        Self::with_base_url(last_fm, metadata, Self::BASE_URL)
    }

    /// A lookup that asks another host: a test's mock server.
    pub fn with_base_url(last_fm: &LastFmSettings, metadata: &MetadataSettings, base_url: &str) -> Self {
        // The Accept-Language is captured once, on the client, at construction.
        let mut headers = HeaderMap::new();
        if let Some(language) = accept_language_header::header_value(metadata)
            && let Ok(value) = HeaderValue::from_str(&language)
        {
            headers.insert(ACCEPT_LANGUAGE, value);
        }
        let client = client_builder()
            .timeout(Duration::from_secs(8))
            .default_headers(headers)
            .build()
            .expect("the Last.fm cover client builds");
        Self {
            client,
            api_key: last_fm.api_key.clone(),
            base_url: base_url.to_string(),
        }
    }

    async fn fetch(&self, routing: &OutsideRouting) -> anyhow::Result<Option<Bytes>> {
        let artist = routing.artist.as_deref().unwrap_or("").trim();
        let img_url = match routing.kind {
            RoutingKind::Album => {
                let album = routing
                    .album
                    .as_deref()
                    .or(routing.title.as_deref())
                    .unwrap_or("")
                    .trim();
                self.get_album_image_url(artist, album).await?
            }
            // Last.fm artist.getInfo returns the placeholder star, never a real image
            RoutingKind::Artist => None,
            RoutingKind::Song => {
                self.get_track_image_url(artist, routing.title.as_deref().unwrap_or("").trim())
                    .await?
            }
        };
        let Some(img_url) = img_url.filter(|u| !u.is_empty()) else {
            return Ok(None);
        };

        let answer = HttpAnswer::read(self.client.get(&img_url).send().await?).await?;
        Ok(answer.is_success().then_some(answer.body))
    }

    async fn get_track_image_url(&self, artist: &str, title: &str) -> anyhow::Result<Option<String>> {
        if title.is_empty() {
            return Ok(None);
        }
        let url = format!(
            "{}?method=track.getInfo&artist={}&track={}&api_key={}&format=json&autocorrect=1",
            self.base_url,
            dotnet::escape_data_string(artist),
            dotnet::escape_data_string(title),
            self.api_key
        );
        self.pick_image_from_response(&url, "track", Some("album")).await
    }

    async fn get_album_image_url(&self, artist: &str, album: &str) -> anyhow::Result<Option<String>> {
        if album.is_empty() {
            return Ok(None);
        }
        let url = format!(
            "{}?method=album.getInfo&artist={}&album={}&api_key={}&format=json&autocorrect=1",
            self.base_url,
            dotnet::escape_data_string(artist),
            dotnet::escape_data_string(album),
            self.api_key
        );
        // The album response has the image array directly under <album>.
        let direct = self.pick_image_from_response(&url, "album", None).await?;
        if direct.as_deref().is_some_and(|d| !d.is_empty()) {
            return Ok(direct);
        }
        // Singles often don't have an album entry: fall back to track-level lookup
        // using the album name as if it were the title.
        self.get_track_image_url(artist, album).await
    }

    /// Walks the Last.fm response to find the best image URL. Path differs
    /// depending on which endpoint we hit:
    ///   track.getInfo  → response.track.album.image[]
    ///   album.getInfo  → response.album.image[]
    /// The image[] array has entries with sizes "small"/"medium"/"large"/
    /// "extralarge"/"mega"; we pick the largest non-empty one.
    async fn pick_image_from_response(
        &self,
        url: &str,
        outer_key: &str,
        inner_key: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        let answer = HttpAnswer::read(self.client.get(url).send().await?).await?;
        if !answer.is_success() {
            return Ok(None);
        }
        let doc: Value = serde_json::from_str(&answer.text())?;
        pick_image(&doc, outer_key, inner_key)
    }
}

fn pick_image(doc: &Value, outer_key: &str, inner_key: Option<&str>) -> anyhow::Result<Option<String>> {
    let Some(outer) = try_get_property(doc, outer_key)? else {
        return Ok(None);
    };
    let image_host = match inner_key {
        Some(inner_key) => match try_get_property(outer, inner_key)? {
            Some(inner) => inner,
            None => return Ok(None),
        },
        None => outer,
    };
    let Some(images @ Value::Array(_)) = try_get_property(image_host, "image")? else {
        return Ok(None);
    };

    let size_ranking = |size: &str| match size {
        "mega" => 5,
        "extralarge" => 4,
        "large" => 3,
        "medium" => 2,
        "small" => 1,
        _ => 0,
    };
    let mut best = None;
    let mut best_score = -1;
    for img in enumerate_array(images)? {
        let size = match try_get_property(img, "size")? {
            Some(s) => get_string(s)?.unwrap_or(""),
            None => "",
        };
        let text = match try_get_property(img, "#text")? {
            Some(t) => get_string(t)?.unwrap_or(""),
            None => "",
        };
        if text.is_empty() {
            continue;
        }
        // Last.fm's "deprecated artist image" placeholder is hosted at
        // /i/u/2a96cbd8b46e442fc41c2b86b821562f.png: explicitly skip it
        // so we don't return a star icon as cover art.
        if text.contains("2a96cbd8b46e442fc41c2b86b821562f") {
            continue;
        }
        let rank = size_ranking(size);
        if rank > best_score {
            best_score = rank;
            best = Some(text.to_string());
        }
    }
    Ok(best)
}

#[async_trait]
impl ICoverArtSource for LastFmCoverArtLookup {
    fn name(&self) -> &str {
        "lastfm"
    }

    async fn try_fetch(&self, routing: &OutsideRouting, _background: bool) -> Option<Bytes> {
        if self.api_key.is_empty() {
            return None;
        }
        if routing.artist.as_deref().unwrap_or("").trim().is_empty() {
            return None;
        }
        match self.fetch(routing).await {
            Ok(bytes) => bytes,
            Err(e) => {
                debug!(
                    "lastfm lookup failed for {}/{}/{}: {e}",
                    routing.artist.as_deref().unwrap_or(""),
                    routing.title.as_deref().unwrap_or(""),
                    routing.album.as_deref().unwrap_or("")
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{any, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// An album with no image of its own falls back to the track lookup, the largest
    /// real image wins, and the star placeholder never does.
    #[tokio::test]
    async fn the_largest_real_image_wins_and_an_album_falls_back_to_the_track() {
        let server = MockServer::builder().start().await;
        let img = |name: &str| format!("{}/img/{name}", server.uri());
        Mock::given(query_param("method", "album.getInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"album":{"image":[]}}"#))
            .mount(&server)
            .await;
        Mock::given(query_param("method", "track.getInfo"))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!(
                r##"{{"track":{{"album":{{"image":[
                    {{"size":"small","#text":"{}"}},
                    {{"size":"mega","#text":"https://x/i/u/2a96cbd8b46e442fc41c2b86b821562f.png"}},
                    {{"size":"extralarge","#text":"{}"}},
                    {{"size":"large","#text":""}}]}}}}}}"##,
                img("small.jpg"),
                img("xl.jpg")
            )))
            .mount(&server)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string("xl-bytes"))
            .mount(&server)
            .await;
        let last_fm = LastFmSettings {
            api_key: "key".into(),
            ..Default::default()
        };
        let lookup = LastFmCoverArtLookup::with_base_url(
            &last_fm,
            &MetadataSettings::default(),
            &format!("{}/2.0/", server.uri()),
        );

        let bytes = lookup
            .try_fetch(
                &OutsideRouting {
                    kind: RoutingKind::Album,
                    artist: Some("Air".into()),
                    album: Some("Sexy Boy".into()),
                    ..Default::default()
                },
                false,
            )
            .await
            .expect("an image");

        assert_eq!(&bytes[..], b"xl-bytes");
        let requests = server.received_requests().await.unwrap_or_default();
        assert_eq!(
            requests.last().map(|r| r.url.path().to_string()).as_deref(),
            Some("/img/xl.jpg")
        );
        assert!(
            requests[0]
                .url
                .as_str()
                .contains("api_key=key&format=json&autocorrect=1")
        );

        // No key, no lookups at all.
        let keyless = LastFmCoverArtLookup::with_base_url(
            &LastFmSettings::default(),
            &MetadataSettings::default(),
            &server.uri(),
        );
        let before = server.received_requests().await.unwrap_or_default().len();
        assert!(
            keyless
                .try_fetch(
                    &OutsideRouting {
                        artist: Some("Air".into()),
                        title: Some("x".into()),
                        ..Default::default()
                    },
                    false
                )
                .await
                .is_none()
        );
        assert_eq!(server.received_requests().await.unwrap_or_default().len(), before);
    }
}
