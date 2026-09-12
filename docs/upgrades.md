# Upgrading

loom is three artifacts that must agree on one number:

| Artifact | What it is | Where it runs |
| --- | --- | --- |
| **server** | `loom-server` binary | one machine |
| **daemon** | `loom-daemon` binary | every execution machine |
| **package** | the built UI bundle (the ported bb app served by `LOOM_UI_DIR`) | served by the server to every client |

This page defines what "agree" means, how to update each one, and how to roll
back. The guiding lesson is bb #3143: when a self-update replaces the process
that owns a daemon's transport, every in-flight turn dies. loom's answer is
structural — the server and daemon are independent processes, and delivery is a
replayable log — and this page is how that is used rather than fought.

## Version consistency

Two version fields exist, and only one of them is a compatibility gate:

- **`protocol_version`** — the wire contract on `/ws` and for
  `RunDispatch`/`ProviderReport`. Currently `1`
  (`crates/server/src/lib.rs`, pinned by a test). **This is the gate.**
- **`version`** — the crate semver (`0.1.0`). Informational; releases with the
  same `protocol_version` are interoperable regardless of `version`.

`GET /api/v1/version` reports both:

```bash
curl -s http://127.0.0.1:38886/api/v1/version
# {"version":"0.1.0","protocol_version":1}
```

The server also sends `protocol_version` in the first frame of every `/ws`
connection (`{"type":"welcome",…,"protocol_version":1}`). A daemon reads it
before enrolling and **refuses a mismatch** (`loom_daemon::ensure_compatible_protocol`),
so a daemon built against a different protocol never enrolls and never receives
a dispatch it would misread. The reference UI bundle does not yet enforce it,
so treat the bundle as required to match the server's release as well.

Both binaries answer the same question about the file itself, before either one
has been started — which is what a download has to be checked with
([`releasing.md`](releasing.md)):

```bash
loom-server --version
# loom-server 0.1.0 (x86_64-unknown-linux-musl, protocol 1, commit 0f1e2d3c…)
```

The line names the target triple and the commit the file was built from as well
as the version, so two binaries from different releases are told apart without
starting either of them.

> **Rule:** every server, every daemon and the UI bundle must be from releases
> with the same `protocol_version`. Within that, upgrade in any order.

A mismatch is not a degraded mode. Do not run a mixed `protocol_version`
fleet "temporarily": the daemon will simply refuse to connect, which is the
intended, loud failure.

### Checking a fleet

```bash
# the server's contract
curl -s http://127.0.0.1:38886/api/v1/version | grep protocol_version

# each daemon reports the same number in its log line at startup, and refuses a
# mismatched server before enrolling:
journalctl -u 'loom-host-daemon@builder-1' | grep -i 'protocol version'
```

## Updating

`install.sh` plus a restart is the supported update path. There is no
in-process self-update. The binaries come from a checkout or from a release:

```bash
# A. from a checkout: build, then install from the build output
cargo build --release
sudo deploy/install.sh server

# B. from a release: no toolchain needed, the installer downloads and verifies
sudo deploy/install.sh --release v0.2.0 server

# ...then, either way, on the server machine
sudo systemctl restart loom-server

# ...and on each execution machine (any order when protocol_version is unchanged)
sudo deploy/install.sh --release v0.2.0 daemon builder-1 https://loom.example.com
sudo systemctl restart loom-host-daemon@builder-1
```

`--release <version>` downloads the binaries for this machine's target from the
GitHub Release, checks each of them against the release's `SHA256SUMS`, and only
then replaces `/usr/local/bin/loom-*`;
[`../deploy/README.md`](../deploy/README.md) § Install from a release has the
details, including `GITHUB_TOKEN` for a private repository. The property that
matters here is the failure mode: a download that fails, or one whose digest
does not match, aborts **before** anything is installed and exits non-zero, so a
fleet upgrade is never half-done by a bad connection. A machine that is
unreachable stays on its current binary and keeps reconnecting; when it comes
back it can be upgraded the same way.

