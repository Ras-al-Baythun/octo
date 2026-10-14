//! Tests of the search response parsing and the search merge: the merge order, the dedup keys
//! and a search2 envelope read from XML.

use std::sync::Arc;

use octo_core::models::domain::{Album, Artist, Song};
use octo_core::models::search::SearchResult;
use octo_core::models::subsonic::ExternalPlaylist;
use octo_core::outside::OutsideRouting;
use serde_json::{Value, json};

use super::*;
use crate::subsonic_response_builder::IdRegistry;

struct TestIds;

impl IdRegistry for TestIds {
    fn register(&self, routing: OutsideRouting) -> String {
        format!("{:?}:{}", routing.kind, routing.artist.unwrap_or_default())
    }
}

fn mapper() -> SubsonicModelMapper {
    SubsonicModelMapper::new(SubsonicResponseBuilder::new(Arc::new(TestIds)))
}

fn json_rows(values: Vec<Value>) -> Vec<Row> {
    values.into_iter().map(Row::Json).collect()
}

fn xml_row(name: &str, attributes: &[(&str, &str)]) -> Row {
    let mut element = XElement::new(name);
    for (k, v) in attributes {
        element.set_attr(*k, *v);
    }
    Row::Xml(element)
}

fn external(songs: Vec<Song>, albums: Vec<Album>, artists: Vec<Artist>) -> SearchResult {
    SearchResult {
        songs,
        albums,
        artists,
    }
}

fn counts(rows: &SearchRows) -> (usize, usize, usize) {
    (rows.0.len(), rows.1.len(), rows.2.len())
}

/// Search3() also serves rest/search2 and relays there, so the upstream answers
/// under searchResult2. Reading only searchResult3 dropped every local row and the
/// response then carried nothing but discovery results.
#[test]
fn parse_search_response_json_search_result2_parses_local_rows() {
    let body = r#"{
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1",
            "searchResult2": {
                "song": [ { "id": "song1", "title": "Test Song" } ],
                "album": [ { "id": "alb1", "name": "Test Album" } ],
                "artist": [ { "id": "art1", "name": "Test Artist" } ]
            }
        }
    }"#;
    let rows = mapper().parse_search_response(body.as_bytes(), Some("application/json"));
    assert_eq!(counts(&rows), (1, 1, 1));
}

#[test]
fn parse_search_response_xml_search_result2_parses_local_rows() {
    let body = r#"<?xml version="1.0" encoding="UTF-8"?>
        <subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1">
            <searchResult2>
                <song id="song1" title="Test Song" />
                <album id="alb1" name="Test Album" />
            </searchResult2>
        </subsonic-response>"#;
    let rows = mapper().parse_search_response(body.as_bytes(), Some("application/xml"));
    assert_eq!(counts(&rows), (1, 1, 0));
}

#[test]
fn parse_search_response_json_with_songs_parses_correctly() {
    let body = r#"{
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1",
            "searchResult3": {
                "song": [
                    { "id": "song1", "title": "Test Song", "artist": "Test Artist", "album": "Test Album" }
                ]
            }
        }
    }"#;
    let rows = mapper().parse_search_response(body.as_bytes(), Some("application/json"));
    assert_eq!(counts(&rows), (1, 0, 0));
}

#[test]
fn parse_search_response_xml_with_songs_parses_correctly() {
    let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1">
    <searchResult3>
        <song id="song1" title="Test Song" artist="Test Artist" album="Test Album" />
    </searchResult3>
</subsonic-response>"#;
    let rows = mapper().parse_search_response(body.as_bytes(), Some("application/xml"));
    assert_eq!(counts(&rows), (1, 0, 0));
}

#[test]
fn parse_search_response_json_with_all_types_parses_all_correctly() {
    let body = r#"{
        "subsonic-response": {
            "status": "ok",
            "version": "1.16.1",
            "searchResult3": {
                "song": [ {"id": "song1", "title": "Song 1"} ],
                "album": [ {"id": "album1", "name": "Album 1"} ],
                "artist": [ {"id": "artist1", "name": "Artist 1"} ]
            }
        }
    }"#;
    let rows = mapper().parse_search_response(body.as_bytes(), Some("application/json"));
    assert_eq!(counts(&rows), (1, 1, 1));
}

