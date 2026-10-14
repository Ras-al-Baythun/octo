//! No credential leaves the admin API in clear, not the Discord webhook address, the ntfy
//! token, the Lidarr key or any other. The settings structs are walked as they serialize, so a
//! credential added later fails here until it is masked too.

use std::collections::BTreeSet;

use indexmap::IndexMap;
use octo_core::json::dom::Node;
use octo_core::settings::{AppSettings, JsonObject, LastFmUserSession};
use serde_json::Value;

use super::plumbing::SECRET_PLACEHOLDER;
use super::secrets::{
    SECRET_FIELDS, SECRET_MAPS, keep_saved_secrets, redact_secrets, restore_secret_placeholders,
    secret_sent_elsewhere,
};
use super::test_app::{app, get};
use crate::app::{AppState, TestServices};

/// What a credential is called. A webhook address carries its own token.
fn is_credential(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["password", "secret", "token", "apikey", "sessionkey", "webhook"]
        .iter()
        .any(|word| lower.contains(word))
}

/// The settings with one listener in each per-listener map, so the walk sees what an entry holds.
fn sample_settings() -> AppSettings {
    let mut settings = AppSettings::default();
    settings
        .last_fm
        .user_sessions
        .insert("alice".into(), LastFmUserSession::default());
    settings
        .listen_brainz
        .user_tokens
        .insert("alice".into(), String::new());
    settings
}

/// Every credential among the settings, as a configuration path: "Lidarr:ApiKey", or for one
/// kept per listener "ListenBrainz:UserTokens:*" and "LastFm:UserSessions:*:SessionKey".
fn credential_paths() -> Vec<String> {
    fn walk(value: &Value, prefix: &str, paths: &mut Vec<String>, depth: usize) {
        if depth > 3 {
            return;
        }
        let Value::Object(fields) = value else { return };
        for (name, field) in fields {
            let path = format!("{prefix}:{name}");
            match field {
                // A string, or a string not set (an Option that is None).
                Value::String(_) | Value::Null => {
                    if is_credential(name) {
                        paths.push(path);
                    }
                }
                // A per-listener map: its sample entry is "alice".
                Value::Object(entries) if entries.contains_key("alice") => match &entries["alice"] {
                    Value::String(_) if is_credential(name) => paths.push(format!("{path}:*")),
                    entry @ Value::Object(_) => walk(entry, &format!("{path}:*"), paths, depth + 1),
                    _ => {}
                },
                entry @ Value::Object(_) => walk(entry, &path, paths, depth + 1),
                _ => {}
            }
        }
    }
    let value = serde_json::to_value(sample_settings()).expect("settings serialise");
    let mut paths = Vec::new();
    for (section, fields) in value.as_object().expect("sections") {
        walk(fields, section, &mut paths, 0);
    }
    paths
}

fn masked_paths() -> BTreeSet<String> {
    SECRET_FIELDS
        .iter()
        .map(|(section, key, _)| format!("{section}:{key}"))
        .chain(SECRET_MAPS.iter().map(|(section, map, field)| match field {
            None => format!("{section}:{map}:*"),
            Some(field) => format!("{section}:{map}:*:{field}"),
        }))
        .collect()
}

#[test]
fn every_credential_in_the_settings_is_one_the_admin_api_masks() {
    let masked = masked_paths();
    let paths = credential_paths();
    assert!(
        paths.iter().any(|p| p == "Notifications:DiscordWebhookUrl"),
        "{paths:?}"
    );
    let unmasked: Vec<&String> = paths.iter().filter(|path| !masked.contains(*path)).collect();
    assert!(unmasked.is_empty(), "goes out in clear: {unmasked:?}");
}

/// Every section of the settings is walked, so no section with a credential can be left out of
/// the check.
#[test]
fn every_settings_type_is_walked() {
    let value = serde_json::to_value(sample_settings()).expect("settings serialise");
    let sections: Vec<&String> = value.as_object().expect("sections").keys().collect();
    assert_eq!(sections.len(), AppSettings::SECTIONS.len());
    for section in AppSettings::SECTIONS {
        assert!(sections.iter().any(|s| *s == section), "{section}");
    }
}

/// The settings with every credential set to a synthetic value, and the raw keys the same.
fn synthetic() -> (AppSettings, Vec<(String, String)>) {
    let mut value = serde_json::to_value(sample_settings()).expect("settings serialise");
    let mut raw = Vec::new();
    for (n, path) in credential_paths().iter().enumerate() {
        let text = if path.contains("DiscordWebhookUrl") {
            format!("https://discord.com/api/webhooks/1/synthetic-credential-{n}")
        } else {
            format!("synthetic-credential-{n}")
        };
        let mut node = &mut value;
        for part in path.split(':') {
            node = &mut node[if part == "*" { "alice" } else { part }];
        }
        *node = Value::String(text.clone());
        raw.push((path.replace('*', "alice"), text));
    }
    let settings: AppSettings = serde_json::from_value(value).expect("settings read back");
    (settings, raw)
}

