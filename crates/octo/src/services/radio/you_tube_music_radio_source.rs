//! YouTube Music as a radio source: YouTube Music's radio for the seed. It knows uploads Last.fm
//! never heard of, nightcore and phonk channels included, which is why it suits a library built
//! from YouTube (#78).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use indexmap::IndexMap;
use octo_core::common::SongIdentity;
use octo_core::common::song_identity::SongRef;
use octo_core::last_fm::SimilarTrack;
use octo_core::last_fm::last_fm_radio_seed_normalizer as seed_normalizer;
use octo_core::outside::SongLength;
use octo_core::radio::radio_provider::YOU_TUBE_MUSIC;
use octo_core::radio::{RadioAnswer, RadioMatch, RadioSeed};
use octo_core::settings::SettingsStore;

use super::radio_source::IRadioSource;
use crate::services::framework::MemoryCache;
use crate::services::you_tube::you_tube_music_client::{YouTubeMusicClient, YtmRow};

pub const UPLOAD: &str = "MUSIC_VIDEO_TYPE_UGC";
pub const PODCAST: &str = "MUSIC_VIDEO_TYPE_PODCAST_EPISODE";

/// The seed's own video, its kind, and whether it is the same recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedVideo {
    pub video_id: String,
    pub video_type: Option<String>,
    pub exact: bool,
}

pub struct YouTubeMusicRadioSource {
    ytm: Arc<YouTubeMusicClient>,
    /// The radio source settings.
    settings: Arc<SettingsStore>,
    seeds: MemoryCache<Option<SeedVideo>>,
}

impl YouTubeMusicRadioSource {
    pub fn new(ytm: Arc<YouTubeMusicClient>, settings: Arc<SettingsStore>) -> Self {
        YouTubeMusicRadioSource {
            ytm,
            settings,
            seeds: MemoryCache::new(4000),
        }
    }

    pub async fn seed_video(&self, seed: &RadioSeed) -> anyhow::Result<Option<SeedVideo>> {
        if let Some(video) = seed.you_tube_id.as_deref().filter(|id| !id.is_empty()) {
            return Ok(Some(SeedVideo {
                video_id: video.to_string(),
                video_type: None,
                exact: true,
            }));
        }
        let key = seed_normalizer::track_key(&seed.artist, &seed.title);
        if let Some(cached) = self.seeds.get(&key) {
            return Ok(cached);
        }
        let query = format!("{} {}", seed.artist, seed.title);
        let songs = self.ytm.search(&query, "songs").await?;
        let mut found = songs.as_deref().and_then(|songs| pick(seed, songs));
        let mut videos = None;
        if found.is_none() && songs.is_some() {
            videos = self.ytm.search(&query, "videos").await?;
            found = videos.as_deref().and_then(|videos| pick(seed, videos));
        }
        // A miss is kept only when YouTube Music really answered; a shim that failed is asked
        // again next time.
        if found.is_none() && (songs.is_none() || videos.is_none()) {
            return Ok(None);
        }
        let ttl = if found.is_none() {
            Duration::from_secs(3600)
        } else {
            Duration::from_secs(7 * 24 * 3600)
        };
        self.seeds.set(key, found.clone(), 1, ttl);
        Ok(found)
    }
}

/// Whether a row is no longer than a song can be ([`SongLength::MAX_VIDEO_SECONDS`]), or of a
/// length not given.
fn song_length(row: &YtmRow) -> bool {
    row.duration_seconds.is_none() || SongLength::sane_video_length(row.duration_seconds).is_some()
}

/// The rows as tracks this source suggested. In a video's radio (`videos`), uploads are videos
/// found as themselves.
fn tracks<'a>(rows: impl Iterator<Item = &'a YtmRow>, count: i32, videos: bool) -> Vec<SimilarTrack> {
    rows.map(|row| {
        let upload = row.video_type.as_deref().is_none_or(|kind| kind == UPLOAD);
        SimilarTrack::new(row.artists.join(", "), row.title.clone(), 1.0)
            .with_duration(row.duration_seconds)
            .with_you_tube_id(Some(row.video_id.clone()))
            .with_provider(Some(YOU_TUBE_MUSIC.to_string()))
            .with_video(videos && upload)
    })
    .take(count.max(0) as usize)
    .collect()
}

