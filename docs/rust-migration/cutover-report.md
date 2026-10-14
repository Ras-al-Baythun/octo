# Cutover report (Phase 9 verification)

Verification of the finished Rust build before the cutover commit, run on 2026-10-05 against
`rust-rewrite` `f651ed5` (every port merged). The cutover itself (deleting the C# tree, CI and
`Dockerfile` swap, release tag) is not part of this report.

| | |
|---|---|
| Rust image | `octo-rust:final`, built from `f651ed5` with `Dockerfile.rust` (`CARGO_BUILD_JOBS=2`), 998,961,594 B |
| C# baseline | `octo-csharp:csharp-final` (release `2026.10.03.2`), 1,126,336,765 B |
| Machine | 4 cores, 7 GB RAM, shared with other work; every measurement below ran the two images one after the other on the same stack |

## Recommendation: **GO**, with the soak as the release gate

Every check that can run here passed: the parity corpus has no unexplained diff in either mode
and no flakes, both builds read each other's state and behave the same after a restart on it,
the dashboard's settings save writes byte-identical files, and Key Result 6 is met on every
measure with room to spare. Cut over on `rust-rewrite` as planned, but publish the first Rust
release only after the 7-day soak (Key Result 7, see "Remaining risks"). Keep
`octo-csharp:csharp-final` as the rollback: the downgrade test shows it reads everything the Rust
build writes.

## 1. Parity corpus

Details and the per-group table: [`parity-status.md`](parity-status.md).

- **453 / 453 requests, 0 unexplained diffs** in structural and bytes mode; `parity.py diff`
  exits 0 in both. 430 are identical apart from `Server`/framing; 23 are covered by the
  allowlist (7 cover JPEGs, 16 static-file answers), each pointing at a reviewed known-diffs row.
- **The cover JPEGs pass the perceptual bar:** SSIM 0.9926 (600×600 placeholder) and 0.9937
  (300×300 badged outside covers) against the C# bytes, above PLAN's 0.98.
- **Flakes: none.** The corpus ran 4 times (once under full `cargo test` CPU load). All four
  were clean, and the four Rust recordings are byte-identical to one another.
- **Fixes needed in `crates/`: none.** No diff called for a code change.

## 2. Upgrade and downgrade

### 2.1 C# → Rust on a C#-written install

The parity stack, with Octo's `/music` a writable copy of the fixture library, library actions
on (not a dry run; Delete, BetterQuality and Keep enabled; Review, sweep and duplicates on), genre
normalization on, a Last.fm secret, and a stub answering `auth.getToken`/`auth.getSession`.

1. **C# writes state.** The full corpus (453 requests, including `90-mutations`), then these
   extra flows: a heart on a local song and on a discovery-only song, a rating on an outside
   song, a Last.fm Connect + Finish (the session lands in `settings.json`), a scrobble, a
   settings save (`Server.PublicUrl`, a genre mapping), lyrics choices (`none`, `auto`),
   `libraryAction remove` (the file moves to `.octo-trash` with its `.octo-action.json`
   manifest), `libraryAction upgrade` plus `POST upgrades` (one job left **in flight**,
   `working`/`Searching`, when C# stops), a whole-library genre dry run, a cover Scan, a lyrics
   Scan, the review sweep, a duplicate scan and a rejected-peers clear. Then the read-only part
   of the corpus (groups 00-10, 395 requests) was recorded, and C# was stopped with
   `docker stop`. It left 13 state files: `browse-sessions`, `cover-upgrade`, `external-ids`,
   `genre-backfill`, `library-actions`, `lyrics-choices`, `lyrics-library`, `notice-queue`,
   `rejected-peers`, `review-sweep`, `settings`, `upgrades` (`.json`), plus two quarantine
   manifests under `/music`.
2. **Rust starts on the same volumes** (`docker compose up -d --no-deps octo` with the Rust
   image) and answers `ping` 1.0 s later. Its log shows every store loading and the queues
   resuming: `external id registry restored 14 entries`, `library action journal restored 4
   entries`, `Library actions reconciled 1 interrupted entr(ies)`, and the in-flight upgrade
   job restarts (`search-for-star: 'Bjørn Ålund Hjarta'`). **No warning or error about
   any state file**; the only `WARN` line is lofty's `MPEG: Using bitrate to estimate duration`.
3. **Dashboard settings:** `GET /api/admin/settings` and `GET /api/admin/raw-config` from Rust
   are identical to C#'s answers on the same file (Last.fm session, genre mapping and the
   redacted secrets included).
