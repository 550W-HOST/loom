# Releasing

Two workflows, with one responsibility each.

[`ci.yml`](../.github/workflows/ci.yml) validates every push and pull request
and ships nothing — that split is deliberate, and [`ci.md`](ci.md) explains it.
[`release.yml`](../.github/workflows/release.yml) runs on a tag, builds the
static binary for each of two architectures, verifies the artifacts it is about
to publish, attaches them to a GitHub Release, and pushes the matching container
images to GitHub Container Registry.

| You want to | Do this | What you get |
| --- | --- | --- |
| publish a release | `git tag v0.2.0 && git push origin v0.2.0` | the whole pipeline, then a GitHub Release |
| publish a pre-release | tag it `v0.2.0-rc.1` | the same, marked pre-release — a `-` in the tag is the entire rule |
| rehearse without publishing | Actions → **Release** → *Run workflow* | the same build, verification and packaging, uploaded as workflow artifacts; no release is created |
| re-run a failed tag run | re-run the workflow for that tag | the release is refreshed in place (`gh release edit` + `gh release upload --clobber`), not duplicated |

The tag has to name the version the binary carries: `v` plus the `version` in
[`Cargo.toml`](../Cargo.toml) (`[workspace.package]`), optionally with a suffix
such as `-rc.1`. The `assemble` job fails on anything else, so a release page
cannot be labelled with a version its own files do not report.

## What the jobs are for

| Job | Runs | What it proves |
| --- | --- | --- |
| `ui` | `pnpm install --frozen-lockfile`, `typecheck`, `test`, `pnpm --filter @bb/app run build`, `pnpm run check:bundle` | the bundle every Rust job compiles into the binary is built from the tag's own source and holds its budget |
| `build` (matrix: x86_64, aarch64) | `cargo build --release --locked -p loom --target <triple>` | the binary compiles from the tag with the pinned lockfile |
| `build` → verify | `scripts/verify-release-binaries.sh` | the x86_64 binary runs both roles, answers `/health`, serves the UI it carries with no UI variable set, hosts the artifact it is itself with a matching digest and a `304` for a conditional request, creates a project and enrols a worker; the aarch64 binary is a self-contained aarch64 artifact carrying the tag's commit |
| `build` → package | `scripts/package-release.sh` | the release page's files exist: the bare `loom-<target>` binary and the per-target archive |
| `assemble` | `sha256sum`, version and tag check, `RELEASE_NOTES.md` | one checksum file covering both targets, notes that name the protocol version, and no mislabelled tag |
| `images` | `docker buildx create --driver docker-container`, `scripts/build-container-images.sh` | both container images build from the checksummed file, are pushed as one manifest list each, and the `linux/amd64` halves run and report the version above |
| `release` | `sha256sum -c`, `gh release create`/`edit`/`upload` | the checksummed bytes reached the release page (tag runs only) |

`release` is the only job that needs `contents: write`, `images` the only one that
needs `packages: write`, and `release` the only job that is skipped on a manual
run.

## Why the app is built before the binary is built

The client is compiled into the binary: `crates/server/build.rs` walks
`apps/app/dist` and embeds every file, so a release ships one artifact with no
UI directory to stage, point at or get wrong
([`ui.md`](ui.md)). The bundle is therefore an *input* to the Rust jobs rather
than something shipped beside them, which is why the workflow builds the app
itself first, hands it to the target jobs as an artifact, and gates every Rust
job behind that — rather than trusting that the tag passed CI:

```bash
pnpm --filter @bb/app run build     # → apps/app/dist, embedded by cargo build
pnpm run check:bundle
```

