# Config inventory (Phase 0)

The contract for the Rust config loader. Parity target: commit `15d6840` (release `2026.10.03.2`).

Sources read: `.env.example`, `docker-compose.yml`, `octo/appsettings.json`, `octo/appsettings.Development.json`, `octo/Program.cs`, every class in `octo/Models/Settings/`, `octo/Services/Admin/SettingsFileWriter.cs`, `octo/Services/Admin/RestartTracker.cs`, `octo/Controllers/AdminController.cs` (settings, raw-config, config-sources), `octo/Services/LastFm/LastFmScrobbleService.cs`, `octo/Services/Common/LogRedaction.cs`, `octo/wwwroot/admin/` (form field names), plus every consumer found by grepping for `IOptions<`, `IOptionsMonitor<`, `IConfiguration`, `GetSection(` and `Environment.` across `octo/`. Binder, converter and JSON-provider behavior was checked against the .NET 9 sources (`dotnet/runtime` `release/9.0`).

**Totals:** 164 settings across 14 sections (12 bound classes plus two raw `IConfiguration` keys). This counts each list or dictionary as one setting, and its element fields are listed under it. There are also 5 host-level variables.

> [!NOTE] 2026.10.05 (wave G1)
> 2026.10.05 adds three sections and 21 settings: `RadioSources` (§3.15, 11), `Admin` (§3.16, 2) and `Imports` (§3.17, 4), plus seven in `GeneratedPlaylists` (§3.12). (The download client section that held four more has since been removed; §3.6 is now `Downloads`.) The secret rules changed: every credential now reads as the placeholder (§6). The env vars the release added that Octo itself never reads (`SONIC_CPUS`, `OCTO_IMAGE_TAG`, the starter stack's `COMPOSE_PROFILES` and `NAVIDROME_*`) configure other containers; they are listed in §3.18 so the compose and the installer keep them.

---

## 1. Sources and precedence

`WebApplication.CreateBuilder(args)` (in .NET 9) sets up the configuration sources below, and `Program.cs` then adds `/app/config/settings.json` **last**. Precedence runs from lowest to highest, and a later source wins:

| # | Source | Reload on change | Notes |
|---|---|---|---|
| 1 | Host config: `DOTNET_*` / `ASPNETCORE_*` env vars (prefix stripped), command line | no | Only host keys matter here (`environment`, `urls`). |
| 2 | `appsettings.json` (in the image at `/app`) | yes (default) | Holds most defaults. Optional. |
| 3 | `appsettings.{Environment}.json` | yes | Compose sets `ASPNETCORE_ENVIRONMENT=Production`, and no `appsettings.Production.json` exists. `appsettings.Development.json` holds only `Logging`. |
| 4 | User secrets | n/a | Development only, and no `UserSecretsId` is set, so in practice this is absent. |
| 5 | **Environment variables, all of them, no prefix** | no | `__` maps to `:`. Keys are case-insensitive. |
| 6 | Command-line args | no | The Docker `ENTRYPOINT` passes none. |
| 7 | **`/app/config/settings.json`** | **yes** (`optional: true, reloadOnChange: true`) | Added last in `Program.cs:28`, **so it overrides env vars.** |

**The effective rule: class initializer < `appsettings.json` < env var < `settings.json`.**

Consequences the Rust loader must reproduce:

- **A value saved from the dashboard beats the env var.** Once a key is in `settings.json`, changing `.env` has no effect on that key. `.env.example` says only that env "overrides appsettings.json", which is true.
- **A dashboard form save writes every field of that form**, not only the changed ones (`collectPatch` in `admin.js`). Saving a form therefore freezes the env-derived values of all its fields into `settings.json`.
- **A Raw config save (`PUT /api/admin/raw-config`) writes the whole effective document**, which `GET /api/admin/raw-config` builds from the live options. That freezes every env value shown there into the file.
- **Merging works key by key, not section by section.** Every leaf key resolves on its own: the highest source that defines the key wins. Within a section, `settings.json` can override some keys while env supplies others.
- **Lists and dictionaries merge across sources by index or key.** A list binds from the union of child keys (`Genre:Mappings:0:Pattern`, …) across all providers. So:
  - `"Blocklist": []` in `settings.json` does **not** remove items that env supplies as `Genre__Blocklist__0`.
  - A file list of 1 item over an env list of 3 gives the file's item 0 (merged field by field with env's item 0) plus env's items 1 and 2.
  - Dictionaries (`ListenBrainz:UserTokens`, `LastFm:UserSessions`) merge by key, so a user defined in env cannot be removed through the file. The code comments acknowledge this.
- **A JSON `null` in `settings.json` does not mean "unset".** The JSON provider stores `JsonElement.ToString()`, which is `""` for null. That `""` shadows env and `appsettings.json` (`ConfigurationRoot` returns the first provider, from the top, whose `TryGet` succeeds). Binding `""` then gives:
  - for `string`: `""`.
  - for `int?` (only `LibraryActions.Actions[].Rating`): null.
  - for `int` / `long` / `bool` / enum: **a binding exception** (`InvalidOperationException: Failed to convert configuration value at '…'`). It is thrown on the next `IOptionsMonitor<T>.CurrentValue` / `IOptions<T>.Value` of that whole section.

  The dashboard sends `null` for a cleared number field (`collectPatch`: `el.value === '' ? null`). So clearing a number field and saving breaks that section until the file is fixed. This is a latent C# bug. **Decision needed for Rust:** match it, or treat `""`/null on a non-string leaf as "fall through to the lower source". The second is recommended, with a warning log. The parity harness should not exercise this path.
- **An empty JSON object or array** (`"Mappings": []`, `"UserTokens": {}`) is stored as the key with a `null` value. That makes the section exist but adds no children.
- **Unknown keys are ignored.** There is no `ErrorOnUnknownConfiguration`.

### settings.json load and reload

- **Path:** the constant `/app/config/settings.json`. It is not configurable, and every state file sits beside it in `/app/config`.
- **Missing file:** ignored (optional). If `/app/config` does not exist, the file provider walks up to the nearest existing directory and watches from there, so a file created later is still picked up.
- **Reload:** the watcher is a `FileSystemWatcher` (inotify). On a change it sleeps `ReloadDelay` = **250 ms**, re-parses the whole file, and fires the root reload token. `IOptionsMonitor<T>` caches are then cleared, the next `CurrentValue` rebinds `T` from scratch, and `OnChange` listeners run.
  - The writer's atomic rename is seen as a change, because the watcher handles `Renamed` for the new name.
  - The dashboard waits 600 ms after a save before re-reading (`admin.js`).
- **Parse leniency:** comments are skipped, trailing commas are allowed, and the top level must be an object. Duplicate keys, compared **case-insensitively**, are a `FormatException` (`A duplicate key 'x' was found`).
- **Parse failure at startup:** `InvalidDataException`, and the host fails to start (crash loop under `restart: unless-stopped`).
- **Parse failure on reload:**
  - The provider's data is set to an empty dictionary, and the exception escapes the change callback. Callbacks run on a task, so it goes unobserved.
  - `OnReload()` is **not** called. Consumers that read `IConfiguration` directly then fall back at once, while `IOptionsMonitor` caches keep the last bound values until the next successful reload.
  - **Recommended Rust behavior:** keep the last good snapshot everywhere and log an error. `SettingsFileWriter` refuses to write over an unparseable file, so only hand edits reach this path.
- **`appsettings.json` also hot-reloads**, but it lives inside the image and is not expected to change.

### Env var mapping

- **Raw form:** `Section__Key` → `Section:Key`, and `Section__List__0__Field` → `Section:List:0:Field`. Only a double underscore maps; `_` is literal.
- **Keys are case-insensitive:** `SUBSONIC__URL` = `Subsonic__Url`. If both spellings are set in one environment, which one wins is unspecified (hashtable enumeration order). Rust: last wins in `std::env::vars()` order, and log a warning on a case-only duplicate.
- **There is no `UPPER_SNAKE` mapping inside Octo.** The friendly names (`SUBSONIC_URL`, `VERIFY_DOWNLOADS`, …) exist only in `docker-compose.yml`, which interpolates them into the raw `Section__Key` names.
  - XML doc comments in the settings classes name some env vars that **nothing maps**: `GENRE_BLOCKLIST` ("comma separated") and `PLAYLISTS_DIRECTORY`. Neither is in compose, and no code splits `GENRE_BLOCKLIST`. Rust must not invent these.
- **`.env.example` → container:** the keys of `.env.example` that do not reach Octo:
  - `DOWNLOAD_PATH`, `OCTO_CONFIG_DIR` (volume paths)
  - `YTDLP_MAX_CONCURRENT`, `YTDLP_GATE_RESERVE`, `YTDLP_BG_GATE_WAIT_SEC`, `YTDLP_SEARCH_CACHE_MAX`, `YTDLP_URL_CACHE_MAX`, `YTDLP_URL_CACHE_TTL` (yt-dlp-shim)
- **Compose:** reads three variables that are missing from `.env.example`: `ENABLE_SYNC_CATALOG`, `SYNC_CATALOG_CLIENTS`, `SYNC_CATALOG_MAX_SONGS`. Compose sets the raw `Section__Key` variables on the octo service; all but two are interpolated, and the two literals are `Library__DownloadPath=/music` and `YouTube__ShimUrl=http://yt-dlp-shim:8080`.
- **Compose always passes a value**, often the empty string, because the form `${X:-}` yields `""`. An empty env value is a real value: it overrides `appsettings.json`. That is fine for strings. For numbers, bools and enums, `""` is a binding exception (see above), but compose always supplies a non-empty default for those.

### Value parsing (ConfigurationBinder + TypeConverter, invariant culture)

Every leaf arrives as a string, from env, or from JSON via `JsonElement.ToString()`. In JSON, `true` becomes `"True"`, numbers keep their raw text (`30`, `1.50`), and strings come unescaped.