#[tokio::test]
async fn no_credential_is_ever_returned() {
    let (settings, raw) = synthetic();
    let state = AppState::for_tests_with(
        settings,
        TestServices {
            raw_settings: raw,
            ..Default::default()
        },
    );
    let app = app(state);
    for url in [
        "/api/admin/settings",
        "/api/admin/raw-config",
        "/api/admin/config-sources",
    ] {
        let body = get(&app, url).await.text();
        assert!(!body.contains("synthetic-credential"), "{url}: {body}");
    }
}

fn obj(json: &str) -> JsonObject {
    match Node::parse(json).expect("test JSON parses") {
        Node::Object(map) => map,
        _ => panic!("an object"),
    }
}

fn compact(node: Option<&Node>) -> String {
    node.expect("present").to_json_string(false)
}

#[test]
fn a_save_that_sends_the_placeholders_back_keeps_every_credential() {
    let p = SECRET_PLACEHOLDER;
    let mut patch = obj(&format!(
        r#"{{
          "Lidarr": {{ "ApiKey": "{p}", "BaseUrl": "http://lidarr:8686" }},
          "Notifications": {{ "NtfyToken": "{p}", "DiscordWebhookUrl": "{p}" }},
          "ListenBrainz": {{ "Token": "{p}", "UserTokens": {{ "alice": "{p}", "bob": "new-token", "carol": "{p}" }} }}
        }}"#
    ));
    let stored: IndexMap<String, String> = [("alice".to_string(), "alices-token".to_string())].into();

    assert_eq!(keep_saved_secrets(&mut patch, Some(&stored)), None);
    assert_eq!(
        compact(patch.get("Lidarr")),
        r#"{"BaseUrl":"http://lidarr:8686"}"#
    );
    assert_eq!(compact(patch.get("Notifications")), "{}");
    assert_eq!(
        compact(patch.get("ListenBrainz").and_then(|lb| lb.get("UserTokens"))),
        r#"{"alice":"alices-token","bob":"new-token"}"#
    );

    let mut typed = obj(&format!(
        r#"{{ "Notifications": {{ "NtfyToken": "{p}tk_extra" }} }}"#
    ));
    assert_eq!(
        keep_saved_secrets(&mut typed, None).as_deref(),
        Some("Retype the whole ntfy token; it was added to the hidden placeholder.")
    );
}

#[test]
fn a_saved_credential_never_follows_a_new_address_it_was_not_typed_for() {
    let p = SECRET_PLACEHOLDER;
    let running: IndexMap<&str, &str> = [
        ("Lidarr:BaseUrl", "http://lidarr:8686"),
        ("Lidarr:ApiKey", "stored-key"),
        ("Notifications:NtfyUrl", "https://ntfy.sh/octo"),
        ("Notifications:NtfyToken", "tk_stored"),
    ]
    .into();
    let current = |section: &str, name: &str| {
        running
            .get(format!("{section}:{name}").as_str())
            .map(|v| v.to_string())
    };

    assert_eq!(
        secret_sent_elsewhere(
            &obj(&format!(
                r#"{{ "Lidarr": {{ "BaseUrl": "http://elsewhere:8686", "ApiKey": "{p}" }} }}"#
            )),
            current
        )
        .as_deref(),
        Some("Retype the Lidarr API key for the new address.")
    );
    assert_eq!(
        secret_sent_elsewhere(
            &obj(r#"{ "Notifications": { "NtfyUrl": "https://elsewhere.example/octo" } }"#),
            current
        )
        .as_deref(),
        Some("Retype the ntfy token for the new address.")
    );
    // The same address, a retyped key, or nothing saved yet: nothing to keep from anyone.
    assert_eq!(
        secret_sent_elsewhere(
            &obj(&format!(
                r#"{{ "Lidarr": {{ "BaseUrl": "http://lidarr:8686/", "ApiKey": "{p}" }} }}"#
            )),
            current
        ),
        None
    );
    assert_eq!(
        secret_sent_elsewhere(
            &obj(r#"{ "Lidarr": { "BaseUrl": "http://elsewhere:8686", "ApiKey": "typed-key" } }"#),
            current
        ),
        None
    );
}

#[test]
fn raw_config_puts_back_every_credential_it_hid_and_echoes_none() {
    let p = SECRET_PLACEHOLDER;
    let mut incoming = obj(&format!(
        r#"{{
          "Lidarr": {{ "ApiKey": "{p}" }},
          "Notifications": {{ "DiscordWebhookUrl": "{p}" }},
          "ListenBrainz": {{ "UserTokens": {{ "alice": "{p}" }} }}
        }}"#
    ));
    let stored = obj(r#"{
          "Lidarr": { "ApiKey": "stored-key" },
          "Notifications": { "DiscordWebhookUrl": "https://discord.com/api/webhooks/1/stored" },
          "ListenBrainz": { "UserTokens": { "alice": "stored-token" } }
        }"#);

    restore_secret_placeholders(&mut incoming, &stored);

    assert_eq!(
        Node::Object(incoming).to_json_string(false),
        Node::Object(stored.clone()).to_json_string(false)
    );
    assert!(
        !Node::Object(redact_secrets(&stored))
            .to_json_string(false)
            .contains("stored")
    );
}
