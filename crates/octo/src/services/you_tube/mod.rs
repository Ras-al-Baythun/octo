//! `Services/YouTube`: the yt-dlp shim, for search and streams, and YouTube Music through its
//! `/ytm` endpoints.

pub mod preview_choices;
pub mod stream_size_warmer;
pub mod you_tube_music_client;
pub mod you_tube_resolver;

pub use stream_size_warmer::StreamSizeWarmer;
pub use you_tube_music_client::{YouTubeMusicClient, YtmRow};
pub use you_tube_resolver::{ShimBusy, ShimStream, YouTubeHit, YouTubeResolver};
