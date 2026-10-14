//! The walk for recordings the library holds more than once (#53), handed to the Duplicates
//! playlists.

use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use octo_core::common::dotnet::{self, eq_ignore_case};
use octo_core::json::datetime;
use octo_core::library::generated_playlist_service::dotnet_ticks;
use octo_core::settings::{LibraryActionSettings, SettingsStore};
use octo_media::audio::{SpectrumAnalyzer, SpectrumReport, SpectrumVerdict};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::NavidromeSongPathResolver;
use super::notice_queue::NoticeQueue;
use super::quality_upgrade_worker::compare_ordinal;
use crate::services::framework::http::{HttpAnswer, parse_json};
use crate::services::health::IHealthFields;
use crate::services::health::library_health;
use crate::services::subsonic::NavidromeIdentityService;

/// One library row, as much of it as telling copies apart needs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LibraryTrack {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub recording_id: String,
    pub suffix: String,
    pub bit_rate: i32,
    pub duration: i32,

    /// For a lossless file whose spectrum says it was made from a lossy one, what it was likely
    /// made from ("about 128 kbps MP3"). Only ever set on a copy inside a duplicate group.
    pub transcoded_from: Option<String>,

    /// The ISRCs its tags carry, which name the recording as a MusicBrainz id does.
    pub isrcs: Vec<String>,
}

impl LibraryTrack {
    /// A track from its row fields, with no spectrum note and no ISRCs.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        title: &str,
        artist: &str,
        album: &str,
        recording_id: &str,
        suffix: &str,
        bit_rate: i32,
        duration: i32,
    ) -> Self {
        LibraryTrack {
            id: id.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            album: album.to_string(),
            recording_id: recording_id.to_string(),
            suffix: suffix.to_string(),
            bit_rate,
            duration,
            transcoded_from: None,
            isrcs: Vec::new(),
        }
    }
}

/// What the shared duplicate finder reads of a track: who, what, how long, and its codes.
pub struct LibraryTrackFields;

impl IHealthFields<LibraryTrack> for LibraryTrackFields {
    fn id<'a>(&'a self, song: &'a LibraryTrack) -> &'a str {
        &song.id
    }

    fn title<'a>(&'a self, song: &'a LibraryTrack) -> &'a str {
        &song.title
    }

    fn artist<'a>(&'a self, song: &'a LibraryTrack) -> Option<&'a str> {
        (!song.artist.is_empty()).then_some(song.artist.as_str())
    }

    fn album<'a>(&'a self, song: &'a LibraryTrack) -> Option<&'a str> {
        Some(&song.album)
    }

    fn album_id<'a>(&'a self, _song: &'a LibraryTrack) -> Option<&'a str> {
        None
    }

    fn album_artist<'a>(&'a self, _song: &'a LibraryTrack) -> Option<&'a str> {
        None
    }

    fn genres<'a>(&'a self, _song: &'a LibraryTrack) -> Vec<&'a str> {
        Vec::new()
    }

    fn year(&self, _song: &LibraryTrack) -> Option<i32> {
        None
    }

    fn disc(&self, _song: &LibraryTrack) -> Option<i32> {
        None
    }

    fn track(&self, _song: &LibraryTrack) -> Option<i32> {
        None
    }

    fn seconds(&self, song: &LibraryTrack) -> i32 {
        song.duration
    }

    fn format(&self, song: &LibraryTrack) -> Option<String> {
        Some(song.suffix.clone())
    }

    fn lossless(&self, song: &LibraryTrack) -> bool {
        is_lossless(song)
    }

    fn bit_rate(&self, song: &LibraryTrack) -> Option<i32> {
        Some(song.bit_rate)
    }

    fn bit_depth(&self, _song: &LibraryTrack) -> Option<i32> {
        None
    }

    fn sample_rate(&self, _song: &LibraryTrack) -> Option<i32> {
        None
    }

