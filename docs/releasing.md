# Releasing

Two workflows, with one responsibility each.

[`ci.yml`](../.github/workflows/ci.yml) validates every push and pull request
and ships nothing — that split is deliberate, and [`ci.md`](ci.md) explains it.
[`release.yml`](../.github/workflows/release.yml) runs on a tag, builds the
static binaries for two architectures, verifies the artifacts it is about to
publish, attaches them to a GitHub Release, and pushes the matching container
images to GitHub Container Registry.

| You want to | Do this | What you get |
| --- | --- | --- |
| publish a release | `git tag v0.2.0 && git push origin v0.2.0` | the whole pipeline, then a GitHub Release |
| publish a pre-release | tag it `v0.2.0-rc.1` | the same, marked pre-release — a `-` in the tag is the entire rule |
| rehearse without publishing | Actions → **Release** → *Run workflow* | the same build, verification and packaging, uploaded as workflow artifacts; no release is created |
| re-run a failed tag run | re-run the workflow for that tag | the release is refreshed in place (`gh release edit` + `gh release upload --clobber`), not duplicated |

The tag has to name the version the binaries carry: `v` plus the `version` in
[`Cargo.toml`](../Cargo.toml) (`[workspace.package]`), optionally with a suffix
such as `-rc.1`. The `assemble` job fails on anything else, so a release page
cannot be labelled with a version its own files do not report.

## What the jobs are for

| Job | Runs | What it proves |
| --- | --- | --- |
| `ui` | `pnpm install --frozen-lockfile`, `typecheck`, `test`, then rebuild the bundle and require the tree to be unchanged | the bundle compiled into both binaries is the one `ui/src` produces |
| `build` (matrix: x86_64, aarch64) | `cargo build --release --locked --target <triple>` | the binaries compile from the tag with the pinned lockfile |
| `build` → verify | `scripts/verify-release-binaries.sh` | the x86_64 pair runs, answers `/health`, serves its UI, creates a project and enrols a daemon; the aarch64 pair is a self-contained aarch64 artifact carrying the tag's commit |
| `build` → package | `scripts/package-release.sh` | the release page's files exist, with the layout `deploy/install.sh` expects |
| `assemble` | `sha256sum`, version and tag check, `RELEASE_NOTES.md` | one checksum file covering both targets, notes that name the protocol version, and no mislabelled tag |
| `images` | `docker buildx create --driver docker-container`, `scripts/build-container-images.sh` | both container images build from the checksummed files, are pushed as one manifest list each, and the `linux/amd64` halves run and report the version above |
| `release` | `sha256sum -c`, `gh release create`/`edit`/`upload` | the checksummed bytes reached the release page (tag runs only) |

`release` is the only job that needs `contents: write`, `images` the only one that
needs `packages: write`, and `release` the only job that is skipped on a manual
run.

## Why the bundle is rebuilt before Rust is compiled

`crates/server/src/ui.rs` embeds the UI at compile time:

```rust
const INDEX_HTML: &[u8] = include_bytes!("../../../ui/index.html");
const APP_JS: &[u8] = include_bytes!("../../../ui/app.js");
const STYLE_CSS: &[u8] = include_bytes!("../../../ui/style.css");
```

