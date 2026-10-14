//! The cover a download is tagged with.

use std::sync::Arc;
use std::time::Duration;

use octo_core::common::dotnet;
use octo_core::fingerprint::VerificationResult;
use octo_core::models::domain::Song;
use octo_core::outside::routing::{OutsideRouting, RoutingKind};
use octo_core::settings::{MetadataSettings, SettingsStore};
use octo_media::cover::cover_image;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::cover_keep::{self, OwnCoverJudgement, OwnCoverVerdict};
use super::{CoverArtAggregator, CoverArtArchiveLookup, ITunesCoverArtLookup};

/// The cover a download gets, where it came from, and whether it is simply the one the file
/// already carries (so there is nothing to rewrite). `headline` and `note` say in words what was
/// kept or replaced and why, for the download's timeline; both are None when the song had no
/// cover of its own to judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverChoice {
    pub bytes: Vec<u8>,
    pub source: String,
    pub keeps_existing: bool,
    pub headline: Option<String>,
    pub note: Option<String>,
}

impl CoverChoice {
    pub fn new(bytes: Vec<u8>, source: &str, keeps_existing: bool) -> Self {
        Self {
            bytes,
            source: source.to_string(),
            keeps_existing,
            headline: None,
            note: None,
        }
    }

    fn told(mut self, headline: String, note: Option<String>) -> Self {
        self.headline = Some(headline);
        self.note = note;
        self
    }
}

/// The covers a song has before the chain is asked: the one the file arrived with, whether that
/// file was filed under another album than the one Octo files it under, and, for a replacement
/// of a library song, the cover of the copy it replaces.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoversOnHand<'a> {
    pub arrived: Option<&'a [u8]>,
    pub arrived_from_other_album: bool,
    pub library_copy: Option<&'a [u8]>,
}

impl<'a> CoversOnHand<'a> {
    pub fn new(arrived: Option<&'a [u8]>) -> Self {
        Self {
            arrived,
            ..Self::default()
        }
    }
}

/// One cover the song already has, and whose it is, for the words.
#[derive(Clone, Copy)]
struct Own<'a> {
    bytes: &'a [u8],
    library_copy: bool,
    other_album: bool,
}

impl Own<'_> {
    fn kept(&self) -> &'static str {
        if self.library_copy {
            "Kept your old copy's cover"
        } else {
            "Kept the cover it came with"
        }
    }

    fn its(&self) -> &'static str {
        if self.library_copy {
            "your old copy's"
        } else {
            "its"
        }
    }
}

const APPLE_SOURCE: &str = "iTunes";
const ARCHIVE_SOURCE: &str = "Cover Art Archive";
const CATALOG_SOURCE: &str = "the catalog";
const SEARCH_SOURCE: &str = "a cover search";

/// One source's answer: its cover, or none (`bytes` None).
struct Found {
    source: &'static str,
    bytes: Option<Vec<u8>>,
    side: u32,
    same_art: bool,
}

impl Found {
    fn new(source: &'static str, bytes: Option<Vec<u8>>) -> Self {
        let side = bytes
            .as_deref()
            .filter(|bytes| !bytes.is_empty())
            .and_then(cover_image::measure)
            .map_or(0, |(width, height)| width.min(height));
        Self {
            source,
            bytes,
            side,
            same_art: false,
        }
    }
}

