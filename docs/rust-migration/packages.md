# NuGet packages: used, dead and their Rust replacements

Phase 0 task: "Confirm BouncyCastle is unused and list any other packages that are referenced but dead." Checked against commit `15d6840` by grepping every `.cs` file in `octo/` and `octo.Tests/` (bin/obj excluded) for the package namespaces and their entry-point APIs.

## Summary

| Package | Project | Status | Rust |
|---|---|---|---|
| BouncyCastle.Cryptography 2.6.2 | octo | **Dead.** Nothing references it | Drop it |
| Microsoft.AspNetCore.OpenApi 9.0.4 | octo | **Dead.** None of its APIs are called | Drop it |
| Swashbuckle.AspNetCore 9.0.4 | octo | **Registered but never served** in any shipped configuration | Drop it, or `utoipa` + `utoipa-swagger-ui` behind a cargo feature |
| SixLabors.ImageSharp 3.1.12 | octo | Used | `image` |
| SixLabors.ImageSharp.Drawing 2.1.7 | octo | Used | `tiny-skia` + `cosmic-text` |
| TagLibSharp 2.3.0 | octo | Used | `lofty` |
| coverlet.collector 6.0.2 | tests | **Inert.** CI never asks it to collect | `cargo-llvm-cov` if coverage is wanted |
| Microsoft.AspNetCore.Mvc.Testing 9.0.0 | tests | Used | `axum-test` or `tower::ServiceExt::oneshot` |
| Microsoft.NET.Test.Sdk 17.12.0 | tests | Used (it is the test host) | Built into `cargo test` |
| Moq 4.20.72 | tests | Used | `wiremock` for HTTP, `mockall` or hand-written fakes for traits |
| xunit 2.9.2 | tests | Used | `#[test]` / `#[tokio::test]`, `rstest` for `[Theory]` |
| xunit.runner.visualstudio 2.8.2 | tests | Used (it is what `dotnet test` runs) | Built into `cargo test` |

## octo/octo.csproj

### BouncyCastle.Cryptography 2.6.2: dead

- `grep -rn "BouncyCastle\|Org\.Bouncy"` over `octo/` and `octo.Tests/` finds no matches in any `.cs` file. The only mention is the `PackageReference` in `octo/octo.csproj`.
- Nothing needs it indirectly either. The download client's auth was a bearer token from its own session endpoint. Hashing (`SHA256`, `SHA1`) and `RandomNumberGenerator` come from `System.Security.Cryptography`.
- **Rust:** no replacement. Use `sha2` / `sha1` / `rand` (or `getrandom`) for the BCL crypto calls above.

### Microsoft.AspNetCore.OpenApi 9.0.4: dead

- There are no calls to `AddOpenApi`, `MapOpenApi` or `WithOpenApi`, and no `using Microsoft.AspNetCore.OpenApi` or `Microsoft.OpenApi`. The only OpenAPI wiring is Swashbuckle's (next section). Swashbuckle 9 brings its own `Microsoft.OpenApi` dependency and does not need this package.
- **Rust:** no replacement.

### Swashbuckle.AspNetCore 9.0.4: optional, effectively dead

- `octo/Program.cs:32` `builder.Services.AddEndpointsApiExplorer();` and `octo/Program.cs:33` `builder.Services.AddSwaggerGen();` run in every environment. They only register services.
- `octo/Program.cs:573-577` calls `app.UseSwagger(); app.UseSwaggerUI();` only when `app.Environment.IsDevelopment()`.
- Every shipped configuration sets `ASPNETCORE_ENVIRONMENT: Production` (`docker-compose.yml:9`, `octo/Properties/launchSettings.json:11,21`), so `/swagger` is never served.
- **Rust:** drop it. If a dev-only spec is still wanted, use `utoipa` + `utoipa-swagger-ui` behind a non-default cargo feature. The parity harness must not expect `/swagger`.

### SixLabors.ImageSharp 3.1.12: used

