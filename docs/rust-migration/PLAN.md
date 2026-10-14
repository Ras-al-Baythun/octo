# Migrate Octo from C# (.NET 9) to Rust

## Goal

Replace the ASP.NET Core app in `octo/` (about 55k lines of C# in 210 files) and its xUnit suite in `octo.Tests/` (about 36k lines in 125 files) with a single Rust binary. Existing installs must be able to swap the image without noticing: it keeps the same Subsonic API, the same admin API and UI, the same env vars, the same `/app/config` files and the same on-disk layout under `/music`.

**Decisions made while planning**

| Topic | Decision |
|---|---|
| Rollout | **Big-bang rewrite.** The Rust app is built until it matches the C# app feature for feature, then replaces it in one release. |
| Branch | A **long-lived `rust-rewrite` branch** that replaces `octo/` in place. The C# code comes out in the cutover commit. |
| Upstream changes | **None.** This is a fork and the C# code is frozen, so the parity target is commit `15d6840` (release `2026.10.03.2`). |
| Scope | **The C# app only.** These stay as they are: `yt-dlp-shim/` (Python), `octo/wwwroot/admin/` (HTML/JS/CSS, served unchanged), `scripts/updater/`, `install.sh` and `docker-compose.yml` (apart from the image build). |
| Tests | **Port the unit tests next to each Rust module, and add a black-box HTTP parity harness** that runs the C# and Rust builds side by side. |
| Cover art | **Perceptual tolerance.** Goldens in `CoverGolden/samples.json` must match at SSIM ≥ 0.98. Layout must match exactly, but anti-aliasing may differ. |

**Target stack** (check the versions with `ctx7` when work starts)

| Concern | C# today | Rust |
|---|---|---|
| HTTP server, middleware | ASP.NET Core, controllers | `axum` + `tower` / `tower-http` (CORS, static files, forwarded headers, tracing) |
| Async runtime, workers | `BackgroundService` (17 hosted services) | `tokio` tasks with a `CancellationToken` (`tokio-util`) |
| HTTP clients | `IHttpClientFactory` + delegating handlers | `reqwest` + `tower` layers for rate limits and retries (Deezer, AcoustID) |
| JSON | `System.Text.Json` | `serde` / `serde_json` (field names and casing must match byte for byte) |
| Subsonic XML | `XDocument` / `XElement` | `quick-xml` |
| Config, hot reload | env vars + `/app/config/settings.json` with `IOptionsMonitor` | custom loader (env + JSON) + `notify` watcher + `arc-swap` snapshot |
| Audio tags | TagLibSharp | `lofty` |
| Images, cover art | ImageSharp + ImageSharp.Drawing | `image` (WebP/PNG/JPEG) + `tiny-skia` + `cosmic-text` (shaping, CJK/emoji fallback) |
| Subprocesses | ffmpeg, fpcalc (`Process`) | `tokio::process` |
| Logging | `ILogger` + log redaction | `tracing` + `tracing-subscriber` with a redaction layer |
| OpenAPI (dev only) | Swashbuckle | `utoipa` (optional, can be dropped) |
| Tests | xUnit, Moq, `WebApplicationFactory` | `cargo test`, `wiremock`, `insta` snapshots, `axum-test` / `tower::ServiceExt` |
| BouncyCastle | referenced, but no `using` found | drop it (confirm in Phase 0) |

## Tasks

Phases are ordered by dependency. Each module task includes porting its xUnit tests. A module is not done until its tests pass under `cargo test` and its endpoints pass the parity harness.

### Phase 0: Groundwork and baseline

- [ ] Create the `rust-rewrite` branch from `15d6840` and tag the C# baseline (`csharp-final`).
- [ ] Record the C# baseline: idle and load RSS, cold-start time, image size, p50/p95 latency for `search3`, `getAlbum`, `stream` (Range) and `getCoverArt`.
- [ ] Make an endpoint inventory: every route in the 5 controllers (about 116 Subsonic and 75 admin/cover/lyrics/update attributes), with its query params and response formats (XML, JSON, JSONP if any). Check in as `docs/rust-migration/endpoints.md`.
- [ ] Make a config inventory: every env var in `.env.example` (about 145) and every key in `settings.json` → its settings class and field, its default, and whether it hot-reloads. Check in as `docs/rust-migration/config.md`.
- [ ] Make a state-file inventory: the 20 JSON files (`downloads-history.json`, `upgrades.json`, `lyrics-library.json` and the rest), each with its schema and writer, plus sample real-world files to use as fixtures.
- [ ] Confirm BouncyCastle is unused and list any other packages that are referenced but dead.
- [ ] Build the **parity harness**: a docker-compose profile with the C# image, the Rust image, a Navidrome holding a fixture library, and `wiremock` stubs for Last.fm, Deezer, iTunes, MusicBrainz, AcoustID, Lidarr, the lyrics sources and the yt-dlp shim. Add a request corpus (recorded plus hand-written) and a differ that normalizes volatile fields (timestamps, generated ids, ordering where it is unspecified).
- [ ] Record the corpus by running the C# app with request/response capture against the fixtures.

### Phase 1: Skeleton

- [ ] Set up the Cargo workspace: a `octo` binary crate plus library crates along the module seams (`octo-core` for identity, matching and models; `octo-subsonic`; `octo-media` for tags, covers and audio). Add `rustfmt`, `clippy -D warnings` and `cargo-deny`.
- [ ] Port the config system: the env loader, the `settings.json` loader with hot reload, the typed settings structs (`Models/Settings`, 12 files, about 2k lines), and a `SettingsFileWriter` equivalent that does atomic writes. Port `SettingsFileWriterTests` and `LibraryActionSettingsTests`.
- [ ] Set up app state and DI: a typed `AppState` holding `Arc`s of the services, standing in for the 142 registrations in `Program.cs`.
- [ ] Port the middleware: `AdminRequestGuard`, the global exception handler, forwarded headers, CORS ordering (the guard runs before CORS, as it does today), request logging with redaction (`LogRedactionTests`).
- [ ] Serve the static admin UI from `wwwroot/admin` and `Assets/`, keeping the URLs and caching headers it has today.
- [ ] Set up graceful shutdown and a worker supervisor that starts, cancels and restarts background tasks (`RestartTrackerTests`).
- [ ] Set up a Rust CI job (fmt, clippy, test, deny) on the branch, installing ffmpeg, fpcalc and the fonts that `ci.yml` installs today.

### Phase 2: Pure core (no I/O)

- [ ] `Services/Common` pure logic: `SongIdentity` (1.1k lines), `SingleFlight`, `PathHelper`, `PlaylistIdHelper`, `SongLength`, the live-version detection and the query-variant lookup. The Rust tests must read `docs/song-identity-cases.json`, the same file the Octo apps use.
- [ ] `Services/Tagging`: `ReleaseChooser`, `ReleaseDistance`, `ReleaseIdentifier`, `TagPlan`, `TagEvidence`, `CandidateSources`.
- [ ] `Services/Validation` and `Services/Metadata/GenreNormalizer`, `AcceptLanguageHeader`.
- [ ] `Services/Fingerprint/TrackMatchComparer`, and the download candidate matching (removed).
- [ ] `Services/Lyrics` pure parts: `LyricsText`, `LyricsIdentity`, `LyricsChoices`, LRC/word-timing parsing (`LyricsWordTimingTests`, `LyricsChoiceTests`).
- [ ] `Services/Updates`: `ReleaseVersion`, `ReleaseCheck`.
- [ ] Serializer symmetry: for every state file, read the fixture, write it back, and expect byte-for-byte equality (`SerializerSymmetryTests`).

### Phase 3: Subsonic surface

- [ ] Port the Subsonic models, `SubsonicModelMapper` and `SubsonicRequestParser` (all auth forms: `u`/`p`, `t`/`s`, apiKey if it is supported).
- [ ] Port `SubsonicResponseBuilder` (about 1k lines plus Lyrics) to both XML and JSON. Snapshot every response shape with `insta` against the C# output.
- [ ] Port `SubsonicProxyService`: forwarding to Navidrome, the header allowlist, `Range`/`Content-Range` passthrough, upstream status kept verbatim, streaming bodies with nothing buffered.
- [ ] Port `NavidromeSongPathResolver`, `SyncCatalogService`, `PlaylistSyncService` and `ExternalIdRegistry`.
- [ ] Port `SubSonicController`'s 116 routes, grouped as: system/auth → browsing → search (paging and budget: `SearchPagingTests`, `SearchBudgetTests`, `SearchSongPagePlannerTests`) → media (`stream`, `download`, `getCoverArt`) → playlists, stars and scrobble → lyrics.

### Phase 4: External integrations

- [ ] YouTube: the shim client and the resolver base URL (`YouTubeResolverBaseUrlTests`), and external playback (`ExternalPlaybackTests`).
- [ ] Deezer metadata, with its rate limiter as a tower layer (`DeezerMetadataServiceTests`, `DeezerRateLimitHandlerTests`).
- [ ] iTunes, cover lookup and clean cover client (`CoverLookupTests`, `CleanCoverClientTests`, `CoverChainTests`).
- [ ] MusicBrainz and AcoustID, with the AcoustID rate limiter (`AcoustIdLookupTests`, `MusicBrainz*Tests`, `IsrcVerificationTests`).
- [ ] Last.fm: the service, scrobbling with retries, radio recommendation, the radio stream service and the radio transcoder via ffmpeg (all `LastFm*Tests`, `ScrobbleRetryTests`).
- [ ] ListenBrainz.
- [ ] Lyrics sources: LRCLIB, Kugou, Netease, lyrics.ovh, the sidecar writer and `.lyricsfile` reader (`LyricsTests`, `LyricsSongSourceTests`).
- [ ] Notifications: Discord and ntfy sinks (`DiscordSinkTests`, `NtfySinkTests`, `NotificationServiceTests`).
- [ ] Lidarr: the client, heart acquisition and track fetcher (`Lidarr*Tests`).

### Phase 5: Acquisition pipeline (highest risk)

- [ ] The download client (removed).
- [ ] `BaseDownloadService` (1.9k lines): placement, tagging and attribution (`DownloadPlacementTests`, `DownloadTaggingTests`, `DownloadAttributionTests`, `AlbumFolderTests`, `AlbumFillInTests`).
- [ ] `AcquisitionTracker` and `HeartAcquisitionCoordinator` (`AcquisitionTrackerTests`, `HeartAcquisitionCoordinatorTests`, `HeartOwnershipTests`, `StarOnArrivalTests`, `AcquisitionEndpointTests`).
- [ ] Audio tag writing with `lofty`. For FLAC, MP3 (ID3v2.3/2.4), M4A and Opus, every frame and atom TagLibSharp writes today must read back the same (`TagWriterExtrasTests`, `MergedFormatTests`). Diff tag dumps from both implementations on the fixture files.
- [ ] Download verification: the fpcalc fingerprinter, the spectrum analyzer and loudness meter via ffmpeg, and the verification decision (`AudioFingerprinterTests`, `SpectrumAnalyzerTests`, `LoudnessMeterTests`, `DownloadVerificationDecisionTests`, `FailedRelayDetectionTests`, `RelayedRepeatsTests`).
- [ ] Local library service and directory browser (`LocalLibraryServiceTests`, `DirectoryBrowserTests`).

### Phase 6: Library jobs and background workers

- [ ] Library actions: the executor, worker, quarantine, keep and star, and the one-at-a-time rule (`LibraryAction*Tests`).
- [ ] Upgrade queue, quality upgrade, notice queue, review sweep, duplicates (`UpgradeQueueTests`, `QualityUpgradeTests`, `NoticeQueueTests`, `LibraryReviewSweepTests`, `DuplicateTests`).
- [ ] Generated playlists and the supersedable build coordinator (`GeneratedPlaylistTests`, `SupersedableBuildCoordinatorTests`).
- [ ] Genre backfill worker, journal and undo (`GenreBackfill*Tests`).
- [ ] Lyrics library job and steps (`LyricsLibraryStepsTests`).
- [ ] Make sure every worker resumes from its state file after a restart killed it mid-job, the same way the C# worker does.

### Phase 7: Cover art

- [ ] Load the embedded design (`Services/CoverArt/Design/*.json`, the Inter fonts, the backgrounds) with `include_bytes!`, and ship `OFL.txt` as a license file.
- [ ] Build the list-cover renderer with `tiny-skia` and `cosmic-text`: falling back to the system fonts Noto CJK, Symbola and DejaVu for scripts Inter has no letters for, list hues, logo watermark (`ListCoverTests`).
- [ ] Port `CoverArtService`, `CoverUpgrade` (1.2k lines), `ITunesCoverArtLookup` and the `CoverUpgradeController` routes (`CoverUpgradeTests`).
- [ ] Add a golden test harness: render `CoverGolden/samples.json`, compare with SSIM ≥ 0.98, and on failure write a diff image as a CI artifact.

### Phase 8: Admin and update APIs

- [ ] Port `AdminController`'s 54 routes, `LyricsAdminController` and `UpdateController`, with `AdminContractTests` and `CredentialCheckTests` as the contract. `admin.js` must work unmodified.
- [ ] Port `Services/Admin` and `UpdateHost`: the updater handshake through the `/app/config/update/` folder that `octo-updater.path` watches (`UpdateHostTests`). The C# app's `scripts/updater/test-updater.sh` must still pass.
- [ ] Show the version: replace the csproj `InformationalVersion` with a `CARGO_PKG_VERSION` plus build-time tag, in the same `2026.MM.DD.N` format the dashboard expects.

### Phase 9: Parity, hardening and cutover

- [x] Run the full parity corpus and fix every diff, or record it in `docs/rust-migration/known-diffs.md` with a reason.
- [ ] Soak test: run the Rust image against a real Navidrome library for at least 7 days, with a daily state-file and tag audit.
- [x] Upgrade test: start the Rust image on a `/app/config` and `/music` written by C# `2026.10.03.2`. No migration step may be needed, and every queue and journal must resume.
- [x] Downgrade test: the C# image can still read state written by Rust, or the unsupported files are documented.
- [x] Load test against the Phase 0 baseline. Results for these four: [`cutover-report.md`](cutover-report.md).
- [x] Rewrite the `Dockerfile`: `rust:<ver>-bookworm` builder (with `cargo-chef` for caching) → `debian:bookworm-slim` runtime with ffmpeg, fonts and `libchromaprint-tools`. Keep `EXPOSE 8080`, the volumes and the entrypoint semantics.
- [x] Update `ci.yml` (replace the .NET job with the Rust one, keep the shim and updater jobs) and `docker.yml` (no change to tags or caching). Both call `.github/workflows/rust.yml` (fmt, clippy, test, `cargo deny` with `deny.toml`); the release-tag check reads `VERSION`.
- [x] Cutover commit: delete `octo/*.cs`, `octo.Tests/`, `octo.sln` and `octo.csproj`, move `wwwroot/` and `Assets/` to their new home, and update the README badges and build docs. The new homes: `crates/octo/wwwroot/` and `crates/octo/Assets/`, the cover design in `crates/octo-media/assets/cover-design/`, the cover goldens in `crates/octo-media/testdata/cover-golden/`; the release is the root `VERSION` file, which the update helper reads (falling back to `octo/octo.csproj` in a C# folder).
- [ ] Merge `rust-rewrite` and tag the first Rust release.

## Key Results

1. **API parity:** every Subsonic and admin route in the Phase 0 inventory is served by Rust. The parity corpus shows **0 unexplained diffs**, and every entry in `known-diffs.md` has been reviewed.
2. **Test parity:** each of the 125 xUnit test files has a Rust counterpart (or a written reason it was dropped). `cargo test` is green in CI, and the shared `song-identity-cases.json` passes in full.
3. **Drop-in upgrade:** an existing install switches images with **no config edits and no data migration**. All about 145 env vars, `settings.json` with hot reload, and the 20 state files are read and written compatibly, and the round-trip tests are byte-identical.
4. **Media fidelity:** tags written by Rust read back the same as TagLibSharp's on the fixture set for FLAC, MP3, M4A and Opus. All cover goldens pass at SSIM ≥ 0.98.
5. **Unmodified clients:** the existing admin UI, the Octo desktop and Android apps, Feishin and the updater helper all work against the Rust build with no changes.
6. **Operational gains over the Phase 0 baseline:** idle RSS ≤ 50%, cold start ≤ 1s, runtime image ≤ the .NET image size, and p95 latency no worse on the benchmarked endpoints.
7. **Stability:** a 7-day soak with no panics, no stuck workers and no corrupted state files.
8. **Clean exit:** the main branch has no .NET SDK, C# source or `dotnet` steps in CI or the Dockerfile.