/// The download-time cover chain (#51). The download path used to embed one Deezer URL and stop,
/// so anything Deezer did not know was written with no art, while the aggregator that already
/// knew iTunes and Last.fm sat unused beside it.
///
/// Every strict source is asked (Apple's master of the same album, the Cover Art Archive when a
/// fingerprint named the release the album tag describes, the catalog's own cover), within
/// [`Self::GATHER_TIMEOUT`]; a cover search by name only when none of them has one at least
/// [`Self::SHARP_SIDE`] wide. The first strict answer of those three, catalog first, says what
/// the release looks like, and only covers that look like it count: Apple's sharper master of
/// another album never replaces a single's own art. The cover the song already has is judged
/// against the same picture ([`cover_keep`]) and stays unless a copy of the same art
/// [`cover_keep::MUCH_LARGER`] times its side was found. A library copy's own cover of 1000 px
/// or more stays as it is. Whatever happens, the timeline is told what became of the song's own
/// cover and what else was looked at.
pub struct DownloadCoverResolver {
    archive: Arc<CoverArtArchiveLookup>,
    aggregator: Arc<CoverArtAggregator>,
    /// The default client, for the catalog's cover URL.
    http: reqwest::Client,
    /// The metadata settings, read at every resolve.
    settings: Arc<SettingsStore>,
    itunes: Option<Arc<ITunesCoverArtLookup>>,
}

impl DownloadCoverResolver {
    /// A slow cover source must cost seconds, never the download: the whole finalize phase runs
    /// under the download lock.
    const CATALOG_TIMEOUT: Duration = Duration::from_secs(8);

    /// Big enough that the cover search by name is not asked: the catalog's own covers are
    /// 1000 px.
    pub const SHARP_SIDE: u32 = 1000;

    /// How long the cover sources may take together. Each has its own timeout too; this bounds
    /// the sum, since finalizing runs under the download lock.
    pub const GATHER_TIMEOUT: Duration = Duration::from_secs(20);

    pub fn new(
        archive: Arc<CoverArtArchiveLookup>,
        aggregator: Arc<CoverArtAggregator>,
        http: reqwest::Client,
        settings: Arc<SettingsStore>,
        itunes: Option<Arc<ITunesCoverArtLookup>>,
    ) -> Self {
        Self {
            archive,
            aggregator,
            http,
            settings,
            itunes,
        }
    }

    pub async fn resolve(
        &self,
        song: &Song,
        embedded: Option<&[u8]>,
        ct: &CancellationToken,
    ) -> Option<CoverChoice> {
        self.choose(song, &CoversOnHand::new(embedded), ct).await
    }

    pub async fn choose(
        &self,
        song: &Song,
        on_hand: &CoversOnHand<'_>,
        ct: &CancellationToken,
    ) -> Option<CoverChoice> {
        tokio::select! {
            biased;
            _ = ct.cancelled() => None,
            choice = self.choose_inner(song, on_hand) => choice,
        }
    }

