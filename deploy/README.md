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
    daemon 1       daemon 2   daemon 3   PWA (phone)   desktop (webview)
```

The server and every daemon are separate services with separate data
directories and separate resource domains. A daemon makes outbound connections
only, so it works behind NAT and needs no inbound port.

| Document | Read it for |
| --- | --- |
| [systemd/](systemd/) | the two unit files |
| [env/](env/) | the environment-file templates |
| `install.sh` / `uninstall.sh` | the supported install path |
| [`../docs/remote-access.md`](../docs/remote-access.md) | reaching the server from outside its host |
| [`../docs/mobile.md`](../docs/mobile.md) | using a phone as a client |
| [`../docs/upgrades.md`](../docs/upgrades.md) | version consistency, updates, rollback |
| [`../docs/process-model.md`](../docs/process-model.md) | why server and daemon are independent |

## Prerequisites

- A Linux host with systemd for the server; each execution machine is a Linux
  host with systemd too (macOS and WSL2 can run the binaries directly without
  the units — see [`../docs/process-model.md`](../docs/process-model.md)).
- The two binaries, from either a built checkout (`cargo build --release`) or a
  GitHub Release (`--release <version>`, no toolchain needed — see
  [§ Install from a release](#install-from-a-release)).
- root (or sudo) on each machine. The install script creates a dedicated `loom`
  system user and never runs a service as root.

## Quick start

On the machine that will host the control plane:

```bash
cargo build --release
sudo deploy/install.sh server
$EDITOR /etc/loom/loom-server.env        # optional; loopback + local log is the default
sudo systemctl restart loom-server
curl -s http://127.0.0.1:38886/api/v1/version
```

On each execution machine, where `builder-1` is this machine and
`https://loom.example.com` is the server a browser would open:

```bash
cargo build --release
sudo deploy/install.sh daemon builder-1 https://loom.example.com
$EDITOR /etc/loom/daemon/builder-1.env   # server URL, host name, data paths
sudo systemctl restart loom-host-daemon@builder-1
```

For a single-box deployment, `sudo deploy/install.sh all builder-1 https://loom.example.com`
installs the server and one daemon on the same machine.

