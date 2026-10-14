//! `Octo.Models.Settings.ImportSettings` (the `Imports` section).

use serde::{Deserialize, Serialize};

use crate::common::dotnet::is_blank;

/// Importing songs from somewhere else: a Spotify account, a public Spotify link, or a file
/// another service exported. Octo shows what the library already has, keeps a playlist of it,
/// and fetches the rest a few songs an hour through the same chain a heart uses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct ImportSettings {
    /// The Client ID of a Spotify app you registered at developer.spotify.com. Empty means no
    /// Spotify sign-in; links and files still work. Not a secret: the sign-in uses PKCE, which
    /// has no client secret. Environment variable: IMPORTS__SPOTIFYCLIENTID
    pub spotify_client_id: String,

    /// The redirect URI registered on that Spotify app, exactly as typed there. Spotify takes
    /// only HTTPS or a loopback address (127.0.0.1 or [::1], never "localhost"). The default is a
    /// loopback address with no port, which also lets the Octo apps sign in on any free port.
    /// When it points at Octo itself (Octo behind HTTPS), Octo finishes the sign-in on its own;
    /// otherwise the dashboard asks for the address the browser ended on.
    /// Environment variable: IMPORTS__SPOTIFYREDIRECTURI
    pub spotify_redirect_uri: String,

    /// How many missing songs the trickle starts in an hour, at most. It also waits while
    /// anyone's own downloads are running, so it never holds up a heart. 0 stops it starting any.
    /// Environment variable: IMPORTS__SONGSPERHOUR
    pub songs_per_hour: i32,

    /// How often a list that is kept as a playlist or fetched in full is read again from Spotify,
    /// so new songs there reach the playlist and the trickle. 0 reads them only when asked.
    /// Environment variable: IMPORTS__REFRESHHOURS
    pub refresh_hours: i32,

    /// Where "Keep as playlist" keeps a list. Off (the default): a playlist Octo keeps, whole
    /// from the start, with each missing song playable from outside the library until its file
    /// arrives and takes its place. On: a Navidrome playlist of the songs the library has, as
    /// before, which fills in as the rest arrive. A list already kept one way stays that way.
    /// Environment variable: IMPORTS__KEEPINNAVIDROME
    pub keep_in_navidrome: bool,
}

impl ImportSettings {
    pub const DEFAULT_REDIRECT_URI: &'static str = "http://127.0.0.1/callback";

    pub fn effective_redirect_uri(&self) -> String {
        if is_blank(&self.spotify_redirect_uri) {
            Self::DEFAULT_REDIRECT_URI.to_string()
        } else {
            self.spotify_redirect_uri.trim().to_string()
        }
    }

    pub fn effective_songs_per_hour(&self) -> i32 {
        self.songs_per_hour.clamp(0, 600)
    }
}

impl Default for ImportSettings {
    fn default() -> Self {
        Self {
            spotify_client_id: String::new(),
            spotify_redirect_uri: Self::DEFAULT_REDIRECT_URI.to_string(),
            songs_per_hour: 20,
            refresh_hours: 6,
            keep_in_navidrome: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_redirect_falls_back_and_the_pace_is_clamped() {
        let s = ImportSettings {
            spotify_redirect_uri: "   ".to_string(),
            songs_per_hour: 10_000,
            ..Default::default()
        };
        assert_eq!(s.effective_redirect_uri(), "http://127.0.0.1/callback");
        assert_eq!(s.effective_songs_per_hour(), 600);
        let typed = ImportSettings {
            spotify_redirect_uri: " https://octo.example/imports/spotify/callback ".to_string(),
            songs_per_hour: -1,
            ..Default::default()
        };
        assert_eq!(
            typed.effective_redirect_uri(),
            "https://octo.example/imports/spotify/callback"
        );
        assert_eq!(typed.effective_songs_per_hour(), 0);
        assert_eq!(ImportSettings::default().refresh_hours, 6);
    }
}