- For example `octo/Services/CoverArt/CoverArtService.cs:2`, `octo/Services/CoverArt/CoverVeil.cs:1` and `octo/Services/CoverArt/CoverPainter.cs:2`. It decodes JPEG, PNG and WebP, resizes, and encodes JPEG for cover art, the Octo badge, list covers, measuring cover sizes, and `cover.jpg` writing.
- The JPEG "Written by Octo" COM-segment marker is spliced in by hand (`CoverImage.MarkAsOcto`, `octo/Services/CoverArt/CoverImage.cs:149`), not through ImageSharp. Port it byte for byte (see state-files.md, `cover.jpg`).
- **Rust:** `image` (features `jpeg`, `png`, `webp`), plus `fast_image_resize` if resizing is slow.

### SixLabors.ImageSharp.Drawing 2.1.7: used

- `octo/Services/CoverArt/CoverPainter.cs:3` `using SixLabors.ImageSharp.Drawing.Processing;`. It draws text and paths for the list covers.
- **Rust:** `tiny-skia` for rasterising and `cosmic-text` for shaping and font fallback (Inter, then Noto CJK, Symbola and DejaVu). This is already in the plan.

### TagLibSharp 2.3.0: used

- `octo/Services/Common/BaseDownloadService.cs:13` `using TagLib;`, and `TagLib.File.Create(...)` in about 25 places (for example `Services/Common/TagWriterExtras.cs:251`, `Services/Lyrics/SongLyrics.cs:45`, `Services/CoverArt/CoverUpgrade.cs:649`).
- **Rust:** `lofty`.

### Implicit framework packages worth naming

These come from `Microsoft.NET.Sdk.Web`, not from a `PackageReference`, but they need a Rust choice too:

| .NET | Where | Rust |
|---|---|---|
| `Microsoft.Extensions.Caching.Memory` (`MemoryCache`) | `Services/Subsonic/CredentialCheck.cs:34`, `Services/Metadata/DeezerMetadataService.cs:127`, `Services/Library/GeneratedPlaylistService.cs:51` | `moka` (size-bounded, TTL) |
| `System.Text.Json` | every state file (see state-files.md) | `serde` + `serde_json`, with a custom formatter for STJ's escaping and number/date output |
| `Microsoft.Extensions.Configuration.Json` (`settings.json`, comments and trailing commas allowed) | `Program.cs:27` | `serde_json` cannot parse comments; use `json5` or `jsonc-parser` for reading |

## octo.Tests/octo.Tests.csproj

### coverlet.collector 6.0.2: inert

- A VSTest data collector. It does nothing unless a run passes `--collect:"XPlat Code Coverage"`. `.github/workflows/ci.yml:44` runs `dotnet test --configuration Release --no-build --verbosity normal` with no collector, and no workflow mentions coverage.
- **Rust:** none needed. `cargo-llvm-cov` if coverage is ever wanted.

### Microsoft.AspNetCore.Mvc.Testing 9.0.0: used

- `WebApplicationFactory<Program>` appears in 19 test files, for example `octo.Tests/GenreBackfillTests.cs:286` (`internal sealed class AdminWebFactory : WebApplicationFactory<Program>`) and `octo.Tests/ExternalPlaybackTests.cs:136`.
- **Rust:** build the `axum::Router` with test state and drive it with `axum-test` or `tower::ServiceExt::oneshot`.

### Microsoft.NET.Test.Sdk 17.12.0: used

- The VSTest host that `dotnet test` needs. No source references it.
- **Rust:** built into `cargo test`.

### Moq 4.20.72: used

- `using Moq;` appears in 43 test files, for example `octo.Tests/ExternalSearchServiceTests.cs:5`. Eleven of them also use `Moq.Protected` to fake `HttpMessageHandler.SendAsync`, for example `octo.Tests/ExternalSearchServiceTests.cs:5`.
- **Rust:** `wiremock` replaces the `HttpMessageHandler` fakes, since `reqwest` talks to a real local server. `mockall`, or plain hand-written fake structs, replace the interface mocks.

### xunit 2.9.2 and xunit.runner.visualstudio 2.8.2: used

- Every test file. `<Using Include="Xunit" />` is a global using. `[Theory]` appears in 68 files.
- **Rust:** `#[test]` / `#[tokio::test]`. Use `rstest` (`#[rstest] #[case(...)]`) for `[Theory]` / `[InlineData]`, and `insta` for snapshots.
