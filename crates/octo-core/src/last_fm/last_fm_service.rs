//! The Last.fm service's records ([`SimilarTrack`], [`SimilarArtist`] and [`TrackInfo`]).
//! The client is `octo::services::last_fm::last_fm_service`.

/// One song Last.fm suggested or found.
///
/// `listeners`: how many people Last.fm counts for the row, where the call says.
/// `you_tube_id`: the video a source already knows plays this song. `provider`: the
/// `services::radio::radio_provider` key of the source that suggested it. `is_video`: a YouTube
/// video found as itself (an upload a video's radio suggested), played and shown as one.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarTrack {
    pub artist: String,
    pub title: String,
    pub r#match: f64,
    pub duration: Option<i32>,
    pub listeners: Option<i64>,
    pub you_tube_id: Option<String>,
    pub provider: Option<String>,
    pub is_video: bool,
}

impl SimilarTrack {
    /// A track from its artist, title and match, with no duration and no listener count.
    pub fn new(artist: impl Into<String>, title: impl Into<String>, r#match: f64) -> Self {
        Self {
            artist: artist.into(),
            title: title.into(),
            r#match,
            duration: None,
            listeners: None,
            you_tube_id: None,
            provider: None,
            is_video: false,
        }
    }

    /// The same track with its duration set.
    pub fn with_duration(mut self, duration: Option<i32>) -> Self {
        self.duration = duration;
        self
    }

    /// The same track with its listener count set.
    pub fn with_listeners(mut self, listeners: Option<i64>) -> Self {
        self.listeners = listeners;
        self
    }

    /// The same track with its YouTube id set.
    pub fn with_you_tube_id(mut self, you_tube_id: Option<String>) -> Self {
        self.you_tube_id = you_tube_id;
        self
    }

    /// The same track, a video found as itself or not.
    pub fn with_video(mut self, is_video: bool) -> Self {
        self.is_video = is_video;
        self
    }

    /// The same track with its provider set.
    pub fn with_provider(mut self, provider: Option<String>) -> Self {
        self.provider = provider;
        self
    }
}

/// An artist `artist.getsimilar` named, with how alike Last.fm thinks they are.
#[derive(Debug, Clone, PartialEq)]
pub struct SimilarArtist {
    pub name: String,
    pub r#match: f64,
}

/// `track.getInfo`, as far as the radio reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration: Option<i32>,
    pub tags: Vec<String>,
}
