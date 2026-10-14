# Octo HTTP parity harness

A black-box harness for the C# to Rust rewrite (`docs/rust-migration/PLAN.md`; the port's
working list is [`docs/rust-port/BACKLOG.md`](../docs/rust-port/BACKLOG.md)). It boots one Octo
build against a fixture Navidrome and stubs for every upstream, replays a hand-written request
corpus, records every response, and diffs two recordings after normalizing the values that
change from run to run.

**Dual track.** Both builds live in this repository until the cutover: the C# app in `octo/`,
the Rust one in `crates/`. The C# side of a comparison is the published image of the release
the port is held to, `ghcr.io/winters27/octo:2026.10.05` (the default `--image`), pulled, not
built. Since phase 1 the corpus covers everything in 2026.10.05, signed in to the dashboard
(see "Signing in"). [`recordings/csharp/`](recordings/csharp/) still holds the 2026.10.03.2
baseline that the Rust golden tests (`subsonic_wire_golden_tests.rs`) read; it is not the
reference for a parity run any more. A run records the C# image afresh (cached on the test box
per corpus version), so compare against that.

A Rust build passes when its recording diffs clean against the C# one, or when every remaining
diff is covered by [`allowlist.json`](allowlist.json) (a known difference, pointing at a row in
[`docs/rust-migration/known-diffs.md`](../docs/rust-migration/known-diffs.md)) or by a reviewed
entry in [`intentional-diffs.json`](intentional-diffs.json) (a deliberate improvement; see
"Intentional diffs").

Everything is Python 3.10+ standard library plus Docker Compose. No `pip install`.

## For port agents: one command, two lanes

Commit first (the binary is built from the working tree and the run uses HEAD, so the script
refuses a dirty tree), then from the checkout of your branch, in WSL:

```sh
parity/run.sh                       # compile here, run the corpus against C# and Rust, diff
parity/run.sh --no-build            # same image as last time (corpus or stub changes only)
parity/run.sh --only '13-*'         # some corpus files (00-setup always runs; C# is recorded again)
parity/run.sh --bytes               # also the bytes-mode diff
parity/run.sh --rust-twice          # also Rust against Rust (the determinism gate)
parity/run.sh --csharp-only --csharp-twice   # corpus work: C# against C#, no Rust
parity/run.sh --attach              # follow this branch's run again, fetch its reports
parity/run.sh --queue               # both lanes: WSL's budget and runs, the box's lock and queue
parity/run.sh --selftest            # a dry run of the lane's own machinery (no build, no Docker)
parity/run.sh --clean               # remove this branch's image and folder (on the lane picked)
parity/run.sh --lane wsl|box ...    # pick the lane yourself
parity/run.sh --stop-run <branch>   # stop a WSL run and its stacks by hand, as its watchdog would
```

[`run.sh`](run.sh) picks a lane. **WSL** (the default when WSL has Docker): WSL's own Docker,
as many stacks at once as WSL's memory budget allows, so the runs of several branches, and the
shards of one run, go side by side. **The box** (LXC 131, [`lxc131.sh`](lxc131.sh), one run
at a time): when WSL has no Docker, when you ask for a box build (`--box-build`,
`--low-memory`), or when WSL's budget is full with stacks already waiting and the box is free.
`--lane box` takes the box whatever; it is `lxc131.sh` with the same options, and `lxc131.sh`
itself still works as before. `--attach` without `--lane` follows the WSL run of the branch,
or the box's when WSL has none.

Either way the run is detached and outlives the script: the script follows its log, fetches
the reports into `parity/.run/wsl-<branch>/` (or `lxc131-<branch>/`) and exits with the run's
exit status (0: the run went through, diffs or not; 143: it was stopped). When the script is
stopped (a background limit, a lost terminal), `--attach` follows the run again. A second
start of a branch whose run is still going is refused (exit 75), and so is `--clean`.

### The WSL lane