A tag that skipped this would not publish a server without a UI; it would fail
to build one, because a missing bundle is a build error rather than a runtime
surprise. Catching it here keeps that failure off the tag. The budget check is
the same ratchet CI applies ([`ci.md`](ci.md#the-bundle-budget)), repeated here
because a release is the one build nobody gets to re-run before it is used.

## Targets, and how aarch64 links

| Target | Linker | Why |
| --- | --- | --- |
| `x86_64-unknown-linux-musl` | the host `cc` | a linker for the target already exists on the machine doing the build; this is the recipe measured through to a running server |
| `aarch64-unknown-linux-musl` | `rust-lld` | the host's `ld` is not an aarch64 linker: without this, the build fails with `/usr/bin/ld: unrecognized option '--fix-cortex-a53-843419'` |

The cross configuration is one line in [`.cargo/config.toml`](../.cargo/config.toml):

```toml
[target.aarch64-unknown-linux-musl]
linker = "rust-lld"
```

`rust-lld` is resolved out of the toolchain's own
`lib/rustlib/<host>/bin/`, which is why no `cargo-zigbuild` and no `cross`
container appear anywhere in this pipeline: the cross linker is the toolchain
[`rust-toolchain.toml`](../rust-toolchain.toml) already pins, and both targets
are built by the same `cargo build --target <triple>`.

## The C compiler the build scripts need

A cross *linker* is not the only cross tool a release build uses. The server's
embedded store is `rusqlite` with `bundled`, which compiles SQLite's C source
for the target, so the `cc` crate has to find a C compiler for that target:
without one the build stops at `failed to find tool "x86_64-linux-musl-gcc"`
(the table above is about linking, so this is the part it cannot describe —
both musl targets were run and failed exactly this way before the step existed).

The `build` job's *Install the target's C toolchain* step downloads the
musl-cross-make archive for its target from [musl.cc](https://musl.cc/),
verifies it against the digest recorded beside the target in the job's matrix,
unpacks it under `$RUNNER_TEMP` and appends its `bin/` to `PATH`. That is the
whole configuration, because the archive's `bin/<triple>-gcc` is the name `cc`
looks for; the compiler is not the linker and does not change how either
artifact links, which is why `.cargo/config.toml` still holds exactly one line.

Both targets were built and checked with these archives on 2026-09-21:
`scripts/verify-release-binaries.sh` ran the x86_64 artifact in both roles and
watched it serve the embedded app, enroll a worker and answer a contract-shaped
write, and passed the ELF-only checks for the aarch64 one.

Both results are self-contained, and they are not the same ELF shape. These were
recorded before the client was compiled in **and** before the two roles became
one file: `loom-server` and `loom-worker` were separate artifacts then, so the
list below holds two of each. Today's artifact is one `loom` per target, in the
same two shapes (static PIE on x86_64, static `ET_EXEC` on aarch64) and larger
than the old `loom-server` line by the bundle it carries; the old `loom-worker`
line is the closest thing to a floor for it. The four lines are kept as the
record of that run.

```
loom-server  6.4 MB   x86_64   static PIE     (no interpreter, no NEEDED)
loom-worker  2.4 MB   x86_64   static PIE
loom-server  6.3 MB   aarch64  static, ET_EXEC (no dynamic section at all)
loom-worker  2.4 MB   aarch64  static, ET_EXEC
```

A static PIE carries a dynamic section so it can relocate itself, and `file`
therefore calls it "dynamically linked"; `ldd` calls the same file statically
linked, which is what it is — there is no loader to find and no shared library
to resolve on the host. That is why
[`verify-release-binaries.sh`](../scripts/verify-release-binaries.sh) accepts a
binary on the absence of `INTERP` and `NEEDED` rather than on the absence of a
dynamic section.

## What lands on the release page

| Asset | Contents |
| --- | --- |
| `loom-<version>-<target>.tar.gz` | the binary, named for its target, and `README.md` |
| `loom-<target>` | the one binary, both roles |
| `SHA256SUMS` | checksums for both files above, with relative names |

The archive's top directory holds the binary *named for its target* —
`loom-0.1.0-x86_64-unknown-linux-musl/loom-x86_64-unknown-linux-musl` — beside
`README.md`. An extracted archive can be run in place, or the file installed
under any stable name:

```bash
tar xzf loom-0.1.0-x86_64-unknown-linux-musl.tar.gz
cd loom-0.1.0-x86_64-unknown-linux-musl
./loom-x86_64-unknown-linux-musl server --version
```

There is no installer in the archive, no `loom-server` / `loom-worker` symlink
to create and no configuration to fill in: one binary serves both roles, and
the role is the subcommand.

There is no UI beside the binary, and nothing for an install to place: the
product app is inside it ([`ui.md`](ui.md)). The archive is therefore
just the binary and `README.md`, so `SHA256SUMS` naming
`loom-<target>` and `*.tar.gz` covers the whole download, and
verifying the tarball verifies the client too.

The release is just the binary: place it where you want it and run it as
`loom server` or `loom worker`; there is no bundle to copy and no UI variable to
fill in, because the
client arrives in the binary. That reverses the old rule — an install that
placed no bundle used to be a failure — and a configuration from an earlier
release may still carry a `LOOM_UI_DIR` entry: it is no longer read, the server
serves the
client in its binary and says once that it is ignoring the variable
([`upgrades.md`](upgrades.md)).

`SHA256SUMS` names its files without a directory prefix, so `sha256sum -c
SHA256SUMS` works in whatever directory a downloader put them in.

The same one binary is also published as container images — one per role,
for `linux/amd64` and `linux/arm64` — built by the `images` job from the file
above rather than from a second build of the same commit, so a `docker pull`
carries what `sha256sum -c SHA256SUMS` accepted:

| Image | What it is |
| --- | --- |
| `ghcr.io/550w-host/loom-server:<version>` | the control plane, running as uid/gid 1000, relay log in a volume |
| `ghcr.io/550w-host/loom-worker:<version>` | the execution worker, the same user, no port |

Each is a manifest list covering both platforms, tagged `<version>`, `v<version>`
and — unless the tag is a pre-release — `latest`. [`containers.md`](containers.md)
has the volumes, the port publishing, how a provider gets into the worker image,
and an honest account of what a containerised worker cannot do.

## Verifying an artifact

The pipeline verifies what it publishes; a downloader verifies what they
received. Neither step is optional, and they are different steps.

**In the pipeline**, `scripts/verify-release-binaries.sh` runs the x86_64 binary
in both roles the way a deployment does — a durable data directory, a real port,
real sockets. The recorded output of the pipeline run that published `0e74262f`
is:

```
  loom-server 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit 0e74262f…)
  loom-worker 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit 0e74262f…)
  /health ok (protocol 1, node release-verification)
  GET / -> 200 text/html, 1476 bytes
  GET /app.js -> 200 text/javascript; charset=utf-8, 1191514 bytes
  GET /style.css -> 200 text/css; charset=utf-8, 8801 bytes
  worker enrolled as host_01M29YZ0TA2KCGQ399DA1RW47K
  created project proj_01M29YZ0QT1G3S055KX08W6N4A and read it back from the list
```

Four things there are worth naming. The artifact is *executed*, which is the
only way a musl/glibc difference appears. The worker *enrols*, which is the
protocol handshake a mismatched server and worker would refuse. The project write
follows it, naming the host that worker enrolled as — `projects.create` takes a
source — so the claim is that the two roles work together, not that each half
works alone. And the file is asked about itself rather than read from the
source tree, so what is checked is the file that will be downloaded.

That block was recorded when each role was its own file, which is why it prints
two version lines; today the script asks the one binary three ways — bare, then
`server`, then `worker` — and all three lines name the same build and commit.
The two asset lines in it are **historical** in the other sense too: `/app.js`
and `/style.css`
are the buildless reference client, which no longer exists, and the run predates
the product app. What the script checks now is the client inside the binary: it
starts the server with no UI variable set, which is the whole configuration a
release needs, and asserts that `/` answers `200` with an HTML shell naming its
`/assets/*.js`, that that asset answers `200` as `text/javascript`, that a deep
client route answers the same document (so history routing works), that an
unknown `/api/v1` route is a JSON `404` rather than the shell, and that starting
the same binary with `LOOM_UI_DIR` set still serves the embedded app and says
once that the variable is no longer read and it is ignoring it. The historical `/app.js` and
`/style.css` are what a bundle-on-disk release served; the served paths today are
the app's own hashed assets.

R3 added three lines to that output. They were recorded on a **local rehearsal**
from a checkout rather than by a tag run, which is why the digests and the
host/project ids below differ from the block above; the first `v*` tag after this
change replaces them with runner values:

```
  /install/version ok (protocol 1)
  GET /install/loom-worker -> 200, 4dc62cff848bcede47b495b83c54314996ff8b1fee047949c56b7d2bfc218040 (matches the built loom-worker)
  GET /install/loom-worker (If-None-Match) -> 304
```

These are the self-update source of truth (`docs/upgrades.md`). The script
starts the one binary from a directory that holds only that `loom` file, so
`GET /install/loom-worker` takes its fallback and serves the running
executable. What is served is compared **against the built binary**: the served
digest must equal both the digest of the body that came over the socket and the
digest of the `loom` the script started from. A release that hosted the wrong
binary, or served a digest that did not match its bytes, fails here rather than
on a customer's machine. The conditional request is checked in the same breath,
because a `304` is what keeps a fleet's reconnects from re-downloading the
binary. The middle line was recorded before the two roles became one file and
before the self-serving fallback existed, when the hosted `loom-worker` was a
separate build; the digest it names is that build's.

The two routes also have a shape guard in
`crates/server/tests/release_verification.rs`, which is the W-554 lesson applied
here: the script runs only on a `v*` tag, so a response shape that stopped
matching what it parses would stay invisible until a release.

The aarch64 binary cannot be executed on an x86_64 runner, so it is verified with
`--elf-only`: the same script, checking everything a foreign machine can — the
ELF is a 64-bit ARM aarch64 object, it has no interpreter and no `NEEDED`
library, and the tag's commit is stamped in the file. Running it under qemu
would be the alternative, and it would put a second, slower implementation of
execution into the release path for a check the x86_64 run already makes on the
same source.

**When you download one**, on the machine that will run it:

```bash
sha256sum -c SHA256SUMS
./loom-x86_64-unknown-linux-musl server --version
```

The second line is the same self-description the pipeline checked, and it is
what tells you which release and which commit you are holding before you
install it. A maintainer with an arm64 machine can additionally run
`scripts/verify-release-binaries.sh <dir>` there — the script is not
x86_64-specific, only native. It expects the file as `<dir>/loom`, and also
accepts a downloaded `loom-<target>` when `--expect-target <triple>` names it.

## Reproducing a release locally

Every step the pipeline runs, run by hand from the repository root. The same
commands, in the same order:

```bash
# 1. the bundle the Rust jobs compile into the binary
pnpm install --frozen-lockfile
pnpm run typecheck && pnpm run test
pnpm --filter @bb/app run build
pnpm run check:bundle

# 2. the one binary for both targets, into target/<triple>/release
cargo build --release --locked -p loom --target x86_64-unknown-linux-musl
cargo build --release --locked -p loom --target aarch64-unknown-linux-musl

# 3. run the one this machine can run; check the other is what it claims
scripts/verify-release-binaries.sh target/x86_64-unknown-linux-musl/release \
  --expect-commit "$(git rev-parse HEAD)" --expect-target x86_64-unknown-linux-musl
scripts/verify-release-binaries.sh target/aarch64-unknown-linux-musl/release --elf-only \
  --expect-target aarch64-unknown-linux-musl --expect-commit "$(git rev-parse HEAD)"

# 4. dist/: the bare binary and the archive, per target
scripts/package-release.sh x86_64-unknown-linux-musl
scripts/package-release.sh aarch64-unknown-linux-musl

# 5. what the assemble job does
cd dist && sha256sum -- loom-* >SHA256SUMS && sha256sum -c SHA256SUMS

# 6. what the images job does, from the same dist/ (docker with buildx)
scripts/build-container-images.sh --platform linux/amd64,linux/arm64 \
  --registry ghcr.io/550w-host --tags 0.1.0,v0.1.0,latest --push
```

`--platform linux/amd64 --tags dev` instead builds and loads only the image this
machine can run — the same Dockerfiles, and enough to `docker run --rm
loom-server:dev --version`.

`scripts/package-release.sh` reads the version from `cargo metadata`, so it
names the archive the same way the pipeline does without running anything, and
it never reads the binary — which is what lets it package an aarch64 artifact on
an x86_64 machine.

## Where the version comes from

Asking either role of the binary prints a single line, and only the role name
differs between them:

```
loom-server 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit 0e74262f23475e4f3e5bf32ba41c08a33d26af47)
loom-worker 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit 0e74262f23475e4f3e5bf32ba41c08a33d26af47)
```

The version is `CARGO_PKG_VERSION`. The other three fields are stamped at build
time by [`crates/server/build.rs`](../crates/server/build.rs), which emits
`LOOM_GIT_COMMIT` and `LOOM_BUILD_TARGET` for
[`crates/server/src/build_info.rs`](../crates/server/src/build_info.rs) to read
with `env!`:

- **commit** — `LOOM_GIT_COMMIT` if set, otherwise `git rev-parse HEAD`, and
  `unknown` when the build has neither. The environment variable is what makes
  the identity a build input rather than a property of the directory the build
  happened in, which is the only way to stamp an artifact built from an export
  with no `.git`.
- **target** — whatever triple cargo is compiling for. This is how a file says
  whether it is the musl artifact or a local development build; a plain `cargo
  build` prints `x86_64-unknown-linux-gnu`.
- **protocol** — `loom_server::PROTOCOL_VERSION`, the same constant the server
  reports on `/api/v1/version` and the worker refuses to connect without
  ([`upgrades.md`](upgrades.md)). Seeing it in `--version` means an operator can
  find a version mismatch before a worker reports one for them.

The commit, target and protocol come from one build, so the two lines can never
disagree: there is one file, and it is upgraded as a whole.

## Measured duration

Measured by running this pipeline's own commands, in this workflow's order, on
a workstation. "Cold" means `target/<triple>` was removed first, so every
dependency was compiled; the cargo registry and the pnpm store were warm, which
on a fresh runner they are not.

| Step | Cold |
| --- | --- |
| `pnpm install --frozen-lockfile` | 7.9 s |
| `pnpm run typecheck` | 27.1 s |
| `pnpm run test` | 19.1 s |
| `rebuild the reference bundle + the committed-bundle check` | 5.3 s |
| `cargo build --release --locked --target x86_64-unknown-linux-musl` | 26.7 s |
| `cargo build --release --locked --target aarch64-unknown-linux-musl` | 27.6 s |
| `verify-release-binaries.sh` (x86_64, executed) | 0.5 s (0.8 s with the install-route checks) |
| `verify-release-binaries.sh --elf-only` (aarch64) | 0.2 s |
| `package-release.sh` (one target) | 0.8 s |
| `SHA256SUMS` + `RELEASE_NOTES.md` (the `assemble` job) | 0.5 s |

The bundle row measured the reference client's esbuild step and its
committed-bytes check; the job now runs the app's Vite build and its budget
check, which is not what those 5.3 s measured. The two `cargo build` rows
measured the whole workspace before the build narrowed to the one binary
(`-p loom`); everything else in the table is unchanged work.

| | |
| --- | --- |
| Host | 80 cores, Ubuntu 20.04.6 LTS (Focal Fossa), kernel 5.15 |
| Toolchain | `rustc 1.98.0` (from `rust-toolchain.toml`), pnpm 11.20.0, Node 22 |
| Registry | warm (`~/.cargo`, pnpm store); a runner's first run is not |

The critical path is `ui` (≈60 s) → the slower `build` leg (≈28 s) →
`assemble` (≈1 s), so about **1 m 30 s** of work, with the two `build` legs
running concurrently and `release` adding almost nothing. The cargo numbers are
the ones that should shrink least on a runner: the same `ui` steps took 1 m 00 s
there ([`ci.md`](ci.md#measured-duration)), which is close to the 59 s measured
here, so a runner run lands in the same range. What is not in the table is the
part that only exists in Actions: artifact upload and download between jobs.
The first `v*` tag is what replaces these estimates with runner numbers.

## What is not here

Deliberately, and with the issues that own them:

- **worker self-update from the release page**: a running worker fetches the
  matching binary from the **server** it is joined to (`/install/loom-worker`),
  not from this release page — see [`upgrades.md`](upgrades.md). The release's
  `SHA256SUMS` is what a human verifies a download with;
  the worker verifies the server's own digest. Both are integrity checks against
  transit, not provenance.
- **signing**: `SHA256SUMS` gives integrity against a corrupted download, not
  against a compromised release page — and the worker's digest has the same gap,
  since the server serves both the binary and its digest. A signature (minisign,
  sigstore) would move that trust root, and is the next step if artifacts ever
  come from anywhere other than the server that dispatches the work.
- `crates.io`, and any target other than the two Linux musl ones. Windows goes
  through WSL2 and uses the x86_64 Linux binary; there is nothing to build for
  it.
