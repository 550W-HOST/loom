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
with all five jobs succeeding. The durations below are those runs.

## What runs

| Job | Check name | What it proves |
| --- | --- | --- |
| `checks` | `fmt + clippy + test` | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` |
| `msrv` | `MSRV` | the workspace still compiles on the `rust-version` floor in the manifests |
| `contract` | `bb contract is reproducible` | re-exporting bb's contract yields the committed `contracts/bb` byte for byte |
| `ui` | `UI typecheck, tests and bundle` | `pnpm install --frozen-lockfile`, `pnpm run typecheck` and `pnpm run test` over `ui/` and its seven packages, then a fresh build reproduces the committed `ui/app.js` |
| `pi` | `real pi provider (allowed to fail)` | the `#[ignore]`d provider tests against the real `pi` CLI — `provider_e2e` drives the streamed turn, `real_pi` adds the first-turn-plus-cross-run-resume property — skipped unless the runner has a configured `pi` |
| `self-update` | `daemon self-update end to end` | the `#[ignore]`d self-update tests: a real daemon process, refused by a server that speaks a newer protocol, installing that server's binary over itself, and the reinstalled binary running a real turn |

`cargo clippy` and `cargo test` run with `--locked`, so a build that would need a
lockfile update fails instead of quietly resolving one. `pnpm install
--frozen-lockfile` is the same guarantee for the JavaScript side.

