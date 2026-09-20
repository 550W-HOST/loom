# loom-server — the control plane — as a container image.
#
# `scratch`, and not a distribution base: the binary is a static musl build, so
# it needs no loader, no libc and no shared library, and this process executes
# nothing at all — no provider, no tool, no shell. A userland the server never
# calls is attack surface and CVE feed for no benefit. The price is `docker
# exec`: there is no shell in here to exec into, so a containerised server is
# inspected through `docker logs`, `GET /health` and the data volume.
#
# The build context is not the repository: it is the staged directory
# `scripts/build-container-images.sh` writes (docs/containers.md), which holds
# one binary per architecture, named for the Docker architecture, plus `.keep`:
#
#   scripts/package-release.sh x86_64-unknown-linux-musl
#   scripts/build-container-images.sh --platform linux/amd64
#
# so a hand-built image is those two commands over a context of a few megabytes,
# and the bytes the image carries are the ones the release page publishes.
FROM scratch

# `amd64` or `arm64`, set by BuildKit for the platform being built. One
# Dockerfile therefore builds either image, and the same invocation can build
# both platforms at once — no emulator is involved because nothing in this file
# executes an instruction of the target architecture. That property is
# deliberate: a `RUN` here would put qemu into the release path.
ARG TARGETARCH

# The data directory must exist, owned by the runtime user, *before* it is
# declared a VOLUME. A declared volume whose path is absent from the image gets
# a root:root mountpoint created by the runtime, and a server running as
# 1000:1000 then cannot create `shard-0.log` in it. Copying a placeholder is how
# an owned directory is made without a `RUN`, which is the one instruction that
# would need emulation.
COPY --chown=1000:1000 loom-${TARGETARCH} /usr/local/bin/loom
COPY --chown=1000:1000 .keep /var/lib/loom/server/.keep

# The `loom` identity, as a number. Numeric rather than a name because a name
# needs an /etc/passwd this image deliberately does not carry, and because
# 1000:1000 has to mean the same thing in both images and on a bind-mounted
# workspace. The binary is statically linked, so nothing here needs a passwd
# entry to resolve a user; the process simply is that uid.
USER 1000:1000

# The default CMD flags are the deployment's values, adjusted for a container:
#
#   --bind 0.0.0.0:38886  the container's own interfaces — the port is useless
#                         to anything outside the namespace otherwise. Whether
#                         anyone can reach it is decided by port publishing, not
#                         by this value, so the rule in docs/remote-access.md is
#                         unchanged: never publish this port on a public
#                         interface.
#   --data-dir            matches the VOLUME below, so the durable relay log is
#                         the default rather than an opt-in.
#   --node-id             stamps every envelope this node produces; two server
#                         containers must not share it.
#
# The product app needs no flag: the image carries the server built with the
# client compiled into it, so `/` serves the app with nothing configured here.
#
# --redis-url is deliberately not passed: the in-process/disk backend needs no
# second service (docs/redis-backend.md). LOOM_REDIS_URL remains its only
# environment fallback, for a URL that cannot go on a command line.
CMD ["--bind", "0.0.0.0:38886", "--data-dir", "/var/lib/loom/server", "--node-id", "loom-server"]

# Where the relative paths a unit would write land. The two paths that matter
# are absolute by default either way.
WORKDIR /var/lib/loom

# Informational: the default `--bind` listens here. Port publishing is what
# decides reachability, and `-p 127.0.0.1:38886:38886` keeps the host's own
# loopback-only rule.
EXPOSE 38886

# The relay log, and only the relay log: a container rebuilt from a newer image
# with this volume attached replays the whole retained window instead of
# starting an empty one.
VOLUME /var/lib/loom/server

# The one binary, in its server role. `loom server` rather than a `loom-server`
# symlink: the image carries a single file, and the ENTRYPOINT is where a
# container's role is written down. The CMD above is the default flag set a
# `docker run` starts with.
ENTRYPOINT ["/usr/local/bin/loom", "server"]
