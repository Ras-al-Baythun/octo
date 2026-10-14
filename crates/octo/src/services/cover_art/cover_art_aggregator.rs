//! The cover art chain, with its cache.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use octo_core::common::dotnet;
use octo_core::outside::routing::{OutsideRouting, RoutingKind};
use tracing::{debug, info};

use super::i_cover_art_source::ICoverArtSource;
use crate::services::framework::MemoryCache;

// Bounded in BYTES, and on its own instance rather than shared with the metadata
// caches: these entries are 1000x1000 JPEGs at 150-400KB each, so an entry count
// that suits small metadata records would be a meaningless bound here.
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

const HIT_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// A miss can be caused by a source being throttled, so it must not be
/// remembered for long enough to blank a cover for the life of the process.
const MISS_TTL: Duration = Duration::from_secs(5 * 60);

/// Cover art chain: queries each registered [`ICoverArtSource`] in
/// order and returns the first hit. Caches the result per (kind, artist,
/// album|title) so a queue scroll doesn't trigger N external API calls per
/// visible song.
///
/// Order matters: put broad-catalog sources (Deezer) first so we don't pay
/// the iTunes round-trip for international tracks where iTunes whiffs anyway.
/// Last.fm last because its track image often points to the same iTunes
/// asset we'd have gotten one source earlier.
pub struct CoverArtAggregator {
    sources: Vec<Arc<dyn ICoverArtSource>>,
    /// `None` is a cached miss.
    cache: MemoryCache<Option<Bytes>>,
}

impl CoverArtAggregator {
    pub fn new(sources: Vec<Arc<dyn ICoverArtSource>>) -> Self {
        let names: Vec<&str> = sources.iter().map(|s| s.name()).collect();
        info!(
            "CoverArtAggregator: {} sources in order: {}",
            sources.len(),
            names.join(", ")
        );
        Self {
            sources,
            cache: MemoryCache::new(MAX_CACHE_BYTES),
        }
    }

    pub async fn get_cover(&self, routing: &OutsideRouting, background: bool) -> Option<Bytes> {
        let cache_key = make_cache_key(routing);
        if let Some(cached) = self.cache.get(&cache_key) {
            return cached;
        }

        for source in &self.sources {
            if let Some(bytes) = source
                .try_fetch(routing, background)
                .await
                .filter(|b| !b.is_empty())
            {
                debug!(
                    "cover {} hit for {cache_key} ({} bytes)",
                    source.name(),
                    bytes.len()
                );
                self.cache
                    .set(cache_key, Some(bytes.clone()), bytes.len() as u64, HIT_TTL);
                return Some(bytes);
            }
        }

        debug!("cover all-miss for {cache_key}");
        self.cache.set(cache_key, None, 1, MISS_TTL);
        None
    }

    /// Drop every cached cover. Exposed so a run of throttled lookups can be
    /// cleared without restarting the container.
    pub fn clear_cache(&self) {
        self.cache.clear();
        info!("cover art cache cleared");
    }
}

fn make_cache_key(r: &OutsideRouting) -> String {
    let artist = dotnet::to_lower_invariant(r.artist.as_deref().unwrap_or("").trim());
    let album_or_title = if r.kind == RoutingKind::Album {
        r.album.as_deref().or(r.title.as_deref())
    } else {
        r.title.as_deref().or(r.album.as_deref())
    };
    let album_or_title = dotnet::to_lower_invariant(album_or_title.unwrap_or("").trim());
    let kind = match r.kind {
        RoutingKind::Song => "Song",
        RoutingKind::Album => "Album",
        RoutingKind::Artist => "Artist",
    };
    format!("{kind}|{artist}|{album_or_title}")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::*;

    struct Fake {
        name: &'static str,
        answer: Option<&'static [u8]>,
        asked: AtomicUsize,
    }

    #[async_trait]
    impl ICoverArtSource for Fake {
        fn name(&self) -> &str {
            self.name
        }

        async fn try_fetch(&self, _routing: &OutsideRouting, _background: bool) -> Option<Bytes> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer.map(Bytes::from_static)
        }
    }

    fn fake(name: &'static str, answer: Option<&'static [u8]>) -> Arc<Fake> {
        Arc::new(Fake {
            name,
            answer,
            asked: AtomicUsize::new(0),
        })
    }

    /// The chain stops at the first source with bytes (an empty answer is a miss), and both a hit
    /// and an all-miss are remembered.
    #[tokio::test]
    async fn the_first_hit_wins_and_is_remembered() {
        let empty = fake("deezer", Some(b""));
        let hit = fake("itunes", Some(b"jpeg"));
        let never = fake("lastfm", Some(b"other"));
        let aggregator = CoverArtAggregator::new(vec![empty.clone(), hit.clone(), never.clone()]);
        let routing = OutsideRouting {
            kind: RoutingKind::Album,
            artist: Some(" Air ".into()),
            album: Some("Moon Safari".into()),
            ..Default::default()
        };

        assert_eq!(
            aggregator.get_cover(&routing, false).await.as_deref(),
            Some(&b"jpeg"[..])
        );
        assert_eq!(
            aggregator.get_cover(&routing, false).await.as_deref(),
            Some(&b"jpeg"[..])
        );
        assert_eq!(hit.asked.load(Ordering::SeqCst), 1);
        assert_eq!(never.asked.load(Ordering::SeqCst), 0);
        assert_eq!(make_cache_key(&routing), "Album|air|moon safari");

        let miss = fake("deezer", None);
        let aggregator = CoverArtAggregator::new(vec![miss.clone()]);
        assert!(aggregator.get_cover(&routing, false).await.is_none());
        assert!(aggregator.get_cover(&routing, false).await.is_none());
        assert_eq!(miss.asked.load(Ordering::SeqCst), 1);
        aggregator.clear_cache();
        aggregator.get_cover(&routing, false).await;
        assert_eq!(miss.asked.load(Ordering::SeqCst), 2);
    }
}
