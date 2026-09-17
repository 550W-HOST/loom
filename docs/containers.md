# Containers

`loom-server` and `loom-daemon` are published as container images, one each, for
`linux/amd64` and `linux/arm64`:

```
ghcr.io/550w-host/loom-server:<version>
ghcr.io/550w-host/loom-daemon:<version>
```

They are the same processes as the binaries `deploy/install.sh` installs, with
the same environment variables, the same data layout and the same split between
server and daemon: the server never starts a daemon, and the daemon never needs
the server's process — only its socket. What a container changes is where the
filesystem boundary is, and for the daemon that boundary is the whole question
([§ What a containerised daemon cannot do](#what-a-containerised-daemon-cannot-do)).

| | `loom-server` | `loom-daemon` |
| --- | --- | --- |
| Base | `scratch` | `alpine:3.22` (digest-pinned) |
| Image size | 6.9 MB, plus the UI bundle | 10.7 MB |
| Runs as | `1000:1000` | `1000:1000` |
| Listens on | `0.0.0.0:38886` (`EXPOSE`d) | nothing |
| Data volume | `/var/lib/loom/server` | `/var/lib/loom` |
| Workspace | — | `/workspace` (bind-mounted) |
| UI bundle | `/usr/local/share/loom/ui`, as `LOOM_UI_DIR` | — |
| Provider CLIs | none, and none needed | none — [§ Providers](#providers) |

`scratch` and not a distribution for the server because it needs nothing: the
binary is a static musl build and the process executes no provider and no tool,
so a userland it never calls would only be attack surface. What else the image
carries is a copied directory, which needs no base to copy it. `alpine` and not
`scratch` for the daemon for the opposite reason — it exists to execute provider
CLIs, and a provider is usually not a static binary (`pi` is a Node program), so
the image has to be a base something can be added to.

The server image carries the UI as a directory rather than inside the binary:

```dockerfile
COPY --chown=1000:1000 ui /usr/local/share/loom/ui
ENV LOOM_UI_DIR=/usr/local/share/loom/ui
```

The staged context's `ui/` is the product app's build output — `apps/app/dist`,
from `pnpm --filter @bb/app run build` — so a container is one of the three ways
the same bytes reach a deployment, next to the release archive's `ui/` and an
installed `<prefix>/share/loom/ui` ([`releasing.md`](releasing.md),
[`ui.md`](ui.md)). The copy is the reason the size column says "plus the UI
bundle": the 6.9 MB is the control plane alone, and the bundle adds its own
footprint. Without it the server refuses to start, because there is no
compiled-in fallback.

Neither image carries a Rust toolchain, or anything else that was needed to
build it.

## Quick start

```bash
# from a checkout of the repository
mkdir -p workspace && sudo chown 1000:1000 workspace
docker compose -f deploy/containers/docker-compose.yml up -d
curl -s http://127.0.0.1:38886/health
```

```
{"status":"ok","protocol_version":1,"node_id":"loom-server","uptime_ms":42,"readers":8,"retained_events":0}
```

That is the all-in-one shape: one server, one daemon on the same machine, each
with its own volume. `LOOM_IMAGE_TAG` pins the version pulled, `LOOM_PORT`,
`LOOM_HOST_NAME` and `LOOM_WORKSPACE` move the published port, the name in the
host list and the directory the daemon works in. A private repository needs
`docker login ghcr.io` with a token that can read packages before any of this.

The workspace directory has to exist and be writable by uid 1000 *before* compose
starts: docker creates a missing bind-mount source as `root:root`, and the
daemon then cannot write into the very directory it exists to work in. This is the
container form of the systemd install's `install -d -o loom -g loom`.

## Running them by hand

The compose file is a convenience over two `docker run` commands.

A control plane, reachable from its own machine only:

```bash
docker volume create loom-server-data
docker run -d --name loom-server --restart unless-stopped \
  -p 127.0.0.1:38886:38886 \
  -v loom-server-data:/var/lib/loom/server \
  ghcr.io/550w-host/loom-server:0.1.0
```

An execution machine joining it — here, the same docker host, on a network where
`loom-server` resolves to the container above:

```bash
docker network create loom
docker volume create loom-daemon-state
docker run -d --name loom-daemon --restart unless-stopped \
  --network loom \
  -v loom-daemon-state:/var/lib/loom \
  -v "$PWD/workspace:/workspace" \
  -e LOOM_SERVER_URL=http://loom-server:38886 \
  -e LOOM_HOST_NAME=builder-1 \
  ghcr.io/550w-host/loom-daemon:0.1.0
```

An execution machine joining a server somewhere else is the same command with
that server's address in `LOOM_SERVER_URL` and no `--network` requirement:

```bash
docker run -d --name loom-daemon --restart unless-stopped \
  -v loom-daemon-state:/var/lib/loom \
  -v "$PWD/workspace:/workspace" \
  -e LOOM_SERVER_URL=http://10.0.0.5:38886 \
  -e LOOM_HOST_NAME=builder-1 \
  ghcr.io/550w-host/loom-daemon:0.1.0
```

`LOOM_SERVER_URL` has no default in either the image or the binary. A daemon
container started without it exits immediately:

```
Error: "--server-url (or LOOM_SERVER_URL) is required"
```

which is the failure worth having: an unconfigured daemon that guessed would
enrol somewhere nobody expected.

Everything else in `deploy/env/loom-server.env` and `deploy/env/loom-host-daemon.env`
works as `docker run --env-file`. Those files are written for a host — a
loopback bind, absolute paths a systemd unit created — so a container wants
`LOOM_BIND=0.0.0.0:38886`, and neither the data paths nor `HOME` need setting at
all: the images already point them at the volumes.

## Volumes and permissions

Both images run as uid/gid **1000:1000**, the container-side spelling of the
`loom` system user the units use. A number and not a name: `scratch` has no
`/etc/passwd` to look one up in, and the same number has to mean the same thing
in both images, in a volume and on a bind mount.

| Volume | Holds | Survives a rebuild |
| --- | --- | --- |
| `loom-server-data:/var/lib/loom/server` | the relay log (`shard-0.log` … `shard-7.log`) and `domain.snapshot` | the replay window and the domain state |
| `loom-daemon-state:/var/lib/loom` | `host-id`, `host-id.cursor`, `sessions/`, and the provider's own `$HOME` configuration | the machine's identity and its place in the log |
| `./workspace:/workspace` | whatever a task writes | it is the host directory, so it never went anywhere |

The two volumes are declared `VOLUME`s in the images, so a container started
without `-v` still gets somewhere to write — an anonymous volume, which `docker rm`
takes with it. Name them.

The daemon's identity is the reason its volume exists:

```
$ docker logs loom-daemon
loom-daemon "compose-verify" enrolled as host_01M2A68EPZA2D90GF854KN38Z5 with http://server:38886
$ docker rm -f loom-daemon && docker compose ... up -d
$ docker logs loom-daemon
loom-daemon "compose-verify" enrolled as host_01M2A68EPZA2D90GF854KN38Z5 with http://server:38886
```

The same host id, so the UI's host list and any dispatch aimed at that machine
still mean the same machine. Without the volume it would enrol a second host on
every rebuild, and the old one would sit in the list forever.

**Ownership is the one thing to get right.** The daemon writes as uid 1000, so a
bind-mounted workspace must be writable by uid 1000 — and everything it creates
lands on the host owned by uid 1000. On a single-user Linux host that is usually
already true (the first user *is* 1000); elsewhere pick one of:

```bash
sudo chown -R 1000:1000 ./workspace        # the daemon's user owns the workspace
```

```bash
# or run as your own uid, and give the volumes that uid as well
docker run --user "$(id -u):$(id -g)" ...
docker run --rm -v loom-daemon-state:/data alpine:3 sh -c 'chown -R 501:20 /data'
```

`--user` alone is not enough: it changes the process, not the volume, and the
data directory inside the image is owned by 1000 — a fresh named volume inherits
that ownership, so a process running as 501 cannot write its own `host-id`. Fix
the volume as well, or leave the container at 1000 and fix the workspace.

Reading a status file without a shell in the server image, which has none:

```bash
docker run --rm -v loom-server-data:/data alpine:3 sh -c 'ls -l /data'
```

## Ports

The server image sets `LOOM_BIND=0.0.0.0:38886` because anything else is
unreachable from outside the container's network namespace. **Publishing** is
what decides who can reach it, and the rule in
[`remote-access.md`](remote-access.md) is not a property of the bind address:

| Publishing | Means |
| --- | --- |
| `-p 127.0.0.1:38886:38886` | reachable from that host only — the container form of the default `LOOM_BIND` |
| `-p 38886:38886` | every interface the docker host has — the unauthenticated, command-executing API on a public address, which `remote-access.md` forbids |
| a compose network + `expose`, or `--network` only | reachable by the daemons on that network, by nothing else |

With `network_mode: host` there is no namespace boundary left, and `0.0.0.0` is
the host's own interfaces: set `LOOM_BIND=127.0.0.1:38886` there and publish
nothing.

The daemon image has no `EXPOSE` because the daemon binds no port — it dials out,
which is what lets it run behind NAT:

```bash
$ docker inspect loom-daemon --format '{{json .NetworkSettings.Ports}}'
{}
```

A daemon container reaches the server over `http://` (or `ws://`). The binaries
are built without a TLS client, and asking for one says so:

```
$ docker run --rm ghcr.io/550w-host/loom-daemon:0.1.0 --server-url https://example.com
Error: WebSocket("URL error: TLS support not compiled in")
```

So a containerised daemon does not go through a TLS-terminating proxy; it joins a
network that is already trusted and encrypted — the compose network, a tailnet, a
WireGuard interface — and talks plain HTTP inside it. `remote-access.md`'s
Tailscale setup is still exactly right for browsers and for the UI: it is the
daemon's URL that cannot be an `https://` one.

## Providers

The daemon runs whatever provider the control plane dispatches, so the image that
has one is a deployment decision. Three ways, in the order worth considering.

**1. A derived image.** Reproducible, and the only one that survives a rebuild
without hand-work:

```dockerfile
FROM ghcr.io/550w-host/loom-daemon:0.1.0
USER root
RUN apk add --no-cache nodejs npm git \
 && npm install -g @earendil-works/pi-coding-agent@0.85.1
USER 1000:1000
```

Then `docker build -t loom-daemon-pi:0.1.0 .` and point the compose service (or
`docker run`) at that tag. `apk add` needs a network, so this image is built where
there is one and then shipped; it does not make the daemon need the network at
start-up beyond reaching its server.

**2. Mounting the host's provider in.** Works for a self-contained provider
binary and for nothing else:

```bash
-v /usr/local/bin/pi:/usr/local/bin/pi:ro
```

A Node-installed `pi` is not self-contained — the mount also implies the `node`
interpreter and its module tree — and the daemon ends up coupled to the exact
layout of the host's installation. Mount a *configuration* this way, rather than
the program (`-v ~/.pi:/var/lib/loom/.pi:ro`), and it is a good fit.

**3. Installing at runtime.** `docker exec -u 0 <container> apk add …` is fine for
finding out whether something works, and wrong as a deployment: it is invisible to
the Dockerfile, gone on the next rebuild, and not reproducible from a tag.

Whichever way, an ACP provider also needs *configuration* — for the embedded
Pi adapter, `$HOME/.pi/agent` has to exist. `HOME` in the daemon image is
`/var/lib/loom`, the state volume, so the adapter and Pi configuration survive a
container rebuild. Mounting the host's `~/.pi` over it reuses one that already
exists.

## What a containerised daemon cannot do

The daemon's job is to run providers and tools *on the machine the work is for*
and to change *that machine's* files. Inside a container, "the machine" is the
container's filesystem plus whatever is mounted into it, so the boundary is not a
packaging detail — it is the answer to "what can this execution machine do".

Bind-mounting the workspace gives file-level parity and nothing more. A task that
just reads and writes the workspace behaves the same as under systemd. A task
that needs anything *else* on the host does not: a compiler or a language
toolchain installed there, `git` credentials or an SSH agent, `~/.pi` if it was
not mounted, a database on the host's loopback, docker itself, a host service the
task is supposed to restart. None of those are visible, and the failure is
usually a confusing "command not found" inside a provider run rather than a
permission error at container start.

Mounting more of the host closes the gap one path at a time — up to the point
where the mount list is longer than the reason for containerising. `--privileged`,
or the docker socket, is the other end of that road: it turns the container into
root on the host with extra steps, and abandons the boundary that was the entire
benefit.

**The conclusion to deploy with:**

* the **server** containerises freely. It executes nothing, its state is two
  files, and its whole interface is one port. Nothing is lost.
* the **daemon** containerises when the workspace *is* the environment the task
  needs: CI-style jobs, a task that operates on a checked-out tree, a dedicated
  single-tenant box whose workspace is bind-mounted and owned by uid 1000. Then
  the container is a better boundary than a system user, and the mounts say
  exactly what the work can touch.
* the daemon on a machine whose whole point is that an agent can touch *the
  machine* — a laptop with a toolchain and credentials on it — is what the
  systemd install is for. That is not a limitation to route around; it is the
  deployment that matches the job.

Nothing here is specific to containers: a systemd daemon sandboxed to one
directory has the same property, which is why the daemon unit in
`deploy/systemd/` is deliberately the laxer of the two.

## Compared with the systemd install

| | systemd unit | container |
| --- | --- | --- |
| Configuration | `/etc/loom/*.env` (`EnvironmentFile`) | `-e` / `--env-file` / compose `environment` |
| Identity | `loom` system user, uid chosen at install | `USER 1000:1000`, fixed |
| Data | `/var/lib/loom` owned by `loom` | a named volume owned by 1000 |
| Restart | `Restart=always` / `on-failure` | `restart: unless-stopped` |
| Sandbox | `ProtectSystem=strict`, `NoNewPrivileges`, empty capability set (server); deliberately light (daemon) | namespaces, the image's userland, read-only-where-mounted |
| Limits | `MemoryMax`, `CPUQuota`, `TasksMax`, `OOMScoreAdjust` | `--memory`, `--cpus`, `--pids-limit`, `--oom-score-adj` |
| Upgrade | install new binaries, restart the unit | pull a new tag, recreate the container |
| What the process sees | the host | the container's filesystem + mounts |

The resource limits are suggestions in both cases and have to be sized to the
machine; the asymmetry `deploy/README.md` describes — a bounded control plane, an
execution plane allowed to exhaust a host and be OOM-killed first — carries over
unchanged.

`alpine` is musl, and the binaries are musl-static, so nothing is being
translated: the same file runs on any glibc or musl host, with or without this
image.

## Building an image

The images are built from the **packaged** binaries, the ones the release page
publishes, so an image and a download carry the same bytes:

```bash
pnpm --filter @bb/app run build              # the UI bundle every image carries
cargo build --release --locked --target x86_64-unknown-linux-musl
scripts/package-release.sh x86_64-unknown-linux-musl
scripts/build-container-images.sh --platform linux/amd64 --tags dev
docker run --rm loom-server:dev --version
```

`build-container-images.sh` stages a small build context — one binary per Docker
architecture name, the UI bundle as `ui/` (`--ui-dir`, default
`apps/app/dist`), plus a `.keep` placeholder the Dockerfiles copy to create
their data directories owned by 1000 — and hands it to `docker buildx`. The
placeholders exist because a `RUN` is what would otherwise be needed to create
and `chown` a directory, and a `RUN` is what drags an emulator into a
cross-platform build. Neither Dockerfile executes anything, so one invocation
builds both platforms:

```bash
scripts/build-container-images.sh --platform linux/amd64,linux/arm64 \
  --registry ghcr.io/550w-host --tags 0.1.0,v0.1.0,latest --push
```

By hand, without the script, a single platform:

```bash
mkdir -p dist/context
install -m 0755 dist/loom-server-x86_64-unknown-linux-musl dist/context/loom-server-amd64
install -m 0755 dist/loom-daemon-x86_64-unknown-linux-musl dist/context/loom-daemon-amd64
install -m 0644 deploy/containers/keep dist/context/.keep
cp -R apps/app/dist dist/context/ui
docker build -f deploy/containers/loom-server.Dockerfile -t loom-server:dev dist/context
docker build -f deploy/containers/loom-daemon.Dockerfile -t loom-daemon:dev dist/context
```

A **multi-platform** build additionally needs a builder with the container driver
— the default `docker` driver refuses a manifest list, which is the error you get
if you skip this:

```bash
docker buildx create --name loom --driver docker-container --bootstrap --use
```

The `images` job in [`releasing.md`](releasing.md) runs exactly this: it builds
both images from the checksummed release files on every tag, pushes them to GHCR
with `latest`/`<version>`/`v<version>` tags — no `latest` for a pre-release, the
same `-` rule as the release page — and runs the `linux/amd64` halves back to
check they report the version the release page names. The `linux/arm64` halves
are built from the same files and never executed here, the same reasoning as
`verify-release-binaries.sh --elf-only`.

## Upgrading

Pull the newer tag and recreate the container. The volumes stay, so the replay
window, the enrolled host id and the provider's configuration all survive — that
is what makes a restart an upgrade rather than a new machine.

The protocol rule is unchanged and is the thing to plan around: a server and a
daemon connect only when their protocol versions are **equal**, so server and
daemon images are upgraded together. A daemon image that moves second has a
choice, and it is the same one a bare binary has (see
[`upgrades.md`](upgrades.md) § Daemon self-update):

- **In-container self-update** works if the server hosts a daemon artifact for
  this container's architecture. The daemon image already runs the loop, so all
  that is needed is `LOOM_ARTIFACT_DIR` on the server pointing at a directory
  holding `loom-daemon-<triple>`, and the container restart policy then starts
  the new binary exactly as `Restart=always` would. The public images are not
  laid out for it — `loom-server` is `scratch` and carries no daemon — so this is
  an explicit choice, not the default path.
- **Rebuild the image**, which is the container-native equivalent: the
  replacement arrives as a new image and the runtime's restart policy is the
  supervisor. Set `LOOM_AUTO_UPDATE=0` in the daemon service's environment so the
  two mechanisms cannot both act on the same container.

The dispatches that would have run on a daemon while its container was being
replaced are reaped by the server (`host_stale` or `timed_out`) exactly as for a
binary restart; the thread leaves `working` and the turn is re-issued.
Pinning both to the same version is the safe shape:

```yaml
image: ghcr.io/550w-host/loom-server:0.1.0
image: ghcr.io/550w-host/loom-daemon:0.1.0
```

Rolling back is the same move in reverse: pin the tag that was working and
recreate. The volumes are not part of the image, so they are not replaced by it.
Because a daemon never installs an artifact older than the protocol it already
speaks (§ Failure modes), a rollback is `docker run` with the older tag, never an
in-container one.

## What was verified

A clean-machine run of the images built from this tree's `x86_64-unknown-linux-musl`
artifacts, on a host whose `docker` had no loom installed:

```
loom-server 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit d982b2da4df3…)
loom-daemon 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit d982b2da4df3…)
/health ok (protocol 1, node loom-server)
GET / -> 200 text/html, 1476 bytes
GET /app.js -> 200 text/javascript; charset=utf-8, 1191514 bytes
created project proj_01M2A65T2V4PWKZDA3QKS9CRMC
published 01M2A66KC78X4J0R73EHMXRDKT to project:proj_containercheck
replayed it byte-identically after the container was destroyed and recreated
daemon enrolled as host_01M2A670F85BSDSZENS8FPPNE5
enrolled as the same host_01M2A670F85BSDSZENS8FPPNE5 after a rebuild
docker compose up: server healthy, daemon connected, no ports on the daemon
```

Image metadata was checked rather than assumed: `User` is `1000:1000` in both,
the server's `Volumes` is `/var/lib/loom/server`, the daemon's `ExposedPorts` is
empty, and the data directories inside both images are owned by 1000 — which is
what a declared volume copies into a fresh named volume. A `linux/amd64` and a
`linux/arm64` manifest list were built and pushed for each image from the two
targets' artifacts, and the aarch64 binaries inside the arm64 images were checked
against the artifacts' SHA-256 — but only the amd64 halves are ever executed, here
or in the pipeline.
