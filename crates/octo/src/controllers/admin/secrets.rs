//! The admin API's secret rules: no password, key, token or webhook address leaves the admin API
//! in clear. Every one reads as [`SECRET_PLACEHOLDER`]; a save that sends the placeholder back
//! keeps what is saved; one that types onto the end of it is refused; and a saved credential
//! never follows a server address it was not typed again for.

use indexmap::IndexMap;
use octo_core::common::dotnet::eq_ignore_case;
use octo_core::json::dom::Node;
use octo_core::settings::JsonObject;

use super::plumbing::{SECRET_PLACEHOLDER, child, child_mut, key_of, mask_secret, n_obj};
use super::settings::string_value;
use crate::app::AppState;

/// Every credential among the settings, by section and key, with its name in words for "Retype
/// the whole ...". `every_credential_in_the_settings_is_one_the_admin_api_masks` walks the
/// settings types, so a new one cannot go out in clear without failing it.
pub const SECRET_FIELDS: &[(&str, &str, &str)] = &[
    ("Subsonic", "AdminPassword", "admin password"),
    ("Downloads", "AcoustIdApiKey", "AcoustID application key"),
    ("Downloads", "AcoustIdUserApiKey", "AcoustID user key"),
    ("Lidarr", "ApiKey", "Lidarr API key"),
    ("Lidarr", "WebhookSecret", "Lidarr webhook password"),
    ("LastFm", "ApiKey", "Last.fm API key"),
    ("LastFm", "ApiSecret", "Last.fm shared secret"),
    ("Notifications", "NtfyToken", "ntfy token"),
    ("Notifications", "DiscordWebhookUrl", "Discord webhook address"),
    ("ListenBrainz", "Token", "ListenBrainz token"),
];

/// Credentials that are sent to an address the settings also hold: section, address, key, and
/// the credential in words.
pub const SECRET_ADDRESSES: &[(&str, &str, &str, &str)] = &[
    ("Subsonic", "Url", "AdminPassword", "admin password"),
    ("Lidarr", "BaseUrl", "ApiKey", "Lidarr API key"),
    ("Notifications", "NtfyUrl", "NtfyToken", "ntfy token"),
];

/// Credentials kept one per listener: section, map, and the field inside each entry, or None
/// when the entry itself is the credential.
pub const SECRET_MAPS: &[(&str, &str, Option<&str>)] = &[
    ("LastFm", "UserSessions", Some("SessionKey")),
    ("ListenBrainz", "UserTokens", None),
];

/// Navidrome's address is saved trimmed and without trailing slashes, so the file never holds
/// an `http://nd:4533/` that a relay would turn into `//rest/stream` (the settings read it the
/// same way: `octo_core::settings::normalize_server_url`).
pub fn normalize_saved_addresses(patch: &mut JsonObject) {
    if let Some(subsonic) = child_mut(patch, "Subsonic")
        && let Some(key) = key_of(subsonic, "Url")
        && let Some(url) = string_value(subsonic.get(&key))
    {
        let normalized = octo_core::settings::normalize_server_url(url);
        subsonic.insert(key, Node::String(normalized));
    }
}

/// The text of a JSON string that starts with the placeholder.
fn placeholder_text(node: Option<&Node>) -> Option<&str> {
    string_value(node).filter(|text| text.starts_with(SECRET_PLACEHOLDER))
}

/// The words to refuse a save with when it moves a server's address while keeping the saved
/// credential that goes to it (the placeholder sent back, or the field left out), or None.
/// `current` reads a running value by section and key.
///
/// The address is compared trimmed and without trailing slashes, ignoring case.
pub fn secret_sent_elsewhere(
    patch: &JsonObject,
    current: impl Fn(&str, &str) -> Option<String>,
) -> Option<String> {
    fn address(value: Option<&str>) -> String {
        value.unwrap_or("").trim().trim_end_matches('/').to_string()
    }
    for (section, address_key, name, words) in SECRET_ADDRESSES {
        let Some(part) = child(patch, section) else {
            continue;
        };
        let Some(new_address) = key_of(part, address_key).and_then(|key| string_value(part.get(&key))) else {
            continue;
        };
        if eq_ignore_case(
            &address(Some(new_address)),
            &address(current(section, address_key).as_deref()),
        ) {
            continue;
        }
        if current(section, name).is_none_or(|stored| stored.is_empty()) {
            continue;
        }
        let typed = key_of(part, name)
            .and_then(|key| string_value(part.get(&key)))
            .is_some_and(|text| !text.starts_with(SECRET_PLACEHOLDER));
        if !typed {
            return Some(format!("Retype the {words} for the new address."));
        }
    }
    None
}

