//! One provisioner per request, over that request's proxy, so its once-per-boot set is once per
//! request in practice. A caller builds it with `new(request's proxy, settings)`.

use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexMap;
use octo_core::common::dotnet;
use octo_core::settings::SettingsStore;
use parking_lot::Mutex;
use tracing::{info, warn};

use octo_core::family::abilities::AddToLibrary;

use crate::services::family::FamilyStore;
use crate::services::subsonic::SubsonicProxyService;

/// Makes sure an allowed user has a playlist for each enabled action.
///
/// Created PER USER with THEIR credentials, so the playlists are theirs and private. An
/// admin-owned public playlist would be visible and writable to every account on the server,
/// which would hand the delete button to people who are not on the allowlist. A private
/// playlist per user is exactly the right blast radius.
pub struct LibraryActionPlaylistProvisioner {
    /// Usernames already provisioned by this instance (keys compared ignoring case), so this runs
    /// once per user rather than on every playlist listing.
    done: Mutex<HashSet<String>>,
    proxy: SubsonicProxyService,
    /// The library action settings, read at every call.
    settings: Arc<SettingsStore>,
    /// The family's members, for their own action playlists.
    family: Option<Arc<FamilyStore>>,
}

impl LibraryActionPlaylistProvisioner {
    pub fn new(proxy: SubsonicProxyService, settings: Arc<SettingsStore>) -> Self {
        LibraryActionPlaylistProvisioner {
            done: Mutex::new(HashSet::new()),
            proxy,
            settings,
            family: None,
        }
    }

    /// Create whatever is missing.
    ///
    /// `existing_names` comes from the playlist listing the caller just relayed, so this costs
    /// no extra request in the common case where everything already exists.
    pub async fn ensure(
        &self,
        username: &str,
        existing_names: &[String],
        auth_parameters: &IndexMap<String, String>,
    ) {
        let current = self.settings.current();
        let settings = &current.library_actions;
        let family_titles = self.family_titles(username);
        let actions_on = settings.enabled
            && (settings.playlists_enabled || settings.notices_enabled())
            && settings.is_allowed(Some(username));
        if !actions_on && family_titles.is_empty() {
            return;
        }
        if !self.done.lock().insert(dotnet::ordinal_ignore_case_key(username)) {
            return;
        }

        // The notice playlists Octo fills are the user's own too, for the same reason: they are
        // private, and the admin identity fills them.
        let actions: Vec<String> = if actions_on && settings.playlists_enabled {
            settings
                .effective_actions()
                .iter()
                .filter(|action| action.enabled)
                .map(|action| settings.playlist_title(action))
                .collect()
        } else {
            Vec::new()
        };
        let notices: Vec<String> = if actions_on {
            settings
                .enabled_notice_kinds()
                .into_iter()
                .map(|kind| settings.notice_title(kind))
                .collect()
        } else {
            Vec::new()
        };
        let mut wanted: Vec<String> = Vec::new();
        for title in actions.into_iter().chain(notices).chain(family_titles) {
            let exists = existing_names
                .iter()
                .any(|name| dotnet::eq_ignore_case(name, &title));
            let repeated = wanted.iter().any(|name| dotnet::eq_ignore_case(name, &title));
            if !exists && !repeated {
                wanted.push(title);
            }
        }
        if wanted.is_empty() {
            return;
        }

        for title in wanted {
            // The user's own credentials, so the playlist belongs to them. Drop anything that
            // would make this look like an edit of an existing playlist. Keys compare ignoring
            // case.
            let mut parameters: IndexMap<String, String> = auth_parameters
                .iter()
                .filter(|(key, _)| {
                    !["id", "playlistId", "songId"]
                        .iter()
                        .any(|dropped| dotnet::eq_ignore_case(key, dropped))
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            match parameters
                .keys()
                .find(|key| dotnet::eq_ignore_case(key, "name"))
                .cloned()
            {
                Some(key) => {
                    parameters.insert(key, title.clone());
                }
                None => {
                    parameters.insert("name".into(), title.clone());
                }
            }

            // Never fails into the playlist listing: a failure here costs a playlist, not the
            // user's ability to see their own.
            if self
                .proxy
                .relay_safe("rest/createPlaylist", parameters.iter())
                .await
                .is_some()
            {
                info!("Created action playlist '{title}' for {username}");
            } else {
                warn!("Could not create action playlist '{title}' for {username}");
            }
        }
    }

    /// Forget the once-per-boot marker, so a settings change re-provisions.
    pub fn reset(&self) {
        self.done.lock().clear();
    }

    /// The same provisioner, also making family members' own action playlists.
    pub fn with_family(mut self, family: Arc<FamilyStore>) -> Self {
        self.family = Some(family);
        self
    }

    /// A family member's own action playlists: "Request a copy" (and "Request a copy in FLAC"
    /// when they may ask for FLAC) for members who ask before adding, and "Remove from my
    /// library" for every member. None while Family is off or for anyone else.
    fn family_titles(&self, username: &str) -> Vec<String> {
        let Some(family) = &self.family else {
            return Vec::new();
        };
        if !self.settings.current().family.enabled {
            return Vec::new();
        }
        let Some(member) = family.member(username) else {
            return Vec::new();
        };
        let abilities = member.abilities();
        let mut titles = Vec::new();
        if abilities.add_to_library == AddToLibrary::Request {
            titles.push(REQUEST_PLAYLIST.to_string());
        }
        titles.push(REMOVE_PLAYLIST.to_string());
        titles
    }
}

/// A family member's playlist: songs added to it are asked for (best quality).
pub const REQUEST_PLAYLIST: &str = "Request a copy";

/// A family member's playlist: songs added to it leave their own library.
pub const REMOVE_PLAYLIST: &str = "Remove from my library";

#[cfg(test)]
mod tests {
    use octo_core::settings::{AppSettings, LibraryActionSettings, SubsonicSettings};
    use wiremock::matchers::{any, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    /// The missing titles are created once, with the user's own sign-in and never an id that
    /// would make it an edit.
    #[tokio::test]
    async fn the_missing_playlists_are_created_once_as_the_user() {
        let navidrome = MockServer::builder().start().await;
        Mock::given(path("/rest/createPlaylist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"subsonic-response":{"status":"ok"}}"#, "application/json"),
            )
            .mount(&navidrome)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(404))
            .mount(&navidrome)
            .await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings {
            library_actions: LibraryActionSettings {
                enabled: true,
                review_enabled: true,
                allowed_users: vec!["alice".into()],
                ..Default::default()
            },
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                ..Default::default()
            },
            ..Default::default()
        }));
        let provisioner =
            LibraryActionPlaylistProvisioner::new(SubsonicProxyService::new(settings.clone()), settings);
        let auth: IndexMap<String, String> = [("u", "alice"), ("t", "tok"), ("s", "salt"), ("ID", "p9")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        // Keep comes on with Review; the Review playlist itself exists already.
        provisioner.ensure("bob", &[], &auth).await;
        provisioner
            .ensure("Alice", &["▸ review".to_string()], &auth)
            .await;
        provisioner.ensure("alice", &[], &auth).await;

        let created: Vec<(String, String, bool)> = navidrome
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .map(|r| {
                let query: Vec<(String, String)> = r.url.query_pairs().into_owned().collect();
                let get = |k: &str| query.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
                (
                    get("name").unwrap_or_default(),
                    get("u").unwrap_or_default(),
                    query.iter().any(|(k, _)| k.eq_ignore_ascii_case("id")),
                )
            })
            .collect();
        assert_eq!(created, [("🛠 Keep".to_string(), "alice".to_string(), false)]);
    }
}

#[cfg(test)]
mod family_tests {
    use octo_core::family::abilities::Preset;
    use octo_core::settings::{AppSettings, SubsonicSettings};
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::services::family::test_support::member;

