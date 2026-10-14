//! Tests of the sync catalog page writing in JSON and XML, of `count_rows`, and of what stays
//! byte for byte.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use octo_core::models::domain::{Album, Artist, Song};
use octo_core::soulseek::SoulseekRouting;
use serde_json::Value;

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

/// A test catalog: one song, its album and its artist, all added at `added`.
struct Catalog {
    songs: Vec<Song>,
    albums: Vec<Album>,
    artists: Vec<Artist>,
    added: HashMap<String, DateTime<Utc>>,
}

fn catalog(added: DateTime<Utc>) -> Catalog {
    Catalog {
        songs: vec![Song {
            id: "cat-song".into(),
            title: "New One".into(),
            artist: "Stranger".into(),
            album: "New One".into(),
            album_id: Some("cat-album".into()),
            artist_id: Some("cat-artist".into()),
            duration: Some(200),
            ..Default::default()
        }],
        albums: vec![Album {
            id: "cat-album".into(),
            title: "New One".into(),
            artist: "Stranger".into(),
            artist_id: Some("cat-artist".into()),
            song_count: Some(1),
            ..Default::default()
        }],
        artists: vec![Artist {
            id: "cat-artist".into(),
            name: "Stranger".into(),
            album_count: Some(1),
            ..Default::default()
        }],
        added: ["cat-song", "cat-album", "cat-artist"]
            .into_iter()
            .map(|id| (id.to_string(), added))
            .collect(),
    }
}

#[test]
fn append_json_adds_rows_after_the_library_with_their_catalog_date() {
    let added = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
    let catalog = catalog(added);
    let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{"song":[{"id":"lib-1","title":"Mine"}]}}}"#;

    let output = append(
        body,
        Some("application/json"),
        "searchResult3",
        &builder(),
        &catalog.added,
        &catalog.artists,
        &catalog.albums,
        &catalog.songs,
    )
    .expect("appends");

    let parsed: Value = serde_json::from_slice(&output).expect("JSON");
    let result = &parsed["subsonic-response"]["searchResult3"];
    let ids: Vec<&str> = result["song"]
        .as_array()
        .expect("songs")
        .iter()
        .map(|row| row["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, ["lib-1", "cat-song"]);
    assert_eq!(result["song"][1]["created"], "2026-09-01T12:00:00.000Z");
    assert_eq!(result["song"][1]["albumId"], "cat-album");
    assert_eq!(result["album"][0]["id"], "cat-album");
    assert_eq!(result["artist"][0]["id"], "cat-artist");
    assert_eq!(
        count_rows(&output, Some("application/json"), "searchResult3"),
        Some((1, 1, 2))
    );
}

#[test]
fn append_json_creates_the_envelope_an_empty_page_left_out() {
    let catalog = catalog(Utc::now());
    let body = br#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;
    let output = append(
        body,
        Some("application/json"),
        "searchResult3",
        &builder(),
        &catalog.added,
        &[],
        &[],
        &catalog.songs,
    )
    .expect("appends");
    assert_eq!(
        count_rows(&output, Some("application/json"), "searchResult3"),
        Some((0, 0, 1))
    );
}

#[test]
fn append_xml_keeps_schema_order() {
    let catalog = catalog(Utc::now());
    let body = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1">
  <searchResult3><artist id="lib-ar" name="Mine"/><song id="lib-1" title="Mine"/></searchResult3>
</subsonic-response>"#;

    let output = append(
        body,
        Some("text/xml"),
        "searchResult3",
        &builder(),
        &catalog.added,
        &catalog.artists,
        &catalog.albums,
        &catalog.songs,
    )
    .expect("appends");

    let root = XElement::parse(std::str::from_utf8(&output).expect("UTF-8")).expect("XML");
    let container = root.elements().next().expect("the envelope");
    let rows: Vec<String> = container
        .elements()
        .map(|row| format!("{}:{}", row.name, row.attribute("id").expect("id")))
        .collect();
    assert_eq!(
        rows,
        [
            "artist:lib-ar",
            "artist:cat-artist",
            "album:cat-album",
            "song:lib-1",
            "song:cat-song"
        ]
    );
}

// ---- Library rows kept byte for byte, envelopes and row counts ---------------------------

/// The library's own rows keep their numbers and order as written; only the new rows follow.
#[test]
fn append_json_writes_the_library_rows_back_as_they_were() {
    let added = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap() + chrono::Duration::milliseconds(5);
    let catalog = catalog(added);
    let body = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{"artist":[{"id":"a","rate":1.50}],"album":"bad"}}}"#;
    let output = append(
        body.as_bytes(),
        Some("application/JSON"),
        "searchResult3",
        &builder(),
        &catalog.added,
        &catalog.artists,
        &catalog.albums,
        &[],
    )
    .expect("appends");
    assert_eq!(
        String::from_utf8(output).expect("UTF-8"),
        concat!(
            r#"{"subsonic-response":{"status":"ok","version":"1.16.1","searchResult3":{"artist":[{"id":"a","rate":1.50},"#,
            r#"{"id":"cat-artist","name":"Stranger","albumCount":1,"coverArt":"cat-artist","isExternal":true}],"#,
            r#""album":[{"id":"cat-album","parent":"cat-artist","isDir":true,"title":"New One","name":"New One","album":"New One","#,
            r#""artist":"Stranger","artistId":"cat-artist","songCount":1,"duration":0,"genre":"","coverArt":"cat-album","#,
            r#""created":"2026-09-01T12:00:00.005Z","mediaType":"album","displayArtist":"Stranger","releaseTypes":[],"explicitStatus":"","#,
            r#""sortName":"new one","isExternal":true}]}}}"#
        )
    );
}

