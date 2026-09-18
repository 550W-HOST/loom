# Deploying loom

Everything needed to run the multi-machine shape from
[`../docs/architecture.md`](../docs/architecture.md) § Deployment shapes B,
without reading the source:

```
                  ┌────────────────────────────┐
                  │ loom-server (systemd unit)  │
                  │ loom-relay (in-process)     │
                  └──────────────┬─────────────┘
        ┌──────────────┬─────────┼──────────┬──────────────┐
        ▼              ▼         ▼          ▼              ▼
    worker 1       worker 2   worker 3   PWA (phone)   desktop (webview)
```

The server and every worker are separate services with separate data
directories and separate resource domains. A worker makes outbound connections
only, so it works behind NAT and needs no inbound port.

| Document | Read it for |
| --- | --- |
| [systemd/](systemd/) | the two unit files |
| [env/](env/) | the environment-file templates |
| [containers/](containers/) | the two Dockerfiles and the `docker compose` example ([`../docs/containers.md`](../docs/containers.md)) |
| `install.sh` / `uninstall.sh` | the supported install path |
| [`../docs/remote-access.md`](../docs/remote-access.md) | reaching the server from outside its host |
| [`../docs/mobile.md`](../docs/mobile.md) | using a phone as a client |
| [`../docs/upgrades.md`](../docs/upgrades.md) | version consistency, updates, rollback |
| [`../docs/process-model.md`](../docs/process-model.md) | why server and worker are independent |

## Prerequisites

- A Linux host with systemd for the server; each execution machine is a Linux
  host with systemd too (macOS and WSL2 can run the binary directly without
  the units — see [`../docs/process-model.md`](../docs/process-model.md)).
  Where docker is the deployment mechanism instead, both processes are also
  published as images — the same binary, the same environment variables, and a
  container instead of a unit: [`../docs/containers.md`](../docs/containers.md).
