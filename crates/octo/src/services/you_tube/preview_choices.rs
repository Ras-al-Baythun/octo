//! octoPreview: the videos a song from outside the library could play from, and the one the
//! listener picks. The pick is the video every later play streams and a YouTube download
//! fetches, since both read the song's pinned video first.

use octo_core::outside::{LengthSource, RoutingKind, SongLength};
use serde::Serialize;

use super::you_tube_music_client::PODCAST_EPISODE;
use super::{YouTubeResolver, YtmRow};
use crate::services::outside::ExternalIdRegistry;

/// Where a choice was found.
pub const YOUTUBE_MUSIC: &str = "YouTube Music";
pub const YOUTUBE: &str = "YouTube";

/// How many of YouTube's results are offered beside YouTube Music's rows.
pub const CANDIDATES: usize = 6;

/// One video a song could play from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewChoice {
    pub video_id: String,
    pub title: String,
    /// The channel it is on, or the artists YouTube Music names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length_seconds: Option<i32>,
    pub source: &'static str,
    /// The video the song plays from now.
    pub current: bool,
}

/// The videos the song under `id` could play from: YouTube Music's rows for it first (a
/// release's own audio, mostly), then YouTube's results as the shim ranks them, each video
/// once, the one that plays now marked and put first when neither list has it. None when
/// `id` is no song with an artist and a title; empty when neither source could answer.
pub async fn preview_choices(
    youtube: &YouTubeResolver,
    registry: &ExternalIdRegistry,
    id: &str,
) -> Option<Vec<PreviewChoice>> {
    let r = registry.lookup(id)?.snapshot();
    if r.kind != RoutingKind::Song || !r.has_artist_title() {
        return None;
    }
    let artist = r.artist.clone().unwrap_or_default();
    let title = r.title.clone().unwrap_or_default();
    let query = format!("{artist} {title}");
    let (music, plain) = tokio::join!(
        youtube.music_candidates(&artist, &title),
        youtube.candidates(&query, CANDIDATES, r.duration)
    );
    let mut choices: Vec<PreviewChoice> = Vec::new();
    for row in music
        .iter()
        .filter(|row| row.video_type.as_deref() != Some(PODCAST_EPISODE))
    {
        offer(&mut choices, music_choice(row));
    }
    for hit in plain {
        offer(
            &mut choices,
            PreviewChoice {
                video_id: hit.video_id,
                title: hit.title.unwrap_or_default(),
                channel: hit.channel,
                length_seconds: hit.duration,
                source: YOUTUBE,
                current: false,
            },
        );
    }
    if let Some(video) = r.you_tube_id.as_deref().filter(|v| !v.is_empty()) {
        match choices.iter_mut().find(|choice| choice.video_id == video) {
            Some(choice) => choice.current = true,
            None => choices.insert(
                0,
                PreviewChoice {
                    video_id: video.to_string(),
                    title: title.clone(),
                    channel: Some(artist.clone()),
                    length_seconds: r.duration,
                    source: YOUTUBE,
                    current: true,
                },
            ),
        }
    }
    Some(choices)
}

fn music_choice(row: &YtmRow) -> PreviewChoice {
    PreviewChoice {
        video_id: row.video_id.clone(),
        title: row.title.clone(),
        channel: Some(row.artists.join(", ")).filter(|artists| !artists.is_empty()),
        length_seconds: row.duration_seconds,
        source: YOUTUBE_MUSIC,
        current: false,
    }
}

/// Adds a choice unless its video is listed already.
fn offer(choices: &mut Vec<PreviewChoice>, choice: PreviewChoice) {
    if choice.video_id.is_empty() || choices.iter().any(|known| known.video_id == choice.video_id) {
        return;
    }
    choices.push(choice);
}

/// Makes the song under `id` play from `video`: pinned for every later play and download, and
/// when `length` is a video's length, the song's own length too, so getSong and the next
/// play's hint carry it, and its shown length as far as a video's word goes. False when `id`
/// is no song the registry knows.
pub fn set_preview(registry: &ExternalIdRegistry, id: &str, video: &str, length: Option<i32>) -> bool {
    if !registry.pin_video(id, video, true) {
        return false;
    }
    if let Some(seconds) = SongLength::sane_video_length(length) {
        if let Some(routing) = registry.lookup(id) {
            routing.lock().duration = Some(seconds);
        }
        registry.remember_length(id, Some(seconds), LengthSource::Video);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(video: &str, source: &'static str) -> PreviewChoice {
        PreviewChoice {
            video_id: video.into(),
            title: "Song".into(),
            channel: None,
            length_seconds: None,
            source,
            current: false,
        }
    }

    #[test]
    fn a_video_is_offered_once_and_never_without_an_id() {
        let mut choices = Vec::new();
        offer(&mut choices, choice("a", YOUTUBE_MUSIC));
        offer(&mut choices, choice("a", YOUTUBE));
        offer(&mut choices, choice("", YOUTUBE));
        offer(&mut choices, choice("b", YOUTUBE));
        assert_eq!(
            choices
                .iter()
                .map(|c| (c.video_id.as_str(), c.source))
                .collect::<Vec<_>>(),
            [("a", YOUTUBE_MUSIC), ("b", YOUTUBE)]
        );
    }

    #[test]
    fn the_pick_is_pinned_with_its_length_when_it_is_a_videos() {
        use octo_core::outside::OutsideRouting;
        let registry = ExternalIdRegistry::in_memory();
        let id = registry.register(OutsideRouting {
            kind: RoutingKind::Song,
            artist: Some("Drake".into()),
            title: Some("Passionfruit".into()),
            duration: Some(299),
            ..Default::default()
        });
        assert!(set_preview(&registry, &id, "vid1", Some(300)));
        let r = registry.lookup(&id).expect("there").snapshot();
        assert_eq!(r.you_tube_id.as_deref(), Some("vid1"));
        assert_eq!(r.duration, Some(300));
        // A length no video has (a compilation) leaves the song's own length alone.
        assert!(set_preview(&registry, &id, "vid2", Some(5000)));
        let r = registry.lookup(&id).expect("there").snapshot();
        assert_eq!(r.you_tube_id.as_deref(), Some("vid2"));
        assert_eq!(r.duration, Some(300));
        assert!(!set_preview(&registry, "nobody", "vid1", None));
    }
}
