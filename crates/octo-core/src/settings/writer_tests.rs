//! `SettingsFileWriter` tests, plus golden checks of the written format.
//!
//! Every dashboard save goes through this writer, so the ways it can lose a user's settings are
//! pinned here: merging must keep what the patch does not name, an unreadable file must be
//! refused rather than replaced, and a dictionary setting must be replaceable as a whole.

use super::*;

struct Dir {
    _tmp: tempfile::TempDir,
    path: PathBuf,
}

fn settings_dir() -> Dir {
    let tmp = tempfile::tempdir().expect("temp dir");
    let path = tmp.path().join("settings.json");
    Dir { _tmp: tmp, path }
}

fn obj(json: &str) -> JsonObject {
    match Node::parse(json).expect("valid JSON") {
        Node::Object(map) => map,
        other => panic!("not an object: {other:?}"),
    }
}

fn saved(path: &Path) -> Node {
    Node::parse(&std::fs::read_to_string(path).expect("saved file")).expect("saved JSON")
}

fn at<'a>(node: &'a Node, path: &[&str]) -> Option<&'a Node> {
    path.iter().try_fold(node, |n, key| n.get(key))
}

#[test]
fn merge_keeps_keys_the_patch_does_not_name() {
    let dir = settings_dir();
    std::fs::write(
        &dir.path,
        r#"{ "Lidarr": { "Password": "kept" }, "LastFm": { "ApiKey": "old" } }"#,
    )
    .unwrap();
    let writer = SettingsFileWriter::new(&dir.path);

    writer
        .merge(&obj(r#"{"LastFm": {"ApiKey": "new"}}"#), &[])
        .unwrap();

    let saved = saved(&dir.path);
    assert_eq!(
        at(&saved, &["Lidarr", "Password"]).and_then(Node::as_str),
        Some("kept")
    );
    assert_eq!(
        at(&saved, &["LastFm", "ApiKey"]).and_then(Node::as_str),
        Some("new")
    );
}

/// Before this, an unparseable file read as empty and the next save wrote only that one
/// form's fields, silently discarding everything else the user had saved.
#[test]
fn merge_refuses_a_corrupt_file_and_leaves_it_untouched() {
    let dir = settings_dir();
    let broken = r#"{ "Lidarr": { "Password": "kept" "#;
    std::fs::write(&dir.path, broken).unwrap();
    let writer = SettingsFileWriter::new(&dir.path);

    let result = writer.merge(&obj(r#"{"LastFm": {"ApiKey": "new"}}"#), &[]);
    assert!(
        matches!(result, Err(SettingsWriteError::Corrupt(_))),
        "{result:?}"
    );
    assert_eq!(std::fs::read_to_string(&dir.path).unwrap(), broken);
    assert!(!writer.is_readable());
}

/// The configuration provider accepts comments and trailing commas, so a file the
/// user annotated by hand is valid for Octo and must stay saveable.
#[test]
fn merge_accepts_comments_and_trailing_commas() {
    let dir = settings_dir();
    std::fs::write(
        &dir.path,
        "{\n  // set by hand\n  \"Lidarr\": { \"Password\": \"kept\", },\n}\n",
    )
    .unwrap();
    let writer = SettingsFileWriter::new(&dir.path);

    assert!(writer.is_readable());
    writer
        .merge(&obj(r#"{"LastFm": {"ApiKey": "new"}}"#), &[])
        .unwrap();

    let saved = saved(&dir.path);
    assert_eq!(
        at(&saved, &["Lidarr", "Password"]).and_then(Node::as_str),
        Some("kept")
    );
}

/// A merge can only add dictionary keys. Removing a ListenBrainz per-user token in the
/// dashboard has to replace the whole dictionary or the removed user keeps scrobbling.
#[test]
fn merge_replaces_a_listed_object_instead_of_merging_it() {
    let dir = settings_dir();
    std::fs::write(
        &dir.path,
        r#"{ "ListenBrainz": { "Token": "t", "UserTokens": { "alice": "a", "bob": "b" } } }"#,
    )
    .unwrap();
    let writer = SettingsFileWriter::new(&dir.path);

    writer
        .merge(
            &obj(r#"{"ListenBrainz": {"UserTokens": {"alice": "a"}}}"#),
            &["ListenBrainz.UserTokens"],
        )
        .unwrap();

    let saved = saved(&dir.path);
    let tokens = at(&saved, &["ListenBrainz", "UserTokens"])
        .and_then(Node::as_object)
        .expect("tokens");
    assert!(tokens.contains_key("alice"));
    assert!(!tokens.contains_key("bob"));
    assert_eq!(
        at(&saved, &["ListenBrainz", "Token"]).and_then(Node::as_str),
        Some("t")
    );
}

#[test]
fn is_readable_false_only_for_unparseable_content() {
    let dir = settings_dir();
    let writer = SettingsFileWriter::new(&dir.path);
    assert!(writer.is_readable()); // missing

    std::fs::write(&dir.path, "   ").unwrap();
    assert!(writer.is_readable()); // empty

    std::fs::write(&dir.path, "[1, 2]").unwrap();
    assert!(!writer.is_readable()); // valid JSON, but not an object

    std::fs::write(&dir.path, "{}").unwrap();
    assert!(writer.is_readable());
}

#[test]
fn replace_writes_exactly_the_given_document() {
    let dir = settings_dir();
    std::fs::write(&dir.path, r#"{ "Old": { "Key": 1 } }"#).unwrap();
    let writer = SettingsFileWriter::new(&dir.path);

    writer.replace(&obj(r#"{"New": {"Key": 2}}"#)).unwrap();

    let saved = saved(&dir.path);
    assert!(saved.get("Old").is_none());
    assert_eq!(at(&saved, &["New", "Key"]), Some(&Node::Number("2".into())));
}

// ---- golden format and edge cases ----------------------------------------------------------

/// The documented sample: a POST of the Subsonic form over a file holding LibraryActions and a
/// Last.fm session. Non-ASCII and the HTML-sensitive characters come out \u-escaped, as the
/// fixtures/state/settings.json capture holds them.
#[test]
fn merge_output_matches_the_documented_sample_byte_for_byte() {
    let dir = settings_dir();
    std::fs::write(
        &dir.path,
        concat!(
            r#"{"LibraryActions":{"Enabled":true,"PlaylistPrefix":"🛠 ","NoticePrefix":"▸ ","#,
            r#""AllowedUsers":["alice"],"Actions":[]},"#,
            r#""LastFm":{"UserSessions":{"alice":{"SessionKey":"0123456789abcdef","LastFmUser":"alice_fm"}}}}"#
        ),
    )
    .unwrap();
    let writer = SettingsFileWriter::new(&dir.path);

    writer
        .merge(
            &obj(r#"{"Subsonic":{"Url":"http://nd:4533","AdminPassword":"p&ss+w'rd"}}"#),
            &["ListenBrainz.UserTokens"],
        )
        .unwrap();

    let expected = r#"{
  "LibraryActions": {
    "Enabled": true,
    "PlaylistPrefix": "%uD83D%uDEE0 ",
    "NoticePrefix": "%u25B8 ",
    "AllowedUsers": [
      "alice"
    ],
    "Actions": []
  },
  "LastFm": {
    "UserSessions": {
      "alice": {
        "SessionKey": "0123456789abcdef",
        "LastFmUser": "alice_fm"
      }
    }
  },
  "Subsonic": {
    "Url": "http://nd:4533",
    "AdminPassword": "p%u0026ss%u002Bw%u0027rd"
  }
}"#
    // Written with a placeholder so the escapes survive editors that decode them.
    .replace("%u", "\\u");
    let bytes = std::fs::read(&dir.path).unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap(), expected);
    assert!(
        !dir.path.with_extension("json.tmp").exists(),
        "the temp file is renamed away"
    );
}

/// The reference settings.json fixture round-trips through a no-op write unchanged.
#[test]
fn fixture_settings_json_round_trips_byte_for_byte() {
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/rust-migration/fixtures/state/settings.json");
    let original = std::fs::read(&fixture).expect("fixture");
    let dir = settings_dir();
    std::fs::write(&dir.path, &original).unwrap();
    let writer = SettingsFileWriter::new(&dir.path);

    assert!(writer.update(|_| true).unwrap());
    assert_eq!(std::fs::read(&dir.path).unwrap(), original);

    writer.merge(&JsonObject::new(), &[]).unwrap();
    assert_eq!(std::fs::read(&dir.path).unwrap(), original);
}

/// The recommended divergence: a key matches ignoring case and keeps the file's spelling, so
/// the configuration reader never sees two spellings of one key.
#[test]
fn merge_matches_sections_and_keys_ignoring_case_and_keeps_the_existing_spelling() {
    let dir = settings_dir();
    std::fs::write(&dir.path, r#"{"Subsonic":{"Url":"a","Other":1},"Genre":{}}"#).unwrap();
    let writer = SettingsFileWriter::new(&dir.path);

    let merged = writer
        .merge(&obj(r#"{"subsonic":{"URL":"b","New":true}}"#), &[])
        .unwrap();

    assert_eq!(
        to_indented_json(&merged),
        "{\n  \"Subsonic\": {\n    \"Url\": \"b\",\n    \"Other\": 1,\n    \"New\": true\n  },\n  \"Genre\": {}\n}"
    );
}

/// A replaced key keeps its place; a new key goes last; a replaceObjects key is re-added last;
/// null and arrays replace wholesale; numbers keep their text.
#[test]
fn merge_order_and_wholesale_replacement() {
    let dir = settings_dir();
    std::fs::write(
        &dir.path,
        r#"{"ListenBrainz":{"UserTokens":{"a":"1"},"Token":"t"},"Genre":{"Blocklist":["x","y"],"MaxGenres":3}}"#,
    )
    .unwrap();
    let writer = SettingsFileWriter::new(&dir.path);

    let merged = writer
        .merge(
            &obj(
                r#"{"Genre":{"Blocklist":["z"],"MaxGenres":null,"Ratio":1.50},"listenbrainz":{"usertokens":{"b":"2"}}}"#,
            ),
            &["ListenBrainz.UserTokens"],
        )
        .unwrap();

    assert_eq!(
        Node::Object(merged).to_json_string(false),
        r#"{"ListenBrainz":{"Token":"t","usertokens":{"b":"2"}},"Genre":{"Blocklist":["z"],"MaxGenres":null,"Ratio":1.50}}"#
    );
}

#[test]
fn update_writes_only_when_the_change_says_so() {
    let dir = settings_dir();
    let writer = SettingsFileWriter::new(&dir.path);

    assert!(!writer.update(|_| false).unwrap());
    assert!(!dir.path.exists());

    assert!(
        writer
            .update(|doc| {
                doc.insert("LastFm".into(), Node::object());
                true
            })
            .unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(&dir.path).unwrap(),
        "{\n  \"LastFm\": {}\n}"
    );
    assert_eq!(writer.load().len(), 1);
}

#[test]
fn load_reads_a_corrupt_file_as_empty() {
    let dir = settings_dir();
    std::fs::write(&dir.path, "{ nope").unwrap();
    assert!(SettingsFileWriter::new(&dir.path).load().is_empty());
}

#[test]
fn write_creates_the_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("config").join("settings.json");
    SettingsFileWriter::new(&path).replace(&obj("{}")).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
}

/// A section sent in another case is written with the section's own name, and a file holding two
/// spellings of one section is written with one ("lidarr" beside "Lidarr" would make the reader
/// refuse the file on reload).
#[test]
fn sections_are_written_with_their_own_names() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let path = dir.path().join("settings.json");
    let writer = SettingsFileWriter::new(&path);

    let patch =
        match Node::parse(r#"{ "lidarr": { "BaseUrl": "http://lidarr:8686" }, "custom": { "x": 1 } }"#) {
            Ok(Node::Object(map)) => map,
            _ => panic!("an object"),
        };
    let merged = writer.merge(&patch, &[]).expect("merges");
    assert_eq!(merged.keys().collect::<Vec<_>>(), ["Lidarr", "custom"]);

    let doc = match Node::parse(
        r#"{ "Subsonic": { "Url": "a" }, "LIDARR": { "ApiKey": "k", "BaseUrl": "old" }, "Lidarr": { "BaseUrl": "new" } }"#,
    ) {
        Ok(Node::Object(map)) => map,
        _ => panic!("an object"),
    };
    writer.replace(&doc).expect("replaces");
    let written = writer.load();
    assert_eq!(written.keys().collect::<Vec<_>>(), ["Subsonic", "Lidarr"]);
    let lidarr = written["Lidarr"].as_object().expect("a section");
    assert_eq!(lidarr.get("ApiKey").and_then(Node::as_str), Some("k"));
    assert_eq!(lidarr.get("BaseUrl").and_then(Node::as_str), Some("new"));
}