/// A settings patch with every echoed placeholder taken out, so what is saved stays: a
/// credential, or a listener's ListenBrainz token (kept from `stored_tokens`, since that map is
/// saved whole). The words to refuse it with when someone typed onto the end of a placeholder,
/// or None.
pub fn keep_saved_secrets(
    patch: &mut JsonObject,
    stored_tokens: Option<&IndexMap<String, String>>,
) -> Option<String> {
    for (section, name, words) in SECRET_FIELDS {
        let Some(part) = child_mut(patch, section) else {
            continue;
        };
        let Some(key) = key_of(part, name) else {
            continue;
        };
        let Some(text) = placeholder_text(part.get(&key)) else {
            continue;
        };
        if text != SECRET_PLACEHOLDER {
            return Some(format!(
                "Retype the whole {words}; it was added to the hidden placeholder."
            ));
        }
        part.shift_remove(&key);
    }
    if let Some(tokens) = child_mut(patch, "ListenBrainz").and_then(|lb| child_mut(lb, "UserTokens")) {
        let users: Vec<String> = tokens.keys().cloned().collect();
        for user in users {
            let Some(text) = placeholder_text(tokens.get(&user)) else {
                continue;
            };
            if text != SECRET_PLACEHOLDER {
                return Some(format!(
                    "Retype {user}'s whole ListenBrainz token; it was added to the hidden placeholder."
                ));
            }
            let stored = stored_tokens
                .and_then(|stored| stored.iter().find(|(name, _)| eq_ignore_case(name, &user)))
                .map(|(_, token)| token.clone())
                .filter(|token| !token.is_empty());
            match stored {
                Some(token) => {
                    tokens.insert(user, Node::String(token));
                }
                None => {
                    tokens.shift_remove(&user);
                }
            }
        }
    }
    None
}

/// The words for the first credential in a Raw config document that still starts with the
/// placeholder once the placeholders are restored: someone typed onto its end.
pub fn secret_typed_into_placeholder(incoming: &JsonObject) -> Option<String> {
    for (section, name, words) in SECRET_FIELDS {
        if let Some(part) = child(incoming, section)
            && let Some(key) = key_of(part, name)
            && placeholder_text(part.get(&key)).is_some()
        {
            return Some((*words).to_string());
        }
    }
    let tokens = child(incoming, "ListenBrainz").and_then(|lb| child(lb, "UserTokens"))?;
    tokens
        .iter()
        .find(|(_, node)| placeholder_text(Some(node)).is_some())
        .map(|(user, _)| format!("ListenBrainz token of {user}"))
}

/// Each listener's ListenBrainz token as the placeholder, names kept; a blank token is left out.
pub fn mask_tokens(tokens: &IndexMap<String, String>) -> Node {
    Node::Object(
        tokens
            .iter()
            .filter(|(_, token)| !token.is_empty())
            .map(|(user, _)| (user.clone(), Node::String(SECRET_PLACEHOLDER.to_string())))
            .collect(),
    )
}

/// The credentials Octo runs with, shaped like the settings file: what Raw config keeps when the
/// file itself cannot be read.
pub fn running_secrets(state: &AppState) -> JsonObject {
    let current = state.settings.current();
    let text = |value: &str| Node::String(value.to_string());
    let optional = |value: &Option<String>| {
        value
            .as_deref()
            .map_or(Node::Null, |v| Node::String(v.to_string()))
    };
    let sessions = Node::Object(
        current
            .last_fm
            .user_sessions
            .iter()
            .map(|(user, session)| {
                (
                    user.clone(),
                    n_obj([
                        ("SessionKey", text(&session.session_key)),
                        ("LastFmUser", text(&session.last_fm_user)),
                    ]),
                )
            })
            .collect(),
    );
    let tokens = Node::Object(
        current
            .listen_brainz
            .user_tokens
            .iter()
            .map(|(user, token)| (user.clone(), text(token)))
            .collect(),
    );
    let mut secrets = JsonObject::new();
    secrets.insert(
        "Subsonic".into(),
        n_obj([("AdminPassword", optional(&current.subsonic.admin_password))]),
    );
    secrets.insert(
        "Downloads".into(),
        n_obj([
            ("AcoustIdApiKey", text(&current.downloads.acoust_id_api_key)),
            (
                "AcoustIdUserApiKey",
                text(&current.downloads.acoust_id_user_api_key),
            ),
        ]),
    );
    secrets.insert(
        "Lidarr".into(),
        n_obj([
            ("ApiKey", optional(&current.lidarr.api_key)),
            ("WebhookSecret", optional(&current.lidarr.webhook_secret)),
        ]),
    );
    secrets.insert(
        "LastFm".into(),
        n_obj([
            ("ApiKey", text(&current.last_fm.api_key)),
            ("ApiSecret", text(&current.last_fm.api_secret)),
            ("UserSessions", sessions),
        ]),
    );
    secrets.insert(
        "Notifications".into(),
        n_obj([
            ("NtfyToken", text(&current.notifications.ntfy_token)),
            (
                "DiscordWebhookUrl",
                text(&current.notifications.discord_webhook_url),
            ),
        ]),
    );
    secrets.insert(
        "ListenBrainz".into(),
        n_obj([
            ("Token", text(&current.listen_brainz.token)),
            ("UserTokens", tokens),
        ]),
    );
    secrets
}

