#!/usr/bin/env bash
# Build the loom-server and loom-worker container images.
#
# The two images are the one `loom` binary in its two roles, and the input is the
# *packaged* binary — what `scripts/package-release.sh` writes as
# `dist/loom-<target>` — so an image carries the same bytes the release page
# publishes, rather than a second build of the same commit. That is also why
# aarch64 images can be built on an amd64 machine: nothing here runs the binary,
# and neither Dockerfile has a `RUN`, so no emulator is involved. The aarch64
# binary is still verified by `scripts/verify-release-binaries.sh`, which is where
# the machine that can execute it lives.
#
# Nothing else is staged: the server's product app is compiled into the binary,
# so the server image serves the client the packaged bytes were built with and
# has no bundle to copy in or path to configure.
#
# What the build reads is a staged context, not the repository: one binary per
# Docker architecture, named `loom-<amd64|arm64>`, plus the `.keep` placeholder
# the Dockerfiles copy into their volumes. `--platform` selects which binary the
# build needs, so a one-platform build stages one platform. The Dockerfiles are
# handed to docker with `--file`, so they stay in `deploy/`.
#
# Usage:
#   scripts/build-container-images.sh [options]
#
#   --platform LIST   platform(s) to build, comma-separated
#                     default linux/<the architecture of the docker daemon>
#   --tags LIST       image tags, comma-separated  default dev
#   --registry REPO   repository prefix to tag under, e.g. ghcr.io/550w-host
#                     default empty, i.e. `loom-server:<tag>`
#   --dist-dir DIR    where the packaged binary is  default dist
#   --push            push to the registry; without it the images are loaded
#                     into the local daemon, which takes a single platform
#   -h, --help        this text
#
# Needs docker with the buildx plugin, and the staged binary for every platform
# named.

set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() { sed -n '2,/^$/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
note() { printf '  %s\n' "$*"; }

platforms=""
tags="dev"
registry=""
dist_dir="$repo_root/dist"
push=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    -h | --help)
      usage
      exit 0
      ;;
    --platform)
      platforms="${2:-}"
      shift 2
      ;;
    --tags)
      tags="${2:-}"
      shift 2
      ;;
    --registry)
      registry="${2:-}"
      shift 2
      ;;
    --dist-dir)
      dist_dir="${2:-}"
      shift 2
      ;;
    --push)
      push=1
      shift
      ;;
    *) die "unrecognised option: $1" ;;
  esac
done

command -v docker >/dev/null || die "docker is required"
# `docker build --platform` is not enough here: a multi-platform image is a
# manifest list, which only buildx builds. Checked once, before any staging.
docker buildx version >/dev/null 2>&1 ||
  die "the docker buildx plugin is required (Docker Desktop ships it; 'docker buildx install' for a plain daemon)"

# A local build is for the machine it runs on. The default is read from the
# daemon rather than from `uname` because that is the architecture the image has
# to be loadable by.
if [[ -z "$platforms" ]]; then
  host_arch="$(docker version --format '{{.Server.Arch}}')"
  platforms="linux/$host_arch"
fi

# One target is published per platform (docs/releasing.md § Targets), and the
# context names the binary by Docker architecture because that is the only name
# BuildKit's TARGETARCH can select with.
target_for_platform() { # <platform> -> "<rust target triple> <docker architecture>"
  case "$1" in
    linux/amd64) printf '%s\n' "x86_64-unknown-linux-musl amd64" ;;
    linux/arm64) printf '%s\n' "aarch64-unknown-linux-musl arm64" ;;
    *) die "no release binary is built for platform $1 (linux/amd64 and linux/arm64 are)" ;;
  esac
}

context="$dist_dir/context"
rm -rf "$context"
mkdir -p "$context"
install -m 0644 "$repo_root/deploy/containers/keep" "$context/.keep"

IFS=',' read -r -a platform_list <<<"$platforms"
for platform in "${platform_list[@]}"; do
  read -r triple arch <<<"$(target_for_platform "$platform")"
  built="$dist_dir/loom-$triple"
  [[ -f "$built" ]] ||
    die "$built is missing; build and package $triple first: scripts/package-release.sh $triple"
  install -m 0755 "$built" "$context/loom-$arch"
  note "staged $triple as $arch"
done

# `--load` puts the image in the local daemon and carries one platform only;
# pushing is what a manifest list is for.
output=(--load)
if ((push)); then
  output=(--push)
elif ((${#platform_list[@]} > 1)); then
  die "--load builds one platform; add --push to publish a multi-platform manifest"
fi

IFS=',' read -r -a tag_list <<<"$tags"
[[ -n "${tag_list[0]}" ]] || die "--tags is empty"

for image in loom-server loom-worker; do
  refs=()
  for tag in "${tag_list[@]}"; do
    refs+=(--tag "${registry:+$registry/}$image:$tag")
  done
  note "building $image for $platforms"
  docker buildx build \
    --file "$repo_root/deploy/containers/$image.Dockerfile" \
    --platform "$platforms" \
    "${refs[@]}" \
    "${output[@]}" \
    "$context"
done

note "built ${tag_list[*]} for $platforms"
