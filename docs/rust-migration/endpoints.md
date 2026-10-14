# Endpoint inventory (C# baseline)

This is the route contract that the Rust rewrite and the parity harness work against. It was written by reading the code at `15d6840` (release 2026.10.03.2):

- `octo/Controllers/SubSonicController.cs`
- `octo/Controllers/AdminController.cs`
- `octo/Controllers/CoverUpgradeController.cs`
- `octo/Controllers/LyricsAdminController.cs`
- `octo/Controllers/UpdateController.cs`
- `octo/Program.cs`
- `octo/Middleware/AdminRequestGuard.cs`
- `octo/Middleware/GlobalExceptionHandler.cs`
- the helpers those files call (`SubsonicRequestParser`, `SubsonicProxyService`, `SubsonicResponseBuilder*`, `CredentialCheck`, `RequestIdentity`, `LocalLibraryService.ParseExternalId`, `PlaylistIdHelper`)

Line numbers point at the action method's declaration. Items marked **(verify)** come from reading the code and how ASP.NET Core behaves, not from a captured response. The harness corpus must confirm them before they count as part of the contract.

---

## 1. Summary

| Controller / source | Actions | Route templates | Distinct endpoints | Notes |
|---|---:|---:|---:|---|
| `SubsonicController` (`[Route("")]`) | 33 | 80 | 39 Subsonic endpoints + 1 radio stream + 1 catch-all | 78 `rest/*` templates. Every Subsonic endpoint has both `rest/x` and `rest/x.view`. 31 actions take GET and POST. `radio/stream/{token}` takes GET and HEAD. The catch-all `{**endpoint}` takes **every** method. |
| `AdminController` (`[Route("api/admin")]`) | 53 | 53 | 53 | 52 under `/api/admin/*`, plus the absolute `GET /admin` redirect |
| `CoverUpgradeController` (`[Route("api/admin/covers/upgrade")]`) | 6 | 6 | 6 | all gated on a browse session |
| `LyricsAdminController` (`[Route("api/admin/lyrics")]`) | 9 | 9 | 9 | all gated on a browse session |
| `UpdateController` (`[Route("api/admin/update")]`) | 3 | 3 | 3 | |
| `Program.cs` minimal APIs | 0 | 0 | 0 | No `MapGet`/`MapPost`. Static files come from `MapStaticAssets()`, `UseDefaultFiles()`, `UseStaticFiles()` and `UseStaticFiles(/Assets)`. Swagger is mapped only in Development. |
| `Middleware/` | n/a | n/a | n/a | `AdminRequestGuard` (any `/api/admin*` path) and `GlobalExceptionHandler` (all routes) |
| **Total** | **104** | **151** | | |

How the Subsonic routes split across the groups in section 3:

| Group | Endpoints (each also has a `.view` alias) |
|---|---|
| System / auth | `ping`, `getOpenSubsonicExtensions`, `jukeboxControl` |
| Browsing | `getSong`, `getArtist`, `getAlbum`, `getAlbumInfo`, `getAlbumInfo2`, `getArtistInfo`, `getArtistInfo2`, `getRandomSongs`, `getSimilarSongs`, `getSimilarSongs2` |
| Search | `search2`, `search3` |
| Media | `stream`, `getCoverArt`, `getTranscodeDecision`, plus `/radio/stream/{token}` (not under `rest/`) |
| Playlists and internet radio | `getPlaylists`, `getPlaylist`, `createPlaylist`, `updatePlaylist`, `deletePlaylist`, `getInternetRadioStations`, `createInternetRadioStation`, `updateInternetRadioStation`, `deleteInternetRadioStation` |
| Stars, ratings, scrobble | `star`, `setRating`, `scrobble`, `reportPlayback` |
| Lyrics | `getLyricsBySongId`, `getLyrics`, `getLyricsCandidates`\*, `setLyricsChoice`\* |
| Octo-specific extensions | `getAcquisitions`, `getLibraryActions`, `getUpgrades`, `libraryAction` (\*the octoLyrics pair above are Octo extensions too) |
| Catch-all | `/{**endpoint}`: every other Subsonic call (`getArtists`, `getAlbumList2`, `unstar`, `download`, ...), the Navidrome native API (`/api/*`, `/auth/*`), and the root `/` |

---

## 2. Cross-cutting behavior (applies to every route below)

### 2.1 Pipeline order (`Program.cs` L550-607)

1. `UseForwardedHeaders()`. Only `X-Forwarded-Proto` is trusted (`ForwardLimit = 1`, known networks and proxies cleared). Host and client IP are not rewritten. The scheme feeds the absolute `streamUrl` built in `getInternetRadioStations` and the `ping` "not configured" message.
2. `UseExceptionHandler(_ => {})` plus `GlobalExceptionHandler` (see 2.8).
3. `UseAdminRequestGuard()`, for any path that starts with the segment `/api/admin` (see 2.9). It is registered before CORS on purpose, so its `OnStarting` callback runs after CORS and strips every `Access-Control-*` header from `/api/admin*` responses.
4. Raw-body capture (L559-571). For **every** POST, PUT and PATCH request, on every path, the body is buffered into `HttpContext.Items["Octo.RawBody"]` and then rewound. The faithful relay forwards this copy.
5. Swagger and Swagger UI, in the Development environment only (`/swagger`, `/swagger/v1/swagger.json`).
6. `MapStaticAssets()`, then `UseDefaultFiles()`, then `UseStaticFiles()`, then `UseStaticFiles(RequestPath="/Assets", FileProvider=<AppContext.BaseDirectory>/Assets)` when that directory exists. See section 5.
7. `UseAuthorization()`. No policies are configured.
8. `UseCors()` with the default policy: `AllowAnyOrigin`, `AllowAnyMethod`, `AllowAnyHeader`, and exposed headers `X-Content-Duration`, `X-Total-Count`, `X-Nd-Authorization`. A CORS preflight (OPTIONS with `Access-Control-Request-Method`) gets its answer from this middleware, except on `/api/admin*`, where the guard answers it first.
9. `MapControllers()`. Endpoint routing runs implicitly at the start of the pipeline (WebApplication default), so a route is matched before the static-file middleware runs. A static-file middleware skips any request that already matched an endpoint with a delegate, and that includes the catch-all. In practice, only `MapStaticAssets` endpoints can serve files. **(verify)**

No HTTPS redirection runs. No response compression or caching middleware runs.

### 2.2 Routing semantics a reimplementation must copy