    async fn created_for(user: &str, family_on: bool) -> Vec<String> {
        let navidrome = MockServer::start().await;
        Mock::given(path("/rest/createPlaylist"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"subsonic-response":{"status":"ok"}}"#, "application/json"),
            )
            .mount(&navidrome)
            .await;
        let mut app = AppSettings {
            subsonic: SubsonicSettings {
                url: Some(navidrome.uri()),
                ..Default::default()
            },
            ..Default::default()
        };
        app.family.enabled = family_on;
        let settings = Arc::new(SettingsStore::for_tests(app));
        let family = Arc::new(FamilyStore::in_memory());
        family.upsert_member(member("alex", Preset::Member, 3));
        family.upsert_member(member("sam", Preset::Listener, 4));
        family.upsert_member(member("kim", Preset::Kid, 5));
        let provisioner =
            LibraryActionPlaylistProvisioner::new(SubsonicProxyService::new(settings.clone()), settings)
                .with_family(family);
        let auth: IndexMap<String, String> = [("u", user), ("t", "tok"), ("s", "salt")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        provisioner.ensure(user, &[], &auth).await;
        navidrome
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .filter_map(|r| {
                r.url
                    .query_pairs()
                    .find(|(k, _)| k == "name")
                    .map(|(_, v)| v.into_owned())
            })
            .collect()
    }

    #[tokio::test]
    async fn members_get_their_own_request_and_remove_playlists() {
        assert_eq!(
            created_for("sam", true).await,
            [REQUEST_PLAYLIST, REMOVE_PLAYLIST]
        );
        // A kid may ask for MP3 copies only.
        assert_eq!(
            created_for("kim", true).await,
            [REQUEST_PLAYLIST, REMOVE_PLAYLIST]
        );
        // A member who adds songs at once has nothing to ask for.
        assert_eq!(created_for("alex", true).await, [REMOVE_PLAYLIST]);
        assert!(created_for("winters", true).await.is_empty());
        assert!(created_for("sam", false).await.is_empty(), "Family off");
    }
}
