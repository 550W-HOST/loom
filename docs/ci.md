# CI

Every push and pull request runs [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml).
The workflow exists because the checks that keep this repository honest — format,
lint, tests, the declared minimum Rust version, and the bb contract artifacts —
were until now run by hand. A code review that ran them manually found two real
defects in the relay, both of the kind a test would have caught. Manual
discipline does not scale to the next one.

Nothing here builds a release, an image or a deployment. That is deliberate:
CI validates, it does not ship. Building and publishing is
[`release.yml`](../.github/workflows/release.yml), which runs on a version tag —
[`releasing.md`](releasing.md).

Both triggers have been observed green: the `push` run
[34666491758](https://github.com/550W-HOST/loom/actions/runs/34666491758) and the
`pull_request` run
[34666503929](https://github.com/550W-HOST/loom/actions/runs/34666503929), each
with all five jobs succeeding. The durations below are those runs; the workflow
has gained the `api-coverage` job and the client change since, and the places
where a measurement no longer describes today's job say so. They also predate the
client being compiled into the server: the `ui` job now uploads `apps/app/dist`
as the `ui-dist` artifact and every Rust job downloads and embeds it, because
`cargo build` does not compile without that bundle — the wall-clock numbers are a
record of the runs that produced them, not of today's job set.

## What runs

| Job | Check name | What it proves |
| --- | --- | --- |
| `checks` | `fmt + clippy + test` | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` |
| `api-coverage` | `API coverage document is current` | `node scripts/check-api-coverage.mjs` re-reads the contract and the source routes and refuses a stale row, or an implemented JSON-body route with no `validate_request*` assertion |
| `msrv` | `MSRV` | the workspace still compiles on the `rust-version` floor in the manifests |
| `contract` | `bb contract is reproducible` | re-exporting bb's contract yields the committed `contracts/bb` byte for byte |
| `ui` | `UI typecheck, tests, bundle and provenance` | `pnpm install --frozen-lockfile`, `pnpm run typecheck`, `pnpm run test`, `pnpm --filter @bb/app run build` builds the bundle every Rust job below compiles into the server and uploads it as the `ui-dist` artifact, `pnpm run check:bundle` holds that build to the committed budget, `pnpm provenance:test` covers patch-ledger negative cases and `BB_SRC=... pnpm run provenance:check` verifies recomputed source/package/contract hashes and the import closure, and `BB_SRC=... pnpm run port-plan:test`/`port-plan:check` verifies the route-level port plan |
| `e2e` | `Browser acceptance` | the Playwright suite in `e2e/` drives the real thing — the binary serving the app it was built with, a worker on the same machine running an ACP stub — in a desktop and a mobile viewport: bootstrap, an unreachable server, a thread that answers and survives a reload, a permission request that blocks the turn until it is answered (allow and deny), automations running and reporting, and a machine going offline and coming back |
| `pi` | `real pi provider (allowed to fail)` | the `#[ignore]`d provider tests against the real `pi` CLI — `provider_e2e` drives the streamed turn, `real_pi` adds the first-turn-plus-cross-run-resume property — skipped unless the runner has a configured `pi` |
| `self-update` | `worker self-update end to end` | the `#[ignore]`d self-update tests: a real worker process, refused by a server that speaks a newer protocol, installing that server's binary over itself, and the reinstalled binary running a real turn |

`cargo clippy` and `cargo test` run with `--locked`, so a build that would need a
lockfile update fails instead of quietly resolving one. `pnpm install
--frozen-lockfile` is the same guarantee for the JavaScript side.

`checks`, `msrv` and `pi` all run with `needs: ui`, so the client's checks are a
single gate ahead of every job that compiles the Rust workspace. The [UI
job](#the-ui-job) explains what they cover.

## Toolchain

`rust-toolchain.toml` is the single source of the version for both CI and a
developer machine. The workflow does not name a toolchain:

```yaml
run: |
  rustup toolchain install
  rustup show active-toolchain
```

A bare `rustup toolchain install` reads the file: it resolves the channel, and
when that toolchain is already present it still installs the components the file
lists (`rustfmt`, `clippy`) if they are missing. That is what keeps `cargo fmt`
and `cargo clippy` available in a job that never asks for them by name, and it is
why "CI and local agree" is a property of the file rather than of two lists that
have to be kept in step.

`RUSTUP_TOOLCHAIN` is not set anywhere, so nothing overrides the channel file
inside the repository.

The channel in that file is `stable`, which is the repository's existing choice,
and CI follows it rather than second-guessing it: a new stable release is
adopted on the next run, exactly as it is adopted locally. The floor that must
not move is `rust-version`, and the `msrv` job below is what enforces it. If
release-day stability is ever preferred over currency, changing the channel to
an exact version (`channel = "1.98.0"`) is the only edit needed — CI picks it up
with no workflow change.

## MSRV

`rust-version` in the workspace manifests claims the floor. The `msrv` job is
what makes that claim testable rather than aspirational.

The job reads the floor out of the manifests instead of hardcoding it:

```bash
cargo metadata --no-deps --format-version 1 --locked \
  | jq -r '[.packages[].rust_version] | unique
           | if length == 1 then .[0]
             else error("crates disagree on rust-version: \(.)") end'
```

so the toolchain under test cannot drift away from `rust-version`. Every crate
inherits `rust-version.workspace = true`, and the job fails if the crates ever
disagree. Compiling is then explicit — `cargo +$VERSION check` — because the
repository's own channel file would otherwise win inside the checkout.

It is `check`, not `build`: the property under test is that the floor can
type-check the workspace. That is what catches a call into a newer std
(`Option::is_none_or` needs 1.82) or a dependency that raised its own floor.

That second failure mode is not hypothetical. The floor was false before this
workflow existed: `loom-worker` depended on `tokio-tungstenite 0.30`, which pulls
`sha1 0.11 -> digest 0.11 -> block-buffer 0.12`. `block-buffer 0.12` declares
`rust-version = "1.85"` and its manifest is edition 2024, which cargo 1.80 cannot
even parse — so `cargo +1.80 check` failed outright, in the workspace and in
every crate that reaches the worker's dependency graph. `axum`'s `ws` feature
already brought `tungstenite 0.29`, so the fix was to stop carrying a second
WebSocket stack: the workspace now pins `tokio-tungstenite = "0.29"` and the
lockfile lost fourteen duplicate packages (`tungstenite`, `sha1`, `digest`,
`block-buffer`, `crypto-common`, `hybrid-array`, `rand`, `rand_core`,
`getrandom`, `chacha20`, `const-oid`, `cpufeatures 0.3.1`, `r-efi` and
`tokio-tungstenite`).
The relay, server, worker, hub and protocol crates now compile on 1.80.

`tungstenite 0.29` carries no advisory: the only one recorded for the crate
(GHSA-9mcr-873m-xcxp) covers `<= 0.20.0`.

The MSRV job goes green the moment the floor stops being true, so raising
`rust-version` is a visible decision rather than a side effect of a dependency
bump.

### The floor is now 1.88

The ACP SDK set it: `agent-client-protocol` 2.0.0 and
`agent-client-protocol-schema` 1.5.0 both declare `rust-version = "1.88"`, and
their manifests are edition 2024, which Cargo cannot parse before 1.85 at all.
Since loom's provider strategy is ACP — every agent is reached through it — the
protocol's floor became loom's. Probe it directly:

```bash
cargo +1.88 check --workspace --all-targets --locked
```

The raise also relaxed the `tokio-tungstenite` pin's *reason*: a manifest using
edition 2024 is now parseable, so the pin survives only to keep one WebSocket
stack in the tree (and the duplicate `sha1`/`rand` versions out), not because
the toolchain cannot read the alternative.

**If you are reading this to downgrade the floor**, note what the 1.80 work
established: the floor is not a number in a manifest, it is a property the MSRV
job verifies by compiling. Lowering it without re-running that job in the
workspace — including the worker's whole dependency graph — reintroduces exactly
the false declaration this job removed.

## Caching

[`Swatinem/rust-cache`](https://github.com/Swatinem/rust-cache) restores the
cargo registry and `target`. Each job passes its own `prefix-key`, so the `msrv`
and `pi` jobs cannot restore a `target` built by a different toolchain or feature
set. A cold run is what the measurements below describe; a warm run skips
compilation almost entirely.

The `ui` job has no cargo artifacts to keep and caches the pnpm store instead,
through `actions/setup-node`'s `cache: pnpm`, which keys it on `pnpm-lock.yaml`.

## Measured duration

Real runs on `ubuntu-24.04` (4 vCPU), push run
[34666491758](https://github.com/550W-HOST/loom/actions/runs/34666491758) and PR
run [34666503929](https://github.com/550W-HOST/loom/actions/runs/34666503929):

| Job | Push | PR |
| --- | --- | --- |
| `UI typecheck, tests, bundle and provenance` | 1 m 00 s | 59 s |
| `fmt + clippy + test` | 1 m 02 s | 1 m 03 s |
| `MSRV` | 23 s | 26 s |
| `bb contract is reproducible` | 27 s | 32 s |
| `real pi provider` (skipped) | 30 s | 14 s |
| `worker self-update end to end` (not in these runs) | — | — |

`ui` is on the critical path now: `checks`, `msrv`, `pi` and `self-update` start
only once it has passed, so a run is roughly `ui` plus the slowest Rust job —
**2 m 09 s** (push) and **2 m 10 s** (PR) end to end, against the 1 m 45 s of the
four-job runs these replace. All six jobs are `success` in both except `pi`,
which skips on a runner with no configured `pi`.

The `self-update` job was added after those two runs, so it has no runner number
here. Measured locally, busy otherwise idle: **20.7 s** for all three tests
(`--ignored --test-threads=1`), of which the acceptance scenario is ~18 s —
downloading the artifact, replacing the binary, and running a provider turn —
and 1.8 s is the deliberate wait proving the disabled switch keeps running. That
run downloaded a 2.4 MB worker-only artifact; the artifact is the one binary
now, so the download is larger by the client it carries and the timing is a
record rather than today's number. It has
no network dependency, so on a runner it is bounded by the cargo build it shares
with the other jobs.

Both were the first runs of this workflow revision, so the pnpm store cache was a
cold miss (`reused 0, downloaded 183`) and the cargo caches were warm from
`main`. Inside the `ui` job:

| Step | Push |
| --- | --- |
| `pnpm install --frozen-lockfile` | 5 s |
| `pnpm typecheck` | 31 s |
| `pnpm test` | 7 s |
| rebuilding the reference bundle (`ui/`) | under 1 s |

That last row is a step that no longer exists: those runs rebuilt the buildless
reference client and required the committed bytes to match. The job now builds
the product app and holds it to its budget, which is a Vite build of a whole
application rather than one file bundled with esbuild, so the row is recorded
history rather than a measurement of today's job. `typecheck` is still the
dominant step — `tsc --noEmit` over every workspace project — and the metadata
checks that follow it are unchanged.

For reference, the individual steps measured cold on a 24-core workstation
constrained to `-j4` to approximate a runner:

| Step | Cold |
| --- | --- |
| `cargo fmt --all -- --check` | 0.3 s |
| `cargo clippy --workspace --all-targets --locked -j4 -- -D warnings` | 16.7 s |
| `cargo test --workspace --locked -j4` | 26.1 s |
| `cargo +1.88 check --workspace --all-targets --locked -j4` | 16.0 s |
| contract re-export (bb fetch + `bun install` + export) | 14.9 s |

The `checks` job is roughly 43 s of real work; the rest of its wall-clock is
checkout, toolchain download and cache restore. Every job has a 20-minute
`timeout-minutes`, which is generous enough that a timeout means something
hangs rather than something is slow.

## The Redis tests

`loom-relay`'s contract suite runs over the in-process, disk and Redis backends;
the Redis cases are skipped unless `LOOM_REDIS_URL` names a reachable server. CI
sets nothing, so they skip and the two other backends still run — the suite is
green with no service, which is the behaviour
[`redis-backend.md`](redis-backend.md) promises.

Confirming the skip needs `--nocapture`, because the message goes to stderr and
the harness captures it:

```bash
cargo test -p loom-relay -- --nocapture
# loom-relay: skipping the Redis backend cases; set LOOM_REDIS_URL …
```

Adding the service is only worth doing if a change touches `RedisBackend`. A
`redis` service container on the `checks` job plus

```yaml
env:
  LOOM_REDIS_URL: redis://127.0.0.1:6379
```

is the whole change; the mechanisms that exist today (`unique_prefix()`, the
unreachable-server skip, `purge`) already make these tests safe to run against a
shared server. It is not enabled here because it would make every push wait on a
service for a backend the default build does not use.

## The contract job

`contracts/bb` is generated, so a stale export is a silent divergence between
loom's public surface and bb's. The job regenerates it and fails on any
difference:

```bash
git diff --exit-code -- contracts/bb
test -z "$(git status --porcelain -- contracts/bb)"
```

Both halves are needed: `git diff` catches modifications to tracked artifacts and
`git status` catches a file the exporter added or removed.

The bb revision is read from `contracts/bb/manifest.json` — both the repository
and the commit — and fetched shallow, so the pin lives in exactly one place and
the job fails on exporter drift, never on unrelated progress in a repository this
fork does not track.

## The UI job

The Rust workspace is not the only thing in this repository that can break
silently. The client is a pnpm workspace of the product app and the pinned bb
packages it builds against — `apps/app` plus `ui/packages/*`, listed in
[`ui-package-sync.md`](ui-package-sync.md) — carrying a strict `tsc --noEmit`, a
vitest suite, and the only build of the bundle the Rust jobs compile into the
server. None of it ran on a push or a pull request before this job:
`thread-view` is a 16k-line projection layer, and the only way to know a change
to it was sound was to run pnpm by hand.

The app is also the compile-time input of every Rust job. `crates/server/build.rs`
walks `apps/app/dist` and embeds it in the binary, so a client that does
not build is not a degraded server — it is a server that does not build at all.
That is what makes this job required rather than advisory, and why it ends by
uploading the bundle as `ui-dist` for the jobs below to download.

The job is the repository's own entry points, in the order a developer runs
them:

```bash
pnpm install --frozen-lockfile
pnpm run typecheck                  # every ui/packages/* and apps/app
pnpm run test                       # the same fan-out
pnpm --filter @bb/app run build     # → apps/app/dist, the input to cargo build
pnpm run check:bundle               # the budget, against that build
```

`typecheck` and `test` are the root scripts rather than an explicit list of
packages, so a project added to the workspace is checked as soon as it joins.
The build is the app's own, and it is the first step of the pipeline: `cargo
build` embeds `apps/app/dist` ([`ui.md`](ui.md)), so the artifact this job
uploads is what `checks`, `msrv`, `pi` and `self-update` download before they
compile. The release workflow does the same with its own `ui` job
([`releasing.md`](releasing.md)).

### The bundle budget

`pnpm run check:bundle` reads the `bundle-stats.json` the build just wrote and
holds it to `apps/app/bundle-budget.json`: a boot payload that grew past its
allowance, a heavy package that reached the boot path again, or an on-demand
package that leaked out of its dynamic-import gate into another chunk's static
closure. Every measured lazy route gets the same ratchet — the JavaScript between
"app shell painted" and "route content painted" — so a route that quietly doubles
its payload fails here instead of in a browser on a phone.

The budget is a ratchet, not a target: making the client bigger is allowed, but
only by editing `bundle-budget.json` in the same commit, which is the reviewable
form of that decision.

pnpm's version is not written in the workflow. The `ui` job sets up pnpm with no
`version:` input, and the action reads `packageManager` from the root
`package.json` — the same shape as the bb revision, which the `contract` job
reads from the manifest rather than repeating. Node is pinned to the `22` major:
it only has to run `tsc`, `vitest`, Vite and the checker scripts, each of which
the lockfile fixes. The pnpm store is cached, keyed on `pnpm-lock.yaml`.

### What else the job checks

Two generated-metadata checks ride along, because they are the same kind of claim
as the bundle — a machine-readable statement that nothing else verifies:

```bash
BB_SRC=… pnpm run provenance:test && BB_SRC=… pnpm run provenance:check
BB_SRC=… pnpm run port-plan:test  && BB_SRC=… pnpm run port-plan:check
```

`provenance:check` recomputes the app tree, every package it builds against, the
contract manifest and the pinned commit's own `HEAD`, and fails on unregistered,
duplicate, overlapping, glob, hash, add/delete/rename, mode or symlink drift;
`provenance:test` covers those negative cases first. `port-plan:check` verifies
the route-level port plan. Both fetch the pinned checkout shallow from the
manifest's own commit, so a failure here means drift in this repository and never
progress in bb.

The last step applies the `contract` job's check to the client tree:

```bash
git diff --exit-code -- ui/ apps/app/
test -z "$(git status --porcelain -- ui/ apps/app/)"
```

Both halves are needed: `git diff` catches a modified checked-in file and
`git status` catches one the build added or removed. The app's `dist/` is
gitignored, so a successful build leaves the tree clean and any difference is a
real one.

`checks`, `msrv` and `pi` declare `needs: ui`, so the client's checks are a
single gate ahead of every Rust job. It costs wall-clock — a run is roughly the
`ui` job plus the slowest Rust job — and the durations below are measured with
that in place.

## The browser acceptance job

Every other job in this file runs *parts* of the product: the Rust workspace
through its own crates, the client through `vitest` and `jsdom`. None of them can
say whether a person with a browser can use the thing, which is how the
acceptance of the porting issues was done by hand — a browser driven by whoever
was working, screenshots pasted into the issue, and no way to notice a
regression the next week.

The `e2e` job is that acceptance, runnable. It needs `ui` for the same reason
every other Rust job does: the app is compiled into the binary, so the suite
downloads `ui-dist`, builds `loom`, and then *is* the deployment — `loom server`
serving the app it was built with, and `loom worker` enrolling against it from
the same machine.

```bash
cargo build -p loom
pnpm --filter @loom/e2e exec playwright install --with-deps chromium
pnpm --filter @loom/e2e run typecheck
pnpm --filter @loom/e2e test
```

`e2e/helpers/stack.ts` starts the stack once per run — a temporary data
directory, a free-ish port, an ACP stub — and `stopStack` returns the machine to
the state it was borrowed in. The stub answers a prompt with a fixed reply and
asks for permission when the prompt mentions it, so a turn is deterministic and
needs no credentials or model; `LOOM_E2E_PROVIDER_CMD` swaps in a real agent
when a person wants one. The worker is started with a state file, exactly as
[`process-model.md`](process-model.md) has deployments start it, because
without one every restart enrolls as a *new* machine and the machine list is
where that accumulates.

Two projects run the same specs: `desktop` at 1440×900 and `mobile` at a phone's
viewport. The second one is not decoration — the shell's sidebar is a drawer
there, and the first version of the suite found the difference immediately.

The suite is deliberately small and grows with the porting stages: it covers the
surfaces that exist (shell, threads, permissions, automations, machines) and not
the ones still being ported (files, terminal, settings sections). A failure is
reported with the trace, a screenshot, and the stack's own logs; the traces and
screenshots are uploaded when the job fails.

## The `pi` job

`the_real_pi_process_streams_through_the_bridge` is `#[ignore]`d because it runs
the real `pi` CLI. It is the only automated coverage of the bridge the worker
actually uses in production, so leaving it manual was the larger risk, but three
things follow from what it is:

- **Its own job, so it is not in the required set.** `pi` is upstream
  (`@earendil-works/pi-coding-agent`); a release of it is not a reason to block a
  merge into this repository.
- **`continue-on-error: true`,** so a red `pi` job does not fail the workflow
  run. It shows in the checks list as failed, which is where a human should
  notice it, without gating anything.
- **Skipped unless `pi` is configured.** This is the part that is not obvious.
  The test's doc comment says it passes on a machine with no credentials; what
  it actually requires is a *configured* `pi`, and those are different things.
  A runner that has the CLI installed but no `$HOME/.pi/agent` gets no frames at
  all: `pi --mode rpc --no-session </dev/null` exits immediately, emitting
  nothing, and the embedded `pi-acp` integration then fails on
  `should report started` in under a second — deterministically, on every run,
  with no credentials involved.

  A job that is red every time is worse than no job: it trains people to ignore
  the checks list. So the job probes the behaviour the test needs and skips with
  a `::notice::` when it is absent, which is the state of a stock runner. Red
  then means something really changed.

  The probe captures the Pi RPC child invocation used internally by
  `pi-acp`: `pi --mode rpc --no-session </dev/null`, and asks whether it
  produced any frames. `</dev/null` makes pi exit on EOF so it returns in a few
  seconds either way, and the output is captured whole rather than piped, so
  `pipefail` cannot mistake a `SIGPIPE` for "no frames". Verified both ways
  locally: clean `$HOME` → `configured=false` in 0.4 s, configured `$HOME` →
  `configured=true` in 2.3 s and the test passes.

  To enable real coverage, give the job a configured `pi` — a `$HOME/.pi/agent`
  with the models and credentials you want the bridge exercised against. Without
  that, the test stays a local check; the command is in
  [Reproducing CI locally](#reproducing-ci-locally).

The CLI is installed from npm (`npm install -g @earendil-works/pi-coding-agent@0.85.1`,
about 3 s) rather than expected on the runner, and the version is pinned so a new
upstream release cannot turn the job red without a commit here.

## The `self-update` job

`crates/loom/tests/self_update.rs` covers the acceptance scenario from
[`upgrades.md`](upgrades.md) — *protocol mismatch → update → reconnect → the run
is handled correctly* — and like the `pi` job it is `#[ignore]`d because it
executes real processes. Unlike the `pi` job it needs **nothing external**: the
artifact the fake server serves is the binary this repository just built
(`CARGO_BIN_EXE_loom`), driven in its worker role, so the job is allowed to fail
and is a candidate for the required set.

What it does, in one process:

1. starts a fake server that answers `welcome` with `PROTOCOL_VERSION + 1` and
exposes the two `/install/*` routes;
2. runs the real binary as a worker against it — the worker refuses, fetches the
artifact, verifies its SHA-256, `rename`s it over its own executable, and exits
**0**;
3. asserts the file on disk is now the served bytes (whole-file comparison, not
a marker), executable, with the digest recorded;
4. starts the **installed file** against a **real** control plane, dispatches a
real provider turn through the relay, and asserts the thread reaches `idle` with
the expected streamed output in the replayable log.

The two companion tests cover the conditional request (a second run against an
unchanged artifact sends `If-None-Match` and gets a `304`, and still restarts)
and the operator switch (`--no-auto-update` never fetches, never touches the
binary, and keeps running).

Two properties of the harness are worth knowing before editing it:

- **The stale binary is a real ELF with a marker appended.** Self-update replaces
  the running executable, so the test cannot point the worker at an arbitrary
  path; it starts the real binary with bytes appended (which the loader ignores)
  and then compares whole files. A test that only checked for a marker would pass
  even if the install wrote nothing.
- **The worker is spawned by path, never by `PATH` lookup.** `UpdateConfig`
  installs over `std::env::current_exe()`, so the path the test spawns *is* the
  path under test; a lookup would silently exercise a different file.

Run it locally with the same commands the job uses:

```bash
cargo build -p loom --locked
cargo test -p loom --test self_update --locked -- --ignored --test-threads=1
```

`--test-threads=1` is not required by the logic (each test uses its own
`tempfile` directory) but keeps the two real-process tests from competing for
CPU, which makes a failure's timing legible.

## Branch protection

Protect `main` and require these six checks:

| Required | Reason |
| --- | --- |
| `fmt + clippy + test` | format, lint and the full test suite |
| `MSRV` | the declared floor keeps compiling |
| `bb contract is reproducible` | the committed contract is what the exporter produces |
| `UI typecheck, tests, bundle and provenance` | the client type-checks and passes its tests, the product app builds the bundle every Rust job embeds and holds it to its bundle budget, and the source/package/contract provenance and the port plan match their manifests |
| `worker self-update end to end` | a real worker follows a newer-protocol server: fetch, verify, install over itself, restart, run a turn |
| `Browser acceptance` | a real browser drives the real stack — server, worker, an approval that blocks its turn — in a desktop and a phone viewport |

The five jobs that declare `needs: ui` — `checks`, `msrv`, `pi`, `self-update`
and `e2e` — are skipped when `ui` fails, which is not a hole: `ui` is itself
required, so a red one blocks the merge and the skipped jobs only save runner
time.

`worker self-update end to end` is the one `#[ignore]`d, process-executing job
that **is** required: it depends on nothing outside this repository (the artifact
it installs is the binary the job just built) and it is the only automated
evidence for the upgrade path this repository promises. The `pi` job is not
required for the opposite reason.

Do **not** require `real pi provider (allowed to fail)`. It is `continue-on-error`
by design, an upstream CLI must not gate this repository, and on a runner with an
unconfigured `pi` it skips rather than reporting. Revisit this if the job is ever
given a configured `pi`; it would then be worth requiring, since it is the only
coverage of the production bridge.

On GitHub: *Settings → Branches → Branch protection rules → `main`*, enable
*Require status checks to pass before merging*, then select the six above. Two
further settings are recommended and independent of this workflow:

- *Require branches to be up to date before merging* — otherwise a pull request
  can pass against a base that has since moved, which is exactly the case the
  relay's reader and reconnect defects came from.
- *Require a pull request before merging* — direct pushes to `main` bypass every
  check here.

Neither setting is enforced by the workflow file, and neither is enabled by this
change set; they are repository settings a maintainer applies.

## Reproducing CI locally

Every cargo command below needs the app's bundle in the tree, because the server
embeds it: CI downloads the `ui-dist` artifact first, and locally the equivalent
is `pnpm --filter @bb/app run build` (the `ui` job's recipe further down) before
the first cargo command.

The `checks` job is three commands:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The `msrv` job, once the floor is installed
(`rustup toolchain install 1.88 --profile minimal`):

```bash
cargo +1.88 check --workspace --all-targets --locked
```

The `contract` job needs `bun` and a bb checkout at the pinned revision:

```bash
git clone --filter=blob:none https://github.com/get-bb/bb.git /tmp/bb
git -C /tmp/bb checkout "$(jq -r .source.commit contracts/bb/manifest.json)"
BB_SRC=/tmp/bb scripts/export-bb-contract.sh
git diff --exit-code -- contracts/bb
```

The `ui` job needs Node and pnpm (`corepack enable` picks up the pinned pnpm
from `packageManager`), then runs the app's build and every metadata check:

```bash
pnpm install --frozen-lockfile
pnpm run typecheck
pnpm run test
pnpm --filter @bb/app run build     # the bundle the Rust jobs compile in
pnpm run check:bundle
BB_SRC=/tmp/bb pnpm run provenance:test && BB_SRC=/tmp/bb pnpm run provenance:check
BB_SRC=/tmp/bb pnpm run port-plan:test  && BB_SRC=/tmp/bb pnpm run port-plan:check
git diff --exit-code -- ui/ apps/app/
test -z "$(git status --porcelain -- ui/ apps/app/)"
```

Where CI uploads `apps/app/dist` as the `ui-dist` artifact, a local run just
leaves it in place — the four jobs that need it download it into the same path
before they call cargo.

`/tmp/bb` is the pinned checkout the `contract` job's recipe above produces —
provenance and the port plan read their commit from `ui/provenance.json`, which
is the same bb revision.

The `e2e` job needs the bundle and a Chromium of its own, then runs the suite
against a stack it starts itself:

```bash
pnpm --filter @bb/app run build
cargo build -p loom
pnpm --filter @loom/e2e exec playwright install chromium
pnpm --filter @loom/e2e test                # add --project=desktop to run one
LOOM_E2E_KEEP=1 pnpm --filter @loom/e2e test  # keep the run's data and logs
```

A leftover stack from a killed run is refused rather than adopted — set
`LOOM_E2E_PORT` to run beside it. `LOOM_E2E_PROVIDER_CMD` points the worker at a
real agent instead of the stub.

The `pi` job needs `pi` installed *and configured*; the test is skipped in CI
without the latter, so this is where it actually runs:

```bash
npm install -g @earendil-works/pi-coding-agent@0.85.1
cargo test -p loom-worker --test provider_e2e -- --ignored
```

These are the commands the workflow runs, not equivalents of them.