    async fn choose_inner(&self, song: &Song, on_hand: &CoversOnHand<'_>) -> Option<CoverChoice> {
        let settings = self.settings.current().metadata.clone();
        let require_square = settings.replace_video_covers;
        let library_copy = on_hand.library_copy.filter(|bytes| !bytes.is_empty());
        let arrived = on_hand.arrived.filter(|bytes| !bytes.is_empty());

        // The library copy's cover is the picture the album shows now, beside its other songs,
        // so a good one stays without asking anyone. The cover art page is where it gets changed.
        if let Some(old) = library_copy {
            let old_judged = cover_keep::judge(old, None, require_square, false, true);
            if old_judged.verdict == OwnCoverVerdict::Keep {
                let own = Own {
                    bytes: old,
                    library_copy: true,
                    other_album: false,
                };
                return Some(kept(&own, &old_judged, Some(&old_judged.why), None));
            }
        }

        let mut owns = Vec::new();
        if let Some(small) = library_copy {
            owns.push(Own {
                bytes: small,
                library_copy: true,
                other_album: false,
            });
        }
        if let Some(arrived) = arrived {
            owns.push(Own {
                bytes: arrived,
                library_copy: false,
                other_album: on_hand.arrived_from_other_album,
            });
        }

        let mut found = self.gather(song, &settings, require_square).await;

        // What the release looks like: the catalog's cover of the album the song is filed under,
        // else the archive's cover of the release a fingerprint named, else Apple's master of the
        // album, all strict matches. A cover search by name is never the measure.
        let strict = |source: &str| {
            found
                .iter()
                .find(|f| f.bytes.is_some() && f.source == source)
                .and_then(|f| f.bytes.clone())
        };
        let reference = strict(CATALOG_SOURCE)
            .or_else(|| strict(ARCHIVE_SOURCE))
            .or_else(|| strict(APPLE_SOURCE));

        let judged: Vec<(Own, OwnCoverJudgement)> = owns
            .iter()
            .map(|own| {
                let judgement = cover_keep::judge(
                    own.bytes,
                    reference.as_deref(),
                    require_square,
                    own.other_album,
                    own.library_copy,
                );
                (*own, judgement)
            })
            .collect();
        let contender = first_largest(
            judged
                .iter()
                .filter(|(_, judgement)| judgement.verdict != OwnCoverVerdict::Replace),
            |(_, judgement)| judgement.side(),
        );

        // Only the same art counts: a sharper picture of something else is never taken for the
        // release's. With no strict source to say what the release looks like, the song's own
        // cover says it.
        let measure = reference.as_deref().or(contender.map(|(own, _)| own.bytes));
        let measure_looks = cover_image::looks_hash(measure);
        for candidate in &mut found {
            candidate.same_art = match (measure_looks, cover_image::looks_hash(candidate.bytes.as_deref())) {
                (Some(looks), Some(theirs)) => cover_image::look_alike(looks, theirs),
                _ => true,
            };
        }
        let best = first_largest(found.iter().filter(|f| f.bytes.is_some() && f.same_art), |f| {
            f.side
        });
        let considered = considered(&found);

        if let Some((keep, judgement)) = contender {
            match best {
                Some(best) if cover_keep::is_much_larger(best.side, judgement.side()) => {
                    let judgement = OwnCoverJudgement {
                        why: format!("{} has the same art much larger", best.source),
                        ..judgement.clone()
                    };
                    return Some(replaced(keep, &judgement, best, considered.as_deref()));
                }
                _ => {
                    let why = [
                        (judgement.same_art == Some(true)).then_some("the same art"),
                        match best {
                            None => Some("nothing else was found"),
                            Some(best) if best.side > judgement.side() => {
                                Some("nothing found was much larger")
                            }
                            Some(_) => None,
                        },
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("; ");
                    return Some(kept(keep, judgement, Some(&why), considered.as_deref()));
                }
            }
        }

        if let Some(best) = best {
            if best.side < Self::SHARP_SIDE {
                info!(
                    "Best cover for {} - {} is {} px, from {}",
                    song.artist, song.title, best.side, best.source
                );
            }
            if let Some((rejected, judgement)) = judged.first() {
                return Some(replaced(rejected, judgement, best, considered.as_deref()));
            }
            let bytes = best
                .bytes
                .clone()
                .expect("only answers with a cover are the best");
            let headline = format!("Cover from {}, {}", best.source, size(&bytes));
            return Some(CoverChoice::new(bytes, best.source, false).told(
                headline,
                join(&[Some("It came with no cover of its own."), considered.as_deref()]),
            ));
        }

        // Nothing found anywhere: the largest cover the song has stays, whatever it is. A picture
        // of another album or a blown-up one beats no picture at all.
        let fallback = first_largest(
            judged
                .iter()
                .filter(|(own, _)| cover_image::is_usable(Some(own.bytes), require_square)),
            |(_, judgement)| judgement.side(),
        );
        if let Some((last, judgement)) = fallback {
            return Some(kept(
                last,
                judgement,
                Some("nothing else was found"),
                considered.as_deref(),
            ));
        }

        let frame = arrived?;
        if !require_square {
            return None;
        }
        let cropped = cover_image::crop_to_square(frame)?;
        cover_image::is_usable(Some(&cropped), true).then(|| {
            CoverChoice::new(cropped, "the centre of a video frame", false).told(
                "Cut the cover out of its video frame".to_string(),
                join(&[
                    Some("It came with a video frame, not a square cover, and nothing else was found."),
                    considered.as_deref(),
                ]),
            )
        })
    }

    /// Every strict source's cover, in the order asked: Apple's master of the album, the
    /// archive's cover of the release a fingerprint named, the catalog's own cover. A sharp one
    /// no longer stops the others, since a larger copy of the same art may come after it. The
    /// cover search by name is asked only when none of those has one at least
    /// [`Self::SHARP_SIDE`] wide. Unusable answers (too small, or not square while video covers
    /// are replaced) count as none.
    async fn gather(&self, song: &Song, settings: &MetadataSettings, require_square: bool) -> Vec<Found> {
        // The sources share one budget: past it a source counts as having none.
        let deadline = tokio::time::Instant::now() + Self::GATHER_TIMEOUT;
        let mut found = Vec::new();
        let artist = song.primary_artist.as_deref().unwrap_or(&song.artist);
        let ask = |source: &'static str, bytes: Option<Vec<u8>>| {
            let usable = cover_image::is_usable(bytes.as_deref(), require_square);
            Found::new(source, if usable { bytes } else { None })
        };

        // A compilation's album artist is nobody Apple would list it under.
        if let Some(itunes) = &self.itunes
            && !song.is_compilation
        {
            let fetch = async {
                // A barcode the chooser found names one release outright, so Apple is asked by
                // it first and the master lookup below answers from that match without a search.
                if let Some(barcode) = song.barcode.as_deref().filter(|b| !b.is_empty())
                    && !dotnet::is_blank(&song.album)
                {
                    itunes
                        .prime_by_barcode(
                            &[(artist.to_string(), song.album.clone(), barcode.to_string())],
                            None,
                        )
                        .await;
                }
                itunes
                    .try_fetch_album_master(Some(artist), Some(&song.album), Some(&song.title))
                    .await
            };
            let master = tokio::time::timeout_at(deadline, fetch).await.ok().flatten();
            found.push(ask(APPLE_SOURCE, master.map(|bytes| bytes.to_vec())));
        }

        // Only when the album tag IS the release the fingerprint matched: a download tagged with
        // a compilation's name must not get the original album's cover.
        let named = |id: &Option<String>| id.as_deref().is_some_and(|id| !id.is_empty());
        if settings.use_cover_art_archive
            && (named(&song.music_brainz_release_id) || named(&song.music_brainz_release_group_id))
            && VerificationResult::album_is_from_release(song)
        {
            let fetch = self.archive.try_fetch(
                song.music_brainz_release_id.as_deref(),
                song.music_brainz_release_group_id.as_deref(),
            );
            let archived = tokio::time::timeout_at(deadline, fetch).await.ok().flatten();
            found.push(ask(ARCHIVE_SOURCE, archived.map(|bytes| bytes.to_vec())));
        }

        if let Some(url) = song
            .cover_art_url_large
            .as_deref()
            .or(song.cover_art_url.as_deref())
            .filter(|url| !url.is_empty())
        {
            let catalog = tokio::time::timeout_at(deadline, self.download(url))
                .await
                .ok()
                .flatten();
            found.push(ask(CATALOG_SOURCE, catalog));
        }

        if !found.iter().any(|f| f.side >= Self::SHARP_SIDE) {
            let routing = OutsideRouting {
                kind: if dotnet::is_blank(&song.album) {
                    RoutingKind::Song
                } else {
                    RoutingKind::Album
                },
                artist: Some(artist.to_string()),
                title: Some(song.title.clone()),
                album: Some(song.album.clone()),
                ..Default::default()
            };
            // The aggregator never fails; only the budget can cut it short.
            let searched = tokio::time::timeout_at(deadline, self.aggregator.get_cover(&routing, true))
                .await
                .ok()
                .flatten();
            found.push(ask(SEARCH_SOURCE, searched.map(|bytes| bytes.to_vec())));
        }
        found
    }