`checks`, `msrv` and `pi` all run with `needs: ui`. Every one of them compiles
`loom-server` — `pi` through `loom-daemon`, which depends on it — so each would
otherwise embed a stale bundle and pass. The [UI job](#the-ui-job) explains why.

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
workflow existed: `loom-daemon` depended on `tokio-tungstenite 0.30`, which pulls
`sha1 0.11 -> digest 0.11 -> block-buffer 0.12`. `block-buffer 0.12` declares
`rust-version = "1.85"` and its manifest is edition 2024, which cargo 1.80 cannot
even parse — so `cargo +1.80 check` failed outright, in the workspace and in
every crate that reaches the daemon's dependency graph. `axum`'s `ws` feature
already brought `tungstenite 0.29`, so the fix was to stop carrying a second
WebSocket stack: the workspace now pins `tokio-tungstenite = "0.29"` and the
lockfile lost fourteen duplicate packages (`tungstenite`, `sha1`, `digest`,
`block-buffer`, `crypto-common`, `hybrid-array`, `rand`, `rand_core`,
`getrandom`, `chacha20`, `const-oid`, `cpufeatures 0.3.1`, `r-efi` and
`tokio-tungstenite`).
The relay, server, daemon, hub and protocol crates now compile on 1.80.

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
workspace — including the daemon's whole dependency graph — reintroduces exactly
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
| `UI typecheck, tests and bundle` | 1 m 00 s | 59 s |
| `fmt + clippy + test` | 1 m 02 s | 1 m 03 s |
| `MSRV` | 23 s | 26 s |
| `bb contract is reproducible` | 27 s | 32 s |
| `real pi provider` (skipped) | 30 s | 14 s |
| `daemon self-update end to end` (not in these runs) | — | — |

`ui` is on the critical path now: `checks`, `msrv`, `pi` and `self-update` start
only once it has passed, so a run is roughly `ui` plus the slowest Rust job —
**2 m 09 s** (push) and **2 m 10 s** (PR) end to end, against the 1 m 45 s of the
four-job runs these replace. All six jobs are `success` in both except `pi`,
which skips on a runner with no configured `pi`.

The `self-update` job was added after those two runs, so it has no runner number
here. Measured locally, busy otherwise idle: **20.7 s** for all three tests
(`--ignored --test-threads=1`), of which the acceptance scenario is ~18 s —
downloading ~2.4 MB, replacing the binary, and running a provider turn — and
1.8 s is the deliberate wait proving the disabled switch keeps running. It has
no network dependency, so on a runner it is bounded by the cargo build it shares
with the other jobs.

Both were the first runs of this workflow revision, so the pnpm store cache was a
cold miss (`reused 0, downloaded 183`) and the cargo caches were warm from
`main`. Inside the `ui` job:

| Step | Push |
| --- | --- |
| `pnpm install --frozen-lockfile` | 5 s |
| `pnpm typecheck` (8 projects) | 31 s |
| `pnpm test` (18 tests) | 7 s |
| `pnpm --filter @loom/ui run build` | under 1 s |
| the committed-bundle check | under 1 s |

`typecheck` dominates — it is `tsc --noEmit` over eight projects — while esbuild
rebuilds the 1.19 MB bundle in well under a second, which is why the artifact
check costs almost nothing next to the install that precedes it.

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
silently. `ui/` is a pnpm workspace of nine projects — `@loom/ui` and the seven
projection packages ported from bb, listed in
[`ui-package-sync.md`](ui-package-sync.md) — carrying a strict `tsc --noEmit`
and a vitest suite of 18 tests. None of it ran on a push or a pull request
before this job: `thread-view` is a 16k-line projection layer, and the only way
to know a change to it was sound was to run pnpm by hand.

The job is the repository's own entry points, in the order a developer runs
them:

```bash
pnpm install --frozen-lockfile
pnpm run typecheck   # the root script: @loom/ui and every ui/packages/*
pnpm run test        # the same fan-out; 18 tests today
pnpm --filter @loom/ui run build
```

`typecheck` and `test` are the root scripts rather than an explicit list of
packages, so a package added to `ui/packages/` is checked as soon as it joins
the workspace. `build` is not the root script: each package's own `build` is a
second `tsc --noEmit` over what the typecheck step just checked, and the only
artifact that has to be produced is `@loom/ui`'s.

pnpm's version is not written in the workflow. The `ui` job sets up pnpm with no
`version:` input, and the action reads `packageManager` from the root
`package.json` — the same shape as the bb revision, which the `contract` job
reads from the manifest rather than repeating. Node is pinned to the `22` major:
it only has to run `tsc`, `vitest` and esbuild, each of which the lockfile
fixes. The pnpm store is cached, keyed on `pnpm-lock.yaml`.

### Why the bundle is committed

`ui/app.js` is generated **and committed on purpose**, because the server embeds
it at compile time:

```rust
const INDEX_HTML: &[u8] = include_bytes!("../../../ui/index.html");
const APP_JS: &[u8] = include_bytes!("../../../ui/app.js");
const STYLE_CSS: &[u8] = include_bytes!("../../../ui/style.css");
```

A `cargo build` therefore needs no JavaScript toolchain and no `LOOM_UI_DIR`
set: the binary carries a working UI with zero setup, which is the property
[`ui.md`](ui.md) describes. `ui/scripts/build.mjs` builds `ui/dist/app.js` with
esbuild and copies it back over `ui/app.js`; `ui/index.html` and `ui/style.css`
are copied *into* `dist/`, never written back, so `app.js` — 32,774 lines,
1.19 MB — is the one file that can fall behind.

That drift is invisible to every other check. `cargo test` compiles and passes
against whatever `ui/app.js` contains, while `vitest` and `tsc` exercise
`ui/src`, and nothing compares the two trees. Change `ui/src`, forget
`pnpm --filter @loom/ui run build`, and every job is green while the server
serves a UI no current source describes — a defect visible only in a browser.
(`LOOM_UI_DIR` means a deployment can serve a different bundle again, which is
another reason the check is about the committed bytes and not about what some
running server happens to return.)

So the last two steps are the `contract` job's check applied to `ui/`:

```bash
git diff --exit-code -- ui/
test -z "$(git status --porcelain -- ui/)"
```

Both halves matter for the same reason they do for `contracts/bb`: `git diff`
catches a modified `ui/app.js`, and `git status` catches one the build added or
removed. The build's output under `ui/dist/` is gitignored, so a successful
build leaves the tree clean and any difference is a real one.

### The command that reproduces the bundle

The bundle only reproduces from inside `ui/`. esbuild writes one module path
comment per bundled module, relative to the working directory it was given, so
`pnpm --filter @loom/ui run build` — which runs the script with `ui/` as its cwd,
exactly as the root `build` script does — is the invocation that produces the
committed bytes. Running `node ui/scripts/build.mjs` from the repository root
rewrites 177 of those comments and changes no behaviour at all, and the job then
fails on the diff while the UI is in fact correct. Worth knowing before
debugging the first such failure: the check is about reproducibility, and the fix
is to run the command the job runs. Making the build independent of the caller's
working directory would change the committed bundle, which is a `ui/` change
rather than a CI one.

### Why the Rust jobs wait for it

`include_bytes!` reads the tree, so every job that compiles `loom-server`
compiles the UI whether or not the change touched it. `needs: ui` makes a stale
bundle stop the workflow at the job that can explain it, instead of letting the
other four run to a green result against a UI no source describes.

It costs wall-clock: a run is now roughly the `ui` job plus the slowest Rust
job, since those three no longer start until `ui` is done. That is the trade —
a trustworthy signal over total duration — and the durations below are measured
with it in place.

Uploading the built bundle as an artifact from `ui` and downloading it in the
Rust jobs would couple them more tightly, but it is not needed while the bundle
is committed: the Rust jobs read the same bytes the diff check just approved.
Replacing `include_bytes!` with a runtime load is the other real alternative —
the `LOOM_UI_DIR` source already exists — but it trades a compile-time guarantee
for a deployment-time one, and it is a separate decision from adding CI.

## The `pi` job

`the_real_pi_process_streams_through_the_bridge` is `#[ignore]`d because it runs
the real `pi` CLI. It is the only automated coverage of the bridge the daemon
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

`crates/daemon/tests/self_update.rs` covers the acceptance scenario from
[`upgrades.md`](upgrades.md) — *protocol mismatch → update → reconnect → the run
is handled correctly* — and like the `pi` job it is `#[ignore]`d because it
executes real processes. Unlike the `pi` job it needs **nothing external**: the
artifact the fake server serves is the `loom-daemon` this repository just built
(`CARGO_BIN_EXE_loom-daemon`), so the job is allowed to fail and is a candidate
for the required set.

What it does, in one process:

1. starts a fake server that answers `welcome` with `PROTOCOL_VERSION + 1` and
exposes the two `/install/*` routes;
2. runs the real daemon binary against it — the daemon refuses, fetches the
artifact, verifies its SHA-256, `rename`s it over its own executable, and exits
**0**;
3. asserts the file on disk is now the served bytes (whole-file comparison, not
a marker), executable, with the digest recorded;
4. starts the **installed file** against a **real** `loom-server`, dispatches a
real provider turn through the relay, and asserts the thread reaches `idle` with
the expected streamed output in the replayable log.

The two companion tests cover the conditional request (a second run against an
unchanged artifact sends `If-None-Match` and gets a `304`, and still restarts)
and the operator switch (`--no-auto-update` never fetches, never touches the
binary, and keeps running).

Two properties of the harness are worth knowing before editing it:

- **The stale binary is a real ELF with a marker appended.** Self-update replaces
  the running executable, so the test cannot point the daemon at an arbitrary
  path; it starts the real binary with bytes appended (which the loader ignores)
  and then compares whole files. A test that only checked for a marker would pass
  even if the install wrote nothing.
- **The daemon is spawned by path, never by `PATH` lookup.** `UpdateConfig`
  installs over `std::env::current_exe()`, so the path the test spawns *is* the
  path under test; a lookup would silently exercise a different file.

Run it locally with the same commands the job uses:

```bash
cargo build -p loom-daemon -p loom-server --locked
cargo test -p loom-daemon --test self_update --locked -- --ignored --test-threads=1
```

`--test-threads=1` is not required by the logic (each test uses its own
`tempfile` directory) but keeps the two real-process tests from competing for
CPU, which makes a failure's timing legible.

## Branch protection

Protect `main` and require these five checks:

| Required | Reason |
| --- | --- |
| `fmt + clippy + test` | format, lint and the full test suite |
| `MSRV` | the declared floor keeps compiling |
| `bb contract is reproducible` | the committed contract is what the exporter produces |
| `UI typecheck, tests and bundle` | the UI packages type-check and pass their tests, and the committed bundle is what the build produces |
| `daemon self-update end to end` | a real daemon follows a newer-protocol server: fetch, verify, install over itself, restart, run a turn |

The last four jobs of the workflow are skipped when `ui` fails, which is not a
hole: `ui` is itself required, so a red one blocks the merge and the skipped
jobs only save runner time.

`daemon self-update end to end` is the one `#[ignore]`d, process-executing job
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
*Require status checks to pass before merging*, then select the three above. Two
further settings are recommended and independent of this workflow:

- *Require branches to be up to date before merging* — otherwise a pull request
  can pass against a base that has since moved, which is exactly the case the
  relay's reader and reconnect defects came from.
- *Require a pull request before merging* — direct pushes to `main` bypass every
  check here.

Neither setting is enforced by the workflow file, and neither is enabled by this
change set; they are repository settings a maintainer applies.

## Reproducing CI locally

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
from `packageManager`), then runs the four commands above plus the artifact
check:

```bash
pnpm install --frozen-lockfile
pnpm run typecheck
pnpm run test
pnpm --filter @loom/ui run build
git diff --exit-code -- ui/
test -z "$(git status --porcelain -- ui/)"
```

The build has to go through `pnpm --filter`, not `node ui/scripts/build.mjs`
from the repository root — see
[the command that reproduces the bundle](#the-command-that-reproduces-the-bundle).

The `pi` job needs `pi` installed *and configured*; the test is skipped in CI
without the latter, so this is where it actually runs:

```bash
npm install -g @earendil-works/pi-coding-agent@0.85.1
cargo test -p loom-daemon --test provider_e2e -- --ignored
```

These are the commands the workflow runs, not equivalents of them.