#[test]
fn parse_search_response_xml_with_all_types_parses_all_correctly() {
    let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1">
    <searchResult3>
        <song id="song1" title="Song 1" />
        <album id="album1" name="Album 1" />
        <artist id="artist1" name="Artist 1" />
    </searchResult3>
</subsonic-response>"#;
    let rows = mapper().parse_search_response(body.as_bytes(), Some("application/xml"));
    assert_eq!(counts(&rows), (1, 1, 1));
}

#[test]
fn parse_search_response_invalid_json_returns_empty() {
    let rows = mapper().parse_search_response(b"{invalid json}", Some("application/json"));
    assert_eq!(counts(&rows), (0, 0, 0));
}

#[test]
fn parse_search_response_empty_search_result_returns_empty() {
    let body = r#"{ "subsonic-response": { "status": "ok", "version": "1.16.1", "searchResult3": {} } }"#;
    let rows = mapper().parse_search_response(body.as_bytes(), Some("application/json"));
    assert_eq!(counts(&rows), (0, 0, 0));
}

#[test]
fn merge_search_results_json_merges_songs_correctly() {
    let local = json_rows(vec![json!({"id": "local1", "title": "Local Song"})]);
    let result = external(
        vec![Song {
            id: "ext1".into(),
            title: "External Song".into(),
            ..Default::default()
        }],
        vec![],
        vec![],
    );
    let merged = mapper().merge_search_results(local, vec![], vec![], &result, &[], true, None);
    assert_eq!(merged.0.len(), 2);
}

#[test]
fn merge_search_results_json_case_insensitive_deduplication() {
    let local_artists = json_rows(vec![json!({"id": "local1", "name": "Test Artist"})]);
    let result = external(
        vec![],
        vec![],
        // Different case - should still be filtered
        vec![Artist {
            id: "ext1".into(),
            name: "test artist".into(),
            ..Default::default()
        }],
    );
    let merged = mapper().merge_search_results(vec![], vec![], local_artists, &result, &[], true, None);
    assert_eq!(merged.2.len(), 1); // Only the local artist
}

#[test]
fn merge_search_results_json_merges_external_albums() {
    let local_albums = json_rows(vec![
        json!({"id": "local1", "name": "Owned Album", "artist": "Someone"}),
    ]);
    let result = external(
        vec![],
        vec![Album {
            id: "ext1".into(),
            title: "Discovered Album".into(),
            artist: "Someone Else".into(),
            ..Default::default()
        }],
        vec![],
    );
    let merged = mapper().merge_search_results(vec![], local_albums, vec![], &result, &[], true, None);
    assert_eq!(merged.1.len(), 2);
}

fn rainbows() -> SearchResult {
    external(
        vec![],
        vec![
            Album {
                id: "ext1".into(),
                title: "in rainbows".into(),
                artist: "radiohead".into(),
                ..Default::default()
            },
            Album {
                id: "ext2".into(),
                title: "Kid A".into(),
                artist: "Radiohead".into(),
                ..Default::default()
            },
        ],
        vec![],
    )
}

/// An album you already own must not be listed twice, matched case-insensitively.
#[test]
fn merge_search_results_json_deduplicates_album_against_local() {
    let local_albums = json_rows(vec![
        json!({"id": "local1", "name": "In Rainbows", "artist": "Radiohead"}),
    ]);
    let merged = mapper().merge_search_results(vec![], local_albums, vec![], &rainbows(), &[], true, None);
    // The local one plus only the album that isn't a duplicate.
    assert_eq!(merged.1.len(), 2);
}

#[test]
fn merge_search_results_xml_deduplicates_album_against_local() {
    let local_albums = vec![xml_row(
        "album",
        &[("id", "local1"), ("name", "In Rainbows"), ("artist", "Radiohead")],
    )];
    let merged = mapper().merge_search_results(vec![], local_albums, vec![], &rainbows(), &[], false, None);
    assert_eq!(merged.1.len(), 2);
}

