# Octo: the Rust server, published as ghcr.io/winters27/octo for amd64 and arm64 (docker.yml),
# and built from this folder by docker-compose.yml's octo-source. The contract: port 8080
# (ASPNETCORE_URLS), /app/config for settings and state, /music for the library, /app/downloads
# created, and the app itself as PID 1 so SIGTERM reaches it and it drains requests and workers
# within its 10 s budget.
#
# From octo/ it takes the dashboard (octo/wwwroot), the logo and social images (octo/Assets),
# the list cover design (octo/Services/CoverArt/Design, built into the binary) and
# appsettings.json (built in). The release number is the VERSION file (else octo/octo.csproj,
# octo-core's build.rs).
#
#   docker build -t octo:dev .
#   docker build --build-arg OCTO_VERSION=2026.11.01.1 -t octo:dev .
#   docker build --build-arg CARGO_BUILD_JOBS=2 -t octo:dev .   # small machine

ARG RUST_VERSION=1.99
ARG CHEF_VERSION=0.1.78

# ---- Toolchain + cargo-chef, prebuilt ----
# Bookworm, like the runtime stage below: a binary linked against a newer glibc (the plain
# rust-1.99 tags are Debian trixie now) would not start on bookworm-slim.
FROM lukemathwalker/cargo-chef:${CHEF_VERSION}-rust-${RUST_VERSION}-bookworm AS chef
ENV CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0
WORKDIR /src

# ---- Dependency recipe: changes only when a Cargo.toml or Cargo.lock does ----
FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo chef prepare --recipe-path recipe.json

# ---- Build ----
FROM chef AS build
# Unset by default, so cargo uses every core the builder has. A machine short of memory for
# the LTO link and the heavier crates (lofty, image, cosmic-text) passes 2.
ARG CARGO_BUILD_JOBS
# Unset by default, so the release profile's thin LTO applies. The parity box, when its host is
# short of memory, passes "off": the same program without the 3 GB link (parity/box-run.sh
# --low-memory).
ARG CARGO_PROFILE_RELEASE_LTO
COPY --from=planner /src/recipe.json recipe.json
# Every third-party crate, compiled once and cached in this layer.
RUN cargo chef cook --release --locked --package octo --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY crates crates
# The release (VERSION; octo-core's build.rs reads it unless OCTO_VERSION is set), and what the
# binary compiles in from octo/: the list cover design (JSON, Inter fonts, painted
# backgrounds) and appsettings.json. octo.csproj is build.rs's fallback for a tree without
# VERSION.
COPY VERSION ./
COPY octo/Services/CoverArt/Design octo/Services/CoverArt/Design
COPY octo/appsettings.json octo/appsettings.Development.json octo/octo.csproj octo/
ARG OCTO_VERSION
RUN cargo build --release --locked --package octo --bin octo \
    && install -D -m 0755 target/release/octo /out/octo

# ---- Runtime ----
FROM debian:bookworm-slim
WORKDIR /app

# ffmpeg: Continuous Subsonic Radio normalizes mixed FLAC/M4A sources into one stable MP3
# response inside the Octo process, and the loudness and spectrum checks read through it.
# fpcalc (libchromaprint-tools) is the other half of download verification: it turns a
# finished download into the Chromaprint fingerprint AcoustID is asked about. Absent, the
# feature degrades to a no-op and logs once; it never fails a download.
# Generated list covers set names in Inter (inside the app); Noto CJK and Symbola draw the
# Chinese, Japanese, Korean and emoji names Inter has no letters for, DejaVu Arabic and Hebrew.
# ca-certificates: slim Debian carries no root store, and every upstream (Deezer, MusicBrainz,
# Last.fm, GitHub, ...) is HTTPS.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates ffmpeg fonts-dejavu-core fonts-noto-cjk fonts-symbola libchromaprint-tools \
    && rm -rf /var/lib/apt/lists/* \
    && mkdir -p /app/downloads /app/config

# The binary looks for wwwroot/ and Assets/ beside itself (StaticRoots::from_env, and the
# cover service's logo lookup).
COPY --from=build /out/octo /app/octo
COPY octo/wwwroot /app/wwwroot
COPY octo/Assets /app/Assets
# Inter is compiled into the binary; its license ships beside it.
COPY octo/Services/CoverArt/Design/Fonts/OFL.txt /app/licenses/Inter-OFL.txt

# No version label here: the published image gets org.opencontainers.image.version (and the
# rest) from docker.yml's metadata step, and the release itself is in the binary.
LABEL org.opencontainers.image.title="octo" \
      org.opencontainers.image.licenses="GPL-3.0-only"

EXPOSE 8080
ENV ASPNETCORE_URLS=http://+:8080

# Exec form: octo is PID 1 and receives SIGTERM from `docker stop` directly.
ENTRYPOINT ["/app/octo"]
