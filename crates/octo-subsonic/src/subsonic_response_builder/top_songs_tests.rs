use std::sync::Arc;

use octo_core::models::domain::Song;
use octo_core::soulseek::SoulseekRouting;
use serde_json::{Value, json};

use super::*;
use crate::subsonic_response_builder::IdRegistry;

struct TestIds;

impl IdRegistry for TestIds {
    fn register(&self, routing: SoulseekRouting) -> String {
        format!("{:?}:{}", routing.kind, routing.artist.unwrap_or_default())
    }
}

fn builder() -> SubsonicResponseBuilder {
    SubsonicResponseBuilder::new(Arc::new(TestIds))
}

fn outside(title: &str) -> Song {
    Song {
        id: format!("ext-{title}"),
        title: title.into(),
        artist: "Drake".into(),
        album: "HABIBTI (FOMO)".into(),
        duration: Some(218),
        is_local: false,
        external_provider: Some("outside".into()),
        external_id: Some(format!("ext-{title}")),
        ..Default::default()
    }
}

fn answer(reply: &SubsonicReply) -> Value {
    serde_json::from_str(&reply.text()).unwrap()
}

#[test]
fn an_artists_list_has_no_chart_fields() {
    let song = outside("Quebec");
    let rows = [RankedRow {
        rank: 1,
        song: &song,
        plays: Some(10),
        listeners: Some(1),
        library: None,
    }];
    let header = TopSongsHeader {
        artist: Some("Drake"),
        source: "lastfm",
        ..Default::default()
    };

    let top =
        answer(&builder().create_top_songs_response(&header, &rows))["subsonic-response"]["topSongs"].clone();

    let keys: Vec<&str> = top.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(keys, vec!["artist", "source", "entry"]);
    assert_eq!(top["entry"][0]["rank"], 1);
    assert_eq!(top["entry"][0]["plays"], 10);
    assert_eq!(top["entry"][0]["inLibrary"], false);
    assert_eq!(top["entry"][0]["song"]["title"], "Quebec");
}

#[test]
fn a_chart_says_which_chart_its_name_and_country() {
    let song = outside("Solar Eclipse");
    let library = json!({"id": "nd-1", "title": "Solar Eclipse", "artist": "Drake"});
    let rows = [
        RankedRow {
            rank: 1,
            song: &song,
            plays: None,
            listeners: None,
            library: Some(&library),
        },
        RankedRow {
            rank: 2,
            song: &song,
            plays: None,
            listeners: None,
            library: None,
        },
    ];
    let header = TopSongsHeader {
        artist: None,
        source: "apple",
        chart: Some("new"),
        name: Some("Best New Songs"),
        country: Some("us"),
    };

    let response = answer(&builder().create_top_songs_response(&header, &rows));
    let top = &response["subsonic-response"]["topSongs"];

    assert_eq!(response["subsonic-response"]["type"], "octo");
    let keys: Vec<&str> = top.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["artist", "source", "chart", "name", "country", "entry"]
    );
    assert_eq!(top["artist"], Value::Null);
    assert_eq!(top["chart"], "new");
    assert_eq!(top["name"], "Best New Songs");
    assert_eq!(top["country"], "us");
    assert_eq!(top["entry"][0]["inLibrary"], true);
    assert_eq!(top["entry"][0]["song"]["id"], "nd-1");
    assert_eq!(top["entry"][1]["inLibrary"], false);
}

#[test]
fn charts_lists_each_chart_with_its_playlist_while_it_is_one() {
    let charts = [
        ChartChoiceRow {
            id: "34".into(),
            name: "Popular right now".into(),
            label: "Top songs".into(),
            kind: "overall",
            playlist: Some("og123".into()),
            on: true,
        },
        ChartChoiceRow {
            id: "20".into(),
            name: "Top Alternative".into(),
            label: "Alternative".into(),
            kind: "genre",
            playlist: None,
            on: false,
        },
    ];

    let response = answer(&builder().create_charts_response(Some("us"), &charts));
    let listed = &response["subsonic-response"]["charts"];

    assert_eq!(listed["country"], "us");
    assert_eq!(
        listed["chart"][0],
        json!({"id": "34", "name": "Popular right now", "label": "Top songs", "kind": "overall", "playlist": "og123", "on": true})
    );
    assert!(listed["chart"][1].get("playlist").is_none());
    assert_eq!(listed["chart"][1]["on"], false);
}

#[test]
fn standard_top_songs_json_is_the_library_entry_or_the_outside_song() {
    let library = json!({"id": "nd-1", "title": "Quebec", "artist": "Drake"});
    let rows = [
        TopSongRow::Library(library),
        TopSongRow::Outside(outside("Solar Eclipse")),
    ];

    let response = answer(&builder().create_standard_top_songs_response("json", &rows));
    let songs = &response["subsonic-response"]["topSongs"]["song"];

    assert_eq!(response["subsonic-response"]["openSubsonic"], true);
    assert_eq!(songs[0]["id"], "nd-1");
    assert_eq!(songs[1]["title"], "Solar Eclipse");
}

#[test]
fn standard_top_songs_xml_writes_library_songs_as_navidromes_xml() {
    let library = json!({
        "id": "nd-1", "title": "Quebec", "duration": 130, "starred": "2026-10-01T00:00:00Z",
        "genres": [{"name": "Hip-Hop"}], "isrc": ["USUM72600001"], "replayGain": {"trackGain": -6.5}
    });
    let rows = [
        TopSongRow::Library(library),
        TopSongRow::Outside(outside("Solar Eclipse")),
    ];

    let xml = builder()
        .create_standard_top_songs_response("xml", &rows)
        .text()
        .into_owned();

    assert!(xml.contains("openSubsonic=\"true\""), "{xml}");
    assert!(xml.contains("<topSongs>"), "{xml}");
    assert!(xml.contains("id=\"nd-1\""), "{xml}");
    assert!(xml.contains("isExternal=\"false\""), "{xml}");
    assert!(xml.contains("<genres name=\"Hip-Hop\""), "{xml}");
    assert!(xml.contains("<isrc>USUM72600001</isrc>"), "{xml}");
    assert!(xml.contains("<replayGain trackGain=\"-6.5\""), "{xml}");
    assert!(xml.contains("title=\"Solar Eclipse\""), "{xml}");
}

#[test]
fn the_top_songs_extension_is_listed_only_while_search_discovery_is_on() {
    let listed = |top_songs: bool| {
        let reply = builder()
            .merge_open_subsonic_extensions_with("json", None, None, true, false, top_songs, false, false);
        answer(&reply)["subsonic-response"]["openSubsonicExtensions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == "octoTopSongs")
            .cloned()
    };
    assert_eq!(listed(true).unwrap()["versions"], json!([1, 2]));
    assert!(listed(false).is_none());
    // The older call never lists it, so the answers recorded before charts stay as they were.
    let older = answer(&builder().merge_open_subsonic_extensions("json", None, None, true, false));
    assert!(!older.to_string().contains("octoTopSongs"));
}
