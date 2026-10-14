//! The library actions answers that need only settings. The answer for a `LibraryActionOutcome`
//! and getUpgrades take app types and are in the `octo` crate.

use octo_core::settings::library_action::{LibraryAction, LibraryActionSettings};
use serde_json::json;

use super::{SUBSONIC_VERSION, SubsonicReply, SubsonicResponseBuilder};

/// The OpenSubsonic extension a client checks for before it offers a library action.
pub const LIBRARY_ACTIONS_EXTENSION: &str = "octoLibraryActions";

/// Version 2 adds the upgrade action and getUpgrades; version 3 the edits below,
/// getLibraryTrash and the admin flag. Every older version is still listed.
pub const LIBRARY_ACTIONS_EXTENSION_VERSION: i32 = 3;

/// The Delete action, named for what a person sees: the song leaves the library, and the file
/// waits in quarantine.
pub const REMOVE_ACTION: &str = "remove";

/// The Better quality action, from version 2: look for a higher quality copy and swap it in, the
/// song keeping its place. Queued, never run inside the request.
pub const UPGRADE_ACTION: &str = "upgrade";

/// The Wrong lyrics action: show the next source's lyrics for the song and never these again.
pub const LYRICS_ACTION: &str = "lyrics";

/// The Wrong cover action: put another cover on the song's album and never use this one again.
pub const COVER_SWAP_ACTION: &str = "cover-swap";

/// Version 3: write tags into a song's file, in place.
pub const RETAG_ACTION: &str = "retag";

/// Version 3: put a song on the album another song is on, by copying that album's tags.
pub const JOIN_ALBUM_ACTION: &str = "joinAlbum";

/// Version 3: find a cover for a song's album and put it inside a file that has none.
pub const COVER_ACTION: &str = "cover";

/// Version 3: the tags a download of the song would get. Writes nothing.
pub const LOOKUP_ACTION: &str = "lookup";

/// Version 3: put back the last retag, joinAlbum or cover of a song.
pub const UNDO_ACTION: &str = "undo";

/// Version 3: put a removed song back from the trash.
pub const RESTORE_ACTION: &str = "restore";

/// The Wrong version action for a video the person picked (octoPreview 2's Choose version on
/// a library song): `libraryAction?action=replace&videoId=[&length=]` downloads exactly that
/// video in the file's place, the song keeping its tags, favorites and playlist places.
/// Queued, never run inside the request.
pub const REPLACE_ACTION: &str = "replace";

pub const EDIT_ACTIONS: [&str; 6] = [
    RETAG_ACTION,
    JOIN_ALBUM_ACTION,
    COVER_ACTION,
    LOOKUP_ACTION,
    UNDO_ACTION,
    RESTORE_ACTION,
];

impl SubsonicResponseBuilder {
    /// getLibraryActions: what this server lets the caller do. Always JSON. The field names are a
    /// contract with the Octo app.
    #[allow(clippy::too_many_arguments)]
    pub fn create_library_actions_response(
        &self,
        settings: &LibraryActionSettings,
        username: Option<&str>,
        parallel: i32,
        upgrade_ready: bool,
        upgrade_source: &str,
        admin: bool,
        edits: bool,
        covers: bool,
    ) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "openSubsonic": true,
            "libraryActions": {
                "enabled": settings.enabled,
                "allowed": settings.is_allowed(username),
                "dryRun": settings.dry_run,
                "actions": offered_actions(settings, upgrade_ready, edits, covers, admin),
                // Whether the caller is a Navidrome admin. Removing and every edit need it.
                "admin": admin,
                // 0 means kept until someone removes it by hand.
                "keepDays": settings.effective_quarantine_retention_days(),
                // How many upgrades run at once, which is how many downloads may.
                "parallel": parallel,
                // Where an upgrade looks, for a client to say so rather than assume. Null when upgrade is not offered.
                "upgradeSource": upgrade_ready.then_some(upgrade_source),
            },
        }))
    }

    /// libraryAction with a state of its own, such as "queued" for an upgrade. Always ok, so
    /// the client reads the state rather than an error, even when nothing was done.
    pub fn create_library_action_response(
        &self,
        song_id: &str,
        state: &str,
        detail: Option<&str>,
        action: &str,
    ) -> SubsonicReply {
        self.create_json_response(json!({
            "status": "ok",
            "version": SUBSONIC_VERSION,
            "type": "octo",
            "openSubsonic": true,
            "libraryAction": {
                "id": song_id,
                "action": action,
                "state": state,
                "detail": detail,
            },
        }))
    }
}

/// The actions offered, by the names the app knows them by.
fn offered_actions(
    settings: &LibraryActionSettings,
    upgrade_ready: bool,
    edits: bool,
    covers: bool,
    admin: bool,
) -> Vec<&'static str> {
    let enabled: Vec<LibraryAction> = settings
        .effective_actions()
        .into_iter()
        .filter(|action| action.enabled)
        .map(|action| action.action)
        .collect();
    let mut offered = Vec::new();
    // Removing needs a Navidrome admin, which libraryAction checks again; offered only to one.
    if enabled.contains(&LibraryAction::Delete) && admin {
        offered.push(REMOVE_ACTION);
    }
    if enabled.contains(&LibraryAction::BetterQuality) && upgrade_ready {
        offered.push(UPGRADE_ACTION);
    }
    // A replacement by a picked video: the pick is what the song then plays from, so it can
    // be offered to anyone the allowlist admits, admin or not.
    if enabled.contains(&LibraryAction::WrongVersion) {
        offered.push(REPLACE_ACTION);
    }
    if enabled.contains(&LibraryAction::WrongLyrics) {
        offered.push(LYRICS_ACTION);
    }
    if enabled.contains(&LibraryAction::WrongCover) {
        offered.push(COVER_SWAP_ACTION);
    }
    if edits {
        offered.extend([
            RETAG_ACTION,
            JOIN_ALBUM_ACTION,
            LOOKUP_ACTION,
            UNDO_ACTION,
            RESTORE_ACTION,
        ]);
        if covers {
            offered.push(COVER_ACTION);
        }
    }
    offered
}