    async fn download(&self, url: &str) -> Option<Vec<u8>> {
        let attempt = async {
            let response = self.http.get(url).send().await?;
            if !response.status().is_success() {
                return anyhow::Ok(None);
            }
            Ok(Some(response.bytes().await?.to_vec()))
        };
        match tokio::time::timeout(Self::CATALOG_TIMEOUT, attempt).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => {
                debug!("cover download {url} failed: {error}");
                None
            }
            Err(_) => {
                debug!("cover download {url} failed: The operation was canceled.");
                None
            }
        }
    }
}

/// The first of the largest: a later item of the same size never replaces an earlier one.
fn first_largest<T>(items: impl Iterator<Item = T>, side: impl Fn(&T) -> u32) -> Option<T> {
    let mut best: Option<(u32, T)> = None;
    for item in items {
        let item_side = side(&item);
        if best.as_ref().is_none_or(|(known, _)| item_side > *known) {
            best = Some((item_side, item));
        }
    }
    best.map(|(_, item)| item)
}

/// "Also looked at: iTunes had none, Cover Art Archive 1200 px, the catalog 1000 px."
fn considered(found: &[Found]) -> Option<String> {
    if found.is_empty() {
        return None;
    }
    let parts: Vec<String> = found
        .iter()
        .map(|f| match (&f.bytes, f.same_art) {
            (None, _) => format!("{} had none", f.source),
            (Some(_), true) => format!("{} {} px", f.source, f.side),
            (Some(_), false) => format!("{} {} px of a different picture", f.source, f.side),
        })
        .collect();
    Some(format!("Also looked at: {}.", parts.join(", ")))
}