`run.sh` compiles `octo` in your checkout as a static musl binary (as for the box, below,
through `cargo-slot`), checks HEAD out into the run's own folder
(`~/.octo-parity/runs/<branch>/src`, sharing your repository's objects, so you can go on
working or switch branches while it runs), copies itself beside it (`runs/<branch>/run.sh`: the
lane's machinery is the script you started; the corpus, the stubs and `parity.py` are the
commit's) and starts the run there under `setsid nohup`. The run builds
`octo-rust-lane:<branch>` from `Dockerfile`'s runtime stage around the binary, records the
corpus against the C# image and the Rust image, and diffs, like the box.

- **Side by side.** Every stack has its own compose project,
  `octo-lane-<branch>-<run id>-<recording>-<shard>`, and so its own volumes; its own block of
  host ports from 21000 to 22999 (blocks of 10, taken under a lock in `~/.octo-parity/ports/`
  for the stack's life and checked free first); and its own two networks on the subnets that go
  with that block, `10.213.<block>.0/25` (sandbox, internal) and `.128/25` (edge). The lane
  makes those networks itself before the stack and removes them after: Docker's own address
  pools hold only about 30 networks for everyone in WSL (two per stack), and they ran out on
  2026-10-08 with 15 stacks up ("all predefined address pools have been fully subnetted").
  No fixed host port, and nothing named like the box's `octo-parity-*` projects.
- **The recordings go at once.** C#, Rust, Rust again (`--rust-twice`) and C# again
  (`--csharp-twice`) are each recorded on stacks of their own at the same time, each stack
  taking a stack's share of the budget for its life, and waiting for it when there is none.
- **The C# cache** is `~/.octo-parity/cs-cache/<corpus digest>-<shard plan>[-waits]/`. The
  box keys by the corpus digest alone; the shard plan is part of the key here because every
  shard starts on a fresh stack, and `-waits` marks a recording made by a `parity.py` that keeps
  wait times (which adaptive waits read). One run records a missing one while others that need
  it wait for it and then use it. `--only` records C# for that run alone.
- **Shards.** When `parity/shards.json` is there and `parity.py` can record one shard of it
  (`run --shard NAME`) and join shards (`merge`), every recording is cut into those shards,
  each on a fresh stack of its own, all at once, and joined with `parity.py merge` into the one
  recording the diff reads (`out/<recording>/all`). Until then a recording is one shard, the
  whole corpus; with `--only` it is always one shard. When `parity.py run` has
  `--waits-from`, the Rust recording of a shard starts once the C# recording of that shard is
  there, and adapts its waits to it.
- **The WSL memory watchdog** runs beside each WSL run (detached like the run, log
  `runs/<branch>/watchdog.log`): every 10 s it reads WSL's `MemAvailable`, and under 1.5 GB it
  stops its own run: everything in the run's session (TERM, KILL after 10 s) and its stacks
  (`docker compose down -v`), nothing else. When several runs are going, the newest is stopped
  first: an older run's watchdog waits 25 s and looks again. The Proxmox host watchdog is for
  the box lane only.
- **Stacks a killed run left behind** (its supervisor gone, or an up that failed half way)
  are taken down by the next WSL run, networks included. `run.sh --stop-run <branch>` stops a
  WSL run by hand the way the watchdog does.

Files: `~/.octo-parity/runs/<branch>/` holds `run-<id>.log`, `.info` and `.done` (as on the
box), `current`, `out/` (the recordings; `out/jobs/` has each stack's log and state) and the
reports.

### WSL's budget (`cargo-slot`)

`~/bin/cargo-slot` is now one memory budget for cargo builds and parity stacks
([`budget.sh`](budget.sh); `bash parity/budget.sh --install` puts it there). `cargo-slot CMD`
works as before and takes a cargo build's share; `run.sh` takes a stack's share
(`cargo-slot --stack`, or `--rust-stack` for a stack of the Rust build) for every stack it
starts; `cargo-slot --status` shows who holds what and who waits. A build that does not fit
waits in line until enough is free. A stack is a polite taker: it takes its share only while a
build's share stays free beside it, and waits out of line, so a waiting stack, which then holds
its share for up to an hour, never stands in line ahead of a build.

| | Units of 256 MB | GB | Measured on 2026-10-08 |
| --- | ---: | ---: | --- |
| The budget | 50 | 12.5 | WSL has 15.6 GB; about 1.1 GB goes to the system, dockerd and the shells, and 2 GB is left as headroom |
| A cargo build | 14 | 3.5 | A test rebuild after an edit peaks at 2.8 GB (2.3 to 3.4 GB in phase 0), the musl release build at 3.4 GB. A cold test build reaches 4.7 to 5.3 GB, which the headroom covers |
| A parity stack (C#) | 5 | 1.25 | The C# stack (two Octos, Navidrome, the stubs, the gateway) peaks at 1.2 GB of cgroup memory over the whole corpus, 0.8 GB of it not page cache; `parity.py` itself 30 MB |
| A stack of the Rust build | 3 | 0.75 | The same stack with the Rust Octos peaks at 0.1 to 0.3 GB (eight Rust shard stacks at once: 91 to 284 MB each, under 0.1 GB of it not page cache) |

Builds alone: three at once, as before. A stack starts only while a build's share stays free
beside it, so stacks never take the last 3.5 GB and one build always finds room: with no build
going, stacks can take 9 GB (twelve Rust stacks, or seven C#), with one build 5.5 GB (seven
Rust stacks), with two 2 GB (two Rust stacks). A second or third build waits while stacks hold
the rest. WSL's page cache counts as available (it is given back on demand), which is why the
watchdog reads `MemAvailable` and a stack's share is sized by what it uses besides cache. Each unit is a
file in `/tmp/octo-budget`, held with `flock` on an open file the command and its children
inherit, so a share is freed when the last of them exits, however it ends (a killed holder
frees its share). One taker gathers units at a time (the gate), so two takers never each hold
half of what they need. A build still holding a slot of the old three-slot `cargo-slot` counts
as a cargo build until it ends.

### The box lane (LXC 131)

The harness runs on the test box through the Proxmox host (`ssh pmx`): `run.sh --lane box`, or
`parity/lxc131.sh` with the same options, plus these two:

```sh
parity/lxc131.sh --box-build        # compile on the box instead (avoid: see below)
parity/lxc131.sh --box-build --low-memory   # that, with one job and no LTO
```

**Compile in WSL, never on the Proxmox host.** By default `lxc131.sh` compiles `octo` in your
checkout as a static musl binary (`cargo build --release --locked --target
x86_64-unknown-linux-musl`, into `target/x86_64-unknown-linux-musl/release/octo`), sends it to
the box beside the commits, and [`box-run.sh`](box-run.sh) `--prebuilt` builds the image from
`Dockerfile`'s own runtime stage with the build stage swapped for that binary
(`.parity-bin/Dockerfile` in the box checkout): Debian packages, cached after the first time,
and a copy. The image is the release image in every way but how the binary was compiled.
Static musl because a WSL glibc build needs a newer glibc than bookworm-slim has and does not
start there. Once per WSL install: `rustup target add x86_64-unknown-linux-musl` and
`apt install musl-tools`. The compile goes through `~/bin/cargo-slot`, like every build in WSL.

It bundles the commits the box lacks, checks them out in `/opt/octo-parity/runs/<branch>/src`
and runs [`box-run.sh`](box-run.sh) there: the image (above; with `--box-build`, the whole
`docker buildx build -f Dockerfile` on the box, own builder `octo-parity`, capped at
3.5 GB), the corpus against the C# image (recorded once per corpus version and kept in
`/opt/octo-parity/cs-cache/`) and against the Rust image, then the diff. It prints the summary
and the diffs per backlog group, and copies the full reports to
`parity/.run/lxc131-<branch>/report.txt`. A box build takes about 8 minutes cold, the
prebuilt image under a minute. A recording of the whole corpus takes about 11 minutes against
C# and 45 to 55 against Rust: about 45 of those are waits for features the port does not have
yet, timing out (57 to 65 timed-out waits on 2026-10-08, 2,400 to 2,800 s). Starting and
removing the stack is about 26 s of it, so keeping the stubs and Navidrome up between
recordings would save little, and it is not done: the stubs count calls (`times`) and write
files, and the corpus changes Navidrome's library, so every recording starts from a fresh
stack. To spend less time on the box, rerun only the corpus files you work on (`--only`; the C#
side is then recorded again too, about 11 minutes for the whole corpus and less for a part).
One run at a time on the box: a second one waits for the first.

**The run is detached on the box, so it outlives the script.** `lxc131.sh` starts
`box-run.sh --start`, which runs the box side under `setsid nohup` (its wait for the box lock
included) and returns at once; `lxc131.sh` then follows the run's log over ssh until the run
writes its done marker, fetches the reports and exits with `box-run.sh`'s exit status (0: the
run went through, diffs or not; 143: it was stopped). A copy of the log lands in
`parity/.run/lxc131-<branch>/run-<id>.log`. When the script is stopped (a background limit, a
lost terminal), the run goes on:

```sh
parity/lxc131.sh --attach   # follow it again from the start of its log; fetches the reports at the end
parity/lxc131.sh --queue    # who holds the box lock, for how long, and who waits behind whom
```

`--attach` works on a run that is still waiting, running or already done (then it prints the
log and fetches the reports). A second start of a branch whose run is still going is refused
(exit 75) with a pointer to `--attach`, and so is `--clean`. On the box each run has its files
in `/opt/octo-parity/runs/<branch>/`: `run-<id>.log`, `run-<id>.info` (the supervisor's pid,
when it was queued, its options), `run-<id>.locked` (when it got the lock), `run-<id>.done`
(its exit status, written last) and `current` (the latest id). A run started by an older
`lxc131.sh` (from a branch without `--attach`) still runs in the foreground of its ssh session
and dies with it; `--queue` shows those as "in the foreground of an older lxc131.sh". Both kinds
take the same lock (`/opt/octo-parity/run.lock`, `flock`) and use the same stack names, so they
queue behind one another as before.

`--queue` reads the lock straight from the kernel (which process's open file holds the flock)
and lists the holder (with how long it has held the lock, when the run recorded it), each
waiting run with how long it has waited and whom it waits behind, and strays: a `flock 9` left
by a run killed while it waited (it lets go the moment it gets the lock), or a process still
holding the lock after its run was killed. Waiters are not strictly first come first served.

`--selftest [--sleep S]` checks this machinery without queuing behind real runs: it runs as
`<branch>-selftest` with its own lock (`/opt/octo-parity/selftest.lock`), prints a line every
5 s for S seconds (default 60), writes a one-line report and ends, with a host watchdog of its
own. It still sends the commits and checks them out on the box, but builds nothing and starts
no Docker.

It touches only its own things on the box: `/opt/octo-parity`, the compose projects
`octo-parity-cs` and `octo-parity-rs`, the images `octo-rust-parity:<branch>` and the builder.
Never the starter rig (`/opt/octo`, project `octo`). It refuses to build or pull with less than
8 GB free on the box's disk.

**The Proxmox host is shared, and short of memory.** LXC 111 (Brandon's own Octo) runs on it.
On 2026-10-07 a run while the host had about 5 GB free pushed it into swap until nothing
answered for minutes. So `lxc131.sh` starts only with 3.5 GB free on the host
(`PARITY_MIN_HOST_MB`), and a watchdog stops the run if the host falls under 1.5 GB. The
watchdog ([`host-watchdog.sh`](host-watchdog.sh)) runs on the Proxmox host itself, detached
with `setsid nohup` for the life of the run, so it does not die with `lxc131.sh` either: it
reads `MemAvailable` every 15 s, and under 1.5 GB it has the box stop the run that holds the
box lock (`box-run.sh --stop-holder`: TERM to that run's `box-run.sh` and everything under it,
KILL after 10 s, then the `octo-parity-cs` and `-rs` stacks down). That is the run using the
memory, whether it is the watchdog's own run or one it waits behind. Waiting runs, other
containers and the starter rig are left alone (the watchdog that lived in `lxc131.sh` before
`--attach` stopped every `parity.py` on the box, and older branches still carry it). It exits when its run writes the done marker, when the run has gone without one, or
when the container stops. It reads the run's files through the container's
`/proc/<init>/root`, so it costs no `pct exec` per poll. Its log is
`/root/octo-parity-watchdog-<branch>.log` on the Proxmox host. `--attach` starts it again if
it is not running. Never run two parity runs side by side on the box, and never raise the limits.
On 2026-10-07 another container's load swung by 4 GB a minute and left the host about 3.6 GB
to spare, so the box build (its LTO link takes about 3 GB) tripped the watchdog every time,
once with the host at 1 GB and its swap full. That is why the compile moved to WSL. If you
must compile on the box, `--box-build --low-memory` builds the same program with one job and
no LTO, slower and with its own cache, and a later `--no-build` run reuses that image.

## Running it by hand


```sh
# Record a build: recreate the stack from scratch, replay the corpus, tear it down again.
python3 parity/parity.py run --image ghcr.io/winters27/octo:2026.10.05 --out /tmp/parity/csharp

# Compare two recordings. Exit 0 = no unexplained diff, 1 = diffs (report on stdout), 3 = --only
# picked no request (--only takes what record's does: a file name, a glob, several with commas).
python3 parity/parity.py diff /tmp/parity/csharp /tmp/parity/csharp-again
python3 parity/parity.py diff --mode bytes /tmp/parity/csharp /tmp/parity/csharp-again

# The Rust build (docker build -f Dockerfile -t octo-rust:dev .). A different project and
# ports let it run beside a C# stack.
python3 parity/parity.py run --image octo-rust:dev --project parity-rust --port 18580 --nd-port 18553 \
    --out /tmp/parity/rust
python3 parity/parity.py diff /tmp/parity/csharp /tmp/parity/rust --report /tmp/parity/rust.txt
```

The steps can also run one at a time, for example to poke at a live stack:

```sh
python3 parity/parity.py up                  # fresh stack; Octo on 127.0.0.1:18480, Navidrome on :18453
python3 parity/parity.py record --target http://127.0.0.1:18480 --out /tmp/parity/x [--only '04-media/*'] [-v]
python3 parity/parity.py show /tmp/parity/x '09-admin/settings*'      # print recorded entries
docker compose -p parity logs stubs | grep UNMATCHED                  # upstream calls with no stub
python3 parity/parity.py down                # containers, networks and volumes (never images)
```

Options: `--image` (also `OCTO_IMAGE`), `--port` (`PARITY_PORT`, default 18480), `--nd-port`
(`PARITY_ND_PORT`, 18453), `--project` (`PARITY_PROJECT`, `parity`), `--settle` (seconds to wait
after Octo first answers, default 10), `--keep` on `run` to leave the stack up.

A run of the whole corpus takes about 25 minutes against C# (most of it waiting for background
jobs and Navidrome rescans). The stack is capped at roughly 2.3 GB of RAM (`mem_limit` on each
service) and needs two small images besides Octo: `deluan/navidrome:0.64.2` and
`python:3.13-alpine`. Nothing is built; nothing is pruned. `run` saves the containers' logs in
`<out>/logs/` (`stubs.log` lists every upstream call with the mapping row that answered it, or
`UNMATCHED`). `--until-patience N` (default 3): after N waits in one corpus file time out, its
other waits get at most 20 s, so a build missing a feature does not sit out every wait (see also
"Adaptive waits").

**The C# image.** The harness only ever runs a prebuilt image; nothing in `parity/` builds
one. The released C# image is on ghcr.io (`ghcr.io/winters27/octo:<release>`). To compare a
C# change that is not released yet, build it from this tree with its own Dockerfile:

```sh
docker build -f Dockerfile.csharp -t octo-csharp:dev .
python3 parity/parity.py run --image octo-csharp:dev --out /tmp/parity/csharp-dev
```

**The 2026.10.03.2 baseline** in `recordings/csharp/` is what the Rust golden tests read; a port
group that changes those answers moves its golden tests onto a fresh 2026.10.05 recording. To
record one (`parity.py digest` names the corpus version a recording was made from, and `diff`
warns when two recordings come from different corpus versions):

```sh
rm -rf parity/recordings/csharp
python3 parity/parity.py run --out parity/recordings/csharp
python3 parity/parity.py run --out /tmp/parity/check
python3 parity/parity.py diff parity/recordings/csharp /tmp/parity/check                # must be 0
python3 parity/parity.py diff --mode bytes parity/recordings/csharp /tmp/parity/check   # must be 0
```

## Shards

One stack replays the corpus one file after another: about 11 minutes against C# and 45 to 55
against Rust. Most of that is waiting (timers, background jobs, Navidrome scans, and for Rust the
waits for features it does not have yet), so the corpus is cut into shards that run side by side,
each on a fresh stack of its own, and joined again before the diff.

[`shards.json`](shards.json) is the plan, an ordered list of shards:

```jsonc
[
 {"name": "acquisition",                          // lower-case letters, digits and dashes
  "files": ["96-covers-explicit-writes"],         // the files it owns, in corpus order
  "setup": ["20-library-edits"],                  // run first, not kept
  "reads": {"96-covers-explicit-writes": {"20-library-edits": "why"}}}   // what a file depends on
]
```

- Every corpus file is owned by exactly one shard; merge keeps the owner's recording of it.
  `00-setup` runs in every shard (as with `--only`), whoever owns it.
- `setup` holds request globs (`--only` style) run for what they capture or leave behind; their
  answers come from the shard that owns them. A whole file there costs its whole run time.
- `reads` says, per corpus file, which earlier files leave state its answers depend on, with the
  reason. It is kept with the shard that runs the file.

`parity.py shards check` fails when a corpus file is in no shard or in two, when a shard lists its
files out of corpus order, when a request uses a `{{variable}}` (or a sign-in) captured by a
request its shard does not run before it, and when a file's `reads` are not all run in full
before it in its shard. `shards show --times RECORDING` prints each shard with its length from a
recording's per-file times (every recording keeps them, `fileSeconds` in `meta.json`), and
`shards digest` prints the plan's hash (the C# cache key includes it, so a new plan records C#
again).

```sh
python3 parity/parity.py shards check
python3 parity/parity.py run --shard library --project p-library --port 18600 --nd-port 18605 \
    --out /tmp/parity/cs-library      # one shard: its files and setup, on its own stack
python3 parity/parity.py merge /tmp/parity/cs /tmp/parity/cs-*/   # one recording, corpus order
python3 parity/parity.py diff /tmp/parity/cs /tmp/parity/rs
```

Each shard needs its own `--project` and ports (the main port and the next two, and the Navidrome
port). `merge RUN-DIR SHARD-DIR...` copies each corpus file from the shard that owns it (from
`meta.json`'s `shard`, which `--shard` writes), refuses two owners, a missing file (unless
`--partial`), and shards recorded from other corpus inputs, another image or another plan, and
copies each shard's logs to `RUN-DIR/logs/<shard>/`. Every stack mints its own Navidrome ids and
sign-ins on its own ports, so the merged `meta.json` keeps each shard's host and volatile captures
with the files it recorded (`parts`), and `diff` normalizes each file with its own shard's values.
`diff`, the per-group report and `--only` work on a merged recording as on any other, and a merged
recording diffs against an unsharded one.

**How the plan was made.** By what each file writes and reads, then measured: every corpus file
recorded alone after `00-setup` on a fresh C# stack and diffed against an unsharded C# recording.
Twenty files answered the same alone; the other six showed what they read:

| File | Reads what these leave | Seen as |
| --- | --- | --- |
| `08-catchall-native` | `02-browsing` | the artists' `externalInfoUpdatedAt`, which Navidrome fills in after 02's artist info calls |
| `10-static-cors` | `05-playlists` | the internet radio station 05 leaves |
| `18-dashboard` | `14-settings-media` | 14's saves write the whole settings file, so 18's saves answer with every section under `persisted` |
| `20-library-edits` | `19-health` | 19's fixes in the library actions history and the tag edits journal |
| `90-mutations` | `05-playlists`, `14-settings-media`, `20-library-edits` | the admin's Deep Cuts list, made by 05's `getPlaylists` and kept for the day; the whole settings file; 20's retags in the file sizes |
| `96-covers-explicit-writes` | `20-library-edits` | 20's trash folder and file sizes |

Each smallest set was then checked the same way (for example `05`, `14`, `20` and `90` alone give
`90` its unsharded answers; `20` without `19` is enough for `96`). One file also takes a capture
from another: `90-mutations` the browse token from `09-admin/browse-auth-admin` (the checker
finds this by itself).
`99-signed-out-writes` ends with the restart, so it is last in its shard.

Two files wait on timers, which set where they go:

- `15-radio` waits for Sounds alike's first look, about 125 s after Octo starts, so `14` runs
  before it in the `radio` shard.
- `16-lists` waits for the second Made for you attempt for the listener, which comes 5 minutes
  after the first; unsharded, `05-playlists`' `getPlaylists` made the first long before. The
  `lists` shard runs `05` first; even so `16` waits about 5 minutes there, which makes it a shard
  of its own (nothing before it would shorten the wait).

| Shard | Owns | Runs first, not kept |
| --- | --- | --- |
| `radio` | 14, 15 | |
| `lists` | 16 | 05 |
| `imports` | 12, 13, 21 | |
| `dashboard` | 17, 18 | 14 |
| `library` | 00 to 11, 19, 20, 90, 99 | 14 |
| `acquisition` | 96 | 20 |

Each shard's recording time on 2026-10-08 in WSL, two recordings of each (C# 2026.10.05, and
the Rust build of `rust/main` `4ff2c86`, every group merged, with adaptive waits), not counting the
stack's start and removal (about 30 s) or waiting for the budget:

| Shard | C# | Rust |
| --- | ---: | ---: |
| `radio` | 278 s | 273 to 275 s |
| `lists` | 312 to 313 s | 307 to 310 s |
| `imports` | 42 to 47 s | 42 to 46 s |
| `dashboard` | 138 to 147 s | 133 to 135 s |
| `library` | 260 to 272 s | 251 to 261 s |
| `acquisition` | 262 to 273 s | 259 to 263 s |

The same commit unsharded, on one stack each: 683 s against C# and 663 s against Rust. `lists`
sets the length: `16-lists` waits about 5 minutes for Made for you's second attempt wherever it
runs. Against a Rust build that still lacked features (`19db112`), `library` and `acquisition`
were the long ones (about 530 s each): `20` reads `19`, `90` reads `20`, and `96` needs what `20`
leaves, so they cannot be split further.

## Adaptive waits

A Rust build without a file's feature sits out that file's waits until each times out (57 to 65
of them, 2,400 to 2,800 s of a Rust recording on 2026-10-08). Every recording now keeps how long
each wait took (`waitMs`), its limit (`waitLimitMs`) and when it ended on a clock that starts with
the stack's Octo container (`atMs`); the diff never compares these. `record` (and `run`)
`--waits-from REF` reads them from a reference recording, the C# one, and gives each wait a new
limit:

- C# met it: `max(20 s, 3 x its C# time)`; when C# took a second or more (a job, a download, a
  Navidrome scan, not an answer that was already there), at least its C# time plus 60 s; and at
  least until 20 s past the instant C# met it on that clock (a timer counted from Octo's start fires at the same instant in both builds, however
  fast each got there). Never more than the corpus's `timeoutMs`.