#[test]
fn merge_search_results_xml_merges_songs_correctly() {
    let local = vec![xml_row("song", &[("id", "local1"), ("title", "Local Song")])];
    let result = external(
        vec![Song {
            id: "ext1".into(),
            title: "External Song".into(),
            ..Default::default()
        }],
        vec![],
        vec![],
    );
    let merged = mapper().merge_search_results(local, vec![], vec![], &result, &[], false, None);
    assert_eq!(merged.0.len(), 2);
}

#[test]
fn merge_search_results_xml_deduplicates_artists() {
    let local_artists = vec![xml_row("artist", &[("id", "local1"), ("name", "Test Artist")])];
    let result = external(
        vec![],
        vec![],
        vec![
            // Same name - should be filtered
            Artist {
                id: "ext1".into(),
                name: "Test Artist".into(),
                ..Default::default()
            },
            // Different name - should be included
            Artist {
                id: "ext2".into(),
                name: "Different Artist".into(),
                ..Default::default()
            },
        ],
    );
    let merged = mapper().merge_search_results(vec![], vec![], local_artists, &result, &[], false, None);
    assert_eq!(merged.2.len(), 2); // 1 local + 1 external (duplicate filtered)
}

#[test]
fn merge_search_results_empty_local_results_returns_only_external() {
    let result = external(
        vec![Song {
            id: "ext1".into(),
            ..Default::default()
        }],
        vec![Album {
            id: "ext2".into(),
            ..Default::default()
        }],
        vec![Artist {
            id: "ext3".into(),
            name: "Artist".into(),
            ..Default::default()
        }],
    );
    let merged = mapper().merge_search_results(vec![], vec![], vec![], &result, &[], true, None);
    assert_eq!(counts(&merged), (1, 1, 1));
}

#[test]
fn merge_search_results_empty_external_results_returns_only_local() {
    let merged = mapper().merge_search_results(
        json_rows(vec![json!({"id": "local1"})]),
        json_rows(vec![json!({"id": "local2"})]),
        json_rows(vec![json!({"id": "local3", "name": "Local"})]),
        &SearchResult::default(),
        &[],
        true,
        None,
    );
    assert_eq!(counts(&merged), (1, 1, 1));
}

// ---- Merge order, dedup keys and partial reads --------------------------------------------

fn ids(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|row| match row {
            Row::Json(value) => value["id"].as_str().unwrap_or("").to_string(),
            Row::Xml(element) => element.attribute("id").unwrap_or("").to_string(),
        })
        .collect()
}

/// Locals first, outside songs you do not own next, then the library's trailing rows; an
/// outside song is left out when a leading or a trailing library row is the same song.
#[test]
fn merge_orders_songs_and_leaves_out_owned_ones_in_both_formats() {
    let result = external(
        vec![
            Song {
                id: "e-own".into(),
                artist: "Drake".into(),
                title: "Too Good (feat. Rihanna)".into(),
                ..Default::default()
            },
            Song {
                id: "e-live".into(),
                artist: "Drake".into(),
                title: "Too Good (Live)".into(),
                ..Default::default()
            },
            Song {
                id: "e-trailing".into(),
                artist: "B".into(),
                title: "Later".into(),
                ..Default::default()
            },
        ],
        vec![],
        vec![],
    );
    let json_local = json_rows(vec![
        json!({"id": "l1", "artist": "Drake feat. Rihanna", "title": "Too Good"}),
    ]);
    let json_trailing = json_rows(vec![json!({"id": "t1", "artist": "B", "title": "Later"})]);
    let merged = mapper().merge_search_results(
        json_local,
        vec![],
        vec![],
        &result,
        &[],
        true,
        Some(json_trailing),
    );
    assert_eq!(ids(&merged.0), ["l1", "e-live", "t1"]);

    let xml_local = vec![xml_row(
        "song",
        &[
            ("id", "l1"),
            ("artist", "Drake feat. Rihanna"),
            ("title", "Too Good"),
        ],
    )];
    let xml_trailing = vec![xml_row(
        "song",
        &[("id", "t1"), ("artist", "B"), ("title", "Later")],
    )];
    let merged =
        mapper().merge_search_results(xml_local, vec![], vec![], &result, &[], false, Some(xml_trailing));
    assert_eq!(ids(&merged.0), ["l1", "e-live", "t1"]);
    // Library rows are moved into the Subsonic namespace.
    let Row::Xml(first) = &merged.0[0] else {
        panic!("xml")
    };
    assert_eq!(first.namespace.as_deref(), Some(SUBSONIC_NAMESPACE));
}

