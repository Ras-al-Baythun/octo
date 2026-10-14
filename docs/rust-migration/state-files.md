# State files: what Octo persists on disk

Phase 0 inventory for the Rust rewrite, taken at commit `15d6840` (release `2026.10.03.2`). Wave G1 added the files and fields of release 2026.10.05: §4.26 to §4.32, `admin-recovery-code` in §5, and the notes marked 2026.10.05 under the files that gained fields. It lists every file the C# app writes or reads back as state. For each one it gives the path and the root it lives under, the writer, the readers, the exact serialized shape, whether the write is atomic, locking, and what happens when the file is missing, partial or corrupt.

Fixtures for every JSON file are in [`fixtures/state/`](fixtures/state/). They are **not hand-written**. A small C# program, [`fixtures/generator/`](fixtures/generator/), builds sample objects with the real Octo types and serializes them with exactly the options each writer uses. Some fixtures come out of the real store class itself: `external-ids.json` from `ExternalIdRegistry`, `settings.json` from `SettingsFileWriter`, `genre-backfill-journal.jsonl` from `GenreBackfillJournal` and `update/request` from `UpdateHost`. Every JSON fixture was then deserialized back into its C# type and re-serialized, and the output was byte-identical in every case. See [Regenerating the fixtures](#regenerating-the-fixtures).

> **`octo.Tests/SerializerSymmetryTests.cs` is not a state-file test.** The plan's Phase 2 line ("Serializer symmetry: for every state file, read the fixture, write it back... (`SerializerSymmetryTests`)") misreads it. That file checks that the **Subsonic XML and JSON responses** carry the same fields: `SubsonicResponseBuilder.ConvertSongToJson` and `ConvertSongToXml` produce the same scalar keys, and so do album and artist. It also checks that an XML song declares `suffix`/`contentType`/`bitRate` (`m4a`/`audio/mp4`/`128`, or `flac` when `WaitForLosslessOnPlay`), that an unknown year is left out, and that numbers render invariantly. No C# test round-trips the state files. The state-file round-trip test is **new work** for the Rust port, and the fixtures here are its input. Port `SerializerSymmetryTests` with the Subsonic module in Phase 3.

---

## 1. Roots