/// Swaps one placeholder back for the stored value, or drops the key when nothing is stored.
/// A real value is left alone.
fn restore_placeholder(incoming: Option<&mut JsonObject>, existing: Option<&JsonObject>, name: &str) {
    let Some(incoming) = incoming else { return };
    let Some(key) = key_of(incoming, name) else { return };
    if string_value(incoming.get(&key)) != Some(SECRET_PLACEHOLDER) {
        return;
    }
    let stored = existing
        .and_then(|existing| key_of(existing, name).and_then(|k| existing.get(&k)))
        .and_then(|node| string_value(Some(node)))
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    match stored {
        Some(text) => {
            incoming.insert(key, Node::String(text));
        }
        None => {
            incoming.shift_remove(&key);
        }
    }
}

/// Undo the placeholder in a Raw config document before it replaces the file: keep the stored
/// credential when the file has one, otherwise drop the key so an environment value keeps
/// applying. A real value is left alone.
pub fn restore_secret_placeholders(incoming: &mut JsonObject, existing_file: &JsonObject) {
    for (section, name, _) in SECRET_FIELDS {
        restore_placeholder(child_mut(incoming, section), child(existing_file, section), name);
    }

    // Each listener's ListenBrainz token, by name; one only the environment has is dropped.
    if let Some(tokens) = child_mut(incoming, "ListenBrainz").and_then(|lb| child_mut(lb, "UserTokens")) {
        let stored_tokens = child(existing_file, "ListenBrainz").and_then(|lb| child(lb, "UserTokens"));
        let users: Vec<String> = tokens.keys().cloned().collect();
        for user in users {
            // RestorePlaceholder over the map itself, with the user's name as the key.
            restore_placeholder(Some(&mut *tokens), stored_tokens, &user);
            if key_of(tokens, &user).is_none() {
                tokens.shift_remove(&user);
            }
        }
    }

    // Each listener's session the same way, matched by username. An entry whose key is only in
    // the environment is dropped whole, so the environment keeps applying.
    let Some(sessions) = child_mut(incoming, "LastFm").and_then(|lastfm| child_mut(lastfm, "UserSessions"))
    else {
        return;
    };
    let stored_sessions = child(existing_file, "LastFm").and_then(|lastfm| child(lastfm, "UserSessions"));
    let users: Vec<String> = sessions.keys().cloned().collect();
    for user in users {
        let Some(Node::Object(session)) = sessions.get_mut(&user) else {
            continue;
        };
        let stored = stored_sessions
            .and_then(|stored| key_of(stored, &user).and_then(|k| stored.get(&k)))
            .and_then(Node::as_object);
        restore_placeholder(Some(session), stored, "SessionKey");
        if key_of(session, "SessionKey").is_none() {
            sessions.shift_remove(&user);
        }
    }
}

/// The listener whose Last.fm session key, after the placeholders were restored, still starts
/// with the placeholder: someone typed onto the end of it. Saved, it would be a key Last.fm
/// refuses, and the listener would be disconnected for it. None when there is none.
pub fn session_key_typed_into_placeholder(incoming: &JsonObject) -> Option<String> {
    let sessions = child(incoming, "LastFm").and_then(|lastfm| child(lastfm, "UserSessions"))?;
    sessions.iter().find_map(|(user, node)| {
        let session = node.as_object()?;
        let key = key_of(session, "SessionKey")?;
        placeholder_text(session.get(&key)).map(|_| user.clone())
    })
}

fn mask_in(section: Option<&mut JsonObject>, name: &str) {
    let Some(section) = section else { return };
    let keys: Vec<String> = section
        .keys()
        .filter(|key| eq_ignore_case(key, name))
        .cloned()
        .collect();
    for key in keys {
        if let Some(Node::String(text)) = section.get(&key) {
            let masked = mask_secret(Some(text));
            section.insert(key, Node::String(masked));
        }
    }
}

/// The merged file echoed after a save, with every credential masked the same way the GET masks
/// it.
pub fn redact_secrets(merged: &JsonObject) -> JsonObject {
    let mut copy = merged.clone();
    for (section, name, _) in SECRET_FIELDS {
        mask_in(child_mut(&mut copy, section), name);
    }
    for (section, map, field) in SECRET_MAPS {
        let Some(entries) = child_mut(&mut copy, section).and_then(|part| child_mut(part, map)) else {
            continue;
        };
        let keys: Vec<String> = entries.keys().cloned().collect();
        for key in keys {
            match field {
                None => mask_in(Some(&mut *entries), &key),
                Some(field) => mask_in(entries.get_mut(&key).and_then(Node::as_object_mut), field),
            }
        }
    }
    copy
}