- C# timed out too, or the reference has no time for it: the corpus's `timeoutMs`, as before.
- `--until-patience` still applies on top: after that many timeouts in one file, at most 20 s.

A wait is still recorded as met or timed out exactly as before, so the answers stay comparable;
only how long a missing feature is waited for changes. The reference must be recorded with the
same shard plan, so the instants line up (the C# cache is per plan). For a shard, `--waits-from`
reads the reference's recording of the same shard: give it that shard's folder (as
`parity/run.sh` does), or a merged recording whose `parts` folders are still there. Only the
same shard ran the shard's setup files on the same clock; with any other reference the instants
of files the shard does not own are not used.

**The numbers, 3, 20 s and 60 s,** come from the waits of one sharded C# and one sharded Rust
recording (2026-10-08, the Rust build of `3c4c593`):

- Of the 65 waits both builds met, the slowest in Rust took 4.3 s
  (`96-covers-explicit-writes/covers-apply-aurora-done`, 4.3 s in C# too), and none took more
  than a third of `max(10 s, 3 x C#)`.
- The waits Rust does not meet yet take C# 0 to 12 s, timers aside: Navidrome seeing a change
  5.6 to 8.2 s, a download 8 to 12 s. Three times that leaves room for a port that is slower
  than C#.
- A floor of 10 s was tried first, and it cut a correct wait short: `19-health/plant-wait`
  waits on Navidrome alone (C# met it at once), and on a busy WSL Navidrome took over 11 s. The
  floor is 20 s, the time `--until-patience` already gives a build that has the feature.
- `max(20 s, 3 x C#)` alone cut one too, once `rust/main` had group D: a heart's download
  took Rust 12 s in one recording and over 40 s in the other, run at the same time
  (C# 12 s, limit 39 s). A job that ends in a Navidrome scan can wait for the scan before it,
  and Octo spaces its own scans 30 s apart, so a wait on background work gets a minute on top of
  C#'s time.
- Waits that ride a fixed timer (Sounds alike's passes in `15-radio`, 41 to 60 s in C#) get 124
  to 181 s, which is more than the corpus's 90 s for two of them, so they keep their 90 s.

What it saves, measured in WSL on 2026-10-08 with the Rust build of `3c4c593`: the whole corpus
on one stack took 3,055 s; in shards without adaptive waits 1,096 s (the longest shard 1,033 s
of recording, five shards then); in the six shards with adaptive waits the longest shard records
in 598 s. Adapted per shard: `acquisition` 36 waits, `library` 28, `imports` 9, `dashboard` 6,
`lists` 3, `radio` 1. Rust against Rust stays at 0 diffs with adaptive waits, and the C# against
Rust diff lists the same requests as without them.

## What runs

```
                 host 127.0.0.1:18480 / :18453
                              │
                         ┌─────────┐     network "edge" (published ports only)
                         │ gateway │
                         └────┬────┘     network "sandbox" (internal: no route out)
        ┌─────────────────────┼──────────────────────┐
   ┌────┴────┐          ┌─────┴─────┐          ┌─────┴─────┐
   │  octo   │ ───────▶ │ navidrome │          │   stubs   │  Lidarr :8686,
   │ (image  │          │  0.64.2   │          │ (python)  │  yt-dlp shim :8090,
   │ under   │ ─────────────────────────────▶  │           │  HTTPS :443 for every
   │ test)   │                                 │           │  hard-coded upstream host
   └─────────┘                                 └───────────┘
```

| Service | What it is |
|---|---|
| `octo` | The image under test (`OCTO_IMAGE`). Configured only through env vars in [`docker-compose.yml`](docker-compose.yml); `/app/config` is a fresh volume each run, seeded by `config-seed` with [`fixtures/config/`](fixtures/config/) (5 fetched songs). The library is a fresh writable copy of the fixtures per run (`parity/.run/<project>/music`), and `fixtures/planted/` is mounted read-only for `19-health` and `20-library-edits` to copy in. |
| `octo-alt` | The same image with the sign-in off and the dashboard on its own port (see "The second Octo"). |
| `navidrome` | `deluan/navidrome:0.64.2` over the fixture library (read-only). External agents, insights and the auth rate limiter are off. |
| `stubs` | [`stubs/stub_server.py`](stubs/stub_server.py), one process answering Lidarr, the yt-dlp shim, octo-sonic and every HTTPS upstream from the JSON mappings in [`stubs/mappings/`](stubs/mappings/). |
| `gateway` | [`stubs/gateway.py`](stubs/gateway.py), a TCP forwarder. Docker cannot publish ports from an internal-only network, so this is the one container on both networks. |

### Fixtures

- **Music library** ([`fixtures/music/`](fixtures/music/), 260 KB, checked in): 10 tracks of 2-4 s
  sine tones by 3 artists on 3 albums, one container per album: FLAC (with `cover.jpg` and a
  sidecar `.lrc`), MP3 (ID3v2.4, `folder.jpg`) and M4A/AAC (no cover). Names include
  `Café del Mar`, `Bjørn Ålund`, `Ágætis Prófun`, `Þögn`, `宇多田テスト`, `桜の歌` and `Ñandú (Remix)`.
  [`fixtures/make-library.sh`](fixtures/make-library.sh) regenerates it with ffmpeg using
  bit-exact flags and fixed mtimes, and writes `fixtures/music.sha256`. The output is
  byte-identical between runs of one ffmpeg build, but encoder output differs between builds,
  and Navidrome reports sizes and bit rates, so the files are checked in. Regenerating means
  re-recording the baseline. Git does not keep mtimes, and Navidrome relays them as
  `Last-Modified` (`download`, `HEAD stream`), so `parity.py up` sets every file and folder
  back to the baseline's instant (`2020-01-01T20:04:05Z`) before it starts the stack.
- **Navidrome users:** `admin` / `parity-admin` (created by `ND_DEVAUTOCREATEADMINPASSWORD`)
  and the non-admin `listener` / `listener-pass` (created by `parity.py up` through the
  native API).
- **Octo config:** Navidrome URL and admin login, Lidarr and shim URLs pointing at the stubs,
  a Last.fm API key (so search discovery runs) with radio, stations and mixes off, lyrics
  fetching on with every source, update checks off, library actions off. See the comments in
  `docker-compose.yml`.

### Hard-coded HTTPS upstreams

Octo calls these with fixed `https://` URLs (from `grep -rn 'https://' crates/`): `api.deezer.com`
(plus the `*.dzcdn.net` image CDN), `itunes.apple.com` (and `is1-ssl.mzstatic.com`),
`musicbrainz.org`, `coverartarchive.org`, `api.acoustid.org`, `ws.audioscrobbler.com` and
`www.last.fm`, `lrclib.net`, `lyrics.kugou.com`, `mobileservice.kugou.com`, `music.163.com`,
`api.lyrics.ovh`, `api.github.com`, `api.listenbrainz.org` and `ntfy.sh`.

**Approach: intercept, don't switch off.** The `stubs` container carries a docker network alias
for each of those host names, so inside the sandbox they resolve to it. It serves TLS on 443
with one certificate whose SANs list every host, signed by a test CA
([`certs/`](certs/), regenerated by `certs/make-certs.sh`, keys checked in on purpose). The Octo
container gets that CA as its only root store through `SSL_CERT_FILE` / `SSL_CERT_DIR`
(OpenSSL, which .NET on Linux uses, honors both), so no derived image is needed. The stub
routes by the `Host` header. Because the network is `internal`, any host without an alias
fails to resolve instead of reaching the internet, which keeps every run deterministic.

**For the Rust build:** this works as long as its HTTP client trusts the platform store as
configured by `SSL_CERT_FILE` (reqwest with `rustls-native-certs` or `rustls-platform-verifier`
on Linux, or `native-tls`). A client built with compiled-in `webpki-roots` would reject the
stubs; in that case add the parity CA another way rather than turning features off.
Checked on 2026-10-04 (task 2-D) with the workspace reqwest 0.13 (`rustls`, platform verifier):
a request to `https://api.deezer.com` resolved to an `openssl s_server` presenting
`certs/stub.crt` succeeds with `SSL_CERT_FILE=certs/ca.crt`, and equally with only
`SSL_CERT_DIR` pointing at a directory holding `ca.crt`; without either it fails with
`InvalidCertificate(UnknownIssuer)`.

Keep the host list in three places in sync: `certs/make-certs.sh` (SANs), the `stubs` aliases in
`docker-compose.yml`, and the mappings.

### Stub mappings

`stubs/mappings/*.json` are tried in file-name order, then in list order; the first match wins.
Each entry names a `service` (the Host header for HTTPS hosts, else `lidarr` or
`shim`), an optional `method`, a `path` or `pathRegex` (matched against the raw,
percent-encoded path), optional `query` regexes, and a `body` (JSON), `bodyText` or `bodyFile`
(served with Range support). Rows can also match the request body (`bodyRegex`); answer only
their first N hits (`times`, which gives sequences); serve a fixture's byte size
(`{{size:bodies/x}}`); and fill in dates relative to today (`"template": true`, `{{date:-N}}`). The stub server's
docstring has the details. Unmatched calls get `404 {"error":"no stub"}` and an `UNMATCHED`
line in the stubs' log (`<out>/logs/stubs.log` after a `run`).

The mapping files are numbered by who uses them: `05-downloads` and `06-acquisition` (13),
`35-covers-explicit` (17, 96), `35-radio` (15), `35-settings-media`
(14), `36-lists` (16), `50-health` (19), `50-spotify` (12), `51-library-edits` (20). Each keeps to
its own Deezer ids and names, because a row in an earlier file answers any call it fits.

The data set ([`40-discovery.json`](stubs/mappings/40-discovery.json)): Last.fm `track.search`
returns three songs by the discovery-only artist **Zephyr Echo** (`Glass Harbor`, `Neon Tide`,
`Paper Moons`) for queries containing "zephyr", and `Café del Mar` (local) plus `Borealis`
(external) for "aurora". Deezer knows artist 1001, album 2001 and tracks 3001-3003. LRCLIB has
synced lyrics for `Polar Night`, lyrics.ovh has `Glass Harbor`. Everything else gets an empty
or not-found answer in the upstream's own shape ([`90-https-fallbacks.json`](stubs/mappings/90-https-fallbacks.json)).
The shim answers every search with video `parityVid01` and streams `stubs/bodies/tone.mp3`;
the Zephyr Echo songs have their real lengths (241, 215 and 244 s).

## The corpus

[`corpus/*.json`](corpus/) runs in file order, request by request, against one fresh stack.

```jsonc
{
  "group": "subsonic-browsing",
  "description": "...",
  "defaults": {"auth": "p"},              // merged into every request
  "requests": [
    {"name": "song-local-json",           // unique per file; the id is "<file>/<name>"
     "method": "GET",                     // default GET
     "path": "/rest/getSong",
     "auth": "p",                         // p | t (token+salt) | enc | bad | listener | apikey | u-only | none
     "client": "parity",                  // the c= parameter
     "f": "json",                         // appended after the auth params; null drops a default
     "query": [["id", "{{song_flac1}}"]], // pairs keep order and repeats; or "rawQuery": "..."
     "headers": {"Range": "bytes=0-9"},
     "body": {"json": {...}},             // or {"form": [[k, v]]}, {"raw": "...", "contentType": "..."},
                                          // or {"multipart": [{"name", "value"} | {"name", "filename",
                                          // "contentType", "file": "fixtures/..." | "text": "..."}]}
     "session": "admin",                  // dashboard sign-in: admin | recovery | null (signed out)
     "server": "main",                    // main | alt | alt-admin (the second Octo, below)
     "capture": {"var": "json:a.b[0].c"}, // also header:Name, cookie:name, regex:(...)
     "until": {"from": "json:state", "in": ["done"]},  // re-send until true (also "notIn",
                                          // "regex", "status"); {{vars}} work; the last answer is kept
     "timeoutMs": 60000, "pollMs": 500,   // for "until"
     "unordered": true,                   // upstream order undefined: sort arrays / children
     "anyKeyOrder": true,                 // bytes mode compares this JSON parsed (C# member order varies)
     "keepLengthHeaders": true,           // Content-Length / Transfer-Encoding count in structural mode
     "ignoreKeys": ["positionMs"],        // drop these members/attributes before comparing
     "backlog": "D,B",                    // the backlog groups this request checks (default "base")
     "delayBeforeMs": 1500,
     "timeout": 60},
    {"name": "history-file",              // not HTTP: a command in the Octo container (or octo-alt
     "exec": ["cat", "/app/config/downloads-history.json"]}  // with "server"); exit code + stdout
  ]
}
```

`{{var}}` substitutes a captured value or a built-in (`admin_user`, `admin_pass`,
`listener_user`, `listener_pass`, `salt`, `token`, `enc_pass`, `api_key`, `host`, `host_alt`,
`host_alt_admin`). A capture written as a string is **volatile**: the diff replaces its value
with `{{var}}` everywhere in that recording (Navidrome's random song, user, playlist and session
ids, run ids, sign-in cookies). A capture written as `{"from": "...", "normalize": false}` is only
reused, and must itself match between builds (name-derived album/artist ids, Octo's
sha256-derived external ids, import list ids, static-file ETags). A request that names a
variable no earlier request captured is recorded as skipped (a diff when the other build ran
it). `--only GLOB` always keeps `00-setup`, whose captures the rest needs.

| File | Requests | Covers (endpoints.md section) |
|---|---:|---|
| `00-setup` | 9 | Captures: album/artist/song ids, Navidrome JWT via `POST /auth/login` through the relay, external ids from a discovery search, the admin and recovery sign-ins |
| `01-system` | 43 | 3.1 and 2.2-2.4: `ping` in xml/json, `.view`/plain, auth `u/p`, `t/s`, `enc:`, wrong password, non-admin, `apiKey`, `u` only, none; `jsonp`, `f=JSON`, `F=json`, case and trailing slash, POST form/JSON bodies (body overrides query), PUT/HEAD/OPTIONS falling to the catch-all; `getOpenSubsonicExtensions` merge; `jukeboxControl`; system relays |
| `02-browsing` | 65 | 3.2 and 3.9 relays: `getSong`/`getArtist`/`getAlbum` local (merge with the catalog), external, legacy `ext-`, `pl-`, missing and unknown ids, jsonp; album/artist info v1/v2; random and similar songs (radio off); `getArtists`, `getIndexes`, `getMusicDirectory`, album lists, genres, starred, now playing, top songs, play queue, bookmarks, shares, repeated ids |
| `03-search` | 25 | 3.3: `search3`/`search2` local-only, mixed, discovery-only, CJK and diacritics, quoted, type-ahead, songs/albums only, album offset, later song page, empty query, Symfonium sync walk, folder, wrong password, jsonp, POST form |
| `04-media` | 42 | 3.4: `stream` local (FLAC/MP3/M4A, Range, open-ended Range), external through the shim (with Range), auth failures, missing/unknown id; HEAD and `download` through the catch-all; `getCoverArt` local, sized, no art, missing id, `octo-radio`, registry ids with and without `c=Octo`, legacy ids, `pl-`, station id; `getTranscodeDecision`; `radio/stream` token routing |
| `05-playlists` | 19 | 3.5 read side: `getPlaylists`/`getInternetRadioStations` merges, jsonp, wrong password, Octo-id read-only refusals |
| `06-lyrics` | 24 | 3.7: `getLyricsBySongId` (sidecar, LRCLIB, none, external, enhanced), `getLyrics` (relay, live fill-in), `getLyricsCandidates`, `setLyricsChoice` |
| `07-octo-extensions` | 15 | 3.8: `getAcquisitions`, `getLibraryActions`, `getUpgrades`, `libraryAction` |
| `08-catchall-native` | 34 | 3.9 and 2.2: `GET /`, Octo-owned 404s, external-id safety net and element naming, unknown endpoints, DELETE on a Subsonic route, POST bodies through the faithful relay, native `/api` with JWT (lists, Octo's external song/album/artist answers, discovery appends, radio playlist ids) |
| `09-admin` | 97 | Section 4 and 2.9: `/admin` redirect, every safe admin GET, the guard (OPTIONS, preflight, writes without `X-Octo-Admin`, CORS stripping), every browse-session endpoint without and with a session (cookie, header, stale cookie hiding a header), browse auth failures and success, validation 400s, side-effect-free POSTs |
| `10-static-cors` | 25 | Sections 5 and 7: static files plain, HEAD, `If-None-Match`, Range (also unsatisfiable), Brotli/gzip, upper-case path, `/Assets`; CORS on simple requests, preflights, errors and admin 403s; `X-Forwarded-Proto` |
| `11-signin` | 130 | Group J: every `/api/admin` GET signed out and signed in, unknown admin paths, bogus sign-ins, the recovery session, the app's Subsonic sign-in on `[AdminAppCall]` routes, the second Octo (written by `tools/admin_routes.py`) |
| `12-imports` | 206 | Group H: file, zip and link imports and their caps, Spotify connect, finish, callback and reads (stubbed accounts and API), lists, playlists, the trickle controls, `getImports`, `getImport`, `importAction` |
| `13-downloads` | 96 | Groups D and B: the downloads list and log, acquisitions, `getAcquisition`, `clearAcquisitions`, Find songs from the dashboard and Subsonic, picks and their refusals, the relayed download file name |
| `14-settings-media` | 177 | Groups I, S, O, K, A, J: secret masking and the placeholder rules, transcode lengths, outside albums the library holds, the bare address 302, Last.fm search cleanup, the status cache, recovery, sign out, demoted and deleted admins |
| `15-radio` | 86 | Group L: song, album, artist and outside-seed radio across Last.fm, YouTube Music, ListenBrainz and Sounds alike (stubbed octo-sonic), bans, learning, weights, `/api/admin/sonic`, `/api/admin/radio-outcomes` |
| `16-lists` | 68 | Groups G and Q: `getTopSongs`, `getArtistTopSongs`, `getTopChart`, Popular right now, Made for you (New Releases, Rediscover, Deep Cuts), the explicit filter |
| `17-covers-explicit` | 74 | Groups P and C: the covers upgrade run, explicit or clean outside songs and albums with their own ids, the explicit backfill preview and refusals, Try it with the advisory |
| `18-dashboard` | 18 | Group N: the activity view, the Afterglow palette, the Library actions history and Recently removed reads |
| `19-health` | 115 | Group R: Library health over 8 planted songs: report, previews, fixes, lookups, stop, undo |
| `20-library-edits` | 153 | Group F: `libraryAction` edits (retag, joinAlbum, cover, lookup, undo), remove to the trash with its `.lrc`, put back, a second copy, `getLibraryActions`, `getLibraryTrash`, the dashboard's trash and history |
| `21-static` | 31 | The 2026.10.05 dashboard files with the admin security headers |
| `90-mutations` | 56 | Run last: star/unstar/rating/scrobble/now playing/reportPlayback, a playlist create-update-delete round trip, a settings patch and re-read, the side-effect admin POSTs (caches, notifications, update check, Last.fm connect flow, job cancel/resume/undo, lyrics choices), sign-out |
| `96-covers-explicit-writes` | 47 | Group P: cover replace and explicit marks written into the library files, checked with `ffprobe` and Navidrome, and undone |
| `99-signed-out-writes` | 73 | Group J: every admin write signed out (written by `tools/admin_routes.py`); runs last, the restart last of all |
| **Total** | **1728** | |

## Recordings

`record` writes one `<corpus-file>.json` per corpus file plus `meta.json` (target, image label,
captured variables). Each entry keeps the request (method, target, headers, body), the status,
**all response headers in wire order** (repeats included), and the body: text bodies up to
64 KB verbatim, larger ones and binary bodies (audio, images, compressed static files) as
size + SHA-256. Raw values are stored; normalization happens only at diff time.

## Normalization

Applied to both sides before comparing:

| What | How |
|---|---|
| `Date`, `Connection`, `Keep-Alive` headers | dropped |
| `Content-Length`, `Transfer-Encoding` | dropped in structural mode; compared in bytes mode unless the body length moved with normalized values |
| The target's `host:port` | `{{host}}` (it appears in Octo's messages and stream URLs) |
| Volatile captures (song/user/playlist ids, JWT, session cookie, salts) | `{{var}}`, raw and percent-encoded |
| ProblemDetails `traceId` | `00-TRACE-00` |
| JWTs anywhere | `JWT` |
| ISO timestamps dated 2025 or later (made by the run; fixture dates are 2018-2021) | `TS(<sep><.f if it had a fraction><zone form>)`, keeping the shape but not the digit count (Go trims trailing zeros, so the count varies) |
| HTTP dates dated 2025 or later | `HTTP-DATE` |
| `Last-Modified` on static files (build time) | `BUILD-TIME` ([`normalize.json`](normalize.json)) |
| `roles` arrays / `<roles>` runs (Navidrome builds them from Go maps) | sorted |
| .NET ticks of a run-time instant (18 digits, 2025 on: activity ids, journal keys, state files) | `600000000000000000` ([`normalize.json`](normalize.json)) |
| The trash's day folder `.octo-trash/<yyyy-MM-dd>/` | `.octo-trash/DAY/` |
| Navidrome's image hash on a playlist's cover id (`pl-<id>_<16 hex>`, added once it made the image) | `pl-<id>` |
| Radio outcome multipliers (they fade with each play's age) | 4 decimals (`15-radio/outcomes-after-*`) |
| Requests marked `unordered` | arrays (JSON) and child elements (XML) sorted, recursively |
| `ignoreKeys` (per request, or global in `normalize.json`) | members/attributes removed |

More rules go in `normalize.json` (`rules`: `requests` glob, `where` = `body` or
`header:<lower-case name>`, `regex`, `replace`).

**Structural mode** (default) parses JSON and XML and compares values: key and attribute order,
whitespace between elements and number spelling (`1` vs `1.0`) are not compared; headers are
compared as a sorted multiset. **Bytes mode** compares the normalized body text exactly
(escaping, key order, XML indentation and declaration, attribute order) and the headers in
wire order, so it catches what a client parsing strictly would see. Requests marked
`unordered` are compared structurally in both modes. Binary bodies are compared by hash in
both.

## Allowlist

[`allowlist.json`](allowlist.json) explains diffs that are deliberate. Each entry must quote
text that appears in `docs/rust-migration/known-diffs.md`, or `diff` refuses to run:

```json
[
  {"id": "server-header",
   "requests": ["*"],
   "aspects": ["header:server"],
   "knownDiff": "Server header",
   "reason": "axum sends no Server header; Kestrel sent `Server: Kestrel`."}
]
```

`requests` and `aspects` are globs. Aspects are `status`, `header:<lower-case name>`,
`header-order`, `body` and `transport`. A request whose every differing aspect is allowed
counts as explained; the report lists how often each entry was used.

## Signing in

Since 2026.10.05 nothing under `/api/admin` answers without a dashboard sign-in (`AdminSignInGate`),
so the corpus signs in for real. `00-setup` signs in through `POST /api/admin/browse/auth` with the
Navidrome admin (the `admin_session` cookie), reads the recovery code from the container
(`exec cat /app/config/admin-recovery-code`) and signs in with it (`recovery_session`). A request
with `"session": "admin"` or `"recovery"` then carries `Cookie: octo_browse=...`, like a browser,
only on `/api/admin` paths and only when it sets no cookie or browse token of its own. Writes also
need `X-Octo-Admin: 1` (the request guard). A build without a recovery code skips the recovery
requests, which shows as a diff.

The signed-out answers are compared on purpose:
[`corpus/11-signin.json`](corpus/11-signin.json) asks every `/api/admin` GET signed out and
signed in, unknown admin paths, bogus cookies and tokens, the recovery session's limits, the
Octo app's Subsonic sign-in on the `[AdminAppCall]` routes (admin, token, listener, wrong
password), and the second Octo. [`corpus/99-signed-out-writes.json`](corpus/99-signed-out-writes.json)
sends every admin write signed out; a build without the gate would carry them out, so it runs
last and `POST /api/admin/restart` is the very last request. Both files are written by
[`tools/admin_routes.py`](tools/admin_routes.py) from the C# controllers: `admin_routes.py write`
after a route is added, and `admin_routes.py check` fails while a route is missing.

## The second Octo

`octo-alt` is the same image with `Admin__SignIn=off` and the dashboard on a port of its own
(`Admin__Port=8081`), its own config volume, the same library, Navidrome and stubs. A request
with `"server": "alt"` goes to its main port (`PARITY_PORT + 1`, which answers 404 for `/admin`
and `/api/admin`), `"server": "alt-admin"` to its admin port (`PARITY_PORT + 2`, 404 for
everything else). It also has no Spotify app and a redirect Spotify refuses, for the import
page's "not set up" answers.

## Determinism rule

C# against C# must give 0 diffs on the whole corpus, in both modes, and Rust against Rust too.
The C# image is fixed, so only its existing settings can hold it still:

- **Timers off or out of reach.** Radio, stations and mixes are off in the env; `15-radio` and
  `16-lists` switch radio, Popular right now and New Releases on through the settings API and
  back off. The trickle fetches nothing (`Imports__SongsPerHour=0`) and lists are not read
  again on a timer (`Imports__RefreshHours=0`). Sounds alike runs with no pause between songs,
  and its first look (2 minutes after start) is waited for. Library actions are switched on and
  off at run time by `19-health` and `20-library-edits`, with their playlists off.
- **Background work is waited for, never raced.** Every job a request starts (Find songs, a
  download, a health run, a cover or explicit run, a Spotify read) is followed by an `until` on
  its end state before anything reads what it changed. Navidrome rescans are waited for the same
  way. Slow stub answers (`delayMs`, `times`) make the "still running" refusals certain.
- **Random values are captured or ignored.** Run ids, search ids, OAuth state and PKCE values
  are captured (volatile). Orders C# itself does not keep are compared `unordered` or
  `anyKeyOrder`.
- **Clocks.** Run-time timestamps and .NET ticks are normalized; plays are scrobbled with 2020
  times; New Releases' stub dates are relative to today (`"template": true`).
- **Stub data is unique per corpus file.** Every file uses its own Deezer id range and names, so
  one file's rows never answer another's calls (the first match in file-name order wins across
  all mapping files).

What still cannot be made repeatable is left out, with the reason:

| Left out | Why |
| --- | --- |
| Station contents, station learning, station covers, `radio/stream` playback | Unseeded `new Random()` weighted draw (`LastFmRadioRecommendationService.cs:545`) and `Random.Shared` tune-in (`RadioTuneInSelector.cs:16`) |
| Library neighbours in song radio (#78) and Sounds alike as a radio source | Unseeded shuffle (`LastFmRadioTrackResolver.cs:147`) and Navidrome's `getRandomSongs`; the fixture songs are also filler-length (under 45 s) |
| The order of Made for you lists | Shuffled with a seed that changes daily (`ForYouLists.cs:179`); Deep Cuts is compared unordered |
| The trickle's pacing, `nextUtc`, running and progress | Wall-clock pacing (`TrickleWorker.cs:370-409`); `SongsPerHour=0` keeps it off |
| A pending Spotify sign-in lapsing, token renewal, 429 and 5xx waits | 15 minute and real retry waits |
| Opus transcodes | ffmpeg's Ogg muxer picks a random stream serial, so the bytes differ every run |
| Transcodes with `estimateContentLength` whose estimate is short | Where the body is cut depends on pipe timing |
| Activity items while running (download fractions, cover run counts) | Racy by nature; only settled states are read |
| `/api/admin/sonic` in the signed-in sweep of `11-signin` | Its state depends on how fast the run got there; `15-radio` reads it once settled |
| The hourly role re-check of an admin already checked | Its clock is internal to tests (`AdminRoleCheck.cs:30`); a fresh demoted or deleted admin is covered |
| `GET /api/admin/discover-servers` signed in | Scans the network; the sandbox's addresses change every run |
| Stopping a health fix run (a lookup run is covered), the noLength fix | No fix step can be made reliably slow; the upgrade queue ticks every 2 s |
| The cover run's `reason` once a resumed run is done (`17-covers-explicit/covers-resumed-done`) | The bulk match reports its progress through `Progress<string>`, which can post the last "barcodes 3 of 3" after the run cleared it (`CoverUpgrade.cs:509,518`); it then stays |
| The `coverArt` of the playlists an import keeps (`12-imports/nd-*playlist-gym`, `-listener`) | Navidrome gives a new, empty playlist its cover id a moment later, sometimes after the corpus reads it, sometimes not for 15 s |
| The album's `genres` in `20-library-edits/album-without-cafe` | Navidrome sometimes keeps the genre of a song that joined the album and went back (Post-Rock beside Electronic), in either build |
| The planted artist's and albums' covers in `19-health/plant-wait` | Navidrome adds them after its scan reports done, and on a busy machine sometimes not within a minute; the request is there for its captures |

Answers the C# app gives that are timing-dependent but settle (the share switch's 10 minute
recheck, the 10 s signed-out status cache) are read so that the same answer comes out either way.

## Diffs per backlog group

Each request carries the backlog groups it checks (`"backlog"`, default `base`: behavior the
Rust port already had at 2026.10.04). [`groups.json`](groups.json) puts some diffs on a group by
what differs, ahead of the request's own tag: the admin security headers and the gate's `signIn`
answers are J wherever they show, `explicitStatus` is P, `ownedCount` is O, and so on. The diff
ends with a table of requests with an unexplained diff per group (one request can count in
several); `--groups-out FILE` writes the request ids per group as JSON. This table is the port's
worklist ([`docs/rust-port/BACKLOG.md`](../docs/rust-port/BACKLOG.md)).

## The release the Rust build claims

The harness compares with C# 2026.10.05, so `run.sh`, `lxc131.sh` and `box-run.sh` build the
Rust binary with `OCTO_VERSION=2026.10.05` (octo-core's `build.rs` reads it before the `VERSION`
file). Otherwise a branch whose `VERSION` is ahead of 2026.10.05, as the release branch's is,
would differ in every answer that names the release (the update card's `running`, the
User-Agent). `PARITY_OCTO_VERSION` picks another release.

## Intentional diffs

The port does not have to stay one to one: Brandon wants it improved. A deliberate improvement
is an entry in [`intentional-diffs.json`](intentional-diffs.json), reviewed diff by diff:

```json
{"id": "plain-timeout-words",
 "backlog": "base",
 "requests": ["13-downloads/find-*"],
 "aspects": ["body"],
 "lines": ["The request was canceled due to the configured HttpClient.Timeout", "took too long"],
 "reason": "Plain words instead of .NET's exception text.",
 "knownDiff": "Plain error words",
 "reviewed": "Brandon 2026-10-08"}
```

- `requests` names the requests (globs inside one corpus file; never `*`), `aspects` the parts
  (`status`, `body`, `header:<name>`), and `lines` (regexes) what the diff itself says: every
  diff line of the aspect must match one, so a different change on the same answer still fails.
- `reason` says why the Rust answer is better; a change a user or an app can see needs Brandon
  by name in `reviewed`. Each entry also gets a row in `docs/rust-migration/known-diffs.md`.
- An entry without `reviewed` is only proposed: the diff lists it as not reviewed and still
  counts the request as unexplained. The summary lists every intentional entry it applied, so
  none hides.

`allowlist.json` stays for the differences the port carries from 2026.10.04 that are not
improvements (framing, the `Server` header, encoder bytes).

## Facts the baseline settles

From `recordings/csharp/` (these close several **(verify)** items in `endpoints.md`):

- `GET /api/admin/lyrics/library` answers `400` with `{"subsonic-response":{...,"error":{"code":10,"message":"Operation not valid"}}}` on every call: the `busy`/`Busy` name collision is real (endpoints.md 4.9, 6.6).
- `getSimilarSongs*` with radio off relays to `{Url}//rest/...`; Navidrome answers the double slash with its web UI (`200 text/html`), which Octo passes through (3.2, 6.5).
- `radio/stream/<48 chars>` with an unknown token is a bare `404` (GET and HEAD); a shorter token is relayed to Navidrome (3.4).
- `GET /` is the `400` ProblemDetails for `endpoint`; Octo-owned paths (`/favicon.ico`, `/administrator`, `/assets/...`, `/api/admin/<unknown>`) are ProblemDetails `404`s; `HEAD /api/admin/settings` is a `404` (2.2).
- `POST /api/admin/browse/auth` with an empty JSON body is a `400` ValidationProblemDetails; a bad bool in `covers/upgrade/thumb?found=` likewise (2.8).
- The local `stream` relay keeps Navidrome's `206` and `Content-Range` (6.4).
- `X-Octo-Browse-Token` alone does not sign `browse/session` in (cookie only), and a stale `octo_browse` cookie hides a valid header (2.9).

## Not covered

The harness is only as good as its corpus. Known gaps, besides the nondeterministic parts
listed under "Determinism rule":

- **The update helper and restart:** `POST /api/admin/restart` only signed out (last request),
  `POST /api/admin/update` only refused; first-run LAN discovery.
- **Lidarr's side of Find songs and acquisition**, including the stale guid fix (`e3aed24`):
  Lidarr would have to be set up with a root folder and profile ids and stubbed through album
  lookup and releases. Picks that replace a library song (the upgrade queue) are not run.
- **The Find songs list past 300 copies** (57ab90d, 13ec9f9): the answer is over 64 KB, so it
  would be compared by hash only.
- **The audit line** "{who} saved settings: {keys}": it goes to the log only.
- **The 55 shared library health cases** (`docs/library-health-cases.json`) are song rows fed
  straight to the checks; the Rust unit tests read them. `19-health` plants 8 songs that trip
  about 11 of them end to end.
- **Spotify import:** the 250,000 songs per person cap, the 64 MB request limit, the refresh
  token path.
- **Feature switches left at one value:** `DownloadOnPlay`,
  `StorageMode=Cache`, `EnableExternalPlaylists`, notification sinks, Genre normalization.
- **apiKey auth:** Navidrome 0.64.2 does not implement it, so only its failure paths.
- **Images:** binary bodies are compared by SHA-256, so the Rust cover renderer differs from
  ImageSharp's bytes even when it looks the same (allowlisted); tags written by lofty differ
  from TagLib#'s in ways the `ffprobe` dumps show.
- **Transport:** keep-alive behavior, chunk boundaries, HTTP/2, client disconnects.