- **Literal segments match case-insensitively.** `/REST/Ping.VIEW` reaches `Ping`. A trailing slash is ignored (`/rest/ping/` matches). **(verify)**
- **A method mismatch falls through to the catch-all; it never produces a 405.** `{**endpoint}` has no method constraint. A `HEAD /rest/stream`, `PUT /rest/ping` or `DELETE /rest/getAlbum` therefore matches the catch-all: Octo returns a synthetic empty "ok" when an id parameter is external, and otherwise does a faithful relay with the original method. This is also true of admin GET routes: `HEAD /api/admin/settings` reaches the catch-all, which returns 404 because the path is Octo-owned. **(verify)**
- The catch-all only matches when nothing more specific does. Literal routes and `radio/stream/{token:length(48)}` win. A token that is not exactly 48 characters falls through to the catch-all and is relayed to Navidrome.
- `GET /` matches the catch-all with no `endpoint` value. `endpoint` is a non-nullable `string` on an `[ApiController]`, so the implicit `[Required]` probably produces an automatic `400 application/problem+json` (`"The endpoint field is required."`) before the action runs. If it does not, `TryServeNativeRadioAsync` throws a `NullReferenceException`, which becomes a 500 JSON body. Either way, `/` is **not** relayed. **(verify)**
  Since 2026.10.05 (`4a329aa`, #80): `endpoint` is nullable, and a `GET` or `HEAD /` is a `302` to `app/` (relative, Navidrome's web app); any other method goes on with an empty endpoint and is relayed.

### 2.3 Subsonic parameter extraction (`SubsonicRequestParser.ExtractAllParametersAsync`)

Every Subsonic action, including the catch-all, builds one `Dictionary<string,string>` like this:

1. **Query string** first. A repeated key is joined with commas (`id=A&id=B` becomes `"A,B"`). Keys are **case-sensitive** (ordinal), so `F=json` is not `f`.
2. **Body** next, read only when `Content-Length > 0` or a `Content-Type` is present. Body values **override** query values.
   - `application/x-www-form-urlencoded` or `multipart/form-data` (`HasFormContentType`): every field, with repeats comma-joined. If `ReadFormAsync` throws, the raw body is parsed as a query string instead.
   - Any content type containing `application/json`: a top-level object. Each value becomes `JsonElement.ToString()`, so strings arrive unquoted, numbers and nested objects arrive as raw JSON text, and booleans arrive as `True`/`False`. Malformed JSON is ignored.
3. `scrobble` additionally reads `id`, `submission` and `time` as **lists**, from both the query and the form (`ExtractParameterValuesAsync`), skipping blank values.

When the dictionary is relayed to Navidrome (`BuildQueryAsync` / `RestoreRepeatedParameters`), a value that is still exactly what the client sent goes out again as the client's separate repeated values, in their original order. A value a handler changed or added goes out once. Everything is sent as **query parameters on a GET** (except the faithful relay, 2.7). When the body is forwarded and is url-encoded, unchanged form fields are left out of the query.

### 2.4 How the response format is chosen

- The format comes **only from the `f` parameter**, defaulting to `"xml"`. `Accept` is never consulted.
- Builders that compare **case-sensitively** (`format == "json"`): `CreateResponse`, `CreateError`, `CreateSongResponse`, `CreateInfoResponse`, `CreateMergedResponse`, `CreateJsonResponse` callers in `MergeSearchResults`, `BuildSimilarSongsResponse`, `BuildRandomSongsResponse` and `RelayAsAskedAsync`.
- Places that compare **case-insensitively**: `IsSuccessfulSubsonicResponse`, the `getPlaylists` and `getInternetRadioStations` merges, `MergeOpenSubsonicExtensions`, `CreateLyricsListResponse`, `CreateLyricsResponse`, `HasNoLegacyLyrics`, and the JSON branch of `getLyricsBySongId`.
- As a result, `f=JSON` gives mixed results, and **any value other than `json` (for example `jsonp`) produces XML** from every Octo-built response.
- `search2`/`search3` also answer in JSON when the **upstream** content type contains `json`, even if `f` asked for something else.
- These endpoints **always answer JSON, whatever `f` says**: `getAcquisitions`, `getLibraryActions`, `getUpgrades`, `libraryAction`, `getLyricsCandidates`, `setLyricsChoice`, and the external-id branch of `getTranscodeDecision`.

### 2.5 Envelopes Octo builds itself

- `version` is always `"1.16.1"`. Octo-built envelopes carry no `type`, `serverVersion` or `openSubsonic`. The exceptions add `"type":"octo"`: `getAcquisitions`, the fallback list in `getOpenSubsonicExtensions`, and the octoLibraryActions and octoLyrics replies.
- **XML** uses namespace `http://subsonic.org/restapi`. The root `<subsonic-response status=".." version="1.16.1">` is written by `XDocument.ToString()`, which means **pretty-printed with two-space indents and no `<?xml?>` declaration**. Content type: `application/xml`, no charset (`ContentResult`). The `getPlaylists` and `getInternetRadioStations` merges re-serialize Navidrome's XML the same way (`File(..., "application/xml")`).
- **JSON** is `{"subsonic-response":{...}}`, written by `JsonResult`. That means compact output, content type `application/json; charset=utf-8`, and the web defaults: camelCase for anonymous-type members, dictionary keys left as they are, nulls written, and enums written as numbers unless the code calls `.ToString()`.
- **The empty "ok" differs by format.** `CreateResponse(format, element, {})` in **JSON omits the element entirely** (`{"subsonic-response":{"status":"ok","version":"1.16.1"}}`), while XML includes an empty `<element/>`. Every "synthetic ok" in this document behaves this way.
- Errors: `{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":N,"message":"..."}}}`, or `<error code="N" message="..."/>`. Subsonic errors always go out with **HTTP 200**.
- The field-level song, album and artist shapes (`ConvertSongFields`, `BuildAlbumFields`, `JsonShapeToXml`, and the lyrics, acquisitions and library-actions JSON) live in `SubsonicResponseBuilder*.cs`. They are part of this contract but are not repeated here.

### 2.6 JSONP

**Octo does not implement JSONP.** Nothing reads `callback` and no response is wrapped. When Octo builds the response, `f=jsonp` produces XML. When the request is relayed verbatim (passthrough endpoints, the catch-all, empty-query search, local `getSong`, and so on), `f` and `callback` go to Navidrome unchanged, so the client gets **Navidrome's** JSONP.

Mixed cases:
- `getAlbum` and `getArtist` with local ids always ask Navidrome for JSON, then answer with `CreateMergedResponse` in XML when `f=jsonp`.
- `getPlaylists` and `getInternetRadioStations` with `f=jsonp` cannot parse the body as XML. The success check fails, so the upstream body passes through unchanged, without Octo's rows.

### 2.7 Relay mechanics (`SubsonicProxyService`)

| Method | Upstream request | Failure behavior | Used by |
|---|---|---|---|
| `RelayAsync(endpoint, params)` | `GET {Url.TrimEnd('/')}/{endpoint}?{query}` with no forwarded headers and no body | Throws `OctoNotConfiguredException` when the URL is missing or invalid. A non-2xx answer throws `HttpRequestException` (`EnsureSuccessStatusCode`). | most passthroughs |
| `RelaySafeAsync` | same | Any exception becomes `(null, null, false)` | most "safe" passthroughs |
| `RelayRawAsync` (faithful relay) | The client's own **method**, plus the captured raw body with its `Content-Type`. Forwarded request headers: `Authorization`, `X-Nd-Client-Unique-Id`, `X-Nd-Authorization`, `Accept`, `User-Agent`, `If-None-Match`, `If-Modified-Since`, `Range`. | The upstream **status is returned verbatim**. Forwarded response headers (allowlist): `X-Nd-Authorization`, `ETag`, `Last-Modified`, `Cache-Control`, `Content-Range`, `Accept-Ranges`, `Vary`, `X-Total-Count`, `Access-Control-Expose-Headers`, and since 2026.10.05 `Content-Disposition` (`0a700c0`). The body is **fully buffered**, not streamed; since 2026.10.05 (`67f3c93`) a body whose length may be Navidrome's estimate (`LengthMayBeEstimate`, below) is read to its real end, and ending short of the announced length is not a failure. | catch-all, native `/api/*` |
| `RelayStreamAsync` | `GET {Url}/rest/stream?{query}` (note: `Url` is **not** trimmed). Forwards `Range` and `If-Range`. | Upstream non-2xx becomes a bare `StatusCodeResult(status)`, which `[ApiController]` turns into ProblemDetails (2.8); since 2026.10.05 a `416` also carries Navidrome's `Content-Range` (`bytes */N`). An exception becomes `500 {"error":"Error streaming from Subsonic: ..."}`. Since 2026.10.05, when the length may be an estimate, `Content-Length` is left out (the body goes chunked) and an early end is the end of the song. | local `stream` |

`LengthMayBeEstimate` (2026.10.05): never for a `206`; `Accept-Ranges: bytes` says exact and `Accept-Ranges: none` says estimate; with neither, the request decides: `estimateContentLength=true`, a `format` other than `raw`, or a `maxBitRate` above 0 (parameter names in any case).

Passthrough results are written back as `File(body, upstreamContentType ?? "application/{f}")`, which always means **HTTP 200**. Upstream errors arrive inside a 200 Subsonic envelope anyway, because a non-2xx would already have thrown.

### 2.8 Errors outside the Subsonic envelope

**GlobalExceptionHandler.** An unhandled exception becomes `Content-Type: application/json` with `{"subsonic-response":{"status":"failed","version":"1.16.1","error":{code,message}}}`. **It is always JSON, whatever `f` says**, and it applies to admin routes too.

| Exception | HTTP | Subsonic code | Message |
|---|---|---|---|
| `OctoNotConfiguredException` | 503 | 0 | the exception's own message (tells the user to set `SUBSONIC_URL`) |
| `FileNotFoundException` | 404 | 70 | `Resource not found` |
| `DirectoryNotFoundException` | 404 | 70 | `Directory not found` |
| `UnauthorizedAccessException` | 401 | 40 | `Wrong username or password` |
| `ArgumentNullException` | 400 | 10 | `Required parameter is missing` |
| `ArgumentException` | 400 | 10 | `Invalid request` |
| `FormatException` | 400 | 10 | `Invalid format` |
| `InvalidOperationException` | 400 | 10 | `Operation not valid` |
| `HttpRequestException` | 502 | 0 | `External service unavailable` |
| `TimeoutException` | 504 | 0 | `Request timeout` |
| anything else | 500 | 0 | `An internal server error occurred` |

The same ordering applies to subclasses (C# `switch` order): `ArgumentNullException` is matched before `ArgumentException`.

Routes that let a relay exception escape to this handler:
- `getRandomSongs` (all cases)
- local `getSong`
- `star` and `setRating` when the URL is unconfigured: they catch only `HttpRequestException`, so the exception produces a 503 JSON body
- `scrobble` (same reason)
- `getTranscodeDecision` with a local id and no URL configured
- metadata-service exceptions in any external-id branch

**`[ApiController]` conversions.** Every controller carries this attribute. **(verify)**
- A bodiless client-error result (`NotFound()`, `BadRequest()`, `Unauthorized()`, or a `StatusCodeResult` with status 400 or above) is rewritten into `application/problem+json` (`{type,title,status,traceId}`). This covers `getCoverArt` 404s, the catch-all's Octo-owned 404, `RelayStreamAsync` upstream errors, and the admin `NotFound()`s.
- A result built with an object body (`BadRequest(new{error})`, and so on) is left as it is.
- Model-binding and validation failures become an automatic `400 application/problem+json` (`ValidationProblemDetails` with `errors`) before the action runs. This includes empty or invalid JSON on a `[FromBody]`, a bad `bool` in the query, and **implicit `[Required]`** on non-nullable reference types. `<Nullable>enable</Nullable>` is set, so these are required:
  - `DELETE lastfm/radio/history?user=`
  - `RadioUserRequest.User`
  - `LastFmScrobbleUserRequest.User` (an empty string fails too)
  - `ReviewDismissRequest.Path`
  - `ChoiceRequest.Candidate`
  - the catch-all's `endpoint`

Admin JSON follows the web defaults: camelCase property names, enums as numbers unless `.ToString()`'d. Several endpoints build `Dictionary<string,object>` or `JsonObject` on purpose so that keys stay **PascalCase**: `settings` GET, `raw-config` GET, `genre/presets`, and the rows inside `config-sources`.

### 2.9 Authentication

**Subsonic.** Octo has no credential store and never checks a password itself. It relies on four mechanisms:

| Mechanism | How | Where |
|---|---|---|
| **R** Relay-gated | The request is relayed with the caller's `u`/`t`/`s`/`p`/`apiKey`, and Navidrome's answer, including a `failed` envelope, goes back to the client. | every passthrough |
| **P** Ping check | `rest/ping` relayed with the caller's parameters (`f` forced to `json` in `CheckCallerAsync`/`getAcquisitions`, left as the client's in `getPlaylist`). Unreachable gives code 0 `Octo can't reach Navidrome to check who is asking`. Rejected gives code 40 `Wrong username or password`. | octo* extensions, `getAcquisitions`, station and mix `getPlaylist`, the first mix cover draw |
| **C** CredentialCheck | `SubsonicCredential.From` reads the keys `u,t,s,p,apiKey,jwt`, and returns null when only `u` is present (null means refused). The ping is relayed with `f=json` and a 5 s timeout. Verdicts are cached by credential fingerprint: accepted for 10 min, refused for 30 s, unreachable not cached. | external-id `stream`, external-id or playlist or album `star` |
| **N** None | Answered locally with no check | external-id `getSong`, `getAlbum`, `getArtist`, `getAlbumInfo*`, `getArtistInfo*`, every `getCoverArt` branch except local, external `getTranscodeDecision`, external `reportPlayback`, external `setRating`, `jukeboxControl`, the catch-all synthetic ok, native `/api/song/{ext}`, `/api/album/{ext}`, `/api/artist/{ext}`, `/api/song?album_id=`, `/api/album?artist_id=`, and `radio/stream/{token}` (the token is the capability) |

How Octo names the user:
- `RequestIdentity.UsernameAsync` uses `u` when present. Otherwise it resolves the owner of an `apiKey` with `rest/tokenInfo`, caching hits for 5 min and misses for 30 s.
- `NativeUsername` uses `u`, else the Navidrome JWT carried in `X-Nd-Authorization` or `Authorization: Bearer`. It first checks the token map captured from logins, then reads the payload claims `username`, `preferred_username`, `user`, `name`, `sub`.

**Admin.** There is no authentication. Two layers apply:

1. **`AdminRequestGuard`**, for paths starting with the segment `/api/admin`:
   - It strips every `Access-Control-*` response header.
   - GET and HEAD pass through.
   - **OPTIONS gets `204` with no CORS approval** and an empty body, which the browser treats as a refused preflight.
   - Any other method without an `X-Octo-Admin` request header (any value) gets **`403`** with `{"error":"Admin changes must come from Octo's dashboard. A script can send the X-Octo-Admin header to opt in."}`.
2. **Browse session**, on the endpoints marked **S** in section 4.
   - The token comes from the cookie `octo_browse` or the header `X-Octo-Browse-Token`. **The cookie wins**: a stale cookie hides a valid header.
   - The cookie is created by `POST /api/admin/browse/auth`: `HttpOnly`, `SameSite=Strict`, `Secure` only when the request is HTTPS, `Path=/api/admin`, `Max-Age` 90 days, with a sliding expiry in the store.
   - AdminController's `BrowseUser` **re-issues the cookie** on every signed-in request. `CoverUpgradeController` and `LyricsAdminController` only call `Validate`; they slide the server-side expiry but do not re-issue the cookie.
   - Failure: `401 {"error":"Sign in with your Navidrome admin account first."}`. `GET browse` uses `"Browse session required."` instead.

### 2.10 ID taxonomy (decides local, proxied or mixed)

| Shape | Meaning | Recognized by |
|---|---|---|
| any id present in `ExternalIdRegistry` (pure base62 with no prefix, persisted) | Octo external song, album or artist (type `"song"` from `ParseSongId`). The registry's `Kind` tells songs, albums and artists apart. | `ParseExternalId` checks the registry first |
| `ext-{provider}-{song\|album\|artist}-{id...}` | Legacy external id | `ParseExternalId` |
| `ext-{provider}-{id...}` (3 or more parts, `parts[2]` not a type) | Legacy external song | `ParseExternalId` |
| `ext-album-*`, `ext-artist-*` with no registry hit | Pre-registry ids. `getCoverArt` serves the placeholder or a 404. | `GetCoverArt` |
| `pl-{deezer\|qobuz}-{id}` | External playlist, exposed as an album. Navidrome's own `pl-{id}_{hex}` cover ids are **not** matched. | `PlaylistIdHelper` |
| 22 characters starting `or` (radio station) or `og` (generated mix) | Octo playlist ids. Read-only. | `IsOctoPlaylistId` |
| `octo-radio` | Static branded station cover | `GetCoverArt` |
| 48-character token | Continuous Radio stream session | `radio/stream/{token:length(48)}` |
| anything else | Navidrome's own id, which is relayed | |

"External" in the tables below means `ParseSongId(id).isExternal`: either a registry hit or an `ext-` prefix.

---

## 3. Subsonic routes

Unless a row says otherwise:
- Every route accepts **GET and POST** at both `/rest/{name}` and `/rest/{name}.view`.
- Parameters come from the merged dictionary in 2.3.
- `f` (default `xml`) selects the format.
- All other parameters are relayed untouched.

Abbreviations used in the Response column:
- **SX**: a Subsonic envelope in XML or JSON, chosen by `f` (2.4/2.5).
- **PT**: the upstream body and content type, verbatim, with status 200.
- **SX-JSON**: a Subsonic envelope, always JSON.

### 3.1 System and auth

| Endpoint | Action (line) | Parameters | Handling | Auth | Response |
|---|---|---|---|---|---|
| `ping` | `Ping` (L207) | all relayed | **Proxied** as `rest/ping.view` with `RelaySafeAsync` | R | PT on success. The URL check runs **before** any relay: a blank or non-absolute `Subsonic:Url` gives SX error code 0 `Octo isn't configured yet. Open {scheme}://{host}/admin and set your Navidrome URL (SUBSONIC_URL), then point this client at Octo instead of Navidrome.` A relay failure gives SX error code 0 `Octo can't reach Navidrome at {Url}. Check the URL is correct and reachable from the Octo container (use a LAN IP or service name, not localhost).` A Navidrome auth failure passes through as Navidrome's envelope. |
| `getOpenSubsonicExtensions` | `GetOpenSubsonicExtensions` (L2568) | all relayed | **Mixed**: relayed, then merged | none (spec) | When upstream says ok, Octo adds or extends `octoAcquisitions [1]`, `octoLyrics [1]` (only while `LyricsChoiceService` is registered **and** `Metadata:FetchLyrics`), `octoLibraryActions [1,2]` (only while `LibraryActions:Enabled`) and `songLyrics [1,2]`. Versions are added and never removed. In JSON the result is the upstream document re-serialized with `application/json`. In XML it is the re-serialized upstream with the upstream content type. An upstream `failed` envelope passes through unchanged. An unparseable body goes back as-is. With no upstream answer, Octo lists only its own extensions (JSON includes `"type":"octo"`). Format check is case-insensitive. |
| `jukeboxControl` | `JukeboxControl` (L3237) | `f` | **Local** | N | Always SX error code 0 `Jukebox is not supported`. Nothing is relayed. |

### 3.2 Browsing

| Endpoint | Action (line) | Parameters | Handling | Auth | Response and quirks |
|---|---|---|---|---|---|
| `getSong` | `GetSong` (L1655) | `id` (required) | **Mixed** | external N, local R | Missing `id` gives SX code 10 `Missing id parameter`. A local id is relayed with `RelayAsync` (exceptions go to the global handler) and returns PT. An external id is built from `metadataService.GetSongAsync` into SX `song`. A missing song gives code 70 `Song not found`. |
| `getArtist` | `GetArtist` (L1691) | `id` (required) | **Mixed** | external N, local R | Missing `id` gives code 10. An external artist gives SX `artist` plus albums from the metadata service, with `album.artist`/`artistId` filled from the artist (code 70 `Artist not found`). A local artist is relayed with **`f` forced to `json`**. Octo then searches the catalog for the artist (5 results, exact `SameArtistName`) and appends catalog albums the library does not own (by `SongIdentity.Key` of the title), sets `albumCount`, and answers `CreateMergedResponse` in the client's format. If the upstream JSON has no `artist`, the client gets Navidrome's answer (`RelayAsAskedAsync`): the JSON already fetched for `f=json`, otherwise a re-request in the client's format. A relay failure gives code 70 `Artist not found`. |
| `getAlbum` | `GetAlbum` (L1837) | `id` (required) | **Mixed** | external and playlist N, local R | Missing `id` gives code 10. A `pl-{provider}-{id}` id returns the external playlist **as an album** (`CreatePlaylistAsAlbumResponse`) and registers its tracks in the playlist cache; a miss or exception gives code 70 `Playlist not found`. An external album gives SX `album` from the metadata service (code 70 `Album not found`). A local album is relayed with **`f` forced to `json`**. Octo then finds a catalog album that **holds the library's songs** (`AlbumFillIn`), appends catalog songs the library does not own, sorts by `track`, and recomputes `songCount` and `duration`, answering `CreateMergedResponse`. The fallbacks match `getArtist`. |
| `getAlbumInfo2`, `getAlbumInfo` | `GetAlbumInfo2` (L3624) | `id` | **Mixed** | external N, local R | An external id gives SX `albumInfo` with `notes:""` and `small/medium/largeImageUrl` all set to the album cover URL, as **child elements in XML** (`CreateInfoResponse`). A local id is relayed **always as `rest/getAlbumInfo2`**, even for a v1 request, and returns PT. A relay failure gives the empty-ok `albumInfo`. |
| `getArtistInfo2`, `getArtistInfo` | `GetArtistInfo2` (L3655) | `id` | **Mixed** | external N, local R | An external id gives SX `artistInfo2` with `biography:""` and the three image URLs set to the artist image. A local id is relayed **always as `rest/getArtistInfo2`**. A relay failure gives the empty-ok `artistInfo2`. The v1 request still gets the `artistInfo2` element name. |
| `getRandomSongs` | `GetRandomSongs` (L242) | all relayed | **Proxied** with `RelayAsync`, no catch | R | `ContentResult` holding the **UTF-8-decoded** upstream body, with `Content-Type` set to the upstream type or `application/json`, status 200. Exceptions go to the global handler (502/503 JSON). `GetRandomSongs_DISABLED_HIJACK` (L255) is dead code. |
| `getSimilarSongs`, `getSimilarSongs2` | `GetSimilarSongs` (L2770) | `id` (required), `count` (int, default 50, clamped to `[1, LastFm.EffectiveRadioTrackCount]`) | **Mixed** | local seed R (via getSong), else N | The response key is `similarSongs2` when the path contains `getSimilarSongs2`, otherwise `similarSongs`. Missing `id` gives code 10. When Last.fm radio is off, the request is relayed with `RelayAsync(Request.Path.Value)`. **Quirk:** the path has a leading `/`, so the upstream URL becomes `{Url}//rest/getSimilarSongs2.view`, keeping the client's casing and `.view`; a relay failure gives the empty-ok. When radio is on, the seed's artist and title come from the external metadata or from a relayed `getSong` (`f=json`). Octo asks Last.fm for similar tracks and resolves each one (local copies preferred, concurrency 10), spaces repeated artists apart (`LastFmRadioSpacing`), and returns SX with `song[]`. Any failure gives the empty-ok. Side effects: the queue is registered and the top 8 YouTube ids are prewarmed. |

### 3.3 Search

| Endpoint | Action (line) | Parameters | Handling | Auth | Response and quirks |
|---|---|---|---|---|---|
| `search3`, `search2` | `Search3` (L954) | `query` (trimmed, surrounding `"` removed), `songCount` (default 20), `songOffset` (default 0), `albumCount` (default 20), `albumOffset` (default 0), `artistCount` (default 20), `artistOffset` (default 0), `musicFolderId`, `c`, `u`/`apiKey` | **Mixed** | R in practice (an upstream `failed` envelope passes through) | The element is `searchResult2` when the path contains `search2`, otherwise `searchResult3`, and the relay target matches. Branches, in order: |

1. **Sync walk.** Applies when the query is empty, there is no `musicFolderId`, `Subsonic:EnableSyncCatalog` is on, and `c` contains one of the names in `SyncCatalogClients` (case-insensitive). Octo relays the walk, returns a failed body unchanged, and appends the user's sync-catalog artists, albums and songs once the library pages run out. It waits at most 15 s for the catalog and probes `Exists` with `f=json` counts of 1. If the relay fails, the client gets the empty-ok envelope.
2. **Later song page.** Applies when there is a query, `songOffset > 0` and discovery is on. Octo continues page one's remembered order (`SearchSongOrderCache`, keyed by user, `c`, endpoint, `musicFolderId` and query), or rebuilds it. It does one relay for the library rows on that page (and Navidrome's albums and artists at the client's offsets), then merges. If Navidrome is unreachable, Octo falls through to a plain relay.
3. **Plain relay.** Applies when the query is empty or `songOffset > 0` and branches 1 and 2 did not answer. `RelayAsync` returns PT; an exception gives the empty-ok envelope.
4. **Discovery merge** (page one). `SearchBudget.Compute(songCount, EnableSearchDiscovery)` splits the page into local and external slots. External albums (Deezer, at most 20) and artists (at most 20) are skipped for a type-ahead probe (`songCount > 0` with zero external budget), for a non-zero album or artist offset, or when discovery is off. External playlists are included when `EnableExternalPlaylists` is on and `albumOffset <= 0`. The local relay sends the adjusted counts. **A failed upstream envelope passes through unchanged.** When the relay fails outright (Navidrome down), Octo still answers with discovery rows alone, **without any auth check**.
   - Merge output for JSON: `{song, album, artist}` arrays.
   - Merge output for XML: elements in the order **artists, then albums, then songs**.
   - JSON is chosen when `f == "json"` **or** the upstream content type contains `json`.

### 3.4 Media

| Route | Action (line) | Parameters | Handling | Auth | Response and quirks |
|---|---|---|---|---|---|
| `rest/stream[.view]` (GET, POST) | `Stream` (L1451) | `id` (required), `c`, `f` (for errors only), `maxBitRate`/`format`/... (relayed), header `Range` | **Mixed** | local R, external **C** | **Missing `id` gives `400 {"error":"Missing id parameter"}` as plain JSON, not a Subsonic envelope.** A local id is streamed through `RelayStreamAsync`: `Range`/`If-Range` are forwarded; `Accept-Ranges`, `Content-Range`, `Content-Length`, `ETag`, `Last-Modified` and the upstream status (200/206) are copied; the content type is the upstream's or `audio/mpeg`. The `FileStreamResult` claims range processing, but the stream is not seekable, so ASP.NET does not slice it **(verify)**. An upstream non-2xx becomes a ProblemDetails response with that status. For an external id: |

1. The credential check runs first; refused gives SX code 40, unreachable gives SX code 0.
2. When `WaitForLosslessOnPlay` is on and a local file exists for the id, the file is served with ASP.NET range support. The content type comes from the extension: mp3 `audio/mpeg`, flac `audio/flac`, ogg `audio/ogg`, m4a `audio/mp4`, wav `audio/wav`, aac `audio/aac`, anything else `audio/mpeg`.
3. A **first-byte request** counts as a play and calls `QueuePlay`. That means no `Range` header, or one starting `bytes=0-`, and not HEAD.
4. When `WaitForLosslessOnPlay` is on, Octo enqueues an acquisition and waits for it (at most `LosslessWaitTimeoutSeconds`, then falls back to the lossy preview). Errors: code 70 `Could not fetch a lossless copy: ...` or `Lossless copy is no longer on disk`.
5. Otherwise the stream comes **directly from the shim**: the client's `Range` is passed on, and the response carries the upstream status (200/206), `Content-Type`, `Accept-Ranges: bytes`, `Content-Length` and `Content-Range`, copied straight through.
6. If no source is found: SX code 70 `No playable source found for this track`. A client disconnect produces an empty result. Any other exception gives `500 {"error":"Failed to stream: ..."}`.

`HEAD /rest/stream` does **not** reach this action (2.2).

| Route | Action (line) | Parameters | Handling | Auth | Response and quirks |
|---|---|---|---|---|---|
| `rest/getCoverArt[.view]` | `GetCoverArt` (L2012) | `id` (required), `size` (int, more than 0, used only for generated covers), `u`, `c` | **Mixed** | local R, everything else N | Missing `id` gives `NotFound()` as ProblemDetails 404. Resolution order: |

1. `octo-radio` serves the branded placeholder.
2. A station id for user `u` gets a generated JPEG list cover drawn from up to 4 seed covers at `size`. Without a cover service it gets the unbranded placeholder.
3. A generated mix for `u` gets a generated JPEG. Its seeds are Navidrome covers fetched with the caller's parameters at `size=128`, and it carries no Octo badge.
4. `pl-{provider}-{id}` fetches the playlist's cover URL and returns its bytes and content type. Any failure serves the branded placeholder.
5. A registry id goes through `CoverArtAggregator`. The bytes carry the Octo badge **unless `c` equals `Octo`** (trimmed, case-insensitive). The content type is **always `image/jpeg`**. A miss or exception serves the placeholder, or a ProblemDetails 404 when `c=Octo`.
6. `ext-album-*` and `ext-artist-*` serve the placeholder (404 for `c=Octo`).
7. `ext-{provider}-{type}-{id}` fetches the cover URL from the artist, album or song, adds the badge unless `c=Octo`, and returns `image/jpeg`. Otherwise placeholder or 404.
8. Anything else is relayed with `RelayAsync` and returns PT, falling back to `image/jpeg`. **A relay failure serves the unbranded placeholder, with status 200.**

The placeholder is a 200 JPEG. When the cover service returns no bytes, the answer is a ProblemDetails 404.

| Route | Action (line) | Parameters | Handling | Auth | Response and quirks |
|---|---|---|---|---|---|
| `rest/getTranscodeDecision[.view]` | `GetTranscodeDecision` (L3156) | `mediaId` | **Mixed** | external N, local R | An external `mediaId` gets, always in JSON (`JsonResult`), `{"subsonic-response":{"status":"ok","version":"1.16.1","transcodeDecision":{"canDirectPlay":true,"canTranscode":false}}}`. A local id is relayed as **`rest/getTranscodeDecision.view`** with `RelayAsync` and returns PT (falling back to `application/json`). An `HttpRequestException` gives the same direct-play JSON. |
| `radio/stream/{token:length(48)}` (**GET, HEAD**) | `StreamGeneratedRadio` (L640) | path `token`, header `Icy-MetaData` | **Local** | the token itself | An unknown token or unresolved station gives **404 with an empty body** (status set directly, no ProblemDetails). Otherwise the response is 200 with `Content-Type: audio/mpeg`, `Cache-Control: no-store, no-transform`, `Accept-Ranges: none`, `icy-name: {station}`, `icy-br: {EffectiveRadioStreamBitrateKbps}`, and `icy-metaint: {IcyMetadataStream.DefaultInterval}` when `LastFm:EnableIcyMetadata` is on and the request sends `Icy-MetaData: 1`. HEAD returns the headers only. GET streams an endless transcoded MP3 with ICY metadata interleaved when asked. A failure before the first byte gives 503; a failure after it aborts the connection. Tokens are issued by `getInternetRadioStations`. |

`download`, `hls`, `getAvatar` and `getCaptions` have **no handler**. They reach the catch-all (3.9): an external `id` gets the empty-ok, and anything else gets the faithful relay, which buffers the whole body in memory.

### 3.5 Playlists and internet radio

| Endpoint | Action (line) | Parameters | Handling | Auth | Response and quirks |
|---|---|---|---|---|---|
| `getPlaylists` | `GetPlaylists` (L357) | `u`, all relayed | **Mixed**: relay, then append | R | A relay failure, an empty body, or a body that is not `ok` (format-aware) is passed through as-is; with no body at all the client gets SX code 0 `Unable to authenticate with Navidrome`. On success Octo, without delaying the reply, makes sure this user's library-action playlists exist; on a user's first request it also seeds the radio profile from starred, frequent and recent songs. It then appends the visible **radio stations** (`ExposeRadioAsPlaylists`) and the **generated mixes** (`GeneratedPlaylists:Enabled`) as extra `playlist` rows. Output: `application/json` or `application/xml` (XML re-serialized, pretty-printed). If the merge throws, the upstream body goes back unchanged. |
| `getPlaylist` | `GetPlaylist` (L426) | `id`, `u` | **Mixed** | mix and station P (client's `f`), others R | A generated mix id for `u`: `rest/ping` is relayed with `id` removed (failure gives SX code 40 `Wrong username or password`), then the mix is materialised and returned with `CreateGeneratedPlaylistResponse`. A station id among `u`'s playlist stations: the same ping check, then the tracks are resolved (concurrency 4, explicit filter applied, back-to-back repeats of an artist or album dropped), blended into discovery, durations completed, rows swapped for the sync-catalog version where one exists, the queue registered, and `CreateRadioPlaylistResponse` returned. Any other id is relayed with `RelaySafeAsync` and returns PT; a failure gives SX code 0 `Playlist not found`. |
| `createPlaylist`, `updatePlaylist`, `deletePlaylist` | `MutatePlaylist` (L710) | `playlistId` (falls back to `id`) | **Mixed** | R | An Octo playlist id (22 characters starting `or` or `og`) gives SX code 70 `Octo's generated playlists are read-only`. Otherwise the request is relayed as `"rest/" + <last path segment without .view>`, keeping the client's casing, and returns PT. A failure gives SX code 0 `Unable to update playlist`. |
| `getInternetRadioStations` | `GetInternetRadioStations` (L484) | `u`, all relayed | **Mixed** | R | Same pass-through rules as `getPlaylists` (code 0 `Unable to authenticate with Navidrome`). On success, the profile bootstrap runs, then the visible stream stations (`ExposeRadioAsStreams`) are listed. A stream session token is issued for each station whose ready pool is warm. When none is warm, Octo prepares **one** starter, bounded by `EffectiveStarterPublishTimeout`, and warms the rest in the background. Each published station is appended as `internetRadioStation {id, name, streamUrl: "{scheme}://{host}{pathBase}/radio/stream/{token}", coverArt: id}` (attributes in XML). If the merge throws, the upstream body goes back unchanged. |
| `createInternetRadioStation`, `updateInternetRadioStation`, `deleteInternetRadioStation` | `MutateInternetRadioStation` (L688) | `id` | **Mixed** | R | An Octo playlist id gives SX code 70 read-only. Otherwise the request is relayed as `rest/<segment>` and returns PT; a failure gives code 0 `Unable to update internet radio station`. |

### 3.6 Stars, ratings, scrobble, playback reporting

| Endpoint | Action (line) | Parameters | Handling | Auth | Response and quirks |
|---|---|---|---|---|---|
| `star` | `Star` (L2374) | `id`, `albumId`, `artistId` (relayed), `u`, `c`, credentials | **Mixed** | external C, local R | Branches, in order: |

1. **External playlist** `id` (`pl-...`). Without the playlist sync service: code 0 `Playlist functionality is not enabled`. Otherwise the credential check runs, a full playlist download starts in the background, and the reply is the empty-ok `starred`.
2. **Registry album** (`id`, else `albumId`, whose registry kind is Album). The credential check runs. When no heart source has `AlbumEnabled`, the reply is still empty-ok and the star is ignored. Otherwise Octo holds a star-on-arrival (unless `c` is an Octo app), begins tracking the album, queues the album download, and replies empty-ok `starred`. Nothing is relayed.
3. **External song** while some heart source has `SongEnabled`. The credential check runs, then star-on-arrival, tracking and `QueueTrack`, then the empty-ok reply.
4. **Anything else** is relayed with `RelayAsync` and returns PT. When the reply is ok, `u` is set and the id is not blank, the radio profile records the heart (`MarkHeart`). An `HttpRequestException` gives SX code 0 `Error connecting to Subsonic server: ...`. An unconfigured URL escapes to the global handler as a 503.

`unstar` has **no handler**: it reaches the catch-all, where an external id gets the empty-ok `unstar` and anything else is relayed.

| Endpoint | Action (line) | Parameters | Handling | Auth | Response and quirks |
|---|---|---|---|---|---|
| `setRating` | `SetRating` (L2711) | `id`, `rating`, `u`, `t`, `s` | **Mixed** | external N, local R | An external `id` gets the empty-ok `setRating` and **is not relayed**. Otherwise the request is **always relayed first** and Navidrome's answer goes back as PT. After that, when the reply is ok, `u`, `t` and `s` are all present (not `p`, not `apiKey`), the rating maps to an enabled library action, and the scope allows it (Global, or the track sits in a notice playlist), Octo queues a `RatingActionRequest`. An `HttpRequestException` gives SX code 0 `Error connecting to Subsonic server: ...`. |
| `scrobble` | `Scrobble` (L2967) | `id` (repeatable), `submission` (repeatable), `time` (repeatable, unix ms), `u`/`apiKey` | **Mixed** | R / P | The first `id` triggers a prewarm of the next 8 of the 16 upcoming queue entries. **Only library ids are relayed**: `id`, `submission` and `time` are paired by index when their counts match the number of ids; `time` values that do not pair up are dropped. When every id is external, `rest/ping` is relayed instead as the credential check and the client gets **the ping's body**. When the reply is ok, Octo learns from the plays: the radio profile, Last.fm scrobble and Now Playing, and ListenBrainz (external songs only). A single `submission` applies to every id, and a missing one counts as completed. Duplicate completed reports are suppressed per user, id and time. An `HttpRequestException` gives the empty-ok `scrobble`. |
| `reportPlayback` | `ReportPlayback` (L3215) | `mediaId` (falls back to `id`) | **Mixed** | external N, local R | A local, non-empty id is relayed with `RelaySafeAsync` and returns PT. Otherwise, or when the relay fails, the reply is the empty-ok `reportPlayback`. This action carries a duplicate stray `[HttpGet, HttpPost]` attribute (L3207); it has no effect. |

### 3.7 Lyrics

| Endpoint | Action (line) | Parameters | Handling | Auth | Response and quirks |
|---|---|---|---|---|---|
| `getLyricsBySongId` | `GetLyricsBySongId` (L3263) | `id`, `enhanced` (`true` or `1`), `c` | **Mixed** | external N, local R | `fetching` means the lyrics service exists and `Metadata:FetchLyrics` is on. For an **external id** (looked up in the registry, then by decoding the external id): a pinned choice wins (by id, then artist and title), otherwise a live lookup runs (4 s interactive budget, continuing in the background for up to 30 s), and the reply is `CreateLyricsListResponse`. When the lookup is still running and `c=Octo`, the reply is SX code 0 `Still looking for lyrics; ask again shortly`. For a **local id**: Navidrome is asked, and for `f=json` without `enhanced` Octo **adds `enhanced=true` upstream**. An ok reply is what grants access. A pin wins. Otherwise, when fetching is on and the song's own lyrics timing can be read, a live lookup runs with the song's own lyrics ranked among the sources. A non-own result wins unless it is "instrumental" and the song has lyrics. When nothing beats Navidrome, the Navidrome body goes back, **with `cueLine` and `kind` removed** for JSON clients that did not ask for `enhanced`. A relay failure gives the empty-ok `lyricsList`. Word cues are included only when `enhanced` is on. |
| `getLyrics` | `GetLyrics` (L3336) | `artist`, `title` (trimmed) | **Mixed** | R | Always relayed first. A failure gives SX code 0 `Octo can't reach Navidrome`. When the reply is not ok, or artist or title is empty, it goes back as PT. A pin for that artist and title wins (`CreateLyricsResponse`). When fetching is on and Navidrome's `lyrics.value` is blank, a live lookup fills it in as plain text. Otherwise PT. |
| `getLyricsCandidates` (octoLyrics v1) | `GetLyricsCandidates` (L3387) | `id` (required), `artist`, `title` (overrides) | **Local** (Navidrome is used for the auth check and song lookup) | P | Always SX-JSON. Lookups off gives code 0 `Lyrics lookups are off on this server`. Missing `id` gives code 10 `Required parameter is missing: id`. An unknown song gives code 70 `Song not found`. Otherwise every source is searched within a 12 s budget and the reply is `CreateLyricsCandidatesResponse(id, currentChoice, candidates)`. |
| `setLyricsChoice` (octoLyrics v1) | `SetLyricsChoice` (L3417) | `id` (required), `candidate` (required: a candidate id, `auto` or `none`) | **Local** | P | Always SX-JSON. No choice service gives code 0 `Lyrics are off on this server`. Missing parameters give code 10 `Required parameter is missing: id and candidate`. `auto` clears the pin, even for an unknown song. An unknown song otherwise gives code 70. `none` hides the song's lyrics. A candidate requires FetchLyrics and is pinned within 12 s; when that fails, code 70 `Those lyrics could not be found; ask for the candidates again`. On success: `CreateLyricsChoiceResponse(id, choice)`. The pin records who set it (`NativeUsername`). |

### 3.8 Octo-specific extensions

All of these answer in **SX-JSON whatever `f` says**. Auth is **P**: a ping with `f=json`, where unreachable gives code 0 and rejected gives code 40.

| Endpoint | Action (line) | Parameters | Response |
|---|---|---|---|
| `getAcquisitions` (octoAcquisitions v1) | `GetAcquisitions` (L2540) | `u`/`apiKey` | `{"status":"ok","version":"1.16.1","type":"octo","acquisitions":{"acquisition":[{id, artist, title, album, state(lowercase), progress, bytesDone, bytesTotal, source, startedAt, updatedAt (yyyy-MM-ddTHH:mm:ssZ), error, libraryId, ahead, note}]}}`. Only the caller's own rows are included, and nulls are kept. |
| `getLibraryActions` (octoLibraryActions v1) | `GetLibraryActions` (L2587) | `u` | `CreateLibraryActionsResponse(settings, u, parallel, upgradeReady, upgradeSourceName)` |
| `getUpgrades` (octoLibraryActions v2) | `GetUpgrades` (L2602) | `u` | `CreateUpgradesResponse`: the caller's upgrade jobs, with live progress for jobs in the `Working` state |
| `libraryAction` (octoLibraryActions v1/v2) | `ApplyLibraryAction` (L2629) | `id`, `action` (`remove` or `upgrade`, case-insensitive) | Missing parameters give code 10 `Required parameter is missing: id and action`. An unknown action gives code 0 `Unknown action "{a}"; this server knows remove and upgrade`. **`remove`** without `u`, or with library actions off, gives `state: skipped` with a reason. Otherwise it runs `LibraryActionExecutor.ApplyAsync(Delete)`, **not tied to the request**, so a client that hangs up does not stop it. **`upgrade`** checks its gates in order (signed in by username, enabled, on the allow-list, BetterQuality on, dry run off, source ready) and gives `skipped` with a reason for the first that fails. Otherwise the song is added to the upgrade queue (origin `"app"`) and the reply is `queued`, `"Looking for a higher quality copy on {source}."`. |

### 3.9 Catch-all proxy: `/{**endpoint}`, `GenericEndpoint` (L3682)

**Methods: all of them.** It matches every path no other route claims (2.2). `endpoint` is the path without its leading `/`, in the client's casing. The steps run in this order:

1. **Octo-owned path.** If the lower-cased path starts with `admin` (which also matches `administrator`...) or `api/admin`, starts with `assets/`, or equals `favicon.ico`, the reply is `NotFound()`, a ProblemDetails 404. Nothing is relayed.
2. **Native radio** (`TryServeNativeRadioAsync`), for `api/playlist` or `api/playlist/...`:
   - A reserved Octo id with a method other than GET gives `405 {"error":"Octo's generated playlists are read-only"}`.
   - A non-reserved sub-path falls through to step 3 onward.
   - Otherwise Octo makes a faithful relay. For the list (GET with an empty tail), it forces `_start=0&_end=1000` upstream. An upstream status outside 2xx goes back with its body.
   - **List**: the user's visible playlist stations are appended in native shape (`id, name, comment:"Generated by Octo Radio", ownerName, public:false, songCount, duration (180 per unknown track), createdAt, updatedAt, path:"", smartPlaylist:true, readonly:true, validUntil`). The client's `_start`/`_end` page is cut **after** the merge, and `X-Total-Count` is set to the merged total.
   - `api/playlist/{octoId}` returns that station object. When the user cannot see it: `404 {"error":"Radio station not found for this user"}`.
   - `api/playlist/{octoId}/tracks` materialises the station and returns the native song array, paged, with `X-Total-Count`.
3. **External-id safety net.** When any of the query or body keys `id`, `mediaId`, `albumId`, `artistId` holds an external id, the reply is the empty-ok `CreateResponse(f, ElementFor(endpoint))`. `ElementFor` strips `.view` and turns `getXxx` into `xxx`, falling back to `response`.
4. **`GET api/song/{externalId}`** (leaf only). The song is enriched, its top duration resolved, and returned in the native song object shape (see `BuildNativeSongObject`: `suffix`/`bitRate` are `flac`/950 when `WaitForLosslessOnPlay`, otherwise `m4a`/128; `createdAt` and `updatedAt` fixed at `2020-01-01T00:00:00Z`; `artistId`/`albumId` fall back to `{id}-ar`/`{id}-al`). Status 200, `application/json`.
5. **`api/song?title=...`** with `_start` absent or 0. Octo makes a faithful relay. When the reply is 200 with a JSON array, it appends up to `min(_end - count, 60)` discovery songs (`_end` defaults to count + 60), re-sends the upstream headers except `X-Total-Count`, and sets `X-Total-Count` to the new length. Any problem falls through to the plain relay.
6. **`api/album/{registryAlbumId}`** returns the native album object (`libraryId` 1, `duration` as a float, `tags.releasetype`).
7. **`api/artist/{registryArtistId}`** returns the native artist object (counts both flat and under `stats.albumartist` / `stats.artist`, plus image URLs).
8. **`api/album?artist_id={registryArtistId}`** returns that artist's albums. `_end` at or before `_start` (Feishin sends `-1`) means the rest of the list. `X-Total-Count` is set.
9. **`api/album?name=...`** with `_start` absent or 0. Same pattern as step 5: up to 20 Deezer albums not already in the library (matched with `AlbumKey`), inheriting `libraryId` from the first real row.
10. **`api/song?album_id={registryAlbumId}`** returns the catalog album's tracks as native songs, with `X-Total-Count`.
11. **Faithful relay** (`RelayRawAsync`) for everything else: the upstream status, the allowlisted headers (2.7), `Content-Type` (or `application/{f}`) and the body. When a `POST auth/login` gets a 200, Octo **captures the Navidrome identity** from it (`CaptureLogin`). Any exception gives SX code 0 `Error connecting to Subsonic server: ...` with status 200.

Endpoints that normally land here and are passed straight through:
- Every Subsonic call without its own handler: `getMusicFolders`, `getIndexes`, `getArtists`, `getMusicDirectory`, `getGenres`, `getAlbumList`, `getAlbumList2`, `getSongsByGenre`, `getNowPlaying`, `getStarred`, `getStarred2`, `unstar`, `download`, `getTopSongs`, `getUser`, `getUsers`, `getScanStatus`, `startScan`, `getPlayQueue`, `savePlayQueue`, `getBookmarks`, `createBookmark`, `deleteBookmark`, `getShares`, `createShare`, `tokenInfo`, `getAvatar`, `hls`, `getLicense`, ...
- Navidrome's native API: `/auth/login`, `/api/*`.

Their `.view` spelling and casing are preserved upstream.

---

## 4. Admin routes, grouped by area

Legend for the **Gate** column:
- **G**: AdminRequestGuard only. GET is open; a write needs `X-Octo-Admin`.
- **S**: the browse session is required as well (2.9).

Every write route (POST, PUT, DELETE) is also behind G. Bodies are JSON; binding is case-insensitive, so `user` and `User` are the same field. Responses are `application/json; charset=utf-8` unless noted.

### 4.1 UI entry

| Method | Path | Action (line) | Gate | Response |
|---|---|---|---|---|
| GET | `/admin` (also `/admin/`) | `AdminController.AdminRoot` (L666) | none (outside `/api/admin`) | `302` with `Location: /admin/index.html` |

### 4.2 Settings, configuration and process control

| Method | Path | Action (line) | Parameters | Gate | Response |
|---|---|---|---|---|---|
| GET | `/api/admin/settings` | `GetSettings` (L679) | none | G | `JsonResult` with **PascalCase** sections: `Subsonic`, `Library`, `Server`, `Updates`, `Downloads`, `Lidarr`, `YouTube`, `LastFm`, `LibraryActions`, `Metadata`, `GeneratedPlaylists`, `Genre`, `Notifications`, `ListenBrainz`, `_meta` (`ConfigFilePath`, `ConfigFileExists`, `ConfigFileValid`, `RestartPending`, `SecretPlaceholder`, `Version`). `Subsonic.AdminPassword`, `LastFm.ApiSecret` and every session key show as `"(saved, not shown)"` when set. Every other secret goes out in clear. `LastFm.DiscoveryStations` and `Genre.Mappings` come from the raw file. |
| POST | `/api/admin/settings` | `SaveSettings` (L961) | raw JSON object (a partial patch, any subset of the GET shape) | G | `200 {"ok":true,"persisted":<merged file, secrets redacted>}`. `_meta` is dropped. Validation failures give `400 {"error":...}`: empty body, invalid JSON, LibraryActions (enabled with no allowed users, more than 50 users, more than 5 actions, a name over 80 characters, a rating outside 0-5, a duplicate rating), Genre.Mappings (more than 200 rules, a pattern empty or over 60 characters, a duplicate pattern, a genre over 60 characters, a match other than Contains or Exact), LastFm.DiscoveryStations (more than 12, duplicate or missing ids or names, a name over 100 characters, tags not between 1 and 5). Secret placeholder rules: an echoed placeholder means "keep the saved value", and text added to the placeholder gives a 400. `LastFm.UserSessions` is always removed from the patch. `ListenBrainz.UserTokens` is replaced, not merged. A corrupt file gives `409`. A write failure gives `500 {"error"}`. |
| GET | `/api/admin/raw-config` | `GetRawConfig` (L1644) | none | G | The effective config as a pretty-printed (indented) JSON document, `Content-Type: application/json` (no charset). PascalCase sections like `settings` minus `_meta`. Secrets are masked the same way. |
| PUT | `/api/admin/raw-config` | `PutRawConfig` (L1906) | raw JSON object | G | Replaces `settings.json` **wholesale**, after restoring placeholders from the stored file (or from the running values when the file cannot be read). `200 {"ok":true,"bytes":N}`. `400` for an empty body, invalid JSON, a non-object, or text added to a placeholder (admin password, API secret, session key). `500` on a write failure. |
| GET | `/api/admin/config-sources` | `GetConfigSources` (L1976) | none | G | `{keys:[{Key,Value,IsSecret}], configFile}` over a fixed list of about 150 keys. Keys ending in `Password`, `ApiKey`, `Secret`, `Token` or `WebhookUrl` are masked with `•` (at most 16 of them). |
| GET | `/api/admin/status` | `GetStatus` (L2088) | none | G | `{octo:{ok,detail,warning,configured}, services:{navidrome,lidarr,ytDlpShim,lastfm: ServiceProbe}, time:ISO-8601}`. The probes run in parallel. The Navidrome probe calls `GET {Url}/rest/ping?u=probe&p=probe&v=1.16.1&c=octo&f=json` with a 5 s timeout. |
| POST | `/api/admin/restart` | `Restart` (L2143) | none | G | `202 {"ok":true,"message":"restarting"}`. After 1 s Octo calls `StopApplication()`; after 2 s more it calls `Environment.Exit(1)`. |
| POST | `/api/admin/clear-metadata-cache` | `ClearMetadataCache` (L2428) | none | G | `200 {"cleared":true}` |
| POST | `/api/admin/test-notification` | `TestNotification` (L519) | none | G | `200 {"results":[...]}`, one entry per notification sink |
| GET | `/api/admin/discover-servers` | `DiscoverServers` (L385) | none | G | `200 {"servers":[...]}` from a LAN scan |
| GET | `/api/admin/library-status` | `LibraryStatus` (L399) | none | G | `{autoDetect, pinnedLibraryPath, navidromeReports, visibleToOcto, configuredFallback, effectiveDownloadPath, writable, rescanAuthenticated, libraries:[{id,name,folder,visible}]}`. Triggers music-folder detection (cached). |
| GET | `/api/admin/lidarr/options` | `GetLidarrOptions` (L2115) | none | G | `200` with the Lidarr options object. Any exception gives `400 {"error"}`. |
| POST | `/api/admin/lidarr/test` | `TestLidarrConnection` (L2125) | body `{baseUrl?, apiKey?}` | G | `200 {"ok":true,"message":"Connected to Lidarr. Choices loaded.","options":...}`. Failure gives `400 {"ok":false,"error"}`. Nothing is persisted. |

### 4.3 Browse session and filesystem

| Method | Path | Action (line) | Parameters | Gate | Response |
|---|---|---|---|---|---|
| POST | `/api/admin/browse/auth` | `BrowseAuth` (L440) | body `{username, password}` | G | Octo posts `{username,password}` to `{Url}/auth/login`. Success requires `isAdmin:true`, and returns `200 {"ok":true,"user"}` plus `Set-Cookie: octo_browse=...` (2.9). No URL configured gives `503 {"error":"Navidrome URL is not configured yet."}`. Blank fields give `401` `Username and password are required.`. Navidrome refusing gives `401` `Navidrome rejected those credentials.`. A non-admin gives `401` `That account is not a Navidrome admin.`. An exception gives `502` `Could not reach Navidrome to verify credentials.`. |
| GET | `/api/admin/browse` | `Browse` (L488) | query `path?`, header `X-Octo-Browse-Token` | S | `{path, parent, separator, writable, exists, entries, truncated, audioFiles, containerised}`. No session gives `401 {"error":"Browse session required."}`. |
| GET | `/api/admin/browse/session` | `BrowseSession` (L560) | cookie only (no header) | G | `{"signedIn":true,"user"}` or `{"signedIn":false}`. Re-issues the cookie when signed in. |
| POST | `/api/admin/browse/signout` | `BrowseSignOut` (L565) | cookie | G | Revokes the session and deletes the cookie (`Path=/api/admin`). `200 {"ok":true}`. |
| POST | `/api/admin/tags/preview` | `PreviewTags` (L588) | body `{path?, artist?, title?, album?}` | S | `200` with the `TagPreview.PreviewAsync` result. `path` must resolve to a real file under the music root, with no `..` and no symlink on the way; otherwise `400`. Without `path`, both `artist` and `title` are required (`400` otherwise). No `TagPreview` service gives `503`. |
| GET | `/api/admin/library/resolve` | `ResolveLibrarySong` (L1434) | query `id` | S | `{id, resolved, musicRoot, hasAdminIdentity, path, source, sizeBytes, artist, title, album}`. Missing `id` gives `400 {"error":"Pass the Navidrome song id as ?id="}`. |

### 4.4 Last.fm, ListenBrainz and Radio

| Method | Path | Action (line) | Parameters | Gate | Response |
|---|---|---|---|---|---|
| GET | `/api/admin/lastfm/radio` | `GetLastFmRadio` (L172) | query `user?` (defaults to the first known user) | G | `{enabled, hasApiKey, personalizedEnabled, discoveryEnabled, playlistsEnabled, streamsEnabled, streamBitrateKbps, icyMetadataEnabled, minimumPlays, selectedUser, users, learning:{plays,needed,source,refreshing,lastRefreshAttemptUtc,lastRefreshSuccessUtc,lastRefreshError}\|null, stations:[{id,name,kind,personalized,trackCount,seeds,createdUtc,changedUtc,validUntilUtc,preview:[{artist,title}x5]}]}`. Without the radio store: `{users:[],stations:[]}`. |
| POST | `/api/admin/lastfm/radio/refresh` | `RefreshLastFmRadio` (L353) | body `{user, stationId?}` | G | `202 {"ok":true,"queued":bool}`. No refresh queue or a blank user gives `400 {"error":"A known Navidrome user is required"}`. A missing or empty `user` probably fails implicit `[Required]` first (2.8). |
| DELETE | `/api/admin/lastfm/radio/history` | `ResetLastFmRadio` (L362) | query `user` (required) | G | `200 {"ok","user","removedPlays","removedStations","message"}`. A blank user or no store gives `400`. A missing `user` gives an automatic ProblemDetails 400 (2.8). |
| GET | `/api/admin/lastfm/scrobble` | `GetLastFmScrobbling` (L239) | none | G | `{available, hasApiKey, hasApiSecret, enabled, libraryPlays, users}` |
| POST | `/api/admin/lastfm/check` | `CheckLastFmCredentials` (L266) | body `{apiKey?, apiSecret?}` (the placeholder means "the saved value") | G | `200 {"key","secret","message"}`. Scrobbling unavailable gives `404 {"error":"Last.fm scrobbling is not available."}`. |
| POST | `/api/admin/lastfm/scrobble/connect` | `ConnectLastFm` (L287) | body `{user}` | G | `200 {"user","url"}`. A `LastFmScrobbleException` gives `400`. Unavailable gives `404`. |
| POST | `/api/admin/lastfm/scrobble/finish` | `FinishLastFm` (L302) | body `{user}` | G | `200 {"ok":true,"user","lastFmUser"}`. Not yet approved, a corrupt settings file, or an I/O or permission failure gives `409 {"error"}`. Any other Last.fm error gives `400`. Unavailable gives `404`. |
| POST | `/api/admin/lastfm/scrobble/cancel` | `CancelLastFmConnect` (L277) | body `{user}` | G | `200 {"ok":true}`, or `404` when unavailable |
| POST | `/api/admin/lastfm/scrobble/disconnect` | `DisconnectLastFm` (L333) | body `{user}` | G | `200 {"ok":true,"user","message"}`. The message differs when the session comes from an environment variable. Error gives `400`. Unavailable gives `404`. |
| GET | `/api/admin/listenbrainz/validate` | `ValidateListenBrainz` (L214) | query `user?`, `token?` | G | `{configured:false, valid:false, detail}` or `{configured:true, valid, userName, detail}` |
| POST | `/api/admin/listenbrainz/validate` | `ValidateListenBrainzPost` (L233) | body `{user?, token?}` | G | same as the GET (keeps the token out of URLs) |

### 4.5 Downloads and acquisitions

| Method | Path | Action (line) | Gate | Response |
|---|---|---|---|---|
| GET | `/api/admin/downloads` | `Downloads` (L574) | G | `{"downloads":[...]}`, the 200 most recent |
| GET | `/api/admin/acquisitions` | `Acquisitions` (L640) | G | `{"acquisitions":[AcquisitionJson + provider, externalId, requestedBy (only while RecordRequestedBy is on)]}` for every user |

### 4.6 Library actions, review sweep, duplicates and quality upgrades

| Method | Path | Action (line) | Parameters | Gate | Response |
|---|---|---|---|---|---|
| GET | `/api/admin/library-actions` | `GetLibraryActions` (L1138) | header token | S | `{enabled, dryRun, hasAdminIdentity, allowedUsers, quarantine, entries:[{action,navidromeId,username,title,artist,album,state,detail,dryRun,sourcePath,quarantinePath,resolution,atUtc}]}`, the 200 most recent |
| GET | `/api/admin/notices` | `GetNotices` (L1175) | header token | S | `{entries:[{kind,username,artist,title,album,state,reason,origin,submitted,createdUtc,resolvedUtc}], duplicateScan}` |
| POST | `/api/admin/duplicates/scan` | `ScanDuplicates` (L1205) | none | G | `202 {"ok":true,"queued":true}`. Disabled or missing service gives `400`. No admin identity gives `400`. |
| GET | `/api/admin/review-sweep` | `GetReviewSweep` (L1218) | none | G | `200` with the status object, or `404 {"error":"The library check is not available."}` |
| POST | `/api/admin/review-sweep/start` | `StartReviewSweep` (L1222) | none | G | `202 {"ok":true}`. Disabled, Review off, or a rate of 0 gives `400`. |
| POST | `/api/admin/review-sweep/pause` | `PauseReviewSweep` (L1232) | none | G | `202 {"ok":true}`, or `404` |
| POST | `/api/admin/review-sweep/reset` | `ResetReviewSweep` (L1241) | none | G | `202 {"ok":true}`, or `404` |
| GET | `/api/admin/quality-upgrade` | `GetQualityUpgrade` (L1251) | none | G | `200` with the status object, or a ProblemDetails `404` |
| GET | `/api/admin/lossy` | `GetLossy` (L1268) | header token, query `refresh` (`1`, `true` or `yes` forces a fresh list; the list is cached 5 min in a static) | S | `{total, songs:[{id,title,artist,album,suffix,bitRate,size,path,fromYouTube,attemptKey,lastTried:{atUtc,outcome}\|null,job:{state,detail}\|null}]}`. No worker gives a ProblemDetails `404`. No admin identity gives `400`. Navidrome not listing gives `502`. |
| GET | `/api/admin/upgrades` | `GetUpgrades` (L1319) | header token | S | `{jobs:[{id,title,artist,album,suffix,state,detail,requestedBy,origin,queuedUtc,updatedUtc,startedUtc,progress,stage,source,bytesDone,bytesTotal,note,result}], parallel, source, sourceReady, plan, why, gate:{user,enabled,allowed,dryRun,betterQuality}}` |
| POST | `/api/admin/upgrades` | `QueueUpgrades` (L1379) | body `{songs:[{navidromeId, title?, artist?, album?, suffix?, attemptKey?}]}` (1 to 2000) | S | `202 {"ok":true,"queued":N,"refused":...}`. User not allowed gives `403 {"error"}`. A gate closed, no songs, or more than 2000 gives `400`. No queue gives a ProblemDetails `404`. Origin `"page"`. |
| POST | `/api/admin/upgrades/cancel` | `CancelUpgrades` (L1406) | body `{ids:[...]}` | S | `200 {"ok":true,"cancelled":...}`, or `404` |
| POST | `/api/admin/upgrades/clear` | `ClearUpgrades` (L1416) | none | S | `200 {"ok":true,"cleared":...}`, or `404` |

### 4.7 Genre normalization

| Method | Path | Action (line) | Parameters | Gate | Response |
|---|---|---|---|---|---|
| POST | `/api/admin/genre/backfill` | `StartGenreBackfill` (L1479) | body `{scope?: OctoDownloads\|WholeLibrary (default OctoDownloads), dryRun: bool, confirm?}` | S | `202 {"started":true,"scope","dryRun"}`. Genre off gives `400`. A whole-library apply without `confirm` equal to `Library:DownloadPath` gives `400`. Already running gives `409`. |
| GET | `/api/admin/genre/backfill` | `GetGenreBackfill` (L1504) | header token | S | `{runId,status,scope,dryRun,startedUtc,finishedUtc,total,processed,changed,cleared,skipped,failed,lastPath,reason,errors,preview,canResume,canUndo,settingsChanged,musicPath}` |
| POST | `/api/admin/genre/backfill/cancel` | `CancelGenreBackfill` (L1539) | none | S | `202 {"cancelling":true}` |
| POST | `/api/admin/genre/backfill/resume` | `ResumeGenreBackfill` (L1549) | none | S | `202 {"resumed":true}`. Nothing to resume gives `400`. Settings changed or already running gives `409`. |
| POST | `/api/admin/genre/backfill/undo` | `UndoGenreBackfill` (L1576) | none | S | `202 {"started":true}`. No journal gives `400`. Already running gives `409`. |
| GET | `/api/admin/genre/presets` | `GetGenrePresets` (L1594) | none | G | `{"broad":[{"Id","Pattern","Genre","Match","Enabled"}]}`. PascalCase on purpose, because the UI posts it straight back to `settings`. |

### 4.8 Cover upgrade: `CoverUpgradeController`, base `/api/admin/covers/upgrade`

All routes are **S**, using `Validate`, which does not re-issue the cookie.

| Method | Path | Action (line) | Parameters | Response |
|---|---|---|---|---|
| GET | `/api/admin/covers/upgrade` | `Get` (L48) | header token | `{runId,status,scope,mode,dryRun,smallerThan,picked,soft,folderCovers,fullSize,undo,startedUtc,finishedUtc,total,processed,upgraded,kept,files,failed,lastFolder,reason,songsTotal,songsRead,albumsTotal,albumsDone,errors,preview:[{id,folder,artist,album,fromSide,toSide,source,files,folderCover,result,looksSame}],canResume,busy,canUndo,musicPath}` |
| POST | `/api/admin/covers/upgrade` | `Start` (L95) | body `{scope?: OctoDownloads\|WholeLibrary (default OctoDownloads), mode?: Scan\|... (default Scan), folderCovers: bool = true, smallerThan: int = DefaultSmallerThan (clamped 1-10000), albums?: [string], confirm?}` | `202 {"started":true}`. `albums: []` gives `400` `Pick at least one album.`. A whole-library Apply with no `albums` needs `confirm` equal to the effective music path (`400` otherwise). Already running gives `409`. |
| POST | `/api/admin/covers/upgrade/cancel` | `Cancel` (L122) | none | `202 {"cancelling":true}` |
| POST | `/api/admin/covers/upgrade/resume` | `Resume` (L130) | none | `202 {"resumed":true}`. Nothing to resume gives `400`. Already running gives `409`. |
| GET | `/api/admin/covers/upgrade/thumb/{id}` | `Thumb` (L143) | path `id`, query `found: bool` (default false; an invalid value gives an automatic 400) | **Binary image** with `Cache-Control: private, max-age=300`. When `found=false`, Navidrome's thumbnail is tried first, with its own content type. Otherwise the stored thumbnail is returned with its sniffed MIME type (`CoverImage.MimeType`). A miss gives a ProblemDetails `404`. |
| POST | `/api/admin/covers/upgrade/undo` | `Undo` (L158) | none | `202 {"started":true}`. Nothing to undo gives `400`. Already running gives `409`. |

### 4.9 Lyrics admin: `LyricsAdminController`, base `/api/admin/lyrics`

All routes are **S**, using `Validate`.

| Method | Path | Action (line) | Parameters | Response |
|---|---|---|---|---|
| GET | `/api/admin/lyrics/library` | `GetLibraryRun` (L70) | header token | `{runId,status,scope,upgrade,startedUtc,finishedUtc,total,processed,written,wordTimed,upgraded,alreadyHad,notFound,instrumental,busy(the run's),skipped,failed,lastPath,reason,errors,canResume,mode,wordAlready,picked,rows:[{id,path,artist,title,album,has,result,source,kind,candidateId,doubt,preview}],busy(job running),canUndo,saveTo,review,writesBesideAll,fetching}`. **Probable C# bug.** The anonymous type has both `run.Busy` (an int, the count of busy lookups) and `busy = _job.IsRunning` (a bool, added in `650c57d`). Under the camelCase policy both become `busy`. System.Text.Json throws `InvalidOperationException` ("JSON property name ... collides") before any byte is written, so `GlobalExceptionHandler` probably answers **`400` with Subsonic code 10 `Operation not valid`** on every call. admin.js reads `run.busy` both as a count (L3860) and as a bool (L1546, L3894). No test covers this. **(verify; decide whether Rust copies the bug or fixes it, and record the choice in known-diffs.md)** |
| POST | `/api/admin/lyrics/library` | `StartLibraryRun` (L116) | body `{upgrade: bool, mode?: Walk\|Scan\|Preview\|Save\|Undo (default Walk), scope?, picked?: [string]}` | `202 {"started":true}`. FetchLyrics off for Walk or Preview gives `400`. Preview or Save with no picks gives `400`. Undo with nothing to undo gives `400`. Already running gives `409`. |
| POST | `/api/admin/lyrics/library/cancel` | `CancelLibraryRun` (L136) | none | `202 {"cancelling":true}` |
| POST | `/api/admin/lyrics/library/resume` | `ResumeLibraryRun` (L144) | none | `202 {"resumed":true}`. Nothing to resume gives `400`. Already running gives `409`. |
| POST | `/api/admin/lyrics/review/dismiss` | `DismissReview` (L156) | body `{path}` (required) | `200 {"ok":true}` |
| GET | `/api/admin/lyrics/songs` | `SearchSongs` (L169) | query `q` | `{songs:[{id,title,artist,album,duration,choice}]}`. Searches Navidrome's `search3` as Octo's admin (25 songs). A blank `q` gives `{songs:[]}`. No admin identity gives `400`. |
| GET | `/api/admin/lyrics/candidates` | `Candidates` (L199) | query `id?`, `path?` (looked up in the review list only), `artist?`, `title?` | `{id,path,artist,title,album,duration,choice,candidates}` within a 15 s budget. FetchLyrics off gives `400`. Song not found gives `404 {"error":"Octo could not find that song."}`. |
| POST | `/api/admin/lyrics/choice` | `Choose` (L238) | body `{id?, path?, candidate}` (`candidate` required: an id, `auto` or `none`) | `auto` gives `{id, choice:"auto"}`. `none` gives `{id, choice:"none"}` and needs a Navidrome id (`400` otherwise). A candidate gives `{id, choice, pinned, rewroteFile}`. FetchLyrics off gives `400`. A missing song or lyrics gives `404`. When the choice can be neither pinned nor written to a file, `400`. Pins record the setter as `"dashboard"`. |
| GET | `/api/admin/lyrics/choices` | `Choices` (L279) | header token | `{choices:[{id,artist,title,choice,source,kind ("hidden" or KindOf),setBy,setUtc}]}` |

### 4.10 Updates: `UpdateController`, base `/api/admin/update`

| Method | Path | Action (line) | Parameters | Gate | Response |
|---|---|---|---|---|---|
| GET | `/api/admin/update` | `Get` (L22) | none | G | `{enabled, repo, running, latest, newer, updateAvailable, standing, checkedUtc, error, helper:{installed:false}\|{installed:true,version,mode,dir,installedUtc}, pending, pendingId, unanswered, run:{id,tag,from,state,step,error,startedUtc,finishedUtc,log}\|null, command, imageCommand}` |
| POST | `/api/admin/update/check` | `Check` (L26) | none | G | Same shape after a manual GitHub check (rate-limited to once a minute) |
| POST | `/api/admin/update` | `Update` (L30) | body `{tag}` | G | `202 {"ok":true,"id","tag"}`. Each of these gives `409 {"error"}`: checks disabled, no newer release, `tag` not equal to the latest, helper missing, update already running. The requester is the browse-cookie user, or `"dashboard"`. |

---

## 5. Static files and other non-controller paths

| Path | Served by | Notes |
|---|---|---|
| `/admin/index.html`, `/admin/admin.css`, `/admin/admin.js`, `/admin/icons.svg`, `/admin/octo_logo.png`, `/admin/octo_social*.png` | `MapStaticAssets()` (wwwroot manifest endpoints) | MapStaticAssets endpoints carry ETag and caching headers and may serve precompressed files. Phase 1 must keep "the URLs and caching headers it has today", so capture these headers in the corpus. |
| `/Assets/*` (for example `/Assets/octo_logo.png`) | `MapStaticAssets()` (the csproj mirrors `Assets/**` into `wwwroot/Assets/`). `UseStaticFiles(RequestPath="/Assets")` is the fallback. | The catch-all would otherwise return 404 here (`assets/` is Octo-owned). |
| `/admin` and `/admin/` | `AdminRoot`, which redirects with 302 | `UseDefaultFiles` probably never fires: there is no `wwwroot/index.html`, and the route matches first. |
| `/admin/<missing file>`, `/api/admin/<unknown>`, `/favicon.ico` | catch-all, ProblemDetails 404 | A non-GET under `/api/admin/` without `X-Octo-Admin` gets the guard's 403 first. |
| `/swagger`, `/swagger/v1/swagger.json` | Swashbuckle, **Development only** | Not exposed in production. A parity target only if the Rust build adds an equivalent. |
| `/` | catch-all; see 2.2 (probably a 400 ProblemDetails) | **(verify)** |

---

## 6. Things to settle with the parity harness

1. **HEAD and other methods on `rest/*` routes.** They fall through to the catch-all: external ids get a synthetic ok, local ids get a faithful relay with HEAD (2.2).
2. **`/` with no endpoint.** Implicit `[Required]` gives a 400 ProblemDetails, or else a `NullReferenceException` gives 500 (2.2).
3. **ProblemDetails bodies** (`traceId` is volatile) for `NotFound()` and `StatusCodeResult`, and the implicit-required 400s on admin bodies (2.8).
4. **Local `stream` relay.** The upstream 206, `Content-Range` and `Content-Length` copied onto a non-seekable `FileStreamResult` with range processing on: confirm the status and headers the client actually gets, including when `Subsonic:Url` ends in `/` and `RelayStreamAsync` builds `//rest/stream` (2.7).
5. **`getSimilarSongs*` with radio off.** The relay goes to `{Url}//rest/...` (3.2). Does Navidrome accept the double slash?
6. **The `busy`/`Busy` name collision** in `GET /api/admin/lyrics/library`, which is probably answered as a 400 "Operation not valid" today (4.9).
7. **XML serialization.** Octo-built XML is indented and has no declaration, while relayed XML is byte-for-byte Navidrome's. The differ must not normalize one into the other.
8. **Content-Type spellings.** These must be reproduced exactly: `application/xml` with no charset (ContentResult and File), `application/json` with no charset (File), and `application/json; charset=utf-8` (JsonResult/Ok).
9. **CORS on `/api/admin*`.** Every `Access-Control-*` header is stripped, including on errors, and OPTIONS gets 204 with no allow headers.

---

## 7. Verified against the running C# image (2026-10-04)

Probed `octo-csharp:csharp-final` (no Navidrome configured, `Updates__Check=false`) with curl. These settle several **(verify)** items above.

- **`GET /`** → `400 application/problem+json; charset=utf-8`:
  `{"type":"https://tools.ietf.org/html/rfc9110#section-15.5.1","title":"One or more validation errors occurred.","status":400,"errors":{"endpoint":["The endpoint field is required."]},"traceId":"00-…-…-00"}`.
- **Bodiless 404** (`/favicon.ico`, `/admin/nope.js`): `404 application/problem+json; charset=utf-8` `{"type":"https://tools.ietf.org/html/rfc9110#section-15.5.5","title":"Not Found","status":404,"traceId":"00-<32 hex>-<16 hex>-00"}`. The key order is type, title, status, traceId.
- **Case and trailing slash:** `/REST/Ping.VIEW` and `/rest/ping/` both reach `ping`. Confirmed.
- **Method mismatch → catch-all:** `PUT /rest/ping`, `HEAD /rest/stream?id=x` and a non-preflight `OPTIONS /rest/ping` all reach the catch-all and are relayed. With no URL configured they answer 200 `application/xml` with code 0 `Error connecting to Subsonic server: Octo has no valid Navidrome URL. Set SUBSONIC_URL (Subsonic__Url) to your Navidrome server, e.g. http://192.168.1.10:4533 — an absolute URL reachable from the Octo container, not localhost.`
- **`ping` with no URL:** 200, `Content-Type: application/xml` (no charset), with an explicit `Content-Length`. The body is the code 0 error `Octo isn't configured yet. Open http://localhost:18080/admin and set your Navidrome URL (SUBSONIC_URL), then point this client at Octo instead of Navidrome.` (scheme://host taken from the request). With `f=json` it is `application/json; charset=utf-8`, chunked, and the apostrophe is escaped as `\u0027`. `f=jsonp&callback=cb` gives XML.
- **`/admin` and `/admin/`** → `302`, `Location: /admin/index.html`, `Content-Length: 0`.
- **CORS** on a normal request that carries `Origin`: `Access-Control-Allow-Origin: *` and `Access-Control-Expose-Headers: X-Content-Duration,X-Total-Count,X-Nd-Authorization` (comma-separated, no spaces). There is no `Vary`. Without `Origin`, no CORS headers.
- **CORS preflight** on a non-admin path (`OPTIONS` + `Origin` + `Access-Control-Request-Method`): `204`, `Access-Control-Allow-Headers` echoing the requested headers (comma-joined, no spaces: `X-Foo,content-type`), `Access-Control-Allow-Methods` echoing the requested method, and `Access-Control-Allow-Origin: *`. No body, no `Content-Type`.
- **Admin guard:**
  - `GET /api/admin/settings` with `Origin` has no `Access-Control-*` headers.
  - `OPTIONS /api/admin/settings` → bare `204` with no CORS headers.
  - `POST` without `X-Octo-Admin` → `403 application/json; charset=utf-8` `{"error":"Admin changes must come from Octo's dashboard. A script can send the X-Octo-Admin header to opt in."}`.
- **Static files** (`/admin/*.html|css|js|svg`, `/Assets/*`):
  - `Content-Type` has no charset (`text/html`, `text/css`, `text/javascript`, `image/png`, `image/svg+xml`).
  - `Accept-Ranges: bytes` appears **twice**.
  - `Cache-Control: no-cache`.
  - `ETag: "<base64 of the SHA-256 of the body>"`.
  - `Last-Modified` is the build time.
  - `If-None-Match` with the ETag → `304` (same headers, no body).
  - `Range: bytes=0-9` → `206` with `Content-Range: bytes 0-9/<len>`.
  - With `Accept-Encoding: gzip, br`, the precompressed Brotli variant is served: `Content-Encoding: br`, `Vary: Content-Encoding`, `ETag: "<sha256 of the compressed body>"` followed by `ETag: W/"<sha256 of the original>"`.
  - CORS headers are added as for any other request.
- **Every response** carries `Server: Kestrel` and a `Date` header.
- **More static-file facts** (probed by 2-C, same image):
  - Static paths match like routes: `/ADMIN/Admin.CSS`, `/assets/octo_logo.png` and `/admin/admin.css/` all serve the file. Only GET and HEAD: `POST`/`PUT` or an `OPTIONS` that is not a preflight reach the catch-all (ProblemDetails 404).
  - The plain variant of a compressible file has **no** `Vary`. PNGs have no compressed variant. Compressed: `.html`, `.css`, `.js`, `.svg`.
  - `Accept-Encoding`: highest q wins (`gzip;q=0.5, br;q=0.4` → gzip), Brotli on a tie, `q=0` excludes, `*`/`identity`/`deflate` → plain. With gzip: `Content-Encoding: gzip` and the same two-ETag pattern.
  - `If-None-Match` compares weakly against the served variant's **first** ETag only (`W/"x"` matches `"x"`; on the br variant the plain file's ETag does not match). A list and `*` work. A non-matching `If-None-Match` wins over a matching `If-Modified-Since`. `If-Modified-Since` equal to `Last-Modified` → 304; earlier or in the future → 200.
  - 304: same headers as the 200 minus `Content-Length` (Content-Type kept).
  - `If-Match` not matching, or `If-Unmodified-Since` before `Last-Modified` → `412` with only `Content-Length: 0`.
  - `Range`: one range only (`bytes=0-1,5-6` → 200 full); the unit is not checked (`items=0-1` → 206); `bytes=-5` → the last 5 bytes; past the end → `416` with only `Content-Range: bytes */<len>` and `Content-Length: 0`. `If-Range` with another ETag → 200 full. Ranges on the br variant count compressed bytes.
  - `HEAD` → 200 with the headers and **no** `Content-Length`.
  - `HEAD /admin` and `POST /admin` → catch-all 404 (`AdminRoot` is `[HttpGet]` only).
- **JSON written with `WriteAsJsonAsync`** (the admin guard's 403, `GlobalExceptionHandler`) uses the minimal-API encoder, `UnsafeRelaxedJsonEscaping`: `'` and non-ASCII (the em dash in the not-configured message) go out as is. MVC results (`JsonResult`, `Ok(..)`, ProblemDetails) use the default encoder (`isn\u0027t`).
- **`GlobalExceptionHandler` responses** also carry `Cache-Control: no-cache,no-store`, `Expires: -1` and `Pragma: no-cache`, plus the CORS headers when `Origin` was sent.
- **Two JSON encoders in MVC (settled by the 6-B1 parity run).** `new JsonResult(obj)` writes with the default encoder (`'` → `'`, `+` → `+`, non-ASCII escaped: `settings`, `status`, `config-sources`, the Subsonic envelopes). An `ObjectResult` (`Ok(obj)`, `BadRequest(obj)`, `Accepted(obj)`, `Conflict(obj)`, `NotFound(obj)`, `Unauthorized(obj)`, `StatusCode(n, obj)`, and the automatic ProblemDetails) goes through `SystemTextJsonOutputFormatter`, which in .NET 8+ swaps in `UnsafeRelaxedJsonEscaping` when no encoder is configured: `'`, `+` and letters like `ø` go out as they are (`browse`, `lastfm/scrobble/disconnect`, `tags/preview`, the settings 400s, `covers/upgrade/thumb?found=maybe`). Rust: `json_ok` for a `JsonResult`, `controllers::admin::plumbing::{ok, object_result}` (or `json_relaxed_response`) for an `ObjectResult`. `http::error::{problem, validation_problem}` still use the default encoder; this only shows when a problem's text holds such a character.
