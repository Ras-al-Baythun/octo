use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

fn song(artist: &str, title: &str) -> OutsideRouting {
    OutsideRouting {
        kind: RoutingKind::Song,
        artist: Some(artist.into()),
        title: Some(title.into()),
        duration: Some(218),
        ..Default::default()
    }
}

#[test]
fn a_song_and_an_album_are_found_by_artist_and_name() {
    let hints = ArtworkHints::new();
    hints.remember(
        RoutingKind::Song,
        "Drake",
        "Solar Eclipse",
        "https://cdn/song.jpg",
    );
    hints.remember(
        RoutingKind::Album,
        "Drake",
        "HABIBTI (FOMO)",
        "https://cdn/album.jpg",
    );

    // Case, accents and punctuation do not matter; the length is not part of it.
    assert_eq!(
        hints.for_routing(&song("DRAKE", "solar eclipse")).as_deref(),
        Some("https://cdn/song.jpg")
    );
    let album = OutsideRouting {
        kind: RoutingKind::Album,
        artist: Some("Drake".into()),
        album: Some("HABIBTI (FOMO)".into()),
        ..Default::default()
    };
    assert_eq!(
        hints.for_routing(&album).as_deref(),
        Some("https://cdn/album.jpg")
    );
    assert_eq!(hints.for_routing(&song("Drake", "Quebec")), None);
    let artist = OutsideRouting {
        kind: RoutingKind::Artist,
        artist: Some("Drake".into()),
        ..Default::default()
    };
    assert_eq!(hints.for_routing(&artist), None);
}

#[test]
fn hints_forget_the_oldest_past_five_thousand() {
    let hints = ArtworkHints::new();
    for i in 0..=MAX_HINTS {
        hints.remember(
            RoutingKind::Song,
            "Artist",
            &format!("Song {i}"),
            "https://cdn/x.jpg",
        );
    }
    assert_eq!(hints.len(), MAX_HINTS);
    assert_eq!(hints.for_routing(&song("Artist", "Song 0")), None);
    assert!(
        hints
            .for_routing(&song("Artist", &format!("Song {MAX_HINTS}")))
            .is_some()
    );
}

#[test]
fn nothing_is_remembered_without_an_artist_a_name_or_a_link() {
    let hints = ArtworkHints::new();
    hints.remember(RoutingKind::Song, "", "Song", "https://cdn/x.jpg");
    hints.remember(RoutingKind::Song, "Artist", "", "https://cdn/x.jpg");
    hints.remember(RoutingKind::Song, "Artist", "Song", "");
    assert!(hints.is_empty());
}

#[tokio::test]
async fn a_chart_song_cover_is_the_charts_artwork() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a.jpg/1200x1200bb.jpg"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(vec![0xFF, 0xD8, 0xFF], "image/jpeg"))
        .mount(&server)
        .await;
    let hints = Arc::new(ArtworkHints::new());
    hints.remember(
        RoutingKind::Song,
        "Drake",
        "Solar Eclipse",
        &format!("{}/a.jpg/1200x1200bb.jpg", server.uri()),
    );
    let source = HintedArtwork::new(hints);

    let bytes = source
        .try_fetch(&song("Drake", "Solar Eclipse"), false)
        .await
        .unwrap();

    assert_eq!(bytes.as_ref(), &[0xFF, 0xD8, 0xFF]);
}

#[tokio::test]
async fn without_a_hint_the_cover_is_searched_as_before() {
    let source = HintedArtwork::new(Arc::new(ArtworkHints::new()));
    assert!(
        source
            .try_fetch(&song("Drake", "Solar Eclipse"), false)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn a_link_that_fails_leaves_it_to_the_other_sources() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let hints = Arc::new(ArtworkHints::new());
    hints.remember(
        RoutingKind::Song,
        "Drake",
        "Solar Eclipse",
        &format!("{}/gone.jpg", server.uri()),
    );
    assert!(
        HintedArtwork::new(hints)
            .try_fetch(&song("Drake", "Solar Eclipse"), false)
            .await
            .is_none()
    );
}