4. **Read-only corpus:** C# before the stop vs Rust after the upgrade: 390 of 395 match. The 5
   that differ are restart effects, not format problems: the acquisition queue and the last
   duplicate-scan notice live in memory (C# loses them on a restart too; see 2.2), and the
   genre-backfill run on display is the one the corpus's own `POST genre/backfill` started
   (with genre on it is no longer a refusal), so each recording shows the run before it.
5. **Files Rust changed** while running: `browse-sessions` (the corpus signs in),
   `genre-backfill` (the corpus's run), `library-actions` and `upgrades` (the resumed job's
   `StartedUtc`/`UpdatedUtc`/`AtUtc` only). Everything else stayed byte for byte as C# wrote it.

### 2.2 Rust → C# (downgrade) on the Rust-written install

C# was then started on the volumes Rust had just written. It logs the same three restore lines
(`restored 14`, `restored 4`, `reconciled 1 interrupted`), no state-file warnings, the same
settings answers, and its read-only corpus recording matches Rust's post-upgrade recording on
**394 of 395** requests. The one diff is the genre run id described above. Since C# restarting
on Rust's state and Rust restarting on C#'s state give the same answers, the 2.1 diffs are
confirmed as restart effects shared by both builds.

**Files the C# build cannot read: none found.** Every file Rust wrote was read back by C#.

### 2.3 The fixture state as `/app/config`

`docs/rust-migration/fixtures/state/*` (all 21 JSON/JSONL stores, `update/`, the radio sidecar
under the cache dir, and `.mappings.json` plus a quarantine manifest under `/music`) was copied
into a fresh config, cache and music mount, once per build, on the parity stack.

- **Load:** 23 endpoints that expose the stores (settings, raw-config, status, library status,
  radio, scrobbling, downloads, acquisitions, library actions, notices, review sweep, quality
  upgrade, upgrades, genre backfill, cover upgrade, lyrics library/songs/choices, update,
  playlists, internet radio, starred) answer **identically** from the two builds (timestamps
  made at run time aside). No load warnings in either log.
- **Write-back after a flush (graceful stop):** both builds changed exactly the same 4 files and
  left every other fixture file byte for byte. `external-ids.json` (and a since removed hold file) came
  out **byte-identical** between the builds. `upgrades.json` differs only in the resumed job's
  start time. `browse-sessions.json` holds the same three sessions, but in a different order:
  C# writes `ConcurrentDictionary` order, which depends on .NET's per-process string hashing,
  and Rust appends. Either build reads either order; recorded as a new known-diffs row
  (`browse-sessions.json` row order).
- The byte-exact read → write → compare of each store is covered by the unit tests (29 test
  modules read the fixtures); this run confirms the same holds with the real binary and the
  real mounts.

### 2.4 Settings save round trip

GET `/api/admin/settings`, POST the same document back (as the dashboard's Save does), and read
`settings.json`. Starting from the C#-written file of 2.1 (→ 6,345 B) and from the fixture
(→ 6,260 B), **the file Rust writes is byte-identical to the file C# writes**, the POST answers
are identical, and a GET after the save matches the GET before it.

## 3. Load test

Client: stdlib Python threads with keep-alive connections, 8 workers × 500 requests per endpoint
(4,000 each, after 50 warm-up requests), against the parity stack (fixture Navidrome and stubs),
one image at a time, two rounds in alternating order. Requests: `search3?query=Aurora` (local
plus discovery: Last.fm, the shim and track lookups), `getAlbum` (the FLAC album, merged),
`stream` with `Range: bytes=0-65535` (relayed 206), `getCoverArt&size=300`.

**The stubs were patched for this test** (`disable_nagle_algorithm = True` in a scratch copy).
With the checked-in stubs every keep-alive upstream call can wait for a 40 ms delayed ACK, which
puts a floor of ~86 ms under `search3` for both builds and turns its distribution bimodal (86 or
126 ms), so it measured the stub, not Octo. The checked-in stubs were not changed, to keep the
baseline recording valid (see `parity-status.md`).

Latency in ms, p50 / p95, the two rounds:

| Endpoint | C# round 1 | C# round 2 | Rust round 1 | Rust round 2 | Rust p95 vs C# (mean of rounds) |
|---|---|---|---|---|---:|
| `search3` | 53.2 / 82.3 | 50.6 / 79.4 | 49.4 / 75.6 | 45.4 / 69.6 | **−10%** |
| `getAlbum` | 17.6 / 28.0 | 16.8 / 25.9 | 15.4 / 23.2 | 15.3 / 23.6 | **−13%** |
| `stream` (Range) | 12.3 / 23.4 | 11.6 / 22.4 | 10.2 / 18.9 | 9.9 / 18.3 | **−19%** |
| `getCoverArt` | 7.8 / 19.7 | 7.4 / 19.3 | 6.0 / 15.9 | 6.2 / 15.8 | **−19%** |

Throughput moved the same way (for example `getCoverArt` 880-910 → 1,095-1,130 requests/s). No
request failed in any run. Sequentially (1 worker), `search3` p50/p95 is 9.5/12.3 ms for Rust
and 11.4/14.7 ms for C#; with the stock stubs the sequential `getAlbum`, `stream` and
`getCoverArt` were 25-30% faster in Rust, and the concurrent run gave the same p95 for both
builds within ±3% (the stub floor dominates).

### Operational numbers vs Key Result 6

| Measure | C# | Rust | Target | Result |
|---|---|---|---|---|
| Idle RSS, lone container (`docker stats`, 10 s and 30 s after start) | 209.5 MiB | 41.0 MiB (**19.6%**) | ≤ 50% of C# | met |
| Idle RSS in the stack, before load | 218-222 MiB | 27-42 MiB | - | - |
| Peak RSS under the load above | 310-325 MiB | 32-47 MiB (**~13%**) | - | - |
| Cold start, `docker run -d` → first `200` (6 runs) | 0.96-1.03 s, median 1.01 s | 0.50-0.55 s, median **0.51 s** | ≤ 1 s | met |
| Image size | 1,126,336,765 B | 998,961,594 B (**88.7%**) | ≤ C# | met |
| p95 on the four endpoints | see above | 10-19% lower | no worse | met |
| Graceful `docker stop`, no job in flight | 0.16 s | 0.11 s | - | - |

With a download search in flight both builds take ~10 s to stop: each waits out its 10 s worker
shutdown budget for the acquisition worker (Rust logs `Abandoned background workers at shutdown:
AcquisitionWorker`). Same behavior, nothing lost (the job resumes, see 2.1).

## 4. Tests and flakes

- `cargo test --workspace` **3 runs, all green**: 2,465 tests each time (octo 1,569, octo-core
  648, octo-media 132, octo-subsonic 113, request-line integration 3). The first run overlapped
  three parity runs. A fourth run after the change below was also green.
- **`route_tests::radio::published_stream_consumes_and_replenishes_three_track_session_pool`
  (reported flaky) hardened.** It did not fail here (radio tests 15× under 6 CPU burners, the
  whole `octo` suite 4× under 3 burners: 0 failures), but the cause is visible in the code:
  `RadioFixture::wait_for_calls` returned **silently** after 1 s, and the test's pool polls and
  `started()` waits gave up after 1-2 s. On a loaded machine the initial pool fill could still be
  running when the test reset the transcoder and raised `complete_calls`, so a late fill landed
  in the pool and the counts were off by one. Now `wait_for_calls` waits up to 10 s and **panics**
  if the calls never come, the pool polls allow 10 s, and the "a transcode started" and
  first-bytes waits allow 10 s. Passing runs are as fast as before; only a starved run waits
  longer. (`crates/octo/src/controllers/subsonic/route_tests/radio.rs`)
- No other flaky test was found.

## 5. Every residual difference

All are rows in [`known-diffs.md`](known-diffs.md). Those that showed up in this verification:

| Seen in | Difference | known-diffs row |
|---|---|---|
| every response | no `Server: Kestrel`; `Content-Length` instead of chunked; lower-case header names | HTTP |
| 16 static answers | `Accept-Ranges` once; different Brotli/gzip bytes; variants built in the background, so a request in the first second may get the plain body | Static files (three rows) |
| 7 cover answers | JPEG bytes from the `image` encoder, SSIM ≥ 0.9926 | List covers: JPEG bytes |
| upgrade test | `browse-sessions.json` row order | `browse-sessions.json` row order (6-C), **new** |
| first run | Octo no longer adopts itself as the Navidrome URL | First-run discovery |

The rest of `known-diffs.md` (about 135 rows) covers paths the corpus does not reach (error
texts from .NET exceptions, tag-writing edge cases, cancellation, enum values no member names,
concurrency fixes); each was reviewed when its port merged.

## 6. Remaining risks

1. **No soak yet (Key Result 7).** The 7-day run against a real download client and Navidrome library,
   with a daily state-file and tag audit, cannot run here. It is the release gate: watch for
   panics, stuck workers (the supervisor logs abandoned workers), and state files that stop
   round-tripping.
2. **The acquisition pipeline end to end.** The download stubs return no search results, so a real
   download, tagging with lofty, fingerprint verification and file placement only ran in unit
   tests and in the fixture library, never through the HTTP surface. Same for Lidarr.
3. **Radio, stations and mixes, scrobbling to real Last.fm/ListenBrainz** are off or stubbed in
   the harness (their timers would make the corpus non-deterministic); covered by unit tests only.
4. **Tag writing across real-world files.** lofty and TagLibSharp differ on ID3v2.3/2.2 corner
   cases (known-diffs "Audio tags"); the soak's tag audit should look at those formats.
5. **Clients.** The admin UI was exercised through its HTTP contract, and Symfonium's sync walk
   is in the corpus, but the Octo desktop/Android apps, Feishin and the updater helper
   (`scripts/updater/test-updater.sh`) were not run against the Rust image in this pass.
6. **Memory under a large library.** RSS was measured with a 10-track fixture; the stores
   (external ids, rejected peers, iTunes masters) are bounded LRUs, but the soak should record
   RSS daily.

## How to repeat

The drivers used here (an upgrade/downgrade driver over `parity.py`, the load client, the
cold-start probe and the SSIM check) lived in a scratch directory; the procedure is fully
described above and needs only `parity/` plus a compose override that mounts a writable `/music`
and, for the load test, a stubs copy with `disable_nagle_algorithm = True`.