    fn isrcs<'a>(&'a self, song: &'a LibraryTrack) -> Vec<&'a str> {
        song.isrcs.iter().map(String::as_str).collect()
    }

    fn recording_id<'a>(&'a self, song: &'a LibraryTrack) -> Option<&'a str> {
        Some(&song.recording_id)
    }

    fn cover<'a>(&'a self, _song: &'a LibraryTrack) -> Option<&'a str> {
        None
    }
}

/// Copies of one recording, the one worth keeping first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub key: String,
    pub tracks: Vec<LibraryTrack>,
}

/// What the last walk found, for the dashboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateScanResult {
    pub at_utc: DateTime<Utc>,
    pub tracks: usize,
    pub groups: usize,
    pub added: usize,
    pub complete: bool,
}

const LOSSLESS_SUFFIXES: [&str; 8] = ["flac", "alac", "wav", "aiff", "aif", "ape", "wv", "dsf"];

/// ALAC arrives as m4a, told apart from AAC only by its bitrate.
pub fn is_lossless_file(suffix: &str, bit_rate: i32) -> bool {
    LOSSLESS_SUFFIXES.iter().any(|s| eq_ignore_case(s, suffix))
        || (eq_ignore_case(suffix, "m4a") && bit_rate > 500)
}

pub fn is_lossless(track: &LibraryTrack) -> bool {
    is_lossless_file(&track.suffix, track.bit_rate)
}

/// Walks the library for recordings it holds more than once (#53) and hands them to the
/// Duplicates playlists. It only points them out: nothing here touches a file.
///
/// A duplicate is what Library health calls one, in the Octo app and on the dashboard alike
/// ([`library_health::find_duplicates`]): files whose tags name one recording (an ISRC or a
/// MusicBrainz recording id, lengths within 10 seconds), or files by the same artist whose titles
/// read the same, version and all (a live take, a remix or a radio edit is never a copy), with
/// lengths within 3 seconds. Until 2026-10-05 only files sharing a recording id were grouped
/// here, which found almost nothing in a library where few files carry one, and disagreed with
/// what Library health showed for the same library.
pub struct DuplicateScanWorker {
    queue: Arc<NoticeQueue>,
    identity: NavidromeIdentityService,
    http: reqwest::Client,
    /// The Subsonic, library action and download settings, read at use.
    settings: Arc<SettingsStore>,
    resolver: Option<Arc<NavidromeSongPathResolver>>,
    spectrum: Option<Arc<SpectrumAnalyzer>>,
    /// One stored request, a second absorbed.
    requested: Notify,
    /// Whether that stored request is there, which `Notify` cannot say.
    request_waiting: AtomicBool,
    /// When the scan walking the library now began; None between scans.
    scan_started_utc: Mutex<Option<DateTime<Utc>>>,
    last_scan_utc: Mutex<DateTime<Utc>>,
    last_result: Mutex<Option<DuplicateScanResult>>,

    /// Spectrum verdicts by track id, size and modification time, so a file is decoded once and
    /// not again on every scan. A replaced or re-tagged file has a new key and is looked at again.
    spectra: Mutex<HashMap<String, SpectrumReport>>,
    /// With Family on, the duplicate scan keeps to the shared library (Keep removes files, and
    /// a member's own copy is never the shared song's duplicate).
    scope: Option<crate::services::family::ScopeResolver>,
}

impl DuplicateScanWorker {
    pub const PAGE_SIZE: usize = 500;

    /// 100,000 tracks. A library larger than that is scanned in part, which can add questions
    /// but never settle one.
    pub const MAX_PAGES: usize = 200;

    const FIRST_CHECK: Duration = Duration::from_secs(2 * 60);
    const CHECK_INTERVAL: Duration = Duration::from_secs(60);

    /// How many files one scan may decode. A second or so each, and only copies inside a group
    /// with two or more lossless files are ever looked at; the rest wait for the next scan.
    pub const MAX_SPECTRUM_CHECKS_PER_SCAN: usize = 200;