The two `cargo build --release` lines above are what a checkout needs. On a
machine that has no Rust toolchain, drop them and add `--release <version>` to
the install command instead — see [§ Install from a release](#install-from-a-release).

The install script is idempotent and never overwrites an existing environment
file, so re-running it refreshes binaries and units and keeps your edits. Set
`LOOM_NO_START=1` to install without starting, and `LOOM_SERVICE_MANAGER=0` to
skip `systemctl` and have the script print the hand-run commands (containers,
CI). `deploy/install.sh help` lists every override.

Open the UI at the server URL — the server hosts it on the same origin as the
API, so "point a client at a URL" is the whole configuration.

## Install from a release

An execution machine usually has no Rust toolchain, so the binaries come from
the release instead of a build. For tag `v0.1.0` the release publishes, per
target triple:

| Asset | |
| --- | --- |
| `loom-server-<target>` | the server binary |
| `loom-daemon-<target>` | the daemon binary |
| `SHA256SUMS` | the SHA-256 of every asset in the release |
| `loom-0.1.0-<target>.tar.gz` | both binaries, `deploy/` and the README |

`<target>` is `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl`. The
binaries are statically linked, so one artifact runs on any glibc or musl host.
How these artifacts are built and verified before they are attached, and how to
check a download yourself, is [`../docs/releasing.md`](../docs/releasing.md).

```bash
# the scripts and units (the archive carries deploy/)
curl -fsSLO https://github.com/550W-HOST/loom/releases/download/v0.1.0/loom-0.1.0-x86_64-unknown-linux-musl.tar.gz
tar xzf loom-0.1.0-x86_64-unknown-linux-musl.tar.gz
cd loom-0.1.0-x86_64-unknown-linux-musl

# the binaries: downloaded from the release and SHA-256 verified by the installer
sudo deploy/install.sh --release v0.1.0 all builder-1 https://loom.example.com
```

`--release latest` takes the newest published release, and `v0.1.0` and `0.1.0`
name the same tag. The installer detects `x86_64` against `aarch64` itself and
refuses any machine type the release does not publish; `LOOM_TARGET` overrides
the detection.

What `--release` does, in order: it downloads `SHA256SUMS`,
`loom-server-<target>` and `loom-daemon-<target>` into a temporary directory,
checks each binary against its `SHA256SUMS` line, and only then installs from
that directory — the same install step a local build goes through.

A failed download, a missing asset or a digest that does not match aborts the
run with a non-zero status and leaves `/usr/local/bin/loom-*` as it was. There is
deliberately no fallback to the binaries already on the machine: that would turn
a failed upgrade into a run that looks successful. Downloading uses `curl` (or
`wget`) and `sha256sum` (or `shasum`) and nothing else. Re-running the same
`--release` re-downloads and re-verifies; environment files and data directories
are left alone, as on every other install.

A private repository needs a token, and `--release` then reads the release
through the GitHub API, because `github.com/.../releases/download/...` answers
`404` for a private repository even when a token is attached:

```bash
sudo GITHUB_TOKEN=ghp_… deploy/install.sh --release v0.1.0 daemon builder-1 https://loom.example.com
```

`GH_TOKEN` works too. `LOOM_RELEASE_REPO` (default `550W-HOST/loom`) selects
another repository, and `LOOM_RELEASE_BASE_URL` / `LOOM_RELEASE_API_BASE` point
at a mirror.

The release carries no signature, so `SHA256SUMS` is the root of trust and is
fetched over the same TLS connection as the binaries: it proves a download is
the file the release published, not that the release is the one you wanted.

## Ports

loom opens exactly one port, and only on the server:

| Listener | Default | Set by | Notes |
| --- | --- | --- | --- |
| Server HTTP + WebSocket + UI | `127.0.0.1:38886` | `LOOM_BIND` | the API, the socket and the UI share it |
| Redis (optional shared relay log) | `127.0.0.1:6379` | `LOOM_REDIS_URL` | only when the shared backend is enabled |
| Execution daemon | *none* | — | outbound WebSocket to the server only |

bb reserved a port per *execution machine* because every daemon ran a local
server. loom does not: the daemon dials out and binds nothing, so port
allocation is a server-side concern again. Two `loom-server` instances on one
host need two distinct `LOOM_BIND` values (for example `127.0.0.1:38886` and
`127.0.0.1:38887`); two daemons on one host need no ports at all and are
distinguished only by their instance name and data directory.

Keep `LOOM_BIND` on loopback. `LOOM_BIND=0.0.0.0:38886` publishes an
unauthenticated, command-executing API to every interface — see
[`../docs/remote-access.md`](../docs/remote-access.md).

## Data directories

Each unit owns its data; nothing is shared between the server and a daemon.

| Path | owner | Contents |
| --- | --- | --- |
| `/etc/loom/loom-server.env` | root, mode 0640 | server configuration |
| `/etc/loom/daemon/<server-key>.env` | root, mode 0640 | one daemon instance's configuration |
| `/var/lib/loom/server/` | `loom` | server relay log: `shard-0.log` … `shard-7.log` (durable backend) |
| `/var/lib/loom/machines/<server-key>/host-id` | `loom` | enrolled host identity, persisted on first enroll |
| `/var/lib/loom/machines/<server-key>/host-id.cursor` | `loom` | host-scope replay cursor |
| `/var/lib/loom/machines/<server-key>/sessions/` | `loom` | per-thread provider sessions |

The daemon directory is keyed by the *server*, which is what makes "one
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
sudo install -d -m 0755 /etc/loom /etc/loom/daemon
sudo install -d -o loom -g loom -m 0750 /var/lib/loom/server
sudo install -d -o loom -g loom -m 0750 /var/lib/loom/machines/builder-1

# 2. binaries
sudo install -m 0755 target/release/loom-server /usr/local/bin/loom-server
sudo install -m 0755 target/release/loom-daemon /usr/local/bin/loom-daemon

# 3. units and environment (server machine)
sudo install -m 0644 deploy/systemd/loom-server.service /etc/systemd/system/
sudo install -m 0640 deploy/env/loom-server.env /etc/loom/loom-server.env
sudo systemctl daemon-reload
sudo systemctl enable --now loom-server

# 4. units and environment (each execution machine)
sudo install -m 0644 deploy/systemd/loom-host-daemon@.service /etc/systemd/system/
sudo install -m 0640 deploy/env/loom-host-daemon.env /etc/loom/daemon/builder-1.env
$EDITOR /etc/loom/daemon/builder-1.env
sudo systemctl daemon-reload
sudo systemctl enable --now loom-host-daemon@builder-1
```

## Operating

```bash
# server
systemctl status loom-server
journalctl -u loom-server -f

# one daemon instance
systemctl status 'loom-host-daemon@builder-1'
journalctl -u 'loom-host-daemon@builder-1' -f

# what the server sees
curl -s http://127.0.0.1:38886/health
curl -s http://127.0.0.1:38886/api/v1/hosts
curl -s http://127.0.0.1:38886/api/v1/hosts/primary
```

`GET /api/v1/hosts/primary` always answers `200`, with
`source: "local" | "remote" | "no_host"`. A server with no daemon connected
answers `no_host`; it does not error, and it never exits because a daemon is
missing.

## Resource limits

The units ship with suggested cgroup caps. They are deliberately asymmetric,
and the asymmetry is the point of the fork: one execution machine exhausting
itself must not take the control plane with it.

| | server | daemon |
| --- | --- | --- |
| `MemoryMax` | 2G | 8G |
| `CPUQuota` | 200% | 400% |
| `TasksMax` | 512 | 8192 |
| `OOMScoreAdjust` | — | 200 |

Size them to the machine: the server is CPU-light and bounded by its relay log;
the daemon's ceiling is however much an agent legitimately needs. `OOMScoreAdjust=200`
makes the kernel kill the daemon before the server or the OS. The server unit
additionally runs under `ProtectSystem=strict`, `NoNewPrivileges` and a closed
capability set because it executes no provider or tool; the daemon unit does
not, because a sandbox that forbids writing outside a fixed root would break
every workspace edit an agent makes. The daemon's isolation is its dedicated
user and its machine.

## Uninstall

```bash
sudo deploy/uninstall.sh daemon builder-1          # stop, disable, keep data
sudo deploy/uninstall.sh server                    # stop, disable, keep data
sudo deploy/uninstall.sh daemon builder-1 --purge  # also delete host id + cursor
sudo deploy/uninstall.sh server --purge            # also delete the relay log
```

Without `--purge` the relay log, the enrolled host id and the replay cursor are
left in place, so a reinstall resumes the same identity and window instead of
minting a new host. Reinstalling the same binaries and environment is the
rollback path in [`../docs/upgrades.md`](../docs/upgrades.md).

## Verification

The recorded clean-machine run — server up, a daemon joined, the UI opened, a
task dispatched and its events replayed — is in
[`../docs/deployment-verification.md`](../docs/deployment-verification.md).
