# Porting waves

The plan's phases are cut into tasks that can be ported side by side. Each task names its C#
sources, the tests that move with it, and its target crate. A wave starts once the waves it
depends on have been merged into `rust-rewrite`. Status: ☐ not started · ◐ in progress · ☑ merged.

## Wave 1: foundations (needs only `config` and `json`)

| ☐ | Task | C# sources | Tests | Crate |
|---|---|---|---|---|
| ☑ | 1-A settings | `Models/Settings/*`, `Admin/SettingsFileWriter`, `Admin/RestartTracker`, plus the config layering in `Program.cs` | SettingsFileWriter, LibraryActionSettings, RestartTracker | core |
| ☑ | 1-B models + common | `Models/{Domain,Search,Download,Subsonic}/*`, `Common/{Error,Result,SongIdentity,LiveVersion,PlaylistIdHelper,SingleFlight,SupersedableBuildCoordinator,OctoUserAgent,LogRedaction}` | SongIdentity, SongIdentityCases, LiveVersion, PlaylistIdHelper, SingleFlight, SupersedableBuildCoordinator, LogRedaction, QueryVariantLookup | core |
| ☑ | 1-C list covers | `CoverArt/{CoverBook,CoverPainter,CoverLayout,CoverFonts,CoverColours,CoverBackgrounds,CoverVeil,CoverImage,CoverFiles,CoverArtService}` and the design assets | ListCover, plus the golden harness | media |
| ☑ | 1-D audio tools | `Audio/LoudnessMeter`, `Fingerprint/{AudioFingerprinter,SpectrumAnalyzer}` | LoudnessMeter, AudioFingerprinter, SpectrumAnalyzer | media |

## Wave 2: pure logic, app skeleton, metadata clients (needs 1-A, 1-B)

| | Task | C# sources | Tests | Crate |
|---|---|---|---|---|
| ☑ | 2-A tagging | `Tagging/*`, `Fingerprint/TrackMatchComparer`, the verification types from `Fingerprint/DownloadVerificationService` (types only), `Common/PathHelper` | ReleaseChooser, ReleaseDistance, TrackMatchComparer, PathHelper, PathHelperLayout | core (the pure parts) |
| ☑ | 2-B lyrics/metadata/updates/validation, pure parts | `Lyrics/{LyricsText,LyricsModels,LyricsIdentity,SongLyrics,LyricsfileReader}`, `Metadata/{GenreNormalizer,AcceptLanguageHeader,GenreBackfillState,GenreBackfillJournal}`, `Updates/{ReleaseVersion,UpdateHost}`, `Validation/*` (base and result) | LyricsWordTiming, GenreNormalizer, AcceptLanguageHeader, UpdateHost; the pure parts of Lyrics, LyricsChoice and ReleaseCheck | core + octo |
| ☑ | 2-C app skeleton (Phase 1) | `Program.cs` wiring, `Middleware/*`, static files, CORS, tracing with redaction, graceful shutdown, worker supervisor | LogRedaction (the logging layer), RestartTracker (wiring) | octo |
| ☑ | 2-D metadata clients | `Metadata/{DeezerMetadataService,DeezerRateLimiter,DeezerRateLimitHandler}`, `Fingerprint/{MusicBrainzClient,AcoustIdClient,AcoustIdRateLimiter,AcoustIdRateLimitHandler}`, `CoverArt/{ICoverArtSource,DeezerCoverArtLookup,ITunesCoverArtLookup,LastFmCoverArtLookup,CoverArtArchiveLookup,CoverArtAggregator}` | DeezerMetadataService, DeezerRateLimitHandler, AcoustIdLookup, MusicBrainzReleaseDetails, MusicBrainzStudioAlbum, IsrcVerification, CoverLookup, CleanCoverClient | octo |
| ☑ | 2-E parity harness | docker-compose profile, Navidrome fixture library, wiremock stubs, request corpus, differ, C# recording | - | `parity/` |

## Wave 3: integrations and stores (needs wave 2)