    pub fn new(
        queue: Arc<NoticeQueue>,
        identity: NavidromeIdentityService,
        http: reqwest::Client,
        settings: Arc<SettingsStore>,
        resolver: Option<Arc<NavidromeSongPathResolver>>,
        spectrum: Option<Arc<SpectrumAnalyzer>>,
    ) -> Self {
        DuplicateScanWorker {
            queue,
            identity,
            http,
            settings,
            resolver,
            spectrum,
            requested: Notify::new(),
            request_waiting: AtomicBool::new(false),
            scan_started_utc: Mutex::new(None),
            last_scan_utc: Mutex::new(datetime::min_value()),
            last_result: Mutex::new(None),
            spectra: Mutex::new(HashMap::new()),
            scope: None,
        }
    }

    /// Keeps the scan to the shared library while Family is on.
    pub fn with_library_scope(mut self, scope: crate::services::family::ScopeResolver) -> Self {
        self.scope = Some(scope);
        self
    }

    /// The tracks the scan may pair: all of them, or the shared library's while members have
    /// libraries of their own (Family on, or turned off with their libraries still there).
    /// Until the library index has been read no track can be told apart, so none is paired
    /// and the walk counts as unfinished: nothing is asked or settled from it.
    fn shared_only(&self, tracks: Vec<LibraryTrack>, complete: bool) -> (Vec<LibraryTrack>, bool) {
        match &self.scope {
            Some(scope) if scope.keeps_shared() => {
                if !scope.ready() {
                    return (Vec::new(), false);
                }
                let view = scope.shared_view();
                let shared = tracks
                    .into_iter()
                    .filter(|track| {
                        view.allows(
                            &crate::services::family::LibraryScope::Shared,
                            Some(&track.id),
                            None,
                        )
                    })
                    .collect();
                (shared, complete)
            }
            _ => (tracks, complete),
        }
    }

    pub fn last_result(&self) -> Option<DuplicateScanResult> {
        self.last_result.lock().clone()
    }

    /// A scan walking the library right now.
    pub fn is_scanning(&self) -> bool {
        self.scan_started_utc.lock().is_some()
    }

    /// When the scan walking the library now began.
    pub fn scan_started_utc(&self) -> Option<DateTime<Utc>> {
        *self.scan_started_utc.lock()
    }

    /// "Scan now" pressed and not yet picked up.
    pub fn is_requested(&self) -> bool {
        self.request_waiting.load(Ordering::SeqCst)
    }

    /// The dashboard's "Scan now". A scan already waiting to run absorbs a second request.
    pub fn request_scan(&self) {
        self.request_waiting.store(true, Ordering::SeqCst);
        self.requested.notify_one();
    }

    /// The worker loop, until the host stops.
    pub async fn run(self: Arc<Self>, stopping: CancellationToken) -> anyhow::Result<()> {
        let mut wait = Self::FIRST_CHECK;
        while !stopping.is_cancelled() {
            let requested = tokio::select! {
                answer = tokio::time::timeout(wait, self.requested.notified()) => answer.is_ok(),
                _ = stopping.cancelled() => break,
            };
            if requested {
                self.request_waiting.store(false, Ordering::SeqCst);
            }
            wait = Self::CHECK_INTERVAL;

            // Read afresh every tick, so switching Duplicates on needs no restart and the first
            // scan comes a minute later rather than a day later.
            let settings = self.settings.current().library_actions.clone();
            if !settings.enabled || !settings.duplicates_enabled || !self.identity.has_admin_identity() {
                continue;
            }
            let interval = chrono::TimeDelta::from_std(settings.effective_duplicates_scan_interval())
                .unwrap_or(chrono::TimeDelta::MAX);
            if !requested && Utc::now() - *self.last_scan_utc.lock() < interval {
                continue;
            }

            // Per-scan catch is mandatory: one failed scan must never stop the worker.
            *self.scan_started_utc.lock() = Some(Utc::now());
            let scan = tokio::select! {
                scan = self.scan(&settings) => scan,
                _ = stopping.cancelled() => {
                    *self.scan_started_utc.lock() = None;
                    break;
                }
            };
            if let Err(e) = scan {
                error!("Duplicate scan failed: {e}");
            }
            *self.last_scan_utc.lock() = Utc::now();
            *self.scan_started_utc.lock() = None;
        }
        Ok(())
    }

