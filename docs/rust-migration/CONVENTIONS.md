# Rust port conventions

Read this before porting any part of `octo/` to Rust. The contract documents sit beside it:
[`endpoints.md`](endpoints.md) (routes), [`config.md`](config.md) (settings),
[`state-files.md`](state-files.md) (files on disk, with byte-exact fixtures in `fixtures/state/`) and
[`packages.md`](packages.md). The C# code at tag `csharp-final` (commit `15d6840`) is the
behavior to match. It is frozen. Since the cutover it is no longer in the working tree: read it
with `git show csharp-final:octo/<path>`, or check it out somewhere else
(`git worktree add ../octo-csharp csharp-final`).

## The rule

**Port behavior, not design.** Each C# type becomes a Rust type that does the same thing,
quirks included, and the C# tests become Rust tests that pass. When the C# behavior looks like a
bug, keep it. Then add a row to [`known-diffs.md`](known-diffs.md) only if you deliberately
diverged, saying why. Don't silently "fix" anything.

## Workspace layout

| Crate | Holds | C# sources |
|---|---|---|
| `octo-core` | `config` and `json` (done). Also `settings`, `models`, and the pure logic: `common`, `tagging`, `lyrics` (text/identity/choices), `metadata` (genre normalizer, accept-language), `fingerprint` (comparers, verification types), `updates` (release version), `validation` | `Models/**`, and the pure parts of `Services/**` |
| `octo-media` | `tags` (lofty), `audio` (ffmpeg/fpcalc: loudness, spectrum, fingerprint), `cover` (list-cover rendering, cover image files) | `Services/Audio`, `Services/Fingerprint/{AudioFingerprinter,SpectrumAnalyzer}`, `Services/Common/TagWriterExtras`, `Services/CoverArt/{CoverBook,CoverPainter,CoverLayout,CoverFonts,CoverColours,CoverBackgrounds,CoverVeil,CoverImage,CoverFiles}` |
| `octo-subsonic` | The Subsonic wire format: the XML/JSON response model, request parsing, credentials | `Services/Subsonic/{SubsonicRequestParser,SubsonicCredential}`, and the format-only parts of `SubsonicResponseBuilder` |
| `octo` | Everything with I/O or service wiring: HTTP clients, stores, workers, the acquisition pipeline, controllers, middleware, `main` | the rest |

Files beside the code that the build or the binary reads:

| Path | What | How it is used |
|---|---|---|
| `VERSION` | the release (`2026.10.04`) | compiled into `octo_core::VERSION` by `crates/octo-core/build.rs`; `OCTO_VERSION` at build time overrides it |
| `crates/octo-core/assets/` | the built-in `appsettings.json` and `appsettings.Development.json` | compiled in |
| `crates/octo-media/assets/cover-design/` | the list-cover design: JSON, Inter fonts, painted backgrounds | compiled in; `Fonts/OFL.txt` ships as `/app/licenses/Inter-OFL.txt` |
| `crates/octo/wwwroot/admin/` | the admin UI (HTML/JS/CSS, no build step) | served from disk: `/app/wwwroot` in the image, this directory in a debug build |
| `crates/octo/Assets/` | the logo and social images | served at `/Assets/...` and used as the cover watermark: `/app/Assets` in the image |
| `crates/octo-media/testdata/cover-golden/` | the cover goldens shared with the Octo apps | tests only |

Modules mirror the C# namespaces and file names, in snake case:
`Services/Common/SongIdentity.cs` → `octo_core::common::song_identity`, and
`Services/Lyrics/LyricsText.cs` → `octo_core::lyrics::lyrics_text`. Where a file has both pure
logic and I/O, split it: the pure half goes in `octo-core` and the I/O half in `octo`, under the
same module path (`octo::services::lyrics::…`).

Async methods drop the C# `Async` suffix (`MeasureAsync` → `measure`). Interfaces keep their C#
names, `I` included (`ILoudnessMeter`), so a grep for the C# name finds the Rust one.

Type names keep their C# names (`SongIdentity`, `TagPlan`, `ReleaseChooser`). Methods and fields
are snake case (`NormalizeIsrc` → `normalize_isrc`). A C# static class of helpers becomes a
module of free functions, or an empty struct with associated functions when call sites read
better that way (`SongIdentity::key(..)`). Pick one per type and stay with it.

## Code style

- **Comments:** carry the C# doc comments and the explanatory inline comments across, in
  English, as `///` and `//`. The C# is heavily commented on purpose, and the Rust should read
  the same way. Don't add comments that only restate the code.
- `cargo fmt` (the root `rustfmt.toml`, width 110), and `cargo clippy -- -D warnings` must be clean
  for the crates you touched.