/// The same recording first, the best of those by [`SongIdentity::pick_best`] (a title in another
/// script, a credit written another way and a typo at the same length count; two different
/// songs too close to tell apart pick neither); else the same song in another version, whose
/// radio is still close (the original for a nightcore seed, or the other way round).
pub fn pick(seed: &RadioSeed, rows: &[YtmRow]) -> Option<SeedVideo> {
    let seed_ref =
        SongRef::new(seed.title.clone(), seed.artist.clone()).with_seconds(seed.duration.map(f64::from));
    let row_refs: Vec<SongRef> = rows
        .iter()
        .map(|row| {
            SongRef::new(row.title.clone(), row.artists.join(", "))
                .with_seconds(row.duration_seconds.map(f64::from))
        })
        .collect();
    if let Some(best) = SongIdentity::pick_best(&seed_ref, &row_refs, &SongIdentity::pick(5)) {
        let row = &rows[best];
        return Some(SeedVideo {
            video_id: row.video_id.clone(),
            video_type: row.video_type.clone(),
            exact: true,
        });
    }
    let core = SongIdentity::parse_title(&seed.title, Some(&seed.artist)).key;
    let artist = SongIdentity::key(&SongIdentity::primary_artist(&seed.artist));
    rows.iter()
        .find(|row| {
            !core.is_empty()
                && SongIdentity::parse_title(&row.title, None).key == core
                && row
                    .artists
                    .iter()
                    .any(|name| SongIdentity::key(&SongIdentity::primary_artist(name)) == artist)
        })
        .map(|row| SeedVideo {
            video_id: row.video_id.clone(),
            video_type: row.video_type.clone(),
            exact: false,
        })
}

