//! A one-star rating keeps a song off the listener's radio, for a library song and for one Octo
//! found online, and only for a caller Navidrome accepts. Also, in the built response, radio
//! songs say which source suggested them.

use octo_core::last_fm::last_fm_radio_seed_normalizer::track_key;
use octo_core::models::domain::Song;
use octo_core::outside::{OutsideRouting, RoutingKind};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::test_app::*;

/// Navidrome as the rating path sees it: a ping that passes or fails, one library song, and
/// setRating answering ok.
struct FakeNavidrome {
    ping_ok: bool,
}

impl Respond for FakeNavidrome {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let path = request.url.path();
        if path.contains("/rest/ping") && !self.ping_ok {
            return navidrome_json(
                r#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"Wrong username or password"}}}"#,
            );
        }
        if path.contains("/rest/getSong") {
            return navidrome_json(
                r#"{"subsonic-response":{"status":"ok","song":{"id":"nd-1","artist":"Library Artist","title":"Library Song","duration":200}}}"#,
            );
        }
        navidrome_json(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#)
    }
}

async fn navidrome(ping_ok: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(FakeNavidrome { ping_ok })
        .mount(&server)
        .await;
    server
}

fn rate(id: &str, rating: i32) -> String {
    format!("/rest/setRating.view?u=alice&t=token&s=salt&v=1.16.1&c=test&f=json&id={id}&rating={rating}")
}

fn online_song(state: &crate::app::AppState) -> String {
    state.external_id_registry.register(OutsideRouting {
        kind: RoutingKind::Song,
        artist: Some("Online Artist".into()),
        title: Some("Online Song".into()),
        ..Default::default()
    })
}

#[tokio::test]
async fn one_star_on_a_library_song_keeps_it_off_the_radio_and_another_rating_lifts_it() {
    let server = navidrome(true).await;
    let state = crate::app::AppState::for_tests(settings(&server.uri()));
    let app = app(&state);
    let key = track_key("Library Artist", "Library Song");

    assert!(get(&app, &rate("nd-1", 1)).await.status.is_success());
    assert!(state.last_fm_radio_state.radio_ban_keys("alice").contains(&key));

    assert!(get(&app, &rate("nd-1", 3)).await.status.is_success());
    assert!(!state.last_fm_radio_state.radio_ban_keys("alice").contains(&key));
}

#[tokio::test]
async fn one_star_on_a_song_found_online_counts_once_navidrome_accepts_the_caller() {
    let server = navidrome(true).await;
    let state = crate::app::AppState::for_tests(settings(&server.uri()));
    let id = online_song(&state);
    assert!(get(&app(&state), &rate(&id, 1)).await.status.is_success());
    assert!(
        state
            .last_fm_radio_state
            .radio_ban_keys("alice")
            .contains(&track_key("Online Artist", "Online Song"))
    );
}

#[tokio::test]
async fn one_star_on_a_song_found_online_from_a_caller_navidrome_refuses_changes_nothing() {
    let server = navidrome(false).await;
    let state = crate::app::AppState::for_tests(settings(&server.uri()));
    let id = online_song(&state);
    assert!(get(&app(&state), &rate(&id, 1)).await.status.is_success());
    assert!(state.last_fm_radio_state.radio_ban_keys("alice").is_empty());
}

#[test]
fn the_source_goes_out_in_json_and_xml_and_only_when_there_is_one() {
    use octo_subsonic::subsonic_response_builder::SUBSONIC_NAMESPACE;
    let state = crate::app::AppState::for_tests(octo_core::settings::AppSettings::default());
    let builder = &state.subsonic_response_builder;
    let mut song = Song {
        id: "s1".into(),
        title: "Roads".into(),
        artist: "Portishead".into(),
        is_local: true,
        suggested_by: Some("YouTube Music".into()),
        ..Default::default()
    };
    assert_eq!(
        builder.convert_song_to_json(&song)["octoSuggestedBy"],
        "YouTube Music"
    );
    assert_eq!(
        builder
            .convert_song_to_xml(&song, Some(SUBSONIC_NAMESPACE))
            .attribute("octoSuggestedBy"),
        Some("YouTube Music")
    );

    song.suggested_by = None;
    assert!(
        !builder
            .convert_song_to_json(&song)
            .contains_key("octoSuggestedBy")
    );
    assert!(
        builder
            .convert_song_to_xml(&song, Some(SUBSONIC_NAMESPACE))
            .attribute("octoSuggestedBy")
            .is_none()
    );
}

#[test]
fn display_names_and_back() {
    use octo_core::radio::radio_provider::*;
    for (provider, name) in [
        (LAST_FM, Some("Last.fm")),
        (YOU_TUBE_MUSIC, Some("YouTube Music")),
        (LISTEN_BRAINZ, Some("ListenBrainz")),
        (SOUNDS_ALIKE, Some("Sounds alike")),
        (LIBRARY, Some("Your library")),
        (HISTORY, None),
        ("", None),
    ] {
        assert_eq!(display_name(Some(provider)), name, "{provider}");
        if name.is_some() {
            assert_eq!(from_display_name(name), Some(provider));
        }
    }
}
