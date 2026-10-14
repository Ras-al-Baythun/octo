# Rust port backlog

<!-- markdownlint-configure-file
{ "MD013": false }
-->

The Rust server in `crates/` is at parity with C# 2026.10.04. The C# app in
`octo/` is at 2026.10.05 and moving. This file is the working list for closing
that gap: every C# change since the Rust baseline, grouped, with its parity
diffs and its status. Tick a commit when its behavior, its tests (a
`docs/rust-migration/test-map.md` row each) and its parity cases are on
`rust/main`.

Baseline: Rust at 2026.10.04 parity. Target: `main` at `ce63143` (release
2026.10.05 plus a README commit). The gap is `git log --no-merges
89a4be1..ce63143`, 121 commits, all listed below.

## Working on the port

- **Where cargo runs.** WSL Ubuntu, on its own filesystem (not `/mnt/c`).
  Windows can `cargo check --workspace --tests` for quick edits (2 min 8 s
  cold), but the test suite is Linux only.
- **One worktree, one `target/`.** Each worktree builds into its own
  `target/`; never point several at one `CARGO_TARGET_DIR`. Incremental
  builds are on, so a `target/` grows to about 7.3 GB (3.8 GB of it
  incremental caches). Keep 30 GB free in WSL.
- **One memory budget in WSL.** Run cargo through `~/bin/cargo-slot`. It is
  now a weighted budget of WSL's 16 GB for builds and parity stacks together
  (`parity/budget.sh`, README "WSL's budget"): 12.5 GB, a cargo build 3.5 GB
  (a test rebuild peaks at 2.3 to 3.4 GB with incremental caches, 4 to
  5.3 GB cold), a parity stack 1.25 GB. A build that does not fit waits in
  line; a stack never takes the last build's share. `cargo-slot --status`
  shows who holds what.
- **Parity runs: `parity/run.sh`.** One command, the options of
  `parity/lxc131.sh`. It runs in WSL's own Docker by default, as many stacks
  at once as the budget allows (the runs of several branches side by side),
  or on the test box with `--lane box`. The run outlives the script:
  `--attach` follows it again and fetches its reports, `--queue` shows both
  lanes. `parity/lxc131.sh` still works for the box.
- **The loop.** `cargo test -p <crate> <filter>` while working; the full
  `cargo test --workspace`, `cargo clippy --workspace --tests -- -D warnings`
  and `cargo fmt --all --check` before merging into `rust/main`.
- **Shared files.** The Rust build reads the C# app's own files: the
  dashboard (`octo/wwwroot`), `octo/Assets`, the cover design
  (`octo/Services/CoverArt/Design`), `octo/appsettings.json`, the golden
  cover samples (`crates/octo-media/tests/fixtures/cover-golden`), the shared case files
  (`docs/*-cases.json`) and the release number (`octo/octo.csproj`). A debug
  run serves the dashboard from `octo/wwwroot` with no env vars.