So a `cargo build` compiles whichever `ui/app.js` is in the tree, and a bundle
that drifted from `ui/src` produces a server that is wrong only in a browser —
the failure [`ci.md`](ci.md#the-ui-job) describes at length. The release
workflow therefore runs the same `ui` job as CI and gates every build behind it,
rather than trusting that the tag passed CI:

```bash
pnpm --filter @loom/ui run build
git diff --exit-code -- ui/
test -z "$(git status --porcelain -- ui/)"
```

Both halves are needed for the same reason they are in CI: `git diff` catches a
modified `ui/app.js` and `git status` catches one the build added or removed.

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
`lib/rustlib/<host>/bin/`, which is why no aarch64 toolchain, no
`aarch64-linux-musl-gcc`, no `cargo-zigbuild` and no `cross` container appear
anywhere in this pipeline. The cross toolchain is the toolchain
[`rust-toolchain.toml`](../rust-toolchain.toml) already pins, and both targets
are built by the same `cargo build --target <triple>` with no per-target flags
in the workflow.

Both results are self-contained, and they are not the same ELF shape:

```
loom-server  6.4 MB   x86_64   static PIE     (no interpreter, no NEEDED)
loom-daemon  2.4 MB   x86_64   static PIE
loom-server  6.3 MB   aarch64  static, ET_EXEC (no dynamic section at all)
loom-daemon  2.4 MB   aarch64  static, ET_EXEC
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
| `loom-<version>-<target>.tar.gz` | the two binaries, `deploy/`, `README.md` |
| `loom-server-<target>` | the control plane alone |
| `loom-daemon-<target>` | the execution daemon alone |
| `SHA256SUMS` | checksums for everything above, with relative names |

The archive's top directory holds the binaries *unnamed* —
`loom-0.1.0-x86_64-unknown-linux-musl/loom-server`, not `…/loom-server-x86_64-…`
— because that is the name `deploy/install.sh` looks for under its
`LOOM_BIN_SOURCE`. An extracted archive is therefore installable with the
directory itself as the source:

```bash
tar xzf loom-0.1.0-x86_64-unknown-linux-musl.tar.gz
cd loom-0.1.0-x86_64-unknown-linux-musl
sudo LOOM_BIN_SOURCE=. ./deploy/install.sh server
```

`SHA256SUMS` names its files without a directory prefix, so `sha256sum -c
SHA256SUMS` works in whatever directory a downloader put them in.

The same two binaries are also published as container images — one per process,
for `linux/amd64` and `linux/arm64` — built by the `images` job from the files
above rather than from a second build of the same commit, so a `docker pull`
carries what `sha256sum -c SHA256SUMS` accepted:

| Image | What it is |
| --- | --- |
| `ghcr.io/550w-host/loom-server:<version>` | the control plane, running as uid/gid 1000, relay log in a volume |
| `ghcr.io/550w-host/loom-daemon:<version>` | the execution daemon, the same user, no port |

Each is a manifest list covering both platforms, tagged `<version>`, `v<version>`
and — unless the tag is a pre-release — `latest`. [`containers.md`](containers.md)
has the volumes, the port publishing, how a provider gets into the daemon image,
and an honest account of what a containerised daemon cannot do.

## Verifying an artifact

The pipeline verifies what it publishes; a downloader verifies what they
received. Neither step is optional, and they are different steps.

**In the pipeline**, `scripts/verify-release-binaries.sh` runs the x86_64 pair
the way a deployment does — a durable data directory, a real port, real
sockets:

```
  loom-server 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit 0e74262f…)
  loom-daemon 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit 0e74262f…)
  /health ok (protocol 1, node release-verification)
  GET / -> 200 text/html, 1476 bytes
  GET /app.js -> 200 text/javascript; charset=utf-8, 1191514 bytes
  GET /style.css -> 200 text/css; charset=utf-8, 8801 bytes
  created project proj_01M29YZ0QT1G3S055KX08W6N4A and read it back from the list
  daemon enrolled as host_01M29YZ0TA2KCGQ399DA1RW47K
```

Three things there are worth naming. The artifacts are *executed*, which is the
only way a musl/glibc difference appears. The daemon *enrols*, which is the
protocol handshake a mismatched pair of artifacts would refuse. And the
binaries are asked about themselves rather than read from the source tree, so
what is checked is the file that will be downloaded.

The aarch64 pair cannot be executed on an x86_64 runner, so it is verified with
`--elf-only`: the same script, checking everything a foreign machine can — the
ELF is a 64-bit ARM aarch64 object, it has no interpreter and no `NEEDED`
library, and the tag's commit is stamped in the file. Running it under qemu
would be the alternative, and it would put a second, slower implementation of
execution into the release path for a check the x86_64 run already makes on the
same source.

**When you download one**, on the machine that will run it:

```bash
sha256sum -c SHA256SUMS
./loom-server --version
```

The second line is the same self-description the pipeline checked, and it is
what tells you which release and which commit you are holding before you
install it. A maintainer with an arm64 machine can additionally run
`scripts/verify-release-binaries.sh <extracted-dir>` there — the script is not
x86_64-specific, only native.

## Reproducing a release locally

Every step the pipeline runs, run by hand from the repository root. The same
commands, in the same order:

```bash
# 1. the bundle that will be compiled in must be the committed one
pnpm install --frozen-lockfile
pnpm run typecheck && pnpm run test
pnpm --filter @loom/ui run build
git diff --exit-code -- ui/ && test -z "$(git status --porcelain -- ui/)"