- The binary, from either a built checkout (`cargo build --release -p loom`) or a
  GitHub Release (`--release <version>`, no toolchain needed — see
  [§ Install from a release](#install-from-a-release)). A build from source needs
  the product app built first — the server compiles `apps/app/dist` into the
  binary, so `cargo build` fails without it. A release has already done that.
  The install puts `loom` in `/usr/local/bin` and links `loom-server` and
  `loom-worker` to it: one file, one role per invocation.
- root (or sudo) on each machine. The install script creates a dedicated `loom`
  system user and never runs a service as root.

## Quick start

On the machine that will host the control plane:

```bash
pnpm install --frozen-lockfile          # once per checkout
pnpm --filter @bb/app run build         # cargo build compiles the app into the server
cargo build --release -p loom
sudo deploy/install.sh server
$EDITOR /etc/loom/loom-server.env        # optional; loopback + local log is the default
sudo systemctl restart loom-server
curl -s http://127.0.0.1:38886/api/v1/version
```

On each execution machine, where `builder-1` is this machine and
`https://loom.example.com` is the server a browser would open:

```bash
# a checkout builds the same way, and needs the same app build first
pnpm install --frozen-lockfile && pnpm --filter @bb/app run build
cargo build --release -p loom
sudo deploy/install.sh worker builder-1 https://loom.example.com
$EDITOR /etc/loom/worker/builder-1.env   # server URL, host name, data paths
sudo systemctl restart loom-worker@builder-1
```

For a single-box deployment, `sudo deploy/install.sh all builder-1 https://loom.example.com`
installs the server and one worker on the same machine.

Every `cargo build --release -p loom` above needs `pnpm --filter @bb/app run
build` first: the product app is compiled into the server, so a checkout that has
not built it cannot compile the binary. A machine with no toolchain needs neither
— the release archive carries the binary, and `--release <version>` installs it
from the release — see [§ Install from a release](#install-from-a-release).

The install script is idempotent and never overwrites an existing environment
file, so re-running it refreshes the binary and the units and keeps your edits.
Set `LOOM_NO_START=1` to install without starting, and `LOOM_SERVICE_MANAGER=0`
to skip `systemctl` and have the script print the hand-run commands
(containers, CI). `deploy/install.sh help` lists every override.

Open the UI at the server URL — the server serves the product app on the same
origin as the API, so "point a client at a URL" is the whole configuration. The
app is compiled into the binary, so there is nothing to install beside it and no
variable to point at one: an install, an upgrade and a rollback are that one file
— with the two names it answers to — and nothing else. `LOOM_UI_PROXY` — a
frontend dev server to proxy to — is the only override, it is for developing the
app against a real server, and a `LOOM_UI_DIR` left in an environment file from
an older release is inert: the variable is not read any more.

## Install from a release

An execution machine usually has no Rust toolchain, so the binary comes from
the release instead of a build. For tag `v0.1.0` the release publishes, per
target triple:

| Asset | |
| --- | --- |
| `loom-<target>` | the binary: `loom server` or `loom worker` |
| `SHA256SUMS` | the SHA-256 of every asset in the release |
| `loom-0.1.0-<target>.tar.gz` | the same binary, `deploy/` and the README |

`<target>` is `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`. The
binary is statically linked, so one artifact runs on any glibc or musl host.
How these artifacts are built and verified before they are attached, and how to
check a download yourself, is [`../docs/releasing.md`](../docs/releasing.md).

```bash
# the scripts and units (the archive carries deploy/)
curl -fsSLO https://github.com/550W-HOST/loom/releases/download/v0.1.0/loom-0.1.0-x86_64-unknown-linux-musl.tar.gz
tar xzf loom-0.1.0-x86_64-unknown-linux-musl.tar.gz
cd loom-0.1.0-x86_64-unknown-linux-musl

# the binary: downloaded from the release and SHA-256 verified by the installer
sudo deploy/install.sh --release v0.1.0 all builder-1 https://loom.example.com
```

This path needs no JavaScript toolchain either: this binary was built with
the product app compiled into the server, so `install.sh` installs `loom`, links
`loom-server` and `loom-worker` to it, writes the units and the environment
template, and there is nothing else to fetch.

`--release latest` takes the newest published release, and `v0.1.0` and `0.1.0`
name the same tag. The installer detects `x86_64` against `aarch64` itself and
refuses any machine type the release does not publish; `LOOM_TARGET` overrides
the detection.

`--release` is for the **server**, and for a worker with self-update turned off.
A worker in the default configuration never needs it: when the server speaks a
newer protocol the worker fetches the artifact that server hosts — the
`loom-worker` name `install.sh` leaves beside `loom` — verifies it and restarts
itself ([`../docs/upgrades.md`](../docs/upgrades.md) § Worker self-update).
`install.sh` puts `loom` in `/usr/local/bin` with `loom-worker` as a symlink to
it, which is also how the server knows what to host: the artifact directory
defaults to the directory holding the server's own executable, so a name beside
it is hosted with nothing to configure.

What `--release` does, in order: it downloads `SHA256SUMS` and `loom-<target>`
into a temporary directory, checks the file against its `SHA256SUMS` line, and
only then installs from that directory — the same install step a local build
goes through.

A failed download, a missing asset or a digest that does not match aborts the
run with a non-zero status and leaves `/usr/local/bin/loom` (and the two names
beside it) as it was. There is deliberately no fallback to the binary already on
the machine: that would turn a failed upgrade into a run that looks successful.
Downloading uses `curl` (or `wget`) and `sha256sum` (or `shasum`) and nothing
else. Re-running the same `--release` re-downloads and re-verifies; environment
files and data directories are left alone, as on every other install.

A private repository needs a token, and `--release` then reads the release
through the GitHub API, because `github.com/.../releases/download/...` answers
`404` for a private repository even when a token is attached:

```bash
sudo GITHUB_TOKEN=ghp_… deploy/install.sh --release v0.1.0 worker builder-1 https://loom.example.com
```

`GH_TOKEN` works too. `LOOM_RELEASE_REPO` (default `550W-HOST/loom`) selects
another repository, and `LOOM_RELEASE_BASE_URL` / `LOOM_RELEASE_API_BASE` point
at a mirror.

The release carries no signature, so `SHA256SUMS` is the root of trust and is
fetched over the same TLS connection as the binary: it proves a download is
the file the release published, not that the release is the one you wanted.

## Ports

loom opens exactly one port, and only on the server:

| Listener | Default | Set by | Notes |
| --- | --- | --- | --- |
| Server HTTP + WebSocket + UI | `127.0.0.1:38886` | `LOOM_BIND` | the API, the socket and the UI share it |
| Redis (optional shared relay log) | `127.0.0.1:6379` | `LOOM_REDIS_URL` | only when the shared backend is enabled |
| Execution worker | *none* | — | outbound WebSocket to the server only |

bb reserved a port per *execution machine* because every worker ran a local
server. loom does not: the worker dials out and binds nothing, so port
allocation is a server-side concern again. Two `loom-server` instances on one
host need two distinct `LOOM_BIND` values (for example `127.0.0.1:38886` and
`127.0.0.1:38887`); two workers on one host need no ports at all and are
distinguished only by their instance name and data directory.

Keep `LOOM_BIND` on loopback. `LOOM_BIND=0.0.0.0:38886` publishes an
unauthenticated, command-executing API to every interface — see
[`../docs/remote-access.md`](../docs/remote-access.md).

## Data directories

Each unit owns its data; nothing is shared between the server and a worker.

| Path | owner | Contents |
| --- | --- | --- |
| `/etc/loom/loom-server.env` | root, mode 0640 | server configuration |
| `/etc/loom/worker/<server-key>.env` | root, mode 0640 | one worker instance's configuration |
| `/var/lib/loom/server/` | `loom` | server relay log: `shard-0.log` … `shard-7.log` (durable backend) |
| `/var/lib/loom/machines/<server-key>/host-id` | `loom` | enrolled host identity, persisted on first enroll |
| `/var/lib/loom/machines/<server-key>/host-id.cursor` | `loom` | host-scope replay cursor |
| `/var/lib/loom/machines/<server-key>/sessions/` | `loom` | per-thread provider sessions |

The worker directory is keyed by the *server*, which is what makes "one
instance per server" work: a machine that joins two servers gets two instances,
two data directories and two host identities, and stopping one leaves the other
connected. This is the loom shape of bb's `~/.bb-machines/<server-host>`.

`LOOM_DATA_DIR` and `LOOM_REDIS_URL` are alternatives; setting both is a
startup error. With neither, the server keeps its log in process memory and
loses the replay window on restart — fine for a throwaway local run, not for a
deployment.

## Manual command sequence

The install script is a convenience over the following, which is what to do by
hand (or reproduce in a configuration-management tool). Nothing below needs the
script.

```bash
# 1. user and directories
sudo useradd --system --home-dir /var/lib/loom --shell /usr/sbin/nologin loom
sudo install -d -m 0755 /etc/loom /etc/loom/worker
sudo install -d -o loom -g loom -m 0750 /var/lib/loom/server
sudo install -d -o loom -g loom -m 0750 /var/lib/loom/machines/builder-1

# 2. the binary, and the two names it answers to
sudo install -m 0755 target/release/loom /usr/local/bin/loom
sudo ln -sfn loom /usr/local/bin/loom-server
sudo ln -sfn loom /usr/local/bin/loom-worker

# 3. units and environment (server machine)
sudo install -m 0644 deploy/systemd/loom-server.service /etc/systemd/system/
sudo install -m 0640 deploy/env/loom-server.env /etc/loom/loom-server.env
sudo systemctl daemon-reload
sudo systemctl enable --now loom-server

# 4. units and environment (each execution machine)
sudo install -m 0644 deploy/systemd/loom-worker@.service /etc/systemd/system/
sudo install -m 0640 deploy/env/loom-worker.env /etc/loom/worker/builder-1.env
$EDITOR /etc/loom/worker/builder-1.env
sudo systemctl daemon-reload
sudo systemctl enable --now loom-worker@builder-1
```

## Operating

```bash
# server
systemctl status loom-server
journalctl -u loom-server -f

# one worker instance
systemctl status 'loom-worker@builder-1'
journalctl -u 'loom-worker@builder-1' -f

# what the server sees
curl -s http://127.0.0.1:38886/health
curl -s http://127.0.0.1:38886/api/v1/hosts
curl -s http://127.0.0.1:38886/api/v1/hosts/primary
```

`GET /api/v1/hosts/primary` always answers `200`, with
`source: "local" | "remote" | "no_host"`. A server with no worker connected
answers `no_host`; it does not error, and it never exits because a worker is
missing.

## Resource limits

The units ship with suggested cgroup caps. They are deliberately asymmetric,
and the asymmetry is the point of the fork: one execution machine exhausting
itself must not take the control plane with it.

| | server | worker |
| --- | --- | --- |
| `MemoryMax` | 2G | 8G |
| `CPUQuota` | 200% | 400% |
| `TasksMax` | 512 | 8192 |
| `OOMScoreAdjust` | — | 200 |

Size them to the machine: the server is CPU-light and bounded by its relay log;
the worker's ceiling is however much an agent legitimately needs. `OOMScoreAdjust=200`
makes the kernel kill the worker before the server or the OS. The server unit
additionally runs under `ProtectSystem=strict`, `NoNewPrivileges` and a closed
capability set because it executes no provider or tool; the worker unit does
not, because a sandbox that forbids writing outside a fixed root would break
every workspace edit an agent makes. The worker's isolation is its dedicated
user and its machine.

## Uninstall

```bash
sudo deploy/uninstall.sh worker builder-1          # stop, disable, keep data
sudo deploy/uninstall.sh server                    # stop, disable, keep data
sudo deploy/uninstall.sh worker builder-1 --purge  # also delete host id + cursor
sudo deploy/uninstall.sh server --purge            # also delete the relay log
```

Without `--purge` the relay log, the enrolled host id and the replay cursor are
left in place, so a reinstall resumes the same identity and window instead of
minting a new host. Reinstalling the same binary and environment is the
rollback path in [`../docs/upgrades.md`](../docs/upgrades.md). The worker's two
self-update state files live in the same data directory and are safe to delete:
`worker-update-attempt.json` (the backoff counter) and
`host-artifact.sha256` (the digest the next fetch is conditional on).

## Verification

The recorded clean-machine run — server up, a worker joined, the UI opened, a
task dispatched and its events replayed — is in
[`../docs/deployment-verification.md`](../docs/deployment-verification.md).