#[test]
fn append_xml_stamps_the_song_and_makes_the_envelope_when_missing() {
    let added = Utc.with_ymd_and_hms(2026, 9, 1, 12, 0, 0).unwrap();
    let catalog = catalog(added);
    let body = br#"<subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"></subsonic-response>"#;
    let output = append(
        body,
        Some("application/xml"),
        "searchResult2",
        &builder(),
        &catalog.added,
        &[],
        &[],
        &catalog.songs,
    )
    .expect("appends");
    assert_eq!(
        String::from_utf8(output).expect("UTF-8"),
        concat!(
            "<subsonic-response xmlns=\"http://subsonic.org/restapi\" status=\"ok\" version=\"1.16.1\">\n",
            "  <searchResult2>\n",
            "    <song id=\"cat-song\" parent=\"cat-album\" isDir=\"false\" title=\"New One\" album=\"New One\" ",
            "artist=\"Stranger\" track=\"1\" genre=\"\" coverArt=\"cat-song\" contentType=\"audio/mp4\" suffix=\"m4a\" ",
            "duration=\"200\" bitRate=\"128\" albumId=\"cat-album\" artistId=\"cat-artist\" type=\"music\" isVideo=\"false\" ",
            "mediaType=\"song\" displayArtist=\"Stranger\" displayAlbumArtist=\"Stranger\" explicitStatus=\"\" ",
            "sortName=\"new one\" isExternal=\"true\" created=\"2026-09-01T12:00:00.000Z\" />\n",
            "  </searchResult2>\n",
            "</subsonic-response>"
        )
    );
}

#[test]
fn count_rows_reads_both_formats_and_gives_up_on_anything_else() {
    let json = br#"{"subsonic-response":{"searchResult3":{"artist":[1,2],"album":{},"song":[3]}}}"#;
    assert_eq!(
        count_rows(json, Some("application/json"), "searchResult3"),
        Some((2, 0, 1))
    );
    assert_eq!(
        count_rows(json, Some("application/json"), "searchResult2"),
        Some((0, 0, 0))
    );
    assert_eq!(count_rows(b"{}", Some("application/json"), "searchResult3"), None);
    assert_eq!(
        count_rows(b"nope", Some("application/json"), "searchResult3"),
        None
    );
    let xml = br#"<r xmlns="urn:x"><searchResult3><song/><song/><album/></searchResult3></r>"#;
    assert_eq!(count_rows(xml, None, "searchResult3"), Some((0, 1, 2)));
    assert_eq!(
        count_rows(xml, Some("text/xml"), "searchResult2"),
        Some((0, 0, 0))
    );
    assert_eq!(count_rows(b"<a>", Some("text/xml"), "searchResult3"), None);
    assert!(
        append(
            b"[1]",
            Some("application/json"),
            "searchResult3",
            &builder(),
            &HashMap::new(),
            &[],
            &[],
            &[]
        )
        .is_err()
    );
}