- **Port behavior, then improve on purpose.** Follow
  `docs/rust-migration/CONVENTIONS.md`. A deliberate improvement over the
  C# behavior is fine: give it a `docs/rust-migration/known-diffs.md` row
  (C#, Rust, why) and a parity allowlist entry pointing at it.

## Phase 0 record (2026-10-06)

| Item | Result |
| --- | --- |
| Workspace | Imported at 2026.10.04 parity; `sonic/` excluded (cargo refuses to run in `sonic/` without it; checked) |
| `cargo build --release` (WSL, 16 cores, cold, crates already downloaded) | 2 min 40 s wall, 16 min CPU; peak memory 3.0 GB over idle (the LTO link alone 3.1 GB); binary 33.7 MB |
| `cargo test --workspace` cold | 6 min 18 s wall; peak 4.7 GB over idle |
| Tests | 2,470 pass, 0 fail (octo 1,574 + 3, octo-core 648, octo-media 132, octo-subsonic 113) |
| The 7 failures seen in planning | Environment only: under WSL mirrored networking a closed port such as 127.0.0.1:1 times out instead of refusing. Fixed: the tests use a port bound and let go, which refuses everywhere; the `octo` tests went from 187 s to 51 s |
| Shared song identity cases | 26 of the 2026.10.05 cases fail in Rust (group B); skipped by name in `song_identity_tests.rs`, and a skipped case that starts passing fails the test until it is taken off the list |
| `cargo clippy --workspace --tests -- -D warnings` | Clean, 2 min 41 s cold |
| Edit and rebuild the tests, incremental off (the fork's setting) | Edit in `octo` 89 s, peak 4.0 GB; edit in `octo-core` 87 s, peak 3.9 GB |
| Same, incremental on (now the default) | Edit in `octo` 12 s, peak 2.3 GB; edit in `octo-core` 28 s, peak 3.4 GB |
| `target/` size | 2.8 GB without incremental caches, 7.3 GB with them |
| Windows | `cargo check --workspace --tests` passes (the symlink test is Unix only) |
| `Dockerfile.rust` on LXC 131 (4 cores, cold, own buildx builder capped at 3.5 GB) | 5 min 12 s in all: dependencies (cargo-chef cook) 1 min 44 s, the Octo crates 2 min 43 s; image 993 MB |
| Workflows | `rust.yml`, `docker-rust.yml`, `ci.yml` pass actionlint 1.7.12 with shellcheck, apart from `concurrency.queue`, which actionlint does not know yet (`docker.yml` has the same line and runs) |

Not measured yet, because nothing is pushed: the CI run times on GitHub's
runners, the GitHub Actions cache sizes for the `octo-rust-amd64` and
`octo-rust-arm64` scopes against the repository's 10 GB, and the `sonic` job
on `rust/main`. Record them after the first push of `rust/main` and the first
manual run of the Rust image workflow.

## Parity: first run against C# 2026.10.05

Run on LXC 131 on 2026-10-06 with the corpus as it came from the Rust port
(453 requests, 12 files), the published image
`ghcr.io/winters27/octo:2026.10.05` against `Dockerfile.rust` built from
`rust/main` (`19b375f`).

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against C# (two runs) | 453 identical | 453 identical |
| Rust against Rust (two runs) | 453 identical | 453 identical |
| C# against Rust | 172 unexplained, 281 explained by the allowlist | 172 unexplained |

So the corpus as it stands is deterministic on both builds, and every diff is
a real difference.

Diffs by corpus file:

| Corpus file | Requests | Diffs |
| --- | ---: | ---: |
| `00-setup` | 6 | 1 |
| `01-system` | 43 | 4 |
| `02-browsing` | 65 | 5 |
| `03-search` | 25 | 5 |
| `04-media` | 42 | 2 |
| `05-playlists` | 19 | 3 |
| `06-lyrics` | 24 | 0 |
| `07-octo-extensions` | 15 | 2 |
| `08-catchall-native` | 34 | 4 |
| `09-admin` | 97 | 97 |
| `10-static-cors` | 25 | 18 |
| `90-mutations` | 58 | 31 |
| **Total** | **453** | **172** |

Diffs by gap group (one request can show several groups):

| Group | Diffs | What differs |
| --- | ---: | --- |
| J | 148 | The sign-in gate (401 with `signIn` where Rust answers), the security headers on admin answers (`Content-Security-Policy`, `X-Frame-Options`, `Referrer-Policy`, `X-Content-Type-Options`, `Cache-Control: no-store`), the `browse/session` fields `recovery`, `signInOff`, `user`; 144 of the 172 are J alone |
| P | 10 | `explicitStatus` on outside songs and albums |
| Q | 8 | The Made for you playlist in `getPlaylists`; `octoLists` extension |
| G | 5 | `octoTopSongs` extension; `getTopSongs` answered by Octo (no relayed `Vary: Origin`) |
| D | 4 | `octoAcquisitions` extension at versions 1 and 2 |
| H | 4 | `octoImports` extension |
| N | 3 | `pending` on `genre/backfill` (the activity view) |
| B | 2 | `Content-Disposition` on a relayed download |
| F | 2 | `getLibraryActions` lists 6 actions; `libraryAction` knows retag, joinAlbum, cover, lookup, undo, restore |
| O | 2 | `ownedCount` on outside albums |
| K | 1 | `GET /` is a 302 to `app/` |
| L | 1 | `services.sonic` in `/api/admin/status` |

Read with care: the C# gate answers 401 to almost every admin request, so the
admin features of other groups (D, E, F, G, H, L, N, P, Q, R) cannot show yet.
Phase 1 signs the corpus in and adds the corpus groups below; most of the
backlog only becomes visible then. Groups A, C, E, I, M, R, S, T and U have no
diff in this corpus.

Repeat it (on a box with Docker; LXC 131 used its own folder and compose
projects, cleaned up after):

```bash
docker build -f Dockerfile.rust -t octo-rust:dev .
python3 parity/parity.py run --project p0cs --out /tmp/parity/cs
python3 parity/parity.py run --image octo-rust:dev --project p0rs \
    --port 18580 --nd-port 18553 --out /tmp/parity/rs
python3 parity/parity.py diff /tmp/parity/cs /tmp/parity/rs
```

## Phase 1: the harness at 2026.10.05

Done 2026-10-07 (`parity/README.md` has the details):

- [x] The corpus signs in for real (`00-setup` makes an admin and a recovery
  code sign-in; `"session"` on a request sends the cookie), and keeps the
  signed-out answers: `11-signin` asks every `/api/admin` GET signed out and
  signed in, `99-signed-out-writes` every write signed out (last, the restart
  last of all). Both are written from the C# controllers by
  `parity/tools/admin_routes.py`, whose `check` fails when a route is missing.
- [x] `ADMIN_SIGN_IN=off` and `ADMIN_PORT`: a second Octo in the parity stack
  (`octo-alt`, `"server": "alt"` and `"alt-admin"`).
- [x] Corpus groups for every 2026.10.05 group: `12-imports`, `13-downloads`,
  `14-settings-media`, `15-radio`, `16-lists`, `17-covers-explicit`,
  `18-dashboard`, `19-health`, `20-library-edits`, `21-static`,
  `95-acquisition` (two hearts and a pick end to end through the download stub,
  with a real fixture FLAC and M4A), `96-covers-explicit-writes`. 1,836
  requests in 25 files (453 before).
- [x] Stubs: Spotify accounts, API and embed pages, ListenBrainz labs, the
  shim's YouTube Music answers, octo-sonic, Deezer and Last.fm charts, the
  download client's search results, transfers (the stub writes the file into
  Octo's job folder) and options (the client has since been removed). New stub features:
  `times`, `bodyRegex`, `searchText`, `writeFile`, `{{size:...}}`, dated
  templates.
- [x] Determinism rule: timers off or waited for, random values captured,
  C#'s own unordered answers compared unordered; what cannot be made
  repeatable is listed in `parity/README.md` ("Determinism rule"). No
  test-only seed env var: the C# image is the published one, so only its own
  settings can hold it still.
- [x] Gate: C# against C# 0 diffs and Rust against Rust 0 diffs on the whole
  corpus, structural and bytes (record below).
- [x] One script for the test box: `parity/lxc131.sh` (README, "For port
  agents").
- [x] Parallel parity lanes in WSL: `parity/run.sh` picks WSL or the box, and
  WSL's `cargo-slot` is one memory budget for builds and stacks
  (`parity/budget.sh`; README "The WSL lane", record below).
- [ ] `parity/recordings/csharp` stays the 2026.10.03.2 baseline: the Rust
  golden tests (`subsonic_wire_golden_tests.rs`) pin the wire format the port
  has today. A group that changes those answers moves its golden tests onto a
  fresh 2026.10.05 recording in its own commit (`parity.py run`, then copy the
  corpus files it needs).
- [x] Regenerate the state fixtures with the C# generator at 2026.10.05
  (`docs/rust-migration/fixtures/state`): done in wave G1, with the 7 new
  files, `admin-recovery-code` and the new fields (state-files.md §4.26 on).
- [ ] Regenerate the tag fixtures (`fixtures/tags`) at 2026.10.05: group C
  (the explicit advisory, `ITUNESADVISORY` and `rtng`), with its tagging.

## Phase 1 record (2026-10-07)

Run on LXC 131 from `rust/main` with `parity/lxc131.sh --csharp-twice
--rust-twice --bytes`: the published `ghcr.io/winters27/octo:2026.10.05`
against the Rust build of this branch (still at 2026.10.04 behavior).

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against C# | 1,836 identical | 1,836 identical |
| Rust against Rust | 1,836 identical | 1,836 identical |
| C# against Rust | 1,458 unexplained, 360 explained by the allowlist, 18 identical | the same |

A C# recording of the whole corpus takes about 25 minutes, a Rust one about
30 (the waits for features it lacks time out; `--until-patience` keeps that
bounded). The two gate runs were also made on the corpus before its last
compare-only fixes (ignored keys, member order), then compared again with the
final corpus: 0 in all four comparisons.

**The port's worklist.** Requests with an unexplained diff, per backlog group.
"Own": requests tagged with the group that differ. "Diffs": requests anywhere
whose differing part `parity/groups.json` puts on the group (J counts the
missing admin security headers on every admin answer; L, P, Q count settings
and extension lines in other groups' answers). One request can count in
several groups. Single: requests whose only differing group is this one, so
porting the group alone makes them pass.

| Group | Tagged | Own | Diffs | Single |
| --- | ---: | ---: | ---: | ---: |
| A Last.fm search cleanup | 5 | 5 | 5 | 5 |
| B Download matching | 19 | 18 | 22 | 9 |
| C Tagging polish | 16 | 15 | 15 | 3 |
| D Download log, Find songs | 144 | 133 | 131 | 64 |
| E (removed) | 45 | 45 | 45 | 1 |
| F Library edits, trash | 157 | 140 | 145 | 110 |
| G Top songs, charts | 57 | 55 | 56 | 37 |
| H Spotify import | 208 | 208 | 207 | 96 |
| I Secret masking | 78 | 76 | 76 | 2 |
| J Admin sign-in | 262 | 252 | 927 | 225 |
| K Bare address 302 | 6 | 5 | 6 | 5 |
| L Multi-source radio | 87 | 75 | 136 | 29 |
| N Activity, Afterglow | 48 | 48 | 44 | 6 |
| O Album ownership | 21 | 20 | 26 | 1 |
| P Covers, explicit | 114 | 106 | 145 | 38 |
| Q Made for you | 20 | 14 | 63 | 6 |
| R Library health | 150 | 137 | 130 | 45 |
| S Stream length | 25 | 5 | 4 | 5 |
| base (2026.10.04 behavior) | 479 | 204 | 164 | 18 |

The 18 single "base" diffs are knock-on effects of the groups, not
regressions: the OpenSubsonic extensions list (2026.10.05 lists 12, Rust
9), the Made for you playlists in `getPlaylists`, the relayed download file
name, and `90-mutations`
reading a library that `20-library-edits` changed. J goes first (wave G1): 225
requests differ only by the gate and the admin security headers.

Diffs per corpus file: 00-setup 4, 01-system 4, 02-browsing 5, 03-search 5,
04-media 2, 05-playlists 3, 07-octo-extensions 2, 08-catchall-native 4,
09-admin 97, 10-static-cors 18, 11-signin 130, 12-imports 206, 13-downloads
108, 14-settings-media 149, 15-radio 74, 16-lists 60, 17-covers-explicit 74,
18-dashboard 63, 19-health 101, 20-library-edits 136, 21-static 31,
90-mutations 36, 95-acquisition 35, 96-covers-explicit-writes 34,
99-signed-out-writes 77. 06-lyrics has none.

**Intentional diffs.** `parity/intentional-diffs.json` holds the deliberate
improvements, one reviewed entry per change (requests named, the diff lines
pinned by regex, a reason, and `reviewed` by Brandon for anything a user or an
app sees). Wave G1 proposed the first three (`reviewed` unset, so they explain nothing
until Brandon signs them): `recovery-refused-on-library-files`,
`lidarr-test-saved-key-stays-home` and `config-sources-fixed-mask`, the port
fixes below that the corpus reaches. The list under "Improvements we want while
porting" below is where the rest come from.

### Found while building the harness

C# 2026.10.05 behavior the corpus records as it is. **Decided 2026-10-07
(Brandon): all eleven are fixed in the Rust port only, as intentional diffs**
(a known-diffs.md row and a `parity/intentional-diffs.json` entry each,
`reviewed` left for Brandon before shipping). Five are fixed in wave G1
(marked below); the other six carry a "fix in port" line under their group.

- **"Keep as playlist" on an import answers 500** against Navidrome 0.64.2:
  Octo reads Navidrome's answer to the playlist create as JSON, and it is not
  (`NavidromePlaylistApi.cs:143`; likely the `application/json; charset=utf-8`
  content type, so Navidrome takes the body as an M3U import). Navidrome makes
  an empty playlist named after the time, the list stays marked as kept, and
  the 30 s import loop makes another one on every retry. `12-imports` turns
  each playlist off at once and deletes what it leaves.
- **Clearing an ISRC does nothing on disk** (`TagWriterExtras.cs:98-101` returns
  on an empty value), so undoing an ISRC retag answers `isrc: null` and leaves
  the tag in the file.
- **The explicit backfill accepts a recovery code session**
  (`ExplicitBackfillController.cs:30`, `UserOf`), so the recovery code can
  write explicit marks into music files; the cover upgrade refuses it.
  **Fixed in G1** (`4d38b38`): the gate refuses a recovery-code session on
  every route that reads or changes library files
  (`admin_sign_in_gate::LIBRARY_FILE_ROUTES`: browse, tags/preview,
  library-actions, notices, lossy, upgrades, library/resolve, genre/backfill,
  covers, lyrics, imports, downloads/log, find, trash, health, explicit),
  with the words each endpoint answered; group P's controller needs no check
  of its own.
- **`lidarr/test` sends the saved Lidarr key to any address typed in** when the
  placeholder is sent (`AdminController.cs:2372`); the "a secret never follows
  a new address" rule covers saves only.
  **Fixed in G1** (`4d38b38`, `lidarr_test_key`): the saved key follows the
  saved address only; another address answers 400 "Retype the Lidarr API key
  for the new address.". The other test routes send nothing saved to a typed
  address: `lastfm/check` (fixed Last.fm host), `listenbrainz/validate`
  (fixed ListenBrainz host), `test-notification` (the saved ntfy and
  Discord addresses only), the port test of a since removed group (no secret), the Spotify
  connect (a client id, not a secret).
- **A Navidrome address saved with a trailing slash** passes the same-address
  rule, and the stream relay then calls `//rest/stream`
  (`SubsonicProxyService.cs:344`). **Fixed in G1** (`4d38b38`): saved
  trimmed and without trailing slashes (settings and Raw config), and read
  that way (`normalize_server_url` when the settings bind).
- **Section names in a settings save:** the secret checks ignore case, the file
  writer does not, so `"lidarr"` writes a second section the config loader
  rejects on reload. **Fixed in G1** (`4d38b38`,
  `writer::canonical_sections`): a known section is written under its own
  name, and two spellings of one become one.
- **`config-sources` shows a secret's length** (up to 16 bullets). **Fixed in
  G1** (`4d38b38`): eight bullets for every set secret.

The two G1 fixes no corpus request reaches (Navidrome's address with a trailing
slash, a section name in another case) have their known-diffs.md rows and Rust
tests but no intentional-diffs.json entry: a parity entry names the requests
whose answers differ, and none do. A corpus request for each would need its own
C# recording run; a settings save with `http://navidrome:4533/` would also move
every later request in its corpus file onto the normalized address.
- **Resuming a cancelled cover run counts `kept` and `albumsDone` again**
  ("6 of 3 albums", `CoverUpgrade.cs:696,744`).
- **The cover scan walks into `/music/.octo-trash`**, and the album id of a
  trashed song depends on the dated folder.
- **An outside song's registry id ignores its album** (`ExternalIdRegistry.cs`,
  `MakeShortId`): radio registers the Zephyr Echo songs from Last.fm without an
  album and replaces the search's entry, so later answers lose the album until
  a search registers them again (`95-acquisition` starts with that search).
- **A web address of a since removed group was saved with the spaces it was typed with.**
  **Fixed in G2 with E** (`port/g2-eg`): saved (settings and Raw config,
  `normalize_saved_addresses`) and read (`AppSettings::bind`) trimmed; the proposed
  intentional diff was `web-url-trimmed` (`18-dashboard/settings-weburl-ok`, `-ok-read`).

### G1 record: groups A, K and S (2026-10-07)

Run on LXC 131 from `port/g1-aks` (`7411096`) with `parity/lxc131.sh --no-build --rust-twice
--bytes`, against the cached C# recording of the same corpus.

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against Rust | 1,433 unexplained (1,458 in phase 1), 385 explained, 18 identical | the same |
| Rust against Rust | 1,835 identical, 1 unexplained | the same |

| Group | Own before | Own after | Diffs before | Diffs after |
| --- | ---: | ---: | ---: | ---: |
| A | 5 | 0 | 5 | 0 |
| K | 5 | 0 | 6 | 0 |
| S | 5 | 0 | 4 | 0 |
| B (`0a700c0`, the relayed file name) | 18 | 11 | 22 | 11 |
| base | 204 | 201 | 164 | 163 |

Every other group is as in phase 1 except R, one more (138 own, 131 diffs): the Rust against
Rust diff, `19-health/plant-wait`. That request stops polling once the 7 planted songs are
listed, while Navidrome's full scan is still filling in album covers and MusicBrainz ids, so
which album comes first and whether it has a cover depends on the scan's timing. It touches
none of A, K and S, and it was identical in the phase 1 gate runs; the fix is a wait for the
scan to finish (`getScanStatus`) before it, in the corpus.

The image was not built by the harness: the Proxmox host had about 3.6 GB to spare (another
container's load swings by 4 GB a minute), and both the usual build and `--low-memory` tripped
the watchdog while compiling the `octo` crate. It was the same commit built in WSL as a static
musl binary (`cargo build --release --target x86_64-unknown-linux-musl`, since a WSL glibc
build needs glibc 2.38 and the image is bookworm's 2.36), put into `Dockerfile.rust`'s own
runtime stage on the box and tagged `octo-rust-parity:port-g1-aks`; the runs then used
`--no-build`. The targeted run first (`--only '[01][348]-*'`, 456 requests): A, K and S at 0;
03-search 5 (unchanged), 04-media 2 to 0, 08-catchall-native 4 to 3, 13-downloads 108 to 101,
14-settings-media 149 to 134, 18-dashboard 63 (unchanged).

### G1 record: groups J and I (2026-10-07)

Run on LXC 131 from `port/g1-core` (`25b28bc`, rebased on `rust/main` `4a17b3b`
with A, K and S) with `parity/lxc131.sh --bytes --rust-twice`: the binary
compiled in WSL, the corpus with `19-health/plant-scan-done` (so the C# side was
recorded again, 1,837 requests). The numbers below are that recording diffed again
with `978acef`'s allowlist (the two static-file entries also cover `21-static`
and the admin port's index) and the three proposed intentional diffs.

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against Rust | 809 unexplained (1,433 after A, K and S), 1,004 explained, 24 identical | the same |
| Rust against Rust | 1,837 identical, 0 unexplained | the same |

| Group | Own before | Own after | Diffs before | Diffs after |
| --- | ---: | ---: | ---: | ---: |
| J | 252 | 6 | 927 | 45 |
| I | 76 | 3 | 76 | 3 |

What is left in J and I is the port fixes and one J bug the run found:

- I 3 and J 3 are the proposed intentional diffs: `config-sources` with eight
  bullets (6 requests) and `lidarr/test` refusing the saved key at a new address
  (1). With them reviewed, I is 0 and J is 3.
- J 3: a sign-out answered the gate's cookie renewal ahead of the cookie's
  deletion (`14-settings-media/sign-out-this-browser`, `sign-out-everywhere`,
  `recovery-sign-out-everywhere`; `90-mutations/browse-signout` the same).
  ASP.NET's `Cookies.Delete` took back the gate's `Append`, so the C# sent the
  deletion alone. Fixed in `05bf281` with a Rust test; the targeted run below
  checks it.
- J's 45 "diffs" are requests of other groups whose differing part
  `groups.json` puts on J too; none is J's own.

No group got worse. Own counts after, against the A, K, S record: B 11 (11),
C 12 (15), D 110 (133), E 21 (45), F 128 (140), G 48 (55), H 202 (208), L 45
(75), N 14 (48), O 20 (20), P 68 (106), Q 12 (14), R 97 (138), base 35 (201);
A, K and S 0. Most of the drop is the gate and the security headers, which every
admin answer carried. The other groups' recovery-code requests now meet the
gate's refusal (`17-covers-explicit/explicit-*-recovery*`, intentional) or
their unported route (`13-downloads/find-get-recovery`, D;
`18-dashboard/ambient-recovery`, `activity-recovery-signed`, N;
`18-dashboard/sharing-get-recovery`, E).

Rust against Rust is 0 again: `19-health` now waits for Navidrome's scan to
finish (`plant-scan-done`, `getScanStatus` until `scanning` is false) before it
lists the planted songs.

The harness now compiles in WSL by default (`parity/lxc131.sh`, `box-run.sh
--prebuilt`, README "Compile in WSL"); `--box-build` is the old way.

### G2 record: groups C, O and Q (2026-10-07)

Run on LXC 131 from `port/g2-coq` (`be07a5f`, merged with `rust/main` `13c48b5`) with
`parity/lxc131.sh --rust-twice`: the binary compiled in WSL, against the cached C# recording of
the corpus (1,837 requests). The box run outlived the local script, which stopped at its two
hour limit, and `box-run.sh` stopped with it before the C# against Rust diff; that diff was
then run on the box by hand with the same arguments.

| Comparison | Structural |
| --- | --- |
| C# against Rust | 786 unexplained (809 after J and I), 1,026 explained, 25 identical |
| Rust against Rust | 1,837 identical, 0 unexplained |

| Group | Own before (phase 1) | Own after J and I | Own after | Diffs before (phase 1) | Diffs after |
| --- | ---: | ---: | ---: | ---: | ---: |
| C | 15 | 12 | 9 | 15 | 9 |
| O | 20 | 20 | 17 | 26 | 0 |
| Q | 14 | 12 | 5 | 63 | 12 |

What is left is other groups' work, none of it C's, O's or Q's own:

- C 9: all `95-acquisition` (`heart-neon-*`, `pick-lima-*`). They wait on D's
  `getAcquisition` (404 in Rust), D's Find songs pick and the download log's `Key` and `Log`,
  and two `getSong` requests miss a capture those requests make. The tagging itself matches:
  the five `17-covers-explicit/tags-preview-*` requests (C and J) are identical now that J's
  headers are in.
- O 17 own, 0 diffs: every differing part of those requests is put on another group.
- Q 5 own, 12 diffs: `16-lists/listener-popular-*` (4) and the first list in
  `listener-playlists-lists-*` are G's Popular right now; `16-lists/extensions-*` (4) differ by
  G's `octoTopSongs`, H's `octoImports` and a second version of an earlier extension; the two
  `12-imports/nd-find-playlist-*` miss H's imported playlist, while the Deep Cuts list Octo
  makes beside it matches. `groups.json` puts Popular right now on Q as well as G.

No group got worse against the J and I record: B 11, D 110, E 21, F 128, G 47 (48), H 201
(202), I 3, J 3 (6), L 45, N 14, P 68, R 97, base 29 (35); A, K and S 0. R is tagged on 151
requests (150) with the corpus's `plant-scan-done`. The proposed intentional diff
`isrc-clear-takes-the-tag-off` matched its one aspect (`20-library-edits/tags-copy-undone`, an
F request); with it reviewed that request no longer differs for the ISRC.

Made for you list order was left out of the gate, as planned; Rust against Rust is 0 on the
whole corpus all the same.

### G2 record: group E (2026-10-08)

Branch `port/g2-eg`, rebased on `rust/main` `bbc1b1e`. Group G is not in it: it is owned by
`port/g-charts` (the Apple Music charts plan), and no G code was written here.

Run on LXC 131 from `port/g2-eg` (`662b0ba`, on `13c48b5`) with `parity/lxc131.sh --only
'1[18]-*' --bytes`: the binary compiled in WSL, the C# side recorded again for the subset (204
requests, 195 compared).

| Group | Own before (phase 1) | Own after J and I | Own after |
| --- | ---: | ---: | ---: |
| E | 45 | 21 | 5 |

What was left in E, structural and bytes alike:

- `18-dashboard/sharing-share-bad-body`: the C# answered `{"on": "maybe"}` with STJ's own
  words naming the record (`... converted to
  (removed controller)+ShareRequest. Path: $.on | LineNumber: 0 |
  BytePositionInLine: 14.`, under `$.on`, after `request`). Fixed after the run (`2537f75`,
  the switch's body refuses a member that is not a bool in those words; read from the box's C#
  recording, with a Rust test); the final run below confirms it.
- `18-dashboard/settings-weburl-ok` and `-ok-read`: the port fix, proposed as
  `web-url-trimmed` (`reviewed` unset, so still counted).
- `18-dashboard/activity-rescan-starting` and `-done` (tagged N and E): `/api/admin/activity`
  is N's route (404 in Rust); E's half (removed) was in place for it.

So E's own count is 4 after the fix, 2 with the intentional diff reviewed, and 0 once N's
activity view lands. The other groups' own counts in the subset (D 3, F 3, H 2, I 0, J 3 for the
proposed `config-sources` bullets, L 1, N 14, P 1, R 3, base 3) are their own unported routes
and proposals; the subset cannot show the whole corpus, which the final run below was for.

The final run: `parity/lxc131.sh --rust-twice --bytes` on the whole corpus from `7d56cfe`
(rebased on `3c4c593`, the detached box run), against the cached C# recording (1,837 requests).
A first attempt on `fb3f28f` stopped before it built: the box had 7.0 GB free and needs 8 (this
branch's own image and folder were removed with `--clean`; the space came back once other runs
ended).

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against Rust | 768 unexplained (786 after C, O and Q), 1,044 explained, 25 identical | the same |
| Rust against Rust | 1,837 identical, 0 unexplained | the same |

E is 4 own, as expected: `sharing-share-bad-body` now matches the C# byte for byte, and what is
left is `settings-weburl-ok` and `-ok-read` (the proposed `web-url-trimmed`, which matched
its 2 aspects in both modes and is waiting for review) and the two `activity-rescan-*` requests
of N. No group got worse against the C, O and Q record: B 11, C 9, D 110, F 128, G 47, H 201,
I 3, J 3, L 45, N 14, O 17, P 68, Q 5, R 96 (97), base 29; A, K and S 0.

### G3 record: group L (2026-10-08)

Branch `port/g3-l` (`1b4d021`, merged with `rust/main` `cd89293`). A first targeted run on
LXC 131 (`--only '1[15]-*'`, `694e9ee`, 218 requests compared) left nothing of L's own: every
`15-radio` diff was another group's. The final run: `parity/run.sh --rust-twice` on the WSL lane,
the whole corpus (1,837 requests) against the cached C# recording, 49 minutes.

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against Rust | 741 unexplained (768 after E), 1,069 explained, 27 identical | not run |
| Rust against Rust | 1,837 identical, 0 unexplained | 1,837 identical, 0 unexplained |

| Group | Own before (phase 1) | Own after J and I | Own after | Diffs after |
| --- | ---: | ---: | ---: | ---: |
| L | 75 | 45 | 18 | 6 |

Song radio, station provenance, bans, learning, the outcome stats, Sounds alike's pass and
controls and the octo-sonic status card all match. What is left on L is other groups' work:

- L 18 own: 15 `getSimilarSongs` answers differ only by P's `explicitStatus` on one outside song;
  the 3 `extensions-radio-*` answers by D's second `octoAcquisitions` version and G's and H's
  extensions (octoRadioSources is listed and placed as the C# places it).
- L 6 diffs: those 3 extension answers, and `09-admin/update`, `11-signin/in-get-update` and
  `90-mutations/update-check`, whose update commands pull `yt-dlp-shim` and `octo-sonic`
  (`610dba2`, group T, the release phase); `groups.json` puts them on L for `octo-sonic`.

The registry fix shows in no request of this corpus: the C# answers carry the album too
(`95-acquisition` searches again before it reads the songs, as the harness notes said), so no
`intentional-diffs.json` entry is proposed: like G1's two unreachable fixes, it has a Rust test
and a known-diffs row. No group got worse against the E record: B 11, C 9, D 110, E 4, F 128,
G 47, H 201, I 3, J 3, N 14, O 17, P 68, Q 5, R 96, base 29; A, K and S 0.

The full run was started on the box first and stopped while queued; WSL full runs on `78234a5`
stopped silently after the image build (`run.sh`'s ADAPTIVE substitution under `set -e`), fixed
on `rust/main` in `cd89293`.

### WSL lane record (2026-10-08)

The parity harness now also runs in WSL's own Docker (29.1.3, compose 2.40), many stacks at
once, through `parity/run.sh` (README "The WSL lane"), under one memory budget with the cargo
builds (`parity/budget.sh`, installed as `~/bin/cargo-slot`: 12.5 GB, a build 3.5 GB, a C#
stack 1.25 GB, a stack of the Rust build 0.75 GB).

`rust/main` (`3c4c593`, the same code and corpus as the G2 record's `be07a5f`) with
`parity/run.sh --lane wsl --rust-twice`: the C# recording made afresh in WSL beside the Rust one
and the Rust one again, three stacks at once.

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against Rust | 786 unexplained, 1,026 explained, 25 identical (the G2 box record's numbers) | not run |
| Rust against Rust | 1,836 identical, 1 unexplained | 1,836 identical, 1 unexplained |

The one Rust against Rust diff is `19-health/plant-wait`: its `search3` was answered while
Navidrome was still giving the planted albums their covers after `plant-scan-done` saw the
scan end (one recording has `coverArt` on the albums, the other not, and the two albums
in another order). It showed with five stacks and port agents' builds going at once; the
sharded run below with `--rust-twice` had it 0. A corpus fix (wait for the planted albums'
`coverArt`) is for whoever owns the corpus.

| Run (WSL) | Stacks at once | Wall time |
| --- | --- | --- |
| `rust/main` `--rust-twice`, C# not cached | 3 (C# 741 s, Rust 3,087 s, Rust again 3,088 s) | 51 min run, 55 min with the musl compile |
| `port/g2-bd` (`eb59c77`) beside it and `port/g2-coq` | 1 each (they waited for, then shared, the C# recording `rust/main` was making) | 47 and 52 min run; 684 and 786 unexplained |
| A trial four-shard plan, C# not cached, Rust waits from C# | 4 C# shards, then each Rust shard after its C# one | 28 min |
| The same, C# cached, `--rust-twice` | 8 Rust shards | 17 min; Rust against Rust 0 in both modes |

On the box the same `rust/main --rust-twice` is about 11 min of C# (when not cached) and two
45 to 55 min Rust recordings one after the other, about 2 hours, after the wait for the box
lock (seven runs were queued for it while these ran). The trial plan was a timing test with
the shards work in progress; its C# against Rust count (795) is not yet the whole corpus's.
The longest shard (15-radio to 18-dashboard) took 707 s against C# and about 1,000 s against
Rust, so that shard sets the time of a sharded run.

### G3 record: group H (2026-10-08)

Branch `port/g3-h` (`b0de7a5`, on `rust/main` `cd89293`), the whole corpus on the WSL lane with
`parity/run.sh --rust-twice` against the cached C# recording (1,837 requests). A targeted run
before it (`--only '[01][126]-*'` on LXC 131, 547 requests) found the same 16.

| Comparison | Structural |
| --- | --- |
| C# against Rust | 584 unexplained (768 after E), 1,225 explained, 28 identical |
| Rust against Rust | 1,836 identical, 1 unexplained (bytes the same) |

| Group | Own before (phase 1) | Own after J and I | Own after |
| --- | ---: | ---: | ---: |
| H | 208 | 202 | 16 |

All 16 are the Keep as playlist fix, proposed as `keep-as-playlist-navidrome-takes-it`
(`reviewed` unset, so still counted; with it reviewed H is 0, checked with a local diff of the
recordings): `playlist-on` and `app-playlist-on` answer 200 with the playlist's song count where
the C# answered 500; the lists then carry their `playlistId` (`list-gym-after-playlist`,
`overview-trickle-off`, `getImports-paused`, `overview-after-skips`, `list-gym-after-skips`,
`app-getImport-public`, `getImports-listener-paused`, `getImports-listener-after-read-7`) and
their removal says the playlist stays (`remove-gym`, `app-remove`); and Navidrome's own playlist
is named after the list, holds its songs and belongs to its owner (`nd-find-playlist-gym`,
`nd-playlist-gym`, `nd-find-playlist-listener`, `nd-playlist-listener`; the corpus finds it by
either name, so it needed no change, and deletes it as before, so later files see none).
Everything else in `12-imports` matches, the state files included.

No group got worse: B 11, C 9, D 110, E 4, F 128, G 47, I 3, J 3, L 45, N 14, O 17, P 68, Q 5,
R 97, base 29; A, K and S 0. The extension lists (`01-system/extensions-*`,
`16-lists/extensions-*`) now carry octoImports and differ only by G's octoTopSongs and L's
octoRadioSources. Rust against Rust's one diff is `19-health/plant-wait` (R), the scan timing
the G1 and E records saw.

### G2 record: groups B and D (2026-10-08)

Branch `port/g2-bd`, merged with `rust/main` up to `cd89293` (G1, C, O, Q and E in). Final run
on the WSL lane, `parity/run.sh --lane wsl --rust-twice --bytes`, from `fb30026`, against the
cached C# recording of corpus `28fc0d5b71a3fce9`; its recordings diffed again with `375b829`'s
allowlist (one entry added after the run, below). The box run before it (`eb59c77`, without E)
gave the same B and D counts and also had Rust against Rust at 0.

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against Rust | 664 unexplained, 1,146 explained, 27 identical | the same |
| Rust against Rust | 1,837 identical, 0 unexplained | the same |

| Group | Own before (G1 J and I) | Own after B and D | Own now (with C, O, Q, E) | Diffs now |
| --- | ---: | ---: | ---: | ---: |
| B | 11 | 5 | 2 | 2 |
| D | 110 | 21 | 12 | 10 |

"Own after B and D" is the run from `cd6ad65` (B and D on G1, before C was in). No other group
got worse in any of the runs; the other groups' counts move only with their own merges.

What is left in B and D is other groups' work:

- **P** (`5cea252`, `86ad6b8`): the cover line in the log. C#'s `DownloadCoverResolver` names
  the chosen cover with its size and the covers it passed over ("Cover from the catalog, 300 x
  300 px", "It came with no cover of its own. Also looked at: ..."); the port writes C#'s own
  fallback line, as the C# does when the resolver gives no headline. That one line is B's 2
  (`heart-neon-log`, `pick-lima-row`) and D's live and saved logs, history file and rows (10).
- **G and H**: `13-downloads/extensions-json` and `extensions-xml` list 10 extensions against
  12 (`octoTopSongs` and `octoImports` missing); `octoAcquisitions` matches at versions 1 and 2.

Found and settled on the way:

- `13-downloads/find-library-song-refused` showed Find songs without a library song's length:
  F's `ed0bd7c` (Navidrome's native API sends 301.37 seconds), ported here with its test, since
  Find songs' length check needs it.
- Find songs' small cover of a library song (`95-acquisition/heart-neon-find-cover`) joins the
  allowlist's `cover-jpeg-bytes` entry: the same 160 px picture, from the `image` crate's
  encoder instead of ImageSharp's.
- A relayed download's `Content-Range` total (`heart-neon-relay-download`,
  `pick-lima-relay-download`, tagged B for `0a700c0`'s file name, which matches) differs by a
  few hundred to a few thousand bytes: lofty lays out a tagged FLAC or M4A otherwise than
  TagLib#, while the tags and streams match (`heart-neon-tags`, `pick-lima-tags`). A new
  known-diffs row, "Audio tags: a tagged file's length", and the allowlist entry
  `tagged-file-length` (that header on those two requests only).
- `02cc06b` (the log's "Marked explicit" and "Took off the peer's own album tags") was ported
  once C was on `rust/main`.

The order of "No lyrics found" against "In the library folder" is a race in both builds (the
lyrics look runs in the background); the saved logs are compared unordered.

No intentional diffs and no settings were added. `696cbff`'s import half waits for H, and
`5d3f096`'s Recently removed and activity card for F and N.

### G3 record: groups F and P (2026-10-08)

Branch `port/g3-fp` (`c4ce8f1`: `port/g3-f` and `port/g3-p` merged, with `rust/main` up to
`19db112`, so B, D, E, G, H and L are in). The whole corpus on the WSL lane with
`parity/run.sh --rust-twice` against the cached C# recording (1,837 requests), then
`--only '20-*' --rust-twice` to settle the Rust against Rust diffs.

| Comparison | Structural |
| --- | --- |
| C# against Rust | 260 unexplained, 1,524 explained, 53 identical |
| Rust against Rust (whole corpus) | 1,835 identical, 2 unexplained (`20-library-edits/album-without-cafe`, `plant-copy-album`) |
| Rust against Rust (`20-*` alone) | 152 identical, 1 unexplained (`plant-copy-album`) |

| Group | Own before (G2 record) | Own after | Diffs before | Diffs after |
| --- | ---: | ---: | ---: | ---: |
| F | 128 | 24 | 133 | 24 |
| P | 68 | 15 | 99 | 15 |

What is left in F is R's and G's:

- **R** (14 requests): `plant-copy-seen` waits on Library health (`/api/admin/health`, 404 in
  Rust) for the planted second copy of Café del Mar and captures its id there
  (`song_cafe_copy`, `key_copy`). Without it every request about the copy fails or differs:
  `plant-copy-album`, `retag-copy-isrc` and `-seen`, `tags-copy-isrc`, `undo-copy-isrc`,
  `tags-copy-undone`, `remove-copy` and `-seen`, `folder-after-copy-removed`,
  `trash-files-copy`, `admin-trash-copy`, `trash-admin-copy`, `allow-download-copy`.
- **R, through the journals** (6): `admin-actions-off`, `-dryrun`, `-cafe`, `-final`,
  `tag-edits-journal` and `library-actions-journal` list the fixes `19-health` made in the C#
  run (the Kestrel Moor songs) and the copy's removal. Run alone (`20-*`), the first three match.
- **G** (3): `extensions-on-json`, `-xml` and `extensions-off-again` differ only by
  octoTopSongs at versions 1 and 2, G's proposed `charts-octo-top-songs-version-2`.
- `album-without-cafe` (1): Navidrome's album genres while its scan of the removed song is still
  going; it differed Rust against Rust in the whole run only, and matched C# and itself when
  `20-*` ran alone (the timed-out health waits of `19-health` and `20-library-edits` move the
  scans' timing).

The two Rust against Rust diffs are the same thing: `plant-copy-album` lists the planted copy
under Navidrome's own id, which changes every run and is normalized only through the capture
`plant-copy-seen` makes from the health route. Both go once R is in.

What is left in P is J's, R's knock-on and the proposed intentional diffs:

- **Intentional, not reviewed** (3): `17-covers-explicit/covers-resumed-done`
  (`cover-resume-counts-once`), `96-covers-explicit-writes/covers-scan-done`
  (`cover-scan-leaves-the-trash-out`; the trash entry the C# listed, plus the copy below) and
  `covers-apply-aurora-done` (`cover-run-reason-cleared`, plus the copy).
- **J's `recovery-refused-on-library-files`** (2): `explicit-get-recovery`,
  `explicit-cancel-recovery-idle`.
- **R's knock-on** (9): the planted mp3 copy that `20-library-edits` could not remove without
  the health capture is still in the library, so the explicit marking reads 12 songs against
  11 (`explicit-preview-1-done`, `-2-done`, `explicit-apply-1-done`, `-2-done`,
  `explicit-undo-1-done`, `-2-done`, `explicit-state-after-apply-1`) and the cover run
  embeds into it too (`covers-journal-after-apply`, `covers-undo-done`).
- **base** (1): `covers-start-bad-json`, the admin JSON binder's `$` against the C#'s `$.scope`
  for a cut-off body (`plumbing.rs`).

No other group moved because of F or P. The counts that differ from the earlier records come
with `19db112` (G's charts plan): every settings answer carries `GeneratedPlaylists.Charts` and
`ChartCountry`, which puts the settings reads of E (20), I (23) and base on those groups, and
the dashboard files (`dashboard-files-2026-10-08`, not reviewed) put `21-static` on J and N.
G's `charts-top-chart-says-which-chart` now matches its three `16-lists/top-chart-*` requests,
as expected with P's `explicitStatus` in.

Settled on the way:

- D carried F's `ed0bd7c` too (`571de77`); the merge keeps D's copy and one test row.
- P's cover line in the downloads log (`log_cover_step`) now writes through D's
  `log_song_step`, which clears the cover line B and D were waiting on (B 0, D 2 in this run;
  D's 2 are the extension lists, G's octoTopSongs version).
- F changed shared `octo-media` code: an MP3 rewritten in place keeps its ID3v2 tag size while
  the frames fit, as TagLib# did (`tag_file::taglib_padding`, known-diffs "Audio tags: ID3v2
  padding"). No other group's count moved with it.

No settings were added, and no route was added to `LIBRARY_FILE_ROUTES`: the new admin routes
(`/api/admin/library-actions/allow-download`, `/api/admin/explicit/backfill/*`) sit under
prefixes G1 already listed.

### G4 record: group N (2026-10-08)

Branch `port/g4-n`, on `rust/main` `cd89293` (group E and the WSL lanes merged in). Run on the
WSL lane with `parity/run.sh`: first `--only '1[18]-*'` from `f6b9ee1` (C# recorded again for
the subset, 195 compared), then the whole corpus with `--rust-twice` from `7d1f3f2` against
the cached C# recording (1,837 requests, 1h04m).

| Comparison | Structural |
| --- | --- |
| C# against Rust | 752 unexplained (768 after E), 1,060 explained, 25 identical |
| Rust against Rust | 1,837 identical, 0 unexplained (bytes: the same) |

| Group | Own before (phase 1) | Own after E | Own after | Diffs after |
| --- | ---: | ---: | ---: | ---: |
| N | 48 | 14 | 0 | 0 |

All 14 are gone: `ambient-seeded` (and `-again-cached`, `-recovery`, `-alt-admin-unseeded`)
light the page with the C# recording's colors exactly (the parity cover's palette is also a
Rust test), and the ten `activity-*` requests, `11-signin/in-get-activity` and `in-get-ambient`
match, `activity-rescan-starting` and `-done` included (E's two, now that the Sharing card is
in). Base went from 29 to 27: the genre run's `pending` (`09-admin`), which phase 1 put on N.
E is 2, the proposed `web-url-trimmed`. No group got worse: B 11, C 9, D 110, F 128, G
47, H 201, I 3, J 3, L 45, O 17, P 68, Q 5, R 96; A, K and S 0. N has no proposed intentional
diff.

After `rust/main` `d83eb7c` (H and L) was merged in and the activity view read the Spotify read
and the trickle (`16b07f0`), `parity/run.sh --only '1[128]-*'` (401 requests) had N at 0
again. Its one new J diff, `11-signin/alt-admin-index`, is `aa504ee`'s edit of
`octo/wwwroot/admin/index.html` (the dashboard served differs from the C# image's by one byte),
not N's; `21-static` will show it too.

Then `rust/main` `b4b33cd` (B and D) and `19db112` (G) were merged in, the activity view
read Find songs (D), and the final run was `parity/run.sh --rust-twice` on the whole corpus from
`d839fb7` (corpus `eb2c30598fe12590`, so C# was recorded again; 0h49m on the WSL lane):

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against Rust | 469 unexplained, 1,337 explained, 31 identical | not run |
| Rust against Rust | 1,836 identical, 1 unexplained | 1,836 identical, 1 unexplained |

No activity or ambient request differs. N's 10 are all `21-static` (`static-index`, `-css`,
`-js` and their HEAD and `If-None-Match` forms): the dashboard files `rust/main` changed after
2026.10.05 (`aa504ee`'s spelling and station previews, G's Charts card), covered by G's proposed
`dashboard-files-2026-10-08` (`reviewed` unset, so still counted); with it reviewed N is 0.
The rest is other groups' (E 18, I 23 and part of J are G's two new `GeneratedPlaylists`
settings, `Charts` and `ChartCountry`, in every settings answer). Rust against Rust's one diff
is `19-health/plant-wait` (R), the Navidrome scan timing the earlier records saw.

After `rust/main` `61b1cfb` (F and P) was merged in, every crate file's test count matched
`rust/main`'s apart from N's own additions (`git grep -c`), the gate passed (3,289 tests) and
`parity/run.sh --only '1[128]-*'` (401 requests) had N at 0.

### G4 record: group R (2026-10-08)

Branch `port/g4-r`, on F's `port/g3-f` (`28cabef`), merged with `rust/main` up to `aeb3ed8` (B,
D, F, G, H, L, N and P in). Final run on the WSL lane, `parity/run.sh --no-build --rust-twice`,
of `65c9275`'s build, against the cached C# recording of corpus `eb2c30598fe12590` (1,837
requests). Every crate file's `#[test]` count is at least `rust/main`'s (`git grep -c`), and
the gate passed (3,320 tests; octo-subsonic 120).

| Comparison | Structural | Bytes |
| --- | --- | --- |
| C# against Rust | 106 unexplained, 1,647 explained, 17 intentional, 67 identical | not run |
| Rust against Rust | 1,836 identical, 1 unexplained | 1,836 identical, 1 unexplained |

| Group | Own before (G2 E record) | Own after R | Diffs after R |
| --- | ---: | ---: | ---: |
| R | 96 | 0 | 0 |
| F | 24 (G3 F and P record) | 5 | 5 |
| P | 15 (G3 F and P record) | 6 | 6 |

R is at 0. Rust against Rust's one diff is `19-health/plant-wait`, the Navidrome scan timing
every record since phase 1 has seen: `search3` answered while Navidrome was still giving the
planted albums their covers. A corpus fix (wait for the albums' `coverArt`) is for whoever
owns the corpus. The build run just before (`parity/run.sh --rust-twice`, same commit) had 5
Rust against Rust diffs, all in `19-health`: Navidrome's scans after planting and after the
duplicate fix did not end within the corpus's 60 s while the lane was busy, and the requests
that wait on them followed; the rerun had none of them.

F's 17 requests that waited on R pass: `20-library-edits/plant-copy-seen` captures
`{{song_cafe_copy}}` and `{{copy_dup_key}}` from `/api/admin/health`, and the `*-copy*` chain,
`admin-actions-final`, `tag-edits-journal` and `library-actions-journal` match, and so do the
journals' `admin-actions-off`, `-dryrun` and `-cafe` in the whole run. F's 5 are proposals:
`isrc-clear-takes-the-tag-off` (`tags-copy-undone`, and now `remove-copy-seen`, whose only
difference is Navidrome listing the copy without the code), and the three `extensions-*`
requests, octoTopSongs at versions 1 and 2, which G's reviewed
`charts-octo-top-songs-version-2` does not name in `20-library-edits` (nor D's two in
`13-downloads`). P's 9 knock-on requests (the planted copy left in the library) pass; its 6 are
its 3 proposals, J's 2 and the base binder's `covers-start-bad-json`, as in the F and P record.
`album-without-cafe` and `plant-copy-album`, Rust against Rust in that record, match.

The rest of the 106 is other groups', as the F and P and N records found them: G's `Charts` and
`ChartCountry` in every settings answer (E 18, I 23, part of J and base), the shared dashboard
files on `10-static-cors` (base), the update command (L), the extension lists (D, L), H's 17.

Found and fixed on the way (to match the C#; known-diffs.md rows updated; Rust tests):

- **FLAC padding.** TagLib#'s `Flac.File.Save` keeps the metadata's size on disk while its
  blocks fit (the padding takes up the rest; 4096 bytes when they outgrow it); lofty kept the
  old padding block, so every Library health fix changed a FLAC's size and Navidrome's
  `getSong`/`getAlbum` sizes differed (`19-health/settle1-*`, `settle4-tidewater-bare`,
  `cleanup-wait`). `tag_file::taglib_flac_padding`. B and D's `tagged-file-length` allowlist
  entry now applies to one request instead of two (the FLAC relay matches; the M4A does not).
- **The blank ID3v1 tag.** TagLib# gives every MP3 an ID3v1 tag, blank when it has nothing to
  say. lofty's remove appends a blank one to a file that has none and takes it off one that has
  it, so the first save added 128 bytes and the next took them away (`remove-copy-seen`'s size).
  The tag is now written blank over the one there or after the audio.

`5d3f096` is in pieces: its download log and Find songs routes came with D (`port/g2-bd`), the
trash and Put back routes with F, their three Recently removed tests with R
(`trash_tests.rs`), and its activity card with N. N's `STUB(R)` in `get_activity` is wired: a
`HealthActivity` from the service's run (`From<&HealthRun>`) and its `checking_since()`, with
the C# activity tests over a real run in `library_health_server_tests.rs`.

No settings and no new intentional diffs (the ISRC entry gained a request). No "Fix in port"
line was under R.

## Final parity record (2026-10-08)

Branch `port/integrate`, on `rust/main` `4ff2c86` (every gap group in: A to S but M and T, G
from the charts session), then `harness/shards` (`20a37c0`) and `release/rust` (`f8c07ac`)
merged in. Final run on the WSL lane, `parity/run.sh --rust-twice --bytes`, sharded, of
`e2038cf`'s build, against C# 2026.10.05 recorded again for corpus `55ac89246e2d466e` (1,842
requests); the diff was then taken again on the same recordings with this branch's last
`intentional-diffs.json` (it adds `90-mutations/update-check` to `update-card-pulls-octo` and
drops an entry the new C# recording no longer needs).

| Comparison | Structural | Bytes |
| --- | --- | --- |
| Before: C# against Rust, `4ff2c86`, corpus `eb2c30598fe12590`, unsharded | 106 unexplained | 113 unexplained |
| Before: the same build, sharded, corpus `55ac89246e2d466e` (the shards agent's gate) | 104 unexplained | 111 unexplained |
| After: C# against Rust | 82 unexplained, every one covered by a proposed entry; 0 covered by none | the same |
| After: Rust against Rust | 1,842 identical | 1,842 identical |

The 82 are all proposals waiting for Brandon (`reviewed` unset): with every entry treated as
reviewed, the diff is 0 in both modes. The three reviewed entries explain 39 more.

**The causes, and what became of each:**

| Cause | Requests | Handled |
| --- | ---: | --- |
| G's two settings (`GeneratedPlaylists.Charts`, `ChartCountry`) in every settings, Raw config and settings file answer, and two more keys in config-sources | 55 | Proposed `charts-settings-listed` (46). The 7 config-sources answers carry `config-sources-fixed-mask`'s change too, and one entry has to cover every line of an aspect, so that entry pins the moved keys; `web-url-trimmed` likewise pins the two settings on its 2 answers. |
| `octoTopSongs` version 2 on the extension lists outside `16-lists` (`01-system`, `13-downloads`, `15-radio`, `20-library-edits`) | 12 | Reviewed `charts-octo-top-songs-version-2` names them, with the bytes mode's lines for the same change. |
| The dashboard files read by `10-static-cors` and the second Octo (`11-signin/alt-admin-index`) | 10 | Reviewed `dashboard-files-2026-10-08` names them, with a range's `Content-Range`. |
| `getTopChart`'s added fields in bytes mode | 3 | Reviewed `charts-top-chart-says-which-chart` gains the bytes mode's lines. |
| The update card's hand commands | 3 | First ported as 2026.10.05 wrote them (`7f55c32`); then `release/rust`'s helper version 2 changed them on purpose: proposed `update-card-pulls-octo` (the release agent's), which now names `90-mutations/update-check` too. |
| `covers-start-bad-json`: the shared body binder said `$` and "Expected a value" where System.Text.Json said `$.scope` and "Expected depth to be zero", and put the parameter's entry second | 1 | Fixed for every body endpoint (`e797b0a`): `controllers::admin::json_body` reads a body against each endpoint's C# record, so the path, the words, the position and the order match; tested across endpoints. |
| `12-imports/cap-zip-too-big-unpacked`: the C# answered 500 under the stack's memory (it reads a zip's file whole into one string) | 1 | Gone with the shards agent's 1 GB memory limit for the C# stack: both answer the 400. |
| `album-without-cafe`, `19-health` scan timing, `plant-wait` (the one Rust against Rust diff) | 1 to 5 | Gone with the shards agent's corpus fixes (genres ignored, scan waits, coverArt ignored). |
| `09-admin/lidarr-test-unreachable`: .NET's "Connection refused (stubs:9)" | 1 (new) | Plain error words, an improvement (`8597aa7`): proposed `plain-error-words`. |
| The proposals already on file (J, I, F, P, E, H) | 32 | Waiting for Brandon; unchanged but for their bytes mode's lines. |

**Improvements done** (list above): the shared JSON binder matched to System.Text.Json, plain
error words and no em dashes (`8597aa7`), US spelling in identifiers (`2b132e6`), no `.unwrap()`
outside tests (`c5ebd77`), the wave modules named for what they hold (`a1d46d9`), and 4:2:0
cover JPEGs where ImageSharp wrote them (`4ccc138`; the bytes already differed under
`cover-jpeg-bytes`). Left for the release agent: the cutover items.

**Gate** on `e2038cf`: `cargo test --workspace` 3,333 passed (octo 2,275 and 3, octo-core 769,
octo-media 166, octo-subsonic 120); `#[test]` counts per crate at least `rust/main`'s (octo
2,278 against 2,274, octo-media 166 against 162, octo-core and octo-subsonic equal); clippy
`-D warnings` and fmt clean. One test flaked once under load and passed on three reruns:
`scrobble_tests::a_repeated_finished_play_is_learned_once`. A play sent without a time is
told apart from a repeat by the minute it arrived (`recent_scrobbles`, as the C#), so two
sends that straddle a minute count twice.

**Proposed entries for Brandon's review** (`reviewed` unset; what a person or an app would
notice):

| Entry | Group | Requests | What changes for a person or an app |
| --- | --- | ---: | --- |
| `charts-settings-listed` | G | 46 | The settings page, Raw config and the settings file show the two chart settings (which chart lists to make, and the chart country) like every other setting. |
| `keep-as-playlist-navidrome-takes-it` | H | 16 | "Keep as playlist" on an import works: the playlist appears in Navidrome under the list's name with its songs, instead of an error and an empty playlist named after the time every 30 seconds. |
| `config-sources-fixed-mask` | I | 7 | The configuration sources page shows every saved secret as eight dots, so it no longer gives away how long a password or key is (it also lists the two chart settings). |
| `update-card-pulls-octo` | T | 3 | The update card's hand command pulls Octo with its sidecars instead of building it, and an update helper too old for that is asked to be reinstalled. |
| `recovery-refused-on-library-files` | J | 2 | Signed in with the recovery code, the explicit marking is refused like every other page that touches music files; only a Navidrome admin can run it. |
| `isrc-clear-takes-the-tag-off` | F | 2 | Undoing an ISRC edit takes the code off the file, as the undo already said it did. |
| `web-url-trimmed` | E | 2 | A web address of the removed group was saved without the spaces it was typed with. |
| `lidarr-test-saved-key-stays-home` | I | 1 | Testing Lidarr at a new address asks for the key to be typed again instead of sending the saved key to that address. |
| `plain-error-words` | base | 1 | A failed connection, timeout or error status reads in plain words ("Could not connect to stubs:9: the connection was refused.") instead of .NET's exception text. |
| `cover-resume-counts-once` | P | 1 | A cover run resumed after a stop counts each album and song once ("3 of 3 albums", not "6 of 3"). |
| `cover-scan-leaves-the-trash-out` | P | 1 | The cover scan no longer lists songs in Octo's trash folder as albums of their own. |
| `cover-run-reason-cleared` | P | 0 | A finished cover run no longer keeps its last "barcodes 1 of 1" line by a race (the corpus now ignores that line, so no request uses the entry). |

The reviewed entries this round extended (to other requests with the same change, and to the
bytes mode's lines): `charts-octo-top-songs-version-2`, `charts-top-chart-says-which-chart`,
`dashboard-files-2026-10-08`. Brandon may want to see those additions too.

## Gap groups

Sizes from the migration plan § 2 (C# logic and test lines added, test
methods). "Diffs now" is the latest "own" count (requests tagged with the
group that differ from C# 2026.10.05): the G1 J and I record above (the G2 C, O
and Q record for those three, the G3 H record for H, the G4 record for E and N), proposed
intentional diffs counted as differing. Status: open, in progress, done, or no
Rust change.

| Group | Commits | C# lines | Test methods | Agent-days | Diffs now | Status |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| A Last.fm search cleanup polish | 1 | 40 | 5 | 0.25 | 0 | done |
| B Download matching | 8 | 644 | 46 | 0.75 | 2 | done (G2, `port/g2-bd`) |
| C Tagging polish | 4 | 465 | 24 | 1.0 | 9 | done |
| D Download log, drawer, Find songs picks | 13 | 2,146 | 53 | 1.5 | 12 | done but `696cbff`'s import half (H) (G2, `port/g2-bd`) |
| E (removed) | 8 | 1,084 | 29 | 1.0 | 2 | done (G2, `port/g2-eg`; the 2 are the proposed `web-url-trimmed`) |
| F Library edits, delete, trash, put back | 10 | 1,345 | 35 | 1.25 | 24 | done (G3, `port/g3-fp`); what is left waits for R |
| G Top songs, charts, Popular right now | 4 | 1,472 | 35 | 1.0 | 48 | done on `port/g-charts`, with the charts plan (parity pending) |
| H Spotify import, trickle | 4 | 3,197 | 69 | 2.0 | 16 | done (G3) |
| I Secret masking in the admin API | 3 | 207 | 7 | 0.25 | 3 | done (G1) |
| J Admin sign-in | 6 | 921 | 29 | 1.25 | 6 | done (G1) |
| K Lidarr stale guid, bare address 302 | 2 | 82 | 4 | 0.25 | 0 | done |
| L Multi-source radio | 16 | 2,976 | 127 | 2.0 | 18 | done (G3) |
| M octo-sonic and the shim's `/ytm` | 3 | 22 | 3 | 0 | 0 | no Rust change (own images) |
| N Afterglow, activity view, toasts | 8 | 572 | 18 | 0.5 | 0 | done (G4, `port/g4-n`; R's job waits on R) |
| O Search album ownership | 1 | 808 | 23 | 0.5 | 17 | done |
| P Covers keep-or-replace, explicit marks | 4 | 1,739 | 50 | 1.25 | 15 | done (G3, `port/g3-fp`); what is left is R's knock-on and proposed intentional diffs |
| Q Made for you lists | 3 | 887 | 26 | 0.75 | 5 | done |
| R Library health on the server | 5 | 3,130 | 39 | 2.0 | 0 | done (G4, `port/g4-r`) |
| S Estimated stream length fix | 1 | 153 | 12 | 0.5 | 0 | done |
| T Starter stack, install, CI, images, updater | 15 | 10 | 0 | 1.5 | 0 | release phase |
| U Shared case files, timing tests | 2 | 0 | 0 | 0 | 0 | no Rust change (Rust reads the files) |

Port waves, from the plan: G1 is the settings and env vars for every group,
the 7 new state files with fixtures, then J, I, A, K, S. G2 is B, C, D, E, G,
O, Q. G3 is F, H, L, P. G4 is R and N.

Things the gap brings besides code: 7 new state files
(`explicit-backfill.json`, `imports.json`, `imports-trickle.json`,
`radio-outcomes.json`, `sonic-features.json`, `spotify-accounts.json`,
`tag-edits.json`) plus `admin-recovery-code`; new fields in
`downloads-history.json`, the trash notes and `upgrades.json`; 34 new env
vars; the `zip` crate for imports.

**Done in wave G1 (branch `port/g1-core`):** every 2026.10.05 setting and
env var (config.md §3.6, §3.12, §3.15 to §3.18), bound and clamped, and
listed by GET settings, raw-config and config-sources. The 7 new state
files and `admin-recovery-code`, read and written as the C# stores did, each
with a regenerated fixture and a round trip (state-files.md §4.26 on). The
new fields in the files Octo already kept, including three the list above
missed: `external-ids.json` (`ExplicitContent`, `Version`, and a clean
edit's own id), `generated-playlists.json` (`ForYou`, `ForYouCovers`) and
`library-actions.json` (`Copy`, `DownloadAllowed`, `BlocksDownloads`, state
`Restored`); `downloads-history.json` is now written without nulls, as the C#
writer does. What the later groups find waiting for them:

| Group | Already there |
| --- | --- |
| B, D | `DownloadHistoryEntry.key` and `log` (`octo_core::models::download::{AcquisitionEvent, AcquisitionCandidate}`, with `Quality`); `PickedCopy` (`services::common::download_picks`) and `UpgradeJob.pick`. `base_download_service` records `key: None` (STUB(D); G2 records the run's key). `GET /api/admin/downloads` already drops `log` and says `hasLog`. |
| E | (removed: the download client's sharing settings and the save's checks for them) |
| F | `services::library::library_tag_edits` (`TagEditEntry`, the file); `LibraryActionEntry.copy`, `download_allowed`, `blocks_downloads()` (the journal's never-requested check uses it); `LibraryActionState::Restored`; `QuarantineManifest.sidecars` and `mappings` (written null, STUB(F)). |
| G, Q | `GeneratedPlaylists.PopularNow`, the Made for you switches and clamps (`any_for_you`); `UserMixes.for_you` and `for_you_covers` (filled by Q in G2). |
| H | `Imports` settings; `services::imports` (`ImportList`, `ImportTrack`, `TrickleJob`, the three files' load and save). |
| L | `RadioSources` settings (`effective_weight`); `services::radio::radio_outcome_store` and `services::sonic::sonic_store`; `LastFmRadioBan`; the status page's octo-sonic probe. |
| P | `ExternalRouting.explicit_content` and `version`, the clean id rule and the explicit mark kept across registrations; `services::library::explicit_backfill_store`. |
| H, N, R, D | Their controllers call `BrowseSessionStore::navidrome_user_of` (never the recovery code) and read who called from the gate's `AdminCaller` extension (`middleware::admin_sign_in_gate::{Caller, caller_of}`), as `LibraryHealthController` and `AdminDownloadsController` did. |
| Every group with a new route that reads or changes library files | Add it to `middleware::admin_sign_in_gate::LIBRARY_FILE_ROUTES` (with the words the C# endpoint answered), so the gate refuses the recovery code there as on the rest; the walk test lists the routes. |

## Commits by group

### A. Last.fm search cleanup polish

- [x] `6f90ce1` fix(lastfm): the rest of the mislabelled search rows, and a gap inside "Connected as"

### B. Download matching (versions, mash-ups, wider star search, format rename, relay file name)

- [x] `15087f6` fix(downloads): a star searches as wide as Better quality, and wider again when the limit fills
- [x] `ff46dde` fix(downloads): a song's version tag is not words its file must carry, and the song itself never takes an edit
- [x] `39770ed` fix(downloads): a guest credit inside a version tag, "(Radio Edit - feat. X)", no longer makes the version words a file must carry
- [x] `066ea88` fix(downloads): a drumless, piano, 8D or other performance edition is a version, while deluxe and anniversary editions stay the same recording
- [x] `53731d2` fix(downloads): a mash-up, medley or megamix never passes as the song, and only a file named as just the song is taken for its radio edit
- [x] `85bba0f` feat(downloads): a file named for another format is renamed to the one it holds before tagging
- [x] `0a700c0` fix(relay): a download keeps the file name Navidrome gave it
  (ported with S: `StreamLengthTests` checks it)
- [x] `a38487d` docs(readme): a download named for the wrong format is renamed before tagging
  (README only; the Rust build serves the same README)

### C. Tagging polish (one album's facts, explicit advisory, Try it)

- [x] `2463c23` feat(tagging): a download keeps only its own album's facts, one value per field, and says whether it is explicit
- [x] `67f56fa` docs(readme): what a download loses from a peer's tags, DISCTOTAL and the explicit advisory
- [x] `099fdd3` feat(tagging): Try it on a song shows whether the song is explicit and what a download would take off the file
- [x] `a60f06c` docs(readme): Try it on a song shows the advisory and what a download would take off
  (the `LogStep` lines `02cc06b` added to these paths are D's; the cover chain's `CoversOnHand` is
  P's, and C passes no embedded art for a song filed elsewhere, as `2463c23` did)

### D. Download log, drawer, Find songs picks

- [x] `9ac4e5a` feat(downloads): every download keeps a log, and Find songs picks the copy to fetch
  (its Lidarr half is on `rust/main` with K: `LidarrRelease`, `LidarrReleasePick`,
  `list_releases`, `grab_release`, the grab in `start_album_search`, the request's `release`,
  and its two `LidarrClientTests`; G2 wired `SongFinder`, `DownloadPicks` and the
  download log's `step` to them)
- [x] `6113fec` fix(downloads): a picked copy waiting in the upgrade queue is saved without its derived fields
- [x] `1469b8c` docs(readme): the downloads drawer, Find songs and their endpoints
  (README only)
- [x] `02cc06b` feat(downloads): a download's log says what came off the peer's tags and whether the song is explicit
  (ported once C's `2463c23` was on `rust/main`)
- [x] `11900f1` fix(downloads): Find songs picks a copy by its own id, and only once the search has ended
- [x] `c3e80bb` fix(downloads): a picked copy meets the deny list, and in place of a library file the search's own checks
- [x] `25b1d67` fix(downloads): a second pick before the upgrade starts replaces the first, and otherwise says another copy is on its way
- [x] `e24eca1` fix(downloads): a pick is let go when its download ends, so a later download never takes it
- [x] `45f4463` fix(downloads): Find songs runs two looks a person at once, never drops a running one, and a library song's look is for the allowed list
- [x] `57ab90d` fix(downloads): Find songs keeps a few passed-over files of each reason on a popular song's list, and its fit count matches the rows
- [x] `13ec9f9` fix(downloads): Find songs says how many fitting copies its list shows when it has no room for all of them
- [ ] `696cbff` fix(downloads): a shared download's log names nobody, and import errors show no server paths
  (its download log half is in, with D: a shared download's row names nobody on its queued
  line; the import errors without server paths are group H's `ImportService` and trickle worker)
- [x] `7602787` feat(downloads): keep each finished download's log with its fetched-songs entry

### E. (removed)

The download client's sharing group (its card, switch, port test, web address and password
rules, and its nine commits). Done in G2 (`port/g2-eg`); the code left with the client, and
its proposed intentional diff `web-url-trimmed` with it.

### F. Library edits from the apps, delete, trash, put back

- **Fix in port:** clearing an ISRC does nothing on disk
  (`TagWriterExtras.cs:98-101` returns on an empty value), so undoing an ISRC
  retag answers `isrc: null` and leaves the tag in the file; remove the tag
  (group C's tag writing is the same code), with a Rust test and an
  intentional diff. Done in G2 with C: the shared tag writer clears it
  (`tag_writer_extras::set_or_clear_text`, test
  `clearing_an_isrc_takes_it_off_the_file`), and the proposed intentional diff
  is `isrc-clear-takes-the-tag-off`; F's tag edits only need to call it.

- [x] `374dac8` feat(library): Library health fixes from the apps: tags written in place, albums joined, covers added, and a removed song put back
  (G2: `LibraryTagEdits.Set` must write through `tag_writer_extras::set_or_clear_text`, so a
  cleared field, an ISRC above all, leaves the file; the port-only fix and its intentional diff
  `isrc-clear-takes-the-tag-off` are in place)
- [x] `8606390` test(library): joining a split album takes away a peer's stray album id, release date and version
- [x] `15e3bb5` fix(library): removing a second copy leaves the song wanted, so the kept copy can still be upgraded
- [x] `433acff` fix(library): Delete from disk takes the song's own lyrics files to the trash with it, and Put back returns them
- [x] `a4238fa` fix(library): Put back links the outside song to its file again, as it was before the song was removed
- [x] `3204161` feat(admin): Allow downloading again on the Library actions history, and a removed song's log says how to get it back
- [x] `6b6b61d` fix(library): a removed song's outside link is found by its whole path, and the trash note's link is tested
- [x] `f2c6a65` fix(library): getLibraryActions offers remove only to a Navidrome admin
- [x] `a0fcecb` test(library): joining a song onto an album filed under another album artist takes that album artist, keeps its own artist, and undoes
- [x] `ed0bd7c` fix(library): a library song's length is read from Navidrome's fractional seconds instead of always missing
  (ported with D in G2: Find songs reads a library song's length through it)

### G. Top songs, charts, Popular right now

Ported on `port/g-charts` together with the charts plan (`~/.claude/plans/octo-charts/PLAN.md`):
Popular right now is one of several chart lists (Best New Songs, Trending Songs, the genre charts)
read from Apple Music's public pages, with Deezer and Last.fm behind Popular right now.

- [x] `3e35798` feat(search): an artist's top songs and the chart of the moment, ranked and marked in the library
  (`TopSongsService` registers its songs through `services::library::for_you_lists::catalog_song`,
  which New Releases shares; the STUB(G) mark is gone)
- [x] `3d5bea4` feat(top-songs): every app's getTopSongs ranked by Octo, the caller's songs as their own
- [x] `43d8325` feat(lists): Popular right now, the chart as a playlist in every app
- [x] `2397369` docs(lists): US spelling in a Popular right now comment

### H. Spotify import and the trickle

Done in G3 (`services::imports`, `controllers::admin::imports`, `controllers::subsonic::imports`).

- **Fixed in port:** "Keep as playlist" answered 500 against Navidrome 0.64.2. Navidrome
  creates a playlist from JSON only when the content type is exactly `application/json`; the
  C# sent `application/json; charset=utf-8`, so Navidrome read the body as an M3U file, made an
  empty playlist named after the time and answered in M3U, and every 30 s pass made another.
  The port sends `application/json`, deletes a playlist it made and could not fill, and tries a
  playlist Navidrome would not take again after 1, 2, 4 minutes and on, at most an hour; Rust
  tests and the proposed intentional diff `keep-as-playlist-navidrome-takes-it`.

- [x] `4008358` feat(imports): Spotify lists matched against the library, kept as playlists, and a trickle that fetches the rest
- [x] `ef24d3d` feat(admin): a Spotify import page for connecting, lists, playlists and the trickle
  (no Rust change: the dashboard is the shared `octo/wwwroot`)
- [x] `1e8cf04` fix(install): a reinstall keeps the Spotify import settings it never asks about
  (no Rust change: the shared `install.sh`)
- [x] `45f730c` fix(imports): a file holds at most 50,000 songs, a zip 200 files and 256 MB unpacked, and a person 500 lists

### I. Secret masking in the admin API

Done in G1 (`controllers::admin::secrets`). `d7e8482` is the README.

- [x] `031e563` fix(admin): every key, token and webhook address in the settings reads as the placeholder, and saving it back keeps the stored one
- [x] `9210c63` fix(admin): a saved password, key or token never follows a server address it was not typed again for
- [x] `d7e8482` docs(readme): saved credentials never come back out of the admin API

### J. Admin sign-in (gate, recovery code, role check, ADMIN_PORT)

Done in G1. `3b82267` is the dashboard (`octo/wwwroot`, served as is by both
builds) and `7a9f753` the README, compose, `.env.example` and installer: no
Rust change beyond the settings they name.

- [x] `a4333f0` feat(admin): the admin API needs a sign-in with a Navidrome admin account
- [x] `3b82267` feat(admin): a sign-in screen in front of the whole dashboard
- [x] `7a9f753` docs: signing in to the dashboard, the recovery code, and scripts with a cookie jar
- [x] `2eb2299` fix(admin): the hourly role check reads Navidrome's user list, since getUser only answers for yourself
- [x] `06e9a60` fix(admin): a demoted admin is signed out even when Octo's own Navidrome token came from them
- [x] `f591793` fix(admin): Navidrome refuses its user list to a non-admin with 403, and that now means Octo's token is stale

### K. Lidarr stale guid, bare address 302 (#80)

- [x] `e3aed24` fix(lidarr): a picked release Lidarr forgot is found again by its title, and a pick an album search already covers says it was not grabbed
  (the Lidarr client and track fetcher; `PickedCopy.Size` and `SongFinder`'s size belong to
  D's picks and come with them)
- [x] `4a329aa` fix(proxy): the bare server address sends to the web app, as Navidrome does (#80)

### L. Multi-source radio (#78, sources, blend, learning, bans, Sounds alike client)

- **Fix in port:** an outside song's registry id ignores its album
  (`ExternalIdRegistry.cs`, `MakeShortId`): radio registers songs from
  Last.fm without an album and replaces the search's entry, so later answers
  lose the album; keep the album a registration already had (as the length,
  ISRC and explicit mark are kept), with a Rust test and an intentional diff.
  Done in G3 (`external_id_registry::register`, test
  `a_registration_without_an_album_keeps_the_album_already_known`, known-diffs
  "An outside song's album in the registry"; no corpus request reaches it, so it has no
  intentional-diffs entry, see the G3 record).

Done in G3 on `port/g3-l`. The pure parts are `octo_core::radio` (`RadioProvider`,
`RadioTypes`, `RadioBlend`) and `octo_core::last_fm::radio_filler`; the sources, the set,
song radio and the outcome store are `octo::services::radio`; octo-sonic's client, store and
analysis worker `octo::services::sonic`; `YouTubeMusicClient` and `ListenBrainzRadioClient`
beside their services. `SongRadioService` is shared, with each request's resolver handed to
`build`. `TopSongsService.LibraryQuery` (7a8cc5f's library search) lives in
`octo_core::charts::top_songs::library_query` (group G), which
`last_fm_radio_track_resolver::library_query` re-exports (the STUB(G) mark is gone). `6a119db`'s settings views and the octo-sonic status card came with G1. The station
previews' em dash Brandon saw is in the dashboard (`octo/wwwroot/admin/admin.js`, the Radio
page's station grid joins artist and title with one), which both builds serve as is; no Rust
radio code builds a string with one.

- **Radio placement (port/radio-placement):** the ported song radio keeps `last_fm_radio_placement::place` as its last ordering step for callers without `octoMix=client`, at the radio's own mix (never the Discovery balance), and skips it when library-led; `octoMix=client` callers may ask for up to 100 songs. Your Mix and Starter Radio keep the placement at the end of `shape`.

- [x] `446d393` feat(radio): a song Last.fm cannot place is a radio of its own album and genre (#78)
- [x] `02218c1` feat(radio): one star keeps a song off radio, filler left out, a cached answer never short-changes a bigger ask
- [x] `f621926` fix(radio): British-spelled bookkeeping tags are not station genres again
- [x] `8f5142d` refactor(radio): song radio asks a set of sources and blends them; Last.fm is the first
- [x] `c1d701d` feat(radio): every radio song says which source suggested it (octoSuggestedBy, octoRadioSources)
- [x] `8346a30` feat(radio): YouTube Music's radio is a source, and a song it suggests plays the video it named
- [x] `7b0e226` feat(radio): ListenBrainz's similar recordings are a source, and LB Radio fills artist and genre stations when Octo has a token
- [x] `6a119db` feat(admin): radio sources and their weights on the Radio page
- [x] `2d04f3f` feat(radio): Sounds alike, library songs that sound like the seed, from Octo's own analysis
- [x] `7ba1123` feat(radio): stations draw on every radio source and remember which one suggested each song
- [x] `ece2795` feat(radio): radio learns which sources each listener keeps, and weighs them by it
- [x] `7a8cc5f` fix(radio): song radio asks fairly and never keeps a failure as an answer
- [x] `b446983` fix(radio): stations share per seed and learning counts each serving once
- [x] `3de8dde` fix(sonic): the analysis says it is about to start, not that it is off, until its first look
- [x] `e4b0327` fix(sonic): Sounds alike never forgets songs on a bad listing, and octo-sonic runs locked down
- [x] `7af692c` fix(sonic): the dashboard and comments spell analyzed the US way

### M. octo-sonic (Rust) and the shim's /ytm (Python)

No Rust server change: both are their own images.

- [ ] `0488476` feat(sonic): octo-sonic reads a song and says how it sounds (bliss-rs)
- [ ] `b900557` feat(shim): YouTube Music search and radio, and every shim test runs in CI
- [ ] `9ea267a` feat(sonic): octo-sonic starts with the stack, and RADIO_SOUNDS_ALIKE=false turns it off

### N. Afterglow dashboard, activity view, toasts

- [x] `77701bc` feat(admin): the dashboard's light comes from the fetched songs' covers, read with the station covers' colour rules
- [x] `31e3715` feat(admin): Afterglow, the dashboard lit by a fetched song's cover
- [x] `32d05af` fix(admin): Recently added offers one song per cover, so an album fetched whole is one choice of light
- [x] `5a00fb1` fix(admin): choosing the newest fetched song lights the page with it, and a faintly tinted cover lights it quietly
- [x] `2e4fdcf` fix(admin): Octo's own service card carries the octopus
- [x] `d52ae54` feat(admin): Fetched songs in the Afterglow look
- [x] `ca47988` feat(admin): one activity view of everything Octo is working on
- [x] `af48eb1` feat(admin): live progress toasts for Octo's background work

Done in G4 (`port/g4-n`): `31e3715`, `2e4fdcf`, `d52ae54` and the dashboard half of
`5a00fb1`, `ca47988` and `af48eb1` are `octo/wwwroot`, which the Rust build serves as is. The
server half is `GET /api/admin/ambient` (`controllers::admin::ambient`, over
`services::admin::{cover_palette, ambient_palette_service}` and `cover_colors::from_lch`) and
`GET /api/admin/activity` (`controllers::admin::activity`), with `pending` on the genre run,
the duplicate scan's `is_scanning`/`is_requested`, and `getScanStatus` signed in. Neither route
reads library files, so neither is in `LIBRARY_FILE_ROUTES` (the C# answered the recovery code
on both). The activity view reads Library health (group R, not on `rust/main` yet) through a
view of its own, `HealthActivity` (from `HealthRun`), with a `STUB` where it is read: a
conversion and one call in `get_activity`; the word mapping and its tests are in already. The
rescan (E, since removed), the Spotify read and trickle (H, `ImportsActivity` from `ImportOverview`) and
Find songs (D, `FindLook` from `FindSnapshot`) are read.

### O. Search album ownership

- [x] `f49fb80` fix(search): an outside album the library holds is listed as the library's, and one held in part says how much

### P. Covers keep-or-replace, explicit marks and backfill

- **Fix in port:** resuming a cancelled cover run counts `kept` and
  `albumsDone` again ("6 of 3 albums", `CoverUpgrade.cs:696,744`); count each
  album once, with a Rust test and an intentional diff.
- **Fix in port:** the cover scan walks into `/music/.octo-trash` (and the
  album id of a trashed song depends on the dated folder); leave the trash
  folder out of the scan (group F owns the trash), with a Rust test and an
  intentional diff.
- The recovery-code explicit marks are already refused by the gate (G1).

- [x] `5cea252` fix(covers): a download keeps the cover it came with when it is the same art and big enough
- [x] `86ad6b8` fix(covers): every strict source is asked, and only a copy of the release's own art can win
- [x] `4f2aeb6` feat(explicit): outside songs and albums say explicit or clean, and a clean copy has an id of its own
- [x] `d80141f` feat(explicit): mark the songs already in the library explicit or clean

### Q. Made for you lists

- [x] `f54467a` feat(lists): Made for you, three lists per listener from their own plays
- [x] `1f940f2` fix(lists): only artists a listener played or hearted are theirs, and a shared song filed under their artist is their copy
- [x] `452f47e` fix(lists): Check Mode on Made for you
  (the Made for you settings came from g1-core's `generated_playlist.rs` as it wrote them, with its
  admin settings and raw-config rows, so the two merge cleanly)
- [x] Lists off in the dashboard (2026-10-08, `fix/q-lists-off`): the parity run's
  `16-lists/listener-playlists-lists-off-json` kept New Releases. The getPlaylists right after
  the save came in before the file watcher read settings.json again (250 ms later), so it
  answered from the settings before the save. With Popular right now's chart calls that one
  request took 15 s, the whole wait, so no second ask came. A settings save and a Raw config
  save now read the file again before they answer, and the next request sees the save.
  Rediscover, Deep Cuts and the list settings (weeks, artists, months) had the same lag and the
  same fix. C# still waits for its watcher; a request inside those 250 ms is the only place the
  two can answer differently.

### R. Library health on the server, dashboard parity (logs, Find songs, Recently removed)

Done in G4 (`port/g4-r`): `services::health` (`library_health`, `health_words`, `health_fixes`,
`server_song_fields`, `library_health_service`) and `controllers::admin::health`
(`/api/admin/health` and its `run`, `preview`, `apply`, `lookup`, `stop`, `undo`), already in
`LIBRARY_FILE_ROUTES` since G1. The shared cases (`docs/library-health-cases.json`) run as Rust
tests.

- [x] `69f577f` feat(health): Library health findings and fixes on the server
  (the song row's health fields, `LibrarySnapshot::read_at_utc` and `refresh`,
  `LibraryAlbumIndex::rows`; the duplicate scan groups by Library health's rule;
  `LibraryRescan::hold` came with F; `ActivityController.Health` is N's)
- [x] `e36d823` fix(health): check songs in the order the apps read them
- [x] `c156710` feat(admin): Library health page on the dashboard (the shared dashboard files; the
  controller's `?fresh=1` and slimmer song view)
- [x] `5d3f096` feat(admin): download logs, Find songs and Recently removed for the dashboard
  (download log and Find songs routes with D: `controllers/admin/downloads.rs`; trash and Put
  back with F: `controllers/admin/trash.rs`; the Recently removed tests with R; the activity
  card with N, wired to R's run in G4)
- [x] `eef5fbd` feat(admin): download logs, Find songs and Recently removed on the dashboard (no
  Rust change: the shared dashboard files)

### S. Estimated stream length fix

- [x] `67f3c93` fix(stream): a transcode's estimated length is never promised to the client

### T. Starter stack, install.sh, README, CI, sidecar images, updater

Done in the release phase (plan § 9) on `release/rust`: the compose file, install.sh and the update
helper move to the Rust image there. These 2026.10.05 commits were already in the tree (`rust/main`
holds 2026.10.05's compose, install.sh, README and workflows); what changed with them for Rust is the
release branch's: the Rust server is the root `Dockerfile` and `ghcr.io/winters27/octo`, `docker.yml`
builds it after `ci.yml` (Rust checks) passes, `VERSION` is the release number, and the C# bridge
branch's packaging came across (Octo pulled with `octo-source`, helper version 2 and the dashboard's
warning for an older helper, log rotation, quiet request lines, install.sh executable and waiting).
The update card's hand command and `reinstallCommand` (from `610dba2`, `b2e5d9c` and the bridge) are
ported with their tests (intentional diff `update-card-pulls-octo`).

- [x] `73b83c4` feat: a starter stack with Navidrome, and a README that leads with the music service
- [x] `6a2a477` docs(readme): say the best copy Octo can find, not lossless, since a kept song can be FLAC, MP3 or Opus
- [x] `90df881` fix(install): a terminal that cannot clear the screen no longer ends the installer before it prints a word
- [x] `d1a7e67` fix(install): defaults apply on a fresh install, so pressing Enter no longer makes the Octo folder the music folder
- [x] `aba1743` fix(compose): the starter Navidrome is navidrome-starter, so a Navidrome of your own named navidrome in an override no longer merges with it and leaves Octo's dependency undefined
- [x] `76684ab` fix(install): a re-run keeps everything in .env, and a first install gets the defaults it shows
- [x] `b2e5d9c` ci(images): publish the yt-dlp shim beside Octo, and pull it on image installs
- [x] `610dba2` feat(images): updates pull the yt-dlp shim and octo-sonic instead of compiling them
- [x] `b2298f5` ci(images): native arm64 builds, latest on releases, and the release waits for its images
- [x] `94e30d1` ci(images): the release waits until every image can be pulled without signing in
- [x] `0d81d0b` ci(docker): octo's digests are collected apart from octo-sonic's
- [x] `c038b87` ci(docker): the Sounds alike image is ghcr.io/winters27/octo-sonic
- [x] `25ad530` docs(readme): a 20-second demo at the top, searching, playing and keeping a song
- [x] `0adc6b3` release: 2026.10.05
- [x] `ce63143` docs(readme): the apps' links point at 1.5.0 [skip ci]

### U. Shared case files, timing tests

The Rust tests read the shared case files as they are.

- [ ] `aae574a` test(identity): the shared song cases gain drumless and other editions, karaoke in the style of, and mash-ups, as the apps have them
- [ ] `97855f1` test: two timing tests wait for what they check instead of sleeping

## Group FAM: Family (wave G5)

Octo Family: accounts for the people an owner shares their music with, each with their
own library, devices and abilities. Built in the Rust server only; branch
`port/g5-family`, local only. With `Family:Enabled=false` (the default) nothing
observable changes.

- [x] G5-0 Start: `argon2 0.6`, `chacha20poly1305 0.11`, `ipnet 2.12`; this section
- [x] G5-1 Pure model (`octo-core/src/family/`): abilities, policy, request, device (with the trusted-proxy client address), codes, endpoint classes and `docs/family/opensubsonic-endpoints.txt`
- [x] G5-2 Settings: the `Family` section, appsettings.json, the dashboard document, compose `/members`, install.sh
- [x] G5-3 Vault, passwords and stores: `family.key` (AEAD half and web JWT half), `family.json`, requests, saved songs and albums, family playlists, the family pool
- [x] G5-4 Navidrome admin client, targeted scans, `LibraryIndex` and `LibraryScope`. The shared scope is wired into Library health, the weekly quality upgrade, explicit marks and the duplicate scan; album ownership, heart ownership, outside dedupe, the review sweep and the sync catalog take each person's scope
- [x] G5-5 Member bridge, family gate (every sign-in form, X-Forwarded-For only for the client address), web session (Octo JWT swapped both ways), device registry with a new-device event, fresh member sign-ins for held work, repair on refusal, rule hooks, log masks; freed preset defaults, personal audio quality and device quality mode in the model and store
- [x] G5-6 Request and answer rules, audio gate, library write gate, player cap sync, bypass suite, stream tracker: unknown endpoints refused for kids and members with a limit, native account, player and share writes refused, the audio gate (quality for every caller from the client, the account and the family limit; away, clean-only, downloads, shares and offline copies; no transcode decisions under a ceiling; outside files relayed as the library copy and never from another member's library), the stream tracker (devices at once, newest device wins with a three minute hold, fast-pull pacing, live streams), player cap sync, the `getUser`, `getNowPlaying` and `getStarred`/`getStarred2` answers, and the bypass suite. The library write gate: hearts on outside songs and albums, plays that keep a copy (download on play, Lidarr album on play, the lossless wait), imports Get missing and chosen songs, Find songs picks and `libraryAction`. Direct members add into the family pool, Request members ask (one request per album, one per import list), SaveOnly members save or are told no; a full library turns a Direct add into a request; plays keep no copy for anyone who must ask
- [x] G5-7 Heart, save, family pool, personal library, requests, Request a copy playlists, Remove from my library. The family pool: one file per member download under `<members>/.pool`, relative links in each member's folder, refs in `family-pool.json`, gc to the trash, a better copy repointing every link, a song the shared library wants moving there, stars and family playlist entries following a song whose id changed, copy mode where links do not work (probed, or switched to when Navidrome never shows a linked song), dangling pool files swept. Downloads for members land in the pool (in-flight key `provider:id`, a member who joins gets a link); a song already on disk is linked, never fetched again. Requests: already shared, added from the family, joined in flight, or pending and approved by policy; done only once the index finds the song in the member's library (forced targeted scans, up to 10 minutes); FLAC means FLAC files only and no FLAC made from lossy audio, else "No real FLAC copy found" and try again as MP3 (an MP3 asked for after a failed FLAC request is that request tried again); a better copy of a shared song through the upgrade queue as the owner; import lists to the trickle once approved; request events for pushes. Storage used per member and Remove from my library (own songs only, shared songs refused). Request a copy, Request a copy in FLAC and Remove from my library playlists
- [x] G5-8 Family playlists: `of` ids while Family is on, listed for the owner and every member, each entry served per viewer (the library song when shared or the viewer's own, else its outside version), additions held for approval as playlist addition requests (approving adds the song, declining or canceling takes it out, no weekly slot), changes by ability through one set of rules for `/rest`, the apps and `/family/` (the approval switch needs Manage family); rows carry their real length
- [x] G5-9 Kid profiles: explicit songs left out of their answers by the index's mark (any client, DSub too), outside songs by the catalog's mark, getSong answers error 70, counts kept (a family playlist row's count and length leave out explicit outside songs too); clean stations, mixes, Made for you, charts and song radio; `unrated_count` for "Mark them now"
- [x] G5-10 Member sign-in on `/family/`, pairing, invites: Family page sessions (cookie `octo_family`, never valid on `/api/admin`), every `/api/family` route with the cookie or a Subsonic sign-in, member lifecycle (add, adopt, edit, pause, remove with the typed name and a files choice: keep aside under `<members>/.kept/<user>`, move into the shared library, or trash), a new sign-in link, repair, pairing by username and code with a cap of 30 wrong codes in 10 minutes while a code is live, no CORS on `/api/family`
- [x] G5-11 `octoFamily` extension for the apps: every call, listed only while Family is on, JSON and XML; `addFamilyDevice` takes `username` for managers; `octoFamilyPair` by GET or form post
- [x] G5-12 Notifications and the ListenBrainz fallback rule: every request change (made from the Family page, an app, a heart, an action playlist, an import, a Find songs pick or a playlist addition) is told once, from the request events: a new request and a failed one to the owner's channels (switches `NotifyFamilyRequests` and `NotifyFamilyDevices`, on by default), approved, declined, added and failed to the member's own ntfy topic without the owner's token; new devices to the owner. A managed member uses their own ListenBrainz token or none, never the default one
- [x] G5-13 Dashboards (`/family/` page, admin entry): the Family page and the join page (QR drawn locally, Android intent link, Open in Octo, Get Octo, other apps' steps), the dashboard's Family entry shown only when status reports `family.supported`; the link probe's result (links or copies, with the fix) on both; Try MP3 on a failed FLAC request; checked at 375 px and desktop, light and dark
- [x] G5-14 Owner privacy: members' own songs, albums, artists and libraries left out of the owner's answers, members' playlists out of getPlaylists, and members' songs and albums out of the owner's getGenres counts, from the index (which keeps each song's genres and length); members' own songs marked `octoPersonal`; setLyricsChoice for the Member preset and up; a member's own albums stand in for their outside versions in their own searches, artist pages and outside album pages
- [x] G5-15 Scale (shim threads and single flight, radio cap, fair star queue, load)
  - [x] Shim threads from `SHIM_THREADS` (128), a single-flight test, active streams and running yt-dlp in the shim's health, and a radio stream limit that grows with the family; the load view's live streams, upload rate and download queue per member come from the stream tracker and the acquisition tracker
  - [x] Fair star queue: stars are handed out one requester at a time, one person's own stars keep their order, plays stay first in first out and after stars
  - [x] The load view's outside-song helper slots (active, limit, yt-dlp running): Octo reads them from the shim's health through the shim client at its configured address, with a 2 second timeout, and reuses one answer for 5 seconds. When the shim does not answer, `shim` is null and the Family page says "The YouTube helper did not answer" instead of showing zeros
  - [x] Live streams of outside songs show their title and artist: when such a stream starts, Octo looks the song up (what it holds in memory first, then the metadata service) on its own task, so the stream never waits, and the load view and now playing show the name once it is found
- [x] Integration: every family hook runs on the service that does the work (requests, storage and the member's own library, a removed member's files, folders and the link probe, live streams, the unmarked count, family playlists). End to end with `cargo run` against a scripted Navidrome and the parity stubs (2026-10-09, every check passed): Family turned on, a listener added, joined and paired, an away stream at Standard asking Navidrome for Opus at 160 kbps, a heart saved, a FLAC request approved, downloaded into the pool and linked, Done with one push, a second member added from the family library with no new download, a removal that leaves the other member's copy, and a signed-out device refused with error 40
- [ ] G5-16 Apps (`C:\OctoPlayer`, `feat/family`)
- [ ] G5-17 Parity, live checks, docs

## Release packaging record (`release/rust`, 2026-10-08)

Local only: nothing pushed, tagged or published; no workflow run. VERSION says 2026.10.08 as a
placeholder for the real date.

- **Gate:** `cargo fmt --check`, clippy `-D warnings` and `cargo test --workspace`: 3,325 passed,
  0 failed (3,320 on `rust/main` plus the update card's and the helper's new tests).
  `test-updater.sh` 66/66, `test-install.sh` all passed, shellcheck 0.11.0 clean, actionlint
  1.7.12 clean apart from docker.yml's `concurrency.queue`.
- **Image:** the root Dockerfile built cold in WSL (16 cores) in 297 s (cargo-chef cook 62 s,
  the Octo crates 204 s); a rebuild with only `OCTO_VERSION` changed 215 s. 1.0 GB on disk,
  302 MB compressed (C# 2026.10.05: 1.12 GB, 340 MB). The cargo-chef and bookworm-slim bases
  publish linux/arm64 too; the arm64 build itself was not run (CI's native runners do it).
- **Upgrade, WSL Docker** (own folder `/root/rr-test`, project names `rr<x>`, a `registry:2` on
  127.0.0.1:5000 standing in for ghcr.io: each test tag is the real commit with
  `ghcr.io/winters27/` changed to `localhost:5000/winters27/` in docker-compose.yml only; the
  2026.10.05 install's own C# build is the published `ghcr.io/winters27/octo:2026.10.05` tagged
  with the name compose gives the build). A 2026.10.05 clone with the starter Navidrome, the
  10-song parity library, sign-in required and 2026.10.05's own helper (version 1, systemd):
  Update now (a request file, as the dashboard writes it) to 2026.10.08 was done in 29 s
  (`docker compose build octo` built nothing, `up -d` pulled the Rust image, the helper
  reinstalled itself as version 2, `mode=git`). The dashboard cookie made on C# stayed signed in,
  `settings.json`, `browse-sessions.json` and `admin-recovery-code` were untouched, the dashboard
  files and Subsonic (ping, getArtists, getAlbumList2, search3, stream, getCoverArt) answered as
  on C#. Version 2 then moved it to 2026.10.09 in 30 s (pulls only).
- **Rollback:** `git checkout --detach 2026.10.05 && docker compose up -d` brought C# back in 5 s
  on the same config folder: still signed in, settings the same, no state file warning in its log.
  Update now from there to 2026.10.09 again: done in 29 s. A copied-compose rollback
  (`OCTO_IMAGE_TAG=2026.10.05` in `.env`, `pull` + `up -d`) ran C# 2026.10.05 in 10 s.
- **Fresh install:** a clone at 2026.10.08 and `./install.sh` with Enter for every answer
  (starter Navidrome): pulled all three images, built nothing, everything up, helper version 2.
  octo-sonic reads "not reachable" while the music folder is empty, as on 2026.10.05 (by design).
- **Memory and start** (WSL, busy with other lanes, load 5 to 12; each Octo `--cpus 2 --memory
  768m`, starter Navidrome, 10 songs, 12,000 requests at 16 threads, two runs in both orders):
  C# 2026.10.05 idle 265 to 281 MiB, peak 367 to 441, 29 to 31 CPU-s, 1,863 log bytes per
  request; Rust idle 67 MiB at 10 s and 90 to 94 at 2 min, peak 92 to 95, 6 CPU-s, 24 log bytes
  per request (all of it the status probe's warning about the test's absent download client; with
  it up, 600 requests logged nothing). Process start to "Now listening on": C# 1.74 s median, Rust
  0.89 s. Per path (2,000 requests each): Rust ahead on ping, getArtists, search3, status and
  admin.js, level on getAlbumList2 (both wait on Navidrome). Against the plan's targets (LXC 131,
  idle at most 60, peak at most 80, start at most 0.4 s) this box reads high for both builds
  (C# idles 265 to 281 here against 213 there), so the targets are judged on the LXC 131 rerun.
- **Parity** (WSL lane, built as 2026.10.05): `21-static` 0 unexplained with the re-pinned
  dashboard entry; `09-admin` and `11-signin`'s update requests differ only by the update card
  (entry `update-card-pulls-octo`, proposed, waiting for Brandon) and, in 09, the Charts settings
  rows that `rust/main` already has.

## Improvements we want while porting

- [x] **Charts and new songs from Apple Music** (group G, `port/g-charts`; Brandon 2026-10-07).
  Popular right now from Apple's chart (Deezer, then Last.fm, behind it), plus Best New Songs,
  Trending Songs and genre charts as read-only playlists for every listener; `getTopChart?chart=`,
  `getCharts`, `octoTopSongs` [1,2]; the dashboard's Charts card. Plan and Check Mode:
  `~/.claude/plans/octo-charts/`.

The port does not have to stay one to one. Each of these is a known diff with
its reason when it changes an answer.

- [x] **The admin gate as one router layer, default deny.** Axum has no
  endpoint attributes to read, so the gate is a layer with an explicit list of
  open routes; an unknown `/api/admin/<x>` answers 401 too (group J). Done in
  G1 (`middleware::admin_sign_in_gate`). No answer changes: the C# gate also
  answered 401 to an unknown admin path (its catch-all endpoint carries no
  `[AdminOpen]`), so it needs no intentional-diff entry; a test walks every
  admin route with four methods.
- [x] **The signed-out status cache** (C# caches it 10 s). Under load the Rust
  `/api/admin/status` is slower than C# only because it probes five services
  on every call (group J). Done in G1, as the C# does it (ten seconds, one
  probe round at a time).
- [x] **Plain error words instead of .NET's.** The port copied .NET exception
  texts the apps can show ("The request was canceled due to the configured
  HttpClient.Timeout of 100 seconds elapsing.", "Connection refused
  (host:port)"). Done in the final parity round (`8597aa7`), in the port only
  (the C# is frozen): `services::http_client_factory` says "No answer came
  within 100 seconds, so Octo stopped waiting.", "Could not connect to
  host:port: the connection was refused.", "Could not connect to host:port: no
  address was found for host." and "The server answered 404 (Not Found).", and
  every caller uses it. Proposed entry `plain-error-words`. Left as they were:
  the cancellation texts ("The operation was canceled."), which reach the log,
  not an answer, apart from a lookup the app stopped itself.
- [x] **Em dashes in answers.** The not-configured message in
  `subsonic_proxy_service.rs` and the music folder warning in
  `navidrome_identity_service.rs` (`8597aa7`, port only). No corpus request
  reaches either.
- [x] **US spelling in identifiers** (`2b132e6`): `cover_colors`, `COLORLESS`,
  `normalize`, `analyze`, `catalog_url`, `or_canceled` and 50 test names, on
  identifier tokens only. Kept: tokio's `cancelled()`, the `Cancelled` types
  and state variants (written to state files), `SonicStatus.analysed` (a JSON
  field), the cover book's `lowChromaAsGrey`, `greyBelow` and `greyPenalty`
  (read from the shared design file).
- [x] **`.unwrap()` outside tests** (`c5ebd77`): four were left, none
  reachable; each is now infallible. No `.unwrap()` is outside a test.
- [x] **Module names from the port's waves** (`a1d46d9`): `tests_6a1`,
  `helpers_6b1`, `test_support_6a2` and 13 more are `route_tests`, `plumbing`,
  `test_app` and so on, and `routes_6b1`, `routes_6b2` and `routes_n` are
  `admin_controller_routes`, `library_and_job_routes` and `activity_routes`.
- [x] **Smaller cover JPEGs** (`4ccc138`). The placeholder, badged and shrunk
  covers are 4:2:0 as ImageSharp wrote them (`octo_media::cover::jpeg_420`):
  11.7 KB for the 600 px placeholder against C#'s 11.8 KB (17.6 KB before).
  The list covers stay 4:4:4, as the C# asked for them (`YCbCrRatio444`).
- [ ] **Cut the .NET compatibility helpers where tests prove the same
  answers** (`octo_core::common::dotnet`, the `XElement` replica), after the
  cutover, behind the tests.
- [ ] **Split the `octo` crate** only if the loop gets slow again: today an
  edit there rebuilds the tests in 12 s.
- [x] **A `VERSION` file at the cutover.** `build.rs` reads it first; until
  then it reads `octo/octo.csproj`, so a release bump is one edit for both
  builds. Done on `release/rust`: `VERSION` is the release (docker.yml checks a tag and the
  compose image tags against it; install.sh and the update helper read it first). `octo.csproj`
  stays at 2026.10.05, the last C# release, which the C# code in the tree still is, and is
  `build.rs`'s fallback for a tree without `VERSION`.
- [ ] **Move the shared files beside the crates at the cutover** (dashboard,
  Assets, cover design, appsettings, golden samples) and drop the `../../octo`
  paths. Not at the release: the plan moves them with the C# app's removal (§ 6.4 phase 6,
  "move `wwwroot` and assets into `crates/`"), since the C# app reads them from `octo/` until then.
- [ ] **cargo-deny's two ignored advisories** (`paste` under lofty,
  `ttf-parser` under cosmic-text): drop each when its parent moves on.

## Before the port can be gated in CI

- Push `rust/main` so `rust.yml` and `ci.yml` run, then one manual run of
  `docker-rust.yml`; record the times and the cache sizes above. The
  `octo-rust` package starts private on ghcr.io. Superseded on `release/rust`: `rust.yml`'s jobs are
  in `ci.yml`, `docker-rust.yml` is gone (the `octo` entry of `docker.yml` builds the Rust server),
  and no `octo-rust` package is needed; the first CI and image runs happen when Brandon ships. (Nothing is pushed until
  Brandon says so.)
- The test box: LXC 131 had 11 GB free on its disk after the phase 1 runs (the
  parity builder's cache, about 6 GB at most, and one Rust image of 1 GB are
  kept). The Proxmox host is the tight part: it had 4 to 5 GB of memory free,
  and a run pushed it into swap once (see `parity/README.md`); `lxc131.sh`
  checks the host before it starts and stops the run if the host runs short.
- The bridge release (C#, Octo pulled not built, update helper v2) ships
  together with the Rust release, not before (Brain, "MISTAKE AND UNDO").
