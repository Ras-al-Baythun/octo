//! A source of cover art, one link of the cover art chain.

use async_trait::async_trait;
use bytes::Bytes;
use octo_core::outside::routing::OutsideRouting;

/// One backend that can supply cover art bytes for a routing (song / album /
/// artist). Multiple sources stack behind `CoverArtAggregator`;
/// the aggregator queries them in order and returns the first hit.
///
/// Implementations should be self-contained (own HTTP client, own caching if
/// relevant) and never fail out of [`ICoverArtSource::try_fetch`]: return None
/// for any kind of miss or error so the chain can move on.
#[async_trait]
pub trait ICoverArtSource: Send + Sync {
    /// Short tag for log lines, e.g. "deezer", "itunes", "lastfm".
    fn name(&self) -> &str;

    /// Try to fetch cover art bytes for the routing. Return None on any miss
    /// or transport error. Bytes returned should be a decodable image (caller
    /// will composite a watermark, so don't pre-encode).
    ///
    /// `background` is true only for fire-and-forget prewarm. Sources with a shared
    /// rate-limited lane (Deezer) route this onto the background lane so prewarm traffic
    /// cannot make an interactive search or cover fetch queue behind it.
    async fn try_fetch(&self, routing: &OutsideRouting, background: bool) -> Option<Bytes>;
}
