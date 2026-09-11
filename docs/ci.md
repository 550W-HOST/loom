# CI

Every push and pull request runs [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml).
The workflow exists because the checks that keep this repository honest — format,
lint, tests, the declared minimum Rust version, and the bb contract artifacts —
were until now run by hand. A code review that ran them manually found two real
defects in the relay, both of the kind a test would have caught. Manual
discipline does not scale to the next one.

Nothing here builds a release, an image or a deployment. That is deliberate:
CI validates, it does not ship.

## What runs

| Job | Check name | What it proves |
| --- | --- | --- |
| `checks` | `fmt + clippy + test` | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` |
| `msrv` | `MSRV` | the workspace still compiles on the `rust-version` floor in the manifests |
| `contract` | `bb contract is reproducible` | re-exporting bb's contract yields the committed `contracts/bb` byte for byte |
| `pi` | `real pi provider (allowed to fail)` | the `#[ignore]`d provider test against the real `pi` CLI |

`cargo clippy` and `cargo test` run with `--locked`, so a build that would need a
lockfile update fails instead of quietly resolving one.

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

## Caching

[`Swatinem/rust-cache`](https://github.com/Swatinem/rust-cache) restores the
cargo registry and `target`. Each job passes its own `prefix-key`, so the `msrv`
and `pi` jobs cannot restore a `target` built by a different toolchain or feature
set. A cold run is what the measurements below describe; a warm run skips
compilation almost entirely.

## Measured duration

Cold, with no cache, on a 24-core workstation constrained to four parallel jobs
(`-j4`) to approximate GitHub's `ubuntu-24.04` runner:

| Step | Cold |
| --- | --- |
| `cargo fmt --all -- --check` | 0.3 s |
| `cargo clippy --workspace --all-targets --locked -j4 -- -D warnings` | 16.7 s |
| `cargo test --workspace --locked -j4` | 26.1 s |
| `cargo +1.80 check --workspace --all-targets --locked -j4` | 16.0 s |
| contract re-export (bb fetch + `bun install` + export) | 14.9 s |

The `checks` job is roughly 43 s of real work. Wall-clock on a runner adds
checkout, toolchain download and cache restore, so budget a couple of minutes
cold and far less warm. Every job has a 20-minute `timeout-minutes`.

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
- **No credentials.** It asserts the terminal-state guarantee and that real Pi
  frames reached the bridge; it does not require a model to answer. With none
  configured the run ends `timed_out`, which it accepts. Verified locally: the
  test passes on a machine with `pi 0.85.1` installed and no credentials.

The CLI is installed from npm (`npm install -g @earendil-works/pi-coding-agent@0.85.1`,
about 3 s) rather than expected on the runner, and the version is pinned so a new
upstream release cannot turn the job red without a commit here.

## Branch protection

Protect `main` and require these three checks:

| Required | Reason |
| --- | --- |
| `fmt + clippy + test` | format, lint and the full test suite |
| `MSRV` | the declared floor keeps compiling |
| `bb contract is reproducible` | the committed contract is what the exporter produces |

Do **not** require `real pi provider (allowed to fail)`. It is `continue-on-error`
by design and an upstream CLI must not gate this repository.

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
(`rustup toolchain install 1.80 --profile minimal`):

```bash
cargo +1.80 check --workspace --all-targets --locked
```

The `contract` job needs `bun` and a bb checkout at the pinned revision:

```bash
git clone --filter=blob:none https://github.com/get-bb/bb.git /tmp/bb
git -C /tmp/bb checkout "$(jq -r .source.commit contracts/bb/manifest.json)"
BB_SRC=/tmp/bb scripts/export-bb-contract.sh
git diff --exit-code -- contracts/bb
```

These are the commands the workflow runs, not equivalents of them.