| Target type | Accepted | Rejected (binding exception) |
|---|---|---|
| `string` | anything, including `""` | - |
| `bool` | `bool.Parse(trim(v))`: `true` / `false` in any case, with surrounding whitespace | `1`, `0`, `yes`, `""` |
| `int` (`long` for `MinFileSizeBytes`) | trimmed; `Int32.Parse(NumberStyles.Integer)`: optional sign, surrounding whitespace. **Hex is accepted** with a `0x`, `&h` or `#` prefix (`0x1E` = 30). | `30.0`, `1e3`, `""`, overflow |
| `int?` | `""` → null, otherwise as `int` | as `int` |
| enum | `Enum.Parse(type, v, ignoreCase: true)`. Names match **case-insensitively**. **Numeric strings are accepted, including undefined values** (`"7"` binds to an undefined `StorageMode`, which `GET` echoes as `"7"`). **A comma means a flags OR** (`"Track,Album"` = 0\|1 = `Album`). | an unknown name, `""` |
| `List<T>` | children `:0`, `:1`, … (any order) | a scalar value at the list key is ignored |
| `Dictionary<string, T>` | children by key | - |

Every bounds check happens **at read time** in `Effective*` properties or at the use site ("the house pattern"). The stored value is never altered, and `GET /api/admin/settings` echoes the raw value. There are no `PostConfigure`, `IValidateOptions` or data annotations.

### How settings are consumed (hot reload)

- **Binding:** `Program.cs` calls `Configure<T>(Configuration.GetSection(name))` for 12 classes: `Genre`, `LibraryActions`, `GeneratedPlaylists`, `Subsonic`, `Downloads`, `Lidarr`, `LastFm`, `Notifications`, `Metadata`, `Server`, `ListenBrainz`, `Updates`.
- **Not bound:** `Library:DownloadPath` and `YouTube:ShimUrl` are read straight from `IConfiguration`. `AdminController` also reads `Updates` through `_config.GetSection("Updates").Get<UpdateSettings>()` on each request.
- **No `IOptionsSnapshot`.** Most consumers hold `IOptionsMonitor<T>` and read `.CurrentValue` at use, so they are live. The exceptions:
  - `IOptions<T>.Value`, captured in a singleton at construction, so read once at startup: `SubsonicResponseBuilder`, `SubsonicStartupValidator`, `StartupValidationOrchestrator` (Subsonic); `LastFmCoverArtLookup` (LastFm, Metadata); `DeezerCoverArtLookup`, `LastFmService` (Metadata: the Accept-Language header is set once).
  - `IConfiguration["Library:DownloadPath"]` captured in constructors: `BaseDownloadService`, `LocalLibraryService`, `PlaylistSyncService`. Read per call in `AdminController`, `GenreBackfillWorker`, `CoverUpgrade*`, `Lidarr*`, `NavidromeSongPathResolver`.
  - Decided once when a worker starts: `LibraryActionPlaylistWorker` (`Enabled`, `PlaylistsEnabled`, `PollIntervalSeconds`) and `CacheCleanupService` (`StorageMode == Cache`).
  - **The only `OnChange` subscriber:** `LastFmRadioRefreshWorker`. It fingerprints the station definitions, `EnableRadio` and the personalized "shape", and on a change it re-queues a station rebuild for every known user.
- **`RestartTracker`** (`Services/Admin/RestartTracker.cs`) snapshots 15 keys at startup and reports the ones that differ now in `GET /api/admin/settings` → `_meta.RestartPending`, as `"Section:Key"` strings.
  - Comparison: empty or whitespace equals missing; values are trimmed; if both sides parse as bool, they compare as bools (`True` = `true`); otherwise the comparison is ordinal and case-sensitive.
  - The keys: `Library:DownloadPath`, `YouTube:ShimUrl`, `Subsonic:Url`, `Subsonic:LibraryPath`, `LibraryActions:Enabled`, `LibraryActions:PlaylistsEnabled`, `LibraryActions:PollIntervalSeconds`.
  - The dashboard's `data-restart="true"` markers name the same 15 fields.
- **Rust model** (per PLAN.md): an `ArcSwap<Settings>` snapshot that is rebuilt on every reload. A consumer that must keep C#'s "captured at startup" behavior copies the value out at construction.

**Reload column legend used below:**

- **Live:** read at use, so a change applies within about 250 ms.
- **Restart:** captured at startup.
- **Partial:** some consumers are live and some captured; the details are in the row.
- **Dead:** bound and shown, but nothing acts on it.
- **RT:** listed in `RestartTracker`, so the dashboard shows "restart to apply".

**Who can write a setting:** **every key** can be written by `POST /api/admin/settings` (an untyped deep merge, any keys) and by `PUT /api/admin/raw-config` (a wholesale replace). The **Form** column says whether the dashboard (`wwwroot/admin/index.html`) has a field named `Section.Key` for it. Writers other than these two are noted per row and summarized in §5.

---

## 2. Host-level variables (not settings classes)

| Variable | Where | Default | Effect / Rust mapping |
|---|---|---|---|
| `ASPNETCORE_ENVIRONMENT` (also `DOTNET_ENVIRONMENT`) | compose: `Production` | `Production` | `Development` turns on Swagger UI (`/swagger`) and loads `appsettings.Development.json`. Rust: optional, and the OpenAPI UI may be dropped (PLAN). |
| `ASPNETCORE_URLS` | Dockerfile: `http://+:8080` | `http://+:8080` | Kestrel's bind address. Rust: bind `0.0.0.0:8080`, and honor `ASPNETCORE_URLS` if set (first `http://` entry; `+` and `*` mean all interfaces). With `Admin:Port` set (§3.16) the dashboard's port is added to these (`AdminPortSplit.ListenUrls`: `urls`, else `ASPNETCORE_HTTP_PORTS` as `http://+:<port>` entries, else `http://localhost:5000`), unless an entry already ends in `:<port>`; Rust binds `0.0.0.0:<port>` beside the main address. |
| `ASPNETCORE_HTTP_PORTS` | not set | - | Read only by the admin port's address list (above). |
| `Logging__LogLevel__Default`, `Logging__LogLevel__<Category>` | not set (`appsettings.json` has no `Logging` section) | Information | Standard .NET filter. Development adds `Microsoft.AspNetCore: Warning`. Rust: translate into a `tracing` `EnvFilter`, and honor `RUST_LOG` if set. |
| `TMPDIR` | not set | `/tmp` | Implicit through `Path.GetTempPath()`: the cache dir `octo-cache` (StorageMode=Cache), the radio spool, and the working dir for ffmpeg and fpcalc. Rust: `std::env::temp_dir()`. |
| `DOTNET_USE_POLLING_FILE_WATCHER` | not set | inotify | Switches the settings.json watcher to polling (4 s). Rust: optional; `notify` has a `PollWatcher`. |

---

## 3. Settings by section

Default columns: **Class** is the C# initializer, **appsettings** is `octo/appsettings.json` ("-" means the key is absent there), and **Compose** is the default `docker-compose.yml` passes when the `.env` var is unset ("-" means not mapped). Unless a row says otherwise, the effective default is compose's value under Docker, else appsettings, else the class value.

### 3.1 `Subsonic` → `Octo.Models.Settings.SubsonicSettings` (29)

Enums:

- `StorageMode { Permanent, Cache, Stream }`
- `DownloadMode { Track, Album }`
- `ExplicitFilter { All, ExplicitOnly, CleanOnly }`
- `FolderStructure { Organized, Flat, ByArtist }`
- `DownloadSource { YouTube, Lidarr }`
- `HeartDownloadSource { YouTube, Lidarr }`