/// The sentences that say something, one space apart; None when none does.
fn join(sentences: &[Option<&str>]) -> Option<String> {
    let text = sentences
        .iter()
        .flatten()
        .filter(|s| !dotnet::is_blank(s))
        .copied()
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

fn kept(
    own: &Own,
    judgement: &OwnCoverJudgement,
    why: Option<&str>,
    considered: Option<&str>,
) -> CoverChoice {
    let size = format!("{} x {} px", judgement.width, judgement.height);
    let kb = kb(own.bytes);
    let facts = [Some(size.as_str()), Some(kb.as_str()), why]
        .into_iter()
        .flatten()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
        + ".";
    let source = if own.library_copy {
        "your old copy"
    } else {
        "the file itself"
    };
    CoverChoice::new(own.bytes.to_vec(), source, !own.library_copy)
        .told(own.kept().to_string(), join(&[Some(&facts), considered]))
}

fn replaced(
    own: &Own,
    judgement: &OwnCoverJudgement,
    found: &Found,
    considered: Option<&str>,
) -> CoverChoice {
    let bytes = found
        .bytes
        .clone()
        .expect("only answers with a cover replace one");
    let headline = if judgement.side() > 0 {
        format!(
            "Replaced {} {} px cover with a {} one from {}",
            own.its(),
            judgement.side(),
            size(&bytes),
            found.source
        )
    } else {
        format!(
            "Replaced {} cover with a {} one from {}",
            own.its(),
            size(&bytes),
            found.source
        )
    };
    let why = sentence(&judgement.why);
    CoverChoice::new(bytes, found.source, false).told(headline, join(&[why.as_deref(), considered]))
}

fn size(bytes: &[u8]) -> String {
    match cover_image::measure(bytes) {
        Some((width, height)) => format!("{width} x {height} px"),
        None => "new".to_string(),
    }
}

fn kb(bytes: &[u8]) -> String {
    format!("{} KB", ((bytes.len() + 512) / 1024).max(1))
}

/// The words as a sentence: a capital first (unless they start with a name such as "iTunes")
/// and a full stop.
fn sentence(words: &str) -> Option<String> {
    if dotnet::is_blank(words) {
        return None;
    }
    let mut chars = words.chars();
    let first = chars.next()?;
    let keep = first.is_lowercase() && chars.next().is_some_and(char::is_uppercase);
    let text = if keep {
        words.to_string()
    } else {
        first.to_uppercase().collect::<String>() + &words[first.len_utf8()..]
    };
    Some(text + ".")
}

#[cfg(test)]
#[path = "download_cover_resolver_tests.rs"]
mod tests;