#[test]
fn local_song_keys_read_both_shapes_and_is_listed_uses_them() {
    let rows = vec![
        Row::Json(json!({"artist": "Drake", "title": "Too Good"})),
        xml_row("song", &[("artist", "Air"), ("title", "La Femme d'Argent")]),
        Row::Json(json!({"artist": "", "title": "No artist"})),
    ];
    let keys = SubsonicModelMapper::local_song_keys(&rows);
    assert_eq!(keys.len(), 2);
    let song = |artist: &str, title: &str| Song {
        artist: artist.into(),
        title: title.into(),
        ..Default::default()
    };
    assert!(SubsonicModelMapper::is_listed(&song("drake", "too good"), &keys));
    assert!(SubsonicModelMapper::is_listed(
        &song("AIR", "La Femme d’Argent"),
        &keys
    ));
    assert!(!SubsonicModelMapper::is_listed(
        &song("Drake", "Too Good (Live)"),
        &keys
    ));
}

/// JSON adds an artist key even for an empty name, XML does not; an outside artist with
/// no name is then left out in JSON only.
#[test]
fn artist_dedup_differs_between_the_formats_for_an_empty_name() {
    let result = external(
        vec![],
        vec![],
        vec![Artist {
            id: "e".into(),
            name: "".into(),
            ..Default::default()
        }],
    );
    let merged = mapper().merge_search_results(
        vec![],
        vec![],
        json_rows(vec![json!({"id": "l", "name": null})]),
        &result,
        &[],
        true,
        None,
    );
    assert_eq!(ids(&merged.2), ["l"]);
    let merged = mapper().merge_search_results(
        vec![],
        vec![],
        vec![xml_row("artist", &[("id", "l"), ("name", "")])],
        &result,
        &[],
        false,
        None,
    );
    assert_eq!(ids(&merged.2), ["l", "e"]);
}

#[test]
fn playlists_join_the_albums_as_playlist_rows() {
    let playlist = ExternalPlaylist {
        id: "pl-deezer-9".into(),
        name: "Chill".into(),
        curator_name: Some("Deezer Team".into()),
        provider: "deezer".into(),
        track_count: 12,
        duration: 3000,
        cover_url: Some("http://c".into()),
        ..Default::default()
    };
    let merged = mapper().merge_search_results(
        vec![],
        vec![],
        vec![],
        &SearchResult::default(),
        std::slice::from_ref(&playlist),
        true,
        None,
    );
    assert_eq!(
        octo_core::json::to_string(merged.1[0].as_json().expect("json")),
        r#"{"id":"pl-deezer-9","name":"Chill","artist":"\uD83C\uDFB5 Deezer Deezer Team","artistId":"curator-deezer-deezer-team","genre":"Playlist","songCount":12,"duration":3000,"coverArt":"pl-deezer-9"}"#
    );
    let merged = mapper().merge_search_results(
        vec![],
        vec![],
        vec![],
        &SearchResult::default(),
        &[playlist],
        false,
        None,
    );
    assert_eq!(
        merged.1[0].as_xml().expect("xml").to_xml_string(),
        "<album id=\"pl-deezer-9\" name=\"Chill\" artist=\"\u{1F3B5} Deezer Deezer Team\" artistId=\"curator-deezer-deezer-team\" genre=\"Playlist\" songCount=\"12\" duration=\"3000\" coverArt=\"pl-deezer-9\" xmlns=\"http://subsonic.org/restapi\" />"
    );
}