#[async_trait]
impl IRadioSource for YouTubeMusicRadioSource {
    fn provider(&self) -> &'static str {
        YOU_TUBE_MUSIC
    }

    fn available(&self) -> bool {
        self.settings.current().radio_sources.you_tube_music && self.ytm.configured()
    }

    async fn songs_like(
        &self,
        seed: &RadioSeed,
        count: i32,
        _auth: &IndexMap<String, String>,
    ) -> anyhow::Result<RadioAnswer> {
        if seed.is_artist() {
            // Only when YouTube Music found this very artist: a search for a name it does not
            // know answers with whoever is closest, which is the #78 mistake again.
            let (artist, rows) = self.ytm.artist_radio(&seed.artist, count.clamp(10, 100)).await?;
            let very_artist = artist.as_deref().is_some_and(|artist| {
                SongIdentity::key(artist) == SongIdentity::key(&SongIdentity::primary_artist(&seed.artist))
            });
            if !very_artist {
                return Ok(RadioAnswer::nothing(YOU_TUBE_MUSIC));
            }
            let found = tracks(
                rows.iter()
                    .filter(|row| row.video_type.as_deref() != Some(PODCAST) && !row.artists.is_empty())
                    .filter(|row| song_length(row)),
                count,
                false,
            );
            let r#match = if found.is_empty() {
                RadioMatch::None
            } else {
                RadioMatch::ArtistsTrusted
            };
            return Ok(RadioAnswer::new(YOU_TUBE_MUSIC, r#match, found, Vec::new()));
        }
        let Some(found) = self.seed_video(seed).await? else {
            return Ok(RadioAnswer::nothing(YOU_TUBE_MUSIC));
        };
        let rows = self
            .ytm
            .radio(&found.video_id, (count + 1).clamp(10, 100))
            .await?;
        // A radio from a release keeps to releases and official videos; from an upload, uploads
        // are the music (nightcore radio is mostly uploads). Podcast episodes are never music.
        let from_upload = found.video_type.as_deref().is_none_or(|kind| kind == UPLOAD);
        let suggested = tracks(
            rows.iter()
                .filter(|row| row.video_id != found.video_id && row.video_type.as_deref() != Some(PODCAST))
                .filter(|row| from_upload || row.video_type.as_deref() != Some(UPLOAD))
                .filter(|row| !row.artists.is_empty())
                // Beside songs, never an hour of ambience or a mix; a video's radio is videos.
                .filter(|row| seed.is_video || song_length(row)),
            count,
            seed.is_video,
        );
        let r#match = if found.exact {
            RadioMatch::Song
        } else {
            RadioMatch::Original
        };
        Ok(RadioAnswer::new(YOU_TUBE_MUSIC, r#match, suggested, Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    //! YouTube Music as a radio source: finding the seed's video, its radio, which rows count as
    //! music, artist radio only for the very artist, and the seed search cached.

    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    use octo_core::settings::AppSettings;
    use parking_lot::Mutex;
    use serde_json::{Value, json};
    use wiremock::matchers::any;
    use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

    use super::*;

    const ATV: &str = "MUSIC_VIDEO_TYPE_ATV";
    const UGC: &str = "MUSIC_VIDEO_TYPE_UGC";

    #[derive(Default)]
    struct ShimState {
        calls: Mutex<Vec<String>>,
        search: Mutex<HashMap<String, Vec<Value>>>,
        radio: Mutex<HashMap<String, Vec<Value>>>,
        artist: Mutex<(Option<String>, Vec<Value>)>,
        /// Searches answer 503, as the shim does with its gate full.
        busy: AtomicBool,
    }

    #[derive(Clone, Default)]
    struct Shim(Arc<ShimState>);

    impl Respond for Shim {
        fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
            let path = request.url.path().to_string();
            self.0
                .calls
                .lock()
                .push(format!("{path}?{}", request.url.query().unwrap_or("")));
            if self.0.busy.load(Ordering::SeqCst) && path == "/ytm/search" {
                return ResponseTemplate::new(503);
            }
            let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
            let q = |name: &str| query.get(name).cloned().unwrap_or_default();
            let body = match path.as_str() {
                "/ytm/search" => {
                    json!({"tracks": self.0.search.lock().get(&format!("{}|{}", q("filter"), q("q"))).cloned().unwrap_or_default()})
                }
                "/ytm/radio" => {
                    json!({"tracks": self.0.radio.lock().get(&q("videoId")).cloned().unwrap_or_default()})
                }
                "/ytm/artist-radio" => {
                    let (artist, rows) = self.0.artist.lock().clone();
                    json!({"artist": artist, "tracks": rows})
                }
                _ => json!({}),
            };
            ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json")
        }
    }

    fn row(id: &str, title: &str, artist: &str, seconds: i32, kind: &str) -> Value {
        json!({"videoId": id, "title": title, "artists": [artist], "durationSeconds": seconds, "videoType": kind})
    }

    async fn source() -> (YouTubeMusicRadioSource, Shim, MockServer) {
        let server = MockServer::builder().start().await;
        let shim = Shim::default();
        Mock::given(any()).respond_with(shim.clone()).mount(&server).await;
        let settings = Arc::new(SettingsStore::for_tests(AppSettings::default()));
        let client = Arc::new(YouTubeMusicClient::with_shim_url(Some(&server.uri())));
        (YouTubeMusicRadioSource::new(client, settings), shim, server)
    }

    fn seed(artist: &str, title: &str, seconds: Option<i32>) -> RadioSeed {
        RadioSeed::new(artist, title, seconds, None)
    }

    async fn ask(source: &YouTubeMusicRadioSource, seed: &RadioSeed) -> RadioAnswer {
        source
            .songs_like(seed, 20, &IndexMap::new())
            .await
            .expect("an answer")
    }

    fn searches(shim: &Shim) -> usize {
        shim.0
            .calls
            .lock()
            .iter()
            .filter(|call| call.starts_with("/ytm/search"))
            .count()
    }

    #[tokio::test]
    async fn a_release_gets_its_radio_without_uploads_podcasts_or_itself() {
        let (source, shim, _server) = source().await;
        shim.0.search.lock().insert(
            "songs|Massive Attack Teardrop".into(),
            vec![row("t", "Teardrop", "Massive Attack", 330, ATV)],
        );
        shim.0.radio.lock().insert(
            "t".into(),
            vec![
                row("t", "Teardrop", "Massive Attack", 330, ATV),
                row("g", "Glory Box", "Portishead", 305, ATV),
                row("u", "Teardrop (cover)", "Some Channel", 300, UGC),
                row("p", "Episode 4", "A Podcast", 2400, PODCAST),
            ],
        );
        let answer = ask(&source, &seed("Massive Attack", "Teardrop", Some(331))).await;
        assert_eq!(answer.r#match, RadioMatch::Song);
        assert_eq!(answer.tracks.len(), 1);
        let track = &answer.tracks[0];
        assert_eq!(
            (
                track.artist.as_str(),
                track.title.as_str(),
                track.you_tube_id.as_deref(),
                track.provider.as_deref()
            ),
            ("Portishead", "Glory Box", Some("g"), Some(YOU_TUBE_MUSIC))
        );
    }

    #[tokio::test]
    async fn a_videos_radio_suggests_videos_however_long_and_a_songs_never_an_hour_of_ambience() {
        let (source, shim, _server) = source().await;
        let radio = vec![
            row("bss", "Ba Sing Se Ambience", "Ambience Academy", 4706, UGC),
            row("ek", "Earth Kingdom Ambience", "SeriousChillz", 3601, UGC),
            row("song", "Jasmine Dragon", "Some Artist", 210, ATV),
        ];
        shim.0.radio.lock().insert("bss".into(), radio.clone());
        let video = RadioSeed {
            you_tube_id: Some("bss".into()),
            is_video: true,
            ..seed("Ambience Academy", "Ba Sing Se Ambience", Some(4706))
        };
        let answer = ask(&source, &video).await;
        let found: Vec<(&str, bool)> = answer
            .tracks
            .iter()
            .map(|t| (t.you_tube_id.as_deref().unwrap(), t.is_video))
            .collect();
        assert_eq!(found, [("ek", true), ("song", false)]);

        // The same radio from a song found online: the hour-long upload is left out.
        shim.0.radio.lock().insert("pinned".into(), radio);
        let song = RadioSeed {
            you_tube_id: Some("pinned".into()),
            ..seed("Some Artist", "A Song", Some(200))
        };
        let answer = ask(&source, &song).await;
        let ids: Vec<&str> = answer.tracks.iter().filter_map(|t| t.you_tube_id.as_deref()).collect();
        assert_eq!(ids, ["song"]);
        assert!(answer.tracks.iter().all(|t| !t.is_video));
    }

    #[tokio::test]
    async fn an_upload_is_found_among_videos_and_its_radio_keeps_uploads() {
        let (source, shim, _server) = source().await;
        shim.0.search.lock().insert(
            "videos|Missigno SPOOKY".into(),
            vec![row("s", "SPOOKY", "Missigno", 180, UGC)],
        );
        shim.0.radio.lock().insert(
            "s".into(),
            vec![
                row("d", "DRIFT KING", "Phonk Lord", 150, UGC),
                row("m", "Murder In My Mind", "Kordhell", 145, ATV),
            ],
        );
        let answer = ask(&source, &seed("Missigno", "SPOOKY", Some(180))).await;
        assert_eq!(answer.r#match, RadioMatch::Song);
        let titles: Vec<&str> = answer.tracks.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["DRIFT KING", "Murder In My Mind"]);
    }

    #[tokio::test]
    async fn only_another_version_on_you_tube_music_is_the_originals_radio() {
        let (source, shim, _server) = source().await;
        shim.0.search.lock().insert(
            "songs|Nightcore Galaxy Faded (Nightcore)".into(),
            vec![row("f", "Faded", "Nightcore Galaxy", 212, ATV)],
        );
        shim.0
            .radio
            .lock()
            .insert("f".into(), vec![row("w", "Wake Me Up", "Avicii", 247, ATV)]);
        let answer = ask(&source, &seed("Nightcore Galaxy", "Faded (Nightcore)", Some(160))).await;
        assert_eq!(answer.r#match, RadioMatch::Original);
        assert_eq!(answer.tracks.len(), 1);
    }

    #[tokio::test]
    async fn a_known_video_needs_no_search() {
        let (source, shim, _server) = source().await;
        shim.0
            .radio
            .lock()
            .insert("known".into(), vec![row("n", "Next", "Someone", 200, ATV)]);
        let mut known = seed("A", "B", Some(200));
        known.you_tube_id = Some("known".into());
        let answer = ask(&source, &known).await;
        assert_eq!(answer.tracks.len(), 1);
        assert_eq!(searches(&shim), 0);
    }

    #[tokio::test]
    async fn the_seed_search_is_asked_once() {
        let (source, shim, _server) = source().await;
        shim.0.search.lock().insert(
            "songs|Portishead Roads".into(),
            vec![row("r", "Roads", "Portishead", 305, ATV)],
        );
        let roads = seed("Portishead", "Roads", Some(305));
        ask(&source, &roads).await;
        ask(&source, &roads).await;
        assert_eq!(searches(&shim), 1);
    }

    #[tokio::test]
    async fn a_busy_shim_is_asked_again_next_time_not_remembered_as_nothing_found() {
        let (source, shim, _server) = source().await;
        shim.0.search.lock().insert(
            "songs|Portishead Roads".into(),
            vec![row("r", "Roads", "Portishead", 305, ATV)],
        );
        shim.0
            .radio
            .lock()
            .insert("r".into(), vec![row("g", "Glory Box", "Portishead", 305, ATV)]);
        let roads = seed("Portishead", "Roads", Some(305));

        shim.0.busy.store(true, Ordering::SeqCst);
        assert!(ask(&source, &roads).await.tracks.is_empty());
        assert_eq!(searches(&shim), 1);

        shim.0.busy.store(false, Ordering::SeqCst);
        assert_eq!(ask(&source, &roads).await.tracks.len(), 1);
    }

    #[tokio::test]
    async fn nothing_found_is_nothing() {
        let (source, _shim, _server) = source().await;
        let answer = ask(&source, &seed("Nobody", "Nothing", Some(100))).await;
        assert_eq!(answer.r#match, RadioMatch::None);
        assert!(answer.tracks.is_empty());
    }

    #[tokio::test]
    async fn artist_radio_is_only_for_the_very_artist() {
        for (found, used) in [("Massive Attack", true), ("Massive Attack Tribute Band", false)] {
            let (source, shim, _server) = source().await;
            *shim.0.artist.lock() = (Some(found.into()), vec![row("a", "Angel", found, 379, ATV)]);
            let answer = ask(&source, &seed("Massive Attack", "", None)).await;
            let expected = if used {
                RadioMatch::ArtistsTrusted
            } else {
                RadioMatch::None
            };
            assert_eq!(answer.r#match, expected, "{found}");
            assert_eq!(answer.tracks.len(), usize::from(used), "{found}");
        }
    }

    #[test]
    fn rows_tolerate_missing_fields() {
        let rows = crate::services::you_tube::you_tube_music_client::rows(&json!({"tracks": [
            {"videoId": "a", "title": "A"},
            {"title": "no id"},
            {"videoId": "b", "title": "B", "artists": ["X", null], "durationSeconds": "soon"}
        ]}))
        .expect("reads");
        let ids: Vec<&str> = rows.iter().map(|r| r.video_id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(rows[1].artists, ["X"]);
        assert_eq!(rows[1].duration_seconds, None);
    }

    fn ytm_row(id: &str, title: &str, artist: &str, seconds: i32) -> YtmRow {
        YtmRow {
            video_id: id.into(),
            title: title.into(),
            artists: vec![artist.into()],
            album: None,
            duration_seconds: Some(seconds),
            video_type: Some(ATV.into()),
            thumbnail: None,
        }
    }

    #[test]
    fn pick_takes_the_closest_copy_of_the_recording_not_the_first() {
        let rows = [
            ytm_row("far", "Teardrop", "Massive Attack", 334),
            ytm_row("near", "Teardrop", "Massive Attack", 330),
        ];
        let found = pick(&seed("Massive Attack", "Teardrop", Some(330)), &rows).expect("a pick");
        assert_eq!((found.video_id.as_str(), found.exact), ("near", true));
    }

    #[test]
    fn pick_reads_another_script_and_a_typo_at_the_same_length() {
        let cyrillic = [ytm_row("k", "Камин", "EMIN", 200)];
        let found = pick(&seed("EMIN", "Kamin", Some(201)), &cyrillic).expect("a pick");
        assert_eq!((found.video_id.as_str(), found.exact), ("k", true));

        let typo = [ytm_row("b", "Bohemian Rapsody", "Queen", 355)];
        let found = pick(&seed("Queen", "Bohemian Rhapsody", Some(354)), &typo).expect("a pick");
        assert_eq!((found.video_id.as_str(), found.exact), ("b", true));

        // A typo whose length is far off is not the recording, nor the song in another version.
        let far = [ytm_row("b", "Bohemian Rapsody", "Queen", 380)];
        assert_eq!(pick(&seed("Queen", "Bohemian Rhapsody", Some(354)), &far), None);
    }

    #[test]
    fn pick_takes_neither_of_two_different_songs_too_close_to_tell_apart() {
        let rows = [
            ytm_row("y", "Bohemian Rhapsody", "Queen", 354),
            ytm_row("z", "Bohemian Rhapsodz", "Queen", 354),
        ];
        assert_eq!(pick(&seed("Queen", "Bohemian Rhapsodx", Some(354)), &rows), None);
    }
}
