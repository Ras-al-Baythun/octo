//! Covers for songs outside the library come from the catalog at full size. The lookup used
//! field-qualified searches the catalog no longer answers, so every one came back empty and a
//! smaller source served a soft 600 pixel cover instead.
//!
//! The fake catalog is a mock server: `/cdn/...` stands for the image CDN and answers with its
//! own path as the bytes; anything else is the API, answered by path.

use std::collections::HashMap;

use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::*;
use crate::services::metadata::DeezerRateLimiter;

struct FakeCatalog {
    json: HashMap<&'static str, String>,
}

impl Respond for FakeCatalog {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let path = request.url.path();
        if path.starts_with("/cdn/") {
            return ResponseTemplate::new(200).set_body_bytes(path.as_bytes().to_vec());
        }
        match self.json.get(path) {
            Some(body) => ResponseTemplate::new(200)
                .insert_header("content-type", "application/json; charset=utf-8")
                .set_body_string(body.clone()),
            None => ResponseTemplate::new(404),
        }
    }
}

/// The server, and how a CDN URL on it is written in the catalog's answers.
async fn catalog(
    json: impl FnOnce(&str) -> HashMap<&'static str, String>,
) -> (MockServer, DeezerCoverArtLookup) {
    let server = MockServer::builder().start().await;
    let cdn = format!("http://localhost:{}/cdn", server.address().port());
    Mock::given(any())
        .respond_with(FakeCatalog { json: json(&cdn) })
        .mount(&server)
        .await;
    let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
    let lookup = DeezerCoverArtLookup::with_base_url(http, &MetadataSettings::default(), &server.uri());
    (server, lookup)
}