# 2. both targets, into target/<triple>/release
cargo build --release --locked --target x86_64-unknown-linux-musl
cargo build --release --locked --target aarch64-unknown-linux-musl

# 3. run the one this machine can run; check the other is what it claims
scripts/verify-release-binaries.sh target/x86_64-unknown-linux-musl/release \
  --expect-commit "$(git rev-parse HEAD)" --expect-target x86_64-unknown-linux-musl
scripts/verify-release-binaries.sh target/aarch64-unknown-linux-musl/release --elf-only \
  --expect-target aarch64-unknown-linux-musl --expect-commit "$(git rev-parse HEAD)"

# 4. dist/: the two bare binaries and the archive, per target
scripts/package-release.sh x86_64-unknown-linux-musl
scripts/package-release.sh aarch64-unknown-linux-musl

# 5. what the assemble job does
cd dist && sha256sum -- loom-server-* loom-daemon-* *.tar.gz >SHA256SUMS && sha256sum -c SHA256SUMS

# 6. what the images job does, from the same dist/ (docker with buildx)
scripts/build-container-images.sh --platform linux/amd64,linux/arm64 \
  --registry ghcr.io/550w-host --tags 0.1.0,v0.1.0,latest --push
```

`--platform linux/amd64 --tags dev` instead builds and loads only the image this
machine can run — the same Dockerfiles, and enough to `docker run --rm
loom-server:dev --version`.

`scripts/package-release.sh` reads the version from `cargo metadata`, so it
names the archive the same way the pipeline does without running anything, and
it never reads the binaries — which is what lets it package an aarch64 pair on
an x86_64 machine.

## Where the version comes from

`--version` is a single line:

```
loom-server 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit 0e74262f23475e4f3e5bf32ba41c08a33d26af47)
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
  reports on `/api/v1/version` and the daemon refuses to connect without
  ([`upgrades.md`](upgrades.md)). Seeing it in `--version` means an operator can
  find a version mismatch before a daemon reports one for them.

Both binaries print the same line, and the verification fails if the two
disagree: they ship as one set and are upgraded together.

## Measured duration

Measured by running this pipeline's own commands, in this workflow's order, on
a workstation. "Cold" means `target/<triple>` was removed first, so every
dependency was compiled; the cargo registry and the pnpm store were warm, which
on a fresh runner they are not.

| Step | Cold |
| --- | --- |
| `pnpm install --frozen-lockfile` | 7.9 s |
| `pnpm run typecheck` (8 projects) | 27.1 s |
| `pnpm run test` (18 tests) | 19.1 s |
| rebuild the bundle + the committed-bundle check | 5.3 s |
| `cargo build --release --locked --target x86_64-unknown-linux-musl` | 26.7 s |
| `cargo build --release --locked --target aarch64-unknown-linux-musl` | 27.6 s |
| `verify-release-binaries.sh` (x86_64, executed) | 0.5 s |
| `verify-release-binaries.sh --elf-only` (aarch64) | 0.2 s |
| `package-release.sh` (one target) | 0.8 s |
| `SHA256SUMS` + `RELEASE_NOTES.md` (the `assemble` job) | 0.5 s |

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

- daemon self-update (R3) — the pipeline publishes a new daemon, but nothing
  yet tells a running one that it exists, so a protocol bump still means
  upgrading execution machines by hand
- signing: `SHA256SUMS` gives integrity against a corrupted download, not
  against a compromised release page. A signature (minisign, sigstore) would be
  the next step if the artifacts ever leave the repository's own releases.
- `crates.io`, and any target other than the two Linux musl ones. Windows goes
  through WSL2 and uses the x86_64 Linux binary; there is nothing to build for
  it.