| | Task | C# sources | Tests |
|---|---|---|---|
| ☑ | 3-A Subsonic wire | `Subsonic/{SubsonicRequestParser,SubsonicCredential,SubsonicModelMapper,SubsonicResponseBuilder*,SyncCatalogResponse}` | SubsonicRequestParser, SubsonicModelMapper, SubsonicResponseBuilder |
| ☑ | 3-B Last.fm, ListenBrainz, notifications | `LastFm/{LastFmService,LastFmScrobbleService,LastFmSearchCleanup}`, `ListenBrainz/*`, `Notifications/*`, `Subsonic/RecentScrobbles` | LastFmService, LastFmScrobble, ScrobbleRetry, LastFmSearchCleanup, DiscordSink, NtfySink, NotificationService |
| ☑ | 3-C lyrics sources and service | `Lyrics/{LrclibLyricsSource,KugouLyricsSource,NeteaseLyricsSource,LyricsOvhLyricsSource,LyricsService,LyricsChoices,LyricsSidecarWriter}` | Lyrics, LyricsSongSource, LyricsChoice |
| ☑ | 3-D tags | `Common/TagWriterExtras`, the tag-writing parts of `Common/BaseDownloadService`, `Library/KeptIdentity` | TagWriterExtras, MergedFormat, KeptIdentity |
| ☑ | 3-E Navidrome plumbing | `Subsonic/{SubsonicProxyService,NavidromeIdentityService,SubsonicDiscoveryService,CredentialCheck,RequestIdentity,SearchBudget,SearchSongOrder,SearchSongPagePlanner}`, `Library/{NavidromeSongPathResolver,NavidromePlaylistApi}`, `Validation/{SubsonicStartupValidator,StartupValidationOrchestrator}` | SubsonicProxyService, CredentialCheck, RequestIdentity, SearchBudget, SearchPaging, SearchSongPagePlanner, NavidromeSongPathResolver |
| ☑ | 3-F stores and YouTube | `{ExternalIdRegistry,RadioQueueStore,SongLength}`, `Common/DownloadConcurrency`, `Local/DownloadHistoryService`, `Admin/{BrowseSessionStore,DirectoryBrowser}`, `YouTube/YouTubeResolver`, `Updates/ReleaseCheck` | ExternalIdRegistry, SongLength, BrowseSessionStore, DirectoryBrowser, YouTubeResolverBaseUrl, ReleaseCheck, plus state-file round trips |

## Wave 4: acquisition pipeline (needs wave 3)

| | Task | C# sources | Tests |
|---|---|---|---|
| ☑ | 4-A download client and metadata | (removed) | (removed) |
| ☑ | 4-B download base and verification | `Common/BaseDownloadService`, `Fingerprint/DownloadVerificationService`, `CoverArt/DownloadCoverResolver`, `Common/AlbumFillIn`, `IDownloadService` | DownloadPlacement, DownloadTagging, DownloadAttribution, AlbumFolder, AlbumFillIn, DownloadVerificationDecision, FailedRelayDetection, RelayedRepeats, CoverChain |
| ☑ | 4-C download service | (removed) | (removed) |
| ☑ | 4-D acquisition orchestration | `Common/{AcquisitionTracker,AcquisitionWorker,AcquisitionActivity,TrackAcquisitionQueue,HeartAcquisitionCoordinator,StarOnArrival,ExternalSearchService,CacheCleanupService}`, `Library/{HeartOwnership,LibraryOwnership,ReplacementHandoff}`, `Local/LocalLibraryService` | AcquisitionTracker, HeartAcquisitionCoordinator, HeartOwnership, StarOnArrival, LibraryOwnership, LocalLibraryService, ExternalSearchService, HonestOutsideSongs, OutsideSongSignIn |
| ☑ | 4-E Lidarr | `Lidarr/*` | LidarrClient, LidarrHeartProtection, LidarrTrackFetcher |

## Wave 5: library jobs, radio, background workers (needs wave 4)

| | Task | C# sources | Tests |
|---|---|---|---|
| ☑ | 5-A library actions | `Library/{LibraryActionExecutor,LibraryActionJournal,LibraryActionQuarantine,LibraryActionPlaylistWorker,LibraryActionPlaylistProvisioner,LibraryActionRatingWorker,UpgradeSources}` | LibraryActionKeep, LibraryActionKeepIdentity, LibraryActionOneAtATime, LibraryActionQuarantine, LibraryActionStar, LibraryActionWorker, UpgradeSources, KeptIdentity |
| ☑ | 5-B queues and sweeps | `Library/{UpgradeQueue,QualityUpgradeWorker,NoticeQueue,NoticePlaylistWorker,LibraryReviewSweepWorker,DuplicateScanWorker}` | UpgradeQueue, QualityUpgrade, NoticeQueue, LibraryReviewSweep, Duplicate |
| ☑ | 5-C Last.fm radio | `LastFm/*Radio*`, `IcyMetadataStream`, `Models/Radio/LastFmRadioState`, `Library/GeneratedPlaylistService` | LastFmRadioCore, LastFmRadioSpacing, LastFmRadioTrackResolverMatch, GeneratedPlaylist |
| ☑ | 5-D genre backfill and cover upgrade | `Metadata/GenreBackfillWorker`, `CoverArt/{CoverUpgrade,AlbumCoverFinder}` | GenreBackfill, GenreBackfillUndo, CoverUpgrade |
| ☑ | 5-E lyrics library | `Lyrics/{LyricsLibraryJob,LyricsLibrarySteps}` | LyricsLibrarySteps |
| ☑ | 5-F sync catalog and playlists | `Subsonic/{SyncCatalogService,PlaylistSyncService}` | SyncCatalog, ExternalPlayback |

## Wave 6: controllers and cutover (needs wave 5)

| | Task | C# sources | Tests |
|---|---|---|---|
| ☑ | 6-A Subsonic controller | `Controllers/SubSonicController` (4.4k lines) | AcquisitionEndpoint, LibraryActionEndpoint, LastFmRadioController |
| ☑ | 6-B admin controllers | `Controllers/{AdminController,CoverUpgradeController,LyricsAdminController,UpdateController}` | AdminContract |
| ☑ | 6-C parity run, Dockerfile, CI, cutover | Phase 9 | the full corpus |