| JSON key | .env var (→ `Subsonic__Key`) | Type | Class / appsettings / Compose | Validation, parsing, notes | Reload | Form |
|---|---|---|---|---|---|---|
| `Url` | `SUBSONIC_URL` | string? | null / `""` / `""` | Blank means unconfigured: first-run LAN discovery runs, and when it finds exactly one server it writes this key and calls `StopApplication()` (§5). The proxy treats a non-absolute URI as unconfigured. Consumers `TrimEnd('/')`. | Partial, RT (proxy, `NavidromeIdentityService` and the library services read it live; startup validators log it once) | yes |
| `AdminUsername` | `SUBSONIC_ADMIN_USERNAME` | string? | null / - / `""` | Pairs with `AdminPassword`. Trimmed when compared. | Live | yes |
| `AdminPassword` | `SUBSONIC_ADMIN_PASSWORD` | string? | null / - / `""` | **Secret, masked** (§6). POST: the placeholder is dropped, so the saved value is kept; placeholder plus extra text is a 400; the placeholder together with a changed `AdminUsername` is a 400. | Live | yes |
| `AutoDetectDownloadPath` | `AUTO_DETECT_DOWNLOAD_PATH` | bool | true / - / `true` | When true, the effective download path is the music folder Navidrome reports, falling back to `Library:DownloadPath`. | Live | yes |
| `LibraryPath` | - | string | `""` / - / - | Picks one of several Navidrome libraries by folder path. A stale value is ignored with a warning. Applied at music-folder detection, which runs at startup (forced), from `GET /api/admin/library-status`, and with a 30-min cache TTL. | Partial, RT | yes |
| `ExplicitFilter` | `EXPLICIT_FILTER` | enum ExplicitFilter | All / `All` / `All` | - | Live | yes |
| `DownloadMode` | `DOWNLOAD_MODE` | enum DownloadMode | Track / `Track` / `Track` | Legacy direct-download jobs only. | Live | no |
| `StorageMode` | `STORAGE_MODE` | enum StorageMode | **Permanent** / `Stream` / `Stream` | The class default differs from appsettings, so the effective default is `Stream`. | Partial (`BaseDownloadService` live; `CacheCleanupService` checks `== Cache` once at start) | no |
| `CacheDurationHours` | `CACHE_DURATION_HOURS` | int | 1 / `1` / `1` | No clamp. Used only while the cache cleanup runs (hourly). | Live | no (an HTML mention, no field) |
| `EnableExternalPlaylists` | `ENABLE_EXTERNAL_PLAYLISTS` | bool | **true** / `false` / `false` | The class default differs from appsettings. | Live | yes |
| `EnableSearchDiscovery` | `ENABLE_SEARCH_DISCOVERY` | bool | true / `true` / `true` | - | Live | no |
| `WaitForSearchDurations` | `WAIT_FOR_SEARCH_DURATIONS` | bool | true / `true` / `true` | Rust: false makes `SearchDurationWaitMs` 0. | Live | yes |
| `SearchDurationWaitMs` | `SEARCH_DURATION_WAIT_MS` | int | 1000 / - / `1000` | Rust only. How long a search waits for the YouTube lengths of its top outside rows; `Clamp(0, 10000)`, 0 never waits. Rows not resolved in time go out with Deezer's length and are resolved in the background. Listed last in config-sources. | Live | no |
| `SearchLastFmWaitMs` | `SEARCH_LASTFM_WAIT_MS` | int | 1000 / - / `1000` | Rust only. How long a search waits for Last.fm's track.search; `Clamp(0, 10000)`, 0 waits however long Last.fm takes. Past it the outside songs come from Deezer's track search for the words, in Deezer's order; Last.fm's late answer is kept for the next search of the same words. Listed last in config-sources. | Live | no |
| `EnableSyncCatalog` | `ENABLE_SYNC_CATALOG` (compose only, not in `.env.example`) | bool | true / `true` / `true` | - | Live | yes |
| `SyncCatalogClients` | `SYNC_CATALOG_CLIENTS` (compose only) | string (comma list) | `Symfonium` / `Symfonium` / `Symfonium` | Split on `,`, entries trimmed, empties dropped. **A client matches when its `c` parameter *contains* an entry, ignoring case** (a substring match, not equality). | Live | yes |
| `SyncCatalogMaxSongs` | `SYNC_CATALOG_MAX_SONGS` (compose only) | int | 1000 / `1000` / `1000` | `Math.Clamp(v, 50, 5000)` | Live | yes |
| `StreamSizeWarmPerMinute` | `STREAM_SIZE_WARM_PER_MINUTE` | int | 5 / `5` / `5` | Rust only. Outside songs a minute whose stream size is learned in the background; `Clamp(0, 30)`, 0 turns the lookups off. | Live | yes |
| `CoverCacheMb` | `COVER_CACHE_MB` | int | 1024 / `1024` / `1024` | Rust only. Megabytes of library covers kept in `<config>/cover-cache/` (`services/cover_art/local_cover_cache.rs`): each cover id fetched from Navidrome once at 1200 px, smaller sizes cut from it. `Clamp(0, 102400)`; 0 turns the cache off. Least recently used covers go first, down to 90 %. | Live | yes |
| `SizeFlagClients` | `SIZE_FLAG_CLIENTS` | string (comma list) | `Amperfy` / `Amperfy` / `Amperfy` | Rust only. Split on `,`, entries trimmed, empties dropped; an app matches when its `c` parameter (all values, comma-joined) or its User-Agent *contains* an entry, ignoring case. Answers to a matching app give each outside song without a size `size` 1 and hold any size above 2,147,483,647 to it (`middleware/size_flag.rs`). Empty turns it off. Listed last in config-sources. | Live | yes |
| `PlaylistsDirectory` | - | string | `playlists` / `playlists` / - | **Dead:** its only consumer, `PlaylistSyncService`, is never registered in DI (`GetService` returns null). It would be relative to `Library:DownloadPath`, with null falling back to `"playlists"`. | Dead | yes |
| `DownloadOnStar` | `DOWNLOAD_ON_STAR` | bool | true / `true` / `true` | Feeds only `EffectiveHeartDownloadSources()` when `HeartDownloadSources` is empty. | Live | no |
| `DownloadAlbumOnStar` | `DOWNLOAD_ALBUM_ON_STAR` | bool | true / `true` / `true` | As above. | Live | no |
| `RecordRequestedBy` | `RECORD_REQUESTED_BY` | bool | true / `true` / `true` | - | Live | yes |
| `StarDownloadsForRequester` | `STAR_DOWNLOADS_FOR_REQUESTER` | bool | false / `false` / `false` | - | Live | yes |
| `SkipOwnedSongs` | `SKIP_OWNED_SONGS` | bool | true / `true` / `true` | - | Live | yes |
| `DownloadOnPlay` | `DOWNLOAD_ON_PLAY` | bool | false / `false` / `false` | - | Live | yes |
| `LidarrAlbumOnPlay` | `LIDARR_ALBUM_ON_PLAY` | bool | false / `false` / `false` | - | Live | yes |
| `FolderStructure` | `FOLDER_STRUCTURE` | enum FolderStructure | Flat / `Flat` / `Flat` | - | Live | yes |
| `DownloadSource` | `DOWNLOAD_SOURCE` | enum DownloadSource | YouTube / `YouTube` / `YouTube` | The legacy heart chain, used when `HeartDownloadSources` is empty. | Live | no (only in JS/HTML text) |
| `HeartDownloadSources` | - (raw: `Subsonic__HeartDownloadSources__0__Source`, …) | `List<HeartDownloadStep>` | `[]` / - / - | Element: `Source` (enum HeartDownloadSource), `Enabled` (bool?, legacy), `SongEnabled` (bool?), `AlbumEnabled` (bool?). `EffectiveHeartDownloadSources()` works as follows. It drops steps whose `Source` is undefined and keeps the first step per source. It sets `SongEnabled = SongEnabled ?? Enabled ?? false`, and the same for `AlbumEnabled`. It appends each missing source as disabled. If the list ends up empty, it derives one from `DownloadSource` + `DownloadOnStar` + `DownloadAlbumOnStar` in the order YouTube, Lidarr: Lidarr → Lidarr; otherwise YouTube. `GET /api/admin/settings` and `/raw-config` return the **effective** list (`Source` as a name, booleans non-null). | Live | yes |
| `UseLocalStaging` | `USE_LOCAL_STAGING` | bool | false / `false` / `false` | **Dead:** bound and shown, but nothing reads it. | Dead | yes |

### 3.2 `Library` → raw `IConfiguration` (1)

| JSON key | .env var | Type | Defaults | Notes | Reload | Form |
|---|---|---|---|---|---|---|
| `DownloadPath` | literal `Library__DownloadPath=/music` (compose); host side `DOWNLOAD_PATH` is the volume | string | (no class) / `/music` / `/music` | **The fallback when the key is missing differs by consumer:** `./downloads` (BaseDownloadService, GenreBackfillWorker, PlaylistSyncService, NavidromeSongPathResolver, admin genre and browse), `/music` (CoverUpgrade, LidarrHeartAcquisition, LidarrTrackFetcher, admin GET), `<cwd>/downloads` (LocalLibraryService). It is never missing in practice, because appsettings defines it. It is overridden by the detected Navidrome folder when `Subsonic.AutoDetectDownloadPath` is on. | Partial, RT (captured by BaseDownloadService, LocalLibraryService and PlaylistSyncService; read per call elsewhere) | yes |

### 3.3 `YouTube` → raw `IConfiguration` (1)

| JSON key | .env var | Type | Defaults | Notes | Reload | Form |
|---|---|---|---|---|---|---|
| `ShimUrl` | literal `YouTube__ShimUrl=http://yt-dlp-shim:8080` | string | (no class) / `http://yt-dlp-shim:8080` / same | `YouTubeResolver.ResolveBaseUrl`: blank or whitespace → `http://yt-dlp-shim:8080`, otherwise `Trim().TrimEnd('/')`. The admin status probe reads it per call, with the same fallback. | Restart, RT (captured by the `YouTubeResolver` singleton) | yes |

### 3.4 `Server` → `ServerSettings` (1)

| JSON key | .env var | Type | Defaults | Notes | Reload | Form |
|---|---|---|---|---|---|---|
| `PublicUrl` | - (raw `Server__PublicUrl`) | string | `""` / - / - | For display only. | Live | yes |

### 3.5 `Updates` → `UpdateSettings` (2)

| JSON key | .env var | Type | Defaults | Notes | Reload | Form |
|---|---|---|---|---|---|---|
| `Check` | `UPDATES_CHECK` | bool | true / `true` / `true` | `ReleaseCheck` re-reads it on its 5-min loop; the GitHub check runs every 6 h. | Live | yes |
| `Repo` | `UPDATES_REPO` | string `owner/name` | `winters27/octo` / same / same | Blank → `winters27/octo`, else `Trim().Trim('/')`. A repo change (compared case-insensitively) forces a re-check. | Live | no (shown, no field) |

### 3.6 `Downloads` → `DownloadSettings` (12)

The source-neutral download settings (verification, fingerprinting, naming, concurrency). This section replaced the download client's own section; every key here reads as `Downloads__<Key>`.