async fn asked(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

fn q(request: &Request) -> String {
    request
        .url
        .query_pairs()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

#[tokio::test]
async fn an_album_with_a_known_id_is_fetched_by_that_id() {
    let (server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/album/302127",
            format!(
                r#"{{"id":302127,"title":"Discovery","cover_xl":"{cdn}/discovery-1000.jpg","cover_big":"{cdn}/discovery-500.jpg"}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &OutsideRouting {
                kind: RoutingKind::Album,
                artist: Some("Daft Punk".into()),
                album: Some("Discovery".into()),
                external_album_id: Some("302127".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover");

    assert_eq!(&bytes[..], b"/cdn/discovery-1000.jpg");
    assert!(
        !asked(&server)
            .await
            .iter()
            .any(|r| r.url.path().starts_with("/search"))
    );
}

#[tokio::test]
async fn a_search_is_plain_and_takes_the_asked_for_title() {
    let (server, lookup) = catalog(|cdn| {
        // The same artist's other record first, the asked-for one second.
        HashMap::from([(
            "/search/album",
            format!(
                r#"{{"data":[
                    {{"title":"Homework","artist":{{"name":"Daft Punk"}},"cover_xl":"{cdn}/homework.jpg"}},
                    {{"title":"Random Access Memories (Drumless Edition)","artist":{{"name":"Daft Punk"}},"cover_xl":"{cdn}/ram-drumless.jpg"}}
                  ]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &OutsideRouting {
                kind: RoutingKind::Album,
                artist: Some("Daft Punk".into()),
                album: Some("Random Access Memories (Drumless Edition)".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover");

    assert_eq!(&bytes[..], b"/cdn/ram-drumless.jpg");
    let requests = asked(&server).await;
    let searches: Vec<&Request> = requests
        .iter()
        .filter(|r| r.url.path() == "/search/album")
        .collect();
    assert_eq!(searches.len(), 1);
    let q = q(searches[0]);
    assert!(!q.contains("artist:"));
    assert!(!q.contains("album:"));
    assert!(q.contains("Daft Punk"));
}

#[tokio::test]
async fn a_songs_cover_comes_from_a_plain_track_search() {
    let (server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/search",
            format!(
                r#"{{"data":[
                    {{"title":"Something Else","artist":{{"name":"Air"}},"album":{{"title":"Talkie Walkie","cover_xl":"{cdn}/other.jpg"}}}},
                    {{"title":"Sexy Boy","artist":{{"name":"Air"}},"album":{{"title":"Moon Safari","cover_xl":"{cdn}/moon-safari.jpg"}}}}
                  ]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &OutsideRouting {
                kind: RoutingKind::Song,
                artist: Some("Air".into()),
                title: Some("Sexy Boy".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover");

    assert_eq!(&bytes[..], b"/cdn/moon-safari.jpg");
    let requests = asked(&server).await;
    let searches: Vec<&Request> = requests.iter().filter(|r| r.url.path() == "/search").collect();
    assert_eq!(searches.len(), 1);
    assert!(!q(searches[0]).contains("track:"));
}

/// The Accept-Language the lookup was built with goes on every request, and an artist's picture
/// is taken from the best-named hit.
#[tokio::test]
async fn an_artists_picture_is_the_best_named_hit_and_the_language_is_sent() {
    let server = MockServer::builder().start().await;
    let cdn = format!("http://localhost:{}/cdn", server.address().port());
    Mock::given(any())
        .respond_with(FakeCatalog {
            json: HashMap::from([(
                "/search/artist",
                format!(
                    r#"{{"data":[{{"name":"Air Supply","picture_xl":"{cdn}/supply.jpg"}},{{"name":"Air","picture_big":"{cdn}/air.jpg"}}]}}"#
                ),
            )]),
        })
        .mount(&server)
        .await;
    let http = Arc::new(DeezerRateLimitHandler::new(Arc::new(DeezerRateLimiter::new())));
    let metadata = MetadataSettings {
        language: "fr-FR, fr".into(),
        ..Default::default()
    };
    let lookup = DeezerCoverArtLookup::with_base_url(http, &metadata, &server.uri());

    let bytes = lookup
        .try_fetch(
            &OutsideRouting {
                kind: RoutingKind::Artist,
                artist: Some("Air".into()),
                ..Default::default()
            },
            true,
        )
        .await
        .expect("a picture");

    assert_eq!(&bytes[..], b"/cdn/air.jpg");
    for request in asked(&server).await {
        assert_eq!(
            request
                .headers
                .get("accept-language")
                .and_then(|v| v.to_str().ok()),
            Some("fr-FR, fr")
        );
    }
}

/// An artist's picture goes to the hit that names the artist, in any script, never to a longer
/// name that only starts with the same letters.
#[tokio::test]
async fn an_artists_picture_is_never_a_name_that_only_starts_the_same() {
    let (_server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/search/artist",
            format!(
                r#"{{"data":[{{"name":"Kinoman","picture_xl":"{cdn}/kinoman.jpg"}},{{"name":"Кино","picture_xl":"{cdn}/kino.jpg"}}]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &OutsideRouting {
                kind: RoutingKind::Artist,
                artist: Some("Kino".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a picture");

    assert_eq!(&bytes[..], b"/cdn/kino.jpg");
}

/// A song search takes the hit credited to the artist written another way over one by an artist
/// whose name merely holds the same letters.
#[tokio::test]
async fn a_songs_cover_follows_the_artist_written_another_way() {
    let (_server, lookup) = catalog(|cdn| {
        HashMap::from([(
            "/search",
            format!(
                r#"{{"data":[
                    {{"title":"Despacito","artist":{{"name":"Luis Fonsiman"}},"album":{{"title":"Covers","cover_xl":"{cdn}/other.jpg"}}}},
                    {{"title":"Despacito","artist":{{"name":"Fonsi, Luis"}},"album":{{"title":"Vida","cover_xl":"{cdn}/vida.jpg"}}}}
                  ]}}"#
            ),
        )])
    })
    .await;

    let bytes = lookup
        .try_fetch(
            &OutsideRouting {
                kind: RoutingKind::Song,
                artist: Some("Luis Fonsi".into()),
                title: Some("Despacito".into()),
                ..Default::default()
            },
            false,
        )
        .await
        .expect("a cover");

    assert_eq!(&bytes[..], b"/cdn/vida.jpg");
}