    async fn scan(&self, settings: &LibraryActionSettings) -> anyhow::Result<()> {
        let (tracks, complete) = self.walk().await?;
        let (tracks, complete) = self.shared_only(tracks, complete);
        let mut groups = Self::find_groups(&tracks);
        let detect = self.settings.current().downloads.detect_transcodes;
        if self.resolver.is_some() && self.spectrum.is_some() && detect {
            let decoded = AtomicUsize::new(0);
            groups = Self::check_transcodes(groups, |track| {
                let track = track.clone();
                let decoded = &decoded;
                async move {
                    if decoded.load(Ordering::SeqCst) >= Self::MAX_SPECTRUM_CHECKS_PER_SCAN {
                        None
                    } else {
                        self.spectrum_of(&track, decoded).await
                    }
                }
            })
            .await;
        }
        let users: Vec<&str> = settings
            .allowed_users
            .iter()
            .map(String::as_str)
            .filter(|user| !dotnet::is_blank(user))
            .collect();
        let added = self.queue.sync_duplicates(&groups, &users, complete);
        self.queue.flush();

        *self.last_result.lock() = Some(DuplicateScanResult {
            at_utc: Utc::now(),
            tracks: tracks.len(),
            groups: groups.len(),
            added,
            complete,
        });
        info!(
            "Duplicate scan: {} tracks, {} groups, {added} new question(s){}",
            tracks.len(),
            groups.len(),
            if complete {
                ""
            } else {
                " (the walk did not finish, so nothing was settled)"
            }
        );
        Ok(())
    }

    /// Every track, by paging search3 with the empty query, the same walk Symfonium makes to copy
    /// a library. Complete only when a short page ends it. `Err` when the request failed, or the
    /// answer was not JSON of the expected shape.
    pub async fn walk(&self) -> anyhow::Result<(Vec<LibraryTrack>, bool)> {
        let base_url = self
            .settings
            .current()
            .subsonic
            .url
            .clone()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        // The scan credential comes from the admin login, so make sure there has been one.
        self.identity.ensure_admin_jwt().await;
        let auth = self.identity.get_scan_auth();
        let (Some((user, token, salt)), false) = (auth, base_url.is_empty()) else {
            return Ok((Vec::new(), false));
        };

        let mut tracks = Vec::new();
        for page in 0..Self::MAX_PAGES {
            let url = format!(
                "{base_url}/rest/search3?f=json&c=octo&v=1.16.1&query=%22%22&songCount={}&songOffset={}&albumCount=0&artistCount=0&u={}&t={}&s={}",
                Self::PAGE_SIZE,
                page * Self::PAGE_SIZE,
                dotnet::escape_data_string(&user),
                dotnet::escape_data_string(&token),
                dotnet::escape_data_string(&salt)
            );
            let answer = HttpAnswer::read(self.http.get(&url).send().await?).await?;
            if !answer.is_success() {
                return Ok((tracks, false));
            }
            let root = parse_json(&answer.body)?;
            let Some(rows) = Self::parse_page(&root, &mut tracks).map_err(anyhow::Error::msg)? else {
                return Ok((tracks, false));
            };
            if rows < Self::PAGE_SIZE {
                return Ok((tracks, true));
            }
        }

        warn!(
            "The duplicate scan stopped at {} tracks. Pairs past that point are not found, and nothing is settled from a partial walk.",
            Self::MAX_PAGES * Self::PAGE_SIZE
        );
        Ok((tracks, false))
    }