- No `unsafe`. No `unwrap()` on anything that can fail at runtime because of input. `expect()`
  with a reason is fine for invariants.
- Errors: C#'s `Error`/`Result<T>` (`Services/Common/Error.cs`, `Result.cs`) become
  `octo_core::common::error::{Error, ErrorType}` and plain `Result<T, Error>`. Where C# throws and
  a caller catches, use `anyhow::Result` internally, or a `thiserror` enum when callers
  branch on the kind.
- Async: tokio. Hold a `parking_lot::Mutex` only when no `.await` happens inside it; otherwise use
  `tokio::sync::Mutex`. C# `SemaphoreSlim` → `tokio::sync::Semaphore`. `CancellationToken` →
  `tokio_util::sync::CancellationToken`.
- Small JSON state stores may use blocking `std::fs` under their lock, as the C# did with
  `File.WriteAllText`. Long file work in async code goes in `spawn_blocking`.
- Logging: the `tracing` macros, with the C# message text kept (`LogInformation("Fetched {Title}",
  t)` → `info!(title = %t, "Fetched {t}")` or just `info!("Fetched {t}")`). Keep the levels.
- **.NET string and number semantics:** use `octo_core::common::dotnet` (char classes, invariant
  casing, `IsNullOrWhiteSpace`, UTF-16 length, `Math.Round`, number formats) instead of Rust's own
  `is_alphabetic`/`to_lowercase`/`round`, which differ from .NET in edge cases. .NET regexes:
  `\b`, `\d`, `\w` and `\p{L}` only see the 16-bit range in .NET; see how
  `common::song_identity` handles that before porting a regex.
- **Checking against the real C#:** the .NET 9 SDK image (`mcr.microsoft.com/dotnet/sdk:9.0`) is
  available in Docker, and there is a local SDK at `~/.cache/octo-verify-dotnet`. When behavior is
  subtle, run the original C# beside your Rust on generated inputs instead of guessing.
- Time: services that the C# gave a `Func<DateTime>` clock or a `TimeProvider` take
  `octo_core::common::clock::Clock` (an `Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>` wrapper with
  a `system()` constructor). Async timers in tests use `tokio::time::pause()`.

## Settings

The settings structs live in `octo_core::settings`. They derive `Deserialize` with
`#[serde(default)]`, and their `Default` impls carry the C# property initializers. They are bound
by `octo_core::config::bind`, which matches field names ignoring case and underscores, so Rust
field names are just the snake case of the C# property names. The `Effective*` computed
properties become methods (`effective_rejected_peer_ttl_days()`).

`octo_core::settings::SettingsStore` holds the live snapshot (`ArcSwap<AppSettings>`), reloaded
when `/app/config/settings.json` changes:

- `IOptionsMonitor<T>.CurrentValue` (read at use, live): call `store.current().subsonic` at the
  point of use. Don't cache it.
- `IOptions<T>.Value` (captured at startup): copy the value out at construction, and comment
  that it is deliberately captured.
- `IConfiguration["Library:DownloadPath"]` and other raw keys: `store.raw("Library:DownloadPath")`.
- `OnChange`: `store.subscribe()`, which returns a `tokio::sync::watch::Receiver`.

In tests, build settings with struct literals plus `..Default::default()`, and a store with
`SettingsStore::for_tests(AppSettings { .. })`, whose `set()` stands in for `TestOptionsMonitor.Set`.

## JSON

- **State files** (everything under `/app/config`) must round-trip byte for byte. Write them with
  `octo_core::json::to_string` (compact) or `to_string_indented`. Those reproduce System.Text.Json's
  escaping, number and layout rules; serde_json's own writer does not. Field names are C#
  PascalCase: `#[serde(rename_all = "PascalCase")]`, plus a `rename` wherever the C# name isn't
  the plain PascalCase of the Rust name. Check every store against its fixture in
  `docs/rust-migration/fixtures/state/` with a read → write → compare test. Dates go through
  `octo_core::json::datetime` (`#[serde(with = "...::utc")]` and the related helpers). Enums are
  written as numbers unless `state-files.md` says otherwise. Computed C# properties that STJ
  serialized must still be written (implement `Serialize` by hand or add a serialized field).
- **API answers** (controllers) used ASP.NET's Web defaults: camelCase names
  (`#[serde(rename_all = "camelCase")]`), nulls written, enums as numbers unless a
  `JsonStringEnumConverter` was in play, and the default encoder's escaping. Answer with
  `octo_core::json::to_string`, not axum's `Json`, so non-ASCII escaping matches.
- **Request bodies:** read with `octo_core::json::web::from_slice` / `from_value`. These match
  field names case-insensitively and read numbers from strings, as ASP.NET's binder did.
