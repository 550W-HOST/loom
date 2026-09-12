# loom-daemon — the execution plane — as a container image.
#
# `alpine`, for the opposite reason the server image is `scratch`: this process
# exists to execute provider CLIs, and a provider is usually not a static
# binary. `pi` is a Node program; running it needs a userland this image can
# host. What is *not* here is a provider — which one, and where it keeps its
# credentials, is a deployment decision (docs/containers.md § Providers), so
# the image is a base: either a derived image installs one
#
#   FROM ghcr.io/550w-host/loom-daemon:0.1.0
#   USER root
#   RUN apk add --no-cache nodejs npm && npm install -g @earendil-works/pi-coding-agent
#
# or the operator mounts one in. The binary itself is still static, so nothing
# in the base is load-bearing for loom — the shell and `apk` are for whatever
# the daemon is asked to run.
#
# Pinned by digest as well as tag: the tag is what a reader recognises, the
# digest is what actually gets built. To move it:
#   docker buildx imagetools inspect alpine:3.22 --format '{{.Manifest.Digest}}'
FROM alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce

# `amd64` or `arm64`, set by BuildKit for the platform being built; the context
# holds one binary per architecture (docs/containers.md).
ARG TARGETARCH

COPY --chown=1000:1000 loom-daemon-${TARGETARCH} /usr/local/bin/loom-daemon

# Owned directories, created without a `RUN` so a cross-platform build needs no
# emulator. The server Dockerfile explains why the volume path cannot be left
# for the runtime to create. `/workspace` is the daemon's default environment
# root: present so a container that was started without a workspace mount still
# has somewhere to work, shadowed the moment one is mounted there.
COPY --chown=1000:1000 .keep /var/lib/loom/.keep
COPY --chown=1000:1000 .keep /workspace/.keep

# The `loom` identity as a number, the same pair both images use.
USER 1000:1000

# HOME is the state volume on purpose. A provider keeps its per-user
# configuration under `$HOME` — `$HOME/.pi/agent` for pi — and putting that
# inside the volume is what lets an authenticated provider survive a container
# rebuild. To reuse a configuration that already exists on the host, mount it
# over just that directory (`-v ~/.pi:/var/lib/loom/.pi:ro`).
#
#   LOOM_DAEMON_STATE    the enrolled host id, so a rebuilt container is the
#                        same machine instead of a second host
#   LOOM_SESSION_DIR     per-thread provider sessions, likewise persistent
#   LOOM_WORKSPACE_ROOT  where managed environments are created; the mount
#                        point is the whole of what this container can edit
#
# LOOM_SERVER_URL has no default on purpose: the daemon refuses to start
# without it rather than guessing a server, so an unconfigured container fails
# immediately and visibly.
ENV HOME=/var/lib/loom \
    LOOM_DAEMON_STATE=/var/lib/loom/host-id \
    LOOM_SESSION_DIR=/var/lib/loom/sessions \
    LOOM_WORKSPACE_ROOT=/workspace

WORKDIR /var/lib/loom

# The host identity and the replay cursor live here, which is what makes "the
# container was rebuilt" different from "a new machine joined". There is no
# EXPOSE and no port: the daemon dials out and binds nothing, so it works behind
# NAT and needs no inbound rule (deploy/README.md § Ports).
VOLUME /var/lib/loom

ENTRYPOINT ["/usr/local/bin/loom-daemon"]