    /// Adds the page's tracks, and returns how many rows the page held, or None when it is not a
    /// successful answer at all (a refusal must not read as the end). `Err` for a member read on
    /// something that is not an object, or a status that is not a string.
    pub fn parse_page(root: &Value, into: &mut Vec<LibraryTrack>) -> Result<Option<usize>, String> {
        let Some(envelope) = property(root, "subsonic-response")? else {
            return Ok(None);
        };
        let ok = match property(envelope, "status")? {
            None => false,
            Some(Value::String(status)) => status == "ok",
            Some(Value::Null) => false,
            Some(_) => {
                return Err("The requested operation requires an element of type 'String'.".to_string());
            }
        };
        if !ok {
            return Ok(None);
        }
        let Some(result) = property(envelope, "searchResult3")? else {
            return Ok(Some(0));
        };
        let Some(Value::Array(songs)) = property(result, "song")? else {
            return Ok(Some(0));
        };

        let mut rows = 0;
        for song in songs {
            rows += 1;
            let Some(id) = text(song, "id")?.filter(|s| !s.is_empty()) else {
                continue;
            };
            // An outside song a server lists beside the library's is not a file to keep or remove.
            if property(song, "isExternal")? == Some(&Value::Bool(true)) {
                continue;
            }
            into.push(LibraryTrack {
                id,
                title: text(song, "title")?.unwrap_or_default(),
                artist: text(song, "artist")?.unwrap_or_default(),
                album: text(song, "album")?.unwrap_or_default(),
                recording_id: text(song, "musicBrainzId")?.unwrap_or_default(),
                suffix: text(song, "suffix")?.unwrap_or_default(),
                bit_rate: int(song, "bitRate")?,
                duration: int(song, "duration")?,
                transcoded_from: None,
                isrcs: isrcs_of(song)?,
            });
        }
        Ok(Some(rows))
    }

    /// Copies of one recording, by Library health's own rule, so the Duplicates playlist asks
    /// about exactly the sets Library health shows. Each set is ranked here for keeping, which can
    /// also weigh a fake lossless copy ([`Self::check_transcodes`]).
    pub fn find_groups(tracks: &[LibraryTrack]) -> Vec<DuplicateGroup> {
        library_health::find_duplicates(tracks, &LibraryTrackFields)
            .into_iter()
            .map(|set| {
                let mut ids: Vec<&str> = set.copies.iter().map(|track| track.id.as_str()).collect();
                ids.sort_by(|a, b| compare_ordinal(a, b));
                DuplicateGroup {
                    key: format!("dup|{}", ids.join(",")),
                    tracks: Self::rank_for_keeping(&set.copies),
                }
            })
            .collect()
    }

    /// The groups again, with every lossless copy in a group that has two or more of them
    /// checked for being a transcode, and each group ranked again. Only those copies: a lone
    /// lossless file outranks the lossy ones either way, and the rest of the library is never
    /// decoded. A copy the check could not judge counts as genuine.
    pub async fn check_transcodes<F, Fut>(groups: Vec<DuplicateGroup>, mut check: F) -> Vec<DuplicateGroup>
    where
        F: FnMut(&LibraryTrack) -> Fut,
        Fut: Future<Output = Option<SpectrumReport>>,
    {
        let mut result = Vec::with_capacity(groups.len());
        for group in groups {
            if group.tracks.iter().filter(|track| is_lossless(track)).count() < 2 {
                result.push(group);
                continue;
            }
            let mut tracks = Vec::with_capacity(group.tracks.len());
            for track in &group.tracks {
                let report = if is_lossless(track) {
                    check(track).await
                } else {
                    None
                };
                match report {
                    Some(report) if report.is_likely_lossy() => tracks.push(LibraryTrack {
                        transcoded_from: report.estimate.clone(),
                        ..track.clone()
                    }),
                    _ => tracks.push(track.clone()),
                }
            }
            result.push(DuplicateGroup {
                key: group.key,
                tracks: Self::rank_for_keeping(&tracks),
            });
        }
        result
    }