`install.sh` never overwrites an existing environment file, so step 2 and 3 are
safe to re-run and safe to run from a configuration-management tool that
replaces binaries. Re-running the same `--release` re-downloads and re-verifies
rather than trusting what is already installed.

### What a restart does not lose

- **The daemon's identity.** The host id is persisted in `LOOM_DAEMON_STATE` on
  first enroll and re-presented on start, so a restart updates the existing host
  instead of enrolling a second one. A host is "a machine, not a connection".
- **The replay window.** With `LOOM_DATA_DIR` the log is on disk and survives a
  server restart; with `LOOM_REDIS_URL` it is shared and also lets a second node
  attach to the same window. Only the default in-process backend loses it.
- **Missed frames.** A daemon persists its host-scope cursor next to its host id
  and, on start, subscribes *then* replays from that cursor. A dispatch
  published while it was restarting arrives late rather than being lost, and the
  event-id dedup set drops the overlap.
- **In-flight runs are not lost, they are reaped.** A run whose daemon restarted
  mid-flight is completed by the server's reaper (`timed_out` after the
  deadline, or `host_stale` after the host stops heartbeating), so a thread
  never stays `working` forever. The provider work itself is not resumed by the
  daemon; re-issue the turn.

If a `protocol_version` change is involved, upgrade the server and every daemon
in one maintenance window while no runs are in flight. Existing daemons will
refuse to reconnect to the new server until they are updated — that refusal is
the migration signal, not an outage to work around.

## Self-update

There is deliberately none.

- **`loom-server`** does not replace its own binary. Replacing a running
  executable is the coupling the fork removes; the update is `install.sh` then
  `systemctl restart`, or an external supervisor doing the same.
- **`loom-daemon`** does not replace its own binary either. Its correctness
  across a restart (persisted identity and cursor) is what makes an external
  restart safe; an in-process update would reintroduce the "transport dies with
  the updater" failure.
- **The desktop shell** may auto-update *itself* — that is packaging, and it is
  the shell's business. It must not replace or restart a running server or
  daemon in place; the shell's two supervision switches are independent of the
  window precisely so a daemon can stop without taking the UI with it.

Because no component updates itself, a scheduled update is an ordinary
`systemctl restart` and needs no quiesce protocol beyond "no runs in flight" for
a `protocol_version` change.

## Rollback

Rolling back is reinstalling the previous binaries and restarting; data formats
are stable within a `protocol_version`.

```bash
# keep the previous binaries where the upgrade can find them again
sudo install -m 0755 /var/lib/loom/bin/loom-server.prev /usr/local/bin/loom-server
sudo systemctl restart loom-server

# a daemon likewise
sudo install -m 0755 /var/lib/loom/bin/loom-daemon.prev /usr/local/bin/loom-daemon
sudo systemctl restart loom-host-daemon@builder-1
```

If the previous binaries were not kept, the previous release is the copy: it
goes through the same download-and-verify path as an upgrade.

```bash
sudo deploy/install.sh --release v0.1.0 server
sudo systemctl restart loom-server
```

Rules:

- Roll back **both ends together** when a `protocol_version` change is involved.
  A new server and an old daemon (or the reverse) is exactly the mismatch the
  handshake refuses.
- The environment file and the data directory are unchanged across an ordinary
  upgrade, so rollback does not touch them.
- The relay log needs no migration within a `protocol_version`. It is an
  append-only per-shard file (or Redis streams) that both the old and the new
  binary read with the same framing. If a future release changes that framing,
  it will bump `protocol_version` and this page will say so.

## Why this is not a package manager

There is no version directory, no atomically-swapped release tree, and no
rollback command. Those are the shape of the orchestration systems the fork
explicitly does not require (`architecture.md` § Deployment shapes): bare
systemd must be enough. Nix, Ansible, a container image or a package repository
can all be layered on top — they all reduce to "put the binary here, write the
environment file, restart the unit", which is exactly what `install.sh` does.