- **settings.json** is edited through `octo_core::json::dom::Node`, which keeps number text as written.

## HTTP

- Outbound: `reqwest`. Each C# named client in `Program.cs` becomes one `reqwest::Client` with
  the same timeout, base URL, user agent (`octo_core::common::octo_user_agent`), decompression
  and cookie policy. The Deezer and AcoustID rate limiters wrap the client in a small type with a
  `send()` method that waits on the limiter first. Every caller has to go through that type, just
  as every C# caller had to resolve the named client.
- Tests that mocked `HttpMessageHandler` use `wiremock` (a real local server), or take a
  `base_url` so tests can point the client at the mock server.
- Inbound: axum 0.8 + tower-http. This is covered later in Phases 1 and 3.

## Interfaces and mocks

A C# interface becomes a Rust trait, using `#[async_trait]` when it is used as `dyn` and has async
methods. Where a C# test mocked a concrete class with Moq (virtual methods), add a trait at that
seam so the Rust test can substitute a fake. Write fakes by hand in the test module; don't
pull in `mockall` unless a fake would be much bigger than the test.

## Tests

- Port every test in the C# test files your task covers. Keep the C# method name in snake case
  (`NormalizeIsrc_RejectsShortCodes` → `normalize_isrc_rejects_short_codes`). An xUnit `[Theory]`
  with `[InlineData]` becomes one test that loops over a case table and names the failing case in
  the assert message.
- Unit tests go in a `#[cfg(test)] mod tests` at the bottom of the module, or in a sibling
  `tests.rs` (`#[cfg(test)] #[path = "song_identity_tests.rs"] mod tests;`) when they are long.
- Shared fixtures: `docs/song-identity-cases.json`,
  `crates/octo-media/testdata/cover-golden/samples.json`, and
  `docs/rust-migration/fixtures/state/*`. Read them from the repo through
  `env!("CARGO_MANIFEST_DIR")` with `../../` paths. Don't copy them.
- Tests that need ffmpeg or fpcalc skip themselves with a printed reason when the tool is
  missing, as `FfmpegFactAttribute` did.
- Record every C# test file you port in [`test-map.md`](test-map.md): the C# file, its test
  count, the Rust location, the Rust test count, and any test you dropped, with the reason.
  "Not portable" is acceptable only when the test exercised .NET machinery itself (for example
  `WebApplicationFactory` plumbing); in that case, port its intent.

## Building

The machine has 4 cores and 7 GB of RAM, and several porters share it.

- Point every build at the shared target directory, so dependencies compile once:
  `export CARGO_TARGET_DIR=/home/anhnt/Projects/octo/target CARGO_BUILD_JOBS=2`.
- If a build says a module you can see in the source does not exist ("cannot find `x` in
  `octo_core`"), the shared target directory served a stale build from another worktree. Run
  `touch crates/*/src/lib.rs` and build again.
- Never run `docker system prune`, `docker image prune` or `docker rmi`, and never delete
  images or volumes you did not create. The C# baseline image (`octo-csharp:csharp-final`) and
  the .NET SDK image are shared by everyone.
- Build and test only your crate, filtered: `cargo test -p octo-core song_identity`. Never run
  `cargo clean`, and never `cargo build --release`.
- Cargo is at `~/.cargo/bin` (`export PATH=$HOME/.cargo/bin:$PATH`).
- Check current docs with `npx ctx7@latest library <name> "<topic>"` and then
  `npx ctx7@latest docs <id> "<topic>"` before using an API you are unsure of. The workspace pins
  recent major versions: axum 0.8, reqwest 0.13, lofty 0.25, quick-xml 0.42, tiny-skia 0.12,
  cosmic-text 0.19, image 0.25, rand 0.10, notify 8.
- Add a dependency to the root `[workspace.dependencies]` first, then to the crate with
  `name.workspace = true`.

## Working alongside other porters

Several porters work at once, each in its own git worktree, and the results are merged after
each wave. **First check that your worktree is on the `rust-rewrite` branch's history**
(`git log --oneline -1 rust-rewrite` should be an ancestor of HEAD, and `crates/` should exist).
If it was created from `main` instead, run `git reset --hard rust-rewrite` before you start.

- Touch only the modules your task names, plus the `mod` lines that register them, and append to
  `test-map.md` and `known-diffs.md`.
- When you need a type another task owns and it doesn't exist yet, write the smallest stub at the
  path that task will fill, marked `// STUB(<owner task>): replaced when <task> lands`, and say so
  in your report.
- Commit your work in your worktree with a message like `feat(rust): port SongIdentity and
  common helpers`, and finish with everything committed.
- Your final report should list the C# files ported, the Rust modules created, the test counts
  (C# vs Rust), any stubs, every deliberate divergence, and anything left undone.