| Root | How it resolves | Controlled by |
|---|---|---|
| **Config dir** `/app/config` | Hard-coded: `const string SettingsFilePath = "/app/config/settings.json"` (`octo/Program.cs:27`). Every state file is `Path.Combine(Path.GetDirectoryName(SettingsFilePath), name)`. | Nothing in the app. The host side is the bind mount `${OCTO_CONFIG_DIR:-./octo-config}:/app/config` (`docker-compose.yml:240`). |
| **Download dir** (configured) | `configuration["Library:DownloadPath"]`, falling back to `./downloads` (or `<cwd>/downloads` in `LocalLibraryService`). | `Library__DownloadPath` (compose sets `/music`, `docker-compose.yml:57`). Host side: `DOWNLOAD_PATH`. |
| **Download dir** (effective) | `NavidromeIdentityService.EffectiveDownloadPath(configured)` (`Services/Subsonic/NavidromeIdentityService.cs:76`): the music folder detected from Navidrome when `Subsonic:AutoDetectDownloadPath` is true and one was found, otherwise the configured path. Used by `BaseDownloadService.DownloadPath` (`Services/Common/BaseDownloadService.cs:48`). | `Subsonic__AutoDetectDownloadPath` (`AUTO_DETECT_DOWNLOAD_PATH`, default true). |
| **Music root** (library actions) | The library root the action resolved the file under (Navidrome's library path). Quarantine is `musicRoot/<QuarantineDirectory>`. | `LibraryActions__QuarantineDirectory` (`LIBRARY_ACTIONS_TRASH_DIR`, default `.octo-trash`). |
| **Cache dir** | `PathHelper.GetCachePath()` = `Path.Combine(Path.GetTempPath(), "octo-cache")` (`Services/Common/PathHelper.cs:97`). On Linux `Path.GetTempPath()` is `$TMPDIR` or `/tmp/`, so normally `/tmp/octo-cache`. | `TMPDIR` only. **Not on a volume.** It is lost when the container is recreated. |

> Note that `.mappings.json` and the m3u playlists use the *configured* `Library:DownloadPath`, but downloads land under the *effective* path. With auto-detect on, the two can differ.

---

## 2. Serializer conventions (System.Text.Json)

Unless a file says otherwise below, the writer calls `JsonSerializer.Serialize(value)` with **no options**. That means:

| Aspect | Behavior | Rust note |
|---|---|---|
| Property names | **PascalCase, exactly as declared in C#**. There is no naming policy, so it is not camelCase. Only two types use `[JsonPropertyName]`: the journal lines `GenreJournalEntry` (`p`,`b`,`a`,`t`,`r`) and `CoverUpgradeJournal.Entry` (`p`,`k`,`h`,`r`). | `#[serde(rename_all = "PascalCase")]` usually works, but rename fields explicitly where the C# name has odd casing (`ETag`, `YouTubeId`, `IsLocal`...). |
| Property order | Declaration order. For positional records, the constructor parameters come first, then properties declared in the record body (for example `LibraryActionEntry`: the 14 positional ones, then `HistoryKept`, `RevealedPath`). | Rust struct field order = output order. |
| **Computed get-only properties are written** | STJ serializes every public getter. Unless it is marked `[JsonIgnore]`, a computed property is in the file. On read it is ignored, because there is no setter or constructor parameter. Cases: `ExternalRouting.HasYouTube`/`HasArtistTitle`, `GenreBackfillRun.CanResume`, `LyricsLibraryRun.CanResume`, `LyricsPin.IsHidden` + `LyricsPin.Lyrics` (a whole nested `LyricsResult` with its own computed `HasSynced`, `HasPlain`, `HasWordTiming`, `Timing`, `IsSongsOwn`), and `LyricsLibraryRow.Found` (also a `LyricsResult`). Not written: `CoverUpgradeRun.DryRun`/`CanResume` and `NoticeEntry.IsOpen` (`[JsonIgnore]`). | Rust must **emit** these, computed the same way, to stay byte-identical (and for a C# downgrade it does no harm). On read, ignore them. |
| Nulls | **Written** as `null`. No `DefaultIgnoreCondition` is set anywhere. | No `skip_serializing_if`. `Option<T>` → `null`. |
| Enums | **Numbers** (the underlying int), except in `notice-queue.json`, which uses `JsonStringEnumConverter` with no naming policy (names exactly as declared: `"Review"`, `"SourceDisagreed"`). | `serde_repr`, plus a string enum for the notice queue. The converter also accepts integers and is case-insensitive on read. |
| Indentation | Compact: no whitespace and **no trailing newline**. `WriteIndented = true` only for `lastfm-radio-state.json`, `.mappings.json` and `settings.json`: 2 spaces, `"key": value` with one space after the colon, `\n` line endings on Linux, no trailing newline, and empty arrays and objects written as `[]` / `{}`. | serde_json's `PrettyFormatter` matches the 2-space shape. Check the empty-collection and `": "` details in the round-trip test. |
| **String escaping** | The default `JavaScriptEncoder` (nobody sets `Encoder`, except `NtfySink`, which does not touch disk). **Every non-ASCII character is escaped** as `\uXXXX` with **upper-case hex** (`ó` → `ó`, `宇` → `宇`, emoji → a surrogate pair `🎵`). HTML-sensitive characters are escaped too: `"` → `"` (**not** `\"`), `'` → `'`, `&` → `&`, `<` → `<`, `>` → `>`, `+` → `+`, `` ` `` → ```, DEL → `\u007F`, NBSP → ` `, U+2028 → ` `. Short escapes stay short: `\\`, `\n`, `\r`, `\t`, `\b`, `\f`. Other control characters → `\u00XX`. `/` is **not** escaped. All of this was checked by running .NET 9. | serde_json writes UTF-8 and `\"`. **A custom `serde_json::ser::Formatter` (`write_string_fragment` / `write_char_escape`) that reproduces this table is required** for byte equality. On read, accept any valid JSON escaping. |
| `DateTime` | ISO 8601 with **the fraction trimmed of trailing zeros, up to 7 digits (100 ns ticks)**, and no fraction at all when it is zero. `Kind=Utc` → `Z` suffix (`"2026-10-03T12:05:30Z"`, `"2026-10-03T14:25:00.1Z"`, `"2026-10-03T14:22:05.1234567Z"`). `Kind=Unspecified` → no suffix. This happens in practice: `DateTime.MinValue` is written as `"0001-01-01T00:00:00"` (see `NoticeEntry.NextLookupUtc` in the fixture). On read, `Z` → Utc, no suffix → Unspecified, and an offset like `+02:00` is **converted to local time**. | Keep 100 ns precision, trim zeros, and keep "had a Z or not" so Unspecified values round-trip. `chrono::DateTime<Utc>` alone is not enough. |
| `DateTime` stored as a string | `DownloadHistoryEntry.DownloadedAt` is a `string` filled with `DateTime.UtcNow.ToString("o")`, which **always has 7 fraction digits** (`"2026-10-02T09:01:44.0807210Z"`). The same instant as a real `DateTime` property elsewhere comes out as `...44.080721Z`. | Keep it as an opaque `String`. |
| `double` | Shortest round-trip form: `1.0` → `1`, `-0.0` → `-0`, `0.042`, `1E-05`, `2.5E-07`, `1E+21`, `1.2345678901234568E+20`; `1e16` → `10000000000000000`. NaN and Infinity **throw** on write, so they never reach disk. | serde_json (ryu) writes `1.0` and `1e-5`. **A custom number formatter is required**: integral doubles without `.0`, and .NET's exponent style. |
| `int`/`long` | Plain integers. Reading `"5"` (a string) **fails**, because `NumberHandling` is strict. | Default serde. |
| Dictionaries | Keys written as-is (no key policy), in insertion order. `Dictionary<,>` enumerates in insertion order unless entries were removed and their slots reused. | `indexmap::IndexMap` to keep the order. |
| Encoding | `File.WriteAllText` / `AppendAllText` write UTF-8 **without a BOM**. Readers (`File.ReadAllText`) accept a BOM. | Write no BOM, tolerate one on read. |
| Reading | Property names are matched **case-sensitively**. Unknown properties are ignored. Missing properties get the C# initializer default (classes) or `default(T)` (positional record parameters). An explicit `null` for a non-nullable C# `string` is accepted and yields null. Comments and trailing commas are **errors** (except in `settings.json`). | `#[serde(default)]` on every struct, no `deny_unknown_fields`, and string fields that tolerate `null`. |

---

## 3. Write and recovery at a glance

"Atomic" means the file is written to a temp file and then renamed over the target (`File.Move(tmp, path, overwrite: true)`, which is `rename(2)` on Linux). There is **no `fsync`** anywhere, so a power loss can still leave an empty or old file. "Coalesced" means a dirty flag plus a timer flush, a final flush in `Dispose()` (DI disposes the factory-created singletons on graceful shutdown), and up to one interval of changes lost on a crash.

| File | Atomic | Temp name | When written | Locking | Missing | Corrupt or unreadable |
|---|---|---|---|---|---|---|
| `settings.json` | yes | `settings.json.tmp` | every admin save | `lock` per writer | `{}` | the writer **refuses** to overwrite (throws `SettingsFileCorruptException`); display readers get `{}` |
| `downloads-history.json` | yes | `.tmp` | each finished download | `lock` | empty | warn, start empty (overwritten on the next record) |
| `lastfm-radio-state.json` | yes | `.tmp` | every change | `lock` | empty doc | warn, start empty. `Version != 1` → start empty |
| `external-ids.json` | yes | `.tmp` | coalesced, 10 s | LRU lock, `Interlocked` dirty flag | empty | warn, cold start |
| `browse-sessions.json` | yes | `.tmp` | create/revoke/expire, and slides at most once a day per session | save lock | empty | warn, everyone signs in again |
| `genre-backfill.json` | yes | `.tmp` | coalesced, 10 s, plus immediately on `Replace` | `lock` | Idle run | warn, Idle run |
| `genre-backfill-journal.jsonl` | **append**; rewrite atomic | `.tmp` (rewrite) | one line per changed file | `lock` | no undo | bad lines skipped (counted, warned) |
| `cover-upgrade.json` | yes | `.tmp` | coalesced, 10 s, plus on `Replace` | `lock` | Idle run | warn, Idle run |
| `cover-upgrade-journal.jsonl` | **append**; rewrite atomic | `.tmp` | one line per changed file | `lock` | no undo | bad lines skipped silently |
| `library-actions.json` | yes | `.tmp` | coalesced, 10 s, plus **synchronous `Flush()` before a file is moved** (write-ahead) | three locks (`_lock`, `_flushLock`, `_reconcileLock`) | empty | warn, empty |
| `notice-queue.json` | yes | `.tmp` | coalesced, 5 s, plus explicit `Flush()` | `_lock` + `_flushLock` | empty | **moved aside** to `notice-queue.json.corrupt-<UtcTicks>`, start empty |
| `generated-playlists.json` | yes | `.tmp` | each count refresh | `lock` | empty | **moved aside** to `.corrupt-<UtcTicks>`, rebuilt |
| `quality-upgrade.json` | yes | `.tmp` | every change | `lock` (serialized inside, written outside) | empty | warn, empty |
| `upgrades.json` | yes | `.tmp` | every change | `lock` | empty | warn, empty |
| `update/release.json` | yes | `.tmp` | each GitHub check | `SemaphoreSlim` gate + `lock` | empty | warn, fresh check |
| `review-sweep.json` | yes | `.tmp` | on change if 5 s have passed since the last write, otherwise by a 5 s timer, plus explicit `Flush()` | `_lock` + `_flushLock` | default state | **moved aside** to `.corrupt-<UtcTicks>`, start over |
| `itunes-masters.json` | yes | `.tmp` | coalesced, 15 s | `ConcurrentDictionary`, `Interlocked` | empty | debug log, empty |
| `lyrics-choices.json` | yes | `.tmp` | every change | `lock` | empty | warn, no pins |
| `lyrics-library.json` | yes | `.tmp` | coalesced, 10 s, plus on `Replace` | `lock` | Idle run | warn, Idle run |
| `lyrics-undo.jsonl` | **append** only | none | one line per Save write | `lock` | no undo | bad lines skipped silently |
| `<download dir>/.mappings.json` | **NO**: `File.WriteAllTextAsync` straight over the file | none | every register/forget | `SemaphoreSlim(1)` | empty | **not caught: the exception reaches the caller**, and every download and lookup that loads mappings fails until the file is fixed |
| `*.octo-action.json` (quarantine) | **NO** | none | once per quarantined file | none | restore refused ("no manifest") | `null` (restore refused) |
| radio `<key>.mp3.json` | yes | `<sidecar>.<guid>.tmp` | after a transcode | none | profile unknown | `null` |
| `update/request` | yes | `request.tmp` | Update now | `lock` | no pending request | key=value parser skips bad lines |
| `imports.json` (2026.10.05) | yes | `.tmp` | every change | `lock` | empty | warn, empty |
| `imports-trickle.json` (2026.10.05) | yes | `.tmp` | every change | `lock` | empty | warn, empty. A job `downloading` at load → `queued` |
| `spotify-accounts.json` (2026.10.05) | yes, mode 0600 | `.tmp` | every change | `lock` | no sign-ins | warn, no sign-ins |
| `radio-outcomes.json` (2026.10.05) | yes | `.tmp` | at most every few seconds, plus on shutdown | `_lock` + `_fileLock` | learning starts again | warn, learning starts again; null entries dropped |
| `sonic-features.json` (2026.10.05) | yes | `.tmp` | at most every few seconds | `_lock` + `_fileLock` | nothing analyzed | warn, start over; null entries dropped |
| `tag-edits.json` (2026.10.05) | yes | `.tmp` | every edit (a library-wide run every few files) | `lock` | no undo | warn, no undo |
| `explicit-backfill.json` (2026.10.05) | yes | `.tmp` | coalesced, 10 s, plus on `Replace` | `lock` | Idle run | warn, Idle run. `Running` at load → `Interrupted` |
| `admin-recovery-code` (2026.10.05, text) | yes, mode 0600 | `.tmp` | first read, and each time the code is used | `lock` | a new code is written | an unreadable file: no code (sign in with Navidrome) |

---

## 4. JSON state files, one by one

All paths are relative to `/app/config` unless they say otherwise. Line numbers are at `15d6840`. "Readers" lists the code that consumes the in-memory state the store loaded. Only the store itself touches the file.

### 4.1 `settings.json`

The editable config. Its keys are covered by `config.md`. It is listed here because it is a file Octo writes.

- **Path:** `/app/config/settings.json`, hard-coded (`Program.cs:27`). It is added as the **last** configuration source, `AddJsonFile(..., optional: true, reloadOnChange: true)` (`Program.cs:28`), so it overrides env vars and hot-reloads.
- **Writer:** `SettingsFileWriter` (`Services/Admin/SettingsFileWriter.cs`): `Merge` :91, `Update` :120, `Replace` :133, `Write` :158-165. Callers: `AdminController` (settings save, `:1048`) and `LastFmScrobbleService` (Connect/Disconnect writes `LastFm.UserSessions.<user>`, `SessionsIn` at `:737`).
- **Readers:** the configuration system (`IOptionsMonitor<T>` for every settings class), `SettingsFileWriter.Load` :44 (display) and `ReadForWrite` :141.
- **Format:** a free-form `JsonObject` tree (section → key → value) written with `content.ToJsonString(new JsonSerializerOptions { WriteIndented = true })`: 2-space indent, default escaping (non-ASCII → `\uXXXX`), key order as in the parsed file plus new keys appended. Parsed with `CommentHandling.Skip` and `AllowTrailingCommas = true`. **Comments in the file are lost on the first save.**
- **Atomic:** temp file + rename. `lock (_lock)` per instance.
- **Corrupt:** `ReadForWrite` throws `SettingsFileCorruptException` ("settings.json is not valid JSON, so Octo will not write over it."). Display readers get `{}`. Missing or whitespace-only file → `{}`.
- **Fixture:** [`settings.json`](fixtures/state/settings.json)

### 4.2 `downloads-history.json`

The fetched-songs log on the dashboard. Newest first, at most **500** entries.

- **Writer:** `DownloadHistoryService.Record` (`Services/Local/DownloadHistoryService.cs:28`) → `SaveLocked` :73-86. Called from `BaseDownloadService` (`Services/Common/BaseDownloadService.cs:345`).
- **Readers:** `DownloadHistoryService.GetRecent` :40 → `AdminController`. Load is lazy, on first use (`LoadLocked` :48).
- **Schema:** a JSON array of `DownloadHistoryEntry` (`Models/Download/DownloadHistoryEntry.cs`):
  `Artist` string, `Title` string, `Album` string|null (the writer always passes `album ?? ""`), `Path` string (absolute), `Format` string (upper-case extension or `"?"`), `Source` string (`"YouTube"`, `"Lidarr"`, ...), `CoverArtUrl` string|null, `SizeBytes` long, `TranscodedFrom` string|null, `Tagging` TagReport|null, `DownloadedAt` **string** (`ToString("o")`, 7 fraction digits), `RequestedBy` string[]|null.
  `TagReport` (positional record, `Services/Tagging/TagPlan.cs:246`): `Confidence` string (the `TagConfidence` name), `Distance` double|null (rounded to 3 places), `ReleaseTitle`, `ReleaseId`, `Source` (the `TagSource` name), `ReleaseDate` string|null, `Candidates` TagReportCandidate[], `Fields` object (string → `{"Value": string|null, "Source": string|null}`), `Notes` string[], `StageSeconds` object (string → double, 2 places), `Rehearsed` bool, `DetailsPrefetchHit` bool, `IntegratedLufs` double|null, `TruePeakDbfs` double|null.
  `TagReportCandidate`: `Source`, `Title`, `Album`, `Type`|null, `Date`|null, `Distance` double, `BiggestPenalties` string[] (`"year 0.20"`).
- **Atomic:** yes. `lock`.
- **Missing or corrupt:** empty list (warning). Whitespace-only → empty.
- **Fixture:** [`downloads-history.json`](fixtures/state/downloads-history.json)

### 4.3 `lastfm-radio-state.json`

Per-user plays, stations and unplayable tracks for Last.fm radio. **Indented.**

- **Writer:** `LastFmRadioStateStore.SaveLocked` (`Services/LastFm/LastFmRadioStateStore.cs:317-331`), with options `new JsonSerializerOptions { WriteIndented = true }`. Called from `RecordPlay` :40, `MarkHeart` :83, `RejectTrack` :134, `ReplaceStations` :169, `MarkRefreshing` :196, `MarkRefreshFailed` :209 and `Reset` :222.
- **Readers:** `LoadLocked` :240 (lazy). Consumers: `LastFmRadioRecommendationService`, `LastFmRadioRefreshWorker`, `LastFmRadioStreamService`, `LastFmRadioWarmupService`, `GeneratedPlaylistService`, `SubSonicController`, `AdminController`.
- **Schema:** `LastFmRadioStateDocument` (`Models/Radio/LastFmRadioState.cs`): `Version` int (must be **1**), `Users` object keyed by **trimmed lower-cased username** → `LastFmRadioUserState`:
  `Username`, `LastSeenUtc`, `NewPlaysSinceRefresh` int, `Plays` LastFmRadioPlay[], `Stations` LastFmRadioStation[], `UnavailableTracks` LastFmRadioUnavailableTrack[], `LastRefreshAttemptUtc` date|null, `LastRefreshSuccessUtc` date|null, `LastRefreshError` string|null (≤500 chars), `Refreshing` bool.
  `LastFmRadioPlay`: `SongId`, `Artist`, `Title`, `Album`|null, `Genre`|null, `Duration` int|null, `IsLocal`, `Hearted`, `LearnedSignal` (default true), `Source` (default `"scrobble"`), `PlayedAtUtc`.
  `LastFmRadioStation`: `Id` (`"or"` + 20 base62 characters), `Key`, `Name`, `Owner`, `Kind` int (`Starter`=0, `YourMix`=1, `Discovery`=2, `Artist`=3, `Genre`=4, `Pinned`=5), `Personalized`, `DefinitionVersion` int, `CreatedUtc`, `ChangedUtc`, `ValidUntilUtc`, `Seeds` string[], `Tracks` LastFmRadioTrack[].
  `LastFmRadioTrack`: `Artist`, `Title`, `Album`|null, `Genre`|null, `Duration`|null, `Year`|null, `Score` double, `Source`, `ResolvedId`|null, `IsLocal`, `ExternalProvider`|null, `YouTubeId`|null.
  `LastFmRadioUnavailableTrack`: `Key`, `Artist`, `Title`, `FailedAtUtc`, `RetryAfterUtc`.
- **On load and on every write, pruning applies** (`PruneLocked` :277): plays older than `LastFm:HistoryRetentionDays` (clamped 7..365, env `LASTFM_HISTORY_RETENTION_DAYS`, default 90) are dropped, at most 2,000 plays per user; unavailable tracks whose `RetryAfterUtc` has passed are dropped, at most 500; at most 100 users by `LastSeenUtc`. After load, `ResolvedId` of non-local tracks is **recomputed** by registering them in `ExternalIdRegistry` (`RehydrateRoutes` :299), and `ExternalProvider` defaults to the registry's provider name.
- **Atomic:** yes. `lock`. Single writer.
- **Missing or corrupt:** empty document. A `Version` other than 1 → empty document (warning).
- **Fixture:** [`lastfm-radio-state.json`](fixtures/state/lastfm-radio-state.json)

### 4.5 `external-ids.json`

Maps the short opaque ids Octo hands to Subsonic clients onto routing info. Without it, a restart turns every external id a client still holds into a Navidrome "not found".

- **Writer:** `ExternalIdRegistry.Flush` (`ExternalIdRegistry.cs:243-268`). A timer every **10 s**, only when dirty. Also runs from `Dispose`.
- **Readers:** `Load` :217-241 (constructor). Consumers: nearly everything (`SubsonicResponseBuilder`, `SubSonicController`, the outside-music metadata service, `LastFmRadioStateStore`, `HeartAcquisitionCoordinator`, `LibraryActionExecutor`, ...).
- **Schema:** an array in **most-recently-used-first** order (replayed in order, it rebuilds the LRU) of the private record `Persisted(string Id, ExternalRouting Routing)` :205:
  `Id` (22 base62 characters, the first 16 bytes of the SHA-256 of a seed: `k:song|yt:{YouTubeId}|a:{Artist}|t:{Title}|d:{Duration}`, `k:album|a:{Artist}|al:{Album}` or `k:artist|a:{Artist}`), and `Routing` (the external routing record): `Kind` int (`Song`=0, `Album`=1, `Artist`=2), `YouTubeId`, `Artist`, `Title`, `Album` (string|null), `Duration` int|null, `ExternalAlbumId`|null, `ExternalArtistId`|null, `Track`|null, `DiscNumber`|null, `TotalTracks`|null, `Isrc`|null, `ShownDuration` int|null, `ShownDurationSource` int (`LengthSource`: `None`=0, `Video`=1, `LastFm`=2, `Deezer`=3), **`HasYouTube`**, **`HasArtistTitle`** (computed, written).
- **Bounds:** at most 10,000 entries (LRU). Trimmed on load as well.
- **Atomic:** yes. Coalesced. If a write fails, the dirty flag is set again.
- **Missing or corrupt:** cold start (warning). Entries with an empty `Id` or a null `Routing` are skipped.
- **Fixture:** [`external-ids.json`](fixtures/state/external-ids.json). It was produced by the real registry, so the ids are genuine.

### 4.6 `browse-sessions.json`

Dashboard sign-ins. Only **hashes** of tokens are stored.

- **Writer:** `BrowseSessionStore.Save` (`Services/Admin/BrowseSessionStore.cs:120-134`), from `Create` :61, `Revoke` :84 and `Touch` :90 (expiry, or a slide of at least one day).
- **Readers:** the constructor :46-58. Consumers: `AdminController`, `CoverUpgradeController`, `LyricsAdminController`, `UpdateController` (auth gate).
- **Schema:** an array of the private record `Saved(string Hash, string User, DateTime Expires)` :34. `Hash` is the upper-case hex SHA-256 of the token (`Convert.ToHexString`). Sliding TTL of 90 days.
- **On load:** entries with `Expires <= now` are dropped.
- **Atomic:** yes. `_saveLock`.
- **Missing or corrupt:** nobody is signed in (warning).
- **Fixture:** [`browse-sessions.json`](fixtures/state/browse-sessions.json)

### 4.8 `genre-backfill.json`

Progress of the genre backfill run (resumable).

- **Writer:** `GenreBackfillStore.Flush` (`Services/Metadata/GenreBackfillState.cs:159-178`). Every 10 s when dirty, immediately on `Replace` :125, and on `Dispose`.
- **Readers:** `Load` :132-157. Consumers: `GenreBackfillWorker`, `AdminController` (through the worker).
- **Schema:** `GenreBackfillRun` :41: `RunId`, `Status` int (`Idle`=0, `Running`=1, `Completed`=2, `Cancelled`=3, `Interrupted`=4, `Failed`=5), `Scope` int (`OctoDownloads`=0, `WholeLibrary`=1), `DryRun` bool, `StartedUtc`|null, `FinishedUtc`|null, `Total`, `Processed`, `Changed`, `Cleared`, `Skipped`, `Failed`, `Cursor` (ints), `LastPath`|null, `Reason`|null, `Errors` string[] (last 20), `Preview` GenreBackfillChange[] (first 500: `Path`, `Before` string[], `After` string[], `Action`, `Rule`|null), `SettingsHash`|null (upper-case hex SHA-256), `Queue` string[] (absolute paths, the walk order), **`CanResume`** (computed, written).
- **On load:** `Status == Running` → `Interrupted` with `Reason = "Octo restarted while this run was in progress."`. It is **never auto-resumed**.
- **Atomic:** yes. Coalesced.
- **Missing or corrupt:** Idle run (warning).
- **Fixture:** [`genre-backfill.json`](fixtures/state/genre-backfill.json)

### 4.9 `genre-backfill-journal.jsonl`

The undo log for genre writes. JSON Lines.

- **Writer:** `GenreBackfillJournal.Append` (`Services/Metadata/GenreBackfillJournal.cs:45-62`): `File.AppendAllText(path, Serialize(entry) + Environment.NewLine)` (`\n`). `Rewrite` :110-134 writes the remaining entries oldest first to `.tmp` with `File.WriteAllLines` (each line + `\n`) and renames it (after a partial undo, `GenreBackfillWorker.cs:330`). `Clear` :136 deletes the file.
- **Readers:** `ReadAll` :68-103 returns the lines **newest first**. Consumers: `GenreBackfillWorker` (undo) and `AdminController` (`Exists`).
- **Schema (one object per line):** `GenreJournalEntry` :10 with short names: `p` path, `b` genres before (string[]), `a` genres after (string[]), `t` timestamp (DateTime UTC), `r` run id.
- **Torn or bad line:** skipped. A `JsonException` is counted and logged once. Blank lines are skipped. Entries with an empty `p` are dropped.
- **Fixture:** [`genre-backfill-journal.jsonl`](fixtures/state/genre-backfill-journal.jsonl)

### 4.10 `cover-upgrade.json`

The cover upgrade run (scan / preview / apply / undo).

- **Writer:** `CoverUpgradeStore.Flush` (`Services/CoverArt/CoverUpgrade.cs:215-231`). Every 10 s, on `Replace` :187, on `Dispose`.
- **Readers:** `Load` :194-213. Consumers: `CoverUpgradeWorker`, `CoverUpgradeController`.
- **Schema:** `CoverUpgradeRun` :57: `RunId`, `Status` int (`Idle`..`Failed`, the same 0-5 as genre), `Scope` int (`OctoDownloads`=0, `WholeLibrary`=1), `Mode` int (`Scan`=0, `Preview`=1, `Apply`=2), `FolderCovers` bool, `SmallerThan` int (default 1000), `Selected` string[]|null, `FullSize`, `Undo`, `StartedUtc`|null, `FinishedUtc`|null, `Total`, `Processed`, `SongsTotal`, `SongsRead`, `AlbumsTotal`, `AlbumsDone`, `Soft`, `Upgraded`, `Kept`, `Files`, `Failed`, `Cursor`, `LastFolder`|null, `Reason`|null, `Errors` string[] (last 20), `Preview` CoverUpgradeChange[] (first 5,000), `Queue` CoverUpgradeItem[]. `DryRun` and `CanResume` are `[JsonIgnore]`, so they are **not written**.
  `CoverUpgradeChange` :46: `Id`, `Folder`, `Artist`, `Album`|null, `FromSide`, `ToSide`, `Source`|null, `Files`, `FolderCover`, `Result` (`soft`/`found`/`upgraded`/`none`), `FirstFile`|null, `Paths` string[]|null, `NavidromeAlbumId`|null, `Barcode`|null, `LooksSame` bool|null.
  `CoverUpgradeItem` :55: `Folder`, `Files` string[]|null, `NavidromeAlbumId`|null.
  Rust only (group P port fix): `Batch` `{"Cursor","SongsRead","Counted":[album id...],"Finished":[album id...]}`, written last and only while a batch is stopped part way: a resume takes `SongsRead` off before the batch runs again and skips the albums it counted or finished. The C# reader ignores it; a run without it reads as before.
- **On load:** `Running` → `Interrupted`, `Reason = "Octo restarted while this run was going."`.
- **Atomic:** yes. Coalesced.
- **Missing or corrupt:** Idle run (warning).
- **Fixture:** [`cover-upgrade.json`](fixtures/state/cover-upgrade.json)

### 4.11 `cover-upgrade-journal.jsonl`

The undo log for cover writes. It works together with the `cover-backups/` folder (§5).

- **Writer:** `CoverUpgradeJournal.Record` (`Services/CoverArt/CoverUpgrade.cs:272-298`). It first writes the old picture to `cover-backups/<hash>` (atomic via `.tmp`, skipped when the file already exists), then appends `Serialize(entry) + Environment.NewLine`. `Rewrite` :327-343 writes the remaining lines to `.tmp` and renames, or deletes the file when nothing is left, and **deletes backups no remaining entry references** (`CoverUpgrade.cs:1137`).
- **Readers:** `ReadAll` :301-317 (newest first), `Backup(hash)` :319, `HasEntries` :268.
- **Schema (one object per line):** `Entry` :247 with short names: `p` path, `k` kind (`"embedded"` or `"file"`), `h` hash|null (the first 32 hex characters, lower case, of the SHA-256 of the old picture; null when there was none), `r` run id.
- **Torn or bad line:** silently skipped.
- **Fixture:** [`cover-upgrade-journal.jsonl`](fixtures/state/cover-upgrade-journal.jsonl)

### 4.12 `library-actions.json`

The write-ahead journal for library actions (delete, wrong song, better quality...).

- **Writer:** `LibraryActionJournal.Flush` / `FlushLocked` (`Services/Library/LibraryActionJournal.cs:293-322`). Coalesced every 10 s, but `LibraryActionExecutor` calls `Flush()` **synchronously before touching the file** (`LibraryActionExecutor.cs:235-241`). If that flush fails, the action is aborted.
- **Readers:** `Load` :260-285. `Reconcile` :181 runs at startup (`LibraryActionPlaylistWorker.cs:64`, `LibraryActionExecutor.cs:131`) and resolves `Pending` entries against the filesystem. Consumers: `LibraryActionExecutor`, `LibraryActionPlaylistWorker`, `BaseDownloadService`, `LidarrHeartAcquisitionService`, `AdminController`.
- **Schema:** an array, oldest first, of `LibraryActionEntry` (positional record :28): `Key` (`"{Action}|{NavidromeId}|{size}:{lastWriteTicks}"`), `Action` int (`Delete`=0, `WrongSong`=1, `WrongVersion`=2, `BetterQuality`=3, `Keep`=4), `NavidromeId`, `Username`, `Title`, `Artist`, `Album`, `SourcePath`|null, `QuarantinePath`|null, `Resolution` int|null (`PathSource`: `NativeApi`=0, `SubsonicGetSong`=1, `LocalMappings`=2, `None`=3), `State` int (`Pending`=0, `Applied`=1, `Failed`=2, `Unresolved`=3, `Skipped`=4, `Rehearsed`=5), `Detail`|null, `DryRun`, `AtUtc`, then the body properties `HistoryKept` bool|null and `RevealedPath`|null.
- **Bounds:** at most 2,000 entries (oldest dropped).
- **Missing or corrupt:** empty (warning). Entries with an empty `Key` are skipped.
- **Fixture:** [`library-actions.json`](fixtures/state/library-actions.json)

### 4.13 `notice-queue.json`

The questions Octo asks through playlists (Review, Duplicates). This is the **only file that writes enums as strings**.

- **Writer:** `NoticeQueue.Flush` (`Services/Library/NoticeQueue.cs:434-457`) with `Json = new() { Converters = { new JsonStringEnumConverter() } }` (:72). A timer every **5 s**, plus explicit `Flush()` from `NoticePlaylistWorker.cs:158` and `DuplicateScanWorker.cs:137`.
- **Readers:** `Load` :412-432 (same options). Consumers: `NoticePlaylistWorker`, `DuplicateScanWorker`, `LibraryReviewSweepWorker`, `LibraryActionExecutor`, `BaseDownloadService`, `SubSonicController`, `AdminController`.
- **Schema:** an array of `NoticeEntry` (record with init properties :15): `Key` (for example `"review|{user lower}|{path}"`), `Kind` (`"Review"`/`"Duplicates"`), `Username`, `LocalPath`, `Artist`, `Title`, `Album`|null, `NavidromeId`|null, `GroupKey`|null, `Order` int, `State` (`"Waiting"`, `"Queued"`, `"Kept"`, `"Acted"`, `"Dismissed"`, `"Expired"`), `Reason`, `Cause` (`InconclusiveReason`: `"None"`, `"Disabled"`, `"NotFingerprinted"`, `"LookupFailed"`, `"NoEntry"`, `"BelowThreshold"`, `"SourceDisagreed"`, `"SoundsLikeAnother"`, `"LengthOff"`), `Origin` (`"Download"`/`"LibrarySweep"`), `Fingerprint`|null, `DurationSeconds` int, `CandidateRecordingId`|null, `FileFormat`|null, `Submitted` bool, `LookupAttempts` int, `NextLookupUtc` (**may be `"0001-01-01T00:00:00"`, with no `Z`**, when never set), `CreatedUtc`, `QueuedUtc`|null, `ResolvedUtc`|null. `IsOpen` is `[JsonIgnore]`.
- **Bounds:** 5,000. Resolved entries are dropped oldest first. Open ones are never dropped.
- **Corrupt:** the file is **renamed** to `notice-queue.json.corrupt-<DateTime.UtcNow.Ticks>` and the queue starts empty.
- **Fixture:** [`notice-queue.json`](fixtures/state/notice-queue.json)

### 4.14 `generated-playlists.json`

Genre and decade mix counts per listener.

- **Writer:** `GeneratedPlaylistService.SaveLocked` (`Services/Library/GeneratedPlaylistService.cs:541-556`), with options `new() { WriteIndented = false }` (the same output as the defaults). Called after each count refresh :197-201.
- **Readers:** `Load` :523-539 (constructor). Consumer: `SubSonicController` (playlists).
- **Schema:** the private `StateDocument` :77: `{"Users": { "<trimmed lower-cased user>": UserMixes }}`. `UserMixes` :69: `Active` string[] (`"genre:Rock"`, `"decade:1990"`), `Counts` object (string → int), `CountsUtc`, `Kinds` string (`"genre,decade"`, `"genre,"`, `",decade"` or `","`).
- **Corrupt:** renamed to `.corrupt-<UtcTicks>`, then rebuilt on the next list.
- **Fixture:** [`generated-playlists.json`](fixtures/state/generated-playlists.json)

### 4.15 `quality-upgrade.json`

The weekly quality-upgrade worker's memory.

- **Writer:** `QualityUpgradeStore.Update` (`Services/Library/QualityUpgradeWorker.cs:54-66`), which serializes inside the lock and writes outside it.
- **Readers:** the constructor :36-46, `Snapshot` :48. Consumers: `QualityUpgradeWorker`, `UpgradeQueue`/`UpgradeWorker` (shared attempts).
- **Schema:** `QualityUpgradeState` :15: `LastRunUtc`|null, `LastOutcome`|null (a `LibraryActionState` name or `"Nothing"`), `Attempts` object keyed by `KeyOf(song)` = `"{library-relative path}|{size}"` (:143) → `QualityUpgradeAttempt(AtUtc, Outcome, Detail)`.
- **Missing or corrupt:** empty (warning).
- **Fixture:** [`quality-upgrade.json`](fixtures/state/quality-upgrade.json)

### 4.16 `upgrades.json`

Songs asked to be found in higher quality.

- **Writer:** `UpgradeQueue.Save` (`Services/Library/UpgradeQueue.cs:248-258`), on every change (`Add`, `Cancel`, `ClearFinished`, `TakeNext`, `Requeue`, `Update`).
- **Readers:** the constructor :97-111. Consumers: `UpgradeWorker`, `SubSonicController`, `AdminController`, `HeartOwnership`, `BaseDownloadService`, `LidarrHeartAcquisitionService`.
- **Schema:** an array of `UpgradeJob` :26: `NavidromeId`, `Title`|null, `Artist`|null, `Album`|null, `Suffix`|null, `AttemptKey`|null, `RequestedBy`, `Origin` (default `"app"`), `State` (string constants: `queued`, `waiting`, `working`, `upgraded`, `notFound`, `rehearsed`, `skipped`, `failed`), `Detail`|null, `AcquisitionKey`|null, `QueuedUtc`, `UpdatedUtc`, `StartedUtc`|null, `Result` UpgradeResult|null. `UpgradeResult` :62: `Before`|null, `BeforeBytes` long|null, `After`|null, `AfterBytes`|null, `NewFile`|null, `KeptAt`|null, `Checks` string[], `Seconds` double|null.
- **On load:** `working` → `queued`. Finished jobs older than 7 days are pruned on `Snapshot`. At most 5,000 open jobs.
- **Missing or corrupt:** empty (warning).
- **Fixture:** [`upgrades.json`](fixtures/state/upgrades.json)

### 4.17 `update/release.json`

The last GitHub release check.

- **Path:** `/app/config/update/release.json`. This is the same folder as the updater handshake (§5). `octo-updater.path` watches only `update/request`, so writing this file does not start the helper.
- **Writer:** `ReleaseCheck.Save` (`Services/Updates/ReleaseCheck.cs:265-278`).
- **Readers:** `Load` :251-263. Consumer: `UpdateController`.
- **Schema:** `ReleaseCheckState` :14: `Repo`|null, `CheckedUtc`|null, `AttemptedUtc`|null, `Error`|null, `ETag`|null (GitHub's ETag, so quotes are escaped as `"`), `Releases` ReleaseNote[] (at most 10; `Tag`, `Name`, `Notes` (≤20,000 chars), `Url`, `PublishedUtc`|null).
- **Missing or corrupt:** fresh state (warning).
- **Fixture:** [`update/release.json`](fixtures/state/update/release.json)

### 4.18 `review-sweep.json`

The library review sweep's cursor and checked-file stamps.

- **Writer:** `ReviewSweepStore.Flush` (`Services/Library/LibraryReviewSweepWorker.cs:108-136`). `Update` flushes right away if 5 s have passed since the last write; otherwise a 5 s timer flushes. There are also explicit `Flush()` calls (:207, :214, :402, :414).
- **Readers:** the constructor :72-88. Consumer: `LibraryReviewSweepWorker`.
- **Schema:** `ReviewSweepState` :35: `Paused`, `Cursor` (library-relative path, `""` at the start of a pass), `Checked` object (library-relative path → `"{length}:{LastWriteTimeUtc.Ticks}"`), `Pass` int (default 1), `Total`, `Found`, `Fine`, `Undecodable`, `LastCheckedUtc`|null, `PassFinishedUtc`|null, `NextPassUtc`.
- **Corrupt:** renamed to `.corrupt-<UtcTicks>`, then the sweep starts over.
- **Fixture:** [`review-sweep.json`](fixtures/state/review-sweep.json)

### 4.19 `itunes-masters.json`

A cache of the iTunes master-artwork URLs that were matched.

- **Writer:** `ITunesCoverArtLookup.FlushCache` (`Services/CoverArt/ITunesCoverArtLookup.cs:339-353`). A timer every **15 s**, plus `Dispose`.
- **Readers:** `LoadCache` :324-337. Consumers: `AlbumCoverFinder`, `DownloadCoverResolver`, `ReleaseDistance` (through the lookup).
- **Schema:** an array of the private `CachedMaster(string Key, string? Url, DateTime At)` :322. `Key` = `SongIdentity.MatchKey(artist, release) + "|album"` or `"|single"`. A `null` `Url` records a miss. The order is `ConcurrentDictionary` enumeration order, which is **unspecified**.
- **On load:** hits older than 30 days and misses older than 1 day are dropped. In memory, the whole cache is cleared once it passes 20,000 entries.
- **Missing or corrupt:** empty (debug log only).
- **Fixture:** [`itunes-masters.json`](fixtures/state/itunes-masters.json). The keys were made with the real `MatchKey`.

### 4.20 `lyrics-choices.json`

Lyrics pinned or hidden by a person.

- **Writer:** `LyricsChoiceStore.Save` (`Services/Lyrics/LyricsChoices.cs:125-139`), from `Set` :90, `Remove` :99 and `RemoveByName` :75.
- **Readers:** `Load` :109-123 (constructor). Consumer: `LyricsChoiceService`.
- **Schema:** an array of `LyricsPin` (positional record :13): `SongId`, `Choice` (a candidate id such as `"lrclib:123"`, `"none"` = hidden, `"auto"`), `Source`|null, `Synced`|null, `Plain`|null, `Artist`|null, `Title`|null, `SetBy`|null, `SetUtc`, then the **computed** `IsHidden` bool and `Lyrics` (null when hidden, otherwise a full `LyricsResult` object: `Source`, `Synced`, `Plain`, `Instrumental`, `HasSynced`, `HasPlain`, `HasWordTiming`, `Timing` int (`None`=0, `Plain`=1, `Line`=2, `Word`=3), `SongTiming` int|null, `IsSongsOwn`, `CandidateId`, `Doubt`).
- **On load:** duplicate `SongId`s → the last one wins. Empty `SongId`s are dropped.
- **Missing or corrupt:** no pins (warning).
- **Fixture:** [`lyrics-choices.json`](fixtures/state/lyrics-choices.json)

### 4.21 `lyrics-library.json`

The "find lyrics for the library" run.

- **Writer:** `LyricsLibraryStore.Flush` (`Services/Lyrics/LyricsLibraryJob.cs:183-201`). Every 10 s, on `Replace` :155, on `Dispose`.
- **Readers:** `Load` :162-181. Consumers: `LyricsLibraryWorker`, `LyricsLibrarySteps`, `LyricsAdminController`.
- **Schema:** `LyricsLibraryRun` :61: `RunId`, `Status` int (`Idle`..`Failed`, 0-5), `Scope` **string** (`"OctoDownloads"`/`"WholeLibrary"`), `Upgrade` bool, `Mode` int (`Walk`=0, `Scan`=1, `Preview`=2, `Save`=3, `Undo`=4), `Rows` LyricsLibraryRow[], `Picked` string[]|null, `WordAlready`, `StartedUtc`|null, `FinishedUtc`|null, `Total`, `Processed`, `Written`, `WordTimed`, `Upgraded`, `AlreadyHad`, `NotFound`, `Instrumental`, `Busy`, `Skipped`, `Failed`, `Cursor`, `LastPath`|null, `Reason`|null, `Errors` string[] (last 20), `Queue` string[], `Review` LyricsReviewEntry[] (last 500), **`CanResume`** (computed, written).
  `LyricsLibraryRow` :21: `Id` (the first 16 hex characters, lower case, of the SHA-1 of the path), `Path`, `Artist`, `Title`, `Album`|null, `Has` (`none`/`plain`/`line`), `Result` (`weak`, `found`, `none`, `busy`, `saved`, `kept`, `blocked`, `failed`), `Source`|null, `Kind`|null, `CandidateId`|null, `Doubt`|null, `Preview` string[], `FoundSynced`|null, `FoundPlain`|null, **`Found`** (computed `LyricsResult`|null, written).
  `LyricsReviewEntry` :57: `Path`, `Artist`, `Title`, `Album`|null, `DurationSeconds`|null, `Source`, `Kind`, `CandidateId`|null, `Reason`, `AtUtc`.
- **On load:** `Running` → `Interrupted`, `Reason = "Octo restarted while this run was in progress."`.
- **Missing or corrupt:** Idle run (warning).
- **Fixture:** [`lyrics-library.json`](fixtures/state/lyrics-library.json)

### 4.22 `lyrics-undo.jsonl`

What a lyrics Save overwrote, so Undo can restore it. JSON Lines, **append only**, never rewritten (only `Clear()` deletes it).

- **Writer:** `LyricsUndoJournal.Record` (`Services/Lyrics/LyricsLibraryJob.cs:238-247`): `File.AppendAllText(path, Serialize(entry) + "\n")`. It is called through `LyricsSidecarWriter.SaveAsync` (`Services/Lyrics/LyricsSidecarWriter.cs:175-191`) **before** each write.
- **Readers:** `ReadAll` :249-261 (oldest first), `HasEntries` :230. Consumer: `LyricsSidecarWriter.Restore` :210.
- **Schema (one object per line):** `Entry(string Path, string Kind, string? Before, string RunId, DateTime AtUtc)` :222, with **full property names**, unlike the other two journals. `Kind` is `"beside"` (a `.lrc`/`.txt` file; `Path` is the sidecar, and `Before` is its previous text or null when it did not exist) or `"inside"` (the song's tags; `Path` is the audio file, and `Before` is the previous lyrics tag).
- **Bad line:** silently skipped. Empty lines are skipped.
- **Fixture:** [`lyrics-undo.jsonl`](fixtures/state/lyrics-undo.jsonl)

### 4.23 `<download dir>/.mappings.json`

Map from external id to the local file Octo downloaded. It lives **in the music folder**, not in `/app/config`.

- **Path:** `Path.Combine(configuration["Library:DownloadPath"] ?? "<cwd>/downloads", ".mappings.json")` (`Services/Local/LocalLibraryService.cs:49-50`). Note that this is the *configured* path, not the auto-detected one.
- **Writer:** `LocalLibraryService.SaveMappingsAsync` (:198-206) with `new JsonSerializerOptions { WriteIndented = true }`. Called from `RegisterDownloadedSongAsync` :76 and `ForgetMappingAsync` :216.
- **Readers:** `LoadMappingsAsync` :167-196 (lazy, cached forever after the first load). Consumers through `ILocalLibraryService`: downloads, the genre backfill and cover upgrade "OctoDownloads" scope, the lyrics job, the review sweep, `NavidromeSongPathResolver` (`PathSource.LocalMappings`), `SubSonicController`.
- **Schema:** an object keyed by `"{ExternalProvider}:{ExternalId}"` → `LocalSongMapping` (:354): `ExternalProvider`, `ExternalId`, `LocalPath`, `LocalSubsonicId`|null (never set by current code), `Title`, `Artist`, `Album`, `DownloadedAt`, `SourcePeer`|null, `SourceFile`|null, `MusicBrainzRecordingId`|null, `TranscodedFrom`|null.
- **Atomic: NO.** `File.WriteAllTextAsync` writes straight over the file, so a crash mid-write truncates it. There is a `SemaphoreSlim(1,1)`.
- **Corrupt: NOT handled.** The `JsonException` from `LoadMappingsAsync` propagates to the caller, and because `_mappings` stays null it is thrown again on every call. **Rust decision needed:** reproduce this (strict parity) or recover (recommended: atomic write, and a corrupt file moved aside like `notice-queue.json`). Record the choice in `known-diffs.md`.
- **Fixture:** [`music/.mappings.json`](fixtures/state/music/.mappings.json)

### 4.24 Quarantine manifest `<quarantined file>.octo-action.json`

- **Path:** `<musicRoot>/<LibraryActions:QuarantineDirectory>/<yyyy-MM-dd (UTC)>/<path relative to musicRoot>` + `.octo-action.json`. The default quarantine folder is `.octo-trash` (`LIBRARY_ACTIONS_TRASH_DIR`). On a name clash the file gets ` (2)`...` (999)`, then ` (<guid N>)`.
- **Writer:** `LibraryActionQuarantine.WriteManifest` (`Services/Library/LibraryActionQuarantine.cs:162-174`) after `Move` :51-93 (a rename, or copy + length check + delete across devices).
- **Readers:** `ReadManifest` :176-185, used by `Restore` :96-122 (moves the file back to `OriginalPath` and deletes the manifest). `Sweep` :128-160 deletes whole dated folders older than `LibraryActions:QuarantineRetentionDays` (`LIBRARY_ACTIONS_TRASH_DAYS`, default 30; 0 = keep forever).
- **Schema:** `QuarantineManifest(string OriginalPath, string NavidromeId, string Action, string Username, DateTime AtUtc)` :10. `Action` is the **enum name as a string** (`"Delete"`, `"WrongSong"`, ...), written with `action.ToString()`.
- **Atomic: no.** A failed write is only a warning, because the journal still has the path.
- **Missing or corrupt:** `null`, so `Restore` refuses ("no manifest, so the original path is unknown").
- **Fixture:** [`music/.octo-trash/2026-10-03/Sigur Rós/Ágætis byrjun/02 - Svefn-g-englar.flac.octo-action.json`](fixtures/state/music/.octo-trash/2026-10-03/)

### 4.25 Radio cache profile `<cache>/radio/<key>.mp3.json`

- **Path:** `/tmp/octo-cache/radio/<key>.mp3.json`. `key` = the lower-case hex SHA-256 of `"{USERNAME upper}\n{stationId}\n{trackIdentity}\n{bitrateKbps}"` (`Services/LastFm/LastFmRadioTrackCache.cs:27`).
- **Writer:** `LastFmRadioTrackCache.SaveProfile` :96-106. Atomic via `<sidecar>.<guid N>.tmp`. Errors are swallowed.
- **Readers:** `GetProfile` :85-94. Absent or bad → `null` ("unknown").
- **Schema:** `RadioAudioProfile` (`Services/LastFm/LastFmRadioAudioTranscoder.cs:28`): `IntegratedLufs`, `LoudnessRangeLu`, `TruePeakDbfs`, `GainDb`, `SpectralCentroidHz`, `SpectralFlatness`, `SpectralRolloffHz` (doubles), `Genre`|null, `Tags` string[]|null.
- **Fixture:** [`cache/radio/<key>.mp3.json`](fixtures/state/cache/radio/)

---

### 4.26 `imports.json` (2026.10.05)

- **Writer:** `ImportStore` (`Services/Imports/ImportStore.cs`), Rust `services::imports::import_store` (the file; the store's methods are group H's).
- **Format:** `{"Lists":[ImportList...]}`, no options. `ImportList`: `Id, Owner, Name, Source` (`spotifyLiked`, `spotifyPlaylist`, `link`, `file`), `SourceRef?, By?, ImageUrl?, SnapshotId?, Total, Tracks[ImportTrack], Partial?, ReadUtc, MatchedUtc?, KeepPlaylist, PlaylistId?, PlaylistNote?, GetMissing, Gone`. `ImportTrack`: `Key, Title, Artist, Album?, Seconds?, Isrc?, LibraryId?`.
- **Fixture:** [`imports.json`](fixtures/state/imports.json)

### 4.27 `imports-trickle.json` (2026.10.05)

- **Writer:** `TrickleQueue` (`Services/Imports/TrickleQueue.cs`), Rust `services::imports::trickle_queue`.
- **Format:** `{"Jobs":[TrickleJob...],"Paused":[user...],"LastStartUtc":date?}`. `TrickleJob`: `Owner, Key, Title, Artist, Album?, Seconds?, Isrc?, State` (`queued`, `downloading`, `done`, `notFound`, `skipped`), `Detail?, AcquisitionKey?, LibraryId?, Attempts, QueuedUtc, UpdatedUtc, StartedUtc?`.
- **Load:** a job `downloading` is `queued` again with the detail "Octo restarted while this was running; it goes again." Done jobs older than 7 days are pruned; not-found ones are retried after 14 days (the queue's own work).
- **Fixture:** [`imports-trickle.json`](fixtures/state/imports-trickle.json)

### 4.28 `spotify-accounts.json` (2026.10.05)

- **Writer:** `SpotifyAccountStore` (`Services/Imports/SpotifyAccountStore.cs`), Rust `services::imports::spotify_account_store`. Its own file rather than settings.json: the access token changes every hour, and settings.json is watched and shown on the dashboard.
- **Format:** `{"<user>":{"AccessToken","RefreshToken","ExpiresUtc","ConnectedUtc","SpotifyId"?,"DisplayName"?,"Problem"?}}`; users compared ignoring case. The temporary file is made 0600 before the rename (not on Windows).
- **Fixture:** [`spotify-accounts.json`](fixtures/state/spotify-accounts.json) (synthetic tokens)

### 4.29 `radio-outcomes.json` (2026.10.05)

- **Writer:** `RadioOutcomeStore` (`Services/Radio/RadioOutcomeStore.cs`), Rust `services::radio::radio_outcome_store`. Its folder is `Octo:StateDirectory` when set (tests), else beside settings.json.
- **Format:** `{"Listeners":{"<listener>":{"<provider>":{"Kept":double,"Skipped":double,"UpdatedUtc":date}}}}`. Counts are faded by age; doubles in STJ's shortest round-trip form.
- **Load:** a blank listener, a listener whose value is null or has no non-null counts, and a null count are dropped.
- **Fixture:** [`radio-outcomes.json`](fixtures/state/radio-outcomes.json)

### 4.30 `sonic-features.json` (2026.10.05)

- **Writer:** `SonicStore` (`Services/Sonic/SonicStore.cs`), Rust `services::sonic::sonic_store`. Same folder rule as 4.29.
- **Format:** `{"Paused","Pass","PassFinishedUtc"?,"NextPassUtc"?,"Songs":{"<navidrome id>":{"Stamp","Title","Artist","Album"?,"Duration"?,"Version","F":[float...]}},"Failed":{"<id>":{"Stamp","Error","AtUtc"}}}`. `F` holds `float` (single) values, written by STJ as the shortest text that reads back to the same single (`1E-05`, `3.4028235E+38`); `octo_core::json::format_single` reproduces it.
- **Load:** a song whose `F` or `Stamp` is null, and a failure whose `Stamp` is null, are dropped.
- **Fixture:** [`sonic-features.json`](fixtures/state/sonic-features.json)

### 4.31 `tag-edits.json` (2026.10.05)

- **Writer:** `TagEditJournal` (`Services/Library/LibraryTagEdits.cs`), Rust `services::library::library_tag_edits`. Rewritten whole on every change, at most 20,000 entries (the oldest go).
- **Format:** `[TagEditEntry...]`: `Id, NavidromeId, Path, Kind` (`retag`, `joinAlbum`, `cover`, `advisory`), `Username, Before{tag: value?}, After{tag: value?}, SizeAfter, WriteTicksAfter, AtUtc, AlbumBefore?` (a `KeptIdentity`: `Title, Album?, AlbumArtist[], AlbumArtists[], AlbumVersion?, ReleaseDate?, AlbumId?, ReleaseTrackId?, Track, TrackCount, Disc, DiscCount, Compilation`), `Undone, RunId?`.
- **Fixture:** [`tag-edits.json`](fixtures/state/tag-edits.json)

### 4.32 `explicit-backfill.json` (2026.10.05)

- **Writer:** `ExplicitBackfillStore` (`Services/Library/ExplicitBackfill.cs`), Rust `services::library::explicit_backfill_store`. Coalesced every 10 s and on `Replace`, like `genre-backfill.json`.
- **Format:** `ExplicitBackfillRun`: `RunId, Status` (number: Idle 0, Running 1, Completed 2, Cancelled 3, Interrupted 4, Failed 5), `Mode` (Preview 0, Apply 1, Undo 2), `StartedUtc?, FinishedUtc?, Total, Processed, Explicit, Clean, NotExplicit, Unsure, AlreadyMarked, Failed, StepTotal, StepDone, Written, LeftAlone, LastSong?, Reason?, Errors[], Rows[ExplicitBackfillRow], Queue[{Path, NavidromeId?}], Cursor, AppliedRunId?`, then the computed **`CanResume`** and **`CanApply`**, which STJ writes too. Errors keep the newest 20, rows the first 50,000.
- **Load:** a `Running` run becomes `Interrupted` with the reason "Octo restarted while this run was going."
- **Fixture:** [`explicit-backfill.json`](fixtures/state/explicit-backfill.json)

### New fields in the existing files (2026.10.05)

| File | What changed | Rust |
|---|---|---|
| `downloads-history.json` (4.2) | `Key?` (the live row's `provider:id`) and `Log?` (`[{At, Kind, Text, Detail?, Candidates?[{Source, File?, Folder?, Format?, BitRate?, BitDepth?, SampleRate?, Size?, Length?, Rank?, Note?, Title?, Album?, Quality?}]}]`, `Quality` computed). **The file is now written with `DefaultIgnoreCondition = WhenWritingNull`**: every null member is left out, at every depth. | `models::download::acquisition_event`; `json::to_string_without_nulls` |
| `lastfm-radio-state.json` (4.3) | `RadioBans[{Key, SongId, Artist, Title, BannedAtUtc}]` after `UnavailableTracks`. | `LastFmRadioBan` |
| `external-ids.json` (4.5) | `ExplicitContent?` (1 explicit, 3 clean, 0 neither) and `Version?` after `ShownDurationSource`. A routing whose `Version` is `clean` (not an artist) mints its id from the seed plus `|v:clean`, so a clean edit has an id of its own; every other id is unchanged. A re-registration keeps the previous `ExplicitContent` when the new one has none. | the external routing record, `ExternalIdRegistry::make_short_id` |
| `library-actions.json` (4.12) | `Copy`, `DownloadAllowed` and the computed `BlocksDownloads` after `RevealedPath`; `State` 6 = `Restored` (a removed song put back). Only `BlocksDownloads` entries keep a song from downloading again. | `LibraryActionEntry::blocks_downloads` |
| `generated-playlists.json` (4.14) | `ForYou{kind: [song JSON...]}` and `ForYouCovers[url...]` per user. | `UserMixes` |
| `upgrades.json` (4.16) | `Pick?` (a `PickedCopy`: `Source, Peer?, File?, Size, Format?, BitRate?, BitDepth?, SampleRate?, Length?, ReleaseGuid?, IndexerId?, ReleaseTitle?`; its derived fields are `[JsonIgnore]`) after `Result`. | `services::common::download_picks` |
| Quarantine manifest (4.24) | `Sidecars?[{OriginalPath, QuarantinePath}]` (the song's lyrics files that went with it) and `Mappings?` (its `.mappings.json` entries, put back on restore). | `QuarantineManifest`, `QuarantinedSidecar` |

## 5. Non-JSON artifacts

- **`admin-recovery-code`** (2026.10.05): the dashboard's one-time recovery code, `AdminRecoveryCode` (`Services/Admin/AdminRecoveryCode.cs`), Rust `services::admin::admin_recovery_code`. One line: four groups of five characters from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789` joined with `-`, then `\n`. Written through `admin-recovery-code.tmp`, made 0600 (best effort: a failure is a warning), then renamed. Read trimmed; a missing or blank file gets a new code. Typed codes are compared ignoring spaces, dashes and case, in fixed time; a used code is replaced at once. Fixture: [`admin-recovery-code`](fixtures/state/admin-recovery-code) (a fixed code).

| Artifact | Path | Writer | Format and rules | Readers |
|---|---|---|---|---|
| Update request | `/app/config/update/request` | `UpdateHost.Request` → `Write` (`Services/Updates/UpdateHost.cs:125-143`, `:175-181`), atomic via `request.tmp` | `key=value\n` lines, in this order: `id` (Guid `D`), `tag`, `by` (matches `^[A-Za-z0-9._@-]{1,64}$`, otherwise `dashboard`), `at` (`yyyy-MM-ddTHH:mm:ssZ`). Deleted when the helper has not picked it up after 90 s. [Fixture](fixtures/state/update/request) | the host helper `scripts/updater/octo-updater.sh` (started by `octo-updater.path` on `PathExists=.../request`), which moves it to `request.taken` |
| Update status / helper / log | `/app/config/update/{status,helper,log}` | **The host helper**, not Octo (`octo-updater.sh` `write_file`, `write_status`, `log`). status/helper are atomic via `.tmp` + `mv -f`, and log is appended | status: `id, tag, from, state, step, error, started, finished`. helper: `version, mode (build or image), dir, installed`. log: `<UTC time> <message>` lines. [Fixtures](fixtures/state/update/) | `UpdateHost.Status` :75, `Helper` :66, `LogTail` :85 (last 40 lines). The parser splits on the first `=` and trims. Times are parsed as UTC |
| Lyrics sidecar `.lrc` | `<audio dir>/<audio stem>.lrc` | `LyricsSidecarWriter.WriteFileAsync` (`Services/Lyrics/LyricsSidecarWriter.cs:246-253`). **Not atomic** (`File.WriteAllTextAsync`) | UTF-8 **without a BOM**. The first line is `[re:Octo]` (the ownership mark, `OctoMark` :42), then the synced text with CRLF → LF, trimmed, plus a final `\n`. Octo replaces only `.lrc` files whose first line (trimmed, BOM stripped) is exactly `[re:Octo]` (`IsOctos` :301). Written only when `Metadata:SaveLyricsTo` (`LYRICS_SAVE_TO`: `beside`, `inside`, `both`) allows beside. [Fixture](fixtures/state/music/) | Navidrome; `SongLyrics.Find` (`Services/Lyrics/SongLyrics.cs:29`); undo `Restore` |
| Lyrics sidecar `.txt` | `<audio stem>.txt` | same method, plain lyrics | Plain text, LF, trimmed, final `\n`, **no mark**. Never overwritten: written only when neither `.lrc` nor `.txt` exists (`SongLyrics.MayWriteBeside`). [Fixture](fixtures/state/music/) | Navidrome, `SongLyrics` |
| Lyrics inside tags | audio file | `LyricsSidecarWriter.WriteInside` (`Services/Lyrics/LyricsSidecarWriter.cs:257-272`, TagLib) | The lyrics tag is set to `[re:Octo]\n` + text (CRLF → LF, trimmed, **no** final newline) | `SongLyrics.MayWriteInside` |
| `cover.jpg` | album folder | `CoverFiles.Write` (`Services/CoverArt/CoverFiles.cs:44-52`), atomic via `cover.jpg.octo-tmp` | A JPEG with a COM segment (`FF FE`, length = 17) holding `Written by Octo`, spliced in **after the APPn segments** (`CoverImage.MarkAsOcto`, `Services/CoverArt/CoverImage.cs:149`). Replaced only when the existing cover is Octo's own and smaller, and never when a `folder.*` or another `cover.*` exists | Navidrome; `CoverImage.IsOctoCover` |
| Cover in tags / folder cover (upgrade) | audio files, `cover.jpg`/`folder.jpg` | `CoverUpgradeWorker` (`CoverUpgrade.cs:875-912`), folder files atomic via `<path>.octo-tmp` | JPEG | undo `CoverUpgrade.cs:1103` |
| Cover backups | `/app/config/cover-backups/<32 hex chars>` | `CoverUpgradeJournal.Record` (`CoverUpgrade.cs:281-287`), atomic via `.tmp`, write-once | The raw old picture bytes, **no extension**. Deduplicated by hash. Unreferenced ones are deleted on `Rewrite` | undo (`Backup(hash)` :319) |
| Found-cover thumbnails | `/app/config/cover-upgrade-found/<album id>.jpg` | `CoverUpgradeStore.SaveFoundThumb` (`CoverUpgrade.cs:153-165`, called at `:750`). **Not atomic** | A JPEG fitted to the thumbnail size. The whole folder is deleted by `ClearFoundThumbs` | `FoundThumb` :167 → `CoverUpgradeController` |
| Covers override folder | `/app/config/covers/<list name, genre or decade>.{jpg,jpeg,png,webp}` | **User-provided**, never written | Images | `CoverArtService` (`Services/CoverArt/CoverArtService.cs:397-415`), with a path-traversal guard |
| Quarantine folder | `<musicRoot>/.octo-trash/<yyyy-MM-dd>/<relative path>` | `LibraryActionQuarantine.Move` (rename, or copy + length check + delete) | The original audio file plus its `.octo-action.json` | `Restore`, `Sweep`, `LibraryActionJournal.Reconcile` |
| Incoming staging | `<effective download dir>/.octo-incoming/`: `replacement-<guid N>.<ext>` (`BaseDownloadService.StageReplacement` :1721), `lidarr/<guid N>/<file>`, `lidarr-<guid N>.<ext>` copies (`Services/Lidarr/LidarrTrackFetcher.cs:135`) | downloads, replacements | A dot folder, so Navidrome does not scan it. The final placement is a single rename (`RevealReplacementAsync` :1737) | download pipeline |
| Downloaded audio | `<effective download dir>/<layout>/...` | `BaseDownloadService` (`PathHelper.BuildLayoutPath`) | Audio with tags written by TagLib | Navidrome |
| Download cache | `/tmp/octo-cache/**/{provider}_{externalId}.*` | cache-mode downloads | Audio files. Deleted after `Subsonic:CacheDurationHours` (`CACHE_DURATION_HOURS`, default 1) by `CacheCleanupService` (`Services/Common/CacheCleanupService.cs:68`), except under `radio/` | `BaseDownloadService.GetCachedFilePath` :1863 |
| Radio cache audio | `/tmp/octo-cache/radio/<key>.mp3` | `LastFmRadioTrackCache.GetOrCreateAsync` (:35-66), atomic via `.<key>.<guid N>.tmp` | MP3. 24 h retention and a 512 MB cap (its own pruning). Access time is touched on use | `LastFmRadioStreamService` |
| Radio transcode spool | `/tmp/octo-radio-in-<guid N>` (+ `.spectral`) | `LastFmRadioAudioTranscoder.TranscodeToMp3Async` (:60-66) | A temporary input copy, deleted afterwards | ffmpeg |
| M3U playlists | `<configured download dir>/<Subsonic:PlaylistsDirectory, default playlists>/<name>.m3u` | `PlaylistSyncService` (`Services/Subsonic/PlaylistSyncService.cs:227-256`, `:311-362`) | `#EXTM3U`, then `#EXTINF:<dur>,<artist> - <title>` and a relative path, with `Environment.NewLine` line endings | **Dead in practice:** `PlaylistSyncService` is never registered in DI, so `BaseDownloadService.PlaylistSyncService` (`GetService<>`, :62-72) is always null. Do not port it, or port it only if the parity harness shows it running |
| Leftover temp and corrupt files | `*.tmp`, `*.octo-tmp`, `*.corrupt-<ticks>` | crashes, corrupt loads | Never cleaned up by Octo. A stale `.tmp` is simply overwritten by the next save | nobody |

---

## 6. Implications for the Rust port

1. **Write a single `state::save_atomic(path, bytes)`** (write `path + ".tmp"`, then rename) and use it everywhere C# is atomic. Adding `fsync` of the file (and directory) is a harmless improvement, but **the temp names must stay the same** (`<file>.tmp`, `cover.jpg.octo-tmp`, `.<key>.<guid>.tmp`), so a C# and Rust process never trip over each other's leftovers in a downgrade test.
2. **One `serde_json` formatter for STJ compatibility**: the escaping table (upper-case `\uXXXX` for non-ASCII and HTML-sensitive characters, `"` for `"`), the double formatting (`1`, not `1.0`; `1E-05`), and `DateTime` with trimmed 7-digit fractions and a `Z` only when the value had one. Every file goes through it. The indented variant (2 spaces) is needed for `lastfm-radio-state.json`, `.mappings.json` and `settings.json`.
3. **Emit the computed fields** listed in §2, and ignore them on read.
4. **Reads are lenient.** Unknown fields are ignored. Missing fields get the C# defaults (spelled out per struct with `#[serde(default = ...)]` where the C# initializer is not `0`/`false`/`null`: `LearnedSignal = true`, `Source = "scrobble"`, `Origin = "app"`, `State = "queued"`, `Pass = 1`, `DryRun = true`, `FolderCovers = true`, `SmallerThan = 1000`, `Version = 1`, `Scope = "OctoDownloads"`, `Has = "none"`, `Result = "weak"`). `null` where C# declares a non-nullable string is accepted.
5. **Keep the recovery behavior per file** (§3): start empty, move aside as `.corrupt-<ticks>` (use .NET ticks: 100 ns intervals since 0001-01-01, so log readers see the same scale), refuse to write (`settings.json`), and Running → Interrupted with the exact `Reason` strings.
6. **Load-time pruning** (expired sessions, rejected peers past the TTL, iTunes TTLs, radio retention, `working` → `queued`) means a load followed by a save is *not* byte-identical once the fixture's dates are in the past. The round-trip test must go through the serde structs, not the stores. The store tests should inject a clock.
7. **JSONL journals:** append `serialize(entry) + "\n"`, skip blank and unparseable lines on read, and rewrite atomically. The short names (`p`/`b`/`a`/`t`/`r` and `p`/`k`/`h`/`r`) apply to two journals only. `lyrics-undo.jsonl` uses full names.
8. **Decide and document** in `known-diffs.md`: the non-atomic, crash-on-corrupt `.mappings.json`; the non-atomic quarantine manifest and `.lrc` writes; and the dead `PlaylistSyncService`.

---

## Regenerating the fixtures

The three C# generators (`fixtures/generator/`, `fixtures/covers/generator/`,
`fixtures/tags/generator/`) build against the C# app, which left this branch at the cutover.
Restore it from its tag first, as untracked files (`.gitignore` covers them), and remove it
again afterwards:

```sh
# from the repo root
git archive csharp-final octo octo.Tests | tar -x
# ... run a generator as below ...
rm -rf octo octo.Tests
```

The generator is in `fixtures/generator/` (`gen.csproj` + `Program.cs`). It compiles against `octo/octo.csproj` under the assembly name `octo.Tests`, so `InternalsVisibleTo` gives it the internal types, and it reaches private nested records (`ExternalIdRegistry.Persisted`, `BrowseSessionStore.Saved`, `ITunesCoverArtLookup.CachedMaster`, `GeneratedPlaylistService.StateDocument`) by reflection. With no .NET SDK on the host:

```sh
# from the repo root; writes into docs/rust-migration/fixtures/state-new/ (then diff and copy)
docker run --rm -v "$PWD":/repo -w /repo/docs/rust-migration/fixtures/generator \
  mcr.microsoft.com/dotnet/sdk:9.0 dotnet run -- /repo/docs/rust-migration/fixtures/state-new
```

Building writes `bin/` and `obj/` under `fixtures/generator/` and `octo/`, which are git-ignored. Run on Windows (the 2026.10.05 regeneration was), STJ's indented writer ends lines with CRLF: turn `lastfm-radio-state.json` back to LF before copying it, and leave `lyrics-undo.jsonl` and `update/request` (a fresh id each run) as they are. The files under `update/status`, `update/helper`, `update/log` and the `.lrc`/`.txt` sidecars were written by hand in the formats documented in §5, because their writers are a shell script or need real audio files.
