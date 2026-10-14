//! The cover a chart already named for its songs. Every chart row comes with its own artwork
//! (Apple's, or Deezer's for the backup chart), but an outside song's cover is otherwise found by
//! searching the cover sources for its artist and title, which can settle on another release's
//! picture. A chart remembers its rows' artwork here, and [`HintedArtwork`], the first of the
//! aggregator's sources, serves it.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use octo_core::common::SongIdentity;
use octo_core::outside::routing::{OutsideRouting, RoutingKind};
use parking_lot::Mutex;
use tracing::debug;

use super::i_cover_art_source::ICoverArtSource;
use crate::services::framework::http::client_builder;

/// The most covers remembered: a few weeks of six lists, with room to spare.
pub const MAX_HINTS: usize = 5_000;

#[derive(Default)]
struct Hints {
    urls: HashMap<String, String>,
    order: VecDeque<String>,
}

/// Cover links by song or album, oldest forgotten first past [`MAX_HINTS`].
#[derive(Default)]
pub struct ArtworkHints {
    hints: Mutex<Hints>,
}

impl ArtworkHints {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remembers the cover for a song (`title` set) or an album (`album` set) by its artist.
    pub fn remember(&self, kind: RoutingKind, artist: &str, name: &str, url: &str) {
        let key = key(kind, artist, name);
        if key.is_empty() || url.is_empty() {
            return;
        }
        let mut hints = self.hints.lock();
        if hints.urls.insert(key.clone(), url.to_string()).is_none() {
            hints.order.push_back(key);
        }
        while hints.order.len() > MAX_HINTS {
            if let Some(oldest) = hints.order.pop_front() {
                hints.urls.remove(&oldest);
            }
        }
    }

    /// The cover remembered for a routing, if any.
    pub fn for_routing(&self, routing: &OutsideRouting) -> Option<String> {
        let name = match routing.kind {
            RoutingKind::Song => routing.title.as_deref()?,
            RoutingKind::Album => routing.album.as_deref()?,
            RoutingKind::Artist => return None,
        };
        let key = key(routing.kind, routing.artist.as_deref()?, name);
        self.hints.lock().urls.get(&key).cloned()
    }

    pub fn len(&self) -> usize {
        self.hints.lock().urls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn key(kind: RoutingKind, artist: &str, name: &str) -> String {
    let (artist, name) = (SongIdentity::key(artist), SongIdentity::key(name));
    if artist.is_empty() || name.is_empty() {
        return String::new();
    }
    format!("{}|{artist}|{name}", kind as i32)
}

/// The cover a chart named, fetched from the link it gave.
pub struct HintedArtwork {
    hints: Arc<ArtworkHints>,
    client: reqwest::Client,
}

impl HintedArtwork {
    pub fn new(hints: Arc<ArtworkHints>) -> Self {
        let client = client_builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("the artwork client builds");
        Self { hints, client }
    }
}

#[async_trait]
impl ICoverArtSource for HintedArtwork {
    fn name(&self) -> &str {
        "chart"
    }

    async fn try_fetch(&self, routing: &OutsideRouting, _background: bool) -> Option<Bytes> {
        let url = self.hints.for_routing(routing)?;
        let response = match self.client.get(&url).send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                debug!("chart artwork answered {}", response.status());
                return None;
            }
            Err(e) => {
                debug!("chart artwork failed: {e}");
                return None;
            }
        };
        response.bytes().await.ok().filter(|b| !b.is_empty())
    }
}

#[cfg(test)]
#[path = "artwork_hints_tests.rs"]
mod tests;