    async fn spectrum_of(&self, track: &LibraryTrack, decoded: &AtomicUsize) -> Option<SpectrumReport> {
        let (resolver, spectrum) = (self.resolver.as_ref()?, self.spectrum.as_ref()?);
        let file = resolver.resolve(&track.id).await?;
        let key = format!(
            "{}|{}|{}",
            track.id,
            file.size_bytes,
            last_write_ticks(&file.absolute_path)
        );
        if let Some(known) = self.spectra.lock().get(&key) {
            return Some(known.clone());
        }

        decoded.fetch_add(1, Ordering::SeqCst);
        let timeout = self
            .settings
            .current()
            .downloads
            .effective_transcode_check_timeout_seconds();
        let report = spectrum.analyze(Path::new(&file.absolute_path), timeout).await;
        // Unknown from a timeout or a missing ffmpeg is not remembered, so it is asked again.
        if report.verdict != SpectrumVerdict::Unknown {
            self.spectra.lock().insert(key, report.clone());
        }
        if report.is_likely_lossy() {
            info!("Duplicate scan: {} is {}", file.absolute_path, report.describe());
        }
        Some(report)
    }

    /// Genuine lossless first, then a lossless file made from a lossy one, then the higher
    /// bitrate, then the length closest to the group's median (a copy much longer or shorter
    /// than the rest is the likelier to be cut or padded), then id.
    pub fn rank_for_keeping(group: &[LibraryTrack]) -> Vec<LibraryTrack> {
        let mut lengths: Vec<i32> = group.iter().map(|track| track.duration).collect();
        lengths.sort_unstable();
        let median = lengths[lengths.len() / 2];
        let mut ranked = group.to_vec();
        ranked.sort_by(|a, b| {
            let genuine = |t: &LibraryTrack| is_lossless(t) && t.transcoded_from.is_none();
            genuine(b)
                .cmp(&genuine(a))
                .then_with(|| is_lossless(b).cmp(&is_lossless(a)))
                .then_with(|| b.bit_rate.cmp(&a.bit_rate))
                .then_with(|| {
                    (i64::from(a.duration) - i64::from(median))
                        .abs()
                        .cmp(&(i64::from(b.duration) - i64::from(median)).abs())
                })
                .then_with(|| compare_ordinal(&a.id, &b.id))
        });
        ranked
    }
}

/// A member of an object; anything but an object is an error.
fn property<'a>(element: &'a Value, name: &str) -> Result<Option<&'a Value>, String> {
    match element {
        Value::Object(map) => Ok(map.get(name)),
        _ => Err("The requested operation requires an element of type 'Object'.".to_string()),
    }
}

fn text(element: &Value, name: &str) -> Result<Option<String>, String> {
    Ok(match property(element, name)? {
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    })
}

/// OpenSubsonic's isrc list; one written as a single string is a list of one.
fn isrcs_of(song: &Value) -> Result<Vec<String>, String> {
    Ok(match property(song, "isrc")? {
        Some(Value::String(one)) if !one.is_empty() => vec![one.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    })
}

fn int(element: &Value, name: &str) -> Result<i32, String> {
    Ok(match property(element, name)? {
        Some(Value::Number(n)) => n.as_i64().and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
        _ => 0,
    })
}

/// The file's last write time in 100 ns ticks: 1601-01-01 for a file that is not there.
fn last_write_ticks(path: &str) -> i64 {
    const MISSING: i64 = 504_911_232_000_000_000;
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|modified| dotnet_ticks(DateTime::<Utc>::from(modified)))
        .unwrap_or(MISSING)
}

#[cfg(test)]
#[path = "duplicate_tests.rs"]
mod tests;