#[test]
fn parse_keeps_the_rows_read_before_a_bad_one() {
    let body = r#"{"subsonic-response":{"searchResult3":{"song":[{"id":"1"},"oops",{"id":"3"}],"album":[{"id":"a"}]}}}"#;
    let rows = mapper().parse_search_response(body.as_bytes(), Some("application/json"));
    assert_eq!(counts(&rows), (1, 0, 0));
    // Not JSON by its content type: read as XML, which this is not.
    let rows = mapper().parse_search_response(body.as_bytes(), Some("text/plain"));
    assert_eq!(counts(&rows), (0, 0, 0));
    // A JSON row keeps every property and is marked as the library's.
    let body = r#"{"subsonic-response":{"searchResult3":{"song":[{"id":"1","n":1.0}]}}}"#;
    let rows = mapper().parse_search_response(body.as_bytes(), Some("application/json; charset=utf-8"));
    assert_eq!(
        octo_core::json::to_string(rows.0[0].as_json().expect("json")),
        r#"{"id":"1","n":1,"isExternal":false}"#
    );
    // An XML row is copied and marked the same way.
    let body = r#"<subsonic-response xmlns="http://subsonic.org/restapi"><searchResult3><song id="1"><genres name="x"></genres></song></searchResult3></subsonic-response>"#;
    let rows = mapper().parse_search_response(body.as_bytes(), None);
    assert_eq!(
        rows.0[0].as_xml().expect("xml").to_xml_string(),
        "<song id=\"1\" isExternal=\"false\" xmlns=\"http://subsonic.org/restapi\">\n  <genres name=\"x\"></genres>\n</song>"
    );
}

fn timed(id: &str, artist: &str, title: &str, seconds: Option<i32>) -> Song {
    Song {
        id: id.into(),
        artist: artist.into(),
        title: title.into(),
        duration: seconds,
        ..Default::default()
    }
}

#[test]
fn an_outside_song_in_another_script_or_credit_is_left_out_when_both_lengths_agree() {
    let result = external(
        vec![
            timed("e-cyr", "Эмин", "Камин", Some(211)),
            timed("e-fonsi", "Fonsi, Luis", "Despacito", Some(229)),
            timed("e-far", "Fonsi, Luis", "Despacito", Some(300)),
            timed("e-nolength", "Эмин", "Камин", None),
        ],
        vec![],
        vec![],
    );
    let json_local = json_rows(vec![
        json!({"id": "l-kamin", "artist": "Emin", "title": "Kamin", "duration": 210}),
        json!({"id": "l-fonsi", "artist": "Luis Fonsi", "title": "Despacito", "duration": 229}),
    ]);
    let merged = mapper().merge_search_results(json_local, vec![], vec![], &result, &[], true, None);
    assert_eq!(ids(&merged.0), ["l-kamin", "l-fonsi", "e-far", "e-nolength"]);

    let xml_local = vec![
        xml_row(
            "song",
            &[
                ("id", "l-kamin"),
                ("artist", "Emin"),
                ("title", "Kamin"),
                ("duration", "210"),
            ],
        ),
        xml_row(
            "song",
            &[
                ("id", "l-fonsi"),
                ("artist", "Luis Fonsi"),
                ("title", "Despacito"),
                ("duration", "229"),
            ],
        ),
    ];
    let merged = mapper().merge_search_results(xml_local, vec![], vec![], &result, &[], false, None);
    assert_eq!(ids(&merged.0), ["l-kamin", "l-fonsi", "e-far", "e-nolength"]);
}

#[test]
fn a_library_row_without_a_length_merges_by_key_alone() {
    let result = external(vec![timed("e-cyr", "Эмин", "Камин", Some(210))], vec![], vec![]);
    let local = json_rows(vec![json!({"id": "l-kamin", "artist": "Emin", "title": "Kamin"})]);
    let merged = mapper().merge_search_results(local, vec![], vec![], &result, &[], true, None);
    assert_eq!(ids(&merged.0), ["l-kamin", "e-cyr"]);
}

#[test]
fn outside_songs_are_never_merged_with_each_other_and_a_typo_stays() {
    let result = external(
        vec![
            timed("e-1", "Emin", "Kamin", Some(210)),
            timed("e-2", "Эмин", "Камин", Some(210)),
            timed("e-typo", "Imagine Dragons", "Beleiver", Some(204)),
        ],
        vec![],
        vec![],
    );
    let local = json_rows(vec![
        json!({"id": "l-believer", "artist": "Imagine Dragons", "title": "Believer", "duration": 204}),
    ]);
    let merged = mapper().merge_search_results(local, vec![], vec![], &result, &[], true, None);
    assert_eq!(ids(&merged.0), ["l-believer", "e-1", "e-2", "e-typo"]);
}