| JSON key | .env var (→ `Downloads__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `VerifyDownloads` | `VERIFY_DOWNLOADS` | bool | false / `false` / `false` | Turns on AcoustID verification. | Live | yes |
| `AcoustIdApiKey` | `ACOUSTID_API_KEY` | string | `""` / `""` / `""` | **Secret**: the placeholder in GET; masked in config-sources (ends in `ApiKey`). | Live | yes |
| `AcoustIdUserApiKey` | `ACOUSTID_USER_KEY` | string | `""` / `""` / `""` | **Secret**: the placeholder in GET; masked in config-sources. | Live | yes |
| `SubmitConfirmedFingerprints` | `ACOUSTID_SUBMIT` | bool | false / `false` / `false` | Needs `AcoustIdUserApiKey`. | Live | yes |
| `MinMatchScore` | `MIN_MATCH_SCORE` | int (percent) | 85 / `85` / `85` | `Math.Clamp(v, 50, 99)`; the fraction is that value / 100.0. | Live | yes |
| `TagFromMusicBrainz` | `TAG_FROM_MUSICBRAINZ` | bool | false / `false` / `false` | Implied true when `NameFromMatch` is on. | Live | yes |
| `FingerprintSeconds` | `FINGERPRINT_SECONDS` | int | 120 / `120` / `120` | `Clamp(15, 600)` | Live | no |
| `FingerprintTimeoutSeconds` | `FINGERPRINT_TIMEOUT_SECONDS` | int | 30 / `30` / `30` | `Clamp(5, 300)` | Live | no |
| `AcoustIdTimeoutSeconds` | `ACOUSTID_TIMEOUT_SECONDS` | int | 10 / `10` / `10` | `Clamp(2, 120)`. The `AcoustId` named HTTP client also has a fixed 10 s `HttpClient.Timeout`. | Live | no |
| `NameFromMatch` | `NAME_FROM_MATCH` | bool | false / `false` / `false` | - | Live | yes |
| `ParallelDownloads` | `PARALLEL_DOWNLOADS` | int | 3 / `3` / `3` | `Clamp(1, 6)` | Live | yes |
| `DetectTranscodes` | `DETECT_TRANSCODES` | bool | true / `true` / `true` | - | Live | yes |

### 3.7 `Lidarr` → `LidarrSettings` (7)

Enum: `LidarrCompletionMode { Accepted, Imported }`.

| JSON key | .env var (→ `Lidarr__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `BaseUrl` | `LIDARR_URL` | string? | null / `""` / `""` | Lidarr counts as configured only when **both** `BaseUrl` and `ApiKey` are non-blank. `TrimEnd('/')` when requests are built. | Live | yes |
| `ApiKey` | `LIDARR_API_KEY` | string? | null / `""` / `""` | **Secret, in clear** in GET; masked in config-sources. | Live | yes |
| `RootFolderPath` | `LIDARR_ROOT_FOLDER_PATH` | string? | null / `""` / `""` | - | Live | yes |
| `QualityProfileId` | `LIDARR_QUALITY_PROFILE_ID` | int | 0 / `0` / `0` | - | Live | yes |
| `MetadataProfileId` | `LIDARR_METADATA_PROFILE_ID` | int | 0 / `0` / `0` | - | Live | yes |
| `CompletionMode` | `LIDARR_COMPLETION_MODE` | enum | Accepted / `Accepted` / `Accepted` | - | Live | yes |
| `ImportTimeoutSeconds` | `LIDARR_IMPORT_TIMEOUT_SECONDS` | int | 1800 / `1800` / `1800` | Clamped where used: the heart wait uses `max(60, v)`; the track fetch and import hand-off use `max(1, v)`; the poll interval is `Clamp(v/30, 1, 10)` s. | Live | yes |

### 3.8 `LastFm` → `LastFmSettings` (25)

| JSON key | .env var (→ `LastFm__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `ApiKey` | `LASTFM_API_KEY` | string | `""` / `""` / `""` | **Secret, in clear** in GET (masked in config-sources). Used trimmed for signing. | Partial (`LastFmService` and scrobbling live; `LastFmCoverArtLookup` captures it at startup) | yes |
| `ApiSecret` | `LASTFM_API_SECRET` | string | `""` / `""` / `""` | **Secret, masked.** POST: placeholder → kept; placeholder plus extra text → 400. Used trimmed. | Live | yes |
| `ScrobbleExternalPlays` | - | bool | true / `true` / - | - | Live | yes |
| `ScrobbleLibraryPlays` | - | bool | true / - / - | - | Live | yes |
| `UserSessions` | - (raw `LastFm__UserSessions__alice__SessionKey`, `…__LastFmUser`) | `Dictionary<string, LastFmUserSession>` (OrdinalIgnoreCase) | `{}` / - / - | Value: `SessionKey` (string, **secret**), `LastFmUser` (string). `SessionFor(user)` trims the name and matches it case-insensitively; a blank `SessionKey` counts as no session. **POST `/settings` strips this key whatever it holds.** Written only by the Connect flow, removed by Disconnect or a refusal (§5). | Live | no |
| `EnableRadio` | `LASTFM_ENABLE_RADIO` | bool | true / `true` / `true` | Radio requires `ApiKey` and `EnableRadio`. The `OnChange` hook re-queues station builds when this goes false → true. | Live | yes |
| `RadioTrackCount` | `LASTFM_RADIO_TRACK_COUNT` | int | 50 / `50` / `50` | `Clamp(10, 100)` | Live | yes |
| `RadioCacheDurationHours` | `LASTFM_RADIO_CACHE_HOURS` | int | 24 / `24` / `24` | `Clamp(1, 168)` | Live | yes |
| `EnablePersonalizedStations` | `LASTFM_ENABLE_PERSONALIZED_STATIONS` | bool | true / `true` / `true` | Part of the `OnChange` personalized fingerprint. | Live | yes |
| `EnableYourMix` | `LASTFM_ENABLE_YOUR_MIX` | bool | true / `true` / `true` | As above. | Live | yes |
| `EnableDiscoveryMix` | `LASTFM_ENABLE_DISCOVERY_MIX` | bool | true / `true` / `true` | As above. | Live | yes |
| `ArtistStationCount` | `LASTFM_ARTIST_STATION_COUNT` | int | 2 / `2` / `2` | `Clamp(0, 5)` | Live | yes |
| `GenreStationCount` | `LASTFM_GENRE_STATION_COUNT` | int | 3 / `3` / `3` | `Clamp(0, 5)` | Live | yes |
| `EnableDiscoveryStations` | `LASTFM_ENABLE_DISCOVERY_STATIONS` | bool | true / `true` / `true` | - | Live | yes |
| `ExposeRadioAsPlaylists` | `LASTFM_EXPOSE_AS_PLAYLISTS` | bool | true / `true` / `true` | - | Live | yes |
| `ExposeRadioAsStreams` | `LASTFM_EXPOSE_AS_STREAMS` | bool | true / `true` / `true` | - | Live | yes |
| `RadioStreamBitrateKbps` | `LASTFM_RADIO_STREAM_BITRATE_KBPS` | int | 192 / `192` / `192` | **Rounded up to a bucket:** ≤96 → 96, ≤128 → 128, ≤192 → 192, ≤256 → 256, otherwise 320. | Live | yes |
| `EnableIcyMetadata` | - | bool | true / `true` / - | - | Live | yes |
| `StarterPublishTimeoutSeconds` | - | int | 8 / - / - | `v <= 0` → none (wait forever), else `TimeSpan.FromSeconds(Clamp(v, 1, 300))`. | Live | yes |
| `RadioLoudnessTargetLufs` | - | int | -16 / - / - | `0` → normalization off, else `Clamp(v, -23, -9)` (as a double). | Live | yes |
| `HistoryRetentionDays` | `LASTFM_HISTORY_RETENTION_DAYS` | int | 90 / `90` / `90` | `Clamp(7, 365)` | Live | yes |
| `DiscoveryPercent` | `LASTFM_DISCOVERY_PERCENT` | int | 35 / `35` / `35` | `Clamp(0, 100)` | Live | yes |
| `RefreshIntervalHours` | `LASTFM_REFRESH_INTERVAL_HOURS` | int | 12 / `12` / `12` | `Clamp(1, 168)` | Live | yes |
| `MinimumPlays` | `LASTFM_MINIMUM_PLAYS` | int | 10 / `10` / `10` | `Clamp(3, 100)` | Live | yes |
| `DiscoveryStations` | - (raw `LastFm__DiscoveryStations__0__Name`, …) | `List<DiscoveryStationSettings>` | `[]` / `[]` / - | Element: `Id` (string), `Name` (string), `Enabled` (bool, default **true**), `Tags` (`List<string>`). **POST validation (400):** at most 12; each must be an object with a non-empty unique `Id` (case-insensitive) and a unique `Name` of 1–100 characters after trimming; 1–5 tags, none blank. **Read-time `EffectiveDiscoveryStations()`:** takes the first 12. Name is trimmed, 1–100 characters, unique case-insensitively, otherwise skipped. A tag is normalized with `Trim().ToLowerInvariant()` and inner whitespace collapsed to one space, kept at 1–80 characters, de-duplicated, at most 5; no tags means the station is skipped. Id is normalized to ASCII letters and digits only, the first 32, lowercased; when empty, the id is `hex(SHA256(lower(trim(name)) + "|" + join("\|", tags))[0..12])` in lowercase, which is 24 hex characters; a duplicate id is skipped. GET `/settings` returns the **raw array from settings.json** when present, otherwise the class serialized with PascalCase names. | Live (`OnChange` re-queues the changed or removed stations) | yes |

### 3.9 `Genre` → `GenreSettings` (9)

Enums:

- `GenreFallbackSource { None, LastFm, MusicBrainz }`
- `GenreEmptyBehavior { Leave, Clear, Unknown }`
- `GenreMatchMode { Contains, Exact }`

| JSON key | .env var (→ `Genre__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `Enabled` | `GENRE_NORMALIZE` | bool | false / `false` / `false` | - | Live | yes |
| `Mappings` | - (raw `Genre__Mappings__0__Pattern`, …) | `List<GenreMappingSettings>` | `[]` / `[]` / - | Element: `Id`, `Pattern`, `Genre` (strings), `Match` (enum, default Contains), `Enabled` (bool, default true). **POST validation (400):** at most 200; each must be an object; pattern is 1–60 characters after trimming and unique case-insensitively; genre is at most 60; `Match` parses case-insensitively as `Contains` or `Exact` (default `Contains`). **Read-time `EffectiveMappings()`:** takes the first 200. The pattern is normalized like a tag and must be 1–60 characters, first occurrence wins. Genre is kept verbatim apart from trimming, at most 60. Id is normalized as for discovery stations, with the fallback `DeterministicId(pattern, [genre])`; a duplicate id is skipped. GET returns the raw file array when present. Otherwise the class is serialized **with `Match` as a number**, because there is no string-enum converter. The presets come from `GET /api/admin/genre/presets` (`BroadGenrePreset()`). | Live | yes |
| `Blocklist` | - (raw `Genre__Blocklist__0`) | `List<string>` | `[]` / `[]` / - | The first 500 entries, normalized like a tag, 1–60 characters, **added to** a fixed built-in list (§7). The comment's claim of a comma-separated `GENRE_BLOCKLIST` env var is false. | Live | yes |
| `Fallback` | `GENRE_FALLBACK` | enum | None / `None` / `None` | - | Live | yes |
| `MaxGenres` | `GENRE_MAX` | int | 10 / `10` / `10` | `Clamp(1, 10)` | Live | yes |
| `OnEmpty` | `GENRE_ON_EMPTY` | enum | Clear / `Clear` / `Clear` | - | Live | yes |
| `UnknownLabel` | `GENRE_UNKNOWN_LABEL` | string | `Unknown` / `Unknown` / `Unknown` | Trimmed; if not 1–60 characters → `Unknown`. | Live | yes |
| `BackfillMaxConsecutiveFailures` | `GENRE_BACKFILL_MAX_FAILURES` | int | 25 / `25` / `25` | `v <= 0 → 0` (never give up), else `Clamp(1, 10000)` | Live | no |
| `BackfillExtensions` | - | `List<string>` | `[]` / `[]` / - | Each entry is trimmed and lowercased, kept at 1–10 characters, and given a leading `.` if missing. An empty result falls back to `.flac .mp3 .m4a .ogg .opus .wav .aiff .aif .wma`. The set is case-insensitive. | Live | no |

### 3.10 `LibraryActions` → `LibraryActionSettings` (24)

Enums:

- `LibraryAction { Delete, WrongSong, WrongVersion, BetterQuality, Keep }`. **This order is persisted as numbers** in `library-actions.json`.
- `LibraryRatingScope { Auto, NoticeOnly, Global }`
- `UpgradeSourceChoice { Auto, Lidarr }`

| JSON key | .env var (→ `LibraryActions__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `Enabled` | `LIBRARY_ACTIONS_ENABLED` | bool | false / `false` / `false` | **POST 400** when the effective `Enabled` is true (from the patch, else current) and the effective `AllowedUsers` has no non-blank entry. | Partial, RT (the playlist worker decides at start; the notice, rating, duplicate and sweep workers read it live) | yes |
| `PlaylistsEnabled` | `LIBRARY_ACTIONS_PLAYLISTS` | bool | true / `true` / `true` | - | Partial, RT (as above) | yes |
| `RatingsEnabled` | `LIBRARY_ACTIONS_RATINGS` | bool | false / `false` / `false` | - | Live | yes |
| `PlaylistPrefix` | `LIBRARY_ACTIONS_PREFIX` | string | `"🛠 "` (U+1F6E0 + space) / same (written as `🛠 `) / `🛠 ` | null → `""`. `""` is allowed (no prefix). Not trimmed. | Live | yes |
| `Actions` | - (raw `LibraryActions__Actions__0__Action`, …) | `List<LibraryActionDefinition>` | `[]` / `[]` / - | Element: `Action` (enum LibraryAction), `Name` (string), `Enabled` (bool, **default false when omitted**), `Rating` (**int?**). **POST 400:** at most 5; each must be an object; name at most 80 after trimming; rating 0–5; no two actions on the same non-zero rating. **Read-time `EffectiveActions()`**, in enum order, for every action: take the first configured entry with that `Action`. The name is trimmed; if it is 0 or more than 80 characters, the default applies (Delete / "Wrong song" / "Wrong version" / "Better quality" / "Keep"). An entry whose prefix + name duplicates an earlier title (case-insensitively) is **dropped**. The rating is `Rating ?? default` (1, 2, 3, 4, 5); outside 0–5 or a duplicate → 0. `Enabled` is the configured value, or when unconfigured, `Action == Keep && (ReviewEnabled \|\| DuplicatesEnabled)`. GET returns the effective list with `Action` as a name. | Live | yes |
| `AllowedUsers` | - (raw `LibraryActions__AllowedUsers__0`) | `List<string>` | `[]` / `[]` / - | **POST 400** with more than 50 entries. `IsAllowed` trims both sides and compares case-insensitively. **Empty means nobody.** | Live | yes |
| `DryRun` | `LIBRARY_ACTIONS_DRY_RUN` | bool | true / `true` / `true` | - | Live | yes |
| `QuarantineDirectory` | `LIBRARY_ACTIONS_TRASH_DIR` | string | `.octo-trash` / same / same | Trim, then trim `/` and `\`. Empty, `.`, `..` or anything containing `..` → `.octo-trash`. Otherwise the first path segment only. | Live | yes |
| `QuarantineRetentionDays` | `LIBRARY_ACTIONS_TRASH_DAYS` | int | 30 / `30` / `30` | `v <= 0 → 0` (keep forever), else `Clamp(1, 3650)` | Live | yes |
| `PollIntervalSeconds` | `LIBRARY_ACTIONS_POLL_SECONDS` | int | 60 / `60` / `60` | `Clamp(15, 3600)` seconds | Partial, RT (fixed by the playlist worker's `PeriodicTimer` at start; the notice worker re-reads it each loop) | yes |
| `MaxActionsPerCycle` | `LIBRARY_ACTIONS_MAX_PER_CYCLE` | int | 20 / `20` / `20` | `Clamp(1, 200)` | Live | yes |
| `KeepReplacedOriginals` | `LIBRARY_ACTIONS_KEEP_REPLACED` | bool | true / `true` / `true` | - | Live | yes |
| `NoticePrefix` | `LIBRARY_ACTIONS_NOTICE_PREFIX` | string | `"▸ "` (U+25B8 + space) / same / `▸ ` | null → `""`. | Live | yes |
| `ReviewEnabled` | `LIBRARY_ACTIONS_REVIEW` | bool | false / `false` / `false` | - | Live | yes |
| `ReviewPlaylistName` | - | string | `Review` / `Review` / - | Blank → `Review`, otherwise trimmed. | Live | yes |
| `ReviewSweepPerHour` | `LIBRARY_ACTIONS_REVIEW_SWEEP_PER_HOUR` | int | 0 / `0` / `0` | `v <= 0 → 0`, otherwise `min(v, 360)` | Live | yes |
| `ReviewSweepOctoDownloads` | `LIBRARY_ACTIONS_REVIEW_SWEEP_OCTO_DOWNLOADS` | bool | false / `false` / `false` | - | Live | yes |
| `DuplicatesEnabled` | `LIBRARY_ACTIONS_DUPLICATES` | bool | false / `false` / `false` | - | Live | yes |
| `DuplicatesPlaylistName` | - | string | `Duplicates` / same / - | Blank → `Duplicates`, otherwise trimmed. | Live | yes |
| `DuplicatesScanHours` | `LIBRARY_ACTIONS_DUPLICATES_SCAN_HOURS` | int | 24 / `24` / `24` | `Clamp(1, 168)` hours | Live | yes |
| `NoticeMaxTracks` | `LIBRARY_ACTIONS_NOTICE_MAX` | int | 100 / `100` / `100` | `Clamp(1, 500)` | Live | yes |
| `RatingsScope` | `LIBRARY_ACTIONS_RATINGS_SCOPE` | enum | Auto / `Auto` / `Auto` | Auto → NoticeOnly when Review or Duplicates is on, otherwise Global. | Live | yes |
| `UpgradePerWeek` | `LIBRARY_ACTIONS_UPGRADE_PER_WEEK` | int | 0 / `0` / `0` | `Clamp(0, 500)` | Live | yes |
| `UpgradeSource` | `LIBRARY_ACTIONS_UPGRADE_SOURCE` | enum | Auto / `Auto` / `Auto` | - | Live | yes |

### 3.11 `Metadata` → `MetadataSettings` (18)

| JSON key | .env var (→ `Metadata__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `Language` | `METADATA_LANGUAGE` | string | `en` / `en` / `en` | Sent as `Accept-Language`. Blank means no header. Split on `,`, and each trimmed part is added with `TryParseAdd`, so an invalid part is skipped silently. | Partial (`DeezerMetadataService` applies it per client; `LastFmService`, `DeezerCoverArtLookup` and `LastFmCoverArtLookup` set it once at construction) | yes |
| `AlbumFromTitle` | `ALBUM_FROM_TITLE` | bool | true / `true` / `true` | - | Live | yes |
| `UseCoverArtArchive` | `COVER_ART_ARCHIVE` | bool | true / `true` / `true` | - | Live | yes |
| `ReplaceVideoCovers` | `REPLACE_VIDEO_COVERS` | bool | true / `true` / `true` | - | Live | yes |
| `WriteCoverFile` | `COVER_FILE` | bool | true / `true` / `true` | - | Live | yes |
| `EmbedFullSizeCovers` | `FULL_SIZE_COVERS` | bool | false / `false` / `false` | Off → embedded covers are capped at 1500 px (`const EmbeddedCoverSide`). | Live | yes |
| `FetchLyrics` | `LYRICS_FETCH` | bool | false / `false` / `false` | - | Live | yes |
| `LyricsSources` | `LYRICS_SOURCES` | string (comma list) | `song,kugou,lrclib,lyricsovh` / **`kugou,lrclib,lyricsovh`** / `song,kugou,lrclib,lyricsovh` | `EffectiveLyricsSources`: split on `,`, trim, drop empties, lowercase, keep only `song kugou lrclib netease lyricsovh`, de-duplicate keeping the first; if `song` is missing, **insert it first**. So the appsettings value is effectively `song,kugou,lrclib,lyricsovh`. GET echoes the raw string. | Live | yes |
| `SaveLyricsTo` | `LYRICS_SAVE_TO` | string | `beside` / - / `beside` | `LyricsSaveTo.Normalize`: trim and lowercase; `inside` or `both` are kept, **anything else → `beside`**. GET returns the normalized value. | Live | yes |
| `PreferWordTimedLyrics` | `LYRICS_PREFER_WORD_TIMED` | bool | true / `true` / `true` | - | Live | yes |
| `WriteLyricsBesideAllSongs` | `LYRICS_WRITE_BESIDE_ALL` | bool | false / `false` / `false` | - | Live | yes |
| `PreferOriginalAlbum` | `PREFER_ORIGINAL_ALBUM` | bool | true / - / `true` | - | Live | yes |
| `YearFromOriginalRelease` | `YEAR_FROM_ORIGINAL_RELEASE` | bool | true / - / `true` | - | Live | yes |
| `PreferredCountries` | `PREFERRED_COUNTRIES` | string (comma list) | `""` / - / `""` | Split on `,`, trim, drop empties, uppercase, keep only 2-character codes, de-duplicate in order. | Live | yes |
| `ReleaseDetailsLookup` | `RELEASE_DETAILS_LOOKUP` | bool | true / - / `true` | - | Live | yes |
| `ReplayGain` | `REPLAYGAIN` | bool | true / - / `true` | - | Live | yes |
| `ReplayGainTimeoutSeconds` | `REPLAYGAIN_TIMEOUT_SECONDS` | int | 45 / - / `45` | `Clamp(10, 300)` | Live | yes |
| `TagRehearsal` | `TAG_REHEARSAL` | bool | false / - / `false` | - | Live | yes |

### 3.12 `GeneratedPlaylists` → `GeneratedPlaylistSettings` (12)

| JSON key | .env var (→ `GeneratedPlaylists__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `Enabled` | `MIXES_ENABLED` | bool | false / `false` / `false` | - | Live | yes |
| `Genres` | `MIXES_GENRES` | bool | true / `true` / `true` | - | Live | yes |
| `Decades` | `MIXES_DECADES` | bool | true / `true` / `true` | - | Live | yes |
| `TrackCount` | `MIX_TRACK_COUNT` | int | 100 / `100` / `100` | `Clamp(10, 500)` | Live | yes |
| `MaxPerArtist` | `MIX_MAX_PER_ARTIST` | int | 3 / `3` / `3` | `Clamp(1, 50)` | Live | yes |
| `CreateAt` | `MIX_CREATE_AT` | int | 20 / `20` / `20` | `Clamp(1, 10000)` | Live | yes |
| `RemoveBelow` | `MIX_REMOVE_BELOW` | int | 10 / `10` / `10` | `Clamp(0, EffectiveCreateAt)` | Live | yes |
| `MaxPlaylists` | `MIX_MAX_PLAYLISTS` | int | 20 / `20` / `20` | `Clamp(1, 100)` | Live | yes |
| `RefreshHours` | `MIX_REFRESH_HOURS` | int | 24 / `24` / `24` | `Clamp(1, 336)` | Live | yes |
| `NewShare` | `MIX_NEW_SHARE` | int (percent) | 0 / `0` / `0` | `Clamp(0, 100)` | Live | yes |
| `NewDays` | `MIX_NEW_DAYS` | int | 30 / `30` / `30` | `Clamp(1, 3650)` | Live | yes |
| `NewReleases` | `FOR_YOU_NEW_RELEASES` | bool | true / - / `true` | Made for you's New Releases. `AnyForYou` = any of the three. | Live | yes |
| `Rediscover` | `FOR_YOU_REDISCOVER` | bool | true / - / `true` | Made for you's Rediscover. | Live | yes |
| `DeepCuts` | `FOR_YOU_DEEP_CUTS` | bool | true / - / `true` | Made for you's Deep Cuts. | Live | yes |
| `NewReleaseWeeks` | `NEW_RELEASE_WEEKS` | int | 8 / - / `8` | `Clamp(1, 26)` | Live | yes |
| `NewReleaseArtists` | `NEW_RELEASE_ARTISTS` | int | 50 / - / `50` | `Clamp(10, 200)` | Live | yes |
| `RediscoverMonths` | `REDISCOVER_MONTHS` | int | 6 / - / `6` | `Clamp(3, 24)` | Live | yes |
| `PopularNow` | `POPULAR_NOW` | bool | true / - / `true` | "Popular right now", the chart as a playlist for every listener. GET lists it first in the section. | Live | yes |
| `Charts` | `CHARTS` | string (comma list) | `new,trending,18,14,6` / - / `new,trending,18,14,6` | The lists made into playlists besides Popular right now, in order: `new`, `trending`, Apple genre ids (`chart_settings::parse_charts`: lowercase, unknown ids and `34` dropped, no repeats, at most 14). Save refuses anything else. Rust only (charts plan); GET lists it after `PopularNow`. | Live | yes |
| `ChartCountry` | `CHART_COUNTRY` | string | `auto` / - / `auto` | `auto` or a two-letter Apple country; save refuses anything else. Rust only (charts plan). | Live | yes |
| `NameFormat` | `MIX_NAME_FORMAT` | string (.NET composite format) | `{0} Mix` / `{0} Mix` / **`""`** | `Name(label)`: if the format contains `{0}`, use `string.Format(InvariantCulture, format, label).Trim()`; on a `FormatException` (a stray brace) **or** when `{0}` is absent (including `""`), use `"{label} Mix"`. Compose turns an unset var into `""`, so GET shows `""` under Docker, but names still come out as `"{label} Mix"`. Rust needs a small .NET-format emulation: `{0}`, `{{`/`}}` escapes, alignment and format specifiers such as `{0,-10}`, and an error on any other index. | Live | yes |

### 3.13 `Notifications` → `NotificationSettings` (8)

| JSON key | .env var (→ `Notifications__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `NtfyUrl` | `NTFY_URL` | string | `""` / `""` / `""` | A non-blank value turns on the ntfy sink; it is parsed into server root + topic. | Live | yes |
| `NtfyToken` | `NTFY_TOKEN` | string | `""` / `""` / `""` | `Authorization: Bearer`. **Secret, in clear** in GET; masked in config-sources (ends in `Token`). | Live | yes |
| `DiscordWebhookUrl` | `DISCORD_WEBHOOK_URL` | string | `""` / `""` / `""` | A non-blank value turns on the Discord sink. **Secret** (it embeds a token): in clear in GET, masked in config-sources. | Live | yes |
| `NotifyDownloadStarted` | `NOTIFY_DOWNLOAD_STARTED` | bool | false / `false` / `false` | - | Live | yes |
| `NotifyDownloadCompleted` | `NOTIFY_DOWNLOAD_COMPLETED` | bool | true / `true` / `true` | - | Live | yes |
| `NotifyDownloadFailed` | `NOTIFY_DOWNLOAD_FAILED` | bool | true / `true` / `true` | - | Live | yes |
| `NotifyAlbumCompleted` | `NOTIFY_ALBUM_COMPLETED` | bool | true / `true` / `true` | - | Live | yes |

### 3.14 `ListenBrainz` → `ListenBrainzSettings` (3)

The section is absent from `appsettings.json` and compose. Raw env only.

| JSON key | .env var | Type | Class default | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `Token` | - (raw `ListenBrainz__Token`) | string | `""` | **Secret, in clear** in GET; masked in config-sources (ends in `Token`). Trimmed when used. | Live | yes |
| `UserTokens` | - (raw `ListenBrainz__UserTokens__alice`) | `Dictionary<string,string>` (OrdinalIgnoreCase) | `{}` | `TokenFor(user)`: returns null if `SubmitExternalPlays` is false. Otherwise the user's own token if the trimmed username matches and the token is non-blank (trimmed), else `Token` (trimmed) if non-blank, else null. **Secret, in clear** in GET and raw-config, and not listed in config-sources. **POST replaces this object whole** (`replaceObjects: ["ListenBrainz.UserTokens"]`). | Live | yes |
| `SubmitExternalPlays` | - | bool | true | - | Live | yes |

### 3.15 `RadioSources` → `RadioSourceSettings` (11, 2026.10.05)

Song radio (getSimilarSongs) and the stations ask every source that is on and blend their answers. GET settings and raw-config list the section after `YouTube`.

| JSON key | .env var (→ `RadioSources__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `YouTubeMusic` | `RADIO_YOUTUBE_MUSIC` | bool | true / `true` / `true` | YouTube Music's radio, through the shim. | Live | yes |
| `ListenBrainz` | `RADIO_LISTENBRAINZ` | bool | true / `true` / `true` | ListenBrainz's similar recordings, and LB Radio with a token. | Live | yes |
| `SoundsAlike` | `RADIO_SOUNDS_ALIKE` | bool | true / `true` / `true` | Library songs that sound like the seed (octo-sonic). Off stops reading how songs sound; octo-sonic still makes every song's transition profile, and the status page shows it running for transitions. | Live | yes |
| `SonicUrl` | `RADIO_SONIC_URL` | string | `http://octo-sonic:8080` (all three) | The octo-sonic sidecar; the status page probes `{SonicUrl}/health`. | Live | yes |
| `SonicPauseSeconds` | `RADIO_SONIC_PAUSE_SECONDS` | int | 2 / `2` / `2` | `Clamp(0, 600)`. | Live | yes |
| `ListenBrainzAlgorithm` | `RADIO_LISTENBRAINZ_ALGORITHM` (compose only) | string | `session_based_days_9000_session_300_contribution_5_threshold_15_limit_50_skip_30` (all three) | The labs similarity dataset. | Live | no |
| `ListenBrainzRadioMode` | `RADIO_LISTENBRAINZ_RADIO_MODE` | string | `easy` (all three) | LB Radio's mode for artist and genre stations: `easy`, `medium` or `hard`; any other value is `easy`. Part of the LB Radio cache key. | Live | yes |
| `LastFmWeight`, `YouTubeMusicWeight`, `ListenBrainzWeight`, `SoundsAlikeWeight` | - (dashboard only) | double | 0.9, 1.0, 0.7, 0.4 / same / - | `EffectiveWeight`: `Clamp(0, 3)`, and 1 for a value that is not a finite number. 0 leaves a source out of the blend. | Live | yes |
| `LearnFromListening` | `RADIO_LEARN_FROM_LISTENING` | bool | true / `true` / `true` | Each listener's plays and skips move the weights (0.3 to 1.5). | Live | yes |

The C# names start `YouTube` (capital T); the Rust fields are `you_tube_music` with a serde rename, and the binder matches them ignoring case.

### 3.16 `Admin` → `AdminSettings` (2, 2026.10.05)

Absent from `appsettings.json`. Neither GET settings nor raw-config lists the section (it is set in `.env`, and the port needs a restart), so a Raw config save never writes it.

| JSON key | .env var (→ `Admin__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `SignIn` | `ADMIN_SIGN_IN` | string | `required` / - / `required` | `SignInOff` is true only for `off` (trimmed, any case), so a typo never opens the dashboard. Start-up warns about any value other than `required` or `off`. Read live by the gate (`IOptionsMonitor`). | Live | no |
| `Port` | `ADMIN_PORT` | int | 0 / - / `0` | A port of its own for the dashboard and the admin API (§2). Read once at start (`builder.Configuration.GetValue<int>("Admin:Port")`): restart to apply. Ignored when it is already one of Octo's own ports. | Restart | no |

### 3.17 `Imports` → `ImportSettings` (4, 2026.10.05)

Absent from `appsettings.json`; read through `_config.GetSection("Imports").Get<ImportSettings>()` where the admin API shows it.

| JSON key | .env var (→ `Imports__Key`) | Type | Class / appsettings / Compose | Validation, parsing | Reload | Form |
|---|---|---|---|---|---|---|
| `SpotifyClientId` | `IMPORTS_SPOTIFY_CLIENT_ID` | string | `""` / - / `""` | Empty: no Spotify sign-in (links and files still work). Not a secret (PKCE). | Live | yes |
| `SpotifyRedirectUri` | `IMPORTS_SPOTIFY_REDIRECT_URI` | string | `http://127.0.0.1/callback` / - / same | `EffectiveRedirectUri`: the default when blank, else trimmed. GET shows the effective value. | Live | yes |
| `SongsPerHour` | `IMPORTS_SONGS_PER_HOUR` | int | 20 / - / `20` | `EffectiveSongsPerHour` = `Clamp(0, 600)`; 0 stops the trickle. | Live | yes |
| `RefreshHours` | `IMPORTS_REFRESH_HOURS` | int | 6 / - / `6` | 0 reads lists again only when asked. | Live | yes |
| `KeepInNavidrome` | `IMPORTS_KEEP_IN_NAVIDROME` | bool | false / - / `false` | Off: "Keep as playlist" makes a playlist Octo keeps, whole from the start, missing songs playable from outside the library until their files arrive. On: a Navidrome playlist of the songs the library has, as before. A list already kept one way stays that way. | Live | yes |

### 3.18 Variables for other containers (2026.10.05)

Compose and the installer pass these to octo-sonic or the starter Navidrome, or use them themselves. Octo does not read them: `SONIC_CPUS` (octo-sonic), `COMPOSE_PROFILES`, `NAVIDROME_ADMIN_PASSWORD`, `NAVIDROME_PORT`, `NAVIDROME_DATA_DIR` (the starter stack), `OCTO_IMAGE_TAG` (the image compose pulls). One raw key was added for tests: `Octo:StateDirectory`, where the radio state files go instead of beside settings.json (the C# tests' only way to move them; Rust tests pass their own paths).

---

## 4. `SettingsFileWriter`: the exact write format

`Octo.Services.Admin.SettingsFileWriter` is a singleton constructed with `/app/config/settings.json`. One `lock` serializes every read-modify-write: `Merge`, `Update`, `Replace` and `IsReadable`.

**Reading (`Load` for display, `ReadForWrite` for writes)**

- `JsonNode.Parse` with `CommentHandling.Skip` and `AllowTrailingCommas` (the same leniency as the config provider). Comments are **dropped** on the next save.
- A missing file, or one that is empty or whitespace only → `{}`.
- If the content is not valid JSON **or is not an object** (for example `[]` or `3`): `Load` returns `{}`, and `ReadForWrite` throws `SettingsFileCorruptException`. `POST /api/admin/settings` then answers **409** `{"error":"settings.json is not valid JSON, so Octo will not write over it. Fix it in Raw config, or on disk at /app/config/settings.json."}`. `IsReadable()` returns false, which surfaces as `_meta.ConfigFileValid`.
- `PUT raw-config` does **not** read before writing (`Replace`), so it is the recovery path.

**Merge semantics (`Merge(patch, replaceObjects)`)**

1. For each `"Section.Key"` in `replaceObjects` (split on the first `.`): when `patch[Section][Key]` is an object **and** the file has `Section` as an object, `current[Section].Remove(Key)`. This is used only for `ListenBrainz.UserTokens`.
2. `DeepMerge(current, patch)`: for each property of the patch, if both sides are objects, recurse; otherwise **replace wholesale** with a deep clone. That covers primitives, arrays, `null`, and an object over a non-object. Arrays are never merged element by element. **A JSON `null` in the patch is written as `null`** (see §1 for what null does on load).
3. **Key matching is case-sensitive** (`JsonObject` default). A patch key `subsonic` next to an existing `Subsonic` creates a second property. The config provider then rejects the file as a case-insensitive duplicate. Only the secret handling in `AdminController` matches keys case-insensitively (`KeyOf`/`Child`). For Rust, the recommendation is to match section and key case-insensitively in the merge, preserving the existing spelling. That is strictly safer, and no well-formed client can tell the difference.
4. **Order:** a replaced key keeps its position; a new key is appended at the end of its object; a key removed by `replaceObjects` is re-added at the end.
5. Merge returns the merged document. The POST echoes it as `{"ok":true,"persisted":<merged with secrets masked>}`.

**`Update(change)`**: read, run the callback, and write only if it returned true. This is used by the Last.fm session Connect, Disconnect and refusal handling.

**`Replace(content)`**: writes `content` as is, with no merge.

**Write (`Write`)**

```text
Directory.CreateDirectory(dirname(path))
File.WriteAllText(path + ".tmp", content.ToJsonString(new JsonSerializerOptions { WriteIndented = true }))
File.Move(path + ".tmp", path, overwrite: true)      // rename(2): atomic on one filesystem, no fsync
```

- The temp file is `/app/config/settings.json.tmp`, a fixed name, so there is no uniqueness and the lock is required.
- **Encoding:** UTF-8 **without BOM**, **no trailing newline**.
- **Indentation:** 2 spaces; the line break is `\n` (`Environment.NewLine` on Linux); `"key": value` with one space after the colon. An empty object is `{}` and an empty array is `[]`. Every array element goes on its own line.
- **Key casing:** exactly as received. The admin API and dashboard use PascalCase (`Subsonic.Url`); `System.Text.Json` applies no naming policy to `JsonObject`.
- **Numbers:** written with their original token text, so `30`, `1.50` and `-16` are kept verbatim from the request body or file.
- **Strings:** escaped with `JavaScriptEncoder.Default` (the writer's default):
  - every non-ASCII character becomes `\uXXXX` with uppercase hex, and astral characters become surrogate pairs (`🛠` → `🛠`, `▸` → `▸`, `é` → `é`).
  - the HTML-sensitive characters are escaped too: `"` → `"`, `&` → `&`, `'` → `'`, `+` → `+`, `<` → `<`, `>` → `>`, `` ` `` → ```.
  - `\` → `\\`, and `\n \r \t \b \f` → short escapes; any other control character → `\u00XX`.
  - The escaping is **normalized** on every write, even for values that were only read from the file.
  - Rust's `serde_json` escapes differently, so a custom formatter is needed. It is a small `serde_json::ser::Formatter` impl that overrides `write_string_fragment` / `write_char_escape`, plus `PrettyFormatter::with_indent(b"  ")`. Before porting, pin the behavior with a golden test in `SettingsFileWriterTests`.

**Sample:** the output after `POST /api/admin/settings` with `{"Subsonic":{"Url":"http://nd:4533","AdminPassword":"p&ss+w'rd"}}` over a file that already held `LibraryActions` and a Last.fm session:

```json
{
  "LibraryActions": {
    "Enabled": true,
    "PlaylistPrefix": "🛠 ",
    "NoticePrefix": "▸ ",
    "AllowedUsers": [
      "alice"
    ],
    "Actions": []
  },
  "LastFm": {
    "UserSessions": {
      "alice": {
        "SessionKey": "0123456789abcdef",
        "LastFmUser": "alice_fm"
      }
    }
  },
  "Subsonic": {
    "Url": "http://nd:4533",
    "AdminPassword": "p&ss+w'rd"
  }
}
```

---

## 5. Admin endpoints and other writers

| Endpoint / code path | Effect on settings.json | Validation and secret handling |
|---|---|---|
| `GET /api/admin/settings` | - | Returns the **effective** values (§3), plus `_meta`: `ConfigFilePath`, `ConfigFileExists`, `ConfigFileValid`, `RestartPending` (array of `"Section:Key"`), `SecretPlaceholder` = `"(saved, not shown)"`, `Version` (InformationalVersion with the `+sha` cut off). Keys are PascalCase (a `Dictionary`, so no camelCase), and enums are returned as names. Leaving masking aside, three fields differ from the raw stored value: `HeartDownloadSources` and `LibraryActions.Actions` are the effective lists, and `Metadata.SaveLyricsTo` is normalized. `LastFm.DiscoveryStations` and `Genre.Mappings` come from the raw file when present. |
| `POST /api/admin/settings` | `Merge(patch, ["ListenBrainz.UserTokens"])` | Body must be a JSON object, otherwise 400 `empty body` / `invalid JSON: …`. `_meta` is removed. Validates `LibraryActions`, `Genre.Mappings` and `LastFm.DiscoveryStations` (§3), with a 400 on failure. Since 2026.10.05, in this order: a saved credential never follows a new address (§6, `SecretSentElsewhere`); the admin password as in §3; `LastFm.UserSessions` is always removed; then every other placeholder is taken out (`KeepSavedSecrets`, §6). Corrupt file → 409; any other exception → 500 `{error}`. Logs `{Who} saved settings: {Section,Section}` (who: the Navidrome user, "someone with the recovery code", "someone (the dashboard sign-in is off)", "{user} (from the Octo app)"). Answers `{"ok":true,"persisted":<merged, every credential masked>}`. |
| `GET /api/admin/raw-config` | - | The effective document as indented JSON (the same masking as GET settings). It always includes every section, so a plain Save does not delete any. Differences from GET settings: `DiscoveryStations` is always the class serialized (never the raw file); `AllowedUsers`, `Blocklist` and `BackfillExtensions` are serialized arrays. |
| `PUT /api/admin/raw-config` | `Replace(parsed)`, wholesale | Body must be a JSON object. Since 2026.10.05 `SecretSentElsewhere` runs first (§6). Placeholders are swapped back from the existing file (every credential of §6, each listener's ListenBrainz token by name); if the file is unreadable, the running values of every credential are used instead (`RunningSecrets`). Logs `{Who} saved the raw config ({Bytes} bytes)`. A placeholder with no stored value → the key is dropped, so env keeps applying. A session whose key exists only in env is dropped whole. Placeholder plus extra text → 400 (admin password, shared secret, or session key: `Connect {user} to Last.fm again…`). Answers `{"ok":true,"bytes":N}`. **No LibraryActions, Genre or DiscoveryStations validation on this path.** |
| `GET /api/admin/config-sources` | - | `{keys:[{Key,Value,IsSecret}], configFile}` for a fixed list of `Section:Key` strings, read through `IConfiguration[key]`. Despite the name, it does **not** report which source a value came from. Lists come out as `""`, because the section has no scalar value. A value counts as secret when its key ends in `Password`, `ApiKey`, `Secret`, `Token` or `WebhookUrl` (case-insensitive); a non-empty secret shows as `'•'` repeated `min(len, 16)` times. |
| `POST /api/admin/restart` | - | 202 `{ok:true,message:"restarting"}`. After 1 s it calls `StopApplication()`; 2 s later it calls `Environment.Exit(1)`. |
| `POST /api/admin/lastfm/scrobble/finish` (`LastFmScrobbleService.FinishConnect`) | `Update`: finds the `LastFm` section case-insensitively, creating `"LastFm"` if missing, and `UserSessions` the same way. Removes every key equal to the user (trimmed, case-insensitive) and sets `UserSessions[user] = {"SessionKey":…, "LastFmUser":…}`. | - |
| `POST /api/admin/lastfm/scrobble/disconnect`, and an automatic removal after Last.fm refuses a session (error 9, after a one-hour retry) | `Update`: removes the user's session entries (only when the stored key matches, in the refusal case). Nothing is written when nothing matched. | When the session exists only in env, the disconnect answers with a message naming `LASTFM__USERSESSIONS__{user}__SESSIONKEY`. |
| First-run auto-detect (`Program.cs`, a background task at startup) | When `Subsonic.Url` is blank and the LAN scan finds exactly one server: `Merge({"Subsonic":{"Url":"<found>"}})`, then `StopApplication()`. | - |

No other code path writes `settings.json`. `LyricsAdminController`, `UpdateController` and `CoverUpgradeController` only read settings.

---

## 6. Secrets: redaction contract

> [!CORRECTION] 2026.10.05
> Every credential now reads as the placeholder (031e563); nothing in the table below goes out in clear any more. The lists the C# keeps (`SecretFields`, `SecretAddresses`, `SecretMaps`) are `controllers::admin::secrets` in Rust, and a test walks the settings structs so a new credential cannot go out in clear unnoticed.

| Setting | GET settings / raw-config | POST echo (`persisted`) | config-sources | Logs |
|---|---|---|---|---|
| `Subsonic.AdminPassword` | placeholder `(saved, not shown)`, or `""` if unset | masked | masked | never logged |
| `LastFm.ApiSecret` | placeholder | masked | masked | `api_sig` query param redacted |
| `LastFm.UserSessions.*.SessionKey` | placeholder; entries with a blank key are omitted | masked | (not listed) | `sk` query param redacted |
| `Downloads.AcoustIdApiKey`, `Downloads.AcoustIdUserApiKey` | placeholder | masked | masked | `client`, `user` query params redacted |
| `Lidarr.ApiKey` | placeholder | masked | masked | - (sent as a header) |
| `LastFm.ApiKey` | placeholder | masked | masked | `api_key` redacted |
| `Notifications.NtfyToken`, `Notifications.DiscordWebhookUrl` | placeholder (the webhook address carries its own token) | masked | masked | - |
| `ListenBrainz.Token`, `ListenBrainz.UserTokens` | placeholder; a blank user token is left out, names kept | masked | Token masked; UserTokens not listed | - |

- **Saving it back keeps what is saved** (`KeepSavedSecrets`): a field that reads exactly as the placeholder is taken out of the patch; the placeholder with text after it is refused ("Retype the whole {words}; it was added to the hidden placeholder."); a listener's ListenBrainz token sent as the placeholder is replaced by the stored one (the map is saved whole), or dropped when none is stored.
- **A saved credential never follows a new address** (`SecretSentElsewhere`, 9210c63): for `Subsonic.Url`/`AdminPassword`, `Lidarr.BaseUrl`/`ApiKey` and `Notifications.NtfyUrl`/`NtfyToken`, a patch that changes the address (compared trimmed, without trailing slashes, ignoring case, against the running `IConfiguration` value) while a credential is stored and the patch does not type a new one is refused: "Retype the {words} for the new address."
- **The checks that send a credential somewhere take the placeholder as the saved one:** `lastfm/check` (shared secret), `listenbrainz/validate` (token), `lidarr/test` (API key). `lidarr/test` sends the saved key to whatever address the request names (BACKLOG, "Found while building the harness"; kept as C# does it, Brandon to decide).
- **Log redaction** (`LogRedaction.cs`, wrapping the whole `ILoggerFactory`, so it applies to request lines, HttpClient lines and app logs, at every level):
  - Regex `(?<=[?&;](?:t|s|p|apikey|token|api_key|client|user|sk|api_sig)=)[^&#\s"'<>]+`, case-insensitive, replaced with `***`.
  - It applies to the message text and to every structured argument: strings, and anything whose `ToString()` is a URL or query. Enums, dates, numbers, `Guid` and `TimeSpan` pass through.
  - Subsonic's `u` is **not** redacted.
  - Rust: a `tracing` layer or formatter that applies the same regex to the message and the fields.

---

## 7. Fixed values referenced by settings (not configurable)

- `GenreSettings.BuiltInBlocklist`: music, people & blogs, people and blogs, gaming, entertainment, education, news & politics, science & technology, howto & style, film & animation, autos & vehicles, pets & animals, sports, travel & events, comedy, nonprofits & activism, shows, trailers, unknown, other, misc, miscellaneous, genre, audio, soundtrack music, youtube, lossless, flac, mp3, 320kbps, cd, vinyl, album.
- `MetadataSettings.KnownLyricsSources` = `song, kugou, lrclib, netease, lyricsovh`. `EmbeddedCoverSide` = 1500.
- `LibraryActionSettings` default names and ratings: Delete 1, Wrong song 2, Wrong version 3, Better quality 4, Keep 5.
- `GenreSettings.BroadGenrePreset()`: the ordered pattern → genre table served by `GET /api/admin/genre/presets` (copy it verbatim from `GenreSettings.cs`).
- HTTP client timeouts are hard-coded in `Program.cs` (ListenBrainz and Last.fm scrobble 10 s, yt-dlp search 60 s and stream infinite, release check 15 s, MusicBrainz 10 s, AcoustID 10 s, notifications 10 s, Cover Art Archive 8 s, LRCLIB and NetEase 8 s, KuGou 6 s). The host shutdown timeout is 10 s. None of these is configurable.

---

## 8. Surprises to carry into the port

1. **`settings.json` outranks env vars.** A dashboard save, and above all a Raw config save, permanently shadows `.env`.
2. **A JSON `null` (which a cleared number field produces) shadows lower sources** as `""`, and makes the whole section fail to bind for int, bool and enum keys.
3. **Enums parse case-insensitively and also take numbers**, including undefined values and comma flag-ORs. **Ints take hex** (`0x`, `&h`, `#`). Bools take only `true`/`false`, in any case.
4. **Class defaults disagree with `appsettings.json`:** `StorageMode` (Permanent vs `Stream`), `EnableExternalPlaylists` (true vs `false`), `LyricsSources` (`song,…` vs no `song`). **Compose disagrees with appsettings:** `GeneratedPlaylists.NameFormat` (`""` vs `{0} Mix`). The load order must be reproduced exactly, and the C# class defaults matter for keys absent from appsettings (all of `ListenBrainz` and `Server`, several `LastFm`, `Subsonic` and `Metadata` keys).
5. **`SyncCatalogClients` matches by substring**, not by equal name.
6. **`Metadata.Language` and `LastFm.ApiKey` partly need a restart** (Last.fm and cover-lookup clients). Neither is flagged in the dashboard.
7. **Dead settings:** `Subsonic.UseLocalStaging` (no reader) and `Subsonic.PlaylistsDirectory` (its reader is never registered). Keep them in the schema, the GET output and the form; nothing has to act on them.
8. **The `SettingsFileWriter` merge is case-sensitive**, while the config provider is case-insensitive and rejects case-duplicates.
9. **Non-ASCII and HTML characters are `\u`-escaped** on every write, which `serde_json` does not do by default.
10. **`GET /settings` falls back to serializing the class** for `Genre.Mappings` when it is not in the file, which turns `Match` into a number. With the shipped config this cannot happen, because an env-only mappings table is the only way to reach it.
